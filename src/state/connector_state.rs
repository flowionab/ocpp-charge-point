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
    /// Charging has stopped after the cable left the **EV** on a station whose
    /// `OCPPCommCtrlr.UnlockOnEVSideDisconnect` is `false`, and the contactor is opening.
    ///
    /// Identical to [`Self::Stopping`] in every respect except how it ends: E09.FR.03 says a
    /// station configured this way keeps hold of its own end of the cable, so this settles to
    /// [`Self::Locked`] rather than unlocking through [`Self::Finishing`]. It is a separate state
    /// because this crate's connector transition is a pure function of `(state, event, policy)` -
    /// by the time the contactor confirms open, the event that caused the stop is long gone, so
    /// the decision has to be carried in the state itself.
    StoppingLocked,
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

/// Where in a session OCPP says the transaction begins - `TxCtrlr.TxStartPoint`
/// (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV2.2).
///
/// OCPP models this as a *set*: the transaction starts once every configured condition holds. The
/// three this charge point can observe are strictly ordered along a session, so "all conditions
/// met" is simply the last of them - which is why this is an ordered enum and
/// [`Self::from_member_list`] takes the maximum rather than tracking a set of flags.
///
/// The other three values OCPP defines are not here, and their absence is enforced rather than
/// implied: `ParkingBayOccupancy` needs a bay sensor this crate has no binding for, `DataSigned`
/// needs signed meter values it does not produce, and `EnergyTransfer` needs a "current is
/// actually flowing" signal distinct from the contactor being closed. The variable's declared
/// `values_list` is narrowed to the three below, so a `SetVariables` naming one of the others is
/// `Rejected` by CV3's validation with a reason - rather than accepted and quietly treated as
/// something else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub(crate) enum TxStartPoint {
    /// The cable is connected and locked - the transaction covers the whole time the bay is
    /// occupied, including any authorization attempt that fails.
    EVConnected,
    /// The presented identifier was authorized. OCPP's default, and this crate's.
    #[default]
    Authorized,
    /// The contactor closed - the transaction covers only the time energy could actually flow.
    PowerPathClosed,
}

impl TxStartPoint {
    /// Parses `TxCtrlr.TxStartPoint`'s `MemberList` value, taking the latest point named.
    ///
    /// An empty or unrecognised value falls back to the default rather than erroring: the value
    /// reaching here has already passed CV3's validation against the declared `values_list`, so an
    /// unparseable one means the device model was written by something other than `SetVariables`
    /// (a hardware binding, a persisted restore), and a charge point is better off starting
    /// transactions the conventional way than not at all.
    pub fn from_member_list(value: &str) -> Self {
        value
            .split(',')
            .filter_map(|member| match member.trim() {
                "EVConnected" => Some(Self::EVConnected),
                "Authorized" => Some(Self::Authorized),
                "PowerPathClosed" => Some(Self::PowerPathClosed),
                _ => None,
            })
            .max()
            .unwrap_or_default()
    }
}

