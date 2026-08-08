use crate::ChargePointRuntime;
use crate::availability::{ChangeAvailabilityHandler, StatusNotifier};
use crate::builder::ChargePointBuilder;
use crate::clock::MonotonicClock;
use crate::connection::ReconnectHandler;
use crate::cost::CostUpdatedHandler;
use crate::device_model::{GetVariablesHandler, SetVariablesHandler};
use crate::executor::Executor;
use crate::hardware::ChargePoint;
use crate::hardware::Connector;
use crate::hardware::Evse;
use crate::local_authorization_list::{GetLocalListVersionHandler, SendLocalListHandler};
use crate::provisioning::{Backoff, BootNotifier, HeartbeatSender};
use crate::remote_control::{
    RequestStartTransactionHandler, RequestStopTransactionHandler, TriggerMessageHandler,
    UnlockConnectorHandler,
};
use crate::reporting::{GetBaseReportHandler, GetReportHandler};
use crate::reservation::{CancelReservationHandler, ReserveNowHandler};
use crate::reset::ResetHandler;
use crate::security::SecurityEventNotifier;
use crate::smart_charging::{
    ClearChargingProfileHandler, GetCompositeScheduleHandler, SetChargingProfileHandler,
};
use crate::transactions::TransactionNotifier;

/// Starts the hardware, then runs the Provisioning functional block's BootNotification
/// exchange (retrying with `backoff` on `Pending`/`Rejected` or a transport failure - see
/// [`ChargePointRuntime::register_until_accepted`]). Once accepted, uses `executor` to spawn
/// background tasks that send a Heartbeat at the interval the CSMS returned, forward every
/// connector status change to the CSMS via StatusNotification, forward every transaction
/// lifecycle event via TransactionEvent, answer every presented-id-token authorization
/// request via Authorize, and forward every reported security event via
/// SecurityEventNotification, for as long as the process runs.
///
/// `executor`/`backoff`/`monotonic`/`clock` are caller-supplied (rather than defaulting to tokio) so this
/// function doesn't hard-depend on an async runtime - std/tokio users can pass
/// [`crate::executor::TokioExecutor`]/[`crate::provisioning::TokioBackoff`]/
/// [`crate::clock::SystemMonotonicClock`]/[`crate::clock::SystemClock`]; embedded targets supply
/// their own. `clock` is what the Smart Charging block composes schedules against (B2), and
/// `monotonic` anchors
/// BootNotification/Heartbeat `currentTime` sync - see
/// [`crate::builder::ChargePointBuilder::provisioning`]'s docs.
///
/// This is a thin "everything on" wrapper around [`ChargePointBuilder`], registering every
/// functional block it exposes in the same order this function has always used. Callers whose
/// CSMS client only implements a subset of blocks - or who want to skip a block outright - should
/// use [`ChargePointBuilder`] directly instead; `N`'s single 24-trait bound below is exactly the
/// limitation the builder exists to remove.
pub async fn setup<T, E, C, N, X, B, M, K>(
    charge_point: T,
    csms: N,
    executor: X,
    backoff: B,
    monotonic: M,
    clock: K,
) -> Result<ChargePointRuntime<T>, T::StartError>
where
    T: ChargePoint<E, C>,
    E: Evse<C>,
    C: Connector,
    N: BootNotifier
        + crate::authorization::ClearCacheHandler
        + crate::network_profile::SetNetworkProfileHandler
        + HeartbeatSender
        + StatusNotifier
        + TransactionNotifier
        + crate::authorization::Authorizer
        + UnlockConnectorHandler
        + ChangeAvailabilityHandler
        + RequestStartTransactionHandler
        + RequestStopTransactionHandler
        + TriggerMessageHandler
        + ReserveNowHandler
        + CancelReservationHandler
        + ResetHandler
        + SendLocalListHandler
        + GetLocalListVersionHandler
        + GetVariablesHandler
        + SetVariablesHandler
        + GetBaseReportHandler
        + GetReportHandler
        + SecurityEventNotifier
        + CostUpdatedHandler
        + crate::meter_values::MeterValuesNotifier
        + SetChargingProfileHandler
        + ClearChargingProfileHandler
        + GetCompositeScheduleHandler
        + ReconnectHandler
        + Clone
        + Send
        + Sync
        + 'static,
    X: Executor,
    B: Backoff + Clone + Send + Sync + 'static,
    M: MonotonicClock + Clone + Send + Sync + 'static,
    K: crate::clock::Clock + Clone + Send + Sync + 'static,
{
    let mut builder = ChargePointBuilder::start(charge_point, executor)
        .await?
        .provisioning(&csms, backoff.clone(), monotonic)
        .await
        .status_notifications(&csms)
        .await
        .transaction_events(&csms)
        .await
        .authorization(&csms, clock.clone())
        .await
        .clear_cache(&csms)
        .await
        .network_profiles(&csms)
        .await
        .security_events(&csms)
        .await
        .remote_control(&csms)
        .await
        .trigger_message(&csms)
        .await
        .availability_control(&csms)
        .await
        .reset(&csms)
        .await
        .device_model(&csms)
        .await
        .meter_values(&csms, backoff.clone(), clock.clone())
        .await;

    // C3.1 (docs/PRODUCTION-ROADMAP.md §5.3): each of these blocks is only registered when the
    // hardware actually declares the matching capability - an absent capability means the CSMS
    // gets `NotImplemented` from `ocpp-client` rather than a handler that was wired up but backed
    // by hardware that can't do the thing. `capabilities()` is the single source of truth every
    // other C3 surface (device model, `SupportedFeatureProfiles`, `*Ctrlr.Available`) also reads.
    let capabilities = builder.capabilities();
    if capabilities.reservation {
        builder = builder.reservation(&csms).await;
    }
    if capabilities.local_auth_list {
        builder = builder.local_authorization_list(&csms).await;
    }
    if capabilities.tariff_and_cost {
        builder = builder.cost(&csms).await;
    }
    if capabilities.smart_charging {
        builder = builder
            .smart_charging(
                &csms,
                alloc::sync::Arc::new(crate::smart_charging::ChargingLimitProjection::new()),
                clock,
                backoff.clone(),
            )
            .await;
    }

    // A7: sweep every queue registered above on OCPP's own MessageAttemptInterval, so a charge
    // point that goes quiet mid-outage still retries what it queued. 60 s is the fallback when
    // the device model has no usable value.
    Ok(builder.offline_queue_retries(backoff, 60).build())
}

