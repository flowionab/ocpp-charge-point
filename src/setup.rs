use crate::ChargePointRuntime;
use crate::hardware::ChargePoint;
use crate::hardware::Connector;
use crate::hardware::Evse;
use alloc::vec::Vec;

pub async fn setup<T: ChargePoint<E, C>, E: Evse<C>, C: Connector>(
    charge_point: T,
) -> Result<ChargePointRuntime<T>, T::StartError> {
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
    runtime
        .hardware()
        .start(runtime.hardware_events(), runtime.hardware_commands())
        .await?;

    Ok(runtime)
}

#[cfg(test)]
mod tests {
    use super::setup;
    use crate::hardware::{
        ChargePoint, Connector, Evse, HardwareCommandReceiver, HardwareEventSender,
        execute_hardware_command,
    };
    use crate::state::{ChargePointEvent, ConnectorEvent, ConnectorState, EvseEvent};
    use alloc::boxed::Box;
    use alloc::sync::Arc;
    use core::convert::Infallible;
    use core::sync::atomic::{AtomicBool, Ordering};

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
        let runtime = setup(TestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector {
                    locked: locked.clone(),
                    lock_succeeds: true,
                }],
            }],
        })
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
        let runtime = setup(TestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector {
                    locked: locked.clone(),
                    lock_succeeds: false,
                }],
            }],
        })
        .await
        .unwrap();

        assert_eq!(
            runtime.state().evses[0].connectors[0],
            ConnectorState::Faulted
        );
        assert!(!locked.load(Ordering::SeqCst));
    }
}
