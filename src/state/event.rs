use alloc::string::String;
use alloc::vec::Vec;
use chrono::{DateTime, Utc};

use crate::clock::MonotonicInstant;
use crate::hardware::Capabilities;
use crate::state::{
    AuthorizationCacheEntry, AuthorizationStatus, ChargingProfile, ChargingProfileCriteria,
    ChargingProfileId, ChargingProfileScope, Component, ConnectorState, ConnectorStatus,
    DeviceModelEvent, DisplayMessageId, DisplayedMessage, IdToken, InstalledChargingProfile,
    LocalListEntry, MeterSample, NetworkConnectionProfile, NetworkProfileSlot, RegistrationStatus,
    Reservation, ReservationId, ResetKind, ResetTarget, SecurityEvent, StopReason, Transaction,
    TransactionId, Variable, VariableAttributeType,
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
    /// The hardware binding's declared [`Capabilities`], captured once during
    /// [`crate::builder::ChargePointBuilder::start`]. The single source of truth every
    /// capability-propagation surface (handler registration, the device model's `*Ctrlr.Available`
    /// variables, 1.6J `SupportedFeatureProfiles`) ultimately derives from - see
    /// `docs/PRODUCTION-ROADMAP.md` §5.3 (C3).
    CapabilitiesDeclared(Capabilities),
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
    /// The CSMS installed/replaced a display message (OCPP `SetDisplayMessage`), accepted by
    /// [`crate::display_message::handle_set_display_message`]. Boxed for the same reason
    /// [`Self::ChargingProfileSet`]'s profile is - the largest thing this variant could carry
    /// (message content up to 1024 bytes) should not be paid for by every event on the mailbox.
    DisplayMessageSet(alloc::boxed::Box<DisplayedMessage>),
    /// The CSMS removed a display message (OCPP `ClearDisplayMessage`), accepted by
    /// [`crate::display_message::handle_clear_display_message`].
    DisplayMessageCleared(DisplayMessageId),
    /// An event addressed to one EVSE (or, via [`EvseEvent::Connector`], one of its connectors).
    Evse {
        /// The addressed EVSE's index.
        evse_id: usize,
        /// The event to apply to that EVSE.
        event: EvseEvent,
    },
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
    /// The driver/EV presented an identifier while the connector is locked; the Authorization
    /// functional block asks the CSMS whether it may start charging.
    IdTokenPresented(IdToken),
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
    /// is itself the authorization decision. Carries the identifier the CSMS supplied, recorded
    /// on the [`Transaction`] this starts. See `docs/ROADMAP.md` §6.
    RemoteStartRequested(IdToken),
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
