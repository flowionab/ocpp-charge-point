use crate::state::{ConnectorEvent, ConnectorStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorState {
    Available,
    Connected,
    Locked,
    /// An identifier was presented and the CSMS's authorization decision is pending.
    Authorizing,
    Starting,
    Charging,
    /// Charging has stopped and the contactor is opening; the cable is still locked.
    Stopping,
    /// The contactor is open and the connector has been unlocked; the cable may still be
    /// plugged in.
    Finishing,
    Unavailable,
    Faulted,
    FaultedSafe,
    Unlocking,
    /// The CSMS has reserved this connector (OCPP `ReserveNow`) for a specific id token; no
    /// cable is connected yet. See `docs/ROADMAP.md` §8.
    Reserved,
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
    /// The status reported to the CSMS via StatusNotification for this connector state.
    ///
    /// Several internal states map to the same OCPP-visible status (e.g. `Locked` and
    /// `Charging` are both `Occupied`) - only an actual change in this mapped value should
    /// trigger a StatusNotification, not every internal transition.
    pub fn availability_status(&self) -> ConnectorStatus {
        match self {
            Self::Available => ConnectorStatus::Available,
            Self::Connected
            | Self::Locked
            | Self::Authorizing
            | Self::Starting
            | Self::Charging
            | Self::Stopping
            | Self::Finishing
            | Self::Unlocking => ConnectorStatus::Occupied,
            Self::Unavailable => ConnectorStatus::Unavailable,
            Self::Faulted | Self::FaultedSafe => ConnectorStatus::Faulted,
            Self::Reserved => ConnectorStatus::Reserved,
        }
    }

    pub(crate) fn apply(&mut self, event: ConnectorEvent) -> ConnectorTransition {
        let (next, command) = match (*self, event) {
            (Self::Available | Self::Reserved, ConnectorEvent::CableConnected) => {
                (Self::Connected, Some(ConnectorCommand::Lock))
            }
            (Self::Available, ConnectorEvent::Reserved(_)) => (Self::Reserved, None),
            (Self::Reserved, ConnectorEvent::ReservationCancelled) => (Self::Available, None),
            (Self::Connected, ConnectorEvent::LockConfirmed) => (Self::Locked, None),
            (Self::Locked, ConnectorEvent::IdTokenPresented(_)) => (Self::Authorizing, None),
            (Self::Locked, ConnectorEvent::RemoteUnlockRequested) => {
                (Self::Unlocking, Some(ConnectorCommand::Unlock))
            }
            (Self::Locked, ConnectorEvent::RemoteStartRequested) => {
                (Self::Starting, Some(ConnectorCommand::CloseContactor))
            }
            (Self::Authorizing, ConnectorEvent::ChargingAuthorized) => {
                (Self::Starting, Some(ConnectorCommand::CloseContactor))
            }
            (Self::Authorizing, ConnectorEvent::AuthorizationDenied) => (Self::Locked, None),
            (Self::Starting, ConnectorEvent::ContactorClosed) => (Self::Charging, None),
            (Self::Charging, ConnectorEvent::ChargingStopped(_)) => {
                (Self::Stopping, Some(ConnectorCommand::OpenContactor))
            }
            (Self::Stopping, ConnectorEvent::ContactorOpened) => {
                (Self::Finishing, Some(ConnectorCommand::Unlock))
            }
            (Self::Finishing, ConnectorEvent::UnlockConfirmed) => (Self::Connected, None),
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
