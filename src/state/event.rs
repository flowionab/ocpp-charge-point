use crate::state::{
    ConnectorStatus, IdToken, MeterSample, RegistrationStatus, Reservation, StopReason,
    Transaction,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChargePointEvent {
    BootCompleted,
    SetAvailable,
    SetUnavailable,
    HardwareFault,
    FaultCleared,
    /// The CSMS answered a BootNotification with its registration decision.
    RegistrationStatusReceived(RegistrationStatus),
    Evse {
        evse_id: usize,
        event: EvseEvent,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvseEvent {
    SetAvailable,
    SetUnavailable,
    FaultDetected,
    FaultCleared,
    Connector {
        connector_id: usize,
        event: ConnectorEvent,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectorEvent {
    CableConnected,
    CableDisconnected,
    LockConfirmed,
    UnlockConfirmed,
    /// The driver/EV presented an identifier while the connector is locked; the Authorization
    /// functional block asks the CSMS whether it may start charging.
    IdTokenPresented(IdToken),
    /// The CSMS accepted the presented identifier (or, for the moment, some other decision
    /// producing the same effect - see `docs/ROADMAP.md` §3).
    ChargingAuthorized,
    /// The CSMS rejected the presented identifier.
    AuthorizationDenied,
    ContactorClosed,
    ContactorOpened,
    /// The CSMS asked to unlock the connector (OCPP `UnlockConnector`) while it's `Locked` with
    /// no active transaction. See `docs/ROADMAP.md` §6.
    RemoteUnlockRequested,
    /// The CSMS asked to start a transaction (OCPP `RequestStartTransaction`) while the
    /// connector is `Locked` with no active transaction. Unlike `IdTokenPresented`, this skips
    /// straight to `Starting` without a separate Authorize round-trip - the CSMS's own request
    /// is itself the authorization decision. See `docs/ROADMAP.md` §6.
    RemoteStartRequested,
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
    SetAvailable,
    SetUnavailable,
    FaultDetected,
    FaultCleared,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChargePointEffect {
    StateChanged,
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
}

/// An [`IdToken`] was presented on a connector and needs an authorization decision, reported to
/// the CSMS via Authorize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationRequested {
    pub evse_id: usize,
    pub connector_id: usize,
    pub id_token: IdToken,
}

/// A connector's [`ConnectorStatus`] changed, reported to the CSMS via StatusNotification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectorStatusChanged {
    pub evse_id: usize,
    pub connector_id: usize,
    pub status: ConnectorStatus,
}

/// Which kind of TransactionEvent this is (OCPP `TransactionEventEnumType`). `Updated` carries
/// *why* it fired - not part of the wire `eventType` itself (that's always `"Updated"`), but
/// needed to pick the right `triggerReason` in the version adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionEventKind {
    Started,
    Updated(TransactionUpdateReason),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactionEventOccurred {
    pub evse_id: usize,
    pub connector_id: usize,
    pub kind: TransactionEventKind,
    /// A snapshot of the transaction at the time of this event.
    pub transaction: Transaction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareCommand {
    LockConnector { evse_id: usize, connector_id: usize },
    UnlockConnector { evse_id: usize, connector_id: usize },
    CloseContactor { evse_id: usize, connector_id: usize },
    OpenContactor { evse_id: usize, connector_id: usize },
}
