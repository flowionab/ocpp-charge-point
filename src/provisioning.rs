//! Provisioning functional block: BootNotification and charge-point registration with the
//! CSMS. See `docs/ROADMAP.md` §2.

use crate::actor::ChargePointActor;
use crate::state::{
    ChargePointEvent, Component, RegistrationStatus, Variable, VariableAttributeType,
};
use alloc::boxed::Box;
#[cfg(feature = "tokio-runtime")]
use core::time::Duration;

/// Default interval to wait before retrying BootNotification after a transport-level failure
/// (as opposed to a `Pending`/`Rejected` response, which carries its own retry interval).
pub const DEFAULT_RETRY_INTERVAL_SECS: u32 = 30;

/// The CSMS's answer to a BootNotification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootNotificationOutcome {
    /// The CSMS's registration decision.
    pub status: RegistrationStatus,
    /// Seconds to wait: the heartbeat interval when `status` is `Accepted`, otherwise the
    /// minimum wait before the charge point may retry BootNotification.
    pub interval_secs: u32,
}

/// Sends a BootNotification and reports the CSMS's decision.
///
/// Implemented per protocol version (see the `ocpp_2_1` module) so
/// [`crate::ChargePointRuntime::register`] stays protocol-agnostic, and so tests can supply a
/// fake implementation without a live connection.
#[async_trait::async_trait]
pub trait BootNotifier {
    /// The error type returned if the BootNotification request itself fails.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Sends a BootNotification identifying the charge point by `vendor_name`/`model_name` and
    /// returns the CSMS's decision.
    async fn notify_boot(
        &self,
        vendor_name: &str,
        model_name: &str,
    ) -> Result<BootNotificationOutcome, Self::Error>;
}

/// Waits between BootNotification retry attempts. Abstracted so tests can skip real delays,
/// and so embedded targets without tokio can supply their own timer.
#[async_trait::async_trait]
pub trait Backoff {
    /// Waits `seconds` before the caller retries.
    async fn wait(&self, seconds: u32);
}

/// A [`Backoff`] backed by `tokio::time::sleep`. Requires the `tokio-runtime` feature.
#[cfg(feature = "tokio-runtime")]
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioBackoff;

#[cfg(feature = "tokio-runtime")]
#[async_trait::async_trait]
impl Backoff for TokioBackoff {
    async fn wait(&self, seconds: u32) {
        tokio::time::sleep(Duration::from_secs(seconds as u64)).await;
    }
}

/// Sends a Heartbeat, telling the CSMS the charge point is still alive. Implemented per
/// protocol version (see the `ocpp_2_1` module), mirroring [`BootNotifier`].
#[async_trait::async_trait]
pub trait HeartbeatSender {
    /// The error type returned if the Heartbeat request itself fails.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Sends a Heartbeat to the CSMS.
    async fn send_heartbeat(&self) -> Result<(), Self::Error>;
}

/// Runs the Provisioning functional block's BootNotification exchange against `actor`: sends a
/// BootNotification via `notifier` and applies the CSMS's decision to the charge point's state
/// (see `ChargePointEvent::RegistrationStatusReceived`).
///
/// This performs a single BootNotification attempt. On `Pending`/`Rejected`, callers are
/// responsible for retrying after the returned outcome's interval, per the OCPP spec. See
/// [`register_until_accepted`] for a version that retries automatically.
pub async fn register<N: BootNotifier>(
    actor: &ChargePointActor,
    notifier: &N,
    vendor_name: &str,
    model_name: &str,
) -> Result<RegistrationStatus, N::Error> {
    let outcome = notifier.notify_boot(vendor_name, model_name).await?;
    let _ = actor
        .send(ChargePointEvent::RegistrationStatusReceived(outcome.status))
        .await;
    Ok(outcome.status)
}

