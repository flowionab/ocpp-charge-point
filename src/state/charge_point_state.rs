use alloc::vec::Vec;

use crate::state::connector_state::ConnectorCommand;
use crate::state::{
    ChargePointEffect, ChargePointEvent, ConnectorEvent, ConnectorState, ConnectorStatusChanged,
    EvseEvent, EvseState, HardwareCommand, RegistrationStatus, StopReason, Transaction,
    TransactionChargingState, TransactionEventKind, TransactionEventOccurred, TransactionId,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChargePointState {
    pub lifecycle: LifecycleState,
    /// The CSMS's most recent BootNotification decision. `None` until the first
    /// BootNotification response arrives.
    pub registration: Option<RegistrationStatus>,
    pub evses: Vec<EvseState>,
    /// The id to assign to the next transaction that starts, incremented every time one does.
    pub next_transaction_id: u64,
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
            next_transaction_id: 0,
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
                    let Some(evse) = self.evses.get_mut(evse_id) else {
                        return effects;
                    };
                    let Some(connector) = evse.connectors.get_mut(connector_id) else {
                        return effects;
                    };
                    let previous_status = connector.availability_status();
                    let previous_state = *connector;
                    let transition = connector.apply(event);
                    let new_state = *connector;
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
                    let status = new_state.availability_status();
                    if status != previous_status {
                        effects.push(ChargePointEffect::StatusNotification(
                            ConnectorStatusChanged {
                                evse_id,
                                connector_id,
                                status,
                            },
                        ));
                    }
                    let stop_reason = match event {
                        ConnectorEvent::ChargingStopped(reason) => Some(reason),
                        _ => None,
                    };
                    if let Some(slot) = evse.transactions.get_mut(connector_id) {
                        if let Some((kind, transaction)) = advance_transaction(
                            slot,
                            &mut self.next_transaction_id,
                            previous_state,
                            new_state,
                            stop_reason,
                        ) {
                            effects.push(ChargePointEffect::TransactionEvent(
                                TransactionEventOccurred {
                                    evse_id,
                                    connector_id,
                                    kind,
                                    transaction,
                                },
                            ));
                        }
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

/// Advances a connector's transaction alongside its `previous_state` -> `new_state` transition,
/// returning the TransactionEvent to report, if any. `event_stop_reason` is the `StopReason`
/// carried by the triggering `ConnectorEvent::ChargingStopped`, if that's what caused this
/// transition.
fn advance_transaction(
    slot: &mut Option<Transaction>,
    next_transaction_id: &mut u64,
    previous_state: ConnectorState,
    new_state: ConnectorState,
    event_stop_reason: Option<StopReason>,
) -> Option<(TransactionEventKind, Transaction)> {
    match (previous_state, new_state) {
        (ConnectorState::Locked, ConnectorState::Starting) => {
            let id = TransactionId(*next_transaction_id);
            *next_transaction_id += 1;
            let transaction = Transaction {
                id,
                charging_state: TransactionChargingState::EvConnected,
                stop_reason: None,
                seq_no: 0,
            };
            *slot = Some(transaction);
            Some((TransactionEventKind::Started, transaction))
        }
        (ConnectorState::Starting, ConnectorState::Charging) => {
            let transaction = slot.as_mut()?;
            transaction.charging_state = TransactionChargingState::Charging;
            transaction.seq_no += 1;
            Some((TransactionEventKind::Updated, *transaction))
        }
        (ConnectorState::Charging, ConnectorState::Stopping) => {
            let transaction = slot.as_mut()?;
            transaction.stop_reason = event_stop_reason;
            None
        }
        (ConnectorState::Stopping, ConnectorState::Finishing) => {
            let mut transaction = slot.take()?;
            transaction.charging_state = TransactionChargingState::EvConnected;
            transaction.seq_no += 1;
            Some((TransactionEventKind::Ended, transaction))
        }
        (_, ConnectorState::Faulted) => {
            let mut transaction = slot.take()?;
            transaction.stop_reason = Some(StopReason::EmergencyStop);
            transaction.seq_no += 1;
            Some((TransactionEventKind::Ended, transaction))
        }
        _ => None,
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

    fn apply_connector_event(
        state: &mut ChargePointState,
        event: ConnectorEvent,
    ) -> Vec<ChargePointEffect> {
        state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::Connector {
                connector_id: 0,
                event,
            },
        })
    }

    /// Drives connector 0 from `Available` up to (and including) `ChargingAuthorized`, i.e.
    /// just before the transaction starts.
    fn plug_in_and_authorize(state: &mut ChargePointState) {
        apply_connector_event(state, ConnectorEvent::CableConnected);
        apply_connector_event(state, ConnectorEvent::LockConfirmed);
    }

    #[test]
    fn authorizing_a_locked_connector_starts_a_transaction() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);

        let effects = apply_connector_event(&mut state, ConnectorEvent::ChargingAuthorized);

        let expected_transaction = Transaction {
            id: TransactionId(0),
            charging_state: TransactionChargingState::EvConnected,
            stop_reason: None,
            seq_no: 0,
        };
        assert_eq!(state.evses[0].transactions[0], Some(expected_transaction));
        assert!(effects.contains(&ChargePointEffect::TransactionEvent(
            TransactionEventOccurred {
                evse_id: 0,
                connector_id: 0,
                kind: TransactionEventKind::Started,
                transaction: expected_transaction,
            }
        )));
    }

    #[test]
    fn the_contactor_closing_updates_the_transaction_to_charging() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);
        apply_connector_event(&mut state, ConnectorEvent::ChargingAuthorized);

        let effects = apply_connector_event(&mut state, ConnectorEvent::ContactorClosed);

        let expected_transaction = Transaction {
            id: TransactionId(0),
            charging_state: TransactionChargingState::Charging,
            stop_reason: None,
            seq_no: 1,
        };
        assert_eq!(state.evses[0].transactions[0], Some(expected_transaction));
        assert!(effects.contains(&ChargePointEffect::TransactionEvent(
            TransactionEventOccurred {
                evse_id: 0,
                connector_id: 0,
                kind: TransactionEventKind::Updated,
                transaction: expected_transaction,
            }
        )));
    }

    #[test]
    fn stopping_charging_ends_the_transaction_once_the_contactor_confirms_open() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);
        apply_connector_event(&mut state, ConnectorEvent::ChargingAuthorized);
        apply_connector_event(&mut state, ConnectorEvent::ContactorClosed);

        let stop_effects = apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingStopped(StopReason::Local),
        );
        assert!(
            !stop_effects
                .iter()
                .any(|effect| matches!(effect, ChargePointEffect::TransactionEvent(_))),
            "no TransactionEvent until the contactor actually confirms it opened"
        );
        assert_eq!(state.evses[0].connectors[0], ConnectorState::Stopping);

        let effects = apply_connector_event(&mut state, ConnectorEvent::ContactorOpened);

        let expected_transaction = Transaction {
            id: TransactionId(0),
            charging_state: TransactionChargingState::EvConnected,
            stop_reason: Some(StopReason::Local),
            seq_no: 2,
        };
        assert_eq!(state.evses[0].connectors[0], ConnectorState::Finishing);
        assert_eq!(state.evses[0].transactions[0], None);
        assert!(effects.contains(&ChargePointEffect::TransactionEvent(
            TransactionEventOccurred {
                evse_id: 0,
                connector_id: 0,
                kind: TransactionEventKind::Ended,
                transaction: expected_transaction,
            }
        )));
    }

    #[test]
    fn a_hardware_fault_during_charging_immediately_ends_the_transaction() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);
        apply_connector_event(&mut state, ConnectorEvent::ChargingAuthorized);
        apply_connector_event(&mut state, ConnectorEvent::ContactorClosed);

        let effects = apply_connector_event(&mut state, ConnectorEvent::FaultDetected);

        let expected_transaction = Transaction {
            id: TransactionId(0),
            charging_state: TransactionChargingState::Charging,
            stop_reason: Some(StopReason::EmergencyStop),
            seq_no: 2,
        };
        assert_eq!(state.evses[0].transactions[0], None);
        assert!(effects.contains(&ChargePointEffect::TransactionEvent(
            TransactionEventOccurred {
                evse_id: 0,
                connector_id: 0,
                kind: TransactionEventKind::Ended,
                transaction: expected_transaction,
            }
        )));
    }

    #[test]
    fn a_fault_with_no_active_transaction_reports_no_transaction_event() {
        let mut state = ChargePointState::new([1]);

        let effects = apply_connector_event(&mut state, ConnectorEvent::FaultDetected);

        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, ChargePointEffect::TransactionEvent(_)))
        );
    }

    #[test]
    fn transaction_ids_increment_across_separate_sessions() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);
        apply_connector_event(&mut state, ConnectorEvent::ChargingAuthorized);
        apply_connector_event(&mut state, ConnectorEvent::ContactorClosed);
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingStopped(StopReason::Local),
        );
        apply_connector_event(&mut state, ConnectorEvent::ContactorOpened);
        apply_connector_event(&mut state, ConnectorEvent::UnlockConfirmed);
        apply_connector_event(&mut state, ConnectorEvent::CableDisconnected);

        plug_in_and_authorize(&mut state);
        apply_connector_event(&mut state, ConnectorEvent::ChargingAuthorized);

        assert_eq!(
            state.evses[0].transactions[0].map(|transaction| transaction.id),
            Some(TransactionId(1))
        );
    }
}
