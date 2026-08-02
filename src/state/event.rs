#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargePointEvent {
    BootCompleted,
    SetAvailable,
    SetUnavailable,
    HardwareFault,
    FaultCleared,
    Evse { evse_id: usize, event: EvseEvent },
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareCommand {
    LockConnector { evse_id: usize, connector_id: usize },
    UnlockConnector { evse_id: usize, connector_id: usize },
    CloseContactor { evse_id: usize, connector_id: usize },
    OpenContactor { evse_id: usize, connector_id: usize },
}
