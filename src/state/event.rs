use alloc::string::String;
use alloc::vec::Vec;
use chrono::{DateTime, Utc};

use crate::clock::MonotonicInstant;
use crate::hardware::Capabilities;
use crate::state::{
    AuthorizationCacheEntry, AuthorizationStatus, BatterySwapEvent, ChargingLimitSource,
    ChargingProfile, ChargingProfileCriteria, ChargingProfileId, ChargingProfileScope, Component,
    ConnectorState, ConnectorStatus, ContractCertificate, DERControlQuery, DeviceModelEvent,
    DisplayMessageId, DisplayedMessage, EVChargingNeeds, EVChargingScheduleReport,
    ExternalChargingLimit, IdToken, InstalledChargingProfile, InstalledDERControl, LocalListEntry,
    MeterSample, NetworkConnectionProfile, NetworkProfileSlot, PendingBatterySwap,
    PeriodicEventStreamId, PeriodicEventStreamParams, RegistrationStatus, Reservation,
    ReservationId, ResetKind, ResetTarget, SecurityEvent, SmartChargingNotification, StopReason,
    Tariff, TariffClearCriteria, TariffScope, Transaction, TransactionId, TriggeredMonitor,
    Variable, VariableAttributeType, VariableMonitorId, VariableMonitoringEvent,
};

