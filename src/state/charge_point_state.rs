use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use chrono::{DateTime, Utc};

use crate::clock::MonotonicInstant;
use crate::hardware::Capabilities;
use crate::state::connector_state::{ConnectorCommand, ConnectorPolicy, TxStartPoint, TxStopPoint};
use crate::state::device_model::{
    AVAILABILITY_COMPONENT_CHARGE_POINT, AVAILABILITY_COMPONENT_CONNECTOR,
    AVAILABILITY_COMPONENT_EVSE, AVAILABILITY_STATE_VARIABLE, CLOCK_COMPONENT,
    CLOCK_DATE_TIME_VARIABLE, NETWORK_CONFIGURATION_COMPONENT, PLUG_RETENTION_LOCK_COMPONENT,
    PROBLEM_VARIABLE,
};
use crate::state::{
    AfrrSignal, AuthorizationCache, AuthorizationRequested, BatterySwapStore, ChargePointEffect,
    ChargePointEvent, ChargingProfileScope, ChargingProfileStore, Component, ConnectorEvent,
    ConnectorState, ConnectorStatus, ConnectorStatusChanged, DERControlStore, DeviceModel,
    DeviceModelEvent, DisplayMessageStore, EventTrigger, EvseEvent, EvseState, EvseStatus,
    ExternalChargingLimit, HardwareCommand, IdToken, LocalAuthorizationList, LocalListEntry,
    MeterSample, NetworkProfileStore, PendingReset, PeriodicEventStreamStore, RegistrationStatus,
    ReservationEndReason, ReservationUpdate, ResetKind, ResetTarget, SecurityEvent,
    SecurityEventType, SmartChargingNotification, StateLimits, StopReason, TariffStore,
    Transaction, TransactionChargingState, TransactionEventKind, TransactionEventOccurred,
    TransactionId, TransactionUpdateReason, TriggeredMonitor, Variable, VariableAttribute,
    VariableAttributeType, VariableCharacteristics, VariableDataType, VariableMonitorStore,
    VariableMonitoringEvent, VariableMutability,
};

/// The wire value OCPP's `AvailabilityState` takes for `status` - the same
/// `ConnectorStatusEnumType` spelling `StatusNotification` uses, so a CSMS reading the device
/// model and one reading the notification see the same word (G01.FR.03-.07).
fn availability_state_value(status: ConnectorStatus) -> &'static str {
    match status {
        ConnectorStatus::Available => "Available",
        ConnectorStatus::Occupied => "Occupied",
        ConnectorStatus::Reserved => "Reserved",
        ConnectorStatus::Unavailable => "Unavailable",
        ConnectorStatus::Faulted => "Faulted",
    }
}

/// One network configuration slot's worth of device-model variables, ready to register (CV1.3).
///
/// A snapshot type rather than reading the profile inline because the registration borrows the
/// device model mutably while the profiles it mirrors live on the same struct.
struct NetworkConnectionProfileSnapshot {
    csms_url: alloc::string::String,
    interface: &'static str,
    transport: &'static str,
    message_timeout_secs: i64,
    security_profile: u8,
}

impl NetworkConnectionProfileSnapshot {
    fn of(profile: &crate::state::NetworkConnectionProfile) -> Self {
        Self {
            csms_url: profile.csms_url.clone(),
            interface: match profile.interface {
                // OCPP's `OcppInterfaceEnumType` spellings. The index is part of the value, so a
                // charge point with two wired interfaces reports which one a profile uses.
                crate::state::NetworkInterface::Wired(0) => "Wired0",
                crate::state::NetworkInterface::Wired(1) => "Wired1",
                crate::state::NetworkInterface::Wired(2) => "Wired2",
                crate::state::NetworkInterface::Wired(_) => "Wired3",
                crate::state::NetworkInterface::Wireless(0) => "Wireless0",
                crate::state::NetworkInterface::Wireless(1) => "Wireless1",
                crate::state::NetworkInterface::Wireless(2) => "Wireless2",
                crate::state::NetworkInterface::Wireless(_) => "Wireless3",
                crate::state::NetworkInterface::Any => "Any",
            },
            transport: match profile.transport {
                crate::state::NetworkTransport::Json => "JSON",
                crate::state::NetworkTransport::Soap => "SOAP",
            },
            message_timeout_secs: profile.message_timeout_secs,
            security_profile: profile.security_profile,
        }
    }

    /// Registers this slot's nine required variables on `NetworkConfiguration[instance]`.
    fn register_into(&self, model: &mut DeviceModel, instance: &str) {
        let component = |()| Component {
            name: NETWORK_CONFIGURATION_COMPONENT.into(),
            instance: Some(instance.into()),
            evse: None,
        };
        let mut register = |name: &str,
                            data_type: VariableDataType,
                            value: alloc::string::String,
                            mutability: VariableMutability| {
            model.register(
                component(()),
                Variable {
                    name: name.into(),
                    instance: None,
                },
                VariableCharacteristics {
                    data_type,
                    unit: None,
                    min_limit: None,
                    max_limit: None,
                    values_list: None,
                    supports_monitoring: false,
                },
                vec![VariableAttribute {
                    attribute_type: VariableAttributeType::Actual,
                    value,
                    mutability,
                    persistent: false,
                    constant: false,
                    requires_reboot: false,
                }],
            );
        };

        use alloc::string::ToString;
        // Everything mirrored from the profile is `ReadOnly` here: `SetNetworkProfile` is how
        // OCPP changes a slot, and accepting a `SetVariables` that this crate would not apply to
        // the connection is the silent-lie failure mode B05.FR.09 forbids (CV2.1).
        register(
            "OcppCsmsUrl",
            VariableDataType::String,
            self.csms_url.clone(),
            VariableMutability::ReadOnly,
        );
        register(
            "OcppInterface",
            VariableDataType::OptionList,
            self.interface.to_string(),
            VariableMutability::ReadOnly,
        );
        register(
            "OcppTransport",
            VariableDataType::OptionList,
            self.transport.to_string(),
            VariableMutability::ReadOnly,
        );
        // The appendix's own note: "This field is ignored." This crate negotiates the version on
        // the connection rather than from the profile, and `NetworkConnectionProfile` does not
        // carry one - so the honest answer is the empty option rather than a guess.
        register(
            "OcppVersion",
            VariableDataType::OptionList,
            alloc::string::String::new(),
            VariableMutability::ReadOnly,
        );
        register(
            "MessageTimeout",
            VariableDataType::Integer,
            self.message_timeout_secs.to_string(),
            VariableMutability::ReadOnly,
        );
        register(
            "SecurityProfile",
            VariableDataType::Integer,
            self.security_profile.to_string(),
            VariableMutability::ReadOnly,
        );
        // Write-only by OCPP's own definition, and this crate keeps it that way for a second
        // reason: an `IdToken` is not the only credential worth not disclosing. `GetVariables`
        // refuses to read a `WriteOnly` attribute, so the password can never leave the charge
        // point through the device model (A01.FR.12).
        //
        // Registered rather than omitted because A01.FR.02's rotation path is addressed to this
        // exact variable, and a required key answering `UnknownVariable` is itself a failure. The
        // write is refused today - applying a new password needs the reconnect plumbing that is
        // CV10's - and refusing is what B05.FR.09 asks of a variable that cannot be honoured.
        //
        // **The refusal is not expressible here.** `WriteOnly` is not `ReadOnly`, so the
        // mutability that blocks the *read* does not block the write, and this registration is
        // re-derived from the profile on every applied event - so a `SetVariables` used to be
        // `Accepted` and then silently discarded on the next event, which is the worst of both.
        // `crate::device_model::REFUSED_WRITE_ONLY_VARIABLES` is where the write is actually
        // refused; see its docs for why an accepted-but-unapplied rotation would lock the station
        // out (A01.FR.03).
        register(
            "BasicAuthPassword",
            VariableDataType::String,
            alloc::string::String::new(),
            VariableMutability::WriteOnly,
        );
        // This crate models neither VPN nor APN configuration, so both are `false` - which is the
        // truth about a connection it set up itself, not a placeholder.
        register(
            "VpnEnabled",
            VariableDataType::Boolean,
            "false".to_string(),
            VariableMutability::ReadOnly,
        );
        register(
            "ApnEnabled",
            VariableDataType::Boolean,
            "false".to_string(),
            VariableMutability::ReadOnly,
        );
    }
}

