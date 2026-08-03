use alloc::vec::Vec;

use crate::state::connector_state::ConnectorCommand;
use crate::state::{
    ChargePointEffect, ChargePointEvent, ConnectorStatusChanged, EvseEvent, EvseState,
    HardwareCommand, RegistrationStatus,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChargePointState {
    pub lifecycle: LifecycleState,
    /// The CSMS's most recent BootNotification decision. `None` until the first
    /// BootNotification response arrives.
    pub registration: Option<RegistrationStatus>,
    pub evses: Vec<EvseState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    Booting,
    Available,
    Unavailable,
    Faulted,
}

impl ChargePointState {
    pub fn new(connector_counts: impl IntoIterator<Item = usize>) -> Self {
        Self {
            lifecycle: LifecycleState::Booting,
            registration: None,
            evses: connector_counts.into_iter().map(EvseState::new).collect(),
        }
    }

    pub fn apply(&mut self, event: ChargePointEvent) -> Vec<ChargePointEffect> {
        let mut effects = Vec::new();
        let changed = match event {
            ChargePointEvent::BootCompleted | ChargePointEvent::SetAvailable => {
                set_if_changed(&mut self.lifecycle, LifecycleState::Available)
            }
            ChargePointEvent::SetUnavailable | ChargePointEvent::FaultCleared => {
                set_if_changed(&mut self.lifecycle, LifecycleState::Unavailable)
            }
            ChargePointEvent::HardwareFault => {
                set_if_changed(&mut self.lifecycle, LifecycleState::Faulted)
            }
            ChargePointEvent::RegistrationStatusReceived(status) => {
                let registration_changed = set_if_changed(&mut self.registration, Some(status));
                let lifecycle_changed = status == RegistrationStatus::Accepted
                    && set_if_changed(&mut self.lifecycle, LifecycleState::Available);
                registration_changed || lifecycle_changed
            }
            ChargePointEvent::Evse { evse_id, event } => match event {
                EvseEvent::Connector {
                    connector_id,
                    event,
                } => {
                    let Some(connector) = self
                        .evses
                        .get_mut(evse_id)
                        .and_then(|evse| evse.connectors.get_mut(connector_id))
                    else {
                        return effects;
                    };
                    let previous_status = connector.availability_status();
                    let transition = connector.apply(event);
                    if let Some(command) = transition.command {
                        effects.push(ChargePointEffect::HardwareCommand(match command {
                            ConnectorCommand::Lock => HardwareCommand::LockConnector {
                                evse_id,
                                connector_id,
                            },
                            ConnectorCommand::Unlock => HardwareCommand::UnlockConnector {
                                evse_id,
                                connector_id,
                            },
                            ConnectorCommand::CloseContactor => HardwareCommand::CloseContactor {
                                evse_id,
                                connector_id,
                            },
                            ConnectorCommand::OpenContactor => HardwareCommand::OpenContactor {
                                evse_id,
                                connector_id,
                            },
                        }));
                    }
                    let status = connector.availability_status();
                    if status != previous_status {
                        effects.push(ChargePointEffect::StatusNotification(
                            ConnectorStatusChanged {
                                evse_id,
                                connector_id,
                                status,
                            },
                        ));
                    }
                    transition.changed
                }
                _ => self
                    .evses
                    .get_mut(evse_id)
                    .is_some_and(|evse| evse.apply(event)),
            },
        };

        if changed {
            effects.insert(0, ChargePointEffect::StateChanged);
        }
        effects
    }
}

fn set_if_changed<T: PartialEq>(current: &mut T, next: T) -> bool {
    if *current == next {
        false
    } else {
        *current = next;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ChargePointEffect;

    #[test]
    fn accepted_registration_records_status_and_makes_the_charge_point_available() {
        let mut state = ChargePointState::new([1]);

        let effects = state.apply(ChargePointEvent::RegistrationStatusReceived(
            RegistrationStatus::Accepted,
        ));

        assert_eq!(state.registration, Some(RegistrationStatus::Accepted));
        assert_eq!(state.lifecycle, LifecycleState::Available);
        assert!(effects.contains(&ChargePointEffect::StateChanged));
    }

    #[test]
    fn pending_registration_records_status_without_becoming_available() {
        let mut state = ChargePointState::new([1]);

        let effects = state.apply(ChargePointEvent::RegistrationStatusReceived(
            RegistrationStatus::Pending,
        ));

        assert_eq!(state.registration, Some(RegistrationStatus::Pending));
        assert_eq!(state.lifecycle, LifecycleState::Booting);
        assert!(effects.contains(&ChargePointEffect::StateChanged));
    }

    #[test]
    fn rejected_registration_records_status_without_becoming_available() {
        let mut state = ChargePointState::new([1]);

        state.apply(ChargePointEvent::RegistrationStatusReceived(
            RegistrationStatus::Rejected,
        ));

        assert_eq!(state.registration, Some(RegistrationStatus::Rejected));
        assert_eq!(state.lifecycle, LifecycleState::Booting);
    }

    #[test]
    fn repeating_the_same_registration_status_reports_no_change() {
        let mut state = ChargePointState::new([1]);
        state.apply(ChargePointEvent::RegistrationStatusReceived(
            RegistrationStatus::Pending,
        ));

        let effects = state.apply(ChargePointEvent::RegistrationStatusReceived(
            RegistrationStatus::Pending,
        ));

        assert!(effects.is_empty());
    }

    #[test]
    fn a_connector_status_change_is_reported_via_status_notification() {
        let mut state = ChargePointState::new([1]);

        let effects = state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::Connector {
                connector_id: 0,
                event: crate::state::ConnectorEvent::CableConnected,
            },
        });

        assert!(effects.contains(&ChargePointEffect::StatusNotification(
            ConnectorStatusChanged {
                evse_id: 0,
                connector_id: 0,
                status: crate::state::ConnectorStatus::Occupied,
            }
        )));
    }

    #[test]
    fn an_internal_transition_that_keeps_the_same_ocpp_status_reports_no_status_notification() {
        let mut state = ChargePointState::new([1]);
        state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::Connector {
                connector_id: 0,
                event: crate::state::ConnectorEvent::CableConnected,
            },
        });

        let effects = state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::Connector {
                connector_id: 0,
                event: crate::state::ConnectorEvent::LockConfirmed,
            },
        });

        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, ChargePointEffect::StatusNotification(_)))
        );
    }
}