/// An event applied to [`crate::state::ChargePointState`], driving its state machine forward.
/// Every functional block feeds its own events into the actor through this type; see
/// [`crate::state::ChargePointState::apply`].
#[derive(Debug, Clone, PartialEq)]
pub enum ChargePointEvent {
    /// The charge point finished booting (the CSMS accepted its BootNotification, or no CSMS
    /// round-trip is required) and becomes available.
    BootCompleted,
    /// The charge point is made available charge-point-wide (OCPP `ChangeAvailability` with no
    /// `evse` addressing). See `docs/ROADMAP.md` §7.
    SetAvailable,
    /// The charge point is made unavailable charge-point-wide (OCPP `ChangeAvailability` with no
    /// `evse` addressing). See `docs/ROADMAP.md` §7.
    SetUnavailable,
    /// A hardware fault was detected at charge-point scope (not tied to a specific EVSE or
    /// connector). Cascades fail-safe to every EVSE and connector - see
    /// `ChargePointState::cascade_charge_point_fault`.
    HardwareFault,
    /// A previously reported charge-point-wide hardware fault has cleared. Cascades recovery to
    /// every EVSE and connector, but only as far as each one's own state allows (e.g. a
    /// connector whose contactor hasn't confirmed open yet stays faulted).
    FaultCleared,
    /// The CSMS answered a BootNotification with its registration decision.
    RegistrationStatusReceived(RegistrationStatus),
    /// The CSMS replaced the local authorization list (OCPP `SendLocalList`). `entries` is
    /// already resolved (by `local_authorization_list::handle_send_local_list`) to the full
    /// resulting list for both full and differential updates - the state machine just stores
    /// it. See `docs/ROADMAP.md` §4.
    LocalListUpdated {
        /// The new list's version number.
        version: i64,
        /// The new list's full contents.
        entries: Vec<LocalListEntry>,
    },
    /// A security-relevant event occurred, to be reported via SecurityEventNotification. Raised
    /// via [`crate::security::report_security_event`], not tied to connector/EVSE state. See
    /// `docs/ROADMAP.md` §1.
    SecurityEventOccurred(SecurityEvent),
    /// The CSMS requested a `Reset` (OCPP `Reset`). Recorded as a
    /// [`crate::state::PendingReset`] and fulfilled - possibly immediately, possibly once
    /// `target` goes idle - as a [`HardwareCommand::Reboot`]. See `crate::reset` and
    /// `docs/ROADMAP.md` §2.
    ResetRequested {
        /// The scope the reset applies to (the whole charge point, or one EVSE).
        target: ResetTarget,
        /// Whether to interrupt anything in progress right away, or wait for `target` to go
        /// idle first.
        kind: ResetKind,
    },
    /// The CSMS wrote a network connection profile into a configuration slot (OCPP
    /// `SetNetworkProfile`). Stored for reporting and for a future connection attempt; it does
    /// **not** switch the live connection - see [`crate::state::NetworkProfileStore`].
    NetworkProfileSet {
        /// The configuration slot addressed.
        slot: i32,
        /// The profile to store. Boxed for the same reason
        /// [`Self::ChargingProfileSet`]'s profile is: it is one of the larger things any variant
        /// carries, and every event on the mailbox would otherwise pay for it.
        profile: alloc::boxed::Box<NetworkConnectionProfile>,
    },
    /// Network profile slots recovered from durable storage at boot.
    PersistedNetworkProfilesRestored {
        /// The recovered slots. Slots beyond the configured bound are dropped from the highest
        /// slot number down.
        slots: Vec<NetworkProfileSlot>,
    },
    /// The CSMS answered an `Authorize` request, and the decision is worth remembering - see
    /// [`crate::state::AuthorizationCache`]. Raised by
    /// [`crate::authorization::run_authorization_requests`] for every decision it receives,
    /// acceptance or rejection alike.
    AuthorizationCached {
        /// The identifier the decision was about.
        id_token: IdToken,
        /// What the CSMS decided.
        status: AuthorizationStatus,
        /// When, per the caller's [`Clock`](crate::clock::Clock) - `None` when that clock wasn't
        /// synchronized, which makes the entry non-expiring (see
        /// [`crate::state::AuthorizationCacheEntry::cached_at`]).
        cached_at: Option<DateTime<Utc>>,
    },
    /// The CSMS asked for the authorization cache to be emptied (OCPP `ClearCache`), or an
    /// operator did.
    AuthorizationCacheCleared,
    /// The authorization cache as recovered from durable storage at boot.
    PersistedAuthorizationCacheRestored {
        /// The recovered entries, least recently authorized first. Entries beyond the configured
        /// bound are dropped from the *oldest* end, keeping the half a cache exists to serve.
        entries: Vec<AuthorizationCacheEntry>,
    },
    /// The CSMS installed a charging profile (OCPP `SetChargingProfile`). Applied through
    /// [`crate::state::ChargingProfileStore::install`], so the replacement rules and the
    /// [`StateLimits::max_charging_profiles`](crate::state::StateLimits::max_charging_profiles)
    /// bound both hold however the profile arrived. A profile the store refuses is logged and
    /// dropped here; `crate::smart_charging::handle_set_charging_profile` checks acceptance
    /// against the store itself before dispatching this, so the CSMS gets the real answer rather
    /// than an optimistic one.
    ChargingProfileSet {
        /// Where the profile was installed - one EVSE, or the whole charge point.
        scope: ChargingProfileScope,
        /// The profile itself. Boxed to keep [`ChargePointEvent`] small: a profile with its
        /// schedules is by far the largest thing any variant carries, and every event on the
        /// actor's mailbox would otherwise pay for it.
        profile: alloc::boxed::Box<ChargingProfile>,
    },
    /// Charging profiles were cleared - by the CSMS (OCPP `ClearChargingProfile`), or by the
    /// charge point itself when a transaction ended and its `TxProfile`s stopped applying.
    ChargingProfilesCleared {
        /// Which profiles to clear - see [`ChargingProfileCriteria`].
        criteria: ChargingProfileCriteria,
    },
    /// The CSMS installed a default tariff (OCPP 2.1 `SetDefaultTariff`), scoped to one EVSE or
    /// the whole charge point (`evseId = 0`). Applied through [`crate::state::TariffStore::set_default`],
    /// so the per-scope replacement rule and the
    /// [`StateLimits::max_tariffs`](crate::state::StateLimits::max_tariffs) bound both hold
    /// however the tariff arrived. A tariff the store refuses is logged and dropped here;
    /// `crate::tariff::handle_set_default_tariff` checks acceptance against the store itself
    /// before dispatching this, so the CSMS gets the real answer rather than an optimistic one.
    /// See `docs/ROADMAP.md` §9.
    DefaultTariffSet {
        /// Where the tariff was installed - one EVSE, or the whole charge point.
        scope: TariffScope,
        /// The tariff itself. Boxed for the same reason
        /// [`Self::ChargingProfileSet`]'s profile is - to keep this enum small on every mailbox
        /// entry that isn't this variant.
        tariff: alloc::boxed::Box<Tariff>,
    },
    /// Default tariffs were cleared (OCPP 2.1 `ClearTariffs`), by id, by EVSE scope, or both. Only
    /// ever affects [`crate::state::TariffStore`] - a driver tariff assigned to a running
    /// transaction (`ChangeTransactionTariff`) already ends with that transaction on its own (see
    /// [`ConnectorEvent::TariffAssigned`]), so there is nothing here for it to clear.
    TariffsCleared {
        /// Which tariffs to clear - see [`TariffClearCriteria`].
        criteria: TariffClearCriteria,
    },
    /// The CSMS opened a periodic event stream (OCPP 2.1 `OpenPeriodicEventStream`). Applied
    /// through [`crate::state::PeriodicEventStreamStore::open`], so the per-id replacement rule
    /// and the [`StateLimits::max_periodic_event_streams`](crate::state::StateLimits::max_periodic_event_streams)
    /// bound both hold however the stream arrived. A stream the store refuses is logged and
    /// dropped here; `crate::periodic_event_stream::handle_open_periodic_event_stream` checks
    /// acceptance against the store itself before dispatching this, so the CSMS gets the real
    /// answer rather than an optimistic one. See `docs/PRODUCTION-ROADMAP.md` B5.6.
    PeriodicEventStreamOpened {
        /// The stream's CSMS-assigned id.
        id: PeriodicEventStreamId,
        /// The variable monitor this stream reports the value of.
        variable_monitoring_id: VariableMonitorId,
        /// The stream's initial parameters.
        params: PeriodicEventStreamParams,
    },
    /// The CSMS closed a periodic event stream (OCPP 2.1 `ClosePeriodicEventStream`). A no-op if
    /// no stream with this id is open.
    PeriodicEventStreamClosed {
        /// The stream to close.
        id: PeriodicEventStreamId,
    },
    /// The CSMS adjusted an open periodic event stream's parameters (OCPP 2.1
    /// `AdjustPeriodicEventStream`). A no-op if no stream with this id is open.
    PeriodicEventStreamAdjusted {
        /// The stream to adjust.
        id: PeriodicEventStreamId,
        /// Its new parameters, replacing whatever it had.
        params: PeriodicEventStreamParams,
    },
    /// A dynamic charging profile took a new limit - OCPP 2.1's `UpdateDynamicSchedule` pushed by
    /// the CSMS, or the response to a `PullDynamicScheduleUpdate` this charge point asked for
    /// (`docs/PRODUCTION-ROADMAP.md` B2.6, OCPP K28.FR.06/K28.FR.08/K28.FR.09).
    ///
    /// Applied through [`crate::state::ChargingProfileStore::apply_dynamic_update`], so the rule
    /// that only a `Dynamic` profile may be updated in place holds however the update arrived. An
    /// update naming an absent or non-dynamic profile is logged and dropped;
    /// `crate::smart_charging::handle_update_dynamic_schedule` checks first, so the CSMS gets the
    /// real answer rather than an optimistic one.
    DynamicScheduleUpdated {
        /// Which profile the CSMS is updating.
        profile_id: ChargingProfileId,
        /// The new limit for the profile's single period, in that schedule's rate unit. `None`
        /// when the update carried only values this crate cannot project onto hardware - see the
        /// store method's docs for why the timestamp still moves.
        limit: Option<f64>,
        /// When the update was applied, per the receiving adapter's clock. Becomes the profile's
        /// new [`ChargingProfile::dyn_update_time`](crate::state::ChargingProfile::dyn_update_time).
        updated_at: DateTime<Utc>,
    },
    /// Priority charging was granted or withdrawn for one transaction - OCPP 2.1's
    /// `UsePriorityCharging` when the CSMS asked, or the charge point's own decision when it
    /// didn't (`docs/PRODUCTION-ROADMAP.md` B2.6).
    ///
    /// Addressed by transaction rather than by connector, which is how OCPP addresses it and also
    /// the only safe reading: by the time this is applied the session the CSMS named may have
    /// ended, and granting priority to whatever started since would be granting it to the wrong
    /// driver. A transaction that is no longer running is logged and dropped.
    PriorityChargingSet {
        /// The transaction to grant or withdraw priority charging for.
        transaction_id: TransactionId,
        /// `true` grants it, `false` withdraws it.
        activated: bool,
        /// Whether the charge point decided this itself rather than being told. Only a local
        /// decision produces [`ChargePointEffect::PriorityChargingChanged`]: reporting a change
        /// back to the CSMS that requested it is noise, and OCPP's `NotifyPriorityCharging`
        /// exists precisely for the changes it did *not* request - the same split
        /// [`Self::ChargingProfilesCleared`]'s sibling
        /// [`ChargePointEffect::ReservationEnded`] makes for reservations.
        locally_initiated: bool,
    },
    /// An event mutating the Component/Variable device model (OCPP `GetVariables`/
    /// `SetVariables`, or 1.6J's `GetConfiguration`/`ChangeConfiguration` projection onto it) -
    /// see `crate::state::device_model` and `crate::device_model`. See `docs/ROADMAP.md` §2.
    DeviceModel(DeviceModelEvent),
    /// The CSMS installed a DER control setting (OCPP 2.1 `SetDERControl`). Applied through
    /// [`crate::state::DERControlStore::install`], so the replacement-by-id rule and the
    /// [`StateLimits::max_der_controls`](crate::state::StateLimits::max_der_controls) bound both
    /// hold however the control arrived. A control the store refuses is logged and dropped here;
    /// `crate::der_control::handle_set_der_control` checks acceptance against the store itself
    /// before dispatching this, so the CSMS gets the real answer rather than an optimistic one.
    /// See `docs/PRODUCTION-ROADMAP.md` B8.2.
    DERControlSet(alloc::boxed::Box<InstalledDERControl>),
    /// DER control settings were cleared (OCPP 2.1 `ClearDERControl`), by id, by kind, by
    /// default/scheduled, or any combination.
    DERControlsCleared {
        /// Which controls to clear - see [`DERControlQuery`].
        query: DERControlQuery,
    },
    /// The CSMS pushed an automatic frequency restoration reserve signal (OCPP 2.1 `AFRRSignal`) -
    /// a grid-balancing setpoint. Recorded as the charge point's latest known signal; nothing in
    /// this crate acts on it yet (`docs/PRODUCTION-ROADMAP.md` B8.2's store-and-report scope), so
    /// this is a single value rather than a growable collection - no bound is needed.
    AfrrSignalReceived {
        /// The signal value, per `v2xSignalWattCurve`'s own units.
        signal: i64,
        /// When the signal becomes active.
        timestamp: DateTime<Utc>,
    },
    /// The hardware binding's declared [`Capabilities`], captured once during
    /// [`crate::builder::ChargePointBuilder::start`]. The single source of truth every
    /// capability-propagation surface (handler registration, the device model's `*Ctrlr.Available`
    /// variables, 1.6J `SupportedFeatureProfiles`) ultimately derives from - see
    /// `docs/PRODUCTION-ROADMAP.md` §5.3 (C3).
    CapabilitiesDeclared(Capabilities),
    /// The integrator's declaration of this charge point's electrical shape - phase counts,
    /// connector types and per-EVSE maximum power (CV1.5). Sent once during setup from
    /// [`crate::hardware::ChargePoint::electrical`].
    ElectricalCharacteristicsDeclared(
        alloc::boxed::Box<crate::hardware::ElectricalCharacteristics>,
    ),
    /// In-flight transactions recovered from durable storage at boot, applied as one atomic
    /// event so recovery can never be observed half-done. See
    /// [`crate::persistence::restore_transactions`] and `docs/PRODUCTION-ROADMAP.md` §7.4 (E4.1).
    ///
    /// Each entry is *closed out* rather than resumed: a `TransactionEvent(Ended)` with
    /// [`StopReason::PowerLoss`] is emitted per recovered transaction, carrying the last meter
    /// reading that reached storage before the power cut, so the CSMS can still bill the energy
    /// delivered. Resuming across a reboot would require asserting that the EV stayed connected
    /// and energy kept flowing while the firmware was not running, which no hardware binding in
    /// [`crate::hardware`] can currently attest to.
    PersistedTransactionsRestored {
        /// The transaction-id counter as of the last persisted transaction start, so a recovered
        /// charge point never reissues an id the CSMS has already seen. Applied as a floor, not
        /// an assignment - a counter that has already advanced further in this process is left
        /// alone.
        next_transaction_id: u64,
        /// The transactions that were in flight when power was lost, in no particular order.
        transactions: Vec<RecoveredTransaction>,
    },
    /// A CSMS-supplied `currentTime` (from a BootNotification or Heartbeat response) was accepted
    /// as this charge point's current best time-sync anchor - see
    /// [`crate::provisioning::evaluate_time_sync`] and [`crate::state::TimeSyncAnchor`]. Raised on
    /// every successful BootNotification/Heartbeat exchange that carried a parseable
    /// `currentTime`, not only when [`crate::provisioning::evaluate_time_sync`] judged the
    /// difference worth a `SettingSystemTime` security event - so the anchor used for the *next*
    /// comparison is always the freshest CSMS time seen, keeping routine drift small even when no
    /// step was reported.
    TimeSynced {
        /// The CSMS's `currentTime`.
        csms_time: DateTime<Utc>,
        /// A [`MonotonicInstant`] reading taken at the moment `csms_time` was accepted, so a
        /// later comparison can advance `csms_time` by elapsed monotonic time rather than reusing
        /// it unmodified - see [`crate::state::TimeSyncAnchor`].
        recorded_at: MonotonicInstant,
    },
    /// The local authorization list recovered from durable storage at boot, applied as one atomic
    /// event before the `SendLocalList`/`GetLocalListVersion` handlers can race it - see
    /// [`crate::persistence::restore_local_authorization_list`] and
    /// `docs/PRODUCTION-ROADMAP.md` §7.2 (E2.4). Replaces the list outright, exactly like
    /// [`ChargePointEvent::LocalListUpdated`] (a fresh boot's in-memory list is always empty, so
    /// there's nothing to merge with).
    PersistedLocalAuthorizationListRestored {
        /// The persisted list's version number.
        version: i64,
        /// The persisted list's full contents.
        entries: Vec<LocalListEntry>,
    },
    /// Reservations recovered from durable storage at boot, applied as one atomic event before
    /// `ReserveNow`/`CancelReservation` can race it - see
    /// [`crate::persistence::restore_reservations`] and `docs/PRODUCTION-ROADMAP.md` §7.2 (E2.6).
    ///
    /// Each entry is only restored onto a connector that is currently [`ConnectorState::Available`]
    /// (the state every connector boots into) - anything else (a connector this firmware no
    /// longer has, or one that's somehow not `Available` this early) is skipped and logged,
    /// mirroring [`ChargePointEvent::PersistedTransactionsRestored`]'s tolerance. A reservation
    /// whose `expires_at` had already passed while the charge point was off is filtered out
    /// *before* this event is even raised - see [`crate::persistence::restore_reservations`]'s
    /// docs for why that decision belongs to the persistence layer rather than the state machine.
    PersistedReservationsRestored {
        /// The reservations to restore, in no particular order.
        reservations: Vec<RecoveredReservation>,
    },
    /// Device model attribute values recovered from durable storage at boot, applied as one
    /// atomic event *after* the hardware binding has finished registering its own variables (see
    /// [`crate::builder::ChargePointBuilder::device_model_persistence`]'s docs for the ordering
    /// rationale) - `docs/PRODUCTION-ROADMAP.md` §7.2 (E2.3).
    ///
    /// A persisted attribute is only applied if `component`/`variable`/`attribute_type` are
    /// already registered in the device model this boot - i.e. the hardware binding re-declared
    /// the variable exists on this firmware/hardware combination. One that isn't (e.g. a firmware
    /// downgrade that removed a variable, or hardware that was reconfigured) is left dormant
    /// rather than applied: [`crate::state::DeviceModel::set_attribute_value`] already returns
    /// `false` for an unregistered target and this event logs that case rather than treating it
    /// as an ordinary no-op, so the binding is unambiguously the source of truth for which
    /// variables exist this boot, never the persisted record.
    PersistedDeviceModelAttributesRestored {
        /// The attribute values to restore, in no particular order.
        attributes: Vec<RecoveredDeviceModelAttribute>,
    },
    /// Charging profiles recovered from durable storage at boot (`docs/PRODUCTION-ROADMAP.md`
    /// §7.2, E2.7).
    ///
    /// Each is installed through the same [`crate::state::ChargingProfileStore::install`] path a
    /// live `SetChargingProfile` takes, so the replacement rules and the
    /// [`StateLimits::max_charging_profiles`](crate::state::StateLimits::max_charging_profiles)
    /// bound hold identically however a profile arrived. A profile addressing an EVSE this
    /// firmware no longer has is discarded and logged (a topology change across the update that
    /// caused the restart), and any refused by the bound raise a `MemoryExhaustion` security
    /// event rather than disappearing quietly - a *limit* the CSMS believes is installed but
    /// isn't is exactly the kind of divergence worth reporting.
    PersistedChargingProfilesRestored {
        /// The recovered profiles, in the order they were installed before the restart.
        profiles: Vec<InstalledChargingProfile>,
    },
    /// The CSMS asked (OCPP `CustomerInformation`, `clear: true`) that everything this charge
    /// point holds about one customer be erased - see [`crate::customer_information`] and
    /// `docs/PRODUCTION-ROADMAP.md` B5.5.
    ///
    /// Removes any [`AuthorizationCacheEntry`] and any [`LocalListEntry`] keyed on `id_token`.
    /// Deliberately does **not** touch a transaction still in progress under this token - see
    /// [`crate::customer_information`]'s docs for why erasing a live transaction's identity is not
    /// a mutation this crate can honestly perform.
    CustomerInformationErased {
        /// The identifier whose held data should be erased.
        id_token: IdToken,
    },
    /// The CSMS installed/replaced a display message (OCPP `SetDisplayMessage`), accepted by
    /// [`crate::display_message::handle_set_display_message`]. Boxed for the same reason
    /// [`Self::ChargingProfileSet`]'s profile is - the largest thing this variant could carry
    /// (message content up to 1024 bytes) should not be paid for by every event on the mailbox.
    DisplayMessageSet(alloc::boxed::Box<DisplayedMessage>),
    /// The CSMS removed a display message (OCPP `ClearDisplayMessage`), accepted by
    /// [`crate::display_message::handle_clear_display_message`].
    DisplayMessageCleared(DisplayMessageId),
    /// An external charging limit (from something other than a CSMS charging profile - typically
    /// a local energy-management system) came into force. Pushed in by an integrator the same way
    /// [`ConnectorEvent::MeterValueSampled`] is - this crate has no local energy-management stack
    /// of its own.
    ///
    /// It is both **enforced and reported**. Enforced by composition, which applies it as an upper
    /// bound on whatever the CSMS's profiles asked for (OCPP K11.FR.01, K12.FR.01, K27.FR.01) - see
    /// [`crate::smart_charging::external_charging_limits`]. Reported with `NotifyChargingLimit`
    /// (2.0.1/2.1 only, `docs/PRODUCTION-ROADMAP.md` B2.8). A limit whose
    /// [`schedule`](ExternalChargingLimit::schedule) is `None` is reported but cannot be enforced -
    /// there is no value to apply - and recording one warns.
    ExternalChargingLimitSet {
        /// The EVSE the limit applies to, or `None` for the whole charging station.
        evse_id: Option<usize>,
        /// The limit itself.
        limit: ExternalChargingLimit,
    },
    /// A previously reported external charging limit has gone away, to be reported via
    /// `ClearedChargingLimit` (2.0.1/2.1 only, `docs/PRODUCTION-ROADMAP.md` B2.8). A no-op if
    /// `evse_id`/`source` do not match a limit this crate currently has recorded - reporting a
    /// clearance for a limit the CSMS was never told about would be worse than silence.
    ExternalChargingLimitCleared {
        /// The EVSE the limit applied to, or `None` for the whole charging station.
        evse_id: Option<usize>,
        /// The source of the limit that ended.
        source: ChargingLimitSource,
    },
    /// An event addressed to one EVSE (or, via [`EvseEvent::Connector`], one of its connectors).
    Evse {
        /// The addressed EVSE's index.
        evse_id: usize,
        /// The event to apply to that EVSE.
        event: EvseEvent,
    },
    /// An event mutating the variable monitoring engine's store (OCPP `SetVariableMonitoring`/
    /// `ClearVariableMonitoring`) - see `crate::state::variable_monitoring` and
    /// `crate::variable_monitoring`. See `docs/ROADMAP.md` §2/§14 (B5.2). **2.x only** - 1.6J has
    /// no such messages.
    VariableMonitoring(VariableMonitoringEvent),
    /// The CSMS accepted a `RequestBatterySwap`, asking this charge point to prepare a swap - see
    /// [`crate::battery_swap::handle_request_battery_swap`]. **2.1 only.**
    BatterySwapRequested(PendingBatterySwap),
    /// A previously recorded [`PendingBatterySwap`] is withdrawn without ever being correlated
    /// with a reported swap - e.g. the station's own hardware failed to prepare for it. Raised
    /// by [`crate::battery_swap::handle_request_battery_swap`] to roll back the store entry it
    /// just recorded when [`crate::hardware::BatterySwapStation::prepare_swap`] fails, so a
    /// `RequestBatterySwap` answered `Rejected` does not leave a pending entry the CSMS was just
    /// told does not exist. **2.1 only.**
    BatterySwapCancelled(crate::state::BatterySwapRequestId),
    /// Hardware reported a battery-swap lifecycle event (OCPP `BatterySwap`) - see
    /// [`crate::battery_swap::report_battery_swap_event`]. **2.1 only.**
    BatterySwapReported(BatterySwapEvent),
}