#[cfg(test)]
mod tests {
    use super::setup;
    use crate::builder::test_support::{
        TestChargePoint, TestConnector, TestEvse, WithCapabilities,
    };
    use crate::executor::TokioExecutor;
    use crate::provisioning::BootNotificationOutcome;
    use crate::provisioning::TokioBackoff;
    use crate::provisioning::test_support::FixedBootNotifier;
    use crate::state::{ConnectorState, RegistrationStatus};
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, Ordering};

    fn accepted_boot_notifier() -> FixedBootNotifier {
        FixedBootNotifier(BootNotificationOutcome {
            status: RegistrationStatus::Accepted,
            interval_secs: 60,
            current_time: None,
        })
    }

    #[tokio::test]
    async fn setup_routes_startup_hardware_events_into_runtime_state() {
        let locked = Arc::new(AtomicBool::new(false));
        let runtime = setup(
            TestChargePoint {
                evses: [TestEvse {
                    connectors: [TestConnector {
                        locked: locked.clone(),
                        lock_succeeds: true,
                    }],
                }],
            },
            accepted_boot_notifier(),
            TokioExecutor,
            TokioBackoff,
            crate::clock::SystemMonotonicClock,
            crate::clock::SystemClock,
        )
        .await
        .unwrap();

        assert_eq!(
            runtime.state().evses[0].connectors[0],
            ConnectorState::Locked
        );
        assert!(locked.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_failed_hardware_command_reports_a_connector_fault() {
        let locked = Arc::new(AtomicBool::new(false));
        let runtime = setup(
            TestChargePoint {
                evses: [TestEvse {
                    connectors: [TestConnector {
                        locked: locked.clone(),
                        lock_succeeds: false,
                    }],
                }],
            },
            accepted_boot_notifier(),
            TokioExecutor,
            TokioBackoff,
            crate::clock::SystemMonotonicClock,
            crate::clock::SystemClock,
        )
        .await
        .unwrap();

        assert_eq!(
            runtime.state().evses[0].connectors[0],
            ConnectorState::Faulted
        );
        assert!(!locked.load(Ordering::SeqCst));
    }

    /// A CSMS fake that behaves exactly like [`FixedBootNotifier`] (accepts every registration
    /// silently) but also records whether the Reservation/Local-Auth-List/Tariff-and-Cost
    /// registration methods were actually called, the only three
    /// [`crate::hardware::CAPABILITY_GATES`] entries that gate a real handler today (see
    /// `has_handler`'s docs). Used by the keystone C3.5 test below to observe C3.1 (handler
    /// registration) the same way the other three surfaces are observed: through the real
    /// production code path, not a re-implementation of it.
    #[derive(Clone)]
    struct RecordingCsms {
        boot: FixedBootNotifier,
        reservation_registered: Arc<AtomicBool>,
        local_auth_list_registered: Arc<AtomicBool>,
        cost_registered: Arc<AtomicBool>,
        smart_charging_registered: Arc<AtomicBool>,
    }

    impl RecordingCsms {
        fn new() -> Self {
            Self {
                boot: accepted_boot_notifier(),
                reservation_registered: Arc::new(AtomicBool::new(false)),
                local_auth_list_registered: Arc::new(AtomicBool::new(false)),
                cost_registered: Arc::new(AtomicBool::new(false)),
                smart_charging_registered: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::provisioning::BootNotifier for RecordingCsms {
        type Error = core::convert::Infallible;
        async fn notify_boot(
            &self,
            vendor_name: &str,
            model_name: &str,
            reason: Option<crate::state::BootReasonCause>,
        ) -> Result<BootNotificationOutcome, Self::Error> {
            self.boot.notify_boot(vendor_name, model_name, reason).await
        }
    }

    #[async_trait::async_trait]
    impl crate::provisioning::HeartbeatSender for RecordingCsms {
        type Error = core::convert::Infallible;
        async fn send_heartbeat(
            &self,
        ) -> Result<Option<chrono::DateTime<chrono::Utc>>, Self::Error> {
            Ok(None)
        }
    }

    #[async_trait::async_trait]
    impl crate::availability::StatusNotifier for RecordingCsms {
        type Error = core::convert::Infallible;
        async fn notify_status(
            &self,
            _evse_id: usize,
            _connector_id: usize,
            _status: crate::state::ConnectorStatus,
            _connector_state: ConnectorState,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::transactions::TransactionNotifier for RecordingCsms {
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
    impl crate::authorization::Authorizer for RecordingCsms {
        type Error = core::convert::Infallible;
        async fn authorize(
            &self,
            _id_token: &crate::state::IdToken,
        ) -> Result<crate::state::AuthorizationStatus, Self::Error> {
            Ok(crate::state::AuthorizationStatus::Accepted)
        }
    }

    #[async_trait::async_trait]
    impl crate::remote_control::UnlockConnectorHandler for RecordingCsms {
        async fn register_unlock_connector_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[async_trait::async_trait]
    impl crate::availability::ChangeAvailabilityHandler for RecordingCsms {
        async fn register_change_availability_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::remote_control::RequestStartTransactionHandler for RecordingCsms {
        async fn register_request_start_transaction_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::remote_control::RequestStopTransactionHandler for RecordingCsms {
        async fn register_request_stop_transaction_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::reservation::ReserveNowHandler for RecordingCsms {
        async fn register_reserve_now_handler(&self, _actor: crate::actor::ChargePointActor) {
            self.reservation_registered.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl crate::reservation::CancelReservationHandler for RecordingCsms {
        async fn register_cancel_reservation_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::reset::ResetHandler for RecordingCsms {
        async fn register_reset_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[async_trait::async_trait]
    impl crate::local_authorization_list::SendLocalListHandler for RecordingCsms {
        async fn register_send_local_list_handler(&self, _actor: crate::actor::ChargePointActor) {
            self.local_auth_list_registered
                .store(true, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl crate::local_authorization_list::GetLocalListVersionHandler for RecordingCsms {
        async fn register_get_local_list_version_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::device_model::GetVariablesHandler for RecordingCsms {
        async fn register_get_variables_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[async_trait::async_trait]
    impl crate::device_model::SetVariablesHandler for RecordingCsms {
        async fn register_set_variables_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[async_trait::async_trait]
    impl crate::reporting::GetBaseReportHandler for RecordingCsms {
        async fn register_get_base_report_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[async_trait::async_trait]
    impl crate::reporting::GetReportHandler for RecordingCsms {
        async fn register_get_report_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[async_trait::async_trait]
    impl crate::security::SecurityEventNotifier for RecordingCsms {
        type Error = core::convert::Infallible;
        async fn notify_security_event(
            &self,
            _event_type: &crate::state::SecurityEventType,
            _tech_info: Option<&str>,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::cost::CostUpdatedHandler for RecordingCsms {
        async fn register_cost_updated_handler(&self, _actor: crate::actor::ChargePointActor) {
            self.cost_registered.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl crate::network_profile::SetNetworkProfileHandler for RecordingCsms {
        async fn register_set_network_profile_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::authorization::ClearCacheHandler for RecordingCsms {
        async fn register_clear_cache_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[async_trait::async_trait]
    impl crate::remote_control::TriggerMessageHandler for RecordingCsms {
        async fn register_trigger_message_handler(&self, _actor: crate::actor::ChargePointActor) {}
    }

    #[async_trait::async_trait]
    impl crate::meter_values::MeterValuesNotifier for RecordingCsms {
        type Error = core::convert::Infallible;

        async fn send_meter_values(
            &self,
            _evse_id: usize,
            _connector_id: usize,
            _sample: crate::state::MeterSample,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::smart_charging::SetChargingProfileHandler for RecordingCsms {
        async fn register_set_charging_profile_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
            self.smart_charging_registered.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl crate::smart_charging::ClearChargingProfileHandler for RecordingCsms {
        async fn register_clear_charging_profile_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::smart_charging::GetCompositeScheduleHandler for RecordingCsms {
        async fn register_get_composite_schedule_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
            _projection: alloc::sync::Arc<crate::smart_charging::ChargingLimitProjection>,
        ) {
        }
    }

    #[async_trait::async_trait]
    impl crate::connection::ReconnectHandler for RecordingCsms {
        async fn register_reconnect_handler<F, FF>(&self, _callback: F)
        where
            F: FnMut() -> FF + Send + Sync + 'static,
            FF: core::future::Future<Output = ()> + Send + 'static,
        {
        }
    }

    /// The keystone test (C3.5, `docs/PRODUCTION-ROADMAP.md` §5.3): for every entry in
    /// [`crate::hardware::CAPABILITY_GATES`], with the capability both present and absent, checks
    /// that every advertisement surface that entry names agrees with the capability set - all
    /// through the real production code paths (`setup()` for handler registration, the actual
    /// `ChargePointState::device_model` `setup()` populated for the device model, and the same
    /// [`crate::hardware::supported_feature_profiles_1_6`] function the 1.6J `GetConfiguration`
    /// handler calls). Data-driven over the table itself, not a hardcoded list of capabilities, so
    /// a capability added to [`crate::hardware::CAPABILITY_GATES`] without being reflected
    /// everywhere fails this test rather than silently drifting.
    #[tokio::test]
    async fn all_four_capability_propagation_surfaces_agree_with_the_capability_set() {
        for gate in crate::hardware::CAPABILITY_GATES {
            for enabled in [true, false] {
                let capabilities = match gate.name {
                    "reservation" => crate::hardware::Capabilities {
                        reservation: enabled,
                        ..Default::default()
                    },
                    "local_auth_list" => crate::hardware::Capabilities {
                        local_auth_list: enabled,
                        ..Default::default()
                    },
                    "tariff_and_cost" => crate::hardware::Capabilities {
                        tariff_and_cost: enabled,
                        ..Default::default()
                    },
                    "smart_charging" => crate::hardware::Capabilities {
                        smart_charging: enabled,
                        ..Default::default()
                    },
                    "has_display" => crate::hardware::Capabilities {
                        has_display: enabled,
                        ..Default::default()
                    },
                    "firmware_management" => crate::hardware::Capabilities {
                        firmware_management: enabled,
                        ..Default::default()
                    },
                    other => panic!(
                        "CAPABILITY_GATES grew a new entry (`{other}`) this test doesn't know \
                         how to set yet - extend the match above so it stays data-driven"
                    ),
                };
                assert_eq!((gate.enabled)(&capabilities), enabled);

                let csms = RecordingCsms::new();
                let runtime = setup(
                    WithCapabilities {
                        inner: TestChargePoint {
                            evses: [TestEvse {
                                connectors: [TestConnector {
                                    locked: Arc::new(AtomicBool::new(false)),
                                    lock_succeeds: true,
                                }],
                            }],
                        },
                        capabilities,
                    },
                    csms.clone(),
                    TokioExecutor,
                    TokioBackoff,
                    crate::clock::SystemMonotonicClock,
                    crate::clock::SystemClock,
                )
                .await
                .unwrap();

                // Surface 1: handler registration (C3.1) - only meaningful for gates that
                // actually gate a handler today.
                if gate.has_handler {
                    let registered = match gate.name {
                        "reservation" => csms.reservation_registered.load(Ordering::SeqCst),
                        "local_auth_list" => csms.local_auth_list_registered.load(Ordering::SeqCst),
                        "tariff_and_cost" => csms.cost_registered.load(Ordering::SeqCst),
                        "smart_charging" => csms.smart_charging_registered.load(Ordering::SeqCst),
                        other => panic!("gate `{other}` claims has_handler but isn't wired here"),
                    };
                    assert_eq!(
                        registered, enabled,
                        "gate `{}`: handler registration ({registered}) disagreed with the \
                         capability set ({enabled})",
                        gate.name
                    );
                }

                // Surface 2: the 2.x device model's `*Ctrlr.Available` variable (C3.2/C3.4).
                if let Some(ctrlr_component) = gate.ctrlr_component {
                    let component = crate::state::Component {
                        name: ctrlr_component.into(),
                        instance: None,
                        evse: None,
                    };
                    let variable = crate::state::Variable {
                        name: "Available".into(),
                        instance: None,
                    };
                    let state = runtime.state();
                    let definition = state
                        .device_model
                        .get(&component, &variable)
                        .unwrap_or_else(|| {
                            panic!("gate `{}`: {ctrlr_component}.Available was never registered in the device model", gate.name)
                        });
                    let actual = &definition
                        .attribute(crate::state::VariableAttributeType::Actual)
                        .unwrap()
                        .value;
                    assert_eq!(
                        actual,
                        &enabled.to_string(),
                        "gate `{}`: {ctrlr_component}.Available ({actual}) disagreed with the \
                         capability set ({enabled})",
                        gate.name
                    );
                }

                // Surface 3: 1.6J `SupportedFeatureProfiles` (C3.3) - the exact function the 1.6J
                // `GetConfiguration` handler calls.
                if let Some(profile) = gate.feature_profile_1_6 {
                    let profiles = crate::hardware::supported_feature_profiles_1_6(&capabilities);
                    let listed = profiles.split(',').any(|name| name == profile);
                    assert_eq!(
                        listed, enabled,
                        "gate `{}`: SupportedFeatureProfiles ({profiles:?}) disagreed with the \
                         capability set ({enabled}) for profile `{profile}`",
                        gate.name
                    );
                }
            }
        }
    }
}
