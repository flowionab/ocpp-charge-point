/// The kind of CSMS-initiated `Reset` requested, matching (a projection of) OCPP's `ResetEnum`.
///
/// OCPP 2.1 adds a third wire value, `ImmediateAndResume` (reset immediately, then automatically
/// resume the transaction that was interrupted) - this crate doesn't yet model resuming a
/// transaction across a reboot, so the OCPP 2.1 adapter projects `ImmediateAndResume` down to
/// `Immediate` (see `crate::reset`'s `ocpp_2_1` module).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetKind {
    /// Interrupt anything in progress right away - fail-safely (open the contactor, then unlock)
    /// - and reboot immediately.
    Immediate,
    /// Wait until the target has no transaction in progress, then reboot - immediately, if it
    /// already doesn't.
    OnIdle,
}

/// The scope of a CSMS-initiated `Reset` request (OCPP's optional `evseId`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetTarget {
    /// Every EVSE on the charge point.
    ChargePoint,
    /// Just this one EVSE.
    Evse {
        /// The targeted EVSE's index.
        evse_id: usize,
    },
}

/// A `Reset` request recorded against [`ChargePointState`](crate::state::ChargePointState) while
/// it waits for `target` to settle - idle, for `ResetKind::OnIdle`, or for the fail-safe stop
/// `ResetKind::Immediate` kicked off to finish confirming with hardware - before the reboot
/// itself is dispatched as a
/// [`HardwareCommand::Reboot`](crate::state::HardwareCommand::Reboot). See `docs/ROADMAP.md` §2.
///
/// Only one `Reset` request is tracked at a time: a new one (of either kind, at either scope)
/// supersedes whatever was previously pending, mirroring how e.g. a second `ReserveNow` on an
/// already-reserved connector would simply overwrite the first in most deployments' expectations
/// - this crate does not queue reset requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingReset {
    /// The scope this reset applies to.
    pub target: ResetTarget,
    /// Whether to interrupt anything in progress right away, or wait for `target` to go idle
    /// first.
    pub kind: ResetKind,
}