impl ChargePointEvent {
    /// The event's variant name, as a low-cardinality `&'static str` suitable for a `tracing`
    /// field.
    ///
    /// Logging `event = %event.name()` rather than `event = ?event` is the difference between a
    /// log a field engineer can filter and aggregate and one that dumps a full payload - which on
    /// an MCU is most of the cost of having logs at all. Use this for the routine per-event line
    /// and reserve `{:?}` for `TRACE`, where the payload is the point.
    ///
    /// The match is exhaustive with no wildcard arm on purpose: a new variant is a compile error
    /// here, so an event cannot start flowing through the actor while logging as something else.
    ///
    /// [`Self::Evse`] delegates to the EVSE's - and in turn the connector's - own name, because
    /// almost everything worth reading in a charge point's log is nested inside it: a log full of
    /// `event=Evse` would name the envelope and hide the cable, the card and the contactor. Pair
    /// this with [`evse_id`](Self::evse_id)/[`connector_id`](Self::connector_id) to say which
    /// connector it happened on.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Evse { event, .. } => event.name(),
            Self::BootCompleted { .. } => "BootCompleted",
            Self::SetAvailable { .. } => "SetAvailable",
            Self::SetUnavailable { .. } => "SetUnavailable",
            Self::HardwareFault { .. } => "HardwareFault",
            Self::FaultCleared { .. } => "FaultCleared",
            Self::RegistrationStatusReceived { .. } => "RegistrationStatusReceived",
            Self::LocalListUpdated { .. } => "LocalListUpdated",
            Self::SecurityEventOccurred { .. } => "SecurityEventOccurred",
            Self::ResetRequested { .. } => "ResetRequested",
            Self::NetworkProfileSet { .. } => "NetworkProfileSet",
            Self::PersistedNetworkProfilesRestored { .. } => "PersistedNetworkProfilesRestored",
            Self::AuthorizationCached { .. } => "AuthorizationCached",
            Self::AuthorizationCacheCleared { .. } => "AuthorizationCacheCleared",
            Self::PersistedAuthorizationCacheRestored { .. } => {
                "PersistedAuthorizationCacheRestored"
            }
            Self::ChargingProfileSet { .. } => "ChargingProfileSet",
            Self::ChargingProfilesCleared { .. } => "ChargingProfilesCleared",
            Self::DefaultTariffSet { .. } => "DefaultTariffSet",
            Self::TariffsCleared { .. } => "TariffsCleared",
            Self::PeriodicEventStreamOpened { .. } => "PeriodicEventStreamOpened",
            Self::PeriodicEventStreamClosed { .. } => "PeriodicEventStreamClosed",
            Self::PeriodicEventStreamAdjusted { .. } => "PeriodicEventStreamAdjusted",
            Self::DynamicScheduleUpdated { .. } => "DynamicScheduleUpdated",
            Self::PriorityChargingSet { .. } => "PriorityChargingSet",
            Self::DeviceModel { .. } => "DeviceModel",
            Self::DERControlSet { .. } => "DERControlSet",
            Self::DERControlsCleared { .. } => "DERControlsCleared",
            Self::AfrrSignalReceived { .. } => "AfrrSignalReceived",
            Self::CapabilitiesDeclared { .. } => "CapabilitiesDeclared",
            Self::ElectricalCharacteristicsDeclared { .. } => "ElectricalCharacteristicsDeclared",
            Self::PersistedTransactionsRestored { .. } => "PersistedTransactionsRestored",
            Self::TimeSynced { .. } => "TimeSynced",
            Self::PersistedLocalAuthorizationListRestored { .. } => {
                "PersistedLocalAuthorizationListRestored"
            }
            Self::PersistedReservationsRestored { .. } => "PersistedReservationsRestored",
            Self::PersistedDeviceModelAttributesRestored { .. } => {
                "PersistedDeviceModelAttributesRestored"
            }
            Self::PersistedChargingProfilesRestored { .. } => "PersistedChargingProfilesRestored",
            Self::CustomerInformationErased { .. } => "CustomerInformationErased",
            Self::DisplayMessageSet { .. } => "DisplayMessageSet",
            Self::DisplayMessageCleared { .. } => "DisplayMessageCleared",
            Self::ExternalChargingLimitSet { .. } => "ExternalChargingLimitSet",
            Self::ExternalChargingLimitCleared { .. } => "ExternalChargingLimitCleared",
            Self::VariableMonitoring { .. } => "VariableMonitoring",
            Self::BatterySwapRequested { .. } => "BatterySwapRequested",
            Self::BatterySwapCancelled { .. } => "BatterySwapCancelled",
            Self::BatterySwapReported { .. } => "BatterySwapReported",
        }
    }

    /// The EVSE this event addresses, if it addresses one. `None` for charge-point-wide events.
    ///
    /// Logged alongside [`name`](Self::name) so a site with several EVSEs can be read one EVSE at
    /// a time.
    pub fn evse_id(&self) -> Option<usize> {
        match self {
            Self::Evse { evse_id, .. } => Some(*evse_id),
            _ => None,
        }
    }

    /// The connector this event addresses, if it addresses one. `None` for EVSE-wide and
    /// charge-point-wide events.
    pub fn connector_id(&self) -> Option<usize> {
        match self {
            Self::Evse {
                event: EvseEvent::Connector { connector_id, .. },
                ..
            } => Some(*connector_id),
            _ => None,
        }
    }
}

