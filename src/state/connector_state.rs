use crate::state::ConnectorEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorState {
    Available,
    Connected,
    Locked,
    Starting,
    Charging,
    Unavailable,
    Faulted,
    FaultedSafe,
    Unlocking,
}

pub(crate) enum ConnectorCommand {
    Lock,
    Unlock,
    CloseContactor,
    OpenContactor,
}

pub(crate) struct ConnectorTransition {
    pub changed: bool,
    pub command: Option<ConnectorCommand>,
}

impl ConnectorState {
    pub(crate) fn apply(&mut self, event: ConnectorEvent) -> ConnectorTransition {
        let (next, command) = match (*self, event) {
            (Self::Available, ConnectorEvent::CableConnected) => {
                (Self::Connected, Some(ConnectorCommand::Lock))
            }
            (Self::Connected, ConnectorEvent::LockConfirmed) => (Self::Locked, None),
            (Self::Locked, ConnectorEvent::ChargingAuthorized) => {
                (Self::Starting, Some(ConnectorCommand::CloseContactor))
            }
            (Self::Starting, ConnectorEvent::ContactorClosed) => (Self::Charging, None),
            (Self::Connected, ConnectorEvent::CableDisconnected) => (Self::Available, None),
            (_, ConnectorEvent::SetUnavailable) => (Self::Unavailable, None),
            (Self::Faulted | Self::FaultedSafe, ConnectorEvent::SetAvailable) => (*self, None),
            (_, ConnectorEvent::SetAvailable) => (Self::Available, None),
            (Self::Faulted, ConnectorEvent::ContactorOpened) => (Self::FaultedSafe, None),
            (Self::FaultedSafe, ConnectorEvent::FaultCleared) => {
                (Self::Unlocking, Some(ConnectorCommand::Unlock))
            }
            (Self::Unlocking, ConnectorEvent::UnlockConfirmed) => (Self::Available, None),
            (_, ConnectorEvent::FaultDetected) => {
                (Self::Faulted, Some(ConnectorCommand::OpenContactor))
            }
            (state, _) => (state, None),
        };

        let changed = *self != next;
        *self = next;
        ConnectorTransition { changed, command }
    }
}
