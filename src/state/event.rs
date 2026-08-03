use crate::state::{ConnectorStatus, RegistrationStatus};

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
}

/// A connector's [`ConnectorStatus`] changed, reported to the CSMS via StatusNotification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectorStatusChanged {
    pub evse_id: usize,
    pub connector_id: usize,
    pub status: ConnectorStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareCommand {
    LockConnector { evse_id: usize, connector_id: usize },
    UnlockConnector { evse_id: usize, connector_id: usize },
    CloseContactor { evse_id: usize, connector_id: usize },
    OpenContactor { evse_id: usize, connector_id: usize },
}