/// Where in a session OCPP says the transaction ends - `TxCtrlr.TxStopPoint`
/// (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV2.2).
///
/// **Stopping is not the mirror image of starting.** A start point is a condition that *begins* to
/// hold, and OCPP starts the transaction once every configured one does - so a configured set
/// resolves to its *latest* member (see [`TxStartPoint`]). A stop point is a condition that
/// *ceases* to hold, and a transaction cannot outlive the first of its conditions to lapse - so a
/// configured set resolves to its **earliest** member, which is why [`Self::from_member_list`]
/// takes the minimum where its start-point counterpart takes the maximum. The variants are
/// declared in the order they lapse along the end of a session, so `Ord` is that order.
///
/// Each point is one transition of this crate's own state machine, because that is the only place
/// where "the condition stopped holding" is observable here: the stop path is driven by an explicit
/// [`ConnectorEvent::ChargingStopped`] from the hardware binding rather than by polling each
/// condition. The three below are exactly the three [`TxStartPoint`] supports, and the declared
/// `values_list` is narrowed to them for the same reason - see [`TxStartPoint`]'s docs.
///
/// One combination OCPP itself warns about is reachable here: a stop point the charge point can
/// never observe lapsing leaves the transaction open forever. `EVConnected` on a station with
/// `OCPPCommCtrlr.UnlockOnEVSideDisconnect = false` is that case, because the cable is deliberately
/// never released (see [`ConnectorState::StoppingLocked`]). OCPP places the responsibility for
/// sensible start/stop combinations on the CSMS, and this crate does not second-guess a
/// configuration it was told to honour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub(crate) enum TxStopPoint {
    /// The authorization that permitted the session ended - the driver stopped it, the CSMS did,
    /// or the identifier was revoked. The earliest of the three, and the one an operator billing
    /// for time of use wants.
    Authorized,
    /// The contactor confirmed open, so energy can no longer flow. This crate's default, and what
    /// it did before `TxStopPoint` was honoured at all.
    #[default]
    PowerPathClosed,
    /// The cable left the connector, so the bay is free again - the transaction covers the whole
    /// time the connector was occupied, including the settling after energy stopped.
    EVConnected,
}

impl TxStopPoint {
    /// Parses `TxCtrlr.TxStopPoint`'s `MemberList` value, taking the *earliest* point named - see
    /// this type's docs for why the earliest and not the latest.
    ///
    /// An empty or unrecognised value falls back to the default, for the reason
    /// [`TxStartPoint::from_member_list`] gives: a charge point is better off ending transactions
    /// the conventional way than never ending them.
    pub fn from_member_list(value: &str) -> Self {
        value
            .split(',')
            .filter_map(|member| match member.trim() {
                "Authorized" => Some(Self::Authorized),
                "PowerPathClosed" => Some(Self::PowerPathClosed),
                "EVConnected" => Some(Self::EVConnected),
                _ => None,
            })
            .min()
            .unwrap_or_default()
    }
}

/// The configurable policy decisions a connector transition depends on
/// (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV2.4).
///
/// [`ConnectorState::apply`] is a pure function of `(state, event, policy)` on purpose: the rules
/// stay testable without a device model, and the *reading* of the device model happens once, in
/// [`crate::state::ChargePointState::apply_connector_event`]. A CSMS changing one of these takes
/// effect on the next event without a reboot, like every other live variable in this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConnectorPolicy {
    /// `TxCtrlr.StopTxOnEVSideDisconnect` - **E09 vs E10**, the branch this type exists for.
    ///
    /// `true` (OCPP's default, and the safe one): the cable leaving the EV ends the transaction.
    /// `false`: the transaction is *suspended* instead and can resume if the cable comes back,
    /// which is what a driver who briefly unplugs to reseat a connector expects.
    pub stop_tx_on_ev_side_disconnect: bool,
    /// `TxCtrlr.TxStartPoint` - where in the session the transaction begins (CV2.2).
    pub tx_start_point: TxStartPoint,
    /// `TxCtrlr.TxStopPoint` - where in the session the transaction ends (CV2.2).
    pub tx_stop_point: TxStopPoint,
    /// `OCPPCommCtrlr.UnlockOnEVSideDisconnect` - **E09.FR.02 vs E09.FR.03**.
    ///
    /// `true` (OCPP's default): the cable leaving the EV releases this station's end too, so the
    /// driver can take the cable away. `false`: the station keeps hold of it until the driver
    /// authorizes again, which is how an operator stops a cable being walked off with. Only
    /// consulted when [`Self::stop_tx_on_ev_side_disconnect`] is on - E10's suspend path leaves the
    /// connector locked anyway (E10.FR.01), and OCPP says the other combination's behaviour is
    /// undefined.
    pub unlock_on_ev_side_disconnect: bool,
    /// `TxCtrlr.StopTxOnInvalidId` - whether a mid-session deauthorization stops the transaction
    /// at once (E05, CV2.5).
    pub stop_tx_on_invalid_id: bool,
    /// `TxCtrlr.MaxEnergyOnInvalidId`, in Wh - the last allowance granted when
    /// `stop_tx_on_invalid_id` is off. `None`/`0` means no allowance, which stops immediately.
    pub max_energy_on_invalid_id_wh: Option<i64>,
}

