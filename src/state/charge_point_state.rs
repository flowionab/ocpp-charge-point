use alloc::vec::Vec;

use crate::state::connector_state::ConnectorCommand;
use crate::state::{
    AuthorizationRequested, ChargePointEffect, ChargePointEvent, ConnectorEvent, ConnectorState,
    ConnectorStatusChanged, DeviceModel, DeviceModelEvent, EvseEvent, EvseState, HardwareCommand,
    IdToken, LocalAuthorizationList, MeterSample, RegistrationStatus, StopReason, Transaction,
    TransactionChargingState, TransactionEventKind, TransactionEventOccurred, TransactionId,
    TransactionUpdateReason,
};

/// The protocol-version-independent internal state of the whole charge point: its lifecycle,
/// registration with the CSMS, and every EVSE it owns. The single source of truth
/// [`crate::actor::ChargePointActor`] owns and mutates via [`ChargePointState::apply`].
#[derive(Debug, Clone, PartialEq)]
pub struct ChargePointState {
    /// The charge point's own lifecycle state, independent of any individual EVSE/connector.
    pub lifecycle: LifecycleState,
    /// The CSMS's most recent BootNotification decision. `None` until the first
    /// BootNotification response arrives.
    pub registration: Option<RegistrationStatus>,
    /// This charge point's EVSEs, indexed by `evse_id` as used throughout this crate and OCPP.
    pub evses: Vec<EvseState>,
    /// The id to assign to the next transaction that starts, incremented every time one does.
    pub next_transaction_id: u64,
    /// The offline authorization cache maintained via `SendLocalList`/`GetLocalListVersion`. See
    /// `docs/ROADMAP.md` §4.
    pub local_authorization_list: LocalAuthorizationList,
    /// The Component/Variable device model (OCPP `GetVariables`/`SetVariables`). See
    /// `docs/ROADMAP.md` §2 and `crate::device_model`.
    pub device_model: DeviceModel,
}

/// The charge point's own lifecycle state, independent of any individual EVSE/connector's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    /// The charge point is starting up; no BootNotification response has been accepted yet.
    Booting,
    /// The charge point is available for use.
    Available,
    /// The charge point has been made unavailable (OCPP `ChangeAvailability`), or a
    /// charge-point-wide hardware fault has cleared and is awaiting an explicit
    /// `SetAvailable`/registration to resume.
    Unavailable,
    /// A charge-point-wide hardware fault is active.
    Faulted,
}

impl ChargePointState {
    /// Creates a fresh charge point with one EVSE per entry in `connector_counts` (each value is
    /// that EVSE's connector count), starting in [`LifecycleState::Booting`] with no
    /// registration, no transactions, and an empty local authorization list.
    pub fn new(connector_counts: impl IntoIterator<Item = usize>) -> Self {
        Self {
            lifecycle: LifecycleState::Booting,
            registration: None,
            evses: connector_counts.into_iter().map(EvseState::new).collect(),
            next_transaction_id: 0,
            local_authorization_list: LocalAuthorizationList::new(),
            device_model: DeviceModel::new(),
        }
    }

