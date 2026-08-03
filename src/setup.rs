use crate::ChargePointRuntime;
use crate::authorization::{Authorizer, run_authorization_requests};
use crate::availability::{StatusNotifier, run_status_notifications};
use crate::hardware::ChargePoint;
use crate::hardware::Connector;
use crate::hardware::Evse;
use crate::provisioning::{BootNotifier, HeartbeatSender, TokioBackoff, run_heartbeat};
use crate::transactions::{TransactionNotifier, run_transaction_events};
use alloc::vec::Vec;

/// Starts the hardware, then runs the Provisioning functional block's BootNotification
/// exchange (retrying with backoff on `Pending`/`Rejected` or a transport failure - see
/// [`ChargePointRuntime::register_until_accepted`]). Once accepted, spawns background tasks
/// that send a Heartbeat at the interval the CSMS returned, forward every connector status
/// change to the CSMS via StatusNotification, forward every transaction lifecycle event via
/// TransactionEvent, and answer every presented-id-token authorization request via Authorize,
/// for as long as the process runs.
pub async fn setup<T, E, C, N>(
    charge_point: T,
    csms: N,
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
        + Clone
        + Send
        + Sync
        + 'static,
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

    let runtime = ChargePointRuntime::new(charge_point, connector_counts);
    // Subscribe before starting the hardware so status/transaction/authorization events fired
    // during `start()` (e.g. a connector that's already occupied at boot) are buffered rather
    // than lost.
    let status_changes = runtime.subscribe_status_notifications();
    let transaction_events = runtime.subscribe_transaction_events();
    let authorization_requests = runtime.subscribe_authorization_requests();
    runtime
        .hardware()
        .start(runtime.hardware_events(), runtime.hardware_commands())
        .await?;

    let hardware = runtime.hardware();
    let vendor_name = hardware.vendor_name().await;
    let model_name = hardware.model_name().await;
    let outcome = runtime
        .register_until_accepted(&csms, &TokioBackoff, vendor_name, model_name)
        .await;

    let heartbeat_sender = csms.clone();
    tokio::spawn(async move {
        run_heartbeat(&heartbeat_sender, &TokioBackoff, outcome.interval_secs).await;
    });

    let status_notifier = csms.clone();
    tokio::spawn(async move {
        run_status_notifications(status_changes, &status_notifier).await;
    });

    let transaction_notifier = csms.clone();
    tokio::spawn(async move {
        run_transaction_events(transaction_events, &transaction_notifier).await;
    });

    let authorizer = csms.clone();
    let actor = runtime.actor();
    tokio::spawn(async move {
        run_authorization_requests(authorization_requests, &authorizer, actor).await;
    });

    Ok(runtime)
}

#[cfg(test)]
mod tests {
    use super::setup;
    use crate::hardware::{
        ChargePoint, Connector, Evse, HardwareCommandReceiver, HardwareEventSender,
        execute_hardware_command,
    };
    use crate::provisioning::BootNotificationOutcome;
    use crate::provisioning::test_support::FixedBootNotifier;
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
