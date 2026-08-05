use alloc::vec::Vec;

use crate::state::{
    ConnectorState, ConnectorStatus, DeviceModelEvent, IdToken, LocalListEntry, MeterSample,
    RegistrationStatus, Reservation, SecurityEvent, StopReason, Transaction,
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
    /// An event mutating the Component/Variable device model (OCPP `GetVariables`/
    /// `SetVariables`, or 1.6J's `GetConfiguration`/`ChangeConfiguration` projection onto it) -
    /// see `crate::state::device_model` and `crate::device_model`. See `docs/ROADMAP.md` §2.
    DeviceModel(DeviceModelEvent),
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
    /// The CSMS reported a new running total cost for this connector's active transaction (OCPP
    /// `CostUpdated`). Ignored if there's no active transaction. See `docs/ROADMAP.md` §9.
    CostUpdated(f64),
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
}