    /// Applies `event` to this charge point's state machine, mutating it in place and returning
    /// the [`ChargePointEffect`]s that resulted (in order; a leading `StateChanged` first if
    /// anything actually changed). Unrecognized event/state combinations (e.g. an event
    /// addressing an EVSE/connector index that doesn't exist, or one that doesn't apply to the
    /// current state) are no-ops that return no effects, rather than an error - this crate's
    /// state machines are designed to tolerate being handed events that don't currently apply.
    pub fn apply(&mut self, event: ChargePointEvent) -> Vec<ChargePointEffect> {
        let mut effects = Vec::new();
        let changed = match event {
            ChargePointEvent::BootCompleted | ChargePointEvent::SetAvailable => {
                set_if_changed(&mut self.lifecycle, LifecycleState::Available)
            }
            ChargePointEvent::SetUnavailable => {
                set_if_changed(&mut self.lifecycle, LifecycleState::Unavailable)
            }
            ChargePointEvent::FaultCleared => {
                let lifecycle_changed =
                    set_if_changed(&mut self.lifecycle, LifecycleState::Unavailable);
                let cascade_changed = self.cascade_charge_point_fault(false, &mut effects);
                lifecycle_changed || cascade_changed
            }
            ChargePointEvent::HardwareFault => {
                let lifecycle_changed =
                    set_if_changed(&mut self.lifecycle, LifecycleState::Faulted);
                let cascade_changed = self.cascade_charge_point_fault(true, &mut effects);
                lifecycle_changed || cascade_changed
            }
            ChargePointEvent::RegistrationStatusReceived(status) => {
                let registration_changed = set_if_changed(&mut self.registration, Some(status));
                let lifecycle_changed = status == RegistrationStatus::Accepted
                    && set_if_changed(&mut self.lifecycle, LifecycleState::Available);
                registration_changed || lifecycle_changed
            }
            ChargePointEvent::LocalListUpdated { version, entries } => {
                self.local_authorization_list = LocalAuthorizationList { version, entries };
                true
            }
            ChargePointEvent::SecurityEventOccurred(event) => {
                effects.push(ChargePointEffect::SecurityEventOccurred(event));
                false
            }
            ChargePointEvent::DeviceModel(event) => match event {
                DeviceModelEvent::VariableRegistered {
                    component,
                    variable,
                    characteristics,
                    attributes,
                } => {
                    self.device_model
                        .register(component, variable, characteristics, attributes);
                    true
                }
                DeviceModelEvent::AttributeValueSet {
                    component,
                    variable,
                    attribute_type,
                    value,
                } => {
                    self.device_model
                        .set_attribute_value(&component, &variable, attribute_type, value)
                }
            },
            ChargePointEvent::Evse { evse_id, event } => match event {
                EvseEvent::Connector {
                    connector_id,
                    event,
                } => self.apply_connector_event(evse_id, connector_id, event, &mut effects),
                EvseEvent::FaultDetected => self.cascade_evse_fault(evse_id, true, &mut effects),
                EvseEvent::FaultCleared => self.cascade_evse_fault(evse_id, false, &mut effects),
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

    /// Applies a single `ConnectorEvent` to one connector, pushing whatever
    /// `ChargePointEffect`s result (hardware commands, status notifications, authorization
    /// requests, transaction events, cost updates). Returns whether anything actually changed.
    /// Shared by direct connector events and by fault cascading (below), so a cascaded
    /// `FaultDetected`/`FaultCleared` produces exactly the same effects a single-connector fault
    /// would.
    fn apply_connector_event(
        &mut self,
        evse_id: usize,
        connector_id: usize,
        event: ConnectorEvent,
        effects: &mut Vec<ChargePointEffect>,
    ) -> bool {
        let Some(evse) = self.evses.get_mut(evse_id) else {
            return false;
        };
        let Some(connector) = evse.connectors.get_mut(connector_id) else {
            return false;
        };
        let previous_state = *connector;
        let stop_reason = match &event {
            ConnectorEvent::ChargingStopped(reason) => Some(*reason),
            _ => None,
        };
        let presented_id_token = match &event {
            ConnectorEvent::IdTokenPresented(id_token) => Some(id_token.clone()),
            _ => None,
        };
        let authorized_id_token = match &event {
            ConnectorEvent::ChargingAuthorized(id_token)
            | ConnectorEvent::RemoteStartRequested(id_token) => Some(id_token.clone()),
            _ => None,
        };
        let meter_sample = match &event {
            ConnectorEvent::MeterValueSampled(sample) => Some(*sample),
            _ => None,
        };
        let reservation_made = match &event {
            ConnectorEvent::Reserved(reservation) => Some(reservation.clone()),
            _ => None,
        };
        let cost_update = match &event {
            ConnectorEvent::CostUpdated(total_cost) => Some(*total_cost),
            _ => None,
        };
        let transition = connector.apply(event);
        let new_state = *connector;
        if let Some(slot) = evse.reservations.get_mut(connector_id) {
            if new_state == ConnectorState::Reserved {
                *slot = reservation_made;
            } else if previous_state == ConnectorState::Reserved {
                *slot = None;
            }
        }
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
        // Fires on every actual `ConnectorState` transition, not just ones that cross a coarse
        // `ConnectorStatus` boundary - so a version adapter with a richer wire status than
        // `ConnectorStatus` (see `ConnectorStatusChanged::connector_state`'s docs) sees every
        // transition it might need to report, not only the ones 2.x's coarser status cares
        // about. Versions whose own status is no richer than `ConnectorStatus` (2.1, 2.0.1) now
        // receive more calls than before for transitions that don't change their own wire status
        // (e.g. `Locked` -> `Authorizing`, both `Occupied`) - those adapters are expected to
        // dedup on `status` themselves if that redundancy matters to them; nothing in this crate
        // currently needs them to.
        if transition.changed {
            effects.push(ChargePointEffect::StatusNotification(
                ConnectorStatusChanged {
                    evse_id,
                    connector_id,
                    status: new_state.availability_status(),
                    connector_state: new_state,
                },
            ));
        }
        if new_state == ConnectorState::Authorizing {
            if let Some(id_token) = presented_id_token {
                effects.push(ChargePointEffect::AuthorizationRequested(
                    AuthorizationRequested {
                        evse_id,
                        connector_id,
                        id_token,
                    },
                ));
            }
        }
        if let Some(slot) = evse.transactions.get_mut(connector_id) {
            if let Some((kind, transaction)) = advance_transaction(
                slot,
                &mut self.next_transaction_id,
                previous_state,
                new_state,
                stop_reason,
                authorized_id_token,
            ) {
                // A new transaction must not inherit a previous one's running cost, and an ended
                // transaction's cost is no longer meaningful.
                if matches!(
                    kind,
                    TransactionEventKind::Started | TransactionEventKind::Ended
                ) {
                    if let Some(cost_slot) = evse.running_costs.get_mut(connector_id) {
                        *cost_slot = None;
                    }
                }
                effects.push(ChargePointEffect::TransactionEvent(
                    TransactionEventOccurred {
                        evse_id,
                        connector_id,
                        kind,
                        transaction,
                    },
                ));
            }
            if let Some(sample) = meter_sample {
                if let Some((kind, transaction)) = apply_meter_sample(slot, sample) {
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
        }
        // Only recorded while a transaction is actually active on this connector - there's
        // nothing meaningful to attach a cost to otherwise. A recorded cost doesn't change
        // `ConnectorState` itself, so `transition.changed` alone wouldn't notice it - without
        // folding it into `changed` here, the actor's watch channel would never publish it (see
        // `ChargePointEffect::StateChanged`).
        let cost_recorded = cost_update.is_some_and(|total_cost| {
            if evse
                .transactions
                .get(connector_id)
                .is_some_and(Option::is_some)
            {
                if let Some(cost_slot) = evse.running_costs.get_mut(connector_id) {
                    *cost_slot = Some(total_cost);
                    return true;
                }
            }
            false
        });
        transition.changed || cost_recorded
    }

    /// Cascades a hardware fault (`detected = true`) or its clearing (`detected = false`) from
    /// one EVSE down to every connector it owns, via the same `apply_connector_event` path a
    /// direct connector-level fault takes - so e.g. a shared-power-source failure forces every
    /// connector on that EVSE into `Faulted` and opens its contactor (fail-safe, per
    /// `CLAUDE.md`), and clearing it only recovers connectors whose contactor has actually
    /// confirmed open (`FaultedSafe`); others stay `Faulted` since `ConnectorState::apply`
    /// no-ops a `FaultCleared` it isn't ready for.
    fn cascade_evse_fault(
        &mut self,
        evse_id: usize,
        detected: bool,
        effects: &mut Vec<ChargePointEffect>,
    ) -> bool {
        let evse_event = if detected {
            EvseEvent::FaultDetected
        } else {
            EvseEvent::FaultCleared
        };
        let status_changed = self
            .evses
            .get_mut(evse_id)
            .is_some_and(|evse| evse.apply(evse_event));
        let connector_count = self
            .evses
            .get(evse_id)
            .map_or(0, |evse| evse.connectors.len());
        let mut connectors_changed = false;
        for connector_id in 0..connector_count {
            let connector_event = if detected {
                ConnectorEvent::FaultDetected
            } else {
                ConnectorEvent::FaultCleared
            };
            connectors_changed |=
                self.apply_connector_event(evse_id, connector_id, connector_event, effects);
        }
        status_changed || connectors_changed
    }

    /// Cascades a charge-point-wide hardware fault (or its clearing) to every EVSE - and, via
    /// `cascade_evse_fault`, every connector on each of them. See `docs/ROADMAP.md` §0's
    /// "erratic-hardware fault containment" item: a top-level fault must drive the whole charge
    /// point fail-safe, not just flip `LifecycleState`.
    fn cascade_charge_point_fault(
        &mut self,
        detected: bool,
        effects: &mut Vec<ChargePointEffect>,
    ) -> bool {
        let mut changed = false;
        for evse_id in 0..self.evses.len() {
            changed |= self.cascade_evse_fault(evse_id, detected, effects);
        }
        changed
    }
}

/// Advances a connector's transaction alongside its `previous_state` -> `new_state` transition,
/// returning the TransactionEvent to report, if any. `event_stop_reason` is the `StopReason`
/// carried by the triggering `ConnectorEvent::ChargingStopped`, if that's what caused this
/// transition. `event_id_token` is the identifier carried by a triggering `ChargingAuthorized`/
/// `RemoteStartRequested`, if that's what caused this transition - recorded on the new
/// `Transaction`.
fn advance_transaction(
    slot: &mut Option<Transaction>,
    next_transaction_id: &mut u64,
    previous_state: ConnectorState,
    new_state: ConnectorState,
    event_stop_reason: Option<StopReason>,
    event_id_token: Option<IdToken>,
) -> Option<(TransactionEventKind, Transaction)> {
    match (previous_state, new_state) {
        // Reached from `Authorizing` (a physically presented id token was authorized) or
        // directly from `Locked` (a CSMS-initiated `RequestStartTransaction` - see
        // `docs/ROADMAP.md` §6) - either way, entering `Starting` from elsewhere always begins a
        // new transaction. Excludes `Starting` -> `Starting` (e.g. a meter sample applied while
        // still `Starting`, which doesn't change connector state) - that must stay a no-op.
        (ConnectorState::Authorizing | ConnectorState::Locked, ConnectorState::Starting) => {
            let id = TransactionId(*next_transaction_id);
            *next_transaction_id += 1;
            let transaction = Transaction {
                id,
                id_token: event_id_token,
                charging_state: TransactionChargingState::EvConnected,
                stop_reason: None,
                seq_no: 0,
                last_meter_sample: None,
            };
            *slot = Some(transaction.clone());
            Some((TransactionEventKind::Started, transaction))
        }
        (ConnectorState::Starting, ConnectorState::Charging) => {
            let transaction = slot.as_mut()?;
            transaction.charging_state = TransactionChargingState::Charging;
            transaction.seq_no += 1;
            Some((
                TransactionEventKind::Updated(TransactionUpdateReason::ChargingStateChanged),
                transaction.clone(),
            ))
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

/// Records a meter reading against the connector's active transaction, if it's currently
/// `Charging` - meter values are only meaningful (and only reported) while energy is actually
/// flowing.
fn apply_meter_sample(
    slot: &mut Option<Transaction>,
    sample: MeterSample,
) -> Option<(TransactionEventKind, Transaction)> {
    let transaction = slot.as_mut()?;
    if transaction.charging_state != TransactionChargingState::Charging {
        return None;
    }
    transaction.last_meter_sample = Some(sample);
    transaction.seq_no += 1;
    Some((
        TransactionEventKind::Updated(TransactionUpdateReason::MeterValuePeriodic),
        transaction.clone(),
    ))
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
    use crate::state::{
        ChargePointEffect, EvseStatus, IdToken, IdTokenKind, TransactionUpdateReason,
    };

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
                connector_state: ConnectorState::Connected,
            }
        )));
    }

    #[test]
    fn an_internal_transition_that_keeps_the_same_ocpp_status_still_reports_the_richer_connector_state() {
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

        // `Connected` -> `Locked` doesn't cross a coarse `ConnectorStatus` boundary (both
        // `Occupied`), but it's still a real `ConnectorState` transition - a version adapter
        // with a richer wire status than `ConnectorStatus` (see `docs/ROADMAP.md` §0's
        // `Ocpp1_6StatusNotifier`) needs to see it even though `status` itself doesn't change.
        assert!(effects.contains(&ChargePointEffect::StatusNotification(
            ConnectorStatusChanged {
                evse_id: 0,
                connector_id: 0,
                status: crate::state::ConnectorStatus::Occupied,
                connector_state: ConnectorState::Locked,
            }
        )));
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

    fn test_id_token() -> IdToken {
        IdToken {
            value: "04A224B2".into(),
            kind: IdTokenKind::ISO14443,
        }
    }

    /// Drives connector 0 from `Available` to `Authorizing`, i.e. just before the CSMS's
    /// authorization decision (`ChargingAuthorized`/`AuthorizationDenied`) arrives.
    fn plug_in_and_authorize(state: &mut ChargePointState) {
        apply_connector_event(state, ConnectorEvent::CableConnected);
        apply_connector_event(state, ConnectorEvent::LockConfirmed);
        apply_connector_event(state, ConnectorEvent::IdTokenPresented(test_id_token()));
    }

    #[test]
    fn a_remote_unlock_request_while_locked_unlocks_the_connector() {
        let mut state = ChargePointState::new([1]);
        apply_connector_event(&mut state, ConnectorEvent::CableConnected);
        apply_connector_event(&mut state, ConnectorEvent::LockConfirmed);

        let effects = apply_connector_event(&mut state, ConnectorEvent::RemoteUnlockRequested);

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Unlocking);
        assert!(effects.contains(&ChargePointEffect::HardwareCommand(
            HardwareCommand::UnlockConnector {
                evse_id: 0,
                connector_id: 0,
            }
        )));

        apply_connector_event(&mut state, ConnectorEvent::UnlockConfirmed);
        assert_eq!(state.evses[0].connectors[0], ConnectorState::Available);
    }

    #[test]
    fn a_remote_start_request_while_locked_starts_a_transaction_without_authorizing() {
        let mut state = ChargePointState::new([1]);
        apply_connector_event(&mut state, ConnectorEvent::CableConnected);
        apply_connector_event(&mut state, ConnectorEvent::LockConfirmed);

        let effects = apply_connector_event(&mut state, ConnectorEvent::RemoteStartRequested(test_id_token()));

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Starting);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, ChargePointEffect::AuthorizationRequested(_)))
        );
        assert!(effects.contains(&ChargePointEffect::HardwareCommand(
            HardwareCommand::CloseContactor {
                evse_id: 0,
                connector_id: 0,
            }
        )));
        let expected_transaction = Transaction {
            id: TransactionId(0),
            id_token: Some(test_id_token()),
            charging_state: TransactionChargingState::EvConnected,
            stop_reason: None,
            seq_no: 0,
            last_meter_sample: None,
        };
        assert_eq!(state.evses[0].transactions[0], Some(expected_transaction.clone()));
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
    fn a_remote_start_request_is_ignored_outside_the_locked_state() {
        let mut state = ChargePointState::new([1]);

        let effects = apply_connector_event(&mut state, ConnectorEvent::RemoteStartRequested(test_id_token()));

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Available);
        assert!(effects.is_empty());
    }

    #[test]
    fn a_remote_unlock_request_is_ignored_outside_the_locked_state() {
        let mut state = ChargePointState::new([1]);

        let effects = apply_connector_event(&mut state, ConnectorEvent::RemoteUnlockRequested);

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Available);
        assert!(effects.is_empty());
    }

    #[test]
    fn presenting_an_id_token_while_locked_requests_authorization() {
        let mut state = ChargePointState::new([1]);
        apply_connector_event(&mut state, ConnectorEvent::CableConnected);
        apply_connector_event(&mut state, ConnectorEvent::LockConfirmed);

        let effects = apply_connector_event(
            &mut state,
            ConnectorEvent::IdTokenPresented(test_id_token()),
        );

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Authorizing);
        assert!(effects.contains(&ChargePointEffect::AuthorizationRequested(
            AuthorizationRequested {
                evse_id: 0,
                connector_id: 0,
                id_token: test_id_token(),
            }
        )));
        assert_eq!(state.evses[0].transactions[0], None);
    }

    #[test]
    fn an_id_token_presented_while_not_locked_is_ignored() {
        let mut state = ChargePointState::new([1]);

        let effects = apply_connector_event(
            &mut state,
            ConnectorEvent::IdTokenPresented(test_id_token()),
        );

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Available);
        assert!(effects.is_empty());
    }

    #[test]
    fn a_denied_authorization_returns_the_connector_to_locked_without_a_transaction() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);

        let effects = apply_connector_event(&mut state, ConnectorEvent::AuthorizationDenied);

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Locked);
        assert_eq!(state.evses[0].transactions[0], None);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, ChargePointEffect::TransactionEvent(_)))
        );
    }

    #[test]
    fn authorizing_a_locked_connector_starts_a_transaction() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);

        let effects = apply_connector_event(&mut state, ConnectorEvent::ChargingAuthorized(test_id_token()));

        let expected_transaction = Transaction {
            id: TransactionId(0),
            id_token: Some(test_id_token()),
            charging_state: TransactionChargingState::EvConnected,
            stop_reason: None,
            seq_no: 0,
            last_meter_sample: None,
        };
        assert_eq!(state.evses[0].transactions[0], Some(expected_transaction.clone()));
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
        apply_connector_event(&mut state, ConnectorEvent::ChargingAuthorized(test_id_token()));

        let effects = apply_connector_event(&mut state, ConnectorEvent::ContactorClosed);

        let expected_transaction = Transaction {
            id: TransactionId(0),
            id_token: Some(test_id_token()),
            charging_state: TransactionChargingState::Charging,
            stop_reason: None,
            seq_no: 1,
            last_meter_sample: None,
        };
        assert_eq!(state.evses[0].transactions[0], Some(expected_transaction.clone()));
        assert!(effects.contains(&ChargePointEffect::TransactionEvent(
            TransactionEventOccurred {
                evse_id: 0,
                connector_id: 0,
                kind: TransactionEventKind::Updated(TransactionUpdateReason::ChargingStateChanged),
                transaction: expected_transaction,
            }
        )));
    }

    #[test]
    fn a_meter_reading_while_charging_updates_the_transaction_and_is_reported() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);
        apply_connector_event(&mut state, ConnectorEvent::ChargingAuthorized(test_id_token()));
        apply_connector_event(&mut state, ConnectorEvent::ContactorClosed);

        let sample = MeterSample {
            energy_wh: 1_500,
            ..Default::default()
        };
        let effects = apply_connector_event(&mut state, ConnectorEvent::MeterValueSampled(sample));

        let expected_transaction = Transaction {
            id: TransactionId(0),
            id_token: Some(test_id_token()),
            charging_state: TransactionChargingState::Charging,
            stop_reason: None,
            seq_no: 2,
            last_meter_sample: Some(sample),
        };
        assert_eq!(state.evses[0].transactions[0], Some(expected_transaction.clone()));
        // A meter reading never changes the connector's physical state.
        assert_eq!(state.evses[0].connectors[0], ConnectorState::Charging);
        assert!(effects.contains(&ChargePointEffect::TransactionEvent(
            TransactionEventOccurred {
                evse_id: 0,
                connector_id: 0,
                kind: TransactionEventKind::Updated(TransactionUpdateReason::MeterValuePeriodic),
                transaction: expected_transaction,
            }
        )));
    }

    #[test]
    fn a_meter_reading_while_not_charging_is_ignored() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);
        apply_connector_event(&mut state, ConnectorEvent::ChargingAuthorized(test_id_token()));
        // Still `Starting` (EvConnected) here, not yet `Charging`.

        let sample = MeterSample {
            energy_wh: 1_500,
            ..Default::default()
        };
        let effects = apply_connector_event(&mut state, ConnectorEvent::MeterValueSampled(sample));

        assert_eq!(
            state.evses[0].transactions[0]
                .as_ref()
                .map(|transaction| transaction.last_meter_sample),
            Some(None)
        );
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, ChargePointEffect::TransactionEvent(_)))
        );
    }

    #[test]
    fn a_meter_reading_with_no_active_transaction_is_ignored() {
        let mut state = ChargePointState::new([1]);

        let effects = apply_connector_event(
            &mut state,
            ConnectorEvent::MeterValueSampled(MeterSample {
                energy_wh: 1_500,
                ..Default::default()
            }),
        );

        assert!(effects.is_empty());
    }

    #[test]
    fn stopping_charging_ends_the_transaction_once_the_contactor_confirms_open() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);
        apply_connector_event(&mut state, ConnectorEvent::ChargingAuthorized(test_id_token()));
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
            id_token: Some(test_id_token()),
            charging_state: TransactionChargingState::EvConnected,
            stop_reason: Some(StopReason::Local),
            seq_no: 2,
            last_meter_sample: None,
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
        apply_connector_event(&mut state, ConnectorEvent::ChargingAuthorized(test_id_token()));
        apply_connector_event(&mut state, ConnectorEvent::ContactorClosed);

        let effects = apply_connector_event(&mut state, ConnectorEvent::FaultDetected);

        let expected_transaction = Transaction {
            id: TransactionId(0),
            id_token: Some(test_id_token()),
            charging_state: TransactionChargingState::Charging,
            stop_reason: Some(StopReason::EmergencyStop),
            seq_no: 2,
            last_meter_sample: None,
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
    fn an_evse_fault_forces_every_connector_under_it_into_a_faulted_safe_state() {
        let mut state = ChargePointState::new([3]);
        apply_connector_event(&mut state, ConnectorEvent::CableConnected);

        let effects = state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::FaultDetected,
        });

        assert_eq!(state.evses[0].status, EvseStatus::Faulted);
        assert_eq!(state.evses[0].connectors[0], ConnectorState::Faulted);
        assert_eq!(state.evses[0].connectors[1], ConnectorState::Faulted);
        assert_eq!(state.evses[0].connectors[2], ConnectorState::Faulted);
        for connector_id in 0..3 {
            assert!(effects.contains(&ChargePointEffect::HardwareCommand(
                HardwareCommand::OpenContactor {
                    evse_id: 0,
                    connector_id,
                }
            )));
        }
    }

    #[test]
    fn an_evse_fault_ends_active_transactions_on_every_connector_it_covers() {
        let mut state = ChargePointState::new([2]);
        plug_in_and_authorize(&mut state);
        apply_connector_event(&mut state, ConnectorEvent::ChargingAuthorized(test_id_token()));
        apply_connector_event(&mut state, ConnectorEvent::ContactorClosed);
        assert!(state.evses[0].transactions[0].is_some());

        let effects = state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::FaultDetected,
        });

        assert_eq!(state.evses[0].transactions[0], None);
        assert!(effects.iter().any(|effect| matches!(
            effect,
            ChargePointEffect::TransactionEvent(TransactionEventOccurred {
                kind: TransactionEventKind::Ended,
                ..
            })
        )));
    }

    #[test]
    fn an_evse_fault_clearing_only_recovers_connectors_that_confirmed_their_contactor_is_open() {
        let mut state = ChargePointState::new([2]);
        state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::FaultDetected,
        });
        // Only connector 0's contactor has actually confirmed open; connector 1's hasn't.
        apply_connector_event(&mut state, ConnectorEvent::ContactorOpened);

        let effects = state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::FaultCleared,
        });

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Unlocking);
        assert_eq!(state.evses[0].connectors[1], ConnectorState::Faulted);
        assert!(effects.contains(&ChargePointEffect::HardwareCommand(
            HardwareCommand::UnlockConnector {
                evse_id: 0,
                connector_id: 0,
            }
        )));
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            ChargePointEffect::HardwareCommand(HardwareCommand::UnlockConnector {
                connector_id: 1,
                ..
            })
        )));
    }

    #[test]
    fn a_charge_point_wide_hardware_fault_cascades_to_every_evse_and_connector() {
        let mut state = ChargePointState::new([1, 1]);

        let effects = state.apply(ChargePointEvent::HardwareFault);

        assert_eq!(state.lifecycle, LifecycleState::Faulted);
        assert_eq!(state.evses[0].status, EvseStatus::Faulted);
        assert_eq!(state.evses[1].status, EvseStatus::Faulted);
        assert_eq!(state.evses[0].connectors[0], ConnectorState::Faulted);
        assert_eq!(state.evses[1].connectors[0], ConnectorState::Faulted);
        assert!(effects.contains(&ChargePointEffect::HardwareCommand(
            HardwareCommand::OpenContactor {
                evse_id: 0,
                connector_id: 0,
            }
        )));
        assert!(effects.contains(&ChargePointEffect::HardwareCommand(
            HardwareCommand::OpenContactor {
                evse_id: 1,
                connector_id: 0,
            }
        )));
    }

    #[test]
    fn a_charge_point_wide_fault_cleared_recovers_evses_whose_contactors_confirmed_open() {
        let mut state = ChargePointState::new([1, 1]);
        state.apply(ChargePointEvent::HardwareFault);
        apply_connector_event(&mut state, ConnectorEvent::ContactorOpened);
        state.apply(ChargePointEvent::Evse {
            evse_id: 1,
            event: EvseEvent::Connector {
                connector_id: 0,
                event: ConnectorEvent::ContactorOpened,
            },
        });

        let effects = state.apply(ChargePointEvent::FaultCleared);

        assert_eq!(state.lifecycle, LifecycleState::Unavailable);
        assert_eq!(state.evses[0].connectors[0], ConnectorState::Unlocking);
        assert_eq!(state.evses[1].connectors[0], ConnectorState::Unlocking);
        assert!(effects.contains(&ChargePointEffect::HardwareCommand(
            HardwareCommand::UnlockConnector {
                evse_id: 0,
                connector_id: 0,
            }
        )));
        assert!(effects.contains(&ChargePointEffect::HardwareCommand(
            HardwareCommand::UnlockConnector {
                evse_id: 1,
                connector_id: 0,
            }
        )));
    }

    fn reservation(id: i64) -> crate::state::Reservation {
        crate::state::Reservation {
            id: crate::state::ReservationId(id),
            id_token: test_id_token(),
        }
    }

    #[test]
    fn reserving_an_available_connector_records_the_reservation_and_reports_reserved() {
        let mut state = ChargePointState::new([1]);

        let effects = apply_connector_event(&mut state, ConnectorEvent::Reserved(reservation(1)));

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Reserved);
        assert_eq!(state.evses[0].reservations[0], Some(reservation(1)));
        assert!(effects.contains(&ChargePointEffect::StatusNotification(
            ConnectorStatusChanged {
                evse_id: 0,
                connector_id: 0,
                status: crate::state::ConnectorStatus::Reserved,
                connector_state: ConnectorState::Reserved,
            }
        )));
    }

    #[test]
    fn reserving_an_occupied_connector_is_ignored() {
        let mut state = ChargePointState::new([1]);
        apply_connector_event(&mut state, ConnectorEvent::CableConnected);
        apply_connector_event(&mut state, ConnectorEvent::LockConfirmed);

        apply_connector_event(&mut state, ConnectorEvent::Reserved(reservation(1)));

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Locked);
        assert_eq!(state.evses[0].reservations[0], None);
    }

    #[test]
    fn cancelling_a_reservation_frees_the_connector() {
        let mut state = ChargePointState::new([1]);
        apply_connector_event(&mut state, ConnectorEvent::Reserved(reservation(1)));

        let effects = apply_connector_event(&mut state, ConnectorEvent::ReservationCancelled);

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Available);
        assert_eq!(state.evses[0].reservations[0], None);
        assert!(effects.contains(&ChargePointEffect::StatusNotification(
            ConnectorStatusChanged {
                evse_id: 0,
                connector_id: 0,
                status: crate::state::ConnectorStatus::Available,
                connector_state: ConnectorState::Available,
            }
        )));
    }

    #[test]
    fn plugging_in_a_reserved_connector_proceeds_normally_and_clears_the_reservation() {
        let mut state = ChargePointState::new([1]);
        apply_connector_event(&mut state, ConnectorEvent::Reserved(reservation(1)));

        apply_connector_event(&mut state, ConnectorEvent::CableConnected);

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Connected);
        assert_eq!(state.evses[0].reservations[0], None);
    }

    #[test]
    fn a_local_list_update_replaces_the_list_and_records_the_version() {
        let mut state = ChargePointState::new([1]);
        let entry = crate::state::LocalListEntry {
            id_token: test_id_token(),
            status: crate::state::AuthorizationStatus::Accepted,
        };

        let effects = state.apply(ChargePointEvent::LocalListUpdated {
            version: 1,
            entries: alloc::vec![entry.clone()],
        });

        assert_eq!(state.local_authorization_list.version, 1);
        assert_eq!(state.local_authorization_list.entries, alloc::vec![entry]);
        assert!(effects.contains(&ChargePointEffect::StateChanged));
    }

    #[test]
    fn registering_a_device_model_variable_adds_it_and_reports_a_change() {
        let mut state = ChargePointState::new([1]);
        let component = crate::state::Component {
            name: "Custom".into(),
            instance: None,
            evse: None,
        };
        let variable = crate::state::Variable {
            name: "Setting".into(),
            instance: None,
        };

        let effects = state.apply(ChargePointEvent::DeviceModel(
            crate::state::DeviceModelEvent::VariableRegistered {
                component: component.clone(),
                variable: variable.clone(),
                characteristics: crate::state::VariableCharacteristics {
                    data_type: crate::state::VariableDataType::String,
                    unit: None,
                    min_limit: None,
                    max_limit: None,
                    values_list: None,
                    supports_monitoring: false,
                },
                attributes: alloc::vec![crate::state::VariableAttribute {
                    attribute_type: crate::state::VariableAttributeType::Actual,
                    value: "hello".into(),
                    mutability: crate::state::VariableMutability::ReadWrite,
                    persistent: false,
                    constant: false,
                    requires_reboot: false,
                }],
            },
        ));

        assert!(state.device_model.has_component(&component));
        assert!(effects.contains(&ChargePointEffect::StateChanged));
    }

    #[test]
    fn setting_a_device_model_attribute_value_updates_it_and_reports_a_change() {
        let mut state = ChargePointState::new([1]);

        let effects = state.apply(ChargePointEvent::DeviceModel(
            crate::state::DeviceModelEvent::AttributeValueSet {
                component: crate::state::Component {
                    name: "OCPPCommCtrlr".into(),
                    instance: None,
                    evse: None,
                },
                variable: crate::state::Variable {
                    name: "HeartbeatInterval".into(),
                    instance: None,
                },
                attribute_type: crate::state::VariableAttributeType::Actual,
                value: "120".into(),
            },
        ));

        let value = state
            .device_model
            .get(
                &crate::state::Component {
                    name: "OCPPCommCtrlr".into(),
                    instance: None,
                    evse: None,
                },
                &crate::state::Variable {
                    name: "HeartbeatInterval".into(),
                    instance: None,
                },
            )
            .unwrap()
            .attribute(crate::state::VariableAttributeType::Actual)
            .unwrap()
            .value
            .clone();
        assert_eq!(value, "120");
        assert!(effects.contains(&ChargePointEffect::StateChanged));
    }

    #[test]
    fn setting_an_unregistered_device_model_attribute_is_a_no_op() {
        let mut state = ChargePointState::new([1]);

        let effects = state.apply(ChargePointEvent::DeviceModel(
            crate::state::DeviceModelEvent::AttributeValueSet {
                component: crate::state::Component {
                    name: "Nonexistent".into(),
                    instance: None,
                    evse: None,
                },
                variable: crate::state::Variable {
                    name: "X".into(),
                    instance: None,
                },
                attribute_type: crate::state::VariableAttributeType::Actual,
                value: "1".into(),
            },
        ));

        assert!(effects.is_empty());
    }

    #[test]
    fn a_security_event_is_reported_without_changing_state() {
        let mut state = ChargePointState::new([1]);
        let event = crate::state::SecurityEvent {
            event_type: crate::state::SecurityEventType::TamperDetectionActivated,
            tech_info: Some("case opened".into()),
        };

        let effects = state.apply(ChargePointEvent::SecurityEventOccurred(event.clone()));

        assert_eq!(
            effects,
            alloc::vec![ChargePointEffect::SecurityEventOccurred(event)]
        );
    }

    #[test]
    fn a_cost_update_is_recorded_while_a_transaction_is_active() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);
        apply_connector_event(&mut state, ConnectorEvent::ChargingAuthorized(test_id_token()));

        apply_connector_event(&mut state, ConnectorEvent::CostUpdated(4.5));

        assert_eq!(state.evses[0].running_costs[0], Some(4.5));
    }

    #[test]
    fn a_cost_update_with_no_active_transaction_is_ignored() {
        let mut state = ChargePointState::new([1]);

        apply_connector_event(&mut state, ConnectorEvent::CostUpdated(4.5));

        assert_eq!(state.evses[0].running_costs[0], None);
    }

    #[test]
    fn a_new_transaction_does_not_inherit_the_previous_ones_cost() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);
        apply_connector_event(&mut state, ConnectorEvent::ChargingAuthorized(test_id_token()));
        apply_connector_event(&mut state, ConnectorEvent::ContactorClosed);
        apply_connector_event(&mut state, ConnectorEvent::CostUpdated(4.5));
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingStopped(StopReason::Local),
        );
        apply_connector_event(&mut state, ConnectorEvent::ContactorOpened);
        assert_eq!(state.evses[0].running_costs[0], None);

        apply_connector_event(&mut state, ConnectorEvent::UnlockConfirmed);
        apply_connector_event(&mut state, ConnectorEvent::CableDisconnected);
        plug_in_and_authorize(&mut state);
        apply_connector_event(&mut state, ConnectorEvent::ChargingAuthorized(test_id_token()));

        assert_eq!(state.evses[0].running_costs[0], None);
    }

    #[test]
    fn transaction_ids_increment_across_separate_sessions() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);
        apply_connector_event(&mut state, ConnectorEvent::ChargingAuthorized(test_id_token()));
        apply_connector_event(&mut state, ConnectorEvent::ContactorClosed);
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingStopped(StopReason::Local),
        );
        apply_connector_event(&mut state, ConnectorEvent::ContactorOpened);
        apply_connector_event(&mut state, ConnectorEvent::UnlockConfirmed);
        apply_connector_event(&mut state, ConnectorEvent::CableDisconnected);

        plug_in_and_authorize(&mut state);
        apply_connector_event(&mut state, ConnectorEvent::ChargingAuthorized(test_id_token()));

        assert_eq!(
            state.evses[0].transactions[0]
                .as_ref()
                .map(|transaction| transaction.id),
            Some(TransactionId(1))
        );
    }
}
