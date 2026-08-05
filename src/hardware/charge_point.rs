use crate::hardware::Connector;
use crate::hardware::Evse;
use crate::hardware::HardwareCommandReceiver;
use crate::hardware::HardwareEventSender;
use alloc::boxed::Box;

/// The top-level hardware binding an integrator implements to plug real (or simulated) hardware
/// into this crate. This is the crate's primary integration point: everything else - protocol
/// handling, state machines, transaction lifecycle, networking - is this crate's own
/// responsibility (see `CLAUDE.md`).
#[async_trait::async_trait]
pub trait ChargePoint<E: Evse<C>, C: Connector> {
    /// The error type returned if [`start`](Self::start) fails to bring the hardware up.
    type StartError: core::error::Error + Send + Sync + 'static;

    /// The charge point's vendor name, reported to the CSMS in `BootNotification`.
    async fn vendor_name(&self) -> &str;

    /// The charge point's model name, reported to the CSMS in `BootNotification`.
    async fn model_name(&self) -> &str;

    /// This charge point's EVSEs, in a fixed order matching `evse_id` addressing throughout this
    /// crate (index 0 is `evse_id` 0, and so on). The set of EVSEs is assumed fixed for the
    /// lifetime of the charge point.
    async fn evses(&self) -> &[E];

    /// Brings the hardware up and hands over the two channels that connect it to this crate's
    /// state machine, for as long as the process runs:
    ///
    /// - `events`: push a [`ChargePointEvent`](crate::state::ChargePointEvent) whenever hardware
    ///   observes something the state machine needs to know about that isn't a direct response
    ///   to a command - e.g. a cable being plugged in
    ///   ([`ConnectorEvent::CableConnected`](crate::state::ConnectorEvent::CableConnected)), an
    ///   id token being presented, or a meter sample. There is no framework-driven polling loop;
    ///   the hardware binding decides when and how often to push these.
    /// - `commands`: drain [`HardwareCommand`](crate::state::HardwareCommand)s the state machine
    ///   emits (lock/unlock a connector, open/close a contactor) and carry them out against the
    ///   matching [`Connector`]. [`crate::hardware::execute_hardware_command`] does this dispatch
    ///   for you, including turning a failed or out-of-range command into the correct
    ///   fault-reporting event - most implementations should just loop
    ///   `execute_hardware_command(evses, commands.recv().await?, &events)` for as long as
    ///   `commands.recv()` keeps returning `Ok`.
    ///
    /// Returns once the hardware has started and both channels are being serviced (typically by
    /// spawning a background task before returning), or `Err` if startup itself fails - a
    /// startup failure is fatal to [`setup`](crate::setup), unlike a fault reported after
    /// startup via `events`, which the state machine handles as a normal (if unwelcome)
    /// transition.
    async fn start(
        &self,
        events: HardwareEventSender,
        commands: HardwareCommandReceiver,
    ) -> Result<(), Self::StartError>;
}
