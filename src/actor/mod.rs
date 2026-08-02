mod charge_point_actor;

pub use self::charge_point_actor::{ActorError, ChargePointActor};

#[cfg(test)]
mod tests {
    use super::ChargePointActor;
    use crate::state::{
        ChargePointEffect, ChargePointEvent, ChargePointState, ConnectorEvent, ConnectorState,
        EvseEvent, HardwareCommand, LifecycleState,
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
            ]
        );

        state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::Connector {
                connector_id: 0,
                event: ConnectorEvent::LockConfirmed,
            },
        });
        let effects = state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::Connector {
                connector_id: 0,
                event: ConnectorEvent::ChargingAuthorized,
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
            ]
        );
    }

    #[tokio::test]
    async fn charge_point_actor_serializes_events_and_publishes_latest_state() {
        let actor = ChargePointActor::spawn([1]);
        let mut states = actor.subscribe();

        actor.send(ChargePointEvent::BootCompleted).await.unwrap();
        states.changed().await.unwrap();

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
        states.changed().await.unwrap();

        assert_eq!(actor.state().lifecycle, LifecycleState::Available);
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Connected
        );
    }
}
