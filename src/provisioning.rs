//! Provisioning functional block: BootNotification and charge-point registration with the
//! CSMS. See `docs/ROADMAP.md` §2.

use crate::state::RegistrationStatus;
use alloc::boxed::Box;
#[cfg(feature = "tokio-runtime")]
use core::time::Duration;

/// Default interval to wait before retrying BootNotification after a transport-level failure
/// (as opposed to a `Pending`/`Rejected` response, which carries its own retry interval).
pub const DEFAULT_RETRY_INTERVAL_SECS: u32 = 30;

/// The CSMS's answer to a BootNotification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootNotificationOutcome {
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
    type Error: core::error::Error + Send + Sync + 'static;

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
    type Error: core::error::Error + Send + Sync + 'static;

    async fn send_heartbeat(&self) -> Result<(), Self::Error>;
}

/// Sends a Heartbeat every `interval_secs` (the interval an accepted BootNotification
/// returned), forever. Errors are logged and do not stop the loop - the next heartbeat is
/// still due at the regular interval, per OCPP.
pub async fn run_heartbeat<H: HeartbeatSender, B: Backoff>(
    sender: &H,
    backoff: &B,
    interval_secs: u32,
) {
    loop {
        backoff.wait(interval_secs).await;
        if let Err(err) = sender.send_heartbeat().await {
            tracing::warn!(error = %err, "heartbeat failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Backoff, HeartbeatSender, run_heartbeat};
    use alloc::boxed::Box;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use core::time::Duration;

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

    #[tokio::test]
    async fn run_heartbeat_sends_at_every_interval_and_never_stops() {
        let sender = CountingHeartbeatSender {
            calls: AtomicUsize::new(0),
        };

        let _ = tokio::time::timeout(
            Duration::from_millis(20),
            run_heartbeat(&sender, &NoopBackoff, 5),
        )
        .await;

        assert!(sender.calls.load(Ordering::SeqCst) > 1);
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
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use super::{BootNotificationOutcome, BootNotifier, HeartbeatSender};
    use crate::state::RegistrationStatus;
    use alloc::boxed::Box;
    use alloc::string::ToString;
    use ocpp_client::ClientError;
    use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};
    use ocpp_client::rust_ocpp::v2_1::enumerations::{
        BootReasonEnumType, RegistrationStatusEnumType,
    };
    use ocpp_client::rust_ocpp::v2_1::messages::boot_notification::{
        BootNotificationRequest, ChargingStationType,
    };
    use ocpp_client::rust_ocpp::v2_1::messages::heartbeat::HeartbeatRequest;

    pub(super) fn build_request(vendor_name: &str, model_name: &str) -> BootNotificationRequest {
        BootNotificationRequest {
            charging_station: ChargingStationType {
                custom_data: None,
                firmware_version: None,
                model: model_name.to_string(),
                modem: None,
                serial_number: None,
                vendor_name: vendor_name.to_string(),
            },
            custom_data: None,
            reason: BootReasonEnumType::PowerUp,
        }
    }

    pub(super) fn map_status(status: RegistrationStatusEnumType) -> RegistrationStatus {
        match status {
            RegistrationStatusEnumType::Accepted => RegistrationStatus::Accepted,
            RegistrationStatusEnumType::Pending => RegistrationStatus::Pending,
            RegistrationStatusEnumType::Rejected => RegistrationStatus::Rejected,
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
            assert_eq!(request.reason, BootReasonEnumType::PowerUp);
        }

        #[test]
        fn every_wire_registration_status_maps_to_our_internal_status() {
            assert_eq!(
                map_status(RegistrationStatusEnumType::Accepted),
                RegistrationStatus::Accepted
            );
            assert_eq!(
                map_status(RegistrationStatusEnumType::Pending),
                RegistrationStatus::Pending
            );
            assert_eq!(
                map_status(RegistrationStatusEnumType::Rejected),
                RegistrationStatus::Rejected
            );
        }
    }
}
