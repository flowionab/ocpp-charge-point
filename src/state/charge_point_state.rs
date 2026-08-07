use alloc::vec::Vec;
use chrono::{DateTime, Utc};

use crate::clock::MonotonicInstant;
use crate::hardware::Capabilities;
use crate::state::connector_state::ConnectorCommand;
use crate::state::{
    AuthorizationCache, AuthorizationRequested, ChargePointEffect, ChargePointEvent,
    ChargingProfileScope, ChargingProfileStore, ConnectorEvent, ConnectorState,
    ConnectorStatusChanged, DeviceModel, DeviceModelEvent, EvseEvent, EvseState, HardwareCommand,
    IdToken, LocalAuthorizationList, LocalListEntry, MeterSample, PendingReset, RegistrationStatus,
    ResetKind, ResetTarget, SecurityEvent, SecurityEventType, StateLimits, StopReason, Transaction,
    TransactionChargingState, TransactionEventKind, TransactionEventOccurred, TransactionId,
    TransactionUpdateReason,
};

/// This charge point's best current estimate of the CSMS's clock, anchored to a
/// [`MonotonicInstant`] so it can be advanced by elapsed real time without ever consulting a
/// (possibly absent or unsynchronized) wall clock - see `crate::clock` and
/// `crate::provisioning::evaluate_time_sync`. Stored on [`ChargePointState`] (mutated only via
/// [`ChargePointEvent::TimeSynced`]) rather than kept local to the Provisioning functional
/// block's heartbeat loop, because both [`crate::provisioning::register`]/
/// [`crate::provisioning::register_until_accepted`] (BootNotification) and
/// [`crate::provisioning::run_heartbeat`] (Heartbeat) need to compare against the *same* anchor
/// for drift detection to mean anything - a reconnect's fresh BootNotification must see the
/// heartbeat loop's last sync, and vice versa, or every exchange looks like a first-ever sync.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimeSyncAnchor {
    /// The CSMS's `currentTime` as of `recorded_at`.
    pub csms_time: DateTime<Utc>,
    /// The [`MonotonicInstant`] reading taken when `csms_time` was accepted. A later estimate is
    /// `csms_time + (now - recorded_at)`, using the same [`crate::clock::MonotonicClock`]
    /// instance throughout a process's lifetime (see [`MonotonicInstant`]'s docs on why readings
    /// from different clock instances cannot be compared).
    pub recorded_at: MonotonicInstant,
}

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
    /// A CSMS-initiated `Reset` request waiting for its target to settle before rebooting. See
    /// `docs/ROADMAP.md` §2.
    pub pending_reset: Option<PendingReset>,
    /// The Component/Variable device model (OCPP `GetVariables`/`SetVariables`). See
    /// `docs/ROADMAP.md` §2 and `crate::device_model`.
    pub device_model: DeviceModel,
    /// The hardware binding's declared capabilities (see [`ChargePointEvent::CapabilitiesDeclared`]).
    /// Conservatively empty ([`Capabilities::default`]) until `ChargePointBuilder::start` captures
    /// the real value - see `docs/PRODUCTION-ROADMAP.md` §5.3 (C3).
    pub capabilities: Capabilities,
    /// Authorization decisions the CSMS has already made, kept so a charge point that can't
    /// reach it can still answer - see [`AuthorizationCache`] and `docs/ROADMAP.md` §3.
    pub authorization_cache: AuthorizationCache,
    /// Every charging profile the CSMS has installed, across every scope - the Smart Charging
    /// functional block's state. See [`ChargingProfileStore`] and `docs/ROADMAP.md` §11.
    pub charging_profiles: ChargingProfileStore,
    /// This charge point's best current estimate of the CSMS's clock, established by
    /// BootNotification/Heartbeat's `currentTime` - see [`TimeSyncAnchor`] and
    /// [`ChargePointEvent::TimeSynced`]. `None` until the first exchange that carried a
    /// parseable `currentTime`.
    pub time_sync: Option<TimeSyncAnchor>,
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
    /// registration, no transactions, and an empty local authorization list. Uses this crate's
    /// default [`StateLimits`]; see [`Self::with_limits`] to bound the growable collections
    /// differently.
    pub fn new(connector_counts: impl IntoIterator<Item = usize>) -> Self {
        Self::with_limits(connector_counts, StateLimits::default())
    }

    /// [`Self::new`] with caller-chosen maxima for the growable collections (the local
    /// authorization list and the device model) - see [`StateLimits`] and
    /// `docs/PRODUCTION-ROADMAP.md` §9.2 (G2.2). The limits are fixed for the life of the state;
    /// nothing a CSMS or a hardware binding sends can raise them.
    pub fn with_limits(
        connector_counts: impl IntoIterator<Item = usize>,
        limits: StateLimits,
    ) -> Self {
        Self {
            lifecycle: LifecycleState::Booting,
            registration: None,
            evses: connector_counts.into_iter().map(EvseState::new).collect(),
            next_transaction_id: 0,
            local_authorization_list: LocalAuthorizationList::with_max_entries(
                limits.max_local_authorization_list_entries,
            ),
            pending_reset: None,
            device_model: DeviceModel::with_max_variables(limits.max_device_model_variables),
            capabilities: Capabilities::default(),
            authorization_cache: AuthorizationCache::with_max_entries(
                limits.max_authorization_cache_entries,
            ),
            charging_profiles: ChargingProfileStore::with_limit(limits.max_charging_profiles),
            time_sync: None,
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
                self.replace_local_authorization_list(version, entries, &mut effects);
                true
            }
            ChargePointEvent::SecurityEventOccurred(event) => {
                effects.push(ChargePointEffect::SecurityEventOccurred(event));
                false
            }
            ChargePointEvent::ResetRequested { target, kind } => {
                self.pending_reset = Some(PendingReset { target, kind });
                // `Immediate` kicks off the fail-safe stop right away, fanned out to every
                // connector in scope - each one's own state machine decides whether it's
                // actually affected (see `ConnectorEvent::ResetRequested`). `OnIdle` just
                // records the request; `check_pending_reset` below fires the reboot once (or
                // if) the target is already idle.
                if kind == ResetKind::Immediate {
                    for (evse_id, connector_id) in self.target_connector_addresses(target) {
                        self.apply_connector_event(
                            evse_id,
                            connector_id,
                            ConnectorEvent::ResetRequested,
                            &mut effects,
                        );
                    }
                }
                true
            }
            ChargePointEvent::AuthorizationCached {
                id_token,
                status,
                cached_at,
            } => self
                .authorization_cache
                .remember(id_token, status, cached_at),
            ChargePointEvent::AuthorizationCacheCleared => self.authorization_cache.clear() > 0,
            ChargePointEvent::PersistedAuthorizationCacheRestored { entries } => {
                let dropped = self.authorization_cache.replace(entries);
                if dropped > 0 {
                    tracing::warn!(
                        dropped,
                        max_entries = self.authorization_cache.max_entries(),
                        "truncated the recovered authorization cache to its configured maximum"
                    );
                }
                !self.authorization_cache.is_empty()
            }
            ChargePointEvent::ChargingProfileSet { scope, profile } => {
                let id = profile.id;
                match self.charging_profiles.install(scope, *profile) {
                    Ok(()) => true,
                    Err(rejection) => {
                        // Reached only if a caller dispatched this without asking the store first
                        // (`crate::smart_charging::handle_set_charging_profile` does ask, so the
                        // CSMS never sees an optimistic Accepted); logged rather than panicking,
                        // per `apply`'s documented tolerance for events that don't apply.
                        tracing::warn!(
                            profile_id = id.0,
                            ?rejection,
                            "a charging profile was refused by the store"
                        );
                        false
                    }
                }
            }
            ChargePointEvent::PersistedChargingProfilesRestored { profiles } => {
                let mut restored_any = false;
                let mut refused = 0usize;
                for installed in profiles {
                    if let ChargingProfileScope::Evse(evse_id) = installed.scope
                        && evse_id >= self.evses.len()
                    {
                        tracing::warn!(
                            evse_id,
                            profile_id = installed.profile.id.0,
                            "discarding a recovered charging profile for an EVSE this charge point no longer has"
                        );
                        continue;
                    }
                    match self
                        .charging_profiles
                        .install(installed.scope, installed.profile)
                    {
                        Ok(()) => restored_any = true,
                        Err(rejection) => {
                            tracing::warn!(?rejection, "a recovered charging profile was refused");
                            refused += 1;
                        }
                    }
                }
                if refused > 0 {
                    // A limit the CSMS believes is installed but that this charge point does not
                    // hold is worth reporting, not just logging - same stance the local
                    // authorization list's truncation takes.
                    effects.push(ChargePointEffect::SecurityEventOccurred(SecurityEvent {
                        event_type: SecurityEventType::MemoryExhaustion,
                        tech_info: Some(alloc::format!(
                            "dropped {refused} recovered charging profiles beyond the configured maximum of {}",
                            self.charging_profiles.max_profiles()
                        )),
                    }));
                }
                restored_any
            }
            ChargePointEvent::ChargingProfilesCleared { criteria } => {
                self.charging_profiles.clear(&criteria) > 0
            }
            ChargePointEvent::DeviceModel(event) => match event {
                DeviceModelEvent::VariableRegistered {
                    component,
                    variable,
                    characteristics,
                    attributes,
                } => {
                    // `false` means the model is at its configured maximum and the registration
                    // was refused (and logged) - reporting no change keeps that consistent with
                    // every other no-op event, rather than waking subscribers for nothing. See
                    // `DeviceModel::register` and `docs/PRODUCTION-ROADMAP.md` §9.2 (G2.2).
                    self.device_model
                        .register(component, variable, characteristics, attributes)
                }
                DeviceModelEvent::AttributeValueSet {
                    component,
                    variable,
                    attribute_type,
                    value,
                } => self.device_model.set_attribute_value(
                    &component,
                    &variable,
                    attribute_type,
                    value,
                ),
            },
            ChargePointEvent::PersistedTransactionsRestored {
                next_transaction_id,
                transactions,
            } => {
                // A floor, never an assignment - see the event's docs. Reissuing a transaction id
                // the CSMS has already seen is worse than skipping a few.
                let counter_changed =
                    set_if_changed_max(&mut self.next_transaction_id, next_transaction_id);
                let mut recovered_any = false;
                for recovered in transactions {
                    // Silently skipping an entry that addresses a connector this firmware no
                    // longer has (a topology change across the update that caused the reboot)
                    // matches `apply`'s documented tolerance for events that don't apply.
                    let addressable = self
                        .evses
                        .get(recovered.evse_id)
                        .is_some_and(|evse| recovered.connector_id < evse.connectors.len());
                    if !addressable {
                        tracing::warn!(
                            evse_id = recovered.evse_id,
                            connector_id = recovered.connector_id,
                            "discarding a recovered transaction for a connector this charge point no longer has"
                        );
                        continue;
                    }
                    let mut transaction = recovered.transaction;
                    transaction.stop_reason = Some(StopReason::PowerLoss);
                    transaction.seq_no += 1;
                    recovered_any = true;
                    effects.push(ChargePointEffect::TransactionEvent(
                        TransactionEventOccurred {
                            evse_id: recovered.evse_id,
                            connector_id: recovered.connector_id,
                            kind: TransactionEventKind::Ended,
                            transaction,
                        },
                    ));
                }
                counter_changed || recovered_any
            }
            ChargePointEvent::PersistedLocalAuthorizationListRestored { version, entries } => {
                self.replace_local_authorization_list(version, entries, &mut effects);
                true
            }
            ChargePointEvent::PersistedReservationsRestored { reservations } => {
                let mut restored_any = false;
                for recovered in reservations {
                    let addressable = self
                        .evses
                        .get(recovered.evse_id)
                        .is_some_and(|evse| recovered.connector_id < evse.connectors.len());
                    if !addressable {
                        tracing::warn!(
                            evse_id = recovered.evse_id,
                            connector_id = recovered.connector_id,
                            "discarding a recovered reservation for a connector this charge point no longer has"
                        );
                        continue;
                    }
                    let current = self.evses[recovered.evse_id].connectors[recovered.connector_id];
                    if current != ConnectorState::Available {
                        tracing::warn!(
                            evse_id = recovered.evse_id,
                            connector_id = recovered.connector_id,
                            ?current,
                            "discarding a recovered reservation for a connector that isn't Available at boot"
                        );
                        continue;
                    }
                    restored_any |= self.apply_connector_event(
                        recovered.evse_id,
                        recovered.connector_id,
                        ConnectorEvent::Reserved(recovered.reservation),
                        &mut effects,
                    );
                }
                restored_any
            }
            ChargePointEvent::PersistedDeviceModelAttributesRestored { attributes } => {
                let mut restored_any = false;
                for recovered in attributes {
                    let applied = self.device_model.set_attribute_value(
                        &recovered.component,
                        &recovered.variable,
                        recovered.attribute_type,
                        recovered.value,
                    );
                    if !applied {
                        // The binding is the source of truth for which variables exist this boot
                        // (see the event's docs) - a persisted value for one it didn't
                        // re-register is left dormant, not applied, and not silently dropped
                        // either: logged so an integrator can notice a variable disappeared.
                        tracing::warn!(
                            component = recovered.component.name.as_str(),
                            variable = recovered.variable.name.as_str(),
                            "discarding a persisted device model attribute for a variable this \
                             firmware/hardware combination did not register this boot"
                        );
                    }
                    restored_any |= applied;
                }
                restored_any
            }
            ChargePointEvent::CapabilitiesDeclared(capabilities) => {
                set_if_changed(&mut self.capabilities, capabilities)
            }
            ChargePointEvent::TimeSynced {
                csms_time,
                recorded_at,
            } => set_if_changed(
                &mut self.time_sync,
                Some(TimeSyncAnchor {
                    csms_time,
                    recorded_at,
                }),
            ),
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
        self.check_pending_reset(&mut effects);
        effects
    }

    /// Applies a [`ConnectorEvent`] to one `(evse_id, connector_id)`, pushing every resulting
    /// effect onto `effects` and returning whether anything actually changed. Shared by the
    /// normal per-connector event path (`ChargePointEvent::Evse { event: EvseEvent::Connector {
    /// .. }, .. }`), by an `Immediate` `Reset`'s fan-out to every connector in its target scope
    /// (see `ChargePointEvent::ResetRequested`), and by fault cascading (below) - so a cascaded
    /// `FaultDetected`/`FaultCleared` produces exactly the same effects a single-connector fault
    /// would. All three need the exact same transition/hardware-command/status/transaction
    /// bookkeeping.
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
            ConnectorEvent::ResetRequested => Some(StopReason::Reset),
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
        let computed_limit = match &event {
            ConnectorEvent::CurrentLimitComputed(limit_ma) => Some(*limit_ma),
            _ => None,
        };
        let confirmed_limit = match &event {
            ConnectorEvent::CurrentLimitConfirmed(limit_ma) => Some(*limit_ma),
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
        if new_state == ConnectorState::Authorizing
            && let Some(id_token) = presented_id_token
        {
            effects.push(ChargePointEffect::AuthorizationRequested(
                AuthorizationRequested {
                    evse_id,
                    connector_id,
                    id_token,
                },
            ));
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
                ) && let Some(cost_slot) = evse.running_costs.get_mut(connector_id)
                {
                    *cost_slot = None;
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
            if let Some(sample) = meter_sample
                && let Some((kind, transaction)) = apply_meter_sample(slot, sample)
            {
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
        // Only recorded while a transaction is actually active on this connector - there's
        // nothing meaningful to attach a cost to otherwise. A recorded cost doesn't change
        // `ConnectorState` itself, so `transition.changed` alone wouldn't notice it - without
        // folding it into the returned value here, the actor's watch channel would never
        // publish it (see `ChargePointEffect::StateChanged`).
        // Recorded for every sample, whatever the connector is doing: clock-aligned MeterValues
        // (B1.1) are due on the wall clock whether or not a transaction is running, so the
        // reading has to outlive the session that produced it. The transaction's own
        // `last_meter_sample` is still only updated while charging - see `apply_meter_sample`.
        let sample_recorded = meter_sample.is_some_and(|sample| {
            let Some(slot) = evse.latest_meter_samples.get_mut(connector_id) else {
                return false;
            };
            set_if_changed(slot, Some(sample))
        });
        // A newly computed limit reaches hardware only when it actually differs from the one
        // already requested for this connector: the projection re-evaluates on every state
        // change, and re-issuing an unchanged limit would put a hardware call on the path of
        // every meter sample.
        let limit_changed = computed_limit.is_some_and(|limit_ma| {
            let Some(slot) = evse.charging_limits.get_mut(connector_id) else {
                return false;
            };
            if *slot == limit_ma {
                return false;
            }
            *slot = limit_ma;
            effects.push(ChargePointEffect::HardwareCommand(
                HardwareCommand::SetCurrentLimit {
                    evse_id,
                    connector_id,
                    limit_ma,
                },
            ));
            true
        });
        let limit_confirmed = confirmed_limit.is_some_and(|limit_ma| {
            let Some(slot) = evse.applied_charging_limits.get_mut(connector_id) else {
                return false;
            };
            set_if_changed(slot, limit_ma)
        });
        let cost_recorded = cost_update.is_some_and(|total_cost| {
            if evse
                .transactions
                .get(connector_id)
                .is_some_and(Option::is_some)
                && let Some(cost_slot) = evse.running_costs.get_mut(connector_id)
            {
                *cost_slot = Some(total_cost);
                return true;
            }
            false
        });
        transition.changed || cost_recorded || limit_changed || limit_confirmed || sample_recorded
    }

    /// Every `evse_id` a [`ResetTarget`] covers - every EVSE, for
    /// [`ResetTarget::ChargePoint`], or the one addressed EVSE (if it exists) for
    /// [`ResetTarget::Evse`].
    /// Replaces the local authorization list, enforcing its configured maximum (see
    /// [`StateLimits::max_local_authorization_list_entries`]). Truncation means authorization
    /// decisions the CSMS believes are cached have been silently dropped, so it raises a
    /// `MemoryExhaustion` security event as well as logging - the same treatment a saturated
    /// [`crate::offline_queue::OfflineQueue`] gets (G2.1), for the same reason: the CSMS is the
    /// only party that can act on it.
    ///
    /// In practice this never truncates a live `SendLocalList` -
    /// [`crate::local_authorization_list::handle_send_local_list`] refuses an over-long update
    /// before it becomes an event. The reachable case is a list restored from durable storage that
    /// was written by a build configured with a larger maximum.
    fn replace_local_authorization_list(
        &mut self,
        version: i64,
        entries: Vec<LocalListEntry>,
        effects: &mut Vec<ChargePointEffect>,
    ) {
        let dropped = self.local_authorization_list.replace(version, entries);
        if dropped > 0 {
            tracing::warn!(
                dropped,
                max_entries = self.local_authorization_list.max_entries,
                "truncated the local authorization list to its configured maximum"
            );
            effects.push(ChargePointEffect::SecurityEventOccurred(SecurityEvent {
                event_type: SecurityEventType::MemoryExhaustion,
                tech_info: Some(alloc::format!(
                    "dropped {dropped} local authorization list entries beyond the configured maximum of {}",
                    self.local_authorization_list.max_entries
                )),
            }));
        }
    }

    fn target_evse_ids(&self, target: ResetTarget) -> Vec<usize> {
        match target {
            ResetTarget::ChargePoint => (0..self.evses.len()).collect(),
            ResetTarget::Evse { evse_id } => {
                if evse_id < self.evses.len() {
                    alloc::vec![evse_id]
                } else {
                    Vec::new()
                }
            }
        }
    }

    /// Every `(evse_id, connector_id)` a [`ResetTarget`] covers.
    fn target_connector_addresses(&self, target: ResetTarget) -> Vec<(usize, usize)> {
        self.target_evse_ids(target)
            .into_iter()
            .flat_map(|evse_id| {
                let connector_count = self.evses[evse_id].connectors.len();
                (0..connector_count).map(move |connector_id| (evse_id, connector_id))
            })
            .collect()
    }

    /// Whether `pending`'s target has settled enough to reboot: for `Immediate`, every
    /// connector in scope has moved past the fail-safe stop this crate itself drove it through
    /// (no longer `Stopping`/`Finishing`); for `OnIdle`, no connector in scope has a transaction
    /// in progress. Either way, this can already be true the instant the request comes in (an
    /// already-idle `OnIdle` target, or an `Immediate` target with nothing to interrupt) - the
    /// reboot then fires immediately, without waiting on any hardware confirmation.
    fn pending_reset_ready(&self, pending: &PendingReset) -> bool {
        match pending.kind {
            ResetKind::Immediate => !self
                .target_connector_addresses(pending.target)
                .into_iter()
                .any(|(evse_id, connector_id)| {
                    matches!(
                        self.evses[evse_id].connectors[connector_id],
                        ConnectorState::Stopping | ConnectorState::Finishing
                    )
                }),
            ResetKind::OnIdle => !self
                .target_connector_addresses(pending.target)
                .into_iter()
                .any(|(evse_id, connector_id)| {
                    self.evses[evse_id].transactions[connector_id].is_some()
                }),
        }
    }

    /// Fires the reboot - one [`HardwareCommand::Reboot`] per EVSE in scope - and clears
    /// `pending_reset` once [`Self::pending_reset_ready`] says the target has settled. Called
    /// unconditionally at the end of every [`Self::apply`], since any event (a hardware
    /// confirmation completing a forced stop, or a transaction ending naturally) can be the one
    /// that satisfies it.
    fn check_pending_reset(&mut self, effects: &mut Vec<ChargePointEffect>) {
        let Some(pending) = self.pending_reset else {
            return;
        };
        if !self.pending_reset_ready(&pending) {
            return;
        }
        self.pending_reset = None;
        effects.push(ChargePointEffect::StateChanged);
        for evse_id in self.target_evse_ids(pending.target) {
            effects.push(ChargePointEffect::HardwareCommand(
                HardwareCommand::Reboot { evse_id },
            ));
        }
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
/// carried by the triggering `ConnectorEvent::ChargingStopped`/`ConnectorEvent::ResetRequested`,
/// if that's what caused this transition. `event_id_token` is the identifier carried by a
/// triggering `ChargingAuthorized`/`RemoteStartRequested`, if that's what caused this transition
/// - recorded on the new `Transaction`.
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
        // Every move between "energy is flowing" and "energy is paused, by one side or the
        // other" is a charging-state change on the same running transaction - which is how 2.x
        // expresses suspension at all (its connector status has no `SuspendedEV`; 1.6J's does,
        // reported separately by that version's StatusNotification adapter). Reaching `Charging`
        // from a suspended state is a resume, not a new transaction, so this arm covers all of
        // them together rather than special-casing the first one.
        (
            ConnectorState::Starting
            | ConnectorState::Charging
            | ConnectorState::SuspendedEv
            | ConnectorState::SuspendedEvse,
            ConnectorState::Charging | ConnectorState::SuspendedEv | ConnectorState::SuspendedEvse,
            // Only an actual move between those states counts. A connector can self-loop
            // (`Charging` -> `Charging` when a meter sample is applied, say), and reporting a
            // charging-state change for that would bump `seqNo` and send the CSMS an Updated event
            // saying nothing changed.
        ) if previous_state != new_state => {
            let charging_state = match new_state {
                ConnectorState::SuspendedEv => TransactionChargingState::SuspendedEV,
                ConnectorState::SuspendedEvse => TransactionChargingState::SuspendedEVSE,
                _ => TransactionChargingState::Charging,
            };
            let transaction = slot.as_mut()?;
            transaction.charging_state = charging_state;
            transaction.seq_no += 1;
            Some((
                TransactionEventKind::Updated(TransactionUpdateReason::ChargingStateChanged),
                transaction.clone(),
            ))
        }
        // Normally reached only from `Charging` (`ChargingStopped`). A CSMS-initiated `Reset`
        // (Immediate) can also interrupt a transaction that's still `Starting` (contactor not
        // yet confirmed closed) - see `ConnectorEvent::ResetRequested`.
        (
            ConnectorState::Charging
            | ConnectorState::SuspendedEv
            | ConnectorState::SuspendedEvse
            | ConnectorState::Starting,
            ConnectorState::Stopping,
        ) => {
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

/// Raises `current` to `next` if `next` is larger, reporting whether it moved. Used for
/// monotonic counters that recovery may only ever advance, never rewind.
fn set_if_changed_max(current: &mut u64, next: u64) -> bool {
    if next > *current {
        *current = next;
        true
    } else {
        false
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
    fn an_internal_transition_that_keeps_the_same_ocpp_status_still_reports_the_richer_connector_state()
     {
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

        let effects = apply_connector_event(
            &mut state,
            ConnectorEvent::RemoteStartRequested(test_id_token()),
        );

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
        assert_eq!(
            state.evses[0].transactions[0],
            Some(expected_transaction.clone())
        );
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

        let effects = apply_connector_event(
            &mut state,
            ConnectorEvent::RemoteStartRequested(test_id_token()),
        );

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

        let effects = apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );

        let expected_transaction = Transaction {
            id: TransactionId(0),
            id_token: Some(test_id_token()),
            charging_state: TransactionChargingState::EvConnected,
            stop_reason: None,
            seq_no: 0,
            last_meter_sample: None,
        };
        assert_eq!(
            state.evses[0].transactions[0],
            Some(expected_transaction.clone())
        );
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
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );

        let effects = apply_connector_event(&mut state, ConnectorEvent::ContactorClosed);

        let expected_transaction = Transaction {
            id: TransactionId(0),
            id_token: Some(test_id_token()),
            charging_state: TransactionChargingState::Charging,
            stop_reason: None,
            seq_no: 1,
            last_meter_sample: None,
        };
        assert_eq!(
            state.evses[0].transactions[0],
            Some(expected_transaction.clone())
        );
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
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
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
        assert_eq!(
            state.evses[0].transactions[0],
            Some(expected_transaction.clone())
        );
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
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
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
    fn a_meter_reading_with_no_active_transaction_reports_nothing_but_is_still_recorded() {
        let mut state = ChargePointState::new([1]);

        let effects = apply_connector_event(
            &mut state,
            ConnectorEvent::MeterValueSampled(MeterSample {
                energy_wh: 1_500,
                ..Default::default()
            }),
        );

        // No transaction to report against - this must not fabricate a `TransactionEvent`.
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, ChargePointEffect::TransactionEvent(_)))
        );
        // But the reading itself is kept: clock-aligned MeterValues (B1.1) are due whether or not
        // anything is charging, so a reading taken between sessions is exactly what they report.
        assert_eq!(
            state.evses[0].latest_meter_samples[0].map(|sample| sample.energy_wh),
            Some(1_500)
        );
        assert!(effects.contains(&ChargePointEffect::StateChanged));
    }

    #[test]
    fn the_latest_meter_reading_outlives_the_transaction_that_produced_it() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
        apply_connector_event(&mut state, ConnectorEvent::ContactorClosed);
        apply_connector_event(
            &mut state,
            ConnectorEvent::MeterValueSampled(MeterSample {
                energy_wh: 4_200,
                ..Default::default()
            }),
        );
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingStopped(StopReason::Local),
        );
        apply_connector_event(&mut state, ConnectorEvent::ContactorOpened);

        assert!(state.evses[0].transactions[0].is_none());
        assert_eq!(
            state.evses[0].latest_meter_samples[0].map(|sample| sample.energy_wh),
            Some(4_200),
            "the meter register does not reset when a session ends, and neither should the \
             reading standalone MeterValues reports"
        );
    }

    #[test]
    fn stopping_charging_ends_the_transaction_once_the_contactor_confirms_open() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
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
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
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
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
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
            expires_at: None,
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

    /// G2.2 (docs/PRODUCTION-ROADMAP.md §9.2): the state machine is the last line of defence for
    /// the local authorization list's bound - `handle_send_local_list` already rejects an
    /// over-long CSMS update, so the only way to get here is a restore from durable storage
    /// written by a build with a larger limit.
    #[test]
    fn a_local_list_update_beyond_the_maximum_is_truncated_and_reported_as_memory_exhaustion() {
        let mut state = ChargePointState::with_limits(
            [1],
            StateLimits::default().with_max_local_authorization_list_entries(1),
        );
        let entries = alloc::vec![
            crate::state::LocalListEntry {
                id_token: test_id_token(),
                status: crate::state::AuthorizationStatus::Accepted,
            },
            crate::state::LocalListEntry {
                id_token: IdToken {
                    value: "second".into(),
                    kind: IdTokenKind::ISO14443,
                },
                status: crate::state::AuthorizationStatus::Accepted,
            },
        ];

        let effects = state.apply(ChargePointEvent::LocalListUpdated {
            version: 4,
            entries: entries.clone(),
        });

        assert_eq!(
            state.local_authorization_list.entries,
            entries[..1].to_vec()
        );
        assert!(effects.iter().any(|effect| matches!(
            effect,
            ChargePointEffect::SecurityEventOccurred(event)
                if event.event_type == crate::state::SecurityEventType::MemoryExhaustion
        )));
    }

    #[test]
    fn a_restored_local_list_beyond_the_maximum_is_truncated_too() {
        let mut state = ChargePointState::with_limits(
            [1],
            StateLimits::default().with_max_local_authorization_list_entries(1),
        );

        let effects = state.apply(ChargePointEvent::PersistedLocalAuthorizationListRestored {
            version: 9,
            entries: alloc::vec![
                crate::state::LocalListEntry {
                    id_token: test_id_token(),
                    status: crate::state::AuthorizationStatus::Accepted,
                },
                crate::state::LocalListEntry {
                    id_token: IdToken {
                        value: "second".into(),
                        kind: IdTokenKind::ISO14443,
                    },
                    status: crate::state::AuthorizationStatus::Accepted,
                },
            ],
        });

        assert_eq!(state.local_authorization_list.entries.len(), 1);
        assert_eq!(state.local_authorization_list.version, 9);
        assert!(effects.iter().any(|effect| matches!(
            effect,
            ChargePointEffect::SecurityEventOccurred(event)
                if event.event_type == crate::state::SecurityEventType::MemoryExhaustion
        )));
    }

    /// G2.2: the device model's bound applies to whatever a hardware binding registers during
    /// `ChargePoint::start`, so a binding with a runaway registration loop can't grow state
    /// without limit.
    #[test]
    fn registering_a_device_model_variable_beyond_the_maximum_is_refused() {
        let mut state = ChargePointState::with_limits(
            [1],
            StateLimits::default().with_max_device_model_variables(1),
        );
        let before = state.device_model.len();

        let effects = state.apply(ChargePointEvent::DeviceModel(
            crate::state::DeviceModelEvent::VariableRegistered {
                component: crate::state::Component {
                    name: "Custom".into(),
                    instance: None,
                    evse: None,
                },
                variable: crate::state::Variable {
                    name: "Setting".into(),
                    instance: None,
                },
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
                    value: "x".into(),
                    mutability: crate::state::VariableMutability::ReadWrite,
                    persistent: false,
                    constant: false,
                    requires_reboot: false,
                }],
            },
        ));

        assert_eq!(state.device_model.len(), before);
        assert!(effects.is_empty());
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
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );

        apply_connector_event(&mut state, ConnectorEvent::CostUpdated(4.5));

        assert_eq!(state.evses[0].running_costs[0], Some(4.5));
    }

    fn test_charging_profile(id: i32) -> crate::state::ChargingProfile {
        use crate::state::{
            ChargingProfileId, ChargingProfileKind, ChargingProfilePurpose, ChargingRateUnit,
            ChargingSchedule, ChargingSchedulePeriod,
        };
        crate::state::ChargingProfile {
            id: ChargingProfileId(id),
            stack_level: 0,
            purpose: ChargingProfilePurpose::TxDefault,
            kind: ChargingProfileKind::Absolute,
            recurrency: None,
            valid_from: None,
            valid_to: None,
            transaction_id: None,
            schedules: alloc::vec![ChargingSchedule {
                id: 1,
                start_schedule: None,
                duration_secs: None,
                rate_unit: ChargingRateUnit::Amps,
                min_charging_rate: None,
                periods: alloc::vec![ChargingSchedulePeriod {
                    start_period_secs: 0,
                    limit: 16.0,
                    number_phases: None,
                }],
            }],
        }
    }

    #[test]
    fn setting_and_clearing_a_charging_profile_goes_through_the_store() {
        use crate::state::{ChargingProfileCriteria, ChargingProfileId, ChargingProfileScope};
        let mut state = ChargePointState::new([1]);

        let effects = state.apply(ChargePointEvent::ChargingProfileSet {
            scope: ChargingProfileScope::Evse(0),
            profile: alloc::boxed::Box::new(test_charging_profile(1)),
        });
        assert!(effects.contains(&ChargePointEffect::StateChanged));
        assert_eq!(state.charging_profiles.len(), 1);

        // Criteria that match nothing leave the store alone, and report no state change.
        let effects = state.apply(ChargePointEvent::ChargingProfilesCleared {
            criteria: ChargingProfileCriteria {
                id: Some(ChargingProfileId(99)),
                ..Default::default()
            },
        });
        assert!(!effects.contains(&ChargePointEffect::StateChanged));
        assert_eq!(state.charging_profiles.len(), 1);

        let effects = state.apply(ChargePointEvent::ChargingProfilesCleared {
            criteria: ChargingProfileCriteria::default(),
        });
        assert!(effects.contains(&ChargePointEffect::StateChanged));
        assert!(state.charging_profiles.is_empty());
    }

    #[test]
    fn a_profile_the_store_refuses_is_logged_rather_than_applied() {
        use crate::state::ChargingProfileScope;
        let mut state = ChargePointState::new([1]);
        let mut without_schedule = test_charging_profile(1);
        without_schedule.schedules.clear();

        let effects = state.apply(ChargePointEvent::ChargingProfileSet {
            scope: ChargingProfileScope::Evse(0),
            profile: alloc::boxed::Box::new(without_schedule),
        });

        assert!(effects.is_empty());
        assert!(state.charging_profiles.is_empty());
    }

    #[test]
    fn a_computed_current_limit_reaches_hardware_once_per_actual_change() {
        let mut state = ChargePointState::new([1]);

        let effects = apply_connector_event(
            &mut state,
            ConnectorEvent::CurrentLimitComputed(Some(16_000)),
        );
        assert!(effects.contains(&ChargePointEffect::HardwareCommand(
            HardwareCommand::SetCurrentLimit {
                evse_id: 0,
                connector_id: 0,
                limit_ma: Some(16_000),
            }
        )));
        assert_eq!(state.evses[0].charging_limits[0], Some(16_000));

        // The same limit again is not re-issued to hardware.
        let effects = apply_connector_event(
            &mut state,
            ConnectorEvent::CurrentLimitComputed(Some(16_000)),
        );
        assert!(effects.is_empty());

        // Dropping the limit entirely is a real change, and is dispatched as such - hardware
        // has to be told to stop limiting, not left holding the last value forever.
        let effects = apply_connector_event(&mut state, ConnectorEvent::CurrentLimitComputed(None));
        assert!(effects.contains(&ChargePointEffect::HardwareCommand(
            HardwareCommand::SetCurrentLimit {
                evse_id: 0,
                connector_id: 0,
                limit_ma: None,
            }
        )));
        assert_eq!(state.evses[0].charging_limits[0], None);
    }

    #[test]
    fn a_confirmed_current_limit_is_recorded_separately_from_the_requested_one() {
        let mut state = ChargePointState::new([1]);

        apply_connector_event(
            &mut state,
            ConnectorEvent::CurrentLimitComputed(Some(16_000)),
        );
        assert_eq!(state.evses[0].applied_charging_limits[0], None);

        let effects = apply_connector_event(
            &mut state,
            ConnectorEvent::CurrentLimitConfirmed(Some(16_000)),
        );
        assert!(effects.contains(&ChargePointEffect::StateChanged));
        assert_eq!(state.evses[0].applied_charging_limits[0], Some(16_000));
        // Confirming does not re-issue the hardware command that asked for it.
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            ChargePointEffect::HardwareCommand(HardwareCommand::SetCurrentLimit { .. })
        )));
    }

    #[test]
    fn suspending_and_resuming_reports_the_transactions_charging_state_each_way() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
        apply_connector_event(&mut state, ConnectorEvent::ContactorClosed);

        for (event, expected) in [
            (
                ConnectorEvent::ChargingSuspendedByEv,
                TransactionChargingState::SuspendedEV,
            ),
            (
                ConnectorEvent::ChargingSuspendedByEvse,
                TransactionChargingState::SuspendedEVSE,
            ),
            (
                ConnectorEvent::ChargingResumed,
                TransactionChargingState::Charging,
            ),
        ] {
            let effects = apply_connector_event(&mut state, event);

            let reported = effects
                .iter()
                .find_map(|effect| match effect {
                    ChargePointEffect::TransactionEvent(occurred) => Some(occurred),
                    _ => None,
                })
                .expect("a suspension or resume is a charging-state change on the transaction");
            assert_eq!(
                reported.kind,
                TransactionEventKind::Updated(TransactionUpdateReason::ChargingStateChanged)
            );
            assert_eq!(reported.transaction.charging_state, expected);
            // The transaction keeps running throughout - this is a pause, not a stop.
            assert!(reported.transaction.stop_reason.is_none());
            assert!(state.evses[0].transactions[0].is_some());
        }
    }

    #[test]
    fn a_suspension_reports_no_status_notification_change_to_2_x_but_does_report_the_transition() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
        apply_connector_event(&mut state, ConnectorEvent::ContactorClosed);

        let effects = apply_connector_event(&mut state, ConnectorEvent::ChargingSuspendedByEv);

        // The coarse status stays `Occupied` (2.x has no suspended connector status), but the
        // notification still carries the full `ConnectorState`, which is what lets the 1.6J
        // adapter report `SuspendedEV` - see `ConnectorStatusChanged::connector_state`.
        let status = effects
            .iter()
            .find_map(|effect| match effect {
                ChargePointEffect::StatusNotification(changed) => Some(changed),
                _ => None,
            })
            .expect("every connector transition reports a status notification");
        assert_eq!(status.status, crate::state::ConnectorStatus::Occupied);
        assert_eq!(status.connector_state, ConnectorState::SuspendedEv);
    }

    #[test]
    fn a_meter_reading_while_suspended_is_still_recorded_against_the_transaction() {
        // A suspended session is still a session: the meter register can move (standing losses,
        // a trickle) and the reading must not be dropped just because energy isn't flowing.
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
        apply_connector_event(&mut state, ConnectorEvent::ContactorClosed);
        apply_connector_event(&mut state, ConnectorEvent::ChargingSuspendedByEv);

        apply_connector_event(
            &mut state,
            ConnectorEvent::MeterValueSampled(MeterSample {
                energy_wh: 5_000,
                ..Default::default()
            }),
        );

        assert_eq!(
            state.evses[0].latest_meter_samples[0].map(|sample| sample.energy_wh),
            Some(5_000)
        );
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
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
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
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );

        assert_eq!(state.evses[0].running_costs[0], None);
    }

    /// A transaction as it would have been persisted mid-charge: `Charging`, with the last meter
    /// reading that reached storage before the power cut.
    fn interrupted_transaction() -> Transaction {
        Transaction {
            id: TransactionId(7),
            id_token: Some(test_id_token()),
            charging_state: TransactionChargingState::Charging,
            stop_reason: None,
            seq_no: 12,
            last_meter_sample: Some(MeterSample {
                energy_wh: 4_200,
                ..Default::default()
            }),
        }
    }

    #[test]
    fn a_recovered_transaction_is_closed_out_as_ended_with_power_loss() {
        let mut state = ChargePointState::new([1]);

        let effects = state.apply(ChargePointEvent::PersistedTransactionsRestored {
            next_transaction_id: 8,
            transactions: alloc::vec![crate::state::RecoveredTransaction {
                evse_id: 0,
                connector_id: 0,
                transaction: interrupted_transaction(),
            }],
        });

        // The billable energy must survive: the last persisted meter reading is still attached to
        // the closing event, and the `seqNo` continues where the interrupted transaction left off
        // rather than restarting.
        let expected = Transaction {
            stop_reason: Some(StopReason::PowerLoss),
            seq_no: 13,
            ..interrupted_transaction()
        };
        assert!(effects.contains(&ChargePointEffect::TransactionEvent(
            TransactionEventOccurred {
                evse_id: 0,
                connector_id: 0,
                kind: TransactionEventKind::Ended,
                transaction: expected,
            }
        )));
        // Closed out, not resumed - nothing is left occupying the connector's transaction slot.
        assert_eq!(state.evses[0].transactions[0], None);
        assert!(effects.contains(&ChargePointEffect::StateChanged));
    }

    #[test]
    fn recovery_restores_the_transaction_id_counter_so_ids_are_never_reused() {
        let mut state = ChargePointState::new([1]);

        state.apply(ChargePointEvent::PersistedTransactionsRestored {
            next_transaction_id: 8,
            transactions: Vec::new(),
        });

        assert_eq!(state.next_transaction_id, 8);

        plug_in_and_authorize(&mut state);
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
        assert_eq!(
            state.evses[0].transactions[0]
                .as_ref()
                .map(|transaction| transaction.id),
            Some(TransactionId(8))
        );
    }

    #[test]
    fn recovery_never_rewinds_a_counter_that_has_already_advanced_further() {
        let mut state = ChargePointState::new([1]);
        state.next_transaction_id = 20;

        state.apply(ChargePointEvent::PersistedTransactionsRestored {
            next_transaction_id: 8,
            transactions: Vec::new(),
        });

        assert_eq!(state.next_transaction_id, 20);
    }

    #[test]
    fn recovering_nothing_changes_nothing() {
        let mut state = ChargePointState::new([1]);

        let effects = state.apply(ChargePointEvent::PersistedTransactionsRestored {
            next_transaction_id: 0,
            transactions: Vec::new(),
        });

        assert!(effects.is_empty());
    }

    #[test]
    fn a_recovered_transaction_addressing_a_connector_that_does_not_exist_is_ignored() {
        let mut state = ChargePointState::new([1]);

        let effects = state.apply(ChargePointEvent::PersistedTransactionsRestored {
            next_transaction_id: 0,
            transactions: alloc::vec![crate::state::RecoveredTransaction {
                evse_id: 4,
                connector_id: 9,
                transaction: interrupted_transaction(),
            }],
        });

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
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
        apply_connector_event(&mut state, ConnectorEvent::ContactorClosed);
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingStopped(StopReason::Local),
        );
        apply_connector_event(&mut state, ConnectorEvent::ContactorOpened);
        apply_connector_event(&mut state, ConnectorEvent::UnlockConfirmed);
        apply_connector_event(&mut state, ConnectorEvent::CableDisconnected);

        plug_in_and_authorize(&mut state);
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );

        assert_eq!(
            state.evses[0].transactions[0]
                .as_ref()
                .map(|transaction| transaction.id),
            Some(TransactionId(1))
        );
    }
}