impl EvseEvent {
    /// The event's variant name for `tracing`, delegating to the connector's own name for
    /// [`Self::Connector`]. See [`ChargePointEvent::name`].
    pub fn name(&self) -> &'static str {
        match self {
            Self::Connector { event, .. } => event.name(),
            Self::SetAvailable { .. } => "EvseSetAvailable",
            Self::SetUnavailable { .. } => "EvseSetUnavailable",
            Self::FaultDetected { .. } => "EvseFaultDetected",
            Self::FaultCleared { .. } => "EvseFaultCleared",
            Self::EVChargingNeedsReported { .. } => "EVChargingNeedsReported",
            Self::EVChargingScheduleReported { .. } => "EVChargingScheduleReported",
        }
    }
}

impl ConnectorEvent {
    /// The event's variant name for `tracing`. See [`ChargePointEvent::name`].
    ///
    /// These are the events a charge point's log is mostly made of - a cable going in, a card
    /// being presented, a contactor closing - so this is the name that matters most.
    pub fn name(&self) -> &'static str {
        match self {
            Self::CableConnected { .. } => "CableConnected",
            Self::CableDisconnected { .. } => "CableDisconnected",
            Self::LockConfirmed { .. } => "LockConfirmed",
            Self::UnlockConfirmed { .. } => "UnlockConfirmed",
            Self::LockFailed { .. } => "ConnectorLockFailed",
            Self::IdTokenPresented { .. } => "IdTokenPresented",
            Self::ContractCertificatePresented { .. } => "ContractCertificatePresented",
            Self::ChargingAuthorized { .. } => "ChargingAuthorized",
            Self::AuthorizationDenied { .. } => "AuthorizationDenied",
            Self::ContactorClosed { .. } => "ContactorClosed",
            Self::ContactorOpened { .. } => "ContactorOpened",
            Self::RemoteUnlockRequested { .. } => "RemoteUnlockRequested",
            Self::RemoteStartRequested { .. } => "RemoteStartRequested",
            Self::RemoteStartPending { .. } => "RemoteStartPending",
            Self::RemoteStartPendingCleared { .. } => "RemoteStartPendingCleared",
            Self::AuthorizationRevoked { .. } => "AuthorizationRevoked",
            Self::ChargingStopped { .. } => "ChargingStopped",
            Self::ChargingSuspendedByEv { .. } => "ChargingSuspendedByEv",
            Self::ChargingSuspendedByEvse { .. } => "ChargingSuspendedByEvse",
            Self::ChargingResumed { .. } => "ChargingResumed",
            Self::MeterValueSampled { .. } => "MeterValueSampled",
            Self::Reserved { .. } => "Reserved",
            Self::ReservationCancelled { .. } => "ReservationCancelled",
            Self::ReservationExpired { .. } => "ReservationExpired",
            Self::CostUpdated { .. } => "CostUpdated",
            Self::TariffAssigned { .. } => "TariffAssigned",
            Self::RunningCostAdvanced { .. } => "RunningCostAdvanced",
            Self::ResetRequested { .. } => "ConnectorResetRequested",
            Self::SetAvailable { .. } => "ConnectorSetAvailable",
            Self::SetUnavailable { .. } => "ConnectorSetUnavailable",
            Self::FaultDetected { .. } => "ConnectorFaultDetected",
            Self::FaultCleared { .. } => "ConnectorFaultCleared",
            Self::CurrentLimitConfirmed { .. } => "CurrentLimitConfirmed",
            Self::CurrentLimitComputed { .. } => "CurrentLimitComputed",
        }
    }
}