/// How an EVSE picks one status to report for several connectors: the most restrictive wins.
///
/// The order is "how much does this stop a driver using the EVSE" rather than anything OCPP
/// states directly - the spec defines the *values*, not how to roll several connectors into one -
/// so it is written out here as a deliberate choice rather than left to enum ordering, which
/// would silently change if a variant were ever inserted.
fn availability_precedence(status: ConnectorStatus) -> u8 {
    match status {
        ConnectorStatus::Available => 0,
        ConnectorStatus::Reserved => 1,
        ConnectorStatus::Occupied => 2,
        ConnectorStatus::Unavailable => 3,
        ConnectorStatus::Faulted => 4,
    }
}

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
    /// Network connection profiles the CSMS has written into configuration slots
    /// (`SetNetworkProfile`). Stored and reported; **not** used to dial - see
    /// [`NetworkProfileStore`] and `docs/ROADMAP.md` §2.
    pub network_profiles: NetworkProfileStore,
    /// Authorization decisions the CSMS has already made, kept so a charge point that can't
    /// reach it can still answer - see [`AuthorizationCache`] and `docs/ROADMAP.md` §3.
    pub authorization_cache: AuthorizationCache,
    /// Every charging profile the CSMS has installed, across every scope - the Smart Charging
    /// functional block's state. See [`ChargingProfileStore`] and `docs/ROADMAP.md` §11.
    pub charging_profiles: ChargingProfileStore,
    /// Every default tariff the CSMS has installed, across every scope (OCPP 2.1
    /// `SetDefaultTariff`) - the Tariff and Cost functional block's tariff-assignment state. A
    /// tariff assigned to one running transaction (`ChangeTransactionTariff`) lives on
    /// [`EvseState::transaction_tariffs`] instead, not here - see [`TariffStore`] and
    /// `docs/ROADMAP.md` §9.
    pub tariffs: TariffStore,
    /// This charge point's best current estimate of the CSMS's clock, established by
    /// BootNotification/Heartbeat's `currentTime` - see [`TimeSyncAnchor`] and
    /// [`ChargePointEvent::TimeSynced`]. `None` until the first exchange that carried a
    /// parseable `currentTime`.
    pub time_sync: Option<TimeSyncAnchor>,
    /// Every variable monitor installed on the charge point (OCPP `SetVariableMonitoring`/
    /// `ClearVariableMonitoring`, reported via `NotifyEvent`) - the variable monitoring engine's
    /// state. See [`VariableMonitorStore`] and `docs/ROADMAP.md` §2/§14 (B5.2).
    pub variable_monitors: VariableMonitorStore,
    /// Messages the CSMS has asked to be shown to the driver (OCPP `SetDisplayMessage`/
    /// `ClearDisplayMessage`). See [`crate::display_message`] and `docs/ROADMAP.md` §15.
    pub display_messages: DisplayMessageStore,
    /// Periodic event streams the CSMS has opened against a variable monitor (OCPP 2.1
    /// `OpenPeriodicEventStream`/`ClosePeriodicEventStream`/`AdjustPeriodicEventStream`) - the
    /// source [`crate::periodic_event_stream::run_periodic_event_streams`] drives
    /// `NotifyPeriodicEventStream` from. See [`PeriodicEventStreamStore`] and
    /// `docs/PRODUCTION-ROADMAP.md` B5.6.
    pub periodic_event_streams: PeriodicEventStreamStore,
    /// `RequestBatterySwap` requests this charge point has accepted but not yet correlated with a
    /// reported `BatterySwap` event. See [`crate::battery_swap`] and
    /// `docs/PRODUCTION-ROADMAP.md` B8.3. **2.1 only.**
    pub battery_swaps: BatterySwapStore,
    /// Every DER control setting the CSMS has installed (OCPP 2.1 `SetDERControl`) - the DER
    /// Control functional block's state. See [`DERControlStore`] and
    /// `docs/PRODUCTION-ROADMAP.md` B8.2.
    pub der_controls: DERControlStore,
    /// The most recent automatic frequency restoration reserve signal the CSMS pushed (OCPP 2.1
    /// `AFRRSignal`). `None` until the first one arrives. See [`AfrrSignal`].
    pub afrr_signal: Option<AfrrSignal>,
    /// An external charging limit currently in force on the whole charging station (OCPP's own
    /// `evseId` absent/zero case) - mirrors [`EvseState::external_charging_limit`] at
    /// station scope. See `docs/PRODUCTION-ROADMAP.md` B2.8.
    pub station_external_charging_limit: Option<ExternalChargingLimit>,
    /// Locally generated capacity available to the whole station - mirrors
    /// [`EvseState::local_generation_limit`] at station scope, and separate from the slot above
    /// for the reason given there (K27.FR.05).
    pub station_local_generation_limit: Option<ExternalChargingLimit>,
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
        let connector_counts: Vec<usize> = connector_counts.into_iter().collect();
        Self {
            lifecycle: LifecycleState::Booting,
            registration: None,
            evses: connector_counts
                .iter()
                .copied()
                .map(EvseState::new)
                .collect(),
            next_transaction_id: 0,
            local_authorization_list: LocalAuthorizationList::with_max_entries(
                limits.max_local_authorization_list_entries,
            ),
            pending_reset: None,
            // Topology-aware: OCPP requires an `AvailabilityState`/`Available` pair on the
            // charge point, on every EVSE and on every connector (CV1.1), and those components
            // can only be named once the topology is known. See
            // `DeviceModel::register_topology_defaults`.
            device_model: DeviceModel::with_topology(
                limits.max_device_model_variables,
                &connector_counts,
            ),
            capabilities: Capabilities::default(),
            network_profiles: NetworkProfileStore::with_max_slots(limits.max_network_profile_slots),
            authorization_cache: AuthorizationCache::with_max_entries(
                limits.max_authorization_cache_entries,
            ),
            charging_profiles: ChargingProfileStore::with_limit(limits.max_charging_profiles),
            tariffs: TariffStore::with_limit(limits.max_tariffs),
            time_sync: None,
            variable_monitors: VariableMonitorStore::with_limit(limits.max_variable_monitors),
            display_messages: DisplayMessageStore::with_max_messages(limits.max_display_messages),
            periodic_event_streams: PeriodicEventStreamStore::with_limit(
                limits.max_periodic_event_streams,
            ),
            battery_swaps: BatterySwapStore::with_max_pending(limits.max_pending_battery_swaps),
            der_controls: DERControlStore::with_limit(limits.max_der_controls),
            afrr_signal: None,
            station_external_charging_limit: None,
            station_local_generation_limit: None,
        }
    }

    /// Whether this charge point may send a CSMS-bound request other than `BootNotification`
    /// right now (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV4).
    ///
    /// **B01.FR.08 is the requirement, and it is stricter than it first reads**: between power-on
    /// and a BootNotification the CSMS answered `Accepted`, the charge point sends nothing else -
    /// *"This includes cached OCPP messages that are still present in the Charging Station from
    /// prior sessions."* So a station that reboots with a full offline queue and is answered
    /// `Pending` must sit on that backlog rather than flush it, which is exactly the case this
    /// predicate exists to stop. B02.FR.02 says the same for `Pending` and B03.FR.02 for
    /// `Rejected`.
    ///
    /// `None` (no BootNotification answered yet, e.g. still booting or the very first attempt is
    /// in flight) is `false` for the same reason `Pending` is: nothing has authorised traffic.
    ///
    /// # What this deliberately does not cover
    ///
    /// B02.FR.02 exempts messages the CSMS itself asked for while `Pending` - a response to
    /// `TriggerMessage`, `GetBaseReport` or `GetReport`. Those are *responses to an inbound
    /// request*, so they never travel through the paths this predicate guards (the offline queues,
    /// which carry charge-point-initiated reports). A caller that does need to send a triggered
    /// request while `Pending` should send it directly rather than consulting this.
    pub fn may_send_requests(&self) -> bool {
        self.registration == Some(RegistrationStatus::Accepted)
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
            ChargePointEvent::NetworkProfileSet { slot, profile } => {
                let stored = self.network_profiles.set(slot, *profile);
                if stored {
                    self.refresh_network_configuration_priority();
                }
                stored
            }
            ChargePointEvent::PersistedNetworkProfilesRestored { slots } => {
                let dropped = self.network_profiles.replace(slots);
                self.refresh_network_configuration_priority();
                if dropped > 0 {
                    tracing::warn!(
                        dropped,
                        max_slots = self.network_profiles.max_slots(),
                        "truncated the recovered network profiles to the configured maximum"
                    );
                }
                !self.network_profiles.is_empty()
            }
            ChargePointEvent::AuthorizationCached {
                id_token,
                status,
                cached_at,
            } => self
                .authorization_cache
                .remember(id_token, status, cached_at),
            ChargePointEvent::AuthorizationCacheCleared => self.authorization_cache.clear() > 0,
            ChargePointEvent::CustomerInformationErased { id_token } => {
                let cache_changed = self.authorization_cache.forget(&id_token);
                let list_changed = self.local_authorization_list.forget(&id_token);
                cache_changed || list_changed
            }
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
            ChargePointEvent::DynamicScheduleUpdated {
                profile_id,
                limit,
                updated_at,
            } => {
                let applied = self
                    .charging_profiles
                    .apply_dynamic_update(profile_id, limit, updated_at);
                if !applied {
                    // Reached only if a caller dispatched this without asking the store first
                    // (`crate::smart_charging::handle_update_dynamic_schedule` does ask).
                    tracing::warn!(
                        profile_id = profile_id.0,
                        "ignoring a dynamic schedule update for a profile that is absent or not \
                         Dynamic"
                    );
                }
                applied
            }
            ChargePointEvent::PriorityChargingSet {
                transaction_id,
                activated,
                locally_initiated,
            } => {
                let granted = self
                    .evses
                    .iter_mut()
                    .flat_map(|evse| evse.transactions.iter_mut())
                    .flatten()
                    .find(|transaction| transaction.id == transaction_id)
                    .map(|transaction| {
                        set_if_changed(&mut transaction.priority_charging, activated)
                    });
                match granted {
                    Some(changed) => {
                        if changed && locally_initiated {
                            effects.push(ChargePointEffect::PriorityChargingChanged(
                                crate::state::PriorityChargingChange {
                                    transaction_id,
                                    activated,
                                },
                            ));
                        }
                        changed
                    }
                    None => {
                        // The named transaction ended between the request arriving and this being
                        // applied. Dropped rather than redirected - see the event's own docs.
                        tracing::warn!(
                            transaction_id = transaction_id.0,
                            "ignoring a priority-charging change for a transaction that is no \
                             longer running"
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
            ChargePointEvent::DefaultTariffSet { scope, tariff } => {
                let id = tariff.id.clone();
                match self.tariffs.set_default(scope, *tariff) {
                    Ok(()) => true,
                    Err(rejection) => {
                        // Reached only if a caller dispatched this without asking the store first
                        // (`crate::tariff::handle_set_default_tariff` does ask, so the CSMS never
                        // sees an optimistic Accepted); logged rather than panicking, per
                        // `apply`'s documented tolerance for events that don't apply.
                        tracing::warn!(
                            ?id,
                            ?rejection,
                            "a default tariff was refused by the store"
                        );
                        false
                    }
                }
            }
            ChargePointEvent::TariffsCleared { criteria } => self.tariffs.clear(&criteria) > 0,
            ChargePointEvent::PeriodicEventStreamOpened {
                id,
                variable_monitoring_id,
                params,
            } => {
                match self
                    .periodic_event_streams
                    .open(crate::state::OpenPeriodicEventStream {
                        id,
                        variable_monitoring_id,
                        params,
                    }) {
                    Ok(()) => true,
                    Err(rejection) => {
                        // Reached only if a caller dispatched this without asking the store first
                        // (`crate::periodic_event_stream::handle_open_periodic_event_stream` does
                        // ask, so the CSMS never sees an optimistic Accepted); logged rather than
                        // panicking, per `apply`'s documented tolerance for events that don't
                        // apply.
                        tracing::warn!(
                            id = id.0,
                            ?rejection,
                            "a periodic event stream was refused by the store"
                        );
                        false
                    }
                }
            }
            ChargePointEvent::DERControlSet(control) => {
                let id = control.id.clone();
                match self.der_controls.install(*control) {
                    Ok(()) => true,
                    Err(rejection) => {
                        // Reached only if a caller dispatched this without asking the store first
                        // (`crate::der_control::handle_set_der_control` does ask, so the CSMS
                        // never sees an optimistic Accepted); logged rather than panicking, per
                        // `apply`'s documented tolerance for events that don't apply.
                        tracing::warn!(?id, ?rejection, "a DER control was refused by the store");
                        false
                    }
                }
            }
            ChargePointEvent::PeriodicEventStreamClosed { id } => {
                self.periodic_event_streams.close(id)
            }
            ChargePointEvent::PeriodicEventStreamAdjusted { id, params } => {
                self.periodic_event_streams.adjust(id, params)
            }
            ChargePointEvent::DERControlsCleared { query } => {
                !self.der_controls.clear(&query).is_empty()
            }
            ChargePointEvent::AfrrSignalReceived { signal, timestamp } => {
                let new_signal = AfrrSignal { signal, timestamp };
                set_if_changed(&mut self.afrr_signal, Some(new_signal))
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
                } => {
                    // Only the `Actual` attribute is what a monitor watches (OCPP monitors are
                    // defined in terms of a variable's actual value - `Target`/`MinSet`/`MaxSet`
                    // are setpoint bookkeeping, not a reading). The old value is read *before*
                    // mutating, since `evaluate` needs the transition, not just the new value -
                    // see `crate::state::VariableMonitorStore::evaluate`'s docs.
                    let old_value = (attribute_type == VariableAttributeType::Actual)
                        .then(|| self.device_model.get(&component, &variable))
                        .flatten()
                        .and_then(|definition| definition.attribute(VariableAttributeType::Actual))
                        .and_then(|attribute| attribute.value.parse::<f64>().ok());
                    let applied = self.device_model.set_attribute_value(
                        &component,
                        &variable,
                        attribute_type,
                        value.clone(),
                    );
                    if applied
                        && attribute_type == VariableAttributeType::Actual
                        && let Ok(new_value) = value.parse::<f64>()
                    {
                        for monitor_id in self
                            .variable_monitors
                            .evaluate(&component, &variable, old_value, new_value)
                        {
                            if let Some(monitor) = self.variable_monitors.get(monitor_id) {
                                let trigger = match monitor.monitor_type {
                                    crate::state::MonitorType::UpperThreshold
                                    | crate::state::MonitorType::LowerThreshold => {
                                        EventTrigger::Alerting
                                    }
                                    crate::state::MonitorType::Delta => EventTrigger::Delta,
                                    // `evaluate` never triggers a `Periodic` monitor - see its
                                    // own docs - so this arm is unreachable in practice; `Alerting`
                                    // is as good a fallback as any other if that ever changes.
                                    crate::state::MonitorType::Periodic => EventTrigger::Alerting,
                                };
                                effects.push(ChargePointEffect::VariableMonitorTriggered(
                                    TriggeredMonitor {
                                        monitor_id: Some(monitor_id),
                                        component: component.clone(),
                                        variable: variable.clone(),
                                        actual_value: value.clone(),
                                        severity: monitor.severity,
                                        trigger,
                                    },
                                ));
                            }
                        }
                    }
                    applied
                }
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
                            // Set later by whatever holds it - see the field's own docs (CV6.1).
                            offline: false,
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
            // CV1.5: the integrator's electrical declaration, projected onto the required
            // `SupplyPhases`/`ConnectorType`/`Power` variables the topology already registered
            // components for.
            ChargePointEvent::ElectricalCharacteristicsDeclared(electrical) => {
                self.register_electrical_variables(&electrical);
                true
            }
            ChargePointEvent::DisplayMessageSet(message) => self.display_messages.set(*message),
            ChargePointEvent::DisplayMessageCleared(id) => self.display_messages.clear(id),
            ChargePointEvent::BatterySwapRequested(pending) => self.battery_swaps.insert(pending),
            ChargePointEvent::BatterySwapCancelled(request_id) => {
                self.battery_swaps.remove(request_id).is_some()
            }
            ChargePointEvent::BatterySwapReported(event) => {
                // A driver-initiated swap the CSMS never asked for correlates with nothing
                // pending, and that's a normal, spec-valid case - not every `BatterySwap` follows
                // a `RequestBatterySwap`. Either way the event itself is always reported.
                let removed = self.battery_swaps.remove(event.request_id).is_some();
                effects.push(ChargePointEffect::BatterySwapEventOccurred(event));
                removed
            }
            ChargePointEvent::TimeSynced {
                csms_time,
                recorded_at,
            } => {
                // CV1.2: `ClockCtrlr.DateTime` is a required variable, and this is the only
                // moment this crate learns what time it is - the CSMS's `currentTime` on a
                // BootNotification or Heartbeat response (G3.2; `ClockCtrlr.TimeSource` says
                // `Heartbeat` for exactly this reason). Refreshed here rather than recomputed on
                // every applied event because `apply` has no `MonotonicClock` to advance the
                // anchor with, so "the CSMS time as of the last sync" is the honest value and
                // the only one available.
                self.device_model.set_attribute_value(
                    &Component {
                        name: CLOCK_COMPONENT.into(),
                        instance: None,
                        evse: None,
                    },
                    &Variable {
                        name: CLOCK_DATE_TIME_VARIABLE.into(),
                        instance: None,
                    },
                    VariableAttributeType::Actual,
                    csms_time.to_rfc3339(),
                );
                set_if_changed(
                    &mut self.time_sync,
                    Some(TimeSyncAnchor {
                        csms_time,
                        recorded_at,
                    }),
                )
            }
            ChargePointEvent::Evse { evse_id, event } => match event {
                EvseEvent::Connector {
                    connector_id,
                    event,
                } => self.apply_connector_event(evse_id, connector_id, event, &mut effects),
                EvseEvent::FaultDetected => self.cascade_evse_fault(evse_id, true, &mut effects),
                EvseEvent::FaultCleared => self.cascade_evse_fault(evse_id, false, &mut effects),
                EvseEvent::EVChargingNeedsReported(needs) => {
                    if self.evses.get(evse_id).is_some() {
                        effects.push(ChargePointEffect::SmartChargingNotification(
                            SmartChargingNotification::EVChargingNeedsReported { evse_id, needs },
                        ));
                    } else {
                        tracing::warn!(
                            evse_id,
                            "ignoring EV charging needs reported for an EVSE that doesn't exist"
                        );
                    }
                    false
                }
                EvseEvent::EVChargingScheduleReported(report) => {
                    if self.evses.get(evse_id).is_some() {
                        effects.push(ChargePointEffect::SmartChargingNotification(
                            SmartChargingNotification::EVChargingScheduleReported {
                                evse_id,
                                report,
                            },
                        ));
                    } else {
                        tracing::warn!(
                            evse_id,
                            "ignoring an EV charging schedule reported for an EVSE that doesn't \
                             exist"
                        );
                    }
                    false
                }
                _ => self
                    .evses
                    .get_mut(evse_id)
                    .is_some_and(|evse| evse.apply(event)),
            },
            ChargePointEvent::ExternalChargingLimitSet { evse_id, limit } => {
                self.set_external_charging_limit(evse_id, limit, &mut effects)
            }
            ChargePointEvent::ExternalChargingLimitCleared {
                evse_id,
                source,
                is_local_generation,
            } => self.clear_external_charging_limit(
                evse_id,
                source,
                is_local_generation,
                &mut effects,
            ),
            ChargePointEvent::VariableMonitoring(event) => match event {
                VariableMonitoringEvent::MonitorSet {
                    id,
                    component,
                    variable,
                    monitor_type,
                    value,
                    severity,
                } => self.variable_monitors.set(
                    id,
                    component,
                    variable,
                    monitor_type,
                    value,
                    severity,
                ),
                VariableMonitoringEvent::MonitorCleared { id } => self.variable_monitors.clear(id),
                VariableMonitoringEvent::BaseSet { base } => self.variable_monitors.set_base(base),
                VariableMonitoringEvent::LevelSet { severity } => {
                    self.variable_monitors.set_level(severity)
                }
            },
        };

        // Every path above that can move a connector, an EVSE or the charge point itself lands
        // here, so the device model's `AvailabilityState` is re-derived once per applied event
        // rather than at each of the several dozen mutation sites - which is what keeps it from
        // drifting when a new event variant lands. Costs one pass over the connectors; a charge
        // point has single digits of them.
        self.sync_availability_variables();
        self.sync_network_configuration_variables();
        if changed {
            effects.insert(0, ChargePointEffect::StateChanged);
        }
        self.check_pending_reset(&mut effects);
        effects
    }

    /// Reads a device-model variable's `Actual` value, or the empty string when it is absent -
    /// which every caller here treats as "not configured".
    fn string_variable(&self, component: &str, variable: &str) -> alloc::string::String {
        self.device_model
            .get(
                &Component {
                    name: component.into(),
                    instance: None,
                    evse: None,
                },
                &Variable {
                    name: variable.into(),
                    instance: None,
                },
            )
            .and_then(|definition| definition.attribute(VariableAttributeType::Actual))
            .map(|attribute| attribute.value.clone())
            .unwrap_or_default()
    }

    /// Reads a `Boolean` device-model variable, defaulting to `default` when it is absent or
    /// unparseable - the same tolerance `crate::authorization` applies, kept here so the state
    /// machine can consult its own configuration without reaching outside `crate::state`.
    fn boolean_variable(&self, component: &str, variable: &str, default: bool) -> bool {
        self.device_model
            .get(
                &Component {
                    name: component.into(),
                    instance: None,
                    evse: None,
                },
                &Variable {
                    name: variable.into(),
                    instance: None,
                },
            )
            .and_then(|definition| definition.attribute(VariableAttributeType::Actual))
            .and_then(|attribute| match attribute.value.as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            })
            .unwrap_or(default)
    }

    /// Re-derives every `AvailabilityState` variable from the current state
    /// (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV1.1). Writes only where the value actually
    /// changed, so a charge point sitting idle does no work beyond the comparison.
    ///
    /// The three levels answer three different questions, which is why they are not the same
    /// computation:
    ///
    /// - A **connector** reports exactly what its `StatusNotification` reports - the same
    ///   [`ConnectorState::availability_status`] projection, so the two can never disagree.
    /// - An **EVSE** reports its own [`EvseStatus`] where that is decisive (`Faulted`,
    ///   `Unavailable` - both set by `ChangeAvailability` or a fault, and both meaning the EVSE is
    ///   out of service whatever its connectors say), and otherwise the busiest of its connectors.
    ///   OCPP has no "half occupied": an EVSE with a cable in one of its connectors is one a
    ///   driver cannot walk up to and use.
    /// - The **charge point** reports its [`LifecycleState`] and nothing else. Rolling EVSE state
    ///   up to the station would make a single charging connector on a 20-EVSE site report the
    ///   whole site as `Occupied`, which is not what a CSMS asking about the station means.
    fn sync_availability_variables(&mut self) {
        let station = match self.lifecycle {
            // Not yet booted is not yet available - and the `Unavailable` a `SetUnavailable`
            // produces is the same thing to a CSMS, so both map to it.
            LifecycleState::Booting | LifecycleState::Unavailable => ConnectorStatus::Unavailable,
            LifecycleState::Available => ConnectorStatus::Available,
            LifecycleState::Faulted => ConnectorStatus::Faulted,
        };
        self.set_availability_state(AVAILABILITY_COMPONENT_CHARGE_POINT, None, station);

        for evse_id in 0..self.evses.len() {
            let statuses: Vec<ConnectorStatus> = self.evses[evse_id]
                .connectors
                .iter()
                .map(|connector| connector.availability_status())
                .collect();
            for (connector_id, status) in statuses.iter().enumerate() {
                self.set_availability_state(
                    AVAILABILITY_COMPONENT_CONNECTOR,
                    Some((evse_id, Some(connector_id))),
                    *status,
                );
            }

            let evse = match self.evses[evse_id].status {
                EvseStatus::Faulted => ConnectorStatus::Faulted,
                EvseStatus::Unavailable => ConnectorStatus::Unavailable,
                EvseStatus::Available => statuses
                    .into_iter()
                    .max_by_key(|status| availability_precedence(*status))
                    .unwrap_or(ConnectorStatus::Available),
            };
            self.set_availability_state(AVAILABILITY_COMPONENT_EVSE, Some((evse_id, None)), evse);
        }
    }

    /// Registers the electrical variables OCPP requires, from the integrator's declaration
    /// (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV1.5).
    ///
    /// Registered **always**, valued only where declared: OCPP marks all three required, so a
    /// variable that is absent is a compliance failure while one that is present and empty is an
    /// honest "this firmware was not told". `EVSE.Power` is the odd one - the appendix requires
    /// its `maxLimit` *characteristic* and only desires a live value, so the declared maximum
    /// becomes `max_limit` rather than the reading.
    fn register_electrical_variables(
        &mut self,
        electrical: &crate::hardware::ElectricalCharacteristics,
    ) {
        let phases = |phases: crate::hardware::SupplyPhases| {
            alloc::string::String::from(phases.as_wire_value().unwrap_or(""))
        };
        self.register_readonly(
            AVAILABILITY_COMPONENT_CHARGE_POINT,
            None,
            "SupplyPhases",
            VariableDataType::Integer,
            phases(electrical.supply_phases),
            None,
        );
        for evse_id in 0..self.evses.len() {
            let evse = electrical.evse(evse_id);
            self.register_readonly(
                AVAILABILITY_COMPONENT_EVSE,
                Some((evse_id, None)),
                "SupplyPhases",
                VariableDataType::Integer,
                evse.map(|evse| phases(evse.supply_phases))
                    .unwrap_or_default(),
                None,
            );
            self.register_readonly(
                AVAILABILITY_COMPONENT_EVSE,
                Some((evse_id, None)),
                "Power",
                VariableDataType::Decimal,
                // No live reading: this crate learns power per connector from meter samples, not
                // per EVSE, so quoting one here would be an invention. The required part is the
                // limit below.
                alloc::string::String::new(),
                evse.and_then(|evse| evse.max_power_w),
            );
            // `Required? = V2X` in the appendix: required of a station that can discharge and
            // absent from one that cannot, so it follows the capability rather than being
            // registered unconditionally like its charge-direction sibling above. Claiming a
            // discharge figure on a charge-only station would advertise hardware that is not
            // there.
            if self.capabilities.supports_bidirectional_power {
                self.register_readonly(
                    AVAILABILITY_COMPONENT_EVSE,
                    Some((evse_id, None)),
                    "DischargePower",
                    VariableDataType::Decimal,
                    alloc::string::String::new(),
                    evse.and_then(|evse| evse.max_discharge_power_w),
                );
            }
            // `Required? = V2X` in the appendix: required of a station that can discharge and
            // absent from one that cannot, so it follows the capability rather than being
            // registered unconditionally like its charge-direction sibling above. Claiming a
            // discharge figure on a charge-only station would advertise hardware that is not
            // there.
            if self.capabilities.supports_bidirectional_power {
                self.register_readonly(
                    AVAILABILITY_COMPONENT_EVSE,
                    Some((evse_id, None)),
                    "DischargePower",
                    VariableDataType::Decimal,
                    alloc::string::String::new(),
                    evse.and_then(|evse| evse.max_discharge_power_w),
                );
            }
            let connector_count = self.evses[evse_id].connectors.len();
            for connector_id in 0..connector_count {
                let connector = electrical.connector(evse_id, connector_id);
                self.register_readonly(
                    AVAILABILITY_COMPONENT_CONNECTOR,
                    Some((evse_id, Some(connector_id))),
                    "SupplyPhases",
                    VariableDataType::Integer,
                    connector
                        .map(|connector| phases(connector.supply_phases))
                        .unwrap_or_default(),
                    None,
                );
                self.register_readonly(
                    AVAILABILITY_COMPONENT_CONNECTOR,
                    Some((evse_id, Some(connector_id))),
                    "ConnectorType",
                    VariableDataType::String,
                    connector
                        .map(|connector| connector.connector_type.clone())
                        .unwrap_or_default(),
                    None,
                );
            }
        }
    }

    /// Registers one read-only, hardware-declared variable. `max_limit` carries OCPP's required
    /// `maxLimit` characteristic where the spec asks for one.
    fn register_readonly(
        &mut self,
        component_name: &str,
        evse: Option<(usize, Option<usize>)>,
        variable: &str,
        data_type: VariableDataType,
        value: alloc::string::String,
        max_limit: Option<f64>,
    ) {
        self.device_model.register(
            Component {
                name: component_name.into(),
                instance: None,
                evse,
            },
            Variable {
                name: variable.into(),
                instance: None,
            },
            VariableCharacteristics {
                data_type,
                unit: None,
                min_limit: None,
                max_limit,
                values_list: None,
                supports_monitoring: false,
            },
            vec![VariableAttribute {
                attribute_type: VariableAttributeType::Actual,
                value,
                // A fact about the hardware, not a setting: `SetVariables` must refuse it
                // (B05.FR.09), and `constant` says it never changes at runtime either.
                mutability: VariableMutability::ReadOnly,
                persistent: false,
                constant: true,
                requires_reboot: false,
            }],
        );
    }

    /// Mirrors every occupied network configuration slot into the device model as a
    /// `NetworkConfiguration` component (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV1.3).
    ///
    /// OCPP addresses these per slot - *"Writing to this variable only sets the password for the
    /// instance configurationSlot of Component NetworkConfiguration"* - so the slot number is the
    /// **component instance**, and a charge point with two slots reports two independent sets.
    /// All nine of the appendix's required variables are registered; the optional VPN/APN detail
    /// is not, because this crate models neither.
    ///
    /// Re-derived per applied event, like the availability variables, and for the same reason: a
    /// slot can be written, replaced or vacated by `SetNetworkProfile`, by a persisted-profile
    /// restore, or by the network-switch logic, and one sync point cannot drift where several
    /// mutation sites would.
    fn sync_network_configuration_variables(&mut self) {
        let wanted: Vec<(String, NetworkConnectionProfileSnapshot)> = self
            .network_profiles
            .slots()
            .iter()
            .map(|slot| {
                (
                    alloc::format!("{}", slot.slot),
                    NetworkConnectionProfileSnapshot::of(&slot.profile),
                )
            })
            .collect();

        // Vacated slots take their whole component with them - a reported CSMS URL the charge
        // point no longer holds reads as current, which is worse than not reporting one.
        let stale: Vec<Component> = self
            .device_model
            .components()
            .filter(|component| component.name == NETWORK_CONFIGURATION_COMPONENT)
            .filter(|component| {
                !wanted
                    .iter()
                    .any(|(instance, _)| component.instance.as_deref() == Some(instance.as_str()))
            })
            .cloned()
            .collect();
        for component in stale {
            self.device_model.remove_component(&component);
        }

        for (instance, snapshot) in wanted {
            snapshot.register_into(&mut self.device_model, &instance);
        }
    }

    /// Writes one component's `AvailabilityState`, if it moved. A component that isn't registered
    /// (a binding that removed it, or a model bounded too low to hold it) is a no-op, matching
    /// [`DeviceModel::set_attribute_value`]'s own tolerance.
    fn set_availability_state(
        &mut self,
        component_name: &str,
        evse: Option<(usize, Option<usize>)>,
        status: ConnectorStatus,
    ) {
        let component = Component {
            name: component_name.into(),
            instance: None,
            evse,
        };
        let variable = Variable {
            name: AVAILABILITY_STATE_VARIABLE.into(),
            instance: None,
        };
        let value = availability_state_value(status);
        let unchanged = self
            .device_model
            .get(&component, &variable)
            .and_then(|definition| definition.attribute(VariableAttributeType::Actual))
            .is_some_and(|attribute| attribute.value == value);
        if unchanged {
            return;
        }
        self.device_model.set_attribute_value(
            &component,
            &variable,
            VariableAttributeType::Actual,
            value.into(),
        );
    }

    /// Writes one connector's `ConnectorPlugRetentionLock`/`Problem`, reporting whether it moved
    /// (G05, CV11). Same tolerance for an unregistered component as
    /// [`Self::set_availability_state`].
    fn set_plug_retention_lock_problem(
        &mut self,
        evse_id: usize,
        connector_id: usize,
        problem: bool,
    ) -> bool {
        let component = Component {
            name: PLUG_RETENTION_LOCK_COMPONENT.into(),
            instance: None,
            evse: Some((evse_id, Some(connector_id))),
        };
        let variable = Variable {
            name: PROBLEM_VARIABLE.into(),
            instance: None,
        };
        let value = if problem { "true" } else { "false" };
        let current = self
            .device_model
            .get(&component, &variable)
            .and_then(|definition| definition.attribute(VariableAttributeType::Actual))
            .map(|attribute| attribute.value.as_str());
        if current.is_none() || current == Some(value) {
            return false;
        }
        self.device_model.set_attribute_value(
            &component,
            &variable,
            VariableAttributeType::Actual,
            value.into(),
        );
        true
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
        // CV2.4: read once, before the mutable borrow, so the transition stays a pure function
        // of (state, event, policy). See `ConnectorPolicy`.
        let policy = ConnectorPolicy {
            stop_tx_on_ev_side_disconnect: self.boolean_variable(
                "TxCtrlr",
                "StopTxOnEVSideDisconnect",
                ConnectorPolicy::default().stop_tx_on_ev_side_disconnect,
            ),
            tx_start_point: TxStartPoint::from_member_list(
                &self.string_variable("TxCtrlr", "TxStartPoint"),
            ),
            tx_stop_point: TxStopPoint::from_member_list(
                &self.string_variable("TxCtrlr", "TxStopPoint"),
            ),
            unlock_on_ev_side_disconnect: self.boolean_variable(
                "OCPPCommCtrlr",
                "UnlockOnEVSideDisconnect",
                ConnectorPolicy::default().unlock_on_ev_side_disconnect,
            ),
            stop_tx_on_invalid_id: self.boolean_variable(
                "TxCtrlr",
                "StopTxOnInvalidId",
                ConnectorPolicy::default().stop_tx_on_invalid_id,
            ),
            max_energy_on_invalid_id_wh: self
                .string_variable("TxCtrlr", "MaxEnergyOnInvalidId")
                .parse::<i64>()
                .ok(),
            authorize_remote_start: self.boolean_variable(
                "AuthCtrlr",
                "AuthorizeRemoteStart",
                ConnectorPolicy::default().authorize_remote_start,
            ),
        };
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
        // Both presentations authorize an identifier; only the Plug & Charge one carries
        // certificate material with it (see `ConnectorEvent::ContractCertificatePresented`).
        let presented_id_token = match &event {
            ConnectorEvent::IdTokenPresented(id_token) => Some((id_token.clone(), None)),
            ConnectorEvent::ContractCertificatePresented {
                id_token,
                certificate,
            } => Some((id_token.clone(), Some(certificate.clone()))),
            _ => None,
        };
        let authorized_id_token = match &event {
            ConnectorEvent::ChargingAuthorized(id_token)
            | ConnectorEvent::RemoteStartRequested { id_token, .. } => Some(id_token.clone()),
            _ => None,
        };
        // Narrower than `authorized_id_token`: only a locally presented identifier the CSMS
        // accepted, which is the one E03 can hold against a connector with no cable (CV2.3).
        let event_authorized_id_token = match &event {
            ConnectorEvent::ChargingAuthorized(id_token) => Some(id_token.clone()),
            _ => None,
        };
        // CV6: the `remoteStartId` a CSMS-initiated start carried, recorded on the transaction
        // it begins so every one of that transaction's events can quote it back (F01.FR.25).
        let event_remote_start_id = match &event {
            ConnectorEvent::RemoteStartRequested {
                remote_start_id, ..
            } => *remote_start_id,
            _ => None,
        };
        // CV6: the reservation this connector is consuming, if any. Read before the transition,
        // because entering `Starting` is what ends the reservation - afterwards there is nothing
        // left to read (F02.FR.06).
        let active_reservation_id = evse
            .honoured_reservations
            .get(connector_id)
            .copied()
            .flatten();
        // Kept because `apply` consumes the event but two later blocks still need to know which
        // one it was.
        let event_kind = match &event {
            ConnectorEvent::AuthorizationRevoked => EventKind::AuthorizationRevoked,
            ConnectorEvent::MeterValueSampled(_) => EventKind::MeterValueSampled,
            ConnectorEvent::LockFailed => EventKind::LockFailed,
            ConnectorEvent::TransactionLimitSet { limit, from_csms } => {
                EventKind::TransactionLimitSet {
                    limit: *limit,
                    from_csms: *from_csms,
                }
            }
            _ => EventKind::Other,
        };
        // CV7: `Some(Some(..))` records a pending start, `Some(None)` clears one, `None` means
        // this event says nothing about it - captured before `apply` consumes the event.
        let pending_remote_start_change = match &event {
            ConnectorEvent::RemoteStartPending(pending) => Some(Some(pending.clone())),
            ConnectorEvent::RemoteStartPendingCleared => Some(None),
            _ => None,
        };
        // CV7/F01.FR.01: the request as it would be held, if the policy routes it through
        // authorization rather than starting it outright. Which of the two happened is only
        // knowable after `apply`, so this is captured now and consumed below.
        let remote_start_request = match &event {
            ConnectorEvent::RemoteStartRequested {
                id_token,
                remote_start_id,
            } => Some(crate::state::PendingRemoteStart {
                id_token: id_token.clone(),
                remote_start_id: *remote_start_id,
            }),
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
        let tariff_update = match &event {
            ConnectorEvent::TariffAssigned(tariff) => Some((**tariff).clone()),
            _ => None,
        };
        let running_cost_update = match &event {
            ConnectorEvent::RunningCostAdvanced { cost, total } => Some(((**cost).clone(), *total)),
            _ => None,
        };
        let computed_limit = match &event {
            ConnectorEvent::CurrentLimitComputed {
                limit_ma,
                externally_caused,
            } => Some((*limit_ma, *externally_caused)),
            _ => None,
        };
        let confirmed_limit = match &event {
            ConnectorEvent::CurrentLimitConfirmed(limit_ma) => Some(*limit_ma),
            _ => None,
        };
        // Captured before `apply` consumes the event; only these two are distinguishable here,
        // and only they change what the CSMS is told.
        let was_cancelled = matches!(event, ConnectorEvent::ReservationCancelled);
        let was_expired = matches!(event, ConnectorEvent::ReservationExpired);
        let transition = connector.apply(event, policy);
        let new_state = *connector;
        let mut reservation_ended = None;
        let mut honoured = None;
        if let Some(slot) = evse.reservations.get_mut(connector_id) {
            // Only an event that actually *carries* a reservation records one. Assigning
            // `reservation_made` unconditionally here would clear the record on every other event
            // that finds the connector still `Reserved` - a meter sample from idle hardware, a
            // held remote start (CV7) - leaving the connector reserved with nothing saying for
            // whom, which reads downstream as an unreserved bay. Entering `Reserved` at all is
            // only possible via `ConnectorEvent::Reserved`, so nothing needs the clearing case.
            if new_state == ConnectorState::Reserved {
                if let Some(reservation) = reservation_made {
                    *slot = Some(reservation);
                }
            } else if previous_state == ConnectorState::Reserved {
                // Why it ended decides whether the CSMS hears about it. A cancellation it sent
                // needs no report, and a cable arriving is the reservation being *honoured* -
                // reporting either as an end would tell the CSMS its reservation failed when it
                // did exactly what it was for. What is left is expiry, and the charge point
                // giving up on a connector it can no longer hold.
                reservation_ended =
                    slot.as_ref()
                        .map(|reservation| reservation.id)
                        .and_then(|id| {
                            let reason = match (was_cancelled, was_expired, new_state) {
                                (true, _, _) => return None,
                                (_, true, _) => ReservationEndReason::Expired,
                                // The cable arriving is the reservation being honoured. Nothing
                                // to report, but the id has to outlive the reservation itself so
                                // the transaction this leads to can quote it (CV6) - see
                                // `EvseState::honoured_reservations`.
                                (_, _, ConnectorState::Connected) => {
                                    honoured = Some(id.0);
                                    return None;
                                }
                                _ => ReservationEndReason::Removed,
                            };
                            Some(ReservationUpdate { id, reason })
                        });
                *slot = None;
            }
        }
        if let Some(slot) = evse.honoured_reservations.get_mut(connector_id) {
            if honoured.is_some() {
                *slot = honoured;
            } else if new_state == ConnectorState::Available {
                // Back to idle: whatever this connector honoured belonged to the session that has
                // now ended, and must not leak into whoever plugs in next - the same rule
                // `running_costs` and `transaction_tariffs` follow.
                *slot = None;
            }
        }
        if let Some(update) = reservation_ended {
            effects.push(ChargePointEffect::ReservationEnded(update));
        }
        // E05 (CV2.5): revoked, but the operator granted a last allowance rather than cutting
        // energy at once - so the transaction is told the meter reading it must stop at. The
        // connector keeps charging until `MeterValueSampled` reaches it, below.
        if matches!(event_kind, EventKind::AuthorizationRevoked)
            && !policy.stop_tx_on_invalid_id
            && let Some(allowance) = policy.max_energy_on_invalid_id_wh.filter(|wh| *wh > 0)
            && let Some(Some(transaction)) = evse.transactions.get_mut(connector_id)
            && transaction.stop_at_energy_wh.is_none()
        {
            let from = transaction
                .last_meter_sample
                .map(|sample| sample.energy_wh)
                .unwrap_or_default();
            transaction.stop_at_energy_wh = Some(from + allowance);
            tracing::info!(
                evse_id,
                connector_id,
                allowance_wh = allowance,
                "the identifier was revoked; granting the configured last allowance"
            );
        }
        // E16 (CV15): a ceiling arriving for this connector's transaction. Filtered to what this
        // build enforces (FR.13) and clamped to whatever the CSMS allows (FR.04) before it is
        // recorded, so both rules hold for the value that is confirmed *and* the value that is
        // enforced - there is only ever one of it.
        let mut limit_set = false;
        if let EventKind::TransactionLimitSet { limit, from_csms } = event_kind
            && let Some(Some(transaction)) = evse.transactions.get_mut(connector_id)
        {
            let supported = limit.supported();
            if supported.is_empty() {
                // Everything the setter asked for is a limit kind this build does not enforce.
                // Recording nothing and confirming nothing is exactly FR.13; saying so at `warn`
                // because an operator who set one deserves to know it did not take.
                tracing::warn!(
                    evse_id,
                    connector_id,
                    from_csms,
                    "a transaction limit named no limit this build supports; ignoring it"
                );
            } else {
                if from_csms {
                    transaction.csms_limit = Some(supported);
                }
                // The CSMS's own limit is the ceiling, not a value to clamp against itself: it is
                // free to raise what it previously set (which is what E16.FR.14's "increased by
                // CSMS" case is), while a locally-set one never rises above it.
                transaction.limit = Some(match (from_csms, transaction.csms_limit) {
                    (false, Some(ceiling)) => supported.clamped_to(&ceiling),
                    _ => supported,
                });
                transaction.seq_no += 1;
                limit_set = true;
                tracing::info!(
                    evse_id,
                    connector_id,
                    from_csms,
                    "a transaction limit was set"
                );
            }
        }
        // CV7/F02: the pending remote start's lifecycle. Recorded on request, dispatched the
        // moment the cable latches, cleared when the connector goes idle without ever being used
        // (a timeout sweep, a fault clearing) so it cannot fire for whoever plugs in next.
        let mut dispatch_pending = None;
        // Recording or releasing a held request is a real state change even though no connector
        // transition accompanies it - `RemoteStartPending` arrives on an idle connector and moves
        // nothing. Without saying so, `apply` reports `changed: false` and the actor never
        // publishes the new state, so nothing downstream (the CSMS-facing snapshot, persistence,
        // the timeout sweep) can see that a request is being held at all.
        let mut pending_changed = false;
        // E03/E03.FR.15 (CV2.3): the CSMS authorized a card presented on a connector with no cable
        // in it - "Start Transaction - IdToken First". Nothing about the connector can move, so
        // the authorization is *held* against it exactly as a `RequestStartTransaction` with no
        // cable is, and is dispatched by the same latch below when the driver plugs in. Holding it
        // in the same slot is what makes the two use cases share one timeout sweep, which is the
        // whole of E03.FR.15: a driver who authorizes and then walks away is deauthorized rather
        // than leaving a live authorization for whoever plugs in next.
        //
        // Only a `ChargingAuthorized` counts, never a `RemoteStartRequested`: the latter is F01's
        // "start now", and OCPP requires a latched cable for it - `RemoteStartPending` is how a
        // CSMS asks for the held form.
        let local_hold = match (previous_state, &event_authorized_id_token) {
            (ConnectorState::Available, Some(id_token)) => Some(id_token.clone()),
            _ => None,
        }
        .map(|id_token| crate::state::PendingRemoteStart {
            id_token,
            // Locally presented, so there is no `remoteStartId` - which is exactly what makes
            // the transaction this eventually starts report `triggerReason = Authorized`
            // rather than `RemoteStart` (see `trigger_reason_for`).
            remote_start_id: None,
        });
        // F01.FR.01 (CV7): a remote start routed through authorization instead of started
        // outright. The `remoteStartId` has to outlive the event that carried it, because the
        // transaction is not created until the decision comes back - the same gap
        // `honoured_reservations` bridges for a reservation id, and the same slot E03 already
        // holds a pre-cable authorization in.
        let authorizing_hold = match new_state {
            ConnectorState::Authorizing => remote_start_request,
            _ => None,
        };
        // At most one of the two can be set: `local_hold` needs a `ChargingAuthorized`, and
        // `authorizing_hold` a `RemoteStartRequested`.
        let hold = local_hold.or(authorizing_hold);
        // Read *before* the block below, because the `Starting` arm consumes the very hold this
        // has to survive: the transaction the authorization starts is created further down, and
        // F01.FR.25 says it quotes the `remoteStartId` the request carried. F02's and E03's holds
        // are taken by the dispatch arm instead, and E03's carries no id anyway.
        let held_remote_start_id = evse
            .pending_remote_starts
            .get(connector_id)
            .and_then(|slot| slot.as_ref())
            .and_then(|pending| pending.remote_start_id);
        if let Some(slot) = evse.pending_remote_starts.get_mut(connector_id) {
            match (&pending_remote_start_change, &hold, new_state) {
                (Some(Some(pending)), _, _) => {
                    pending_changed = slot.as_ref() != Some(pending);
                    *slot = Some(pending.clone());
                }
                (Some(None), _, _) => {
                    pending_changed = slot.take().is_some();
                }
                (None, Some(hold), _) => {
                    pending_changed = slot.as_ref() != Some(hold);
                    *slot = Some(hold.clone());
                }
                // F02/E03: the cable latched, so whatever was held is dispatched. Narrowed to
                // arrivals *from `Connected`*, which is the only way a cable can latch - the other
                // ways to reach `Locked` are a refused authorization (F01.FR.01, below) and the
                // end of a retained-cable session (E09.FR.03), and dispatching a start into either
                // would begin a transaction nobody asked for.
                (None, None, ConnectorState::Locked)
                    if previous_state == ConnectorState::Connected =>
                {
                    dispatch_pending = slot.take()
                }
                // F01.FR.01: back at `Locked` without a transaction means the authorization was
                // refused. The held `remoteStartId` dies with it - otherwise it would attach
                // itself to whatever the next driver starts on this connector.
                (None, None, ConnectorState::Locked)
                    if previous_state != ConnectorState::Locked =>
                {
                    pending_changed = slot.take().is_some();
                }
                // F01.FR.01: the authorization came back positive and the session is starting, so
                // the hold has done its job - `held_remote_start_id` above already carries the id
                // into the transaction. Leaving it would make the connector look, to
                // `remote_control::run_pending_remote_start_timeouts`, like a bay still waiting for
                // a driver who is in fact charging on it.
                (None, None, ConnectorState::Starting)
                    if previous_state == ConnectorState::Authorizing =>
                {
                    pending_changed = slot.take().is_some();
                }
                // Only when the connector *arrives* at idle, not on every event that finds it
                // there. A held start is recorded while the connector is already `Available` and
                // sits there until the cable comes - so clearing on the state alone would let the
                // next meter sample from that connector's hardware silently drop it.
                (None, None, ConnectorState::Available)
                    if previous_state != ConnectorState::Available =>
                {
                    pending_changed = slot.take().is_some();
                }
                _ => {}
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
        // E03 (CV2.3): `Available` is here because a card presented before the cable arrives has
        // nowhere for the connector to move to - it is still `Available` afterwards - yet the CSMS
        // must still be asked, because that answer is what the connector holds until the driver
        // plugs in (see `local_hold` above).
        if matches!(
            new_state,
            ConnectorState::Authorizing | ConnectorState::Available
        ) && let Some((id_token, contract)) = presented_id_token
        {
            effects.push(ChargePointEffect::AuthorizationRequested(
                AuthorizationRequested {
                    evse_id,
                    connector_id,
                    id_token,
                    contract,
                },
            ));
        }
        // F01.FR.01 (CV7): the same request a presented card raises, for a remote start the
        // operator asked to have authorized. Never carries certificate material - a
        // `RequestStartTransaction` conveys an identifier, not a contract; Plug & Charge arrives
        // through `ContractCertificatePresented` instead.
        if let Some(hold) = evse
            .pending_remote_starts
            .get(connector_id)
            .and_then(|slot| slot.as_ref())
            .filter(|_| new_state == ConnectorState::Authorizing)
        {
            effects.push(ChargePointEffect::AuthorizationRequested(
                AuthorizationRequested {
                    evse_id,
                    connector_id,
                    id_token: hold.id_token.clone(),
                    contract: None,
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
                TransactionOrigin {
                    id_token: authorized_id_token,
                    // F01.FR.25: the event's own id when the start was immediate (F01.FR.02), and
                    // the one held across the authorization round trip when it was not
                    // (F01.FR.01) - either way the transaction quotes the request that caused it.
                    remote_start_id: event_remote_start_id.or(held_remote_start_id),
                    reservation_id: active_reservation_id,
                },
                TransactionPoints {
                    tx_start_point: policy.tx_start_point,
                    tx_stop_point: policy.tx_stop_point,
                },
            ) {
                // A new transaction must not inherit a previous one's running cost or driver
                // tariff, and an ended transaction's cost/tariff is no longer meaningful.
                if matches!(
                    kind,
                    TransactionEventKind::Started | TransactionEventKind::Ended
                ) {
                    if let Some(cost_slot) = evse.running_costs.get_mut(connector_id) {
                        *cost_slot = None;
                    }
                    if let Some(tariff_slot) = evse.transaction_tariffs.get_mut(connector_id) {
                        *tariff_slot = None;
                    }
                    if let Some(running_cost_slot) = evse.running_cost.get_mut(connector_id) {
                        *running_cost_slot = None;
                    }
                }
                effects.push(ChargePointEffect::TransactionEvent(
                    TransactionEventOccurred {
                        evse_id,
                        connector_id,
                        kind,
                        transaction,
                        offline: false,
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
                        offline: false,
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
        let limit_changed = computed_limit.is_some_and(|(limit_ma, _)| {
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
        // K11.FR.04/K13.FR.03 (CV18). All three of the requirements' preconditions, in the one
        // place that can see all three: an external system caused it, the rate genuinely moved
        // (`limit_changed` is "by more than `LimitChangeSignificance`", which this build registers
        // as 0), and a transaction is ongoing to report it against.
        if limit_changed
            && computed_limit.is_some_and(|(_, externally_caused)| externally_caused)
            && let Some(Some(transaction)) = evse.transactions.get_mut(connector_id)
        {
            transaction.seq_no += 1;
            effects.push(ChargePointEffect::TransactionEvent(
                TransactionEventOccurred {
                    evse_id,
                    connector_id,
                    kind: TransactionEventKind::Updated(
                        TransactionUpdateReason::ChargingRateChanged,
                    ),
                    transaction: transaction.clone(),
                    offline: false,
                },
            ));
        }
        // E16.FR.01/.03 (CV15): the confirmation the setter is owed, sent once, carrying the
        // ceiling now in force. Emitted here rather than where the limit was recorded so it lands
        // after the connector's own transition effects, in the order the CSMS will read them.
        if limit_set && let Some(Some(transaction)) = evse.transactions.get_mut(connector_id) {
            effects.push(ChargePointEffect::TransactionEvent(
                TransactionEventOccurred {
                    evse_id,
                    connector_id,
                    kind: TransactionEventKind::Updated(TransactionUpdateReason::LimitSet),
                    transaction: transaction.clone(),
                    offline: false,
                },
            ));
        }
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
        let tariff_recorded = tariff_update.is_some_and(|tariff| {
            if evse
                .transactions
                .get(connector_id)
                .is_some_and(Option::is_some)
                && let Some(tariff_slot) = evse.transaction_tariffs.get_mut(connector_id)
            {
                *tariff_slot = Some(tariff);
                return true;
            }
            false
        });
        let running_cost_recorded = running_cost_update.is_some_and(|(cost, total)| {
            if evse
                .transactions
                .get(connector_id)
                .is_some_and(Option::is_some)
                && let Some(running_cost_slot) = evse.running_cost.get_mut(connector_id)
            {
                *running_cost_slot = Some(cost);
                // Recorded beside the cost itself so `enforce_transaction_limit` has a figure to
                // compare a `maxCost` against without needing the tariff (CV15).
                if let Some(total_slot) = evse.running_cost_totals.get_mut(connector_id) {
                    *total_slot = Some(total);
                }
                return true;
            }
            false
        });
        // G05 (CV11): a lock failure is reported as more than a fault. The `Problem` variable is
        // what a CSMS can *read* to tell this apart from a stuck contactor, and the hard-wired
        // `NotifyEvent` beside it is G05.FR.02 - raised here rather than left to a CSMS-configured
        // monitor because the requirement is unconditional, and because this crate's monitors only
        // evaluate numeric values.
        //
        // Cleared when the connector reaches `Available` again: that is the far side of the
        // fail-safe recovery (fault cleared, then unlocked), so the lock has demonstrably worked
        // since. Nothing is reported for the clear - a `Problem` back to `false` is visible to any
        // CSMS that asks, and a station recovering is not news.
        let lock_problem_changed = match (event_kind, new_state) {
            (EventKind::LockFailed, _) => {
                let changed = self.set_plug_retention_lock_problem(evse_id, connector_id, true);
                if changed {
                    tracing::warn!(
                        evse_id,
                        connector_id,
                        "the connector's plug retention lock failed; faulting the connector"
                    );
                    effects.push(ChargePointEffect::VariableMonitorTriggered(
                        TriggeredMonitor {
                            // Hard-wired: nobody configured this, so it carries no monitor id and
                            // is not filtered by `MonitoringLevel`.
                            monitor_id: None,
                            component: Component {
                                name: PLUG_RETENTION_LOCK_COMPONENT.into(),
                                instance: None,
                                evse: Some((evse_id, Some(connector_id))),
                            },
                            variable: Variable {
                                name: PROBLEM_VARIABLE.into(),
                                instance: None,
                            },
                            actual_value: "true".into(),
                            // OCPP's `1-Hardware Failure`: a retention lock that will not move is
                            // a hardware issue, and it leaves this connector unable to charge
                            // until someone attends to it.
                            severity: 1,
                            trigger: EventTrigger::Alerting,
                        },
                    ));
                }
                changed
            }
            (_, ConnectorState::Available) => {
                self.set_plug_retention_lock_problem(evse_id, connector_id, false)
            }
            _ => false,
        };
        let changed = transition.changed
            || cost_recorded
            || tariff_recorded
            || running_cost_recorded
            || limit_changed
            || limit_confirmed
            || sample_recorded
            || pending_changed
            || lock_problem_changed
            // A recorded transaction limit moves nothing about the connector, but it is state a
            // subscriber must see - the CSMS-facing snapshot, persistence, and the projection all
            // read what the transaction is running under (CV15).
            || limit_set;
        // E05 (CV2.5): the last allowance ran out. Checked after the sample has been recorded, so
        // the stop is decided against the reading the CSMS will also see, and dispatched through
        // the ordinary stop path so the transaction ends exactly as any other does.
        let allowance_spent = matches!(event_kind, EventKind::MeterValueSampled)
            && self
                .evses
                .get(evse_id)
                .and_then(|evse| evse.transactions.get(connector_id))
                .and_then(|transaction| transaction.as_ref())
                .is_some_and(|transaction| {
                    transaction.stop_at_energy_wh.is_some_and(|limit| {
                        transaction
                            .last_meter_sample
                            .is_some_and(|sample| sample.energy_wh >= limit)
                    })
                });
        if allowance_spent {
            tracing::info!(
                evse_id,
                connector_id,
                "the allowance granted after deauthorization is spent; stopping"
            );
            self.apply_connector_event(
                evse_id,
                connector_id,
                ConnectorEvent::ChargingStopped(StopReason::DeAuthorized),
                effects,
            );
            return true;
        }
        // E16.FR.05/.10/.14 (CV15): the two moments a transaction limit can change its verdict -
        // a fresh meter reading, or the limit itself moving. FR.10 is why a limit being *set* is
        // one of them: a ceiling below where the transaction already stands binds immediately
        // rather than at the next sample.
        // Whatever moved a figure a ceiling is measured against: a meter reading (energy, state
        // of charge), a cost - the station's own or the CSMS's - or the ceiling itself moving.
        // The last is E16.FR.10's case, a limit set below where the transaction already stands,
        // which binds at once rather than at the next sample.
        if matches!(
            event_kind,
            EventKind::MeterValueSampled | EventKind::TransactionLimitSet { .. }
        ) || running_cost_recorded
            || cost_recorded
        {
            self.enforce_transaction_limit(evse_id, connector_id, effects);
        }
        if let Some(pending) = dispatch_pending {
            // Recursing through `apply_connector_event` rather than reaching into the connector
            // directly, so a remotely started transaction takes byte-for-byte the same path a
            // locally started one does - including the transaction bookkeeping, the hardware
            // command and the status effects.
            tracing::debug!(
                evse_id,
                connector_id,
                "the cable arrived; dispatching the remote start the CSMS asked for"
            );
            self.apply_connector_event(
                evse_id,
                connector_id,
                ConnectorEvent::RemoteStartRequested {
                    id_token: pending.id_token,
                    remote_start_id: pending.remote_start_id,
                },
                effects,
            );
            return true;
        }
        changed
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

    /// Appends any newly-occupied slot to `OCPPCommCtrlr`/`NetworkConfigurationPriority`, which
    /// OCPP defines as the comma-separated slot numbers in the order the charge point should try
    /// them.
    ///
    /// **Appends, never reorders.** The order is the CSMS's decision - it writes that variable
    /// deliberately - so rewriting it whenever a profile arrives would clobber a configuration
    /// this charge point was told to use. A slot absent from the list would never be tried at
    /// all, though, so a newly stored profile is added at the end: the CSMS's own ordering is
    /// preserved, and a profile it just wrote does not silently become unreachable.
    fn refresh_network_configuration_priority(&mut self) {
        let component = Component {
            name: "OCPPCommCtrlr".into(),
            instance: None,
            evse: None,
        };
        let variable = Variable {
            name: "NetworkConfigurationPriority".into(),
            instance: None,
        };
        let current = self
            .device_model
            .get(&component, &variable)
            .and_then(|definition| definition.attribute(VariableAttributeType::Actual))
            .map(|attribute| attribute.value.clone())
            .unwrap_or_default();
        let mut order: Vec<i32> = current
            .split(',')
            .filter_map(|slot| slot.trim().parse().ok())
            .collect();
        // Drop slots that are no longer occupied - reporting a slot the CSMS could select and
        // this charge point has nothing for would be worse than a shorter list.
        order.retain(|slot| self.network_profiles.get(*slot).is_some());
        for occupied in self.network_profiles.slots() {
            if !order.contains(&occupied.slot) {
                order.push(occupied.slot);
            }
        }
        let value = order
            .iter()
            .map(alloc::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        self.device_model.set_attribute_value(
            &component,
            &variable,
            VariableAttributeType::Actual,
            value,
        );
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

    /// Records an external charging limit - on one EVSE, or (`evse_id: None`) the whole charging
    /// station - and pushes the [`ChargePointEffect::SmartChargingNotification`] that reports it.
    /// A limit addressing an EVSE that doesn't exist is dropped and logged rather than recorded
    /// anywhere, mirroring how an out-of-range address is handled throughout this crate.
    ///
    /// Recording it is also what *enforces* it: the slot written here is read by
    /// [`crate::smart_charging::external_charging_limits`], which composition applies as an upper
    /// bound on whatever the CSMS's own profiles asked for (K11.FR.01, K12.FR.01, K27.FR.01). A
    /// limit that carries no schedule is the one exception - there is no number to enforce - and it
    /// is warned about rather than silently reported as if it had taken effect.
    fn set_external_charging_limit(
        &mut self,
        evse_id: Option<usize>,
        limit: ExternalChargingLimit,
        effects: &mut Vec<ChargePointEffect>,
    ) -> bool {
        if limit.schedule.is_none() {
            tracing::warn!(
                source = limit.source.name(),
                "an external charging limit carrying no schedule will be reported to the CSMS but \
                 cannot be enforced - there is no limit value to apply"
            );
        }
        // Which slot depends on what the limit *is*: capacity and constraints are held separately
        // so a station can be under both at once, which is exactly the case K27.FR.05 describes.
        match (evse_id, limit.is_local_generation) {
            (None, false) => self.station_external_charging_limit = Some(limit.clone()),
            (None, true) => self.station_local_generation_limit = Some(limit.clone()),
            (Some(id), is_local_generation) => {
                let Some(evse) = self.evses.get_mut(id) else {
                    tracing::warn!(
                        evse_id = id,
                        "ignoring an external charging limit set for an EVSE that doesn't exist"
                    );
                    return false;
                };
                if is_local_generation {
                    evse.local_generation_limit = Some(limit.clone());
                } else {
                    evse.external_charging_limit = Some(limit.clone());
                }
            }
        }
        effects.push(ChargePointEffect::SmartChargingNotification(
            SmartChargingNotification::ExternalChargingLimitSet { evse_id, limit },
        ));
        true
    }

    /// Applies **E16**'s ceilings to one connector's transaction: suspends energy transfer when
    /// one has been reached, and resumes it when the ceiling moves back above where the
    /// transaction stands (CV15).
    ///
    /// # Which cost, and why it is read rather than chosen here
    ///
    /// **E16.FR.15/.16** split the cost source by whether the station prices the session itself:
    /// with a tariff in force the local running cost decides, and without one the CSMS's
    /// `totalCost`/`CostUpdated` does. Both are already on [`EvseState`] - `running_cost` is
    /// CV8's, `running_costs` is the CSMS's - so this reads whichever exists, preferring the local
    /// one exactly as those requirements order them.
    ///
    /// # Suspend, never end
    ///
    /// E16.FR.06 would have the transaction *end* rather than suspend when `TxCtrlr.TxStopPoint`
    /// contains `EnergyTransfer`. It cannot here: [`TxStopPoint`] models the three points this
    /// crate can observe and `EnergyTransfer` is not among them, so a `SetVariables` naming it is
    /// `Rejected` by CV3's validation against the declared `values_list`. The station can
    /// therefore never be configured into FR.06's branch, and FR.05 - suspend, report
    /// `SuspendedEVSE` with the limit's own trigger reason - is the only answer it can give.
    ///
    /// Suspending means *commanding* zero current, not merely recording a state: OCPP's
    /// `SuspendedEVSE` is a report, and a station that reported it while energy kept flowing
    /// would be lying in the direction that costs the driver money. The command goes out from
    /// here rather than from `crate::smart_charging`'s projection so that a station built without
    /// the smart-charging feature still enforces the limit it accepted.
    fn enforce_transaction_limit(
        &mut self,
        evse_id: usize,
        connector_id: usize,
        effects: &mut Vec<ChargePointEffect>,
    ) {
        let Some(evse) = self.evses.get(evse_id) else {
            return;
        };
        let local_cost = evse
            .running_cost_totals
            .get(connector_id)
            .copied()
            .flatten();
        let csms_cost = evse.running_costs.get(connector_id).copied().flatten();
        let Some(Some(transaction)) = evse.transactions.get(connector_id) else {
            return;
        };
        let Some(limit) = transaction.limit else {
            return;
        };
        // E16.FR.16 before E16.FR.15: a locally priced session uses its own figure, and only a
        // station that cannot price one falls back to what the CSMS last said it had spent.
        let cost = local_cost.or(csms_cost);
        let delivered_wh = transaction
            .energy_start_wh
            .zip(transaction.last_meter_sample)
            .map(|(start, sample)| (sample.energy_wh - start) as f64);
        let soc = transaction
            .last_meter_sample
            .and_then(|sample| sample.soc_percent);

        let reached = None
            .or_else(|| {
                (limit.max_cost.zip(cost)).and_then(|(max, cost)| {
                    (cost >= max).then_some(crate::state::TransactionLimitKind::Cost)
                })
            })
            .or_else(|| {
                (limit.max_energy_wh.zip(delivered_wh)).and_then(|(max, delivered)| {
                    (delivered >= max).then_some(crate::state::TransactionLimitKind::Energy)
                })
            })
            .or_else(|| {
                (limit.max_soc_percent.zip(soc)).and_then(|(max, soc)| {
                    (soc >= max).then_some(crate::state::TransactionLimitKind::Soc)
                })
            });

        match (reached, transaction.limit_reached) {
            // Already suspended for this reason, and still over it - nothing to say twice.
            (Some(_), Some(_)) | (None, None) => {}
            (Some(kind), None) => {
                tracing::info!(
                    evse_id,
                    connector_id,
                    limit = kind.name(),
                    "a transaction limit was reached; suspending energy transfer"
                );
                // Order matters: stop the energy, then report having stopped it.
                self.request_current_limit(evse_id, connector_id, Some(0), effects);
                self.apply_connector_event(
                    evse_id,
                    connector_id,
                    ConnectorEvent::ChargingSuspendedByEvse,
                    effects,
                );
                if let Some(Some(transaction)) = self
                    .evses
                    .get_mut(evse_id)
                    .and_then(|evse| evse.transactions.get_mut(connector_id))
                {
                    transaction.limit_reached = Some(kind);
                    transaction.seq_no += 1;
                    effects.push(ChargePointEffect::TransactionEvent(
                        TransactionEventOccurred {
                            evse_id,
                            connector_id,
                            kind: TransactionEventKind::Updated(
                                TransactionUpdateReason::LimitReached(kind),
                            ),
                            transaction: transaction.clone(),
                            offline: false,
                        },
                    ));
                }
            }
            // E16.FR.14: the ceiling moved back above where this transaction stands, so energy
            // may flow again. The 0 A command is withdrawn rather than raised to a number: what
            // the connector may draw is `crate::smart_charging`'s answer, not this function's,
            // and `None` is "nothing here limits it" rather than a limit of this crate's
            // invention.
            (None, Some(kind)) => {
                tracing::info!(
                    evse_id,
                    connector_id,
                    limit = kind.name(),
                    "the transaction limit was raised; resuming energy transfer"
                );
                self.request_current_limit(evse_id, connector_id, None, effects);
                self.apply_connector_event(
                    evse_id,
                    connector_id,
                    ConnectorEvent::ChargingResumed,
                    effects,
                );
                if let Some(Some(transaction)) = self
                    .evses
                    .get_mut(evse_id)
                    .and_then(|evse| evse.transactions.get_mut(connector_id))
                {
                    transaction.limit_reached = None;
                }
            }
        }
    }

    /// Records a requested current limit for one connector and commands hardware, if it differs
    /// from what that connector was last asked for - the same "only when it actually changed"
    /// rule [`ConnectorEvent::CurrentLimitComputed`] follows, factored out so a transaction limit
    /// (CV15) can reach hardware through it without going round the projection.
    fn request_current_limit(
        &mut self,
        evse_id: usize,
        connector_id: usize,
        limit_ma: Option<u32>,
        effects: &mut Vec<ChargePointEffect>,
    ) {
        let Some(slot) = self
            .evses
            .get_mut(evse_id)
            .and_then(|evse| evse.charging_limits.get_mut(connector_id))
        else {
            return;
        };
        if *slot == limit_ma {
            return;
        }
        *slot = limit_ma;
        effects.push(ChargePointEffect::HardwareCommand(
            HardwareCommand::SetCurrentLimit {
                evse_id,
                connector_id,
                limit_ma,
            },
        ));
    }

    /// Clears an external charging limit previously recorded by [`Self::set_external_charging_limit`]
    /// and pushes the [`ChargePointEffect::SmartChargingNotification`] that reports it - but only
    /// if `evse_id`/`source` actually match a limit currently recorded there. Reporting a
    /// clearance for a limit the CSMS was never told about would be worse than silence, so a
    /// mismatch (or an already-clear slot) is a no-op.
    fn clear_external_charging_limit(
        &mut self,
        evse_id: Option<usize>,
        source: crate::state::ChargingLimitSource,
        is_local_generation: bool,
        effects: &mut Vec<ChargePointEffect>,
    ) -> bool {
        let slot = match (evse_id, is_local_generation) {
            (None, false) => &mut self.station_external_charging_limit,
            (None, true) => &mut self.station_local_generation_limit,
            (Some(id), is_local_generation) => {
                let Some(evse) = self.evses.get_mut(id) else {
                    return false;
                };
                if is_local_generation {
                    &mut evse.local_generation_limit
                } else {
                    &mut evse.external_charging_limit
                }
            }
        };
        if slot.as_ref().is_some_and(|limit| limit.source == source) {
            *slot = None;
            effects.push(ChargePointEffect::SmartChargingNotification(
                SmartChargingNotification::ExternalChargingLimitCleared { evse_id, source },
            ));
            true
        } else {
            false
        }
    }
}

/// Which of the events `apply_connector_event` special-cases this one is - captured before
/// `ConnectorState::apply` consumes it, because two later blocks still need to know (CV2.5).
#[derive(Debug, Clone, Copy, PartialEq)]
enum EventKind {
    /// The identifier was revoked mid-session (E05).
    AuthorizationRevoked,
    /// A meter reading arrived - the thing that can cross an E05 allowance, or a transaction
    /// limit (E16).
    MeterValueSampled,
    /// The connector's plug retention lock failed (G05).
    LockFailed,
    /// A ceiling was set on the transaction (E16, CV15).
    TransactionLimitSet {
        limit: crate::state::TransactionLimit,
        from_csms: bool,
    },
    Other,
}

/// Where in a session a transaction begins and ends - `TxCtrlr.TxStartPoint`/`TxStopPoint`
/// (CV2.2), paired because `advance_transaction` needs both on every call and neither means much
/// without the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TransactionPoints {
    tx_start_point: TxStartPoint,
    tx_stop_point: TxStopPoint,
}

/// How a transaction came to exist - everything `advance_transaction` records on a new
/// [`Transaction`] that describes its origin rather than its progress. Grouped because all three
/// are read from the same triggering event and are only ever used together (CV6).
struct TransactionOrigin {
    /// The identifier that authorized it, from a `ChargingAuthorized`/`RemoteStartRequested`.
    id_token: Option<IdToken>,
    /// The `remoteStartId` of the `RequestStartTransaction` that began it, if it began that way.
    remote_start_id: Option<i64>,
    /// The reservation it consumed, if the connector had honoured one.
    reservation_id: Option<i64>,
}

/// Advances (or starts, or ends) the transaction in `slot` for a connector moving from
/// `previous_state` to `new_state`, returning the TransactionEvent to report, if any.
///
/// `event_stop_reason` is the `StopReason` carried by the triggering
/// `ConnectorEvent::ChargingStopped`/`ConnectorEvent::ResetRequested`, if that is what caused this
/// transition. `origin` carries everything recorded on a *new* transaction about how it began -
/// see [`TransactionOrigin`].
/// Whether moving from `previous_state` to `new_state` is the moment `tx_start_point` names
/// (CV2.2).
///
/// Each point is one transition, and they are checked exclusively rather than cumulatively: with
/// `PowerPathClosed` configured, locking the cable and authorizing both happen without a
/// transaction existing, and the contactor closing is what creates it.
fn starts_transaction(
    previous_state: ConnectorState,
    new_state: ConnectorState,
    tx_start_point: TxStartPoint,
) -> bool {
    match tx_start_point {
        // The cable is in and latched. Everything after this - including an authorization that
        // is refused - happens inside a transaction the CSMS already knows about.
        TxStartPoint::EVConnected => {
            previous_state == ConnectorState::Connected && new_state == ConnectorState::Locked
        }
        // OCPP's default: an identifier was accepted. Reached from `Authorizing` (a card was
        // presented) or straight from `Locked` (a CSMS `RequestStartTransaction`).
        TxStartPoint::Authorized => {
            matches!(
                previous_state,
                ConnectorState::Authorizing | ConnectorState::Locked
            ) && new_state == ConnectorState::Starting
        }
        // The contactor closed, so energy can flow.
        TxStartPoint::PowerPathClosed => {
            previous_state == ConnectorState::Starting && new_state == ConnectorState::Charging
        }
    }
}

/// Whether moving from `previous_state` to `new_state` is the moment `tx_stop_point` names
/// (CV2.2).
///
/// The counterpart of [`starts_transaction`], and checked the same way - exclusively, one
/// transition per point. Which transitions those are is what makes each point real:
///
/// - **`Authorized`** - entering a stop. This crate's stop path is driven by an explicit
///   [`ConnectorEvent::ChargingStopped`] (or a `Reset`, or E05's revocation), and *that event
///   arriving* is the authorization for this session ending; there is no separate "the driver's
///   permission lapsed" signal to observe.
/// - **`PowerPathClosed`** - the contactor confirmed open. Both stopping states settle here, and
///   this is what this crate did unconditionally before `TxStopPoint` was honoured.
/// - **`EVConnected`** - the cable left the connector, which is the connector returning to
///   `Available` from `Connected`. The transaction therefore survives the whole unlock, which is
///   the point: it is billing for the bay being occupied.
fn ends_transaction(
    previous_state: ConnectorState,
    new_state: ConnectorState,
    tx_stop_point: TxStopPoint,
) -> bool {
    match tx_stop_point {
        TxStopPoint::Authorized => {
            matches!(
                previous_state,
                ConnectorState::Starting
                    | ConnectorState::Charging
                    | ConnectorState::SuspendedEv
                    | ConnectorState::SuspendedEvse
            ) && matches!(
                new_state,
                ConnectorState::Stopping | ConnectorState::StoppingLocked
            )
        }
        TxStopPoint::PowerPathClosed => {
            (previous_state == ConnectorState::Stopping && new_state == ConnectorState::Finishing)
                || (previous_state == ConnectorState::StoppingLocked
                    && new_state == ConnectorState::Locked)
        }
        TxStopPoint::EVConnected => {
            previous_state == ConnectorState::Connected && new_state == ConnectorState::Available
        }
    }
}

/// The charging state a transaction should report when it begins at `state`.
fn charging_state_for(state: ConnectorState) -> TransactionChargingState {
    match state {
        ConnectorState::Charging => TransactionChargingState::Charging,
        ConnectorState::SuspendedEv => TransactionChargingState::SuspendedEV,
        ConnectorState::SuspendedEvse => TransactionChargingState::SuspendedEVSE,
        _ => TransactionChargingState::EvConnected,
    }
}

fn advance_transaction(
    slot: &mut Option<Transaction>,
    next_transaction_id: &mut u64,
    previous_state: ConnectorState,
    new_state: ConnectorState,
    event_stop_reason: Option<StopReason>,
    origin: TransactionOrigin,
    points: TransactionPoints,
) -> Option<(TransactionEventKind, Transaction)> {
    let TransactionPoints {
        tx_start_point,
        tx_stop_point,
    } = points;
    // CV2.2: which transition begins a transaction is `TxCtrlr.TxStartPoint`, not a constant.
    // Checked before the arms below because with an earlier start point the *same* transitions
    // that used to begin one now merely update one that already exists.
    if slot.is_none() && starts_transaction(previous_state, new_state, tx_start_point) {
        let id = TransactionId(*next_transaction_id);
        *next_transaction_id += 1;
        let transaction = Transaction {
            id,
            id_token: origin.id_token,
            charging_state: charging_state_for(new_state),
            stop_reason: None,
            seq_no: 0,
            last_meter_sample: None,
            priority_charging: false,
            remote_start_id: origin.remote_start_id,
            reservation_id: origin.reservation_id,
            stop_at_energy_wh: None,
            limit: None,
            csms_limit: None,
            limit_reached: None,
            energy_start_wh: None,
        };
        *slot = Some(transaction.clone());
        return Some((TransactionEventKind::Started, transaction));
    }
    // Recorded the moment the stop begins, whatever `tx_stop_point` says: with a later stop point
    // the transaction outlives this transition, and by the time it does end the event that caused
    // the stop is gone. This is why `stoppedReason` survives an `EVConnected` stop point at all.
    if matches!(
        previous_state,
        ConnectorState::Starting
            | ConnectorState::Charging
            | ConnectorState::SuspendedEv
            | ConnectorState::SuspendedEvse
    ) && matches!(
        new_state,
        ConnectorState::Stopping | ConnectorState::StoppingLocked
    ) && let Some(transaction) = slot.as_mut()
    {
        transaction.stop_reason = event_stop_reason;
    }
    // CV2.2: which transition ends a transaction is `TxCtrlr.TxStopPoint`. Checked before the
    // arms below for the same reason `starts_transaction` is: with an earlier stop point, a
    // transition that used to merely update the transaction is now the one that closes it.
    if ends_transaction(previous_state, new_state, tx_stop_point) {
        let mut transaction = slot.take()?;
        // The bay is only free once the cable is out; every earlier stop point leaves it in.
        transaction.charging_state = if new_state == ConnectorState::Available {
            TransactionChargingState::Idle
        } else {
            TransactionChargingState::EvConnected
        };
        transaction.seq_no += 1;
        return Some((TransactionEventKind::Ended, transaction));
    }
    match (previous_state, new_state) {
        // Reached from `Authorizing` (a physically presented id token was authorized) or
        // directly from `Locked` (a CSMS-initiated `RequestStartTransaction` - see
        // `docs/ROADMAP.md` §6) - either way, entering `Starting` from elsewhere always begins a
        // new transaction. Excludes `Starting` -> `Starting` (e.g. a meter sample applied while
        // still `Starting`, which doesn't change connector state) - that must stay a no-op.

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
        // A fault ends the transaction whatever `TxStopPoint` says: the charge point can no longer
        // observe the conditions a later stop point is waiting for, so honouring one here would
        // leave the transaction open on a connector that is out of service.
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
    // The baseline `maxEnergy` is measured against (E16, CV15): the first reading this
    // transaction saw while charging, so the limit bounds the energy *this session* delivered
    // rather than wherever the meter's lifetime total happened to stand.
    transaction.energy_start_wh.get_or_insert(sample.energy_wh);
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

    // --- CV15: transaction limits (E16) ---

    /// A connector charging, with `wh` on the meter as its baseline.
    fn charging_from(wh: i64) -> ChargePointState {
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
                energy_wh: wh,
                ..Default::default()
            }),
        );
        state
    }

    fn sample_at(wh: i64) -> ConnectorEvent {
        ConnectorEvent::MeterValueSampled(MeterSample {
            energy_wh: wh,
            ..Default::default()
        })
    }

    fn energy_limit(wh: f64) -> ConnectorEvent {
        ConnectorEvent::TransactionLimitSet {
            limit: crate::state::TransactionLimit {
                max_energy_wh: Some(wh),
                ..Default::default()
            },
            from_csms: true,
        }
    }

    /// Only the reasons CV15 introduces - a meter sample reports `MeterValuePeriodic` on its own
    /// account, and counting that here would make every assertion below about limits pass or fail
    /// for the wrong reason.
    fn reported_limit_events(
        effects: &[ChargePointEffect],
    ) -> alloc::vec::Vec<TransactionUpdateReason> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                ChargePointEffect::TransactionEvent(occurred) => match occurred.kind {
                    TransactionEventKind::Updated(
                        reason @ (TransactionUpdateReason::LimitSet
                        | TransactionUpdateReason::LimitReached(_)),
                    ) => Some(reason),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    /// E16.FR.01/.03: the ceiling is recorded and confirmed back once, with its own trigger
    /// reason, which is what tells the CSMS the limit took.
    #[test]
    fn a_transaction_limit_is_recorded_and_confirmed_once() {
        let mut state = charging_from(1_000);

        let effects = apply_connector_event(&mut state, energy_limit(5_000.0));

        assert_eq!(
            state.evses[0].transactions[0]
                .as_ref()
                .and_then(|transaction| transaction.limit)
                .and_then(|limit| limit.max_energy_wh),
            Some(5_000.0)
        );
        assert_eq!(
            reported_limit_events(&effects),
            alloc::vec![TransactionUpdateReason::LimitSet]
        );
    }

    /// E16.FR.05: reaching it suspends energy transfer, says so with the trigger reason that
    /// names *which* limit, and actually stops the current rather than only reporting that it
    /// did.
    #[test]
    fn reaching_an_energy_limit_suspends_energy_transfer_and_says_which_limit() {
        let mut state = charging_from(1_000);
        apply_connector_event(&mut state, energy_limit(5_000.0));

        // 4 999 Wh delivered - still under.
        let effects = apply_connector_event(&mut state, sample_at(5_999));
        assert!(reported_limit_events(&effects).is_empty());
        assert_eq!(state.evses[0].connectors[0], ConnectorState::Charging);

        // 5 000 Wh delivered - at the limit, which counts as reached.
        let effects = apply_connector_event(&mut state, sample_at(6_000));

        assert_eq!(state.evses[0].connectors[0], ConnectorState::SuspendedEvse);
        assert!(
            reported_limit_events(&effects).contains(&TransactionUpdateReason::LimitReached(
                crate::state::TransactionLimitKind::Energy
            ))
        );
        assert!(effects.contains(&ChargePointEffect::HardwareCommand(
            HardwareCommand::SetCurrentLimit {
                evse_id: 0,
                connector_id: 0,
                limit_ma: Some(0),
            }
        )));
    }

    /// The limit is measured from the transaction's own baseline, not the meter's lifetime total
    /// - a station whose meter has 900 kWh on it must not refuse every session.
    #[test]
    fn the_energy_limit_measures_from_this_transactions_first_reading() {
        let mut state = charging_from(900_000);
        apply_connector_event(&mut state, energy_limit(5_000.0));

        let effects = apply_connector_event(&mut state, sample_at(902_000));

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Charging);
        assert!(reported_limit_events(&effects).is_empty());
    }

    /// E16.FR.14: raising the ceiling past where the transaction stands resumes energy transfer,
    /// and withdraws the 0 A command rather than inventing a limit of its own.
    #[test]
    fn raising_the_limit_resumes_energy_transfer() {
        let mut state = charging_from(1_000);
        apply_connector_event(&mut state, energy_limit(5_000.0));
        apply_connector_event(&mut state, sample_at(6_000));
        assert_eq!(state.evses[0].connectors[0], ConnectorState::SuspendedEvse);

        let effects = apply_connector_event(&mut state, energy_limit(10_000.0));

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Charging);
        assert!(
            state.evses[0].transactions[0]
                .as_ref()
                .is_some_and(|transaction| transaction.limit_reached.is_none())
        );
        assert!(effects.contains(&ChargePointEffect::HardwareCommand(
            HardwareCommand::SetCurrentLimit {
                evse_id: 0,
                connector_id: 0,
                limit_ma: None,
            }
        )));
    }

    /// E16.FR.10: a ceiling set below where the transaction already stands binds at once, rather
    /// than waiting for a meter reading that may be a full sampling interval away.
    #[test]
    fn a_limit_set_below_the_energy_already_delivered_binds_immediately() {
        let mut state = charging_from(1_000);
        apply_connector_event(&mut state, sample_at(6_000));

        let effects = apply_connector_event(&mut state, energy_limit(2_000.0));

        assert_eq!(state.evses[0].connectors[0], ConnectorState::SuspendedEvse);
        assert!(
            reported_limit_events(&effects).contains(&TransactionUpdateReason::LimitReached(
                crate::state::TransactionLimitKind::Energy
            ))
        );
    }

    /// E16.FR.04: the CSMS has the last word. A driver asking for more than the prepaid balance
    /// allows gets the balance, not the request.
    #[test]
    fn a_locally_set_limit_cannot_exceed_the_one_the_csms_set() {
        let mut state = charging_from(1_000);
        apply_connector_event(&mut state, energy_limit(5_000.0));

        apply_connector_event(
            &mut state,
            ConnectorEvent::TransactionLimitSet {
                limit: crate::state::TransactionLimit {
                    max_energy_wh: Some(50_000.0),
                    ..Default::default()
                },
                from_csms: false,
            },
        );

        assert_eq!(
            state.evses[0].transactions[0]
                .as_ref()
                .and_then(|transaction| transaction.limit)
                .and_then(|limit| limit.max_energy_wh),
            Some(5_000.0),
        );
    }

    /// ...and the CSMS raising its *own* limit is not clamped against its previous one, which is
    /// what E16.FR.14's "increased by CSMS" case depends on.
    #[test]
    fn the_csms_may_raise_the_limit_it_set_itself() {
        let mut state = charging_from(1_000);
        apply_connector_event(&mut state, energy_limit(5_000.0));

        apply_connector_event(&mut state, energy_limit(50_000.0));

        assert_eq!(
            state.evses[0].transactions[0]
                .as_ref()
                .and_then(|transaction| transaction.limit)
                .and_then(|limit| limit.max_energy_wh),
            Some(50_000.0),
        );
    }

    /// E16.FR.13: a limit naming only kinds this build does not enforce is neither recorded nor
    /// confirmed - the CSMS learns from the silence, having been told what is supported by
    /// `TxCtrlr.SupportedLimits`.
    #[test]
    fn a_limit_this_build_cannot_enforce_is_neither_recorded_nor_confirmed() {
        let mut state = charging_from(1_000);

        let effects = apply_connector_event(
            &mut state,
            ConnectorEvent::TransactionLimitSet {
                limit: crate::state::TransactionLimit {
                    max_time_secs: Some(3_600),
                    ..Default::default()
                },
                from_csms: true,
            },
        );

        assert!(
            state.evses[0].transactions[0]
                .as_ref()
                .is_some_and(|transaction| transaction.limit.is_none())
        );
        assert!(reported_limit_events(&effects).is_empty());
    }

    /// A ceiling on a connector with nothing running is nothing - and must not blow up.
    #[test]
    fn a_limit_on_an_idle_connector_is_a_no_op() {
        let mut state = ChargePointState::new([1]);

        let effects = apply_connector_event(&mut state, energy_limit(5_000.0));

        assert!(reported_limit_events(&effects).is_empty());
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
            ConnectorEvent::RemoteStartRequested {
                id_token: test_id_token(),
                remote_start_id: None,
            },
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
            priority_charging: false,
            remote_start_id: None,
            reservation_id: None,
            stop_at_energy_wh: None,
            limit: None,
            csms_limit: None,
            limit_reached: None,
            energy_start_wh: None,
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
                offline: false,
            }
        )));
    }

    /// **F01.FR.01** (CV7): with `AuthCtrlr.AuthorizeRemoteStart` on, a `RequestStartTransaction`
    /// is authorized exactly as a card presented at the reader would be - the CSMS's own request
    /// is no longer taken as the decision. Energy transfer waits for that decision, so nothing is
    /// asked of the contactor yet.
    #[test]
    fn a_remote_start_is_authorized_first_when_the_operator_asks_for_it() {
        let mut state = ChargePointState::new([1]);
        set_boolean(&mut state, "AuthCtrlr", "AuthorizeRemoteStart", "true");
        apply_connector_event(&mut state, ConnectorEvent::CableConnected);
        apply_connector_event(&mut state, ConnectorEvent::LockConfirmed);

        let effects = apply_connector_event(
            &mut state,
            ConnectorEvent::RemoteStartRequested {
                id_token: test_id_token(),
                remote_start_id: Some(7),
            },
        );

        assert_eq!(
            state.evses[0].connectors[0],
            ConnectorState::Authorizing,
            "F01.FR.01 is exactly the requirement to authorize before allowing energy transfer"
        );
        assert!(effects.contains(&ChargePointEffect::AuthorizationRequested(
            AuthorizationRequested {
                evse_id: 0,
                connector_id: 0,
                id_token: test_id_token(),
                contract: None,
            }
        )));
        assert!(
            !effects.iter().any(|effect| matches!(
                effect,
                ChargePointEffect::HardwareCommand(HardwareCommand::CloseContactor { .. })
            )),
            "the contactor must not close on an authorization nobody has given yet"
        );
        assert!(state.evses[0].transactions[0].is_none());
    }

    /// The `remoteStartId` has to survive the authorization round trip, or F01.FR.25 breaks for
    /// exactly the stations that turned F01.FR.01 on: the CSMS would get a transaction it cannot
    /// correlate with the request that caused it, reported as a local start.
    #[test]
    fn an_authorized_remote_start_still_reports_the_remote_start_id() {
        let mut state = ChargePointState::new([1]);
        set_boolean(&mut state, "AuthCtrlr", "AuthorizeRemoteStart", "true");
        apply_connector_event(&mut state, ConnectorEvent::CableConnected);
        apply_connector_event(&mut state, ConnectorEvent::LockConfirmed);
        apply_connector_event(
            &mut state,
            ConnectorEvent::RemoteStartRequested {
                id_token: test_id_token(),
                remote_start_id: Some(7),
            },
        );

        assert_eq!(
            state.evses[0].connectors[0],
            ConnectorState::Authorizing,
            "otherwise this test says nothing the immediate-start path would not also satisfy"
        );

        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Starting);
        let transaction = state.evses[0].transactions[0]
            .as_ref()
            .expect("the authorization starts the transaction");
        assert_eq!(transaction.remote_start_id, Some(7));
        assert_eq!(transaction.id_token, Some(test_id_token()));
        assert!(
            state.evses[0].pending_remote_starts[0].is_none(),
            "the hold has done its job; leaving it would look like a bay still waiting for a \
             driver who is charging on it"
        );
    }

    /// A refusal must leave nothing behind - the same rule a refused card follows. Without it the
    /// held `remoteStartId` would outlive the request that carried it and attach itself to
    /// whatever the next driver starts on this connector.
    #[test]
    fn a_refused_remote_start_leaves_no_remote_start_id_for_the_next_driver() {
        let mut state = ChargePointState::new([1]);
        set_boolean(&mut state, "AuthCtrlr", "AuthorizeRemoteStart", "true");
        apply_connector_event(&mut state, ConnectorEvent::CableConnected);
        apply_connector_event(&mut state, ConnectorEvent::LockConfirmed);
        apply_connector_event(
            &mut state,
            ConnectorEvent::RemoteStartRequested {
                id_token: test_id_token(),
                remote_start_id: Some(7),
            },
        );

        apply_connector_event(&mut state, ConnectorEvent::AuthorizationDenied);

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Locked);
        assert!(state.evses[0].pending_remote_starts[0].is_none());

        // The driver then presents their own card on the same connector, and that transaction is
        // theirs - not a continuation of the remote start the CSMS was refused.
        apply_connector_event(
            &mut state,
            ConnectorEvent::IdTokenPresented(test_id_token()),
        );
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
        assert_eq!(
            state.evses[0].transactions[0]
                .as_ref()
                .expect("the card starts a transaction")
                .remote_start_id,
            None
        );
    }

    /// **F01.FR.02's second clause**: `Central` and `NoAuthorization` identifiers bypass the check
    /// however `AuthorizeRemoteStart` is set. A `Central` token *is* the CSMS's own decision -
    /// asking it to confirm its own instruction is a round trip with one possible answer - and
    /// `NoAuthorization` names a connector that authorizes nobody.
    #[test]
    fn a_centrally_assigned_token_starts_without_a_separate_authorization() {
        for kind in [IdTokenKind::Central, IdTokenKind::NoAuthorization] {
            let mut state = ChargePointState::new([1]);
            set_boolean(&mut state, "AuthCtrlr", "AuthorizeRemoteStart", "true");
            apply_connector_event(&mut state, ConnectorEvent::CableConnected);
            apply_connector_event(&mut state, ConnectorEvent::LockConfirmed);

            let effects = apply_connector_event(
                &mut state,
                ConnectorEvent::RemoteStartRequested {
                    id_token: IdToken {
                        value: "CSMS-1".into(),
                        kind,
                    },
                    remote_start_id: Some(7),
                },
            );

            assert_eq!(
                state.evses[0].connectors[0],
                ConnectorState::Starting,
                "{kind:?} is exempt from F01.FR.01"
            );
            assert!(
                !effects
                    .iter()
                    .any(|effect| matches!(effect, ChargePointEffect::AuthorizationRequested(_)))
            );
            assert_eq!(
                state.evses[0].transactions[0]
                    .as_ref()
                    .expect("the transaction starts immediately")
                    .remote_start_id,
                Some(7)
            );
        }
    }

    #[test]
    fn a_remote_start_request_is_ignored_outside_the_locked_state() {
        let mut state = ChargePointState::new([1]);

        let effects = apply_connector_event(
            &mut state,
            ConnectorEvent::RemoteStartRequested {
                id_token: test_id_token(),
                remote_start_id: None,
            },
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
                contract: None,
            }
        )));
        assert_eq!(state.evses[0].transactions[0], None);
    }

    /// C07: a Plug & Charge presentation reaches `Authorizing` exactly like a card tap, and the
    /// certificate material rides along to the Authorize rather than being kept somewhere the
    /// authorization path would have to go looking for it.
    #[test]
    fn presenting_a_contract_certificate_requests_authorization_carrying_it() {
        use crate::hardware::{CertificateHashData, HashAlgorithm, OcspCertificateId};
        use crate::state::ContractCertificate;

        let certificate = ContractCertificate {
            chain_pem: Some("-----BEGIN CERTIFICATE-----".into()),
            ocsp_data: alloc::vec![OcspCertificateId {
                hash_data: CertificateHashData {
                    hash_algorithm: HashAlgorithm::Sha256,
                    issuer_name_hash: "a".into(),
                    issuer_key_hash: "b".into(),
                    serial_number: "01".into(),
                },
                responder_url: "http://ocsp.example.com".into(),
            }],
        };

        let mut state = ChargePointState::new([1]);
        apply_connector_event(&mut state, ConnectorEvent::CableConnected);
        apply_connector_event(&mut state, ConnectorEvent::LockConfirmed);

        let effects = apply_connector_event(
            &mut state,
            ConnectorEvent::ContractCertificatePresented {
                id_token: test_id_token(),
                certificate: certificate.clone(),
            },
        );

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Authorizing);
        assert!(effects.contains(&ChargePointEffect::AuthorizationRequested(
            AuthorizationRequested {
                evse_id: 0,
                connector_id: 0,
                id_token: test_id_token(),
                contract: Some(certificate),
            }
        )));
    }

    /// **E03, "Start Transaction - IdToken First"** (CV2.3): a card presented before the cable
    /// still asks the CSMS. The connector has nowhere to move to - there is nothing plugged in -
    /// so the presentation itself, not a state change, is what raises the request.
    #[test]
    fn an_id_token_presented_before_the_cable_still_asks_the_csms() {
        let mut state = ChargePointState::new([1]);

        let effects = apply_connector_event(
            &mut state,
            ConnectorEvent::IdTokenPresented(test_id_token()),
        );

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Available);
        assert!(effects.contains(&ChargePointEffect::AuthorizationRequested(
            AuthorizationRequested {
                evse_id: 0,
                connector_id: 0,
                id_token: test_id_token(),
                contract: None,
            }
        )));
        assert!(
            state.evses[0].pending_remote_starts[0].is_none(),
            "nothing is held until the CSMS actually accepts it"
        );
    }

    /// The other half of E03: the acceptance is held against the connector and dispatched by the
    /// very same latch a held remote start is, so the transaction that results gets identical
    /// bookkeeping to one started the ordinary way round.
    #[test]
    fn an_accepted_card_presented_before_the_cable_is_held_until_the_driver_plugs_in() {
        let mut state = ChargePointState::new([1]);

        apply_connector_event(
            &mut state,
            ConnectorEvent::IdTokenPresented(test_id_token()),
        );
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
        assert_eq!(
            state.evses[0].pending_remote_starts[0]
                .as_ref()
                .map(|pending| pending.id_token.clone()),
            Some(test_id_token())
        );
        assert_eq!(state.evses[0].connectors[0], ConnectorState::Available);

        apply_connector_event(&mut state, ConnectorEvent::CableConnected);
        apply_connector_event(&mut state, ConnectorEvent::LockConfirmed);

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Starting);
        let transaction = state.evses[0].transactions[0]
            .as_ref()
            .expect("the cable arriving dispatches the held authorization");
        assert_eq!(transaction.id_token, Some(test_id_token()));
        assert_eq!(
            transaction.remote_start_id, None,
            "a locally presented card is not a remote start"
        );
        assert!(state.evses[0].pending_remote_starts[0].is_none());
    }

    /// A refusal must leave nothing behind: an identifier the CSMS rejected cannot become a live
    /// authorization for whoever plugs in next.
    #[test]
    fn a_refused_card_presented_before_the_cable_holds_nothing() {
        let mut state = ChargePointState::new([1]);

        apply_connector_event(
            &mut state,
            ConnectorEvent::IdTokenPresented(test_id_token()),
        );
        apply_connector_event(&mut state, ConnectorEvent::AuthorizationDenied);

        assert!(state.evses[0].pending_remote_starts[0].is_none());
    }

    /// The hold has to survive the traffic an idle connector actually produces. Hardware pushes
    /// meter readings whether or not anything is charging (see `EvseState::latest_meter_samples`),
    /// and each of those used to find the connector `Available` and drop whatever was held - which
    /// broke F02's held remote start just as thoroughly as E03's held card.
    #[test]
    fn a_held_authorization_survives_meter_samples_from_an_idle_connector() {
        let mut state = ChargePointState::new([1]);
        apply_connector_event(
            &mut state,
            ConnectorEvent::IdTokenPresented(test_id_token()),
        );
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );

        apply_connector_event(&mut state, ConnectorEvent::MeterValueSampled(sample(10)));

        assert!(state.evses[0].pending_remote_starts[0].is_some());
    }

    /// **E03.FR.15**: the driver never plugged in, so the sweep drops the hold - and the next
    /// driver to plug into that connector gets nothing from it. This is the state-machine half;
    /// the timing lives in `crate::remote_control::run_pending_remote_start_timeouts`.
    #[test]
    fn a_held_card_that_times_out_leaves_no_authorization_behind() {
        let mut state = ChargePointState::new([1]);
        apply_connector_event(
            &mut state,
            ConnectorEvent::IdTokenPresented(test_id_token()),
        );
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );

        apply_connector_event(&mut state, ConnectorEvent::RemoteStartPendingCleared);

        assert!(state.evses[0].pending_remote_starts[0].is_none());
        apply_connector_event(&mut state, ConnectorEvent::CableConnected);
        apply_connector_event(&mut state, ConnectorEvent::LockConfirmed);
        assert_eq!(
            state.evses[0].connectors[0],
            ConnectorState::Locked,
            "a deauthorized hold must not start a session for the next driver"
        );
        assert!(state.evses[0].transactions[0].is_none());
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
            priority_charging: false,
            remote_start_id: None,
            reservation_id: None,
            stop_at_energy_wh: None,
            limit: None,
            csms_limit: None,
            limit_reached: None,
            energy_start_wh: None,
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
                offline: false,
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
            priority_charging: false,
            remote_start_id: None,
            reservation_id: None,
            stop_at_energy_wh: None,
            limit: None,
            csms_limit: None,
            limit_reached: None,
            energy_start_wh: None,
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
                offline: false,
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
            priority_charging: false,
            remote_start_id: None,
            reservation_id: None,
            stop_at_energy_wh: None,
            limit: None,
            csms_limit: None,
            limit_reached: None,
            // The first reading this transaction saw is the baseline `maxEnergy` is measured
            // against (CV15) - recorded whether or not a limit is in force, since one set later
            // must still measure from the session's real start.
            energy_start_wh: Some(sample.energy_wh),
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
                offline: false,
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
            priority_charging: false,
            remote_start_id: None,
            reservation_id: None,
            stop_at_energy_wh: None,
            limit: None,
            csms_limit: None,
            limit_reached: None,
            energy_start_wh: None,
        };
        assert_eq!(state.evses[0].connectors[0], ConnectorState::Finishing);
        assert_eq!(state.evses[0].transactions[0], None);
        assert!(effects.contains(&ChargePointEffect::TransactionEvent(
            TransactionEventOccurred {
                evse_id: 0,
                connector_id: 0,
                kind: TransactionEventKind::Ended,
                transaction: expected_transaction,
                offline: false,
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
            priority_charging: false,
            remote_start_id: None,
            reservation_id: None,
            stop_at_energy_wh: None,
            limit: None,
            csms_limit: None,
            limit_reached: None,
            energy_start_wh: None,
        };
        assert_eq!(state.evses[0].transactions[0], None);
        assert!(effects.contains(&ChargePointEffect::TransactionEvent(
            TransactionEventOccurred {
                evse_id: 0,
                connector_id: 0,
                kind: TransactionEventKind::Ended,
                transaction: expected_transaction,
                offline: false,
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

    /// A reservation has to survive the traffic a reserved connector actually sees. Idle hardware
    /// keeps pushing meter samples, and CV7 can hold a remote start against a reserved bay - each
    /// of those found the connector still `Reserved` and used to overwrite the record with the
    /// nothing that event carried, leaving a bay reserved for no one.
    #[test]
    fn a_reservation_survives_events_that_leave_the_connector_reserved() {
        let mut state = ChargePointState::new([1]);
        apply_connector_event(&mut state, ConnectorEvent::Reserved(reservation(1)));
        assert!(state.evses[0].reservations[0].is_some());

        apply_connector_event(
            &mut state,
            ConnectorEvent::MeterValueSampled(crate::state::MeterSample {
                energy_wh: 10,
                ..Default::default()
            }),
        );

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Reserved);
        assert_eq!(
            state.evses[0].reservations[0].as_ref().map(|r| r.id),
            Some(crate::state::ReservationId(1)),
            "a meter sample says nothing about who the bay is held for"
        );
    }

    fn reservation(id: i64) -> crate::state::Reservation {
        crate::state::Reservation {
            id: crate::state::ReservationId(id),
            id_token: test_id_token(),
            group_id_token: None,
            expires_at: None,
        }
    }

    /// CV7: holding a request is a published state change in its own right - it arrives on an
    /// idle connector and moves nothing, so without saying so the actor would never publish it.
    #[test]
    fn recording_a_held_remote_start_is_itself_a_state_change() {
        let mut state = ChargePointState::new([1]);
        state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::Connector {
                connector_id: 0,
                event: ConnectorEvent::RemoteStartPending(crate::state::PendingRemoteStart {
                    id_token: test_id_token(),
                    remote_start_id: Some(1),
                }),
            },
        });
        assert!(state.evses[0].pending_remote_starts[0].is_some());
    }

    // --- CV2.2: TxCtrlr.TxStartPoint ---

    fn set_string(state: &mut ChargePointState, component: &str, variable: &str, value: &str) {
        state.apply(ChargePointEvent::DeviceModel(
            crate::state::DeviceModelEvent::AttributeValueSet {
                component: Component {
                    name: component.into(),
                    instance: None,
                    evse: None,
                },
                variable: crate::state::Variable {
                    name: variable.into(),
                    instance: None,
                },
                attribute_type: crate::state::VariableAttributeType::Actual,
                value: value.into(),
            },
        ));
    }

    /// The default, unchanged: the transaction begins when the identifier is accepted.
    #[test]
    fn a_transaction_starts_at_authorization_by_default() {
        let mut state = ChargePointState::new([1]);

        apply_connector_event(&mut state, ConnectorEvent::CableConnected);
        apply_connector_event(&mut state, ConnectorEvent::LockConfirmed);
        assert!(
            state.evses[0].transactions[0].is_none(),
            "a latched cable is not yet a transaction under the default start point"
        );

        apply_connector_event(
            &mut state,
            ConnectorEvent::IdTokenPresented(test_id_token()),
        );
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );

        assert!(state.evses[0].transactions[0].is_some());
    }

    /// `EVConnected`: the transaction covers the whole time the bay is occupied, so it exists
    /// *before* anyone has been authorized - which is the point of the setting, and exactly what
    /// a CSMS asking for it expects to see.
    #[test]
    fn ev_connected_starts_the_transaction_when_the_cable_latches() {
        let mut state = ChargePointState::new([1]);
        set_string(&mut state, "TxCtrlr", "TxStartPoint", "EVConnected");

        apply_connector_event(&mut state, ConnectorEvent::CableConnected);
        apply_connector_event(&mut state, ConnectorEvent::LockConfirmed);

        assert!(
            state.evses[0].transactions[0].is_some(),
            "EVConnected starts the transaction at the latch, before any authorization"
        );
        // And authorizing afterwards must not start a *second* one.
        let id = state.evses[0].transactions[0].as_ref().unwrap().id;
        apply_connector_event(
            &mut state,
            ConnectorEvent::IdTokenPresented(test_id_token()),
        );
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
        assert_eq!(state.evses[0].transactions[0].as_ref().unwrap().id, id);
    }

    /// `PowerPathClosed`: nothing exists until the contactor closes, so a session that is
    /// authorized but never energised produces no transaction at all.
    #[test]
    fn power_path_closed_starts_the_transaction_only_when_the_contactor_closes() {
        let mut state = ChargePointState::new([1]);
        set_string(&mut state, "TxCtrlr", "TxStartPoint", "PowerPathClosed");

        apply_connector_event(&mut state, ConnectorEvent::CableConnected);
        apply_connector_event(&mut state, ConnectorEvent::LockConfirmed);
        apply_connector_event(
            &mut state,
            ConnectorEvent::IdTokenPresented(test_id_token()),
        );
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
        assert!(
            state.evses[0].transactions[0].is_none(),
            "authorized is not energised"
        );

        apply_connector_event(&mut state, ConnectorEvent::ContactorClosed);

        let transaction = state.evses[0].transactions[0]
            .as_ref()
            .expect("the contactor closing starts it");
        assert_eq!(
            transaction.charging_state,
            crate::state::TransactionChargingState::Charging,
            "a transaction that begins at the contactor begins already charging"
        );
    }

    /// OCPP models the start point as a set that must *all* hold, and the three points this
    /// crate observes are strictly ordered along a session - so a set resolves to its latest
    /// member.
    #[test]
    fn a_set_of_start_points_resolves_to_the_latest_of_them() {
        let mut state = ChargePointState::new([1]);
        set_string(
            &mut state,
            "TxCtrlr",
            "TxStartPoint",
            "EVConnected,Authorized",
        );

        apply_connector_event(&mut state, ConnectorEvent::CableConnected);
        apply_connector_event(&mut state, ConnectorEvent::LockConfirmed);
        assert!(
            state.evses[0].transactions[0].is_none(),
            "EVConnected alone does not satisfy a set that also names Authorized"
        );

        apply_connector_event(
            &mut state,
            ConnectorEvent::IdTokenPresented(test_id_token()),
        );
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
        assert!(state.evses[0].transactions[0].is_some());
    }

    // --- CV2.2: TxCtrlr.TxStopPoint ---

    /// The transaction that `charging_connector` started, or `None` if it has ended.
    fn transaction(state: &ChargePointState) -> Option<&crate::state::Transaction> {
        state.evses[0].transactions[0].as_ref()
    }

    /// The default, and what this crate did before `TxStopPoint` was honoured: the transaction
    /// ends when the contactor confirms open, not when the stop is asked for.
    #[test]
    fn a_transaction_ends_when_the_power_path_opens_by_default() {
        let mut state = ChargePointState::new([1]);
        charging_connector(&mut state);

        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingStopped(StopReason::Local),
        );
        assert!(
            transaction(&state).is_some(),
            "asking to stop is not the default stop point"
        );

        let effects = apply_connector_event(&mut state, ConnectorEvent::ContactorOpened);

        assert!(transaction(&state).is_none());
        assert!(effects.iter().any(|effect| matches!(
            effect,
            ChargePointEffect::TransactionEvent(occurred)
                if occurred.kind == TransactionEventKind::Ended
        )));
    }

    /// `Authorized`: the earliest of the three. The stop request itself is the authorization
    /// ending, so the transaction closes before the contactor has even confirmed open - and the
    /// `stoppedReason` the request carried still reaches the CSMS.
    #[test]
    fn authorized_ends_the_transaction_the_moment_the_stop_is_requested() {
        let mut state = ChargePointState::new([1]);
        set_string(&mut state, "TxCtrlr", "TxStopPoint", "Authorized");
        charging_connector(&mut state);

        let effects = apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingStopped(StopReason::Remote),
        );

        assert!(transaction(&state).is_none());
        let ended = effects
            .iter()
            .find_map(|effect| match effect {
                ChargePointEffect::TransactionEvent(occurred)
                    if occurred.kind == TransactionEventKind::Ended =>
                {
                    Some(&occurred.transaction)
                }
                _ => None,
            })
            .expect("the stop request ends it under this stop point");
        assert_eq!(ended.stop_reason, Some(StopReason::Remote));

        // And the contactor confirming open afterwards must not end a second time.
        let effects = apply_connector_event(&mut state, ConnectorEvent::ContactorOpened);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, ChargePointEffect::TransactionEvent(_))),
            "the transaction already ended; settling must not report it twice"
        );
    }

    /// `EVConnected`: the transaction bills for the bay being occupied, so it survives the whole
    /// stop - contactor, unlock and all - and ends only when the cable actually leaves.
    #[test]
    fn ev_connected_keeps_the_transaction_open_until_the_cable_leaves() {
        let mut state = ChargePointState::new([1]);
        set_string(&mut state, "TxCtrlr", "TxStopPoint", "EVConnected");
        charging_connector(&mut state);

        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingStopped(StopReason::Local),
        );
        apply_connector_event(&mut state, ConnectorEvent::ContactorOpened);
        apply_connector_event(&mut state, ConnectorEvent::UnlockConfirmed);
        assert_eq!(state.evses[0].connectors[0], ConnectorState::Connected);
        assert!(
            transaction(&state).is_some(),
            "an unlocked but still-plugged connector is still occupied"
        );

        let effects = apply_connector_event(&mut state, ConnectorEvent::CableDisconnected);

        assert!(transaction(&state).is_none());
        let ended = effects
            .iter()
            .find_map(|effect| match effect {
                ChargePointEffect::TransactionEvent(occurred)
                    if occurred.kind == TransactionEventKind::Ended =>
                {
                    Some(&occurred.transaction)
                }
                _ => None,
            })
            .expect("the cable leaving ends it under this stop point");
        assert_eq!(
            ended.charging_state,
            crate::state::TransactionChargingState::Idle,
            "the bay is free, which is more than EVConnected"
        );
        assert_eq!(
            ended.stop_reason,
            Some(StopReason::Local),
            "the reason recorded when the stop began must outlive it"
        );
    }

    /// A stop point is a condition that *ceases* to hold, so a configured set resolves to its
    /// **earliest** member - the opposite of `TxStartPoint`, and the one thing about this
    /// variable worth getting wrong quietly.
    #[test]
    fn a_set_of_stop_points_resolves_to_the_earliest_of_them() {
        let mut state = ChargePointState::new([1]);
        set_string(
            &mut state,
            "TxCtrlr",
            "TxStopPoint",
            "EVConnected,Authorized",
        );
        charging_connector(&mut state);

        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingStopped(StopReason::Local),
        );

        assert!(
            transaction(&state).is_none(),
            "Authorized is the earliest member and must win"
        );
    }

    /// A fault ends the transaction whatever the stop point says: the connector is out of
    /// service, so nothing will ever observe a later condition lapsing.
    #[test]
    fn a_fault_ends_the_transaction_whatever_the_stop_point_is() {
        for stop_point in ["Authorized", "PowerPathClosed", "EVConnected"] {
            let mut state = ChargePointState::new([1]);
            set_string(&mut state, "TxCtrlr", "TxStopPoint", stop_point);
            charging_connector(&mut state);

            apply_connector_event(&mut state, ConnectorEvent::FaultDetected);

            assert!(transaction(&state).is_none(), "{stop_point}");
        }
    }

    /// CV1.5: the three required electrical variables are registered on every component whether
    /// or not the integrator declared anything - OCPP marks them required, so absent is a
    /// compliance failure while present-and-empty is an honest "this firmware was not told".
    #[test]
    fn the_required_electrical_variables_are_registered_even_when_nothing_is_declared() {
        let mut state = ChargePointState::new([1]);

        state.apply(ChargePointEvent::ElectricalCharacteristicsDeclared(
            alloc::boxed::Box::default(),
        ));

        for (component, evse, variable) in [
            ("ChargingStation", None, "SupplyPhases"),
            ("EVSE", Some((0, None)), "SupplyPhases"),
            ("EVSE", Some((0, None)), "Power"),
            ("Connector", Some((0, Some(0))), "SupplyPhases"),
            ("Connector", Some((0, Some(0))), "ConnectorType"),
        ] {
            let value = state
                .device_model
                .get(
                    &Component {
                        name: component.into(),
                        instance: None,
                        evse,
                    },
                    &crate::state::Variable {
                        name: variable.into(),
                        instance: None,
                    },
                )
                .and_then(|definition| {
                    definition.attribute(crate::state::VariableAttributeType::Actual)
                })
                .unwrap_or_else(|| panic!("{component}.{variable} must be registered"));
            assert_eq!(value.value, "", "{component}.{variable}");
            assert_eq!(value.mutability, crate::state::VariableMutability::ReadOnly);
        }
    }

    /// A declaration reaches the wire values - including OCPP's `0` for DC, which means "no
    /// alternating phases" rather than "no supply", and `EVSE.Power`'s required `maxLimit`
    /// characteristic rather than a live reading this crate does not have.
    #[test]
    fn a_declaration_populates_the_electrical_variables_including_dc_and_the_power_limit() {
        use crate::hardware::{
            ConnectorElectrical, ElectricalCharacteristics, EvseElectrical, SupplyPhases,
        };

        let mut state = ChargePointState::new([1]);
        state.apply(ChargePointEvent::ElectricalCharacteristicsDeclared(
            alloc::boxed::Box::new(ElectricalCharacteristics {
                supply_phases: SupplyPhases::ThreePhaseAc,
                evses: alloc::vec![EvseElectrical {
                    supply_phases: SupplyPhases::Dc,
                    max_power_w: Some(150_000.0),
                    max_discharge_power_w: None,
                    connectors: alloc::vec![ConnectorElectrical {
                        connector_type: "cCCS2".into(),
                        supply_phases: SupplyPhases::Dc,
                    }],
                }],
            }),
        ));

        let read = |component: &str, evse, variable: &str| {
            state
                .device_model
                .get(
                    &Component {
                        name: component.into(),
                        instance: None,
                        evse,
                    },
                    &crate::state::Variable {
                        name: variable.into(),
                        instance: None,
                    },
                )
                .cloned()
                .expect("registered")
        };

        assert_eq!(
            read("ChargingStation", None, "SupplyPhases")
                .attribute(crate::state::VariableAttributeType::Actual)
                .unwrap()
                .value,
            "3"
        );
        assert_eq!(
            read("EVSE", Some((0, None)), "SupplyPhases")
                .attribute(crate::state::VariableAttributeType::Actual)
                .unwrap()
                .value,
            "0",
            "DC is OCPP's 0, not an absent value"
        );
        assert_eq!(
            read("EVSE", Some((0, None)), "Power")
                .characteristics
                .max_limit,
            Some(150_000.0),
            "the appendix requires the limit, not the reading"
        );
        assert_eq!(
            read("Connector", Some((0, Some(0))), "ConnectorType")
                .attribute(crate::state::VariableAttributeType::Actual)
                .unwrap()
                .value,
            "cCCS2"
        );
    }

    /// `EVSE.DischargePower` is `Required? = V2X` in the appendix - required of a station that can
    /// discharge, absent from one that cannot. So it follows the capability rather than being
    /// registered unconditionally: advertising a discharge figure on a charge-only station would
    /// claim hardware that is not there.
    #[test]
    fn discharge_power_is_registered_only_on_a_station_that_can_discharge() {
        use crate::hardware::{Capabilities, ElectricalCharacteristics, EvseElectrical};

        let discharge_power = |state: &ChargePointState| {
            state
                .device_model
                .get(
                    &Component {
                        name: "EVSE".into(),
                        instance: None,
                        evse: Some((0, None)),
                    },
                    &crate::state::Variable {
                        name: "DischargePower".into(),
                        instance: None,
                    },
                )
                .cloned()
        };

        let mut charge_only = ChargePointState::new([1]);
        charge_only.apply(ChargePointEvent::ElectricalCharacteristicsDeclared(
            alloc::boxed::Box::default(),
        ));
        assert!(
            discharge_power(&charge_only).is_none(),
            "a charge-only station must not advertise a discharge figure"
        );

        let mut bidirectional = ChargePointState::new([1]);
        bidirectional.apply(ChargePointEvent::CapabilitiesDeclared(Capabilities {
            supports_bidirectional_power: true,
            ..Default::default()
        }));
        bidirectional.apply(ChargePointEvent::ElectricalCharacteristicsDeclared(
            alloc::boxed::Box::new(ElectricalCharacteristics {
                evses: alloc::vec![EvseElectrical {
                    max_discharge_power_w: Some(11_000.0),
                    ..Default::default()
                }],
                ..Default::default()
            }),
        ));

        assert_eq!(
            discharge_power(&bidirectional)
                .expect("a V2X station owes this variable")
                .characteristics
                .max_limit,
            Some(11_000.0),
            "the nameplate figure is the limit, like EVSE.Power's"
        );
    }

    // --- CV2.5: E05, an identifier revoked mid-session ---

    /// The default: the CSMS refuses the identifier and energy stops at once. Anything else would
    /// be handing out energy nobody will be billed for.
    #[test]
    fn a_revoked_identifier_stops_the_transaction_at_once_by_default() {
        let mut state = ChargePointState::new([1]);
        charging_connector(&mut state);

        let effects = apply_connector_event(&mut state, ConnectorEvent::AuthorizationRevoked);

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Stopping);
        assert!(effects.contains(&ChargePointEffect::HardwareCommand(
            crate::state::HardwareCommand::OpenContactor {
                evse_id: 0,
                connector_id: 0,
            }
        )));
    }

    /// With `StopTxOnInvalidId` off and an allowance configured, the driver gets that much more
    /// energy and then stops - which is the point of the setting: a CSMS blocklist update should
    /// not strand someone mid-journey.
    #[test]
    fn a_revoked_identifier_gets_the_configured_allowance_before_stopping() {
        let mut state = ChargePointState::new([1]);
        set_boolean(&mut state, "TxCtrlr", "StopTxOnInvalidId", "false");
        set_string(&mut state, "TxCtrlr", "MaxEnergyOnInvalidId", "500");
        charging_connector(&mut state);
        apply_connector_event(&mut state, ConnectorEvent::MeterValueSampled(sample(1_000)));

        apply_connector_event(&mut state, ConnectorEvent::AuthorizationRevoked);

        assert_eq!(
            state.evses[0].connectors[0],
            ConnectorState::Charging,
            "the allowance means charging continues for now"
        );
        assert_eq!(
            state.evses[0].transactions[0]
                .as_ref()
                .and_then(|transaction| transaction.stop_at_energy_wh),
            Some(1_500),
            "an absolute target, so a dropped or duplicated sample cannot change the allowance"
        );

        // Still inside the allowance.
        apply_connector_event(&mut state, ConnectorEvent::MeterValueSampled(sample(1_400)));
        assert_eq!(state.evses[0].connectors[0], ConnectorState::Charging);

        // And now it is spent.
        apply_connector_event(&mut state, ConnectorEvent::MeterValueSampled(sample(1_500)));
        assert_eq!(state.evses[0].connectors[0], ConnectorState::Stopping);
        assert_eq!(
            state.evses[0].transactions[0]
                .as_ref()
                .and_then(|transaction| transaction.stop_reason),
            Some(crate::state::StopReason::DeAuthorized)
        );
    }

    /// `StopTxOnInvalidId` off but no allowance configured is not "charge forever" - there is
    /// nothing to grant, so it stops like the default.
    #[test]
    fn a_revoked_identifier_with_no_allowance_configured_still_stops() {
        let mut state = ChargePointState::new([1]);
        set_boolean(&mut state, "TxCtrlr", "StopTxOnInvalidId", "false");
        charging_connector(&mut state);

        apply_connector_event(&mut state, ConnectorEvent::AuthorizationRevoked);

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Stopping);
    }

    // --- CV11: G05, Lock Failure ---

    /// Connector 0's `ConnectorPlugRetentionLock`/`Problem` value.
    fn lock_problem(state: &ChargePointState) -> Option<alloc::string::String> {
        state
            .device_model
            .get(
                &Component {
                    name: "ConnectorPlugRetentionLock".into(),
                    instance: None,
                    evse: Some((0, Some(0))),
                },
                &crate::state::Variable {
                    name: "Problem".into(),
                    instance: None,
                },
            )
            .and_then(|definition| {
                definition.attribute(crate::state::VariableAttributeType::Actual)
            })
            .map(|attribute| attribute.value.clone())
    }

    /// **G05.FR.01/.02**: the lock will not engage, so the connector faults fail-safe *and* the
    /// CSMS is told which lock, by name - the whole point of CV11 being distinct from a fault.
    #[test]
    fn a_lock_failure_faults_the_connector_and_names_the_lock() {
        let mut state = ChargePointState::new([1]);
        assert_eq!(lock_problem(&state).as_deref(), Some("false"));

        apply_connector_event(&mut state, ConnectorEvent::CableConnected);
        let effects = apply_connector_event(&mut state, ConnectorEvent::LockFailed);

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Faulted);
        assert!(
            effects.contains(&ChargePointEffect::HardwareCommand(
                crate::state::HardwareCommand::OpenContactor {
                    evse_id: 0,
                    connector_id: 0,
                }
            )),
            "G05.FR.01: nothing may charge through a lock that did not engage"
        );
        assert_eq!(lock_problem(&state).as_deref(), Some("true"));

        let reported = effects
            .iter()
            .find_map(|effect| match effect {
                ChargePointEffect::VariableMonitorTriggered(triggered) => Some(triggered),
                _ => None,
            })
            .expect("G05.FR.02: a NotifyEvent must go out");
        assert_eq!(
            reported.monitor_id, None,
            "hard-wired: nobody configured this, so MonitoringLevel must not suppress it"
        );
        assert_eq!(reported.component.name, "ConnectorPlugRetentionLock");
        assert_eq!(reported.component.evse, Some((0, Some(0))));
        assert_eq!(reported.variable.name, "Problem");
        assert_eq!(reported.actual_value, "true");
    }

    /// An ordinary fault must stay ordinary: reporting every stuck contactor as a lock failure
    /// would make the distinction CV11 exists for worthless.
    #[test]
    fn an_ordinary_fault_does_not_claim_the_lock_failed() {
        let mut state = ChargePointState::new([1]);

        let effects = apply_connector_event(&mut state, ConnectorEvent::FaultDetected);

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Faulted);
        assert_eq!(lock_problem(&state).as_deref(), Some("false"));
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, ChargePointEffect::VariableMonitorTriggered(_)))
        );
    }

    /// The problem clears when the connector is usable again - the far side of the fail-safe
    /// recovery, by which point the lock has demonstrably moved. Silently: a station recovering
    /// is not news, and the value is readable by any CSMS that asks.
    #[test]
    fn the_lock_problem_clears_once_the_connector_recovers() {
        let mut state = ChargePointState::new([1]);
        apply_connector_event(&mut state, ConnectorEvent::CableConnected);
        apply_connector_event(&mut state, ConnectorEvent::LockFailed);
        apply_connector_event(&mut state, ConnectorEvent::ContactorOpened);
        apply_connector_event(&mut state, ConnectorEvent::FaultCleared);
        assert_eq!(
            lock_problem(&state).as_deref(),
            Some("true"),
            "still unlocking; the lock has not moved yet"
        );

        let effects = apply_connector_event(&mut state, ConnectorEvent::UnlockConfirmed);

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Available);
        assert_eq!(lock_problem(&state).as_deref(), Some("false"));
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, ChargePointEffect::VariableMonitorTriggered(_)))
        );
    }

    fn sample(energy_wh: i64) -> crate::state::MeterSample {
        crate::state::MeterSample {
            energy_wh,
            power_w: None,
            current_ma: None,
            voltage_v: None,
            soc_percent: None,
        }
    }

    // --- CV2.4: E09 vs E10, the suspend-or-stop branch on EV-side disconnect ---

    /// Drives a connector to `Charging` so the disconnect tests below start from energy flowing.
    fn charging_connector(state: &mut ChargePointState) {
        apply_connector_event(state, ConnectorEvent::CableConnected);
        apply_connector_event(state, ConnectorEvent::LockConfirmed);
        apply_connector_event(state, ConnectorEvent::IdTokenPresented(test_id_token()));
        apply_connector_event(state, ConnectorEvent::ChargingAuthorized(test_id_token()));
        apply_connector_event(state, ConnectorEvent::ContactorClosed);
        assert_eq!(state.evses[0].connectors[0], ConnectorState::Charging);
    }

    fn set_boolean(state: &mut ChargePointState, component: &str, variable: &str, value: &str) {
        state.apply(ChargePointEvent::DeviceModel(
            crate::state::DeviceModelEvent::AttributeValueSet {
                component: Component {
                    name: component.into(),
                    instance: None,
                    evse: None,
                },
                variable: crate::state::Variable {
                    name: variable.into(),
                    instance: None,
                },
                attribute_type: crate::state::VariableAttributeType::Actual,
                value: value.into(),
            },
        ));
    }

    /// **E09**, the default: the cable leaving the EV ends the transaction, and it ends the
    /// fail-safe way - contactor open first, exactly as any other stop.
    #[test]
    fn an_ev_side_disconnect_stops_the_transaction_by_default() {
        let mut state = ChargePointState::new([1]);
        charging_connector(&mut state);

        let effects = apply_connector_event(&mut state, ConnectorEvent::CableDisconnected);

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Stopping);
        assert!(effects.contains(&ChargePointEffect::HardwareCommand(
            crate::state::HardwareCommand::OpenContactor {
                evse_id: 0,
                connector_id: 0,
            }
        )));
    }

    /// **E10**: with `StopTxOnEVSideDisconnect` off, the same physical event suspends instead.
    /// The transaction stays open and the connector can resume - which is what a driver
    /// reseating a connector expects, and the whole reason OCPP makes this configurable.
    #[test]
    fn an_ev_side_disconnect_suspends_instead_when_the_operator_configured_it_that_way() {
        let mut state = ChargePointState::new([1]);
        charging_connector(&mut state);
        set_boolean(&mut state, "TxCtrlr", "StopTxOnEVSideDisconnect", "false");

        apply_connector_event(&mut state, ConnectorEvent::CableDisconnected);

        assert_eq!(state.evses[0].connectors[0], ConnectorState::SuspendedEv);
        assert!(
            state.evses[0].transactions[0].is_some(),
            "E10 suspends the transaction; it must not have ended"
        );

        apply_connector_event(&mut state, ConnectorEvent::ChargingResumed);
        assert_eq!(state.evses[0].connectors[0], ConnectorState::Charging);
    }

    /// **E09.FR.02**, the default: the station releases its own end too, so the driver can take
    /// the cable away.
    #[test]
    fn an_ev_side_disconnect_releases_the_station_end_by_default() {
        let mut state = ChargePointState::new([1]);
        charging_connector(&mut state);

        apply_connector_event(&mut state, ConnectorEvent::CableDisconnected);
        let effects = apply_connector_event(&mut state, ConnectorEvent::ContactorOpened);

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Finishing);
        assert!(effects.contains(&ChargePointEffect::HardwareCommand(
            crate::state::HardwareCommand::UnlockConnector {
                evse_id: 0,
                connector_id: 0,
            }
        )));
    }

    /// **E09.FR.03**: with `UnlockOnEVSideDisconnect` off the station keeps hold of the cable.
    /// The stop is otherwise identical - contactor first - and settles to `Locked`, where the
    /// driver's next identifier (or a CSMS `UnlockConnector`) is what releases it.
    #[test]
    fn an_ev_side_disconnect_keeps_the_cable_when_the_operator_configured_it_that_way() {
        let mut state = ChargePointState::new([1]);
        charging_connector(&mut state);
        set_boolean(
            &mut state,
            "OCPPCommCtrlr",
            "UnlockOnEVSideDisconnect",
            "false",
        );

        let effects = apply_connector_event(&mut state, ConnectorEvent::CableDisconnected);
        assert_eq!(state.evses[0].connectors[0], ConnectorState::StoppingLocked);
        assert!(
            effects.contains(&ChargePointEffect::HardwareCommand(
                crate::state::HardwareCommand::OpenContactor {
                    evse_id: 0,
                    connector_id: 0,
                }
            )),
            "the fail-safe ordering is not negotiable just because the cable stays locked"
        );

        let effects = apply_connector_event(&mut state, ConnectorEvent::ContactorOpened);

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Locked);
        assert!(
            !effects.iter().any(|effect| matches!(
                effect,
                ChargePointEffect::HardwareCommand(
                    crate::state::HardwareCommand::UnlockConnector { .. }
                )
            )),
            "E09.FR.03 is exactly the requirement not to unlock here"
        );
        assert!(
            state.evses[0].transactions[0].is_none(),
            "the transaction still ends - only the cable is retained"
        );
    }

    /// The retained-cable stop is still a stop: whatever the unlock setting, the transaction
    /// ends at the same point in the stop and reports the same thing. Only the cable differs.
    #[test]
    fn a_retained_cable_stop_reports_the_same_ended_event_an_ordinary_one_does() {
        fn ended_transaction(retain_cable: bool) -> crate::state::Transaction {
            let mut state = ChargePointState::new([1]);
            charging_connector(&mut state);
            if retain_cable {
                set_boolean(
                    &mut state,
                    "OCPPCommCtrlr",
                    "UnlockOnEVSideDisconnect",
                    "false",
                );
            }
            apply_connector_event(&mut state, ConnectorEvent::CableDisconnected);
            apply_connector_event(&mut state, ConnectorEvent::ContactorOpened)
                .into_iter()
                .find_map(|effect| match effect {
                    ChargePointEffect::TransactionEvent(occurred)
                        if occurred.kind == TransactionEventKind::Ended =>
                    {
                        Some(occurred.transaction)
                    }
                    _ => None,
                })
                .expect("the contactor opening ends it under the default stop point")
        }

        assert_eq!(ended_transaction(true), ended_transaction(false));
    }

    /// E10 wins over E09's unlock setting: with the transaction suspended rather than stopped
    /// there is nothing to unlock from, and OCPP calls the other combination undefined.
    #[test]
    fn the_unlock_setting_does_not_apply_when_the_disconnect_only_suspends() {
        let mut state = ChargePointState::new([1]);
        charging_connector(&mut state);
        set_boolean(&mut state, "TxCtrlr", "StopTxOnEVSideDisconnect", "false");
        set_boolean(
            &mut state,
            "OCPPCommCtrlr",
            "UnlockOnEVSideDisconnect",
            "false",
        );

        apply_connector_event(&mut state, ConnectorEvent::CableDisconnected);

        assert_eq!(state.evses[0].connectors[0], ConnectorState::SuspendedEv);
    }

    /// CV6: F02.FR.06/H03 - a transaction that consumes a reservation carries its id, so the CSMS
    /// can close the reservation out against the session that used it.
    ///
    /// Captured *before* the transition, deliberately: entering `Starting` is what ends the
    /// reservation, so a lookup after the fact would find nothing.
    #[test]
    fn a_transaction_started_on_a_reserved_connector_records_the_reservation_it_consumed() {
        let mut state = ChargePointState::new([1]);
        apply_connector_event(&mut state, ConnectorEvent::Reserved(reservation(9)));
        apply_connector_event(&mut state, ConnectorEvent::CableConnected);
        apply_connector_event(&mut state, ConnectorEvent::LockConfirmed);
        apply_connector_event(
            &mut state,
            ConnectorEvent::IdTokenPresented(test_id_token()),
        );

        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
        assert!(
            state.evses[0].transactions[0].is_some(),
            "the transaction this test is about must actually have started"
        );

        assert_eq!(
            state.evses[0].transactions[0]
                .as_ref()
                .and_then(|transaction| transaction.reservation_id),
            Some(9)
        );
    }

    /// A transaction on a connector nobody reserved carries none.
    #[test]
    fn a_transaction_on_an_unreserved_connector_records_no_reservation() {
        let mut state = ChargePointState::new([1]);
        apply_connector_event(&mut state, ConnectorEvent::CableConnected);
        apply_connector_event(&mut state, ConnectorEvent::LockConfirmed);
        apply_connector_event(
            &mut state,
            ConnectorEvent::IdTokenPresented(test_id_token()),
        );

        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
        assert!(
            state.evses[0].transactions[0].is_some(),
            "the transaction this test is about must actually have started"
        );

        assert_eq!(
            state.evses[0].transactions[0]
                .as_ref()
                .and_then(|transaction| transaction.reservation_id),
            None
        );
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
    fn an_expired_reservation_frees_the_connector_and_tells_the_csms() {
        let mut state = ChargePointState::new([1]);
        apply_connector_event(&mut state, ConnectorEvent::Reserved(reservation(1)));

        let effects = apply_connector_event(&mut state, ConnectorEvent::ReservationExpired);

        assert_eq!(state.evses[0].connectors[0], ConnectorState::Available);
        assert_eq!(state.evses[0].reservations[0], None);
        assert!(effects.contains(&ChargePointEffect::ReservationEnded(
            crate::state::ReservationUpdate {
                id: crate::state::ReservationId(1),
                reason: crate::state::ReservationEndReason::Expired,
            }
        )));
    }

    #[test]
    fn a_reserved_connector_that_faults_reports_the_reservation_removed() {
        let mut state = ChargePointState::new([1]);
        apply_connector_event(&mut state, ConnectorEvent::Reserved(reservation(1)));

        let effects = apply_connector_event(&mut state, ConnectorEvent::FaultDetected);

        // The charge point can no longer honour the reservation, and the CSMS is still holding
        // this connector for a driver unless it is told otherwise.
        assert!(effects.contains(&ChargePointEffect::ReservationEnded(
            crate::state::ReservationUpdate {
                id: crate::state::ReservationId(1),
                reason: crate::state::ReservationEndReason::Removed,
            }
        )));
    }

    #[test]
    fn a_reservation_that_is_honoured_or_cancelled_is_not_reported_as_ended() {
        let mut honoured = ChargePointState::new([1]);
        apply_connector_event(&mut honoured, ConnectorEvent::Reserved(reservation(1)));
        let effects = apply_connector_event(&mut honoured, ConnectorEvent::CableConnected);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, ChargePointEffect::ReservationEnded(_))),
            "a cable arriving is the reservation doing its job, not failing"
        );

        let mut cancelled = ChargePointState::new([1]);
        apply_connector_event(&mut cancelled, ConnectorEvent::Reserved(reservation(1)));
        let effects = apply_connector_event(&mut cancelled, ConnectorEvent::ReservationCancelled);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, ChargePointEffect::ReservationEnded(_))),
            "the CSMS asked for this one; reporting it back is noise"
        );
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

    fn test_tariff(id: &str) -> crate::state::Tariff {
        crate::state::Tariff::new(crate::state::TariffId(id.into()), "EUR")
    }

    #[test]
    fn a_default_tariff_is_installed_at_its_scope() {
        let mut state = ChargePointState::new([1]);

        let effects = state.apply(ChargePointEvent::DefaultTariffSet {
            scope: crate::state::TariffScope::Evse(0),
            tariff: alloc::boxed::Box::new(test_tariff("t1")),
        });

        assert!(effects.contains(&ChargePointEffect::StateChanged));
        assert_eq!(state.tariffs.len(), 1);
        assert_eq!(
            state.tariffs.installed()[0].tariff.id,
            crate::state::TariffId("t1".into())
        );
    }

    #[test]
    fn a_default_tariff_the_store_refuses_is_dropped_rather_than_installed() {
        let mut state =
            ChargePointState::with_limits([1], StateLimits::default().with_max_tariffs(1));
        state.apply(ChargePointEvent::DefaultTariffSet {
            scope: crate::state::TariffScope::Evse(0),
            tariff: alloc::boxed::Box::new(test_tariff("t1")),
        });

        let effects = state.apply(ChargePointEvent::DefaultTariffSet {
            scope: crate::state::TariffScope::Evse(1),
            tariff: alloc::boxed::Box::new(test_tariff("t2")),
        });

        assert!(!effects.contains(&ChargePointEffect::StateChanged));
        assert_eq!(state.tariffs.len(), 1);
    }

    #[test]
    fn tariffs_cleared_removes_matching_tariffs() {
        let mut state = ChargePointState::new([1]);
        state.apply(ChargePointEvent::DefaultTariffSet {
            scope: crate::state::TariffScope::Evse(0),
            tariff: alloc::boxed::Box::new(test_tariff("t1")),
        });

        let effects = state.apply(ChargePointEvent::TariffsCleared {
            criteria: crate::state::TariffClearCriteria::default(),
        });

        assert!(effects.contains(&ChargePointEffect::StateChanged));
        assert!(state.tariffs.is_empty());
    }

    #[test]
    fn a_transaction_tariff_is_recorded_while_a_transaction_is_active() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );

        apply_connector_event(
            &mut state,
            ConnectorEvent::TariffAssigned(alloc::boxed::Box::new(test_tariff("t1"))),
        );

        assert_eq!(
            state.evses[0].transaction_tariffs[0],
            Some(test_tariff("t1"))
        );
    }

    #[test]
    fn a_transaction_tariff_with_no_active_transaction_is_ignored() {
        let mut state = ChargePointState::new([1]);

        apply_connector_event(
            &mut state,
            ConnectorEvent::TariffAssigned(alloc::boxed::Box::new(test_tariff("t1"))),
        );

        assert_eq!(state.evses[0].transaction_tariffs[0], None);
    }

    #[test]
    fn a_new_transaction_does_not_inherit_the_previous_ones_tariff() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
        apply_connector_event(&mut state, ConnectorEvent::ContactorClosed);
        apply_connector_event(
            &mut state,
            ConnectorEvent::TariffAssigned(alloc::boxed::Box::new(test_tariff("t1"))),
        );
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingStopped(StopReason::Local),
        );
        apply_connector_event(&mut state, ConnectorEvent::ContactorOpened);
        assert_eq!(state.evses[0].transaction_tariffs[0], None);

        apply_connector_event(&mut state, ConnectorEvent::UnlockConfirmed);
        apply_connector_event(&mut state, ConnectorEvent::CableDisconnected);
        plug_in_and_authorize(&mut state);
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );

        assert_eq!(state.evses[0].transaction_tariffs[0], None);
    }

    fn test_running_cost() -> crate::pricing::TransactionCost {
        let now = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        crate::pricing::TransactionCost::start(
            &test_tariff("t1"),
            &crate::pricing::PricingContext::new(now),
            None,
        )
    }

    #[test]
    fn a_running_cost_is_recorded_while_a_transaction_is_active() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );

        apply_connector_event(
            &mut state,
            ConnectorEvent::RunningCostAdvanced {
                cost: alloc::boxed::Box::new(test_running_cost()),
                total: 0.0,
            },
        );

        assert_eq!(state.evses[0].running_cost[0], Some(test_running_cost()));
    }

    #[test]
    fn a_running_cost_with_no_active_transaction_is_ignored() {
        let mut state = ChargePointState::new([1]);

        apply_connector_event(
            &mut state,
            ConnectorEvent::RunningCostAdvanced {
                cost: alloc::boxed::Box::new(test_running_cost()),
                total: 0.0,
            },
        );

        assert_eq!(state.evses[0].running_cost[0], None);
    }

    #[test]
    fn a_new_transaction_does_not_inherit_the_previous_ones_running_cost() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
        apply_connector_event(&mut state, ConnectorEvent::ContactorClosed);
        apply_connector_event(
            &mut state,
            ConnectorEvent::RunningCostAdvanced {
                cost: alloc::boxed::Box::new(test_running_cost()),
                total: 0.0,
            },
        );
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingStopped(StopReason::Local),
        );
        apply_connector_event(&mut state, ConnectorEvent::ContactorOpened);
        assert_eq!(state.evses[0].running_cost[0], None);

        apply_connector_event(&mut state, ConnectorEvent::UnlockConfirmed);
        apply_connector_event(&mut state, ConnectorEvent::CableDisconnected);
        plug_in_and_authorize(&mut state);
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );

        assert_eq!(state.evses[0].running_cost[0], None);
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
            dyn_update_interval_secs: None,
            dyn_update_time: None,
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
    fn priority_charging_is_granted_to_a_named_transaction_and_ends_with_it() {
        let mut state = ChargePointState::new([1]);
        apply_connector_event(&mut state, ConnectorEvent::CableConnected);
        apply_connector_event(&mut state, ConnectorEvent::LockConfirmed);
        apply_connector_event(
            &mut state,
            ConnectorEvent::RemoteStartRequested {
                id_token: test_id_token(),
                remote_start_id: None,
            },
        );
        let running = state.evses[0].transactions[0].as_ref().unwrap().id;
        assert!(
            !state.evses[0].transactions[0]
                .as_ref()
                .unwrap()
                .priority_charging
        );

        let effects = state.apply(ChargePointEvent::PriorityChargingSet {
            transaction_id: running,
            activated: true,
            locally_initiated: false,
        });
        assert!(effects.contains(&ChargePointEffect::StateChanged));
        assert!(
            state.evses[0].transactions[0]
                .as_ref()
                .unwrap()
                .priority_charging
        );
        // The CSMS asked for this one, so it is not told about it again.
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, ChargePointEffect::PriorityChargingChanged(_)))
        );

        // Re-granting what is already granted changes nothing, so nothing is re-reported.
        let effects = state.apply(ChargePointEvent::PriorityChargingSet {
            transaction_id: running,
            activated: true,
            locally_initiated: false,
        });
        assert!(!effects.contains(&ChargePointEffect::StateChanged));

        // A grant for a transaction that isn't running is dropped rather than applied to whatever
        // happens to be on the connector.
        let effects = state.apply(ChargePointEvent::PriorityChargingSet {
            transaction_id: TransactionId(999),
            activated: true,
            locally_initiated: false,
        });
        assert!(!effects.contains(&ChargePointEffect::StateChanged));
    }

    #[test]
    fn priority_charging_the_charge_point_granted_itself_is_reported_to_the_csms() {
        let mut state = ChargePointState::new([1]);
        apply_connector_event(&mut state, ConnectorEvent::CableConnected);
        apply_connector_event(&mut state, ConnectorEvent::LockConfirmed);
        apply_connector_event(
            &mut state,
            ConnectorEvent::RemoteStartRequested {
                id_token: test_id_token(),
                remote_start_id: None,
            },
        );
        let running = state.evses[0].transactions[0].as_ref().unwrap().id;

        let effects = state.apply(ChargePointEvent::PriorityChargingSet {
            transaction_id: running,
            activated: true,
            locally_initiated: true,
        });

        assert!(
            effects.contains(&ChargePointEffect::PriorityChargingChanged(
                crate::state::PriorityChargingChange {
                    transaction_id: running,
                    activated: true,
                }
            ))
        );
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
            ConnectorEvent::CurrentLimitComputed {
                limit_ma: Some(16_000),
                externally_caused: false,
            },
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
            ConnectorEvent::CurrentLimitComputed {
                limit_ma: Some(16_000),
                externally_caused: false,
            },
        );
        assert!(effects.is_empty());

        // Dropping the limit entirely is a real change, and is dispatched as such - hardware
        // has to be told to stop limiting, not left holding the last value forever.
        let effects = apply_connector_event(
            &mut state,
            ConnectorEvent::CurrentLimitComputed {
                limit_ma: None,
                externally_caused: false,
            },
        );
        assert!(effects.contains(&ChargePointEffect::HardwareCommand(
            HardwareCommand::SetCurrentLimit {
                evse_id: 0,
                connector_id: 0,
                limit_ma: None,
            }
        )));
        assert_eq!(state.evses[0].charging_limits[0], None);
    }

    /// K11.FR.04/K13.FR.03 (CV18): a rate change an *external* control system caused, on a
    /// connector with a transaction running, is a `SHALL`-report to the CSMS. Reachable only since
    /// CV13 made an external limit change the rate at all.
    #[test]
    fn an_externally_caused_rate_change_reports_a_transaction_event() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
        apply_connector_event(&mut state, ConnectorEvent::ContactorClosed);

        let effects = apply_connector_event(
            &mut state,
            ConnectorEvent::CurrentLimitComputed {
                limit_ma: Some(6_000),
                externally_caused: true,
            },
        );

        let reported = effects.iter().find_map(|effect| match effect {
            ChargePointEffect::TransactionEvent(occurred) => Some(occurred),
            _ => None,
        });
        let reported = reported.expect("an externally caused rate change is reported");
        assert_eq!(
            reported.kind,
            TransactionEventKind::Updated(TransactionUpdateReason::ChargingRateChanged)
        );
        // The sequence number moves with every event about a transaction, as OCPP requires.
        assert_eq!(reported.transaction.seq_no, 2);
    }

    /// K01.FR.61 makes the CSMS-caused case a `MAY`, and this crate does not: a schedule period
    /// boundary in a profile the CSMS installed changes the rate on a cadence the CSMS already
    /// knows, and reporting each one would be traffic it can derive.
    #[test]
    fn a_rate_change_the_csms_caused_reports_nothing() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
        apply_connector_event(&mut state, ConnectorEvent::ContactorClosed);

        let effects = apply_connector_event(
            &mut state,
            ConnectorEvent::CurrentLimitComputed {
                limit_ma: Some(6_000),
                externally_caused: false,
            },
        );

        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, ChargePointEffect::TransactionEvent(_)))
        );
    }

    /// The other precondition both requirements state: *a transaction is ongoing*. An external
    /// limit arriving at an idle connector still changes the limit and still reaches hardware -
    /// there is simply no transaction to report it against.
    #[test]
    fn an_externally_caused_rate_change_with_no_transaction_reports_nothing() {
        let mut state = ChargePointState::new([1]);

        let effects = apply_connector_event(
            &mut state,
            ConnectorEvent::CurrentLimitComputed {
                limit_ma: Some(6_000),
                externally_caused: true,
            },
        );

        assert!(effects.contains(&ChargePointEffect::HardwareCommand(
            HardwareCommand::SetCurrentLimit {
                evse_id: 0,
                connector_id: 0,
                limit_ma: Some(6_000),
            }
        )));
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, ChargePointEffect::TransactionEvent(_)))
        );
    }

    /// "Changed by more than `LimitChangeSignificance`" - which this build registers as 0 and does
    /// not honour a write to (CV14), so *any* change qualifies and no change qualifies for
    /// nothing. Re-issuing the same limit is not a rate change, whatever caused it.
    #[test]
    fn an_unchanged_limit_reports_nothing_even_when_externally_caused() {
        let mut state = ChargePointState::new([1]);
        plug_in_and_authorize(&mut state);
        apply_connector_event(
            &mut state,
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        );
        apply_connector_event(&mut state, ConnectorEvent::ContactorClosed);
        apply_connector_event(
            &mut state,
            ConnectorEvent::CurrentLimitComputed {
                limit_ma: Some(6_000),
                externally_caused: true,
            },
        );

        let effects = apply_connector_event(
            &mut state,
            ConnectorEvent::CurrentLimitComputed {
                limit_ma: Some(6_000),
                externally_caused: true,
            },
        );

        assert!(effects.is_empty());
    }

    #[test]
    fn a_confirmed_current_limit_is_recorded_separately_from_the_requested_one() {
        let mut state = ChargePointState::new([1]);

        apply_connector_event(
            &mut state,
            ConnectorEvent::CurrentLimitComputed {
                limit_ma: Some(16_000),
                externally_caused: false,
            },
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
            priority_charging: false,
            remote_start_id: None,
            reservation_id: None,
            stop_at_energy_wh: None,
            limit: None,
            csms_limit: None,
            limit_reached: None,
            energy_start_wh: None,
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
                offline: false,
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

    /// B5.5 (docs/PRODUCTION-ROADMAP.md B5): `CustomerInformationErased` is the real state
    /// mutation behind `CustomerInformation`'s `clear` job - it must reach both stores that key
    /// on an `IdToken`, not just one.
    #[test]
    fn customer_information_erased_forgets_the_token_from_both_the_cache_and_the_local_list() {
        let mut state = ChargePointState::new([1]);
        let token = test_id_token();
        state.authorization_cache.remember(
            token.clone(),
            crate::state::AuthorizationStatus::Accepted,
            None,
        );
        state.apply(ChargePointEvent::LocalListUpdated {
            version: 1,
            entries: alloc::vec![crate::state::LocalListEntry {
                id_token: token.clone(),
                status: crate::state::AuthorizationStatus::Accepted,
            }],
        });

        let effects = state.apply(ChargePointEvent::CustomerInformationErased {
            id_token: token.clone(),
        });

        assert!(state.authorization_cache.entries().is_empty());
        assert!(state.local_authorization_list.entries.is_empty());
        assert!(effects.contains(&ChargePointEffect::StateChanged));
    }

    #[test]
    fn customer_information_erased_for_an_unheld_token_reports_no_change() {
        let mut state = ChargePointState::new([1]);

        let effects = state.apply(ChargePointEvent::CustomerInformationErased {
            id_token: test_id_token(),
        });

        assert!(effects.is_empty());
    }

    fn ems_limit() -> crate::state::ExternalChargingLimit {
        crate::state::ExternalChargingLimit {
            is_local_generation: false,
            source: crate::state::ChargingLimitSource::Ems,
            is_grid_critical: Some(true),
            schedule: None,
        }
    }

    #[test]
    fn an_external_charging_limit_on_a_real_evse_is_recorded_and_reported() {
        let mut state = ChargePointState::new([1]);

        let effects = state.apply(ChargePointEvent::ExternalChargingLimitSet {
            evse_id: Some(0),
            limit: ems_limit(),
        });

        assert_eq!(state.evses[0].external_charging_limit, Some(ems_limit()));
        assert!(
            effects.contains(&ChargePointEffect::SmartChargingNotification(
                crate::state::SmartChargingNotification::ExternalChargingLimitSet {
                    evse_id: Some(0),
                    limit: ems_limit(),
                }
            ))
        );
    }

    #[test]
    fn an_external_charging_limit_with_no_evse_id_applies_station_wide() {
        let mut state = ChargePointState::new([1]);

        state.apply(ChargePointEvent::ExternalChargingLimitSet {
            evse_id: None,
            limit: ems_limit(),
        });

        assert_eq!(state.station_external_charging_limit, Some(ems_limit()));
        // A station-wide limit must not be misattributed to the one EVSE that happens to exist.
        assert_eq!(state.evses[0].external_charging_limit, None);
    }

    #[test]
    fn an_external_charging_limit_for_an_unknown_evse_is_dropped() {
        let mut state = ChargePointState::new([1]);

        let effects = state.apply(ChargePointEvent::ExternalChargingLimitSet {
            evse_id: Some(5),
            limit: ems_limit(),
        });

        assert!(effects.is_empty());
        assert_eq!(state.evses[0].external_charging_limit, None);
    }

    #[test]
    fn clearing_a_matching_external_charging_limit_removes_it_and_reports_it() {
        let mut state = ChargePointState::new([1]);
        state.apply(ChargePointEvent::ExternalChargingLimitSet {
            evse_id: Some(0),
            limit: ems_limit(),
        });

        let effects = state.apply(ChargePointEvent::ExternalChargingLimitCleared {
            is_local_generation: false,
            evse_id: Some(0),
            source: crate::state::ChargingLimitSource::Ems,
        });

        assert_eq!(state.evses[0].external_charging_limit, None);
        assert!(
            effects.contains(&ChargePointEffect::SmartChargingNotification(
                crate::state::SmartChargingNotification::ExternalChargingLimitCleared {
                    evse_id: Some(0),
                    source: crate::state::ChargingLimitSource::Ems,
                }
            ))
        );
    }

    #[test]
    fn clearing_an_external_charging_limit_with_the_wrong_source_is_a_no_op() {
        let mut state = ChargePointState::new([1]);
        state.apply(ChargePointEvent::ExternalChargingLimitSet {
            evse_id: Some(0),
            limit: ems_limit(),
        });

        let effects = state.apply(ChargePointEvent::ExternalChargingLimitCleared {
            is_local_generation: false,
            evse_id: Some(0),
            source: crate::state::ChargingLimitSource::Other,
        });

        assert!(effects.is_empty());
        assert_eq!(state.evses[0].external_charging_limit, Some(ems_limit()));
    }

    #[test]
    fn clearing_a_limit_that_was_never_set_is_a_no_op() {
        let mut state = ChargePointState::new([1]);

        let effects = state.apply(ChargePointEvent::ExternalChargingLimitCleared {
            is_local_generation: false,
            evse_id: Some(0),
            source: crate::state::ChargingLimitSource::Ems,
        });

        assert!(effects.is_empty());
    }

    #[test]
    fn ev_charging_needs_reported_on_a_real_evse_is_forwarded() {
        let mut state = ChargePointState::new([1]);
        let needs = crate::state::EVChargingNeeds {
            requested_energy_transfer: crate::state::EnergyTransferMode::AcThreePhase,
            departure_time: None,
            ac: Some(crate::state::AcChargingNeeds {
                energy_amount_wh: 40_000,
                ev_max_current_a: 32,
                ev_max_voltage_v: 230,
                ev_min_current_a: 6,
            }),
            dc: None,
            max_schedule_tuples: None,
        };

        let effects = state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::EVChargingNeedsReported(needs.clone()),
        });

        assert!(
            effects.contains(&ChargePointEffect::SmartChargingNotification(
                crate::state::SmartChargingNotification::EVChargingNeedsReported {
                    evse_id: 0,
                    needs,
                }
            ))
        );
        // Purely a pass-through notification - nothing persists on the state itself.
        assert!(!effects.contains(&ChargePointEffect::StateChanged));
    }

    #[test]
    fn ev_charging_needs_reported_for_an_unknown_evse_is_dropped() {
        let mut state = ChargePointState::new([1]);
        let needs = crate::state::EVChargingNeeds {
            requested_energy_transfer: crate::state::EnergyTransferMode::Dc,
            departure_time: None,
            ac: None,
            dc: None,
            max_schedule_tuples: None,
        };

        let effects = state.apply(ChargePointEvent::Evse {
            evse_id: 9,
            event: EvseEvent::EVChargingNeedsReported(needs),
        });

        assert!(effects.is_empty());
    }

    #[test]
    fn ev_charging_schedule_reported_on_a_real_evse_is_forwarded() {
        let mut state = ChargePointState::new([1]);
        let report = crate::state::EVChargingScheduleReport {
            schedule: crate::state::ChargingSchedule {
                id: 1,
                start_schedule: None,
                duration_secs: None,
                rate_unit: crate::state::ChargingRateUnit::Amps,
                min_charging_rate: None,
                periods: alloc::vec![crate::state::ChargingSchedulePeriod {
                    start_period_secs: 0,
                    limit: 16.0,
                    number_phases: None,
                }],
            },
            time_base: "2026-01-01T00:00:00Z".parse().unwrap(),
            power_tolerance_accepted: Some(true),
            selected_charging_schedule_id: Some(1),
        };

        let effects = state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::EVChargingScheduleReported(report.clone()),
        });

        assert!(
            effects.contains(&ChargePointEffect::SmartChargingNotification(
                crate::state::SmartChargingNotification::EVChargingScheduleReported {
                    evse_id: 0,
                    report,
                }
            ))
        );
    }

    // --- CV1.1: AvailabilityState tracks the state machine ---
    //
    // (docs/OCPP-2.1-COMPLIANCE-ROADMAP.md CV1.1; B07.FR.09 is what reads these.)

    fn availability_state(
        state: &ChargePointState,
        evse: Option<(usize, Option<usize>)>,
    ) -> String {
        let name = match evse {
            None => "ChargingStation",
            Some((_, None)) => "EVSE",
            Some((_, Some(_))) => "Connector",
        };
        state
            .device_model
            .get(
                &Component {
                    name: name.into(),
                    instance: None,
                    evse,
                },
                &crate::state::Variable {
                    name: "AvailabilityState".into(),
                    instance: None,
                },
            )
            .and_then(|definition| {
                definition.attribute(crate::state::VariableAttributeType::Actual)
            })
            .map(|attribute| attribute.value.clone())
            .unwrap_or_else(|| panic!("AvailabilityState should be registered for {evse:?}"))
    }

    #[test]
    fn a_connectors_availability_state_follows_its_own_transitions_and_leaves_its_neighbour_alone()
    {
        let mut state = ChargePointState::new([2]);
        assert_eq!(availability_state(&state, Some((0, Some(0)))), "Available");

        state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::Connector {
                connector_id: 0,
                event: ConnectorEvent::CableConnected,
            },
        });

        assert_eq!(availability_state(&state, Some((0, Some(0)))), "Occupied");
        assert_eq!(
            availability_state(&state, Some((0, Some(1)))),
            "Available",
            "plugging a cable into one connector says nothing about the other"
        );
    }

    #[test]
    fn the_charge_point_becomes_available_once_the_csms_accepts_it() {
        let mut state = ChargePointState::new([1]);
        assert_eq!(
            availability_state(&state, None),
            "Unavailable",
            "a charge point that has not registered is not available"
        );

        state.apply(ChargePointEvent::RegistrationStatusReceived(
            RegistrationStatus::Accepted,
        ));

        assert_eq!(availability_state(&state, None), "Available");
    }

    #[test]
    fn a_charge_point_wide_fault_shows_as_faulted_at_every_level() {
        let mut state = ChargePointState::new([1]);
        state.apply(ChargePointEvent::RegistrationStatusReceived(
            RegistrationStatus::Accepted,
        ));

        state.apply(ChargePointEvent::HardwareFault);

        assert_eq!(availability_state(&state, None), "Faulted");
        assert_eq!(availability_state(&state, Some((0, None))), "Faulted");
        assert_eq!(availability_state(&state, Some((0, Some(0)))), "Faulted");
    }

    #[test]
    fn an_evse_made_unavailable_reports_unavailable_without_touching_its_sibling() {
        let mut state = ChargePointState::new([1, 1]);
        state.apply(ChargePointEvent::RegistrationStatusReceived(
            RegistrationStatus::Accepted,
        ));

        state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::SetUnavailable,
        });

        assert_eq!(availability_state(&state, Some((0, None))), "Unavailable");
        assert_eq!(availability_state(&state, Some((1, None))), "Available");
        assert_eq!(
            availability_state(&state, None),
            "Available",
            "one EVSE going out of service does not take the charge point with it"
        );
    }

    /// CV1.2: `ClockCtrlr.DateTime` is empty until the charge point has been told the time, then
    /// tracks every sync. Empty rather than a plausible-but-wrong timestamp - see the variable's
    /// own docs.
    #[test]
    fn the_clocks_date_time_variable_starts_empty_and_follows_every_time_sync() {
        use crate::clock::MonotonicInstant;

        let mut state = ChargePointState::new([1]);
        let date_time = |state: &ChargePointState| {
            state
                .device_model
                .get(
                    &Component {
                        name: "ClockCtrlr".into(),
                        instance: None,
                        evse: None,
                    },
                    &crate::state::Variable {
                        name: "DateTime".into(),
                        instance: None,
                    },
                )
                .and_then(|definition| {
                    definition.attribute(crate::state::VariableAttributeType::Actual)
                })
                .map(|attribute| attribute.value.clone())
                .expect("ClockCtrlr.DateTime is registered")
        };

        assert_eq!(date_time(&state), "");

        let first = "2026-08-11T09:30:00Z".parse::<DateTime<Utc>>().unwrap();
        state.apply(ChargePointEvent::TimeSynced {
            csms_time: first,
            recorded_at: MonotonicInstant::from_ticks(0),
        });
        assert_eq!(date_time(&state), first.to_rfc3339());

        // A later heartbeat moves it on, so a `GetVariables` never reports a sync that has since
        // been superseded.
        let second = "2026-08-11T09:31:00Z".parse::<DateTime<Utc>>().unwrap();
        state.apply(ChargePointEvent::TimeSynced {
            csms_time: second,
            recorded_at: MonotonicInstant::from_ticks(60_000_000_000),
        });
        assert_eq!(date_time(&state), second.to_rfc3339());
    }

    /// CV1.3: every occupied configuration slot appears as its own `NetworkConfiguration`
    /// component instance, carrying the nine variables the 2.1 appendix marks required.
    #[test]
    fn each_network_configuration_slot_is_mirrored_into_the_device_model() {
        use crate::state::{NetworkConnectionProfile, NetworkInterface, NetworkTransport};

        let mut state = ChargePointState::new([1]);
        let read = |state: &ChargePointState, slot: &str, name: &str| -> Option<String> {
            state
                .device_model
                .get(
                    &Component {
                        name: "NetworkConfiguration".into(),
                        instance: Some(slot.into()),
                        evse: None,
                    },
                    &crate::state::Variable {
                        name: name.into(),
                        instance: None,
                    },
                )
                .and_then(|definition| {
                    definition.attribute(crate::state::VariableAttributeType::Actual)
                })
                .map(|attribute| attribute.value.clone())
        };

        state.apply(ChargePointEvent::NetworkProfileSet {
            slot: 1,
            profile: alloc::boxed::Box::new(NetworkConnectionProfile {
                csms_url: "wss://csms.example/ocpp".into(),
                interface: NetworkInterface::Wireless(0),
                transport: NetworkTransport::Json,
                security_profile: 3,
                message_timeout_secs: 45,
                identity: None,
            }),
        });

        assert_eq!(
            read(&state, "1", "OcppCsmsUrl").as_deref(),
            Some("wss://csms.example/ocpp")
        );
        assert_eq!(
            read(&state, "1", "OcppInterface").as_deref(),
            Some("Wireless0")
        );
        assert_eq!(read(&state, "1", "OcppTransport").as_deref(), Some("JSON"));
        assert_eq!(read(&state, "1", "MessageTimeout").as_deref(), Some("45"));
        assert_eq!(read(&state, "1", "SecurityProfile").as_deref(), Some("3"));
        assert_eq!(read(&state, "1", "VpnEnabled").as_deref(), Some("false"));
        assert_eq!(read(&state, "1", "ApnEnabled").as_deref(), Some("false"));
        // Required and registered, but never readable - see the registration's docs (A01.FR.12).
        assert!(
            state
                .device_model
                .get(
                    &Component {
                        name: "NetworkConfiguration".into(),
                        instance: Some("1".into()),
                        evse: None,
                    },
                    &crate::state::Variable {
                        name: "BasicAuthPassword".into(),
                        instance: None,
                    },
                )
                .and_then(
                    |definition| definition.attribute(crate::state::VariableAttributeType::Actual)
                )
                .is_some_and(
                    |attribute| attribute.mutability == crate::state::VariableMutability::WriteOnly
                )
        );

        // A slot nobody wrote has no component at all.
        assert_eq!(read(&state, "2", "OcppCsmsUrl"), None);
    }

    /// Vacating a slot takes its component with it: a CSMS URL the charge point no longer holds
    /// must stop being reported, not linger as though it were current.
    #[test]
    fn clearing_a_network_configuration_slot_removes_its_component() {
        use crate::state::{NetworkConnectionProfile, NetworkInterface, NetworkTransport};

        let mut state = ChargePointState::new([1]);
        let profile = NetworkConnectionProfile {
            csms_url: "wss://csms.example/ocpp".into(),
            interface: NetworkInterface::Any,
            transport: NetworkTransport::Json,
            security_profile: 2,
            message_timeout_secs: 30,
            identity: None,
        };
        state.apply(ChargePointEvent::NetworkProfileSet {
            slot: 1,
            profile: alloc::boxed::Box::new(profile.clone()),
        });
        let component = Component {
            name: "NetworkConfiguration".into(),
            instance: Some("1".into()),
            evse: None,
        };
        assert!(state.device_model.has_component(&component));

        state.apply(ChargePointEvent::PersistedNetworkProfilesRestored {
            slots: alloc::vec![],
        });

        assert!(
            !state.device_model.has_component(&component),
            "a vacated slot leaves nothing behind"
        );
    }

    #[test]
    fn an_evse_rolls_up_the_busiest_of_its_connectors() {
        let mut state = ChargePointState::new([2]);
        state.apply(ChargePointEvent::RegistrationStatusReceived(
            RegistrationStatus::Accepted,
        ));
        assert_eq!(availability_state(&state, Some((0, None))), "Available");

        state.apply(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::Connector {
                connector_id: 1,
                event: ConnectorEvent::CableConnected,
            },
        });

        // OCPP has no "half occupied": an EVSE with a cable in one of its connectors is the
        // EVSE a driver cannot walk up to and use.
        assert_eq!(availability_state(&state, Some((0, None))), "Occupied");
    }
}
