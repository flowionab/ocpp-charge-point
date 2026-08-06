//! This crate's protocol-version-independent model of *why* a boot's `BootNotification` is being
//! sent for a reason other than an ordinary, uncommanded restart - the concept OCPP 2.x's
//! `BootNotificationRequest.reason` (`BootReasonEnum`) carries. OCPP 1.6J has no equivalent field
//! at all (`BootNotification.req` predates it), so this has no effect on a 1.6J connection - see
//! `crate::provisioning`'s 1.6J adapter.
//!
//! There are only two causes this crate can currently produce, both originating from
//! [`crate::reset::handle_reset`]: a CSMS-initiated `Reset`. Absence of a [`BootReasonCause`] (a
//! bare `None` wherever this type is used) means something else - a power cut, a watchdog
//! reset, or a crash - caused the restart; see `crate::persistence::BootReasonStore`'s docs for
//! how that's told apart from a commanded one, and `crate::provisioning`'s per-version
//! `build_request` functions for the wire mapping (including what `None` maps to and why).

/// A CSMS-commanded reason this boot's `BootNotification` is being sent, recorded by
/// [`crate::reset::handle_reset`] before the reboot it causes reaches hardware, and read back by
/// [`crate::provisioning::register`]/[`register_until_accepted`](crate::provisioning::register_until_accepted)
/// on the next boot via [`crate::persistence::BootReasonStore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BootReasonCause {
    /// A CSMS `Reset` accepted with `ResetKind::Immediate`: the reboot was commanded and carried
    /// out right away. Maps to OCPP's `BootReasonEnum::RemoteReset`.
    RemoteReset,
    /// A CSMS `Reset` accepted with `ResetKind::OnIdle`: the reboot was commanded, but deferred
    /// until the target had no transaction in progress. Maps to OCPP's
    /// `BootReasonEnum::ScheduledReset`.
    ScheduledReset,
}

impl From<super::ResetKind> for BootReasonCause {
    /// The mapping [`crate::reset::handle_reset`] applies when recording a reset's cause: the
    /// axis `ResetKind` models (interrupt now vs. wait for idle) is exactly the axis
    /// `RemoteReset`/`ScheduledReset` distinguish.
    fn from(kind: super::ResetKind) -> Self {
        match kind {
            super::ResetKind::Immediate => BootReasonCause::RemoteReset,
            super::ResetKind::OnIdle => BootReasonCause::ScheduledReset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::BootReasonCause;
    use crate::state::ResetKind;

    #[test]
    fn immediate_maps_to_remote_reset() {
        assert_eq!(
            BootReasonCause::from(ResetKind::Immediate),
            BootReasonCause::RemoteReset
        );
    }

    #[test]
    fn on_idle_maps_to_scheduled_reset() {
        assert_eq!(
            BootReasonCause::from(ResetKind::OnIdle),
            BootReasonCause::ScheduledReset
        );
    }
}