/// An event addressed to one EVSE, either changing the EVSE's own availability/fault status or
/// (via [`Connector`](EvseEvent::Connector)) addressing one of its connectors.
#[derive(Debug, Clone, PartialEq)]
pub enum EvseEvent {
    /// The EVSE is made available (OCPP `ChangeAvailability` addressing this `evse`, no
    /// `connectorId`). See `docs/ROADMAP.md` §7.
    SetAvailable,
    /// The EVSE is made unavailable (OCPP `ChangeAvailability` addressing this `evse`, no
    /// `connectorId`). See `docs/ROADMAP.md` §7.
    SetUnavailable,
    /// A hardware fault was detected affecting this whole EVSE (e.g. a shared power source or
    /// meter fault), not just one connector. Cascades fail-safe to every connector this EVSE
    /// owns - see `ChargePointState::cascade_evse_fault`.
    FaultDetected,
    /// A previously reported EVSE-wide hardware fault has cleared. Cascades recovery to every
    /// connector, but only as far as each connector's own state allows.
    FaultCleared,
    /// An event addressed to one of this EVSE's connectors.
    Connector {
        /// The addressed connector's index within this EVSE.
        connector_id: usize,
        /// The event to apply to that connector.
        event: ConnectorEvent,
    },
    /// The EV connected to this EVSE reported its charging needs via ISO 15118, to be forwarded
    /// as `NotifyEVChargingNeeds` (2.0.1/2.1 only, `docs/PRODUCTION-ROADMAP.md` B2.8). This crate
    /// does not implement ISO 15118 itself (B4.5) - an integrator with a 15118 stack pushes this
    /// in, the same way [`ConnectorEvent::MeterValueSampled`] lets hardware push in a reading.
    EVChargingNeedsReported(EVChargingNeeds),
    /// The EV connected to this EVSE reported (or accepted) a charging schedule via ISO 15118, to
    /// be forwarded as `NotifyEVChargingSchedule` (2.0.1/2.1 only,
    /// `docs/PRODUCTION-ROADMAP.md` B2.8). See [`Self::EVChargingNeedsReported`] for why this is
    /// pushed in rather than derived.
    EVChargingScheduleReported(EVChargingScheduleReport),
}

