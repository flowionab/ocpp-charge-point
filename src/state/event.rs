use crate::state::{ConnectorStatus, RegistrationStatus, StopReason, Transaction};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorEvent {
    CableConnected,
    CableDisconnected,
    LockConfirmed,
    UnlockConfirmed,
    ChargingAuthorized,
    ContactorClosed,
    ContactorOpened,
    /// Charging has stopped (locally, remotely, or the EV finished) and the contactor should
    /// open. Not used for hardware-fault-driven stops - those go through `FaultDetected`.
    ChargingStopped(StopReason),
    SetAvailable,
    SetUnavailable,
    FaultDetected,
    FaultCleared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargePointEffect {
    StateChanged,
    HardwareCommand(HardwareCommand),
    /// A connector's OCPP-visible status changed; the Availability functional block reports
    /// this to the CSMS via StatusNotification.
    StatusNotification(ConnectorStatusChanged),
    /// A transaction started, was updated, or ended; the Transactions functional block reports
    /// this to the CSMS via TransactionEvent.
    TransactionEvent(TransactionEventOccurred),
}

/// A connector's [`ConnectorStatus`] changed, reported to the CSMS via StatusNotification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectorStatusChanged {
    pub evse_id: usize,
    pub connector_id: usize,
    pub status: ConnectorStatus,
}

/// Which kind of TransactionEvent this is (OCPP `TransactionEventEnumType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionEventKind {
    Started,
    Updated,
    Ended,
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