/// Runs BootNotification against `actor` repeatedly until the CSMS accepts registration,
/// applying every intermediate decision to the charge point's state. Per OCPP, a charge point
/// does not give up: on `Pending`/`Rejected` it waits the response's `interval_secs` before
/// retrying, and on a transport-level failure it waits [`DEFAULT_RETRY_INTERVAL_SECS`] instead.
pub async fn register_until_accepted<N: BootNotifier, B: Backoff>(
    actor: &ChargePointActor,
    notifier: &N,
    backoff: &B,
    vendor_name: &str,
    model_name: &str,
) -> BootNotificationOutcome {
    loop {
        match notifier.notify_boot(vendor_name, model_name).await {
            Ok(outcome) => {
                let _ = actor
                    .send(ChargePointEvent::RegistrationStatusReceived(outcome.status))
                    .await;
                if outcome.status == RegistrationStatus::Accepted {
                    return outcome;
                }
                backoff.wait(outcome.interval_secs).await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "boot notification failed, retrying");
                backoff.wait(DEFAULT_RETRY_INTERVAL_SECS).await;
            }
        }
    }
}

/// The `(Component, Variable)` this crate's device model uses for the heartbeat interval - see
/// [`crate::state::DeviceModel::register_defaults`].
fn heartbeat_interval_variable() -> (Component, Variable) {
    (
        Component {
            name: "OCPPCommCtrlr".into(),
            instance: None,
            evse: None,
        },
        Variable {
            name: "HeartbeatInterval".into(),
            instance: None,
        },
    )
}

/// Reads `actor`'s current `OCPPCommCtrlr`/`HeartbeatInterval` device model value, parsed as
/// whole seconds. `None` if the variable isn't registered, has no `Actual` attribute, its value
/// doesn't parse as a `u32`, or parses to `0` - a `0`-second interval would turn
/// [`run_heartbeat`]'s loop into a busy-spin, so it's treated the same as "not set" rather than
/// honoured. Callers fall back to a caller-supplied default in every one of those cases.
fn heartbeat_interval_secs(actor: &ChargePointActor) -> Option<u32> {
    let (component, variable) = heartbeat_interval_variable();
    let state = actor.state();
    let value = state
        .device_model
        .get(&component, &variable)?
        .attribute(VariableAttributeType::Actual)?
        .value
        .parse::<u32>()
        .ok()?;
    (value != 0).then_some(value)
}