/// An event addressed to one connector, driving its [`crate::state::ConnectorState`] machine.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectorEvent {
    /// A cable was physically plugged into the connector.
    CableConnected,
    /// The cable was physically unplugged from the connector.
    CableDisconnected,
    /// Hardware confirmed the connector lock engaged, in response to a
    /// [`HardwareCommand::LockConnector`].
    LockConfirmed,
    /// Hardware confirmed the connector unlocked, in response to a
    /// [`HardwareCommand::UnlockConnector`].
    UnlockConfirmed,
    /// The connector's plug retention lock failed to engage or release - **OCPP use case G05,
    /// "Lock Failure"** (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV11).
    ///
    /// Raised by [`crate::hardware::execute_hardware_command`] when a binding's
    /// [`Connector::lock`](crate::hardware::Connector::lock) or
    /// [`unlock`](crate::hardware::Connector::unlock) returns an error, and available to a binding
    /// that detects a lock which reports open while it should be closed.
    ///
    /// Drives the connector fail-safe into `Faulted` exactly as [`Self::FaultDetected`] does -
    /// G05.FR.01 is precisely "SHALL NOT start charging" - but is a *distinct* event because a
    /// CSMS has to be able to tell a lock failure from any other fault: it also sets the
    /// connector's `ConnectorPlugRetentionLock`/`Problem` variable and reports it as a hard-wired
    /// `NotifyEvent` (G05.FR.02).
    LockFailed,
    /// The driver/EV presented an identifier; the Authorization functional block asks the CSMS
    /// whether it may start charging.
    ///
    /// Accepted both ways round. With the cable already latched the connector moves to
    /// [`ConnectorState::Authorizing`](crate::state::ConnectorState::Authorizing) and the
    /// acceptance starts the transaction. On a connector with **no cable yet** - OCPP's E03,
    /// "Start Transaction - IdToken First" - the connector cannot move, so the acceptance is held
    /// against it (see [`crate::state::EvseState::pending_remote_starts`]) and dispatched when the
    /// driver plugs in, or dropped when `TxCtrlr.EVConnectionTimeOut` expires (E03.FR.15).
    IdTokenPresented(IdToken),
    /// The EV presented an ISO 15118 contract certificate while the connector is locked - Plug &
    /// Charge, OCPP use case C07. Behaves exactly like [`Self::IdTokenPresented`] with the eMAID
    /// as the identifier, and additionally carries the certificate material the CSMS validates
    /// the contract with (see [`ContractCertificate`]).
    ///
    /// Sent by the integrator's ISO 15118 stack, which is the only thing that ever sees a
    /// `CertificateInstallationRes` - this crate neither speaks that protocol nor parses what it
    /// carries (see [`crate::iso15118`]).
    ContractCertificatePresented {
        /// The eMAID from the contract certificate, as the identifier being authorized.
        id_token: IdToken,
        /// The certificate material to validate it against.
        certificate: ContractCertificate,
    },
    /// The CSMS accepted the presented identifier (or, for the moment, some other decision
    /// producing the same effect - see `docs/ROADMAP.md` §3). Carries the identifier that was
    /// authorized, recorded on the [`Transaction`] this starts.
    ChargingAuthorized(IdToken),
    /// The CSMS rejected the presented identifier.
    AuthorizationDenied,
    /// Hardware confirmed the contactor closed, in response to a
    /// [`HardwareCommand::CloseContactor`].
    ContactorClosed,
    /// Hardware confirmed the contactor opened, in response to a
    /// [`HardwareCommand::OpenContactor`].
    ContactorOpened,
    /// The CSMS asked to unlock the connector (OCPP `UnlockConnector`) while it's `Locked` with
    /// no active transaction. See `docs/ROADMAP.md` §6.
    RemoteUnlockRequested,
    /// The CSMS asked to start a transaction (OCPP `RequestStartTransaction`) while the
    /// connector is `Locked` with no active transaction. Unlike `IdTokenPresented`, this skips
    /// straight to `Starting` without a separate Authorize round-trip - the CSMS's own request
    /// is itself the authorization decision. Carries the identifier the CSMS supplied and the
    /// request's `remoteStartId`, both recorded on the [`Transaction`] this starts - the latter
    /// is how the CSMS correlates the transaction it gets back with the request it made
    /// (F01.FR.25, F02.FR.01/.21; CV6). See `docs/ROADMAP.md` §6.
    RemoteStartRequested {
        /// The identifier the CSMS supplied for the transaction to run under.
        id_token: IdToken,
        /// The request's `remoteStartId`, if it carried one.
        remote_start_id: Option<i64>,
    },
    /// The CSMS asked to start a transaction on a connector that has **no cable yet** - OCPP's
    /// F02, "Remote Start Transaction - Remote Start First" (CV7).
    ///
    /// Unlike [`Self::RemoteStartRequested`] this starts nothing: it records the request against
    /// the connector, which then behaves exactly as if a card had been presented once the cable
    /// latches. See [`crate::state::PendingRemoteStart`].
    RemoteStartPending(crate::state::PendingRemoteStart),
    /// The identifier authorizing this transaction is **no longer valid** - OCPP's E05.
    ///
    /// Raised when the CSMS answers a `TransactionEvent` (or a re-`Authorize`) with a rejected
    /// `idTokenInfo` for a session already underway. What happens next is the operator's
    /// configuration, not this crate's choice: `TxCtrlr.StopTxOnInvalidId` stops immediately, and
    /// otherwise `TxCtrlr.MaxEnergyOnInvalidId` grants a last allowance so the driver is not
    /// stranded mid-motorway by a CSMS blocklist update. See [`crate::state::Transaction::stop_at_energy_wh`].
    AuthorizationRevoked,
    /// The authorization held against this connector is no longer wanted - the cable arrived and
    /// it was dispatched, or `TxCtrlr.EVConnectionTimeOut` expired without one. Covers both a held
    /// `RequestStartTransaction` (F02.FR.07/.08) and a card presented before the cable
    /// (E03.FR.15), which share one slot - see
    /// [`crate::state::EvseState::pending_remote_starts`].
    RemoteStartPendingCleared,
    /// Charging has stopped (locally, remotely, or the EV finished) and the contactor should
    /// open. Not used for hardware-fault-driven stops - those go through `FaultDetected`.
    ChargingStopped(StopReason),
    /// The **EV** stopped drawing energy while charging - a full battery, or a vehicle-side pause.
    /// Drives the connector into
    /// [`ConnectorState::SuspendedEv`](crate::state::ConnectorState::SuspendedEv) and the
    /// transaction's charging state to match, ending neither. Pushed in by the hardware binding,
    /// which is the only thing that can tell which side stopped drawing
    /// (`docs/PRODUCTION-ROADMAP.md` B1.5).
    ChargingSuspendedByEv,
    /// The **EVSE** stopped supplying energy while charging - a smart-charging limit of 0 A, load
    /// management, or a local supply constraint. Drives the connector into
    /// [`ConnectorState::SuspendedEvse`](crate::state::ConnectorState::SuspendedEvse).
    ChargingSuspendedByEvse,
    /// Energy is flowing again after a suspension from either side. A no-op on a connector that
    /// isn't suspended.
    ChargingResumed,
    /// Hardware sampled a meter reading. Reported to the CSMS (via the active transaction's
    /// next TransactionEvent) only while the connector is actually `Charging`; ignored
    /// otherwise. See `docs/ROADMAP.md` §10.
    MeterValueSampled(MeterSample),
    /// The CSMS reserved this connector (OCPP `ReserveNow`) while it's `Available`. See
    /// `docs/ROADMAP.md` §8.
    Reserved(Reservation),
    /// The CSMS cancelled this connector's reservation (OCPP `CancelReservation`) while it's
    /// `Reserved`. See `docs/ROADMAP.md` §8.
    ReservationCancelled,
    /// This connector's reservation ran past its `expiryDateTime` and was released by the charge
    /// point, freeing the connector. Dispatched by
    /// [`crate::reservation::run_reservation_expiry`], not by the CSMS - which is why, unlike
    /// [`Self::ReservationCancelled`], it produces a
    /// [`ChargePointEffect::ReservationEnded`] for the CSMS to be told about.
    ReservationExpired,
    /// The CSMS reported a new running total cost for this connector's active transaction (OCPP
    /// `CostUpdated`). Ignored if there's no active transaction. See `docs/ROADMAP.md` §9.
    CostUpdated(f64),
    /// The CSMS assigned a driver tariff to this connector's active transaction (OCPP 2.1
    /// `ChangeTransactionTariff`). Ignored if there's no active transaction - mirrors
    /// [`Self::CostUpdated`] exactly, including how the assignment is cleared the moment the
    /// transaction starts or ends (see `crate::state::EvseState::transaction_tariffs`). See
    /// `docs/ROADMAP.md` §9 and `docs/PRODUCTION-ROADMAP.md` B7.1.
    /// Boxed, like `DefaultTariffSet`'s payload above and for the same reason: a priced `Tariff`
    /// is by far the largest thing any connector event carries (CV8 gave it energy, time, idle
    /// and fixed-fee components with their conditions), and an enum is as big as its widest
    /// variant. Unboxed, every `ConnectorEvent` in every queue and broadcast on an MCU would pay
    /// for a tariff that almost none of them carry.
    TariffAssigned(alloc::boxed::Box<Tariff>),
    /// This connector's local running-cost calculation was advanced to a new instant (CV8,
    /// I07/I08/I11/I12). Ignored if there's no active transaction - mirrors [`Self::CostUpdated`]
    /// exactly, including how it is cleared the moment the transaction starts or ends (see
    /// [`crate::state::EvseState::running_cost`]).
    ///
    /// Unlike almost every other event this connector receives, this one is never raised from
    /// inside the state machine's own `apply` - it needs a [`crate::clock::Clock`] reading and
    /// the tariff currently pricing the transaction, both of which live outside the clock-free
    /// core (see `crate::clock`'s docs). `crate::tariff::advance_running_cost` computes the new
    /// value in the adapter that already has both (the same one building the `TransactionEvent`
    /// this pricing is reported alongside) and sends it back as this event, purely to be stored.
    ///
    /// Boxed for the same reason [`Self::TariffAssigned`] is: [`crate::pricing::TransactionCost`]
    /// carries a growing list of charging periods, easily the largest payload any connector event
    /// not already boxed would carry.
    RunningCostAdvanced(alloc::boxed::Box<crate::pricing::TransactionCost>),
    /// A CSMS-initiated `Reset` (`ResetKind::Immediate`) covers this connector. Any state where
    /// a cable is engaged (`Connected`/`Locked`/`Authorizing`/`Starting`/`Charging`) is driven
    /// through the same fail-safe stop already used for a normal charging stop (open the
    /// contactor via `Stopping`, then unlock via `Finishing`) - reusing that path rather than a
    /// parallel one, per `CLAUDE.md`. A connector already `Available`/`Reserved`/`Unavailable`/
    /// faulted, or already mid-stop (`Stopping`/`Finishing`/`Unlocking`), ignores this. See
    /// `docs/ROADMAP.md` §2.
    ResetRequested,
    /// The connector is made available (OCPP `ChangeAvailability` addressing this specific
    /// connector). See `docs/ROADMAP.md` §7.
    SetAvailable,
    /// The connector is made unavailable (OCPP `ChangeAvailability` addressing this specific
    /// connector). See `docs/ROADMAP.md` §7.
    SetUnavailable,
    /// A hardware fault was detected on this connector (or a hardware command it issued failed -
    /// see [`crate::hardware::execute_hardware_command`]). Drives the connector fail-safe into
    /// `Faulted`, opening the contactor if it isn't already.
    FaultDetected,
    /// A previously reported fault on this connector has cleared. Only takes effect once the
    /// connector has confirmed its contactor is open (`FaultedSafe`); otherwise a no-op.
    FaultCleared,
    /// Hardware confirmed the current limit was applied, in response to a
    /// [`HardwareCommand::SetCurrentLimit`]. Carries the limit that was applied, in milliamps, or
    /// `None` if the previously-applied limit was removed - see
    /// [`crate::hardware::Connector::set_current_limit`].
    CurrentLimitConfirmed(Option<u32>),
    /// The charging-limit projection computed a new limit for this connector from the composite
    /// schedule (`docs/PRODUCTION-ROADMAP.md` B2.4, [`crate::smart_charging`]): `Some(limit_ma)`
    /// to limit the connector's draw, `None` when no installed profile imposes one any more.
    ///
    /// Recorded on [`crate::state::EvseState::charging_limits`] and dispatched to hardware as a
    /// [`HardwareCommand::SetCurrentLimit`] - but only when it actually differs from the limit
    /// already requested for this connector, so a projection that re-evaluates on every state
    /// change doesn't re-issue the same limit to hardware over and over.
    CurrentLimitComputed(Option<u32>),
}