impl Default for ConnectorPolicy {
    /// OCPP's own defaults, so a charge point whose binding removed the variables behaves the way
    /// the spec says an unconfigured one should.
    fn default() -> Self {
        Self {
            stop_tx_on_ev_side_disconnect: true,
            tx_start_point: TxStartPoint::Authorized,
            tx_stop_point: TxStopPoint::PowerPathClosed,
            unlock_on_ev_side_disconnect: true,
            // OCPP's default, and the safe one: an identifier the CSMS has just refused should
            // not keep drawing energy nobody will be billed for.
            stop_tx_on_invalid_id: true,
            max_energy_on_invalid_id_wh: None,
        }
    }
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
            | Self::StoppingLocked
            | Self::Finishing
            | Self::Unlocking => ConnectorStatus::Occupied,
            Self::Unavailable => ConnectorStatus::Unavailable,
            Self::Faulted | Self::FaultedSafe => ConnectorStatus::Faulted,
            Self::Reserved => ConnectorStatus::Reserved,
        }
    }

    pub(crate) fn apply(
        &mut self,
        event: ConnectorEvent,
        policy: ConnectorPolicy,
    ) -> ConnectorTransition {
        let (next, command) = match (*self, event) {
            // E05 (CV2.5): the identifier was revoked mid-session and the operator wants energy
            // cut at once. Same fail-safe stop as any other - contactor first.
            (
                Self::Charging | Self::SuspendedEv | Self::SuspendedEvse,
                ConnectorEvent::AuthorizationRevoked,
            ) if policy.stop_tx_on_invalid_id
                || policy.max_energy_on_invalid_id_wh.unwrap_or(0) <= 0 =>
            {
                (Self::Stopping, Some(ConnectorCommand::OpenContactor))
            }
            // E09.FR.03 (CV2.4): the same stop, on a station that keeps hold of its own end of the
            // cable. Checked before the ordinary E09 arm because it is the narrower rule - the
            // stop is identical, only its ending differs (see `Self::StoppingLocked`).
            (
                Self::Charging | Self::SuspendedEv | Self::SuspendedEvse,
                ConnectorEvent::CableDisconnected,
            ) if policy.stop_tx_on_ev_side_disconnect && !policy.unlock_on_ev_side_disconnect => {
                (Self::StoppingLocked, Some(ConnectorCommand::OpenContactor))
            }
            // E09/E10 (CV2.4): the cable left the EV while energy was flowing. Which of the two
            // use cases this is, is the operator's configuration - not something the charge point
            // gets to decide, and not something it may silently pick one of.
            //
            // Stopping opens the contactor first, exactly as any other stop does: the fail-safe
            // ordering is not negotiable just because the cable is already out on the EV side.
            (
                Self::Charging | Self::SuspendedEv | Self::SuspendedEvse,
                ConnectorEvent::CableDisconnected,
            ) if policy.stop_tx_on_ev_side_disconnect => {
                (Self::Stopping, Some(ConnectorCommand::OpenContactor))
            }
            // E10: suspended, not stopped. The transaction stays open and resumes if the cable
            // comes back - `ChargingResumed` from `SuspendedEv` is already the way back.
            (Self::Charging | Self::SuspendedEvse, ConnectorEvent::CableDisconnected) => {
                (Self::SuspendedEv, None)
            }
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
            // Plug & Charge takes the identical path: the certificate changes who can answer the
            // authorization question, not what the connector is waiting for while they do.
            (Self::Locked, ConnectorEvent::ContractCertificatePresented { .. }) => {
                (Self::Authorizing, None)
            }
            (Self::Locked, ConnectorEvent::RemoteUnlockRequested) => {
                (Self::Unlocking, Some(ConnectorCommand::Unlock))
            }
            (Self::Locked, ConnectorEvent::RemoteStartRequested { .. }) => {
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
            // E09.FR.03: contactor confirmed open, and no unlock. The cable is still latched with
            // no transaction running, which is exactly `Locked` - so the driver's next identifier
            // (or a CSMS `UnlockConnector`) is what releases it, as the requirement asks.
            (Self::StoppingLocked, ConnectorEvent::ContactorOpened) => (Self::Locked, None),
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
            // G05.FR.01 (CV11): a retention lock that will not engage must not be charged
            // through, so a lock failure takes the identical fail-safe path a fault does. What
            // makes it distinguishable to the CSMS is the `NotifyEvent` and the
            // `ConnectorPlugRetentionLock`/`Problem` variable that go with it, not a different
            // connector state - there is no safer state than the one a fault already produces.
            (_, ConnectorEvent::FaultDetected | ConnectorEvent::LockFailed) => {
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

        let transition = connector.apply(
            ConnectorEvent::ChargingSuspendedByEv,
            ConnectorPolicy::default(),
        );
        assert!(transition.changed);
        assert_eq!(connector, ConnectorState::SuspendedEv);
        // Suspension is a pause, not a stop: nothing is asked of the contactor.
        assert!(transition.command.is_none());

        assert!(
            connector
                .apply(ConnectorEvent::ChargingResumed, ConnectorPolicy::default())
                .changed
        );
        assert_eq!(connector, ConnectorState::Charging);

        assert!(
            connector
                .apply(
                    ConnectorEvent::ChargingSuspendedByEvse,
                    ConnectorPolicy::default()
                )
                .changed
        );
        assert_eq!(connector, ConnectorState::SuspendedEvse);
        assert!(
            connector
                .apply(ConnectorEvent::ChargingResumed, ConnectorPolicy::default())
                .changed
        );
        assert_eq!(connector, ConnectorState::Charging);
    }

    #[test]
    fn a_suspension_can_change_sides_without_passing_through_charging() {
        // The EV stops drawing, then the EVSE cuts supply too (or the reverse) - a real sequence,
        // and reporting a spurious "charging" in between would be wrong.
        let mut connector = charging();
        connector.apply(
            ConnectorEvent::ChargingSuspendedByEv,
            ConnectorPolicy::default(),
        );

        assert!(
            connector
                .apply(
                    ConnectorEvent::ChargingSuspendedByEvse,
                    ConnectorPolicy::default()
                )
                .changed
        );
        assert_eq!(connector, ConnectorState::SuspendedEvse);

        assert!(
            connector
                .apply(
                    ConnectorEvent::ChargingSuspendedByEv,
                    ConnectorPolicy::default()
                )
                .changed
        );
        assert_eq!(connector, ConnectorState::SuspendedEv);
    }

    #[test]
    fn resuming_a_connector_that_is_not_suspended_does_nothing() {
        let mut connector = ConnectorState::Locked;
        let transition =
            connector.apply(ConnectorEvent::ChargingResumed, ConnectorPolicy::default());
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
            connector.apply(suspended, ConnectorPolicy::default());

            let transition = connector.apply(
                ConnectorEvent::ChargingStopped(StopReason::Local),
                ConnectorPolicy::default(),
            );

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
        connector.apply(
            ConnectorEvent::ChargingSuspendedByEv,
            ConnectorPolicy::default(),
        );
        let transition = connector.apply(ConnectorEvent::FaultDetected, ConnectorPolicy::default());
        assert_eq!(connector, ConnectorState::Faulted);
        assert!(matches!(
            transition.command,
            Some(ConnectorCommand::OpenContactor)
        ));

        let mut connector = charging();
        connector.apply(
            ConnectorEvent::ChargingSuspendedByEvse,
            ConnectorPolicy::default(),
        );
        let transition =
            connector.apply(ConnectorEvent::ResetRequested, ConnectorPolicy::default());
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
                    .apply(
                        ConnectorEvent::ChargingSuspendedByEv,
                        ConnectorPolicy::default()
                    )
                    .changed
            );
            assert_eq!(connector, state);
        }
    }
}
