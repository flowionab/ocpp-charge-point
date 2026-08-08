use crate::state::{ConnectorEvent, ConnectorStatus};

/// A connector's internal state machine, protocol-version-independent - version adapters project
/// this down to each OCPP version's own status enum (see
/// [`ConnectorState::availability_status`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorState {
    /// No cable connected, not reserved, no fault.
    Available,
    /// A cable is physically plugged in; the connector is being locked.
    Connected,
    /// The cable is locked in place, awaiting an identifier to be presented or a remote command.
    Locked,
    /// An identifier was presented and the CSMS's authorization decision is pending.
    Authorizing,
    /// Authorization succeeded (or a remote start was requested); the contactor is closing.
    Starting,
    /// The contactor is closed and energy is flowing.
    Charging,
    /// The contactor is closed but the **EV** has stopped drawing energy - a full battery, or a
    /// vehicle-side pause. The transaction is still running and the cable still locked; only the
    /// energy flow has paused, and the EV can resume it on its own.
    ///
    /// Reported by the hardware binding, which is the only thing that can tell *which side*
    /// stopped drawing (`docs/PRODUCTION-ROADMAP.md` B1.5). 1.6J has a wire status for this;
    /// 2.x moved the distinction onto the transaction's `chargingState`, so both versions can
    /// express it - see [`ConnectorStatus`] for why this crate's connector *status* cannot.
    SuspendedEv,
    /// The contactor is closed but the **EVSE** has stopped supplying energy - a smart-charging
    /// limit of 0 A, load management, or a local supply constraint. The transaction is still
    /// running; the charge point resumes it when whatever imposed the pause lifts.
    SuspendedEvse,
    /// Charging has stopped and the contactor is opening; the cable is still locked.
    Stopping,
    /// The contactor is open and the connector has been unlocked; the cable may still be
    /// plugged in.
    Finishing,
    /// Made unavailable (OCPP `ChangeAvailability`); not usable until made available again.
    Unavailable,
    /// A hardware fault was detected; the contactor is being (or has been) forced open.
    Faulted,
    /// A hardware fault was detected and the contactor has confirmed open - safe to unlock once
    /// the fault clears.
    FaultedSafe,
    /// The connector is unlocking, either after a normal session end or fault recovery.
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
            | Self::SuspendedEv
            | Self::SuspendedEvse
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
            // Expiry frees the connector exactly like a cancellation does; the two differ only in
            // what the CSMS is told afterwards - see `ReservationEndReason`.
            (Self::Reserved, ConnectorEvent::ReservationExpired) => (Self::Available, None),
            (Self::Connected, ConnectorEvent::LockConfirmed) => (Self::Locked, None),
            (Self::Locked, ConnectorEvent::IdTokenPresented(_)) => (Self::Authorizing, None),
            (Self::Locked, ConnectorEvent::RemoteUnlockRequested) => {
                (Self::Unlocking, Some(ConnectorCommand::Unlock))
            }
            (Self::Locked, ConnectorEvent::RemoteStartRequested(_)) => {
                (Self::Starting, Some(ConnectorCommand::CloseContactor))
            }
            (Self::Authorizing, ConnectorEvent::ChargingAuthorized(_)) => {
                (Self::Starting, Some(ConnectorCommand::CloseContactor))
            }
            (Self::Authorizing, ConnectorEvent::AuthorizationDenied) => (Self::Locked, None),
            (Self::Starting, ConnectorEvent::ContactorClosed) => (Self::Charging, None),
            // Suspension is a pause *within* a running transaction, not a stop: the contactor
            // stays closed and the cable stays locked, so the way out is either a resume or the
            // same `ChargingStopped` path any charging connector takes.
            (Self::Charging | Self::SuspendedEvse, ConnectorEvent::ChargingSuspendedByEv) => {
                (Self::SuspendedEv, None)
            }
            (Self::Charging | Self::SuspendedEv, ConnectorEvent::ChargingSuspendedByEvse) => {
                (Self::SuspendedEvse, None)
            }
            (Self::SuspendedEv | Self::SuspendedEvse, ConnectorEvent::ChargingResumed) => {
                (Self::Charging, None)
            }
            (
                Self::Charging | Self::SuspendedEv | Self::SuspendedEvse,
                ConnectorEvent::ChargingStopped(_),
            ) => (Self::Stopping, Some(ConnectorCommand::OpenContactor)),
            // A CSMS-initiated `Reset` (Immediate) interrupts any state where a cable is
            // engaged, reusing the exact same fail-safe stop (open contactor, then unlock via
            // `Stopping`/`Finishing`) as a normal charging stop rather than a parallel path
            // (e.g. `Faulted`/`FaultedSafe`, which would misreport this connector as faulted to
            // the CSMS). Already-idle states (`Available`/`Reserved`/`Unavailable`/faulted) and
            // already-settling ones (`Stopping`/`Finishing`/`Unlocking`) are unaffected - see
            // `docs/ROADMAP.md` §2.
            (
                Self::Connected
                | Self::Locked
                | Self::Authorizing
                | Self::Starting
                | Self::Charging
                | Self::SuspendedEv
                | Self::SuspendedEvse,
                ConnectorEvent::ResetRequested,
            ) => (Self::Stopping, Some(ConnectorCommand::OpenContactor)),
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