/// A side effect of applying a [`ChargePointEvent`], to be carried out by the actor's caller
/// (a hardware command dispatched, a report sent to the CSMS, or simply that state changed).
///
/// `PartialEq` only, not `Eq`: [`Self::SmartChargingNotification`] can carry a
/// [`crate::state::ChargingSchedule`], whose periods carry `f64` limits.
#[derive(Debug, Clone, PartialEq)]
pub enum ChargePointEffect {
    /// [`crate::state::ChargePointState`] itself changed; the actor publishes the new state to
    /// [`crate::actor::ChargePointActor::subscribe`]/`state`.
    StateChanged,
    /// A hardware command must be carried out (lock/unlock a connector, open/close a contactor).
    HardwareCommand(HardwareCommand),
    /// A connector's OCPP-visible status changed; the Availability functional block reports
    /// this to the CSMS via StatusNotification.
    StatusNotification(ConnectorStatusChanged),
    /// A transaction started, was updated, or ended; the Transactions functional block reports
    /// this to the CSMS via TransactionEvent.
    TransactionEvent(TransactionEventOccurred),
    /// An identifier was presented and needs an authorization decision; the Authorization
    /// functional block asks the CSMS via Authorize.
    AuthorizationRequested(AuthorizationRequested),
    /// A security-relevant event occurred; the Security functional block reports this to the
    /// CSMS via SecurityEventNotification.
    SecurityEventOccurred(SecurityEvent),
    /// A reservation ended for a reason the CSMS did not ask for; the Reservation functional
    /// block reports this via ReservationStatusUpdate (2.x only).
    ReservationEnded(ReservationUpdate),
    /// Priority charging was granted or withdrawn by the charge point itself; the Smart Charging
    /// block reports this via `NotifyPriorityCharging` (2.1 only). A change the CSMS asked for
    /// produces no effect - see
    /// [`ChargePointEvent::PriorityChargingSet::locally_initiated`](ChargePointEvent::PriorityChargingSet).
    PriorityChargingChanged(PriorityChargingChange),
    /// A threshold or delta variable monitor fired; the variable monitoring engine reports this
    /// to the CSMS via `NotifyEvent` (2.x only). A periodic monitor never produces this - it
    /// reports on its own clock, independent of a value changing at all - see
    /// `crate::variable_monitoring::run_periodic_variable_monitors`.
    VariableMonitorTriggered(TriggeredMonitor),
    /// A battery-swap event occurred; the Battery Swap functional block reports this to the CSMS
    /// via `BatterySwap` (2.1 only). See [`crate::battery_swap::report_battery_swap_event`].
    BatterySwapEventOccurred(BatterySwapEvent),
    /// An external charging limit changed, or an EV reported something via ISO 15118; the Smart
    /// Charging block's `crate::smart_charging::notifications` forwards this to the CSMS as
    /// `NotifyChargingLimit`/`ClearedChargingLimit`/`NotifyEVChargingNeeds`/
    /// `NotifyEVChargingSchedule` (2.0.1/2.1 only). See `docs/PRODUCTION-ROADMAP.md` B2.8.
    SmartChargingNotification(SmartChargingNotification),
}

impl ChargePointEffect {
    /// The effect's variant name, as a low-cardinality `&'static str` suitable for a `tracing`
    /// field. See [`ChargePointEvent::name`] for why the routine log line carries this rather
    /// than a `{:?}` of the payload.
    ///
    /// Exhaustive with no wildcard arm on purpose: a new effect is a compile error here.
    pub fn name(&self) -> &'static str {
        match self {
            Self::StateChanged { .. } => "StateChanged",
            Self::HardwareCommand { .. } => "HardwareCommand",
            Self::StatusNotification { .. } => "StatusNotification",
            Self::TransactionEvent { .. } => "TransactionEvent",
            Self::AuthorizationRequested { .. } => "AuthorizationRequested",
            Self::SecurityEventOccurred { .. } => "SecurityEventOccurred",
            Self::ReservationEnded { .. } => "ReservationEnded",
            Self::PriorityChargingChanged { .. } => "PriorityChargingChanged",
            Self::VariableMonitorTriggered { .. } => "VariableMonitorTriggered",
            Self::BatterySwapEventOccurred { .. } => "BatterySwapEventOccurred",
            Self::SmartChargingNotification { .. } => "SmartChargingNotification",
        }
    }
}

/// A priority-charging grant the charge point made on its own, reported to the CSMS as
/// `NotifyPriorityCharging` (`docs/PRODUCTION-ROADMAP.md` B2.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriorityChargingChange {
    /// The transaction whose priority changed.
    pub transaction_id: TransactionId,
    /// Whether priority charging is now active for it.
    pub activated: bool,
}

