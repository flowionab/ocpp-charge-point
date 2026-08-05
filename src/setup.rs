use crate::authorization::{run_authorization_requests, Authorizer};
use crate::availability::{ChangeAvailabilityHandler, DedupedStatusNotifier, StatusNotifier};
use crate::connection::{reregister_on_reconnect, ReconnectHandler};
use crate::cost::CostUpdatedHandler;
use crate::device_model::{GetVariablesHandler, SetVariablesHandler};
use crate::executor::Executor;
use crate::hardware::ChargePoint;
use crate::hardware::Connector;
use crate::hardware::Evse;
use crate::local_authorization_list::{GetLocalListVersionHandler, SendLocalListHandler};
use crate::offline_queue::{run_with_offline_queue, OfflineQueue};
use crate::provisioning::{run_heartbeat, Backoff, BootNotifier, HeartbeatSender};
use crate::remote_control::{
    RequestStartTransactionHandler, RequestStopTransactionHandler, UnlockConnectorHandler,
};
use crate::reservation::{CancelReservationHandler, ReserveNowHandler};
use crate::security::SecurityEventNotifier;
use crate::transactions::TransactionNotifier;
use crate::ChargePointRuntime;
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

/// Starts the hardware, then runs the Provisioning functional block's BootNotification
/// exchange (retrying with `backoff` on `Pending`/`Rejected` or a transport failure - see
/// [`ChargePointRuntime::register_until_accepted`]). Once accepted, uses `executor` to spawn
/// background tasks that send a Heartbeat at the interval the CSMS returned, forward every
/// connector status change to the CSMS via StatusNotification, forward every transaction
/// lifecycle event via TransactionEvent, answer every presented-id-token authorization
/// request via Authorize, and forward every reported security event via
/// SecurityEventNotification, for as long as the process runs.
///
/// `executor`/`backoff` are caller-supplied (rather than defaulting to tokio) so this function
/// doesn't hard-depend on an async runtime - std/tokio users can pass
/// [`crate::executor::TokioExecutor`]/[`crate::provisioning::TokioBackoff`]; embedded targets
/// supply their own.
pub async fn setup<T, E, C, N, X, B>(
    charge_point: T,
    csms: N,
    executor: X,
    backoff: B,
) -> Result<ChargePointRuntime<T>, T::StartError>
where
    T: ChargePoint<E, C>,
    E: Evse<C>,
    C: Connector,
    N: BootNotifier
        + HeartbeatSender
        + StatusNotifier
        + TransactionNotifier
        + Authorizer
        + UnlockConnectorHandler
        + ChangeAvailabilityHandler
        + RequestStartTransactionHandler
        + RequestStopTransactionHandler
        + ReserveNowHandler
        + CancelReservationHandler
        + SendLocalListHandler
        + GetLocalListVersionHandler
        + GetVariablesHandler
        + SetVariablesHandler
        + SecurityEventNotifier
        + CostUpdatedHandler
        + ReconnectHandler
        + Clone
        + Send
        + Sync
        + 'static,
    X: Executor,
    B: Backoff + Clone + Send + Sync + 'static,
{
    tracing::info!(
        vendor = charge_point.vendor_name().await,
        model = charge_point.model_name().await,
        "Initializing charger"
    );

    let mut connector_counts = Vec::new();
    for evse in charge_point.evses().await {
        connector_counts.push(evse.connectors().await.len());
    }

    let runtime = ChargePointRuntime::new(charge_point, connector_counts, &executor);
    // Subscribe before starting the hardware so status/transaction/authorization events fired
    // during `start()` (e.g. a connector that's already occupied at boot) are buffered rather
    // than lost.
    let status_changes = runtime.subscribe_status_notifications();
    let transaction_events = runtime.subscribe_transaction_events();
    let authorization_requests = runtime.subscribe_authorization_requests();
    let security_events = runtime.subscribe_security_events();
    runtime
        .hardware()
        .start(runtime.hardware_events(), runtime.hardware_commands())
        .await?;

    let hardware = runtime.hardware();
    let vendor_name = hardware.vendor_name().await;
    let model_name = hardware.model_name().await;
    let outcome = runtime
        .register_until_accepted(&csms, &backoff, vendor_name, model_name)
        .await;

    reregister_on_reconnect(
        runtime.actor(),
        csms.clone(),
        backoff.clone(),
        vendor_name.into(),
        model_name.into(),
    )
    .await;

    let heartbeat_sender = csms.clone();
    let heartbeat_backoff = backoff.clone();
    executor.spawn(Box::pin(async move {
        run_heartbeat(&heartbeat_sender, &heartbeat_backoff, outcome.interval_secs).await;
    }));

    // Wrapped in `DedupedStatusNotifier` so `csms` only sees a wire-visible status change, not
    // every internal `ConnectorState` transition `ChargePointState` now reports (see
    // `docs/ROADMAP.md` §0) - restoring the cadence `setup()`'s csms types (2.1, 2.0.1) had
    // before that change, since neither has a status richer than `ConnectorStatus` to justify
    // seeing the extra calls. Wrapped again in `Arc` so the same dedup cache (and the same
    // `OfflineQueue`, below) is shared between the live forwarder task and the reconnect-flush
    // closure - cloning the `Arc` per call is cheap and keeps both paths consistent, unlike
    // constructing a second `DedupedStatusNotifier` with its own empty cache would.
    //
    // Each of Status/Transaction/Security also goes through an `OfflineQueue`: a report that
    // fails to send (e.g. the CSMS connection is currently down) is queued and retried, in
    // order, rather than dropped - both whenever the next report comes in and, via
    // `register_reconnect_handler` below, as soon as the connection itself comes back, so a
    // queued report doesn't wait indefinitely for an unrelated event to trigger a retry.
    let status_queue = Arc::new(OfflineQueue::new());
    let status_notifier = Arc::new(DedupedStatusNotifier::new(csms.clone()));
    let forwarder_queue = status_queue.clone();
    let forwarder_notifier = status_notifier.clone();
    executor.spawn(Box::pin(async move {
        run_with_offline_queue(status_changes, &forwarder_queue, move |changed| {
            let notifier = forwarder_notifier.clone();
            async move {
                notifier
                    .notify_status(
                        changed.evse_id,
                        changed.connector_id,
                        changed.status,
                        changed.connector_state,
                    )
                    .await
            }
        })
        .await;
    }));
    csms.register_reconnect_handler(move || {
        let queue = status_queue.clone();
        let notifier = status_notifier.clone();
        async move {
            crate::offline_queue::flush_offline_queue(&queue, move |changed| {
                let notifier = notifier.clone();
                async move {
                    notifier
                        .notify_status(
                            changed.evse_id,
                            changed.connector_id,
                            changed.status,
                            changed.connector_state,
                        )
                        .await
                }
            })
            .await;
        }
    })
    .await;

    let transaction_queue = Arc::new(OfflineQueue::new());
    let forwarder_queue = transaction_queue.clone();
    let forwarder_csms = csms.clone();
    executor.spawn(Box::pin(async move {
        run_with_offline_queue(transaction_events, &forwarder_queue, move |occurred| {
            let notifier = forwarder_csms.clone();
            async move {
                notifier
                    .notify_transaction_event(
                        occurred.evse_id,
                        occurred.connector_id,
                        occurred.kind,
                        occurred.transaction,
                    )
                    .await
            }
        })
        .await;
    }));
    let reconnect_csms = csms.clone();
    csms.register_reconnect_handler(move || {
        let queue = transaction_queue.clone();
        let csms = reconnect_csms.clone();
        async move {
            crate::offline_queue::flush_offline_queue(&queue, move |occurred| {
                let notifier = csms.clone();
                async move {
                    notifier
                        .notify_transaction_event(
                            occurred.evse_id,
                            occurred.connector_id,
                            occurred.kind,
                            occurred.transaction,
                        )
                        .await
                }
            })
            .await;
        }
    })
    .await;

    let authorizer = csms.clone();
    let actor = runtime.actor();
    executor.spawn(Box::pin(async move {
        run_authorization_requests(authorization_requests, &authorizer, actor).await;
    }));

    let security_queue = Arc::new(OfflineQueue::new());
    let forwarder_queue = security_queue.clone();
    let forwarder_csms = csms.clone();
    executor.spawn(Box::pin(async move {
        run_with_offline_queue(security_events, &forwarder_queue, move |event| {
            let notifier = forwarder_csms.clone();
            async move {
                notifier
                    .notify_security_event(&event.event_type, event.tech_info.as_deref())
                    .await
            }
        })
        .await;
    }));
    let reconnect_csms = csms.clone();
    csms.register_reconnect_handler(move || {
        let queue = security_queue.clone();
        let csms = reconnect_csms.clone();
        async move {
            crate::offline_queue::flush_offline_queue(&queue, move |event| {
                let notifier = csms.clone();
                async move {
                    notifier
                        .notify_security_event(&event.event_type, event.tech_info.as_deref())
                        .await
                }
            })
            .await;
        }
    })
    .await;

    csms.register_unlock_connector_handler(runtime.actor())
        .await;
    csms.register_change_availability_handler(runtime.actor())
        .await;
    csms.register_request_start_transaction_handler(runtime.actor())
        .await;
    csms.register_request_stop_transaction_handler(runtime.actor())
        .await;
    csms.register_reserve_now_handler(runtime.actor()).await;
    csms.register_cancel_reservation_handler(runtime.actor())
        .await;
    csms.register_send_local_list_handler(runtime.actor()).await;
    csms.register_get_local_list_version_handler(runtime.actor())
        .await;
    csms.register_get_variables_handler(runtime.actor()).await;
    csms.register_set_variables_handler(runtime.actor()).await;
    csms.register_cost_updated_handler(runtime.actor()).await;

    Ok(runtime)
}