#[cfg(test)]
mod suspension_tests {
    use super::*;
    use crate::state::{ConnectorStatus, StopReason};

    fn charging() -> ConnectorState {
        ConnectorState::Charging
    }

    #[test]
    fn either_side_can_suspend_charging_and_resume_it() {
        let mut connector = charging();

        let transition = connector.apply(ConnectorEvent::ChargingSuspendedByEv);
        assert!(transition.changed);
        assert_eq!(connector, ConnectorState::SuspendedEv);
        // Suspension is a pause, not a stop: nothing is asked of the contactor.
        assert!(transition.command.is_none());

        assert!(connector.apply(ConnectorEvent::ChargingResumed).changed);
        assert_eq!(connector, ConnectorState::Charging);

        assert!(
            connector
                .apply(ConnectorEvent::ChargingSuspendedByEvse)
                .changed
        );
        assert_eq!(connector, ConnectorState::SuspendedEvse);
        assert!(connector.apply(ConnectorEvent::ChargingResumed).changed);
        assert_eq!(connector, ConnectorState::Charging);
    }

    #[test]
    fn a_suspension_can_change_sides_without_passing_through_charging() {
        // The EV stops drawing, then the EVSE cuts supply too (or the reverse) - a real sequence,
        // and reporting a spurious "charging" in between would be wrong.
        let mut connector = charging();
        connector.apply(ConnectorEvent::ChargingSuspendedByEv);

        assert!(
            connector
                .apply(ConnectorEvent::ChargingSuspendedByEvse)
                .changed
        );
        assert_eq!(connector, ConnectorState::SuspendedEvse);

        assert!(
            connector
                .apply(ConnectorEvent::ChargingSuspendedByEv)
                .changed
        );
        assert_eq!(connector, ConnectorState::SuspendedEv);
    }

    #[test]
    fn resuming_a_connector_that_is_not_suspended_does_nothing() {
        let mut connector = ConnectorState::Locked;
        let transition = connector.apply(ConnectorEvent::ChargingResumed);
        assert!(!transition.changed);
        assert_eq!(connector, ConnectorState::Locked);
    }

    #[test]
    fn a_suspended_connector_stops_through_the_same_path_a_charging_one_does() {
        for suspended in [
            ConnectorEvent::ChargingSuspendedByEv,
            ConnectorEvent::ChargingSuspendedByEvse,
        ] {
            let mut connector = charging();
            connector.apply(suspended);

            let transition = connector.apply(ConnectorEvent::ChargingStopped(StopReason::Local));

            assert_eq!(connector, ConnectorState::Stopping);
            assert!(matches!(
                transition.command,
                Some(ConnectorCommand::OpenContactor)
            ));
        }
    }

    #[test]
    fn a_suspended_connector_still_faults_and_still_resets_fail_safe() {
        let mut connector = charging();
        connector.apply(ConnectorEvent::ChargingSuspendedByEv);
        let transition = connector.apply(ConnectorEvent::FaultDetected);
        assert_eq!(connector, ConnectorState::Faulted);
        assert!(matches!(
            transition.command,
            Some(ConnectorCommand::OpenContactor)
        ));

        let mut connector = charging();
        connector.apply(ConnectorEvent::ChargingSuspendedByEvse);
        let transition = connector.apply(ConnectorEvent::ResetRequested);
        assert_eq!(connector, ConnectorState::Stopping);
        assert!(matches!(
            transition.command,
            Some(ConnectorCommand::OpenContactor)
        ));
    }

    #[test]
    fn a_suspended_connector_is_still_occupied_to_2_x() {
        // 2.x's connector status has no suspended value at all - it moved the distinction onto
        // the transaction's chargingState - so both suspended states must stay `Occupied` here.
        assert_eq!(
            ConnectorState::SuspendedEv.availability_status(),
            ConnectorStatus::Occupied
        );
        assert_eq!(
            ConnectorState::SuspendedEvse.availability_status(),
            ConnectorStatus::Occupied
        );
    }

    #[test]
    fn charging_cannot_be_suspended_before_it_starts() {
        for state in [
            ConnectorState::Locked,
            ConnectorState::Authorizing,
            ConnectorState::Starting,
        ] {
            let mut connector = state;
            assert!(
                !connector
                    .apply(ConnectorEvent::ChargingSuspendedByEv)
                    .changed
            );
            assert_eq!(connector, state);
        }
    }
}
