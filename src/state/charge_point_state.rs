use alloc::vec::Vec;

use crate::state::connector_state::ConnectorCommand;
use crate::state::{ChargePointEffect, ChargePointEvent, EvseEvent, EvseState, HardwareCommand};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChargePointState {
    pub lifecycle: LifecycleState,
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