#[cfg(test)]
mod tests {
    use super::setup;
    use crate::executor::TokioExecutor;
    use crate::hardware::{
        execute_hardware_command, ChargePoint, Connector, Evse, HardwareCommandReceiver,
        HardwareEventSender,
    };
    use crate::provisioning::test_support::FixedBootNotifier;
    use crate::provisioning::BootNotificationOutcome;
    use crate::provisioning::TokioBackoff;
    use crate::state::{
        ChargePointEvent, ConnectorEvent, ConnectorState, EvseEvent, RegistrationStatus,
    };
    use alloc::boxed::Box;
    use alloc::sync::Arc;
    use core::convert::Infallible;
    use core::sync::atomic::{AtomicBool, Ordering};

    fn accepted_boot_notifier() -> FixedBootNotifier {
        FixedBootNotifier(BootNotificationOutcome {
            status: RegistrationStatus::Accepted,
            interval_secs: 60,
        })
    }

    struct TestChargePoint {
        evses: [TestEvse; 1],
    }

    struct TestEvse {
        connectors: [TestConnector; 1],
    }

    struct TestConnector {
        locked: Arc<AtomicBool>,
        lock_succeeds: bool,
    }

    #[derive(Debug)]
    struct TestConnectorError;

    impl core::fmt::Display for TestConnectorError {
        fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            formatter.write_str("test connector operation failed")
        }
    }

    impl core::error::Error for TestConnectorError {}

    #[async_trait::async_trait]
    impl ChargePoint<TestEvse, TestConnector> for TestChargePoint {
        type StartError = Infallible;

        async fn vendor_name(&self) -> &str {
            "Test vendor"
        }

        async fn model_name(&self) -> &str {
            "Test model"
        }

        async fn evses(&self) -> &[TestEvse] {
            &self.evses
        }

        async fn start(
            &self,
            events: HardwareEventSender,
            mut commands: HardwareCommandReceiver,
        ) -> Result<(), Self::StartError> {
            events
                .send(ChargePointEvent::Evse {
                    evse_id: 0,
                    event: EvseEvent::Connector {
                        connector_id: 0,
                        event: ConnectorEvent::CableConnected,
                    },
                })
                .await
                .unwrap();
            execute_hardware_command(&self.evses, commands.recv().await.unwrap(), &events).await;
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl Evse<TestConnector> for TestEvse {
        async fn connectors(&self) -> &[TestConnector] {
            &self.connectors
        }
    }

    #[async_trait::async_trait]
    impl Connector for TestConnector {
        type Error = TestConnectorError;

        async fn lock(&self) -> Result<(), Self::Error> {
            if self.lock_succeeds {
                self.locked.store(true, Ordering::SeqCst);
                Ok(())
            } else {
                Err(TestConnectorError)
            }
        }

        async fn unlock(&self) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn close_contactor(&self) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn open_contactor(&self) -> Result<(), Self::Error> {
            Ok(())
        }
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
        )
        .await
        .unwrap();

        assert_eq!(
            runtime.state().evses[0].connectors[0],
            ConnectorState::Faulted
        );
        assert!(!locked.load(Ordering::SeqCst));
    }
}