/// Sends a Heartbeat every cycle, forever, honouring `actor`'s current
/// `OCPPCommCtrlr`/`HeartbeatInterval` device model value (read fresh from `actor` on each cycle) on
/// every iteration - so a CSMS that changes it via `SetVariables` (OCPP 2.x) or
/// `ChangeConfiguration` (1.6J, projected onto the same variable - see
/// `crate::device_model::ocpp_1_6`) takes effect on the very next heartbeat, without a reboot.
/// Falls back to `fallback_interval_secs` (the interval the accepted BootNotification returned -
/// see [`register_until_accepted`]) whenever the device model's value is missing, unparseable,
/// or `0`, so a bad `SetVariables` write can never turn this into a busy-spin. Errors sending the
/// heartbeat itself are logged and do not stop the loop - the next heartbeat is still due at the
/// regular interval, per OCPP.
pub async fn run_heartbeat<H: HeartbeatSender, B: Backoff>(
    sender: &H,
    backoff: &B,
    actor: &ChargePointActor,
    fallback_interval_secs: u32,
) {
    loop {
        let interval_secs = heartbeat_interval_secs(actor).unwrap_or(fallback_interval_secs);
        backoff.wait(interval_secs).await;
        if let Err(err) = sender.send_heartbeat().await {
            tracing::warn!(error = %err, "heartbeat failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Backoff, HeartbeatSender, heartbeat_interval_secs, run_heartbeat};
    use crate::actor::ChargePointActor;
    use crate::executor::TokioExecutor;
    use crate::state::{
        ChargePointEvent, Component, DeviceModelEvent, Variable, VariableAttributeType,
    };
    use alloc::boxed::Box;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use core::time::Duration;
    use std::sync::Mutex;

    struct CountingHeartbeatSender {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl HeartbeatSender for CountingHeartbeatSender {
        type Error = core::convert::Infallible;

        async fn send_heartbeat(&self) -> Result<(), Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct NoopBackoff;

    #[async_trait::async_trait]
    impl Backoff for NoopBackoff {
        async fn wait(&self, _seconds: u32) {
            // A real yield point, not a no-op: `run_heartbeat`'s loop never returns, so without
            // this the loop's futures are always immediately `Ready` and the task never
            // suspends - starving the executor and preventing the `timeout` below from ever
            // firing.
            tokio::task::yield_now().await;
        }
    }

    /// A [`Backoff`] that records every `seconds` value it was called with (in order), so a test
    /// can inspect exactly which interval `run_heartbeat` used on each cycle.
    struct RecordingBackoff {
        seen: Arc<Mutex<alloc::vec::Vec<u32>>>,
    }

    #[async_trait::async_trait]
    impl Backoff for RecordingBackoff {
        async fn wait(&self, seconds: u32) {
            self.seen.lock().unwrap().push(seconds);
            tokio::task::yield_now().await;
        }
    }

    fn heartbeat_component_and_variable() -> (Component, Variable) {
        (
            Component {
                name: "OCPPCommCtrlr".into(),
                instance: None,
                evse: None,
            },
            Variable {
                name: "HeartbeatInterval".into(),
                instance: None,
            },
        )
    }

    async fn set_heartbeat_interval(actor: &ChargePointActor, value: &str) {
        let (component, variable) = heartbeat_component_and_variable();
        actor
            .send(ChargePointEvent::DeviceModel(
                DeviceModelEvent::AttributeValueSet {
                    component,
                    variable,
                    attribute_type: VariableAttributeType::Actual,
                    value: value.into(),
                },
            ))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_heartbeat_sends_at_every_interval_and_never_stops() {
        let sender = CountingHeartbeatSender {
            calls: AtomicUsize::new(0),
        };
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let _ = tokio::time::timeout(
            Duration::from_millis(20),
            run_heartbeat(&sender, &NoopBackoff, &actor, 5),
        )
        .await;

        assert!(sender.calls.load(Ordering::SeqCst) > 1);
    }

    #[tokio::test]
    async fn a_fresh_actor_reports_the_built_in_default_heartbeat_interval() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        assert_eq!(heartbeat_interval_secs(&actor), Some(60));
    }

    #[tokio::test]
    async fn a_set_variables_change_is_reflected_immediately() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        set_heartbeat_interval(&actor, "120").await;

        assert_eq!(heartbeat_interval_secs(&actor), Some(120));
    }

    #[tokio::test]
    async fn an_unparseable_value_falls_back_to_none() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        set_heartbeat_interval(&actor, "not-a-number").await;

        assert_eq!(heartbeat_interval_secs(&actor), None);
    }

    #[tokio::test]
    async fn a_zero_value_falls_back_to_none_rather_than_busy_spinning() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        set_heartbeat_interval(&actor, "0").await;

        assert_eq!(heartbeat_interval_secs(&actor), None);
    }

    #[tokio::test]
    async fn a_missing_actual_attribute_falls_back_to_none() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let (component, variable) = heartbeat_component_and_variable();
        // Re-registers the variable with only a `Target` attribute - no `Actual` at all, the
        // same shape as a hardware binding that never populated it.
        actor
            .send(ChargePointEvent::DeviceModel(
                DeviceModelEvent::VariableRegistered {
                    component,
                    variable,
                    characteristics: crate::state::VariableCharacteristics {
                        data_type: crate::state::VariableDataType::Integer,
                        unit: Some("s".into()),
                        min_limit: None,
                        max_limit: None,
                        values_list: None,
                        supports_monitoring: false,
                    },
                    attributes: alloc::vec![crate::state::VariableAttribute {
                        attribute_type: crate::state::VariableAttributeType::Target,
                        value: "60".into(),
                        mutability: crate::state::VariableMutability::ReadWrite,
                        persistent: true,
                        constant: false,
                        requires_reboot: false,
                    }],
                },
            ))
            .await
            .unwrap();

        assert_eq!(heartbeat_interval_secs(&actor), None);
    }

    #[tokio::test]
    async fn run_heartbeat_picks_up_a_set_variables_change_without_restarting() {
        let sender = CountingHeartbeatSender {
            calls: AtomicUsize::new(0),
        };
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let seen = Arc::new(Mutex::new(alloc::vec::Vec::new()));
        let backoff = RecordingBackoff { seen: seen.clone() };

        let heartbeat_actor = actor.clone();
        let handle = tokio::spawn(async move {
            run_heartbeat(&sender, &backoff, &heartbeat_actor, 999).await;
        });

        tokio::time::sleep(Duration::from_millis(5)).await;
        set_heartbeat_interval(&actor, "5").await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        handle.abort();

        let seen = seen.lock().unwrap();
        assert!(
            seen.contains(&60),
            "expected the built-in default (60s) to have been used before the change: {seen:?}"
        );
        assert!(
            seen.contains(&5),
            "expected the new interval (5s) to have been picked up without restarting: {seen:?}"
        );
    }
}

/// Test-only `BootNotifier`/`HeartbeatSender` fakes shared across this crate's test modules.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{BootNotificationOutcome, BootNotifier, HeartbeatSender};
    use alloc::boxed::Box;

    /// A `BootNotifier` that always returns the same outcome, regardless of vendor/model.
    #[derive(Clone)]
    pub(crate) struct FixedBootNotifier(pub BootNotificationOutcome);

    #[async_trait::async_trait]
    impl BootNotifier for FixedBootNotifier {
        type Error = core::convert::Infallible;

        async fn notify_boot(
            &self,
            _vendor_name: &str,
            _model_name: &str,
        ) -> Result<BootNotificationOutcome, Self::Error> {
            Ok(self.0)
        }
    }

    #[async_trait::async_trait]
    impl HeartbeatSender for FixedBootNotifier {
        type Error = core::convert::Infallible;

        async fn send_heartbeat(&self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::availability::StatusNotifier for FixedBootNotifier {
        type Error = core::convert::Infallible;

        async fn notify_status(
            &self,
            _evse_id: usize,
            _connector_id: usize,
            _status: crate::state::ConnectorStatus,
            _connector_state: crate::state::ConnectorState,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::transactions::TransactionNotifier for FixedBootNotifier {
        type Error = core::convert::Infallible;

        async fn notify_transaction_event(
            &self,
            _evse_id: usize,
            _connector_id: usize,
            _kind: crate::state::TransactionEventKind,
            _transaction: crate::state::Transaction,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::authorization::Authorizer for FixedBootNotifier {
        type Error = core::convert::Infallible;

        async fn authorize(
            &self,
            _id_token: &crate::state::IdToken,
        ) -> Result<crate::state::AuthorizationStatus, Self::Error> {
            Ok(crate::state::AuthorizationStatus::Accepted)
        }
    }

    #[async_trait::async_trait]
    impl crate::remote_control::UnlockConnectorHandler for FixedBootNotifier {
        async fn register_unlock_connector_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[async_trait::async_trait]
    impl crate::availability::ChangeAvailabilityHandler for FixedBootNotifier {
        async fn register_change_availability_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::remote_control::RequestStartTransactionHandler for FixedBootNotifier {
        async fn register_request_start_transaction_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::remote_control::RequestStopTransactionHandler for FixedBootNotifier {
        async fn register_request_stop_transaction_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[cfg(feature = "reservation")]
    #[async_trait::async_trait]
    impl crate::reservation::ReserveNowHandler for FixedBootNotifier {
        async fn register_reserve_now_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[cfg(feature = "reservation")]
    #[async_trait::async_trait]
    impl crate::reservation::CancelReservationHandler for FixedBootNotifier {
        async fn register_cancel_reservation_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::reset::ResetHandler for FixedBootNotifier {
        async fn register_reset_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[cfg(feature = "local-auth-list")]
    #[async_trait::async_trait]
    impl crate::local_authorization_list::SendLocalListHandler for FixedBootNotifier {
        async fn register_send_local_list_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[cfg(feature = "local-auth-list")]
    #[async_trait::async_trait]
    impl crate::local_authorization_list::GetLocalListVersionHandler for FixedBootNotifier {
        async fn register_get_local_list_version_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::device_model::GetVariablesHandler for FixedBootNotifier {
        async fn register_get_variables_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[async_trait::async_trait]
    impl crate::device_model::SetVariablesHandler for FixedBootNotifier {
        async fn register_set_variables_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[async_trait::async_trait]
    impl crate::reporting::GetBaseReportHandler for FixedBootNotifier {
        async fn register_get_base_report_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[async_trait::async_trait]
    impl crate::reporting::GetReportHandler for FixedBootNotifier {
        async fn register_get_report_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[async_trait::async_trait]
    impl crate::security::SecurityEventNotifier for FixedBootNotifier {
        type Error = core::convert::Infallible;

        async fn notify_security_event(
            &self,
            _event_type: &crate::state::SecurityEventType,
            _tech_info: Option<&str>,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[cfg(feature = "tariff-cost")]
    #[async_trait::async_trait]
    impl crate::cost::CostUpdatedHandler for FixedBootNotifier {
        async fn register_cost_updated_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[async_trait::async_trait]
    impl crate::connection::ReconnectHandler for FixedBootNotifier {
        async fn register_reconnect_handler<F, FF>(&self, _callback: F)
        where
            F: FnMut() -> FF + Send + Sync + 'static,
            FF: core::future::Future<Output = ()> + Send + 'static,
        {
        }
    }
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use super::{BootNotificationOutcome, BootNotifier, HeartbeatSender};
    use crate::state::RegistrationStatus;
    use alloc::boxed::Box;
    use ocpp_client::ClientError;
    use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};
    use ocpp_client::ocpp_types::v21::BootNotificationRequest;
    use ocpp_client::ocpp_types::v21::HeartbeatRequest;
    use ocpp_client::ocpp_types::v21::common::{
        BootReasonEnum, ChargingStation, RegistrationStatusEnum,
    };

    pub(super) fn build_request(vendor_name: &str, model_name: &str) -> BootNotificationRequest {
        BootNotificationRequest {
            charging_station: ChargingStation {
                custom_data: None,
                firmware_version: None,
                // Vendor/model names are integrator-supplied configuration, not runtime
                // hardware readings - a name that doesn't fit the OCPP spec's bounded field
                // is a configuration error, not something to recover from at this boundary.
                model: heapless::String::try_from(model_name)
                    .expect("model name must fit in OCPP's 20-character bound"),
                modem: None,
                serial_number: None,
                vendor_name: heapless::String::try_from(vendor_name)
                    .expect("vendor name must fit in OCPP's 50-character bound"),
            },
            custom_data: None,
            reason: BootReasonEnum::PowerUp,
        }
    }

    pub(super) fn map_status(status: RegistrationStatusEnum) -> RegistrationStatus {
        match status {
            RegistrationStatusEnum::Accepted => RegistrationStatus::Accepted,
            RegistrationStatusEnum::Pending => RegistrationStatus::Pending,
            RegistrationStatusEnum::Rejected => RegistrationStatus::Rejected,
        }
    }

    #[async_trait::async_trait]
    impl BootNotifier for OCPP2_1Client {
        type Error = ClientError<OCPP2_1Error>;

        async fn notify_boot(
            &self,
            vendor_name: &str,
            model_name: &str,
        ) -> Result<BootNotificationOutcome, Self::Error> {
            let response = self
                .send_boot_notification(build_request(vendor_name, model_name))
                .await?;

            Ok(BootNotificationOutcome {
                status: map_status(response.status),
                interval_secs: response.interval.max(0) as u32,
            })
        }
    }

    #[async_trait::async_trait]
    impl HeartbeatSender for OCPP2_1Client {
        type Error = ClientError<OCPP2_1Error>;

        async fn send_heartbeat(&self) -> Result<(), Self::Error> {
            self.send_heartbeat(HeartbeatRequest { custom_data: None })
                .await?;
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn request_carries_the_vendor_and_model_as_a_power_up_boot() {
            let request = build_request("Acme", "Charger 9000");

            assert_eq!(request.charging_station.vendor_name, "Acme");
            assert_eq!(request.charging_station.model, "Charger 9000");
            assert_eq!(request.reason, BootReasonEnum::PowerUp);
        }

        #[test]
        fn every_wire_registration_status_maps_to_our_internal_status() {
            assert_eq!(
                map_status(RegistrationStatusEnum::Accepted),
                RegistrationStatus::Accepted
            );
            assert_eq!(
                map_status(RegistrationStatusEnum::Pending),
                RegistrationStatus::Pending
            );
            assert_eq!(
                map_status(RegistrationStatusEnum::Rejected),
                RegistrationStatus::Rejected
            );
        }
    }
}

/// The OCPP 2.0.1 projection of this functional block: identical wire shape to 2.1's (same
/// `ChargingStation`/`BootReasonEnum`/`RegistrationStatusEnum`, since 2.1's Provisioning block
/// didn't change this part of 2.0.1), just targeting `OCPP2_0_1Client` instead.
#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1 {
    use super::{BootNotificationOutcome, BootNotifier, HeartbeatSender};
    use crate::state::RegistrationStatus;
    use alloc::boxed::Box;
    use ocpp_client::ClientError;
    use ocpp_client::ocpp_2_0_1::{OCPP2_0_1Client, OCPP2_0_1Error};
    use ocpp_client::ocpp_types::v201::BootNotificationRequest;
    use ocpp_client::ocpp_types::v201::HeartbeatRequest;
    use ocpp_client::ocpp_types::v201::common::{
        BootReasonEnum, ChargingStation, RegistrationStatusEnum,
    };

    pub(super) fn build_request(vendor_name: &str, model_name: &str) -> BootNotificationRequest {
        BootNotificationRequest {
            charging_station: ChargingStation {
                custom_data: None,
                firmware_version: None,
                // Vendor/model names are integrator-supplied configuration, not runtime
                // hardware readings - a name that doesn't fit the OCPP spec's bounded field
                // is a configuration error, not something to recover from at this boundary.
                model: heapless::String::try_from(model_name)
                    .expect("model name must fit in OCPP's 20-character bound"),
                modem: None,
                serial_number: None,
                vendor_name: heapless::String::try_from(vendor_name)
                    .expect("vendor name must fit in OCPP's 50-character bound"),
            },
            custom_data: None,
            reason: BootReasonEnum::PowerUp,
        }
    }

    pub(super) fn map_status(status: RegistrationStatusEnum) -> RegistrationStatus {
        match status {
            RegistrationStatusEnum::Accepted => RegistrationStatus::Accepted,
            RegistrationStatusEnum::Pending => RegistrationStatus::Pending,
            RegistrationStatusEnum::Rejected => RegistrationStatus::Rejected,
        }
    }

    #[async_trait::async_trait]
    impl BootNotifier for OCPP2_0_1Client {
        type Error = ClientError<OCPP2_0_1Error>;

        async fn notify_boot(
            &self,
            vendor_name: &str,
            model_name: &str,
        ) -> Result<BootNotificationOutcome, Self::Error> {
            let response = self
                .send_boot_notification(build_request(vendor_name, model_name))
                .await?;

            Ok(BootNotificationOutcome {
                status: map_status(response.status),
                interval_secs: response.interval.max(0) as u32,
            })
        }
    }

    #[async_trait::async_trait]
    impl HeartbeatSender for OCPP2_0_1Client {
        type Error = ClientError<OCPP2_0_1Error>;

        async fn send_heartbeat(&self) -> Result<(), Self::Error> {
            self.send_heartbeat(HeartbeatRequest { custom_data: None })
                .await?;
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn request_carries_the_vendor_and_model_as_a_power_up_boot() {
            let request = build_request("Acme", "Charger 9000");

            assert_eq!(request.charging_station.vendor_name, "Acme");
            assert_eq!(request.charging_station.model, "Charger 9000");
            assert_eq!(request.reason, BootReasonEnum::PowerUp);
        }

        #[test]
        fn every_wire_registration_status_maps_to_our_internal_status() {
            assert_eq!(
                map_status(RegistrationStatusEnum::Accepted),
                RegistrationStatus::Accepted
            );
            assert_eq!(
                map_status(RegistrationStatusEnum::Pending),
                RegistrationStatus::Pending
            );
            assert_eq!(
                map_status(RegistrationStatusEnum::Rejected),
                RegistrationStatus::Rejected
            );
        }
    }
}

/// The OCPP 1.6J projection of this functional block: the same internal `RegistrationStatus`/
/// `BootNotificationOutcome`/`HeartbeatSender` model §0's version-adapter goal calls for,
/// targeting `OCPP1_6Client` instead of `OCPP2_1Client`. 1.6J's `BootNotification.req` has no
/// `reason` field (that's a 2.x addition) and flattens `charging_station.vendor_name`/`model`
/// into top-level `charge_point_vendor`/`charge_point_model` - otherwise the same shape and the
/// same three-value registration status, so `map_status` differs only in its wire enum type.
#[cfg(feature = "ocpp_1_6")]
mod ocpp_1_6 {
    use super::{BootNotificationOutcome, BootNotifier, HeartbeatSender};
    use crate::state::RegistrationStatus;
    use alloc::boxed::Box;
    use ocpp_client::ClientError;
    use ocpp_client::ocpp_1_6::{OCPP1_6Client, OCPP1_6Error};
    use ocpp_client::ocpp_types::v16::BootNotificationRequest;
    use ocpp_client::ocpp_types::v16::HeartbeatRequest;
    use ocpp_client::ocpp_types::v16::common::BootNotificationResponseStatus;

    pub(super) fn build_request(vendor_name: &str, model_name: &str) -> BootNotificationRequest {
        BootNotificationRequest {
            charge_box_serial_number: None,
            // Vendor/model names are integrator-supplied configuration, not runtime hardware
            // readings - a name that doesn't fit the OCPP spec's bounded field is a
            // configuration error, not something to recover from at this boundary.
            charge_point_model: heapless::String::try_from(model_name)
                .expect("model name must fit in OCPP's 20-character bound"),
            charge_point_serial_number: None,
            charge_point_vendor: heapless::String::try_from(vendor_name)
                .expect("vendor name must fit in OCPP's 20-character bound"),
            firmware_version: None,
            iccid: None,
            imsi: None,
            meter_serial_number: None,
            meter_type: None,
        }
    }

    pub(super) fn map_status(status: BootNotificationResponseStatus) -> RegistrationStatus {
        match status {
            BootNotificationResponseStatus::Accepted => RegistrationStatus::Accepted,
            BootNotificationResponseStatus::Pending => RegistrationStatus::Pending,
            BootNotificationResponseStatus::Rejected => RegistrationStatus::Rejected,
        }
    }

    #[async_trait::async_trait]
    impl BootNotifier for OCPP1_6Client {
        type Error = ClientError<OCPP1_6Error>;

        async fn notify_boot(
            &self,
            vendor_name: &str,
            model_name: &str,
        ) -> Result<BootNotificationOutcome, Self::Error> {
            let response = self
                .send_boot_notification(build_request(vendor_name, model_name))
                .await?;

            Ok(BootNotificationOutcome {
                status: map_status(response.status),
                interval_secs: response.interval.max(0) as u32,
            })
        }
    }

    #[async_trait::async_trait]
    impl HeartbeatSender for OCPP1_6Client {
        type Error = ClientError<OCPP1_6Error>;

        async fn send_heartbeat(&self) -> Result<(), Self::Error> {
            self.send_heartbeat(HeartbeatRequest {}).await?;
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn request_carries_the_vendor_and_model() {
            let request = build_request("Acme", "Charger 9000");

            assert_eq!(request.charge_point_vendor, "Acme");
            assert_eq!(request.charge_point_model, "Charger 9000");
        }

        #[test]
        fn every_wire_registration_status_maps_to_our_internal_status() {
            assert_eq!(
                map_status(BootNotificationResponseStatus::Accepted),
                RegistrationStatus::Accepted
            );
            assert_eq!(
                map_status(BootNotificationResponseStatus::Pending),
                RegistrationStatus::Pending
            );
            assert_eq!(
                map_status(BootNotificationResponseStatus::Rejected),
                RegistrationStatus::Rejected
            );
        }
    }
}
