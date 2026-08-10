use crate::hardware::Connector;
use alloc::boxed::Box;

/// A single EVSE (Electric Vehicle Supply Equipment) on the charge point: one or more physical
/// connectors sharing the same power source, addressed as `evse_id` throughout this crate's
/// internal state and OCPP messages.
#[async_trait::async_trait]
pub trait Evse<C: Connector> {
    /// The error type returned by a failed [`reboot`](Self::reboot) on this EVSE.
    type Error: core::error::Error + Send + Sync + 'static;

    /// This EVSE's connectors, in a fixed order matching `connector_id` addressing throughout
    /// this crate (index 0 is `connector_id` 0, and so on). The set of connectors is assumed
    /// fixed for the lifetime of the charge point - there is no hook for adding or removing one
    /// at runtime.
    ///
    /// Synchronous, and deliberately so: this is consulted on the path of *every* hardware
    /// command ([`crate::hardware::execute_hardware_command`]), and an `async fn` returning a
    /// fixed slice would box a future per contactor operation to await something that never
    /// yields.
    fn connectors(&self) -> &[C];

    /// Reboots this EVSE's hardware (OCPP `Reset` - see `docs/ROADMAP.md` §2). Requested when a
    /// CSMS-initiated `Reset` targets this EVSE specifically, or the whole charge point (in
    /// which case every EVSE is rebooted individually - see `crate::reset`, and
    /// `crate::state::HardwareCommand::Reboot`'s docs for why this crate models "reboot" at
    /// EVSE granularity rather than a single whole-charge-point operation). By the time this is
    /// called, any transaction that was in progress on this EVSE's connectors has already been
    /// stopped fail-safely (contactor opened, then unlocked) by the state machine - this
    /// method's only job is to actually restart the hardware; it does not need to bring
    /// connectors back to any particular state itself.
    ///
    /// A real embedded implementation typically never returns at all here: the call triggers an
    /// actual hardware/MCU restart, and the process (and this whole actor) ends before a
    /// response could be observed. Returning `Err` (e.g. the reboot mechanism itself is
    /// unreachable, or a watchdog/relay fails to trigger) is reported the same way as any other
    /// hardware failure: it drives this EVSE into a fault state
    /// ([`EvseEvent::FaultDetected`](crate::state::EvseEvent::FaultDetected)) rather than
    /// panicking or silently doing nothing - see `CLAUDE.md`'s error-handling guidance. Never
    /// retry internally; let the crate decide what happens next.
    async fn reboot(&self) -> Result<(), Self::Error>;
}