/// Why a reservation ended, as OCPP's `ReservationUpdateStatusEnum` expresses it.
///
/// Only the reasons the CSMS needs telling about. A `CancelReservation` it sent itself produces
/// no update - reporting a change back to the peer that requested it is noise, and OCPP does not
/// ask for it. 2.1's third value, `NoTransaction` (the reservation was honoured but no
/// transaction followed), needs a timer this crate does not have yet; see
/// `docs/PRODUCTION-ROADMAP.md` B8.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservationEndReason {
    /// The reservation ran past its `expiryDateTime`.
    Expired,
    /// The charge point released the reservation because it could no longer honour it - the
    /// reserved connector faulted or was made unavailable.
    Removed,
}

/// A reservation ended without the CSMS asking, reported via ReservationStatusUpdate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReservationUpdate {
    /// The reservation that ended.
    pub id: ReservationId,
    /// Why it ended.
    pub reason: ReservationEndReason,
}

/// An [`IdToken`] was presented on a connector and needs an authorization decision, reported to
/// the CSMS via Authorize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationRequested {
    /// The presenting connector's EVSE index.
    pub evse_id: usize,
    /// The presenting connector's index within its EVSE.
    pub connector_id: usize,
    /// The identifier that was presented.
    pub id_token: IdToken,
    /// The ISO 15118 contract certificate presented with it, for a Plug & Charge authorization
    /// (OCPP use case C07) - `None` for every other way an identifier arrives, which is most of
    /// them: a card, an app, a remote start.
    ///
    /// Its presence changes the decision rules, not just the request:
    /// [`crate::authorization::run_authorization_requests`] will not fall back to the
    /// authorization cache or local list for a contract token when the CSMS is unreachable
    /// (C07.FR.07).
    pub contract: Option<ContractCertificate>,
}

/// A connector's [`ConnectorStatus`] changed, reported to the CSMS via StatusNotification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectorStatusChanged {
    /// The changed connector's EVSE index.
    pub evse_id: usize,
    /// The changed connector's index within its EVSE.
    pub connector_id: usize,
    /// The connector's new status, collapsed to OCPP 2.x's coarser 5-value status - what most
    /// version adapters need.
    pub status: ConnectorStatus,
    /// The connector's new state at its full, protocol-version-independent granularity.
    /// Versions with a richer wire status than `status` (e.g. 1.6J's `Preparing`/`Charging`/
    /// `Finishing`/`SuspendedEV`/`SuspendedEVSE`) derive their own mapping from this instead -
    /// see `docs/ROADMAP.md` §0. This effect still only fires when `status` itself changes (the
    /// same cadence every version has always gotten); it does not yet fire on every internal
    /// transition a richer version might also want reported.
    pub connector_state: ConnectorState,
}

/// Which kind of TransactionEvent this is (OCPP `TransactionEventEnumType`). `Updated` carries
/// *why* it fired - not part of the wire `eventType` itself (that's always `"Updated"`), but
/// needed to pick the right `triggerReason` in the version adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionEventKind {
    /// A new transaction began.
    Started,
    /// An in-progress transaction was updated.
    Updated(TransactionUpdateReason),
    /// The transaction ended.
    Ended,
}

/// Why a `TransactionEventKind::Updated` fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionUpdateReason {
    /// The transaction's `charging_state` changed (e.g. `EvConnected` -> `Charging`).
    ChargingStateChanged,
    /// A periodic meter reading was reported while charging.
    MeterValuePeriodic,
}

/// One in-flight transaction read back from durable storage at boot, carried by
/// [`ChargePointEvent::PersistedTransactionsRestored`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredTransaction {
    /// The transaction's connector's EVSE index.
    pub evse_id: usize,
    /// The transaction's connector's index within its EVSE.
    pub connector_id: usize,
    /// The transaction as it was last persisted before power was lost.
    pub transaction: Transaction,
}

/// One reservation read back from durable storage at boot, carried by
/// [`ChargePointEvent::PersistedReservationsRestored`]. Already filtered for expiry - see that
/// variant's docs and [`crate::persistence::restore_reservations`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredReservation {
    /// The reservation's connector's EVSE index.
    pub evse_id: usize,
    /// The reservation's connector's index within its EVSE.
    pub connector_id: usize,
    /// The reservation as it was last persisted before power was lost.
    pub reservation: Reservation,
}

/// One device model attribute value read back from durable storage at boot, carried by
/// [`ChargePointEvent::PersistedDeviceModelAttributesRestored`].
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveredDeviceModelAttribute {
    /// The attribute's component.
    pub component: Component,
    /// The attribute's variable.
    pub variable: Variable,
    /// Which attribute of `variable` this is.
    pub attribute_type: VariableAttributeType,
    /// The persisted value.
    pub value: String,
}

/// A transaction lifecycle event, reported to the CSMS via TransactionEvent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionEventOccurred {
    /// The transaction's connector's EVSE index.
    pub evse_id: usize,
    /// The transaction's connector's index within its EVSE.
    pub connector_id: usize,
    /// Which kind of TransactionEvent this is.
    pub kind: TransactionEventKind,
    /// A snapshot of the transaction at the time of this event.
    pub transaction: Transaction,
    /// OCPP's `offline` flag (E12, `docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV6.1): whether this
    /// event was generated while the CSMS was unreachable.
    ///
    /// **Always `false` when the state machine raises the event, and set later by whatever held
    /// it.** The charge point's own state has no notion of connectivity - that belongs to the
    /// connection, not to the transaction - so the fact is only knowable at the point delivery is
    /// attempted, and it is [`crate::offline_queue::OfflineQueue`] that knows: an event delivered
    /// on its first attempt went out online, and one the queue has had to hold did not. See
    /// [`OfflineQueue::marking_offline`](crate::offline_queue::OfflineQueue::marking_offline).
    pub offline: bool,
}

/// A hardware command the state machine needs carried out: lock/unlock a connector, or
/// open/close its contactor. Dispatched by
/// [`crate::hardware::execute_hardware_command`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareCommand {
    /// Engage the connector's physical lock.
    LockConnector {
        /// The targeted connector's EVSE index.
        evse_id: usize,
        /// The targeted connector's index within its EVSE.
        connector_id: usize,
    },
    /// Release the connector's physical lock.
    UnlockConnector {
        /// The targeted connector's EVSE index.
        evse_id: usize,
        /// The targeted connector's index within its EVSE.
        connector_id: usize,
    },
    /// Close the contactor, allowing energy to flow.
    CloseContactor {
        /// The targeted connector's EVSE index.
        evse_id: usize,
        /// The targeted connector's index within its EVSE.
        connector_id: usize,
    },
    /// Open the contactor, stopping energy flow.
    OpenContactor {
        /// The targeted connector's EVSE index.
        evse_id: usize,
        /// The targeted connector's index within its EVSE.
        connector_id: usize,
    },
    /// Reboots this EVSE's hardware (OCPP `Reset`). A charge-point-wide reset expands to one of
    /// these per EVSE. Dispatched via [`crate::hardware::Evse::reboot`]. See
    /// `docs/ROADMAP.md` §2.
    Reboot {
        /// The targeted EVSE's index.
        evse_id: usize,
    },
    /// Limit the connector's current draw to `limit_ma` milliamps, or remove any applied limit
    /// when it is `None`. Dispatched via [`crate::hardware::Connector::set_current_limit`], and
    /// emitted by [`ConnectorEvent::CurrentLimitComputed`] when the composite schedule's limit
    /// for this connector changes (`docs/PRODUCTION-ROADMAP.md` §"B2 — Smart charging").
    SetCurrentLimit {
        /// The targeted connector's EVSE index.
        evse_id: usize,
        /// The targeted connector's index within its EVSE.
        connector_id: usize,
        /// The current limit to apply, in milliamps, or `None` to remove the applied limit.
        limit_ma: Option<u32>,
    },
}
