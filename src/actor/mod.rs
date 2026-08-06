mod charge_point_actor;

pub use self::charge_point_actor::{ActorError, BootReasonRecorder, ChargePointActor};

#[cfg(test)]
mod tests {
    use super::ChargePointActor;
    use crate::executor::TokioExecutor;
    use crate::state::{
        AuthorizationRequested, ChargePointEffect, ChargePointEvent, ChargePointState,
        ConnectorEvent, ConnectorState, ConnectorStatus, ConnectorStatusChanged, EvseEvent,
        HardwareCommand, IdToken, IdTokenKind, LifecycleState, Transaction,
        TransactionChargingState, TransactionEventKind, TransactionEventOccurred, TransactionId,
    };
    use alloc::vec;

    #[test]
    fn connector_hardware_events_update_only_the_target_connector() {
        let mut state = ChargePointState::new([2]);

        let effects = state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::Connector {
                connector_id: 1,
                event: ConnectorEvent::CableConnected,
            },
        });

        assert_eq!(
            effects,
            vec![
                ChargePointEffect::StateChanged,
                ChargePointEffect::HardwareCommand(HardwareCommand::LockConnector {
                    evse_id: 0,
                    connector_id: 1,
                }),
                ChargePointEffect::StatusNotification(ConnectorStatusChanged {
                    evse_id: 0,
                    connector_id: 1,
                    status: ConnectorStatus::Occupied,
                    connector_state: ConnectorState::Connected,
                }),
            ]
        );
        assert_eq!(state.evses[0].connectors[0], ConnectorState::Available);
        assert_eq!(state.evses[0].connectors[1], ConnectorState::Connected);
    }

    #[test]
    fn invalid_hardware_addresses_do_not_change_state() {
        let mut state = ChargePointState::new([1]);

        let effects = state.apply(ChargePointEvent::Evse {
            evse_id: 3,
            event: EvseEvent::FaultDetected,
        });

        assert!(effects.is_empty());
        assert_eq!(state.lifecycle, LifecycleState::Booting);
    }

    #[test]
    fn connector_must_be_locked_before_charging_can_start() {
        let mut state = ChargePointState::new([1]);

        let effects = state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::Connector {
                connector_id: 0,
                event: ConnectorEvent::CableConnected,
            },
        });

        assert_eq!(
            effects,
            vec![
                ChargePointEffect::StateChanged,
                ChargePointEffect::HardwareCommand(HardwareCommand::LockConnector {
                    evse_id: 0,
                    connector_id: 0,
                }),
                ChargePointEffect::StatusNotification(ConnectorStatusChanged {
                    evse_id: 0,
                    connector_id: 0,
                    status: ConnectorStatus::Occupied,
                    connector_state: ConnectorState::Connected,
                }),
            ]
        );

        state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::Connector {
                connector_id: 0,
                event: ConnectorEvent::LockConfirmed,
            },
        });

        let id_token = IdToken {
            value: "04A224B2".into(),
            kind: IdTokenKind::ISO14443,
        };
        let presentation_effects = state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::Connector {
                connector_id: 0,
                event: ConnectorEvent::IdTokenPresented(id_token.clone()),
            },
        });
        assert_eq!(state.evses[0].connectors[0], ConnectorState::Authorizing);
        assert_eq!(
            presentation_effects,
            vec![
                ChargePointEffect::StateChanged,
                ChargePointEffect::StatusNotification(ConnectorStatusChanged {
                    evse_id: 0,
                    connector_id: 0,
                    status: ConnectorStatus::Occupied,
                    connector_state: ConnectorState::Authorizing,
                }),
                ChargePointEffect::AuthorizationRequested(AuthorizationRequested {
                    evse_id: 0,
                    connector_id: 0,
                    id_token: id_token.clone(),
                }),
            ]
        );

        let effects = state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::Connector {
                connector_id: 0,
                event: ConnectorEvent::ChargingAuthorized(id_token.clone()),
            },
        });

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Starting);
        assert_eq!(
            effects,
            vec![
                ChargePointEffect::StateChanged,
                ChargePointEffect::HardwareCommand(HardwareCommand::CloseContactor {
                    evse_id: 0,
                    connector_id: 0,
                }),
                ChargePointEffect::StatusNotification(ConnectorStatusChanged {
                    evse_id: 0,
                    connector_id: 0,
                    status: ConnectorStatus::Occupied,
                    connector_state: ConnectorState::Starting,
                }),
                ChargePointEffect::TransactionEvent(TransactionEventOccurred {
                    evse_id: 0,
                    connector_id: 0,
                    kind: TransactionEventKind::Started,
                    transaction: Transaction {
                        id: TransactionId(0),
                        id_token: Some(id_token),
                        charging_state: TransactionChargingState::EvConnected,
                        stop_reason: None,
                        seq_no: 0,
                        last_meter_sample: None,
                    },
                }),
            ]
        );
    }

    #[test]
    fn a_fault_unlocks_only_after_the_contactor_is_open_and_fault_is_cleared() {
        let mut state = ChargePointState::new([1]);

        let effects = state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::Connector {
                connector_id: 0,
                event: ConnectorEvent::FaultDetected,
            },
        });
        assert_eq!(
            effects,
            vec![
                ChargePointEffect::StateChanged,
                ChargePointEffect::HardwareCommand(HardwareCommand::OpenContactor {
                    evse_id: 0,
                    connector_id: 0,
                }),
                ChargePointEffect::StatusNotification(ConnectorStatusChanged {
                    evse_id: 0,
                    connector_id: 0,
                    status: ConnectorStatus::Faulted,
                    connector_state: ConnectorState::Faulted,
                }),
            ]
        );

        state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::Connector {
                connector_id: 0,
                event: ConnectorEvent::ContactorOpened,
            },
        });
        let effects = state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::Connector {
                connector_id: 0,
                event: ConnectorEvent::FaultCleared,
            },
        });

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Unlocking);
        assert_eq!(
            effects,
            vec![
                ChargePointEffect::StateChanged,
                ChargePointEffect::HardwareCommand(HardwareCommand::UnlockConnector {
                    evse_id: 0,
                    connector_id: 0,
                }),
                ChargePointEffect::StatusNotification(ConnectorStatusChanged {
                    evse_id: 0,
                    connector_id: 0,
                    status: ConnectorStatus::Occupied,
                    connector_state: ConnectorState::Unlocking,
                }),
            ]
        );
    }

    #[tokio::test]
    async fn charge_point_actor_serializes_events_and_publishes_latest_state() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let mut states = actor.subscribe();

        actor.send(ChargePointEvent::BootCompleted).await.unwrap();
        states.changed().await;

        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::CableConnected,
                },
            })
            .await
            .unwrap();
        states.changed().await;

        assert_eq!(actor.state().lifecycle, LifecycleState::Available);
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Connected
        );
    }
}
