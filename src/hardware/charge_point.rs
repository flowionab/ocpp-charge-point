use crate::hardware::Capabilities;
use crate::hardware::Connector;
use crate::hardware::Evse;
use crate::hardware::HardwareCommandReceiver;
use crate::hardware::HardwareEventSender;
use alloc::boxed::Box;
use alloc::sync::Arc;

/// The top-level hardware binding an integrator implements to plug real (or simulated) hardware
/// into this crate. This is the crate's primary integration point: everything else - protocol
/// handling, state machines, transaction lifecycle, networking - is this crate's own
/// responsibility (see `CLAUDE.md`).
#[async_trait::async_trait]
pub trait ChargePoint<E: Evse<C>, C: Connector> {
    /// The error type returned if [`start`](Self::start) fails to bring the hardware up.
    type StartError: core::error::Error + Send + Sync + 'static;

    /// The charge point's vendor name, reported to the CSMS in `BootNotification`.
    fn vendor_name(&self) -> &str;

    /// The charge point's model name, reported to the CSMS in `BootNotification`.
    fn model_name(&self) -> &str;

    /// This charge point's EVSEs, in a fixed order matching `evse_id` addressing throughout this
    /// crate (index 0 is `evse_id` 0, and so on). The set of EVSEs is assumed fixed for the
    /// lifetime of the charge point.
    fn evses(&self) -> &[E];

    /// The fixed, static capabilities of this hardware (see [`Capabilities`]) - what optional
    /// functional blocks and physical/electrical abilities are actually present, so the crate
    /// (and, eventually, its OCPP advertisements - `docs/PRODUCTION-ROADMAP.md` §5.3) can adapt
    /// rather than assuming everything is available. Values are expected to be static for the
    /// lifetime of the charge point; this is called at most once during setup, not polled.
    ///
    /// **Breaking change:** this is a new *required* method on an existing trait
    /// (`docs/PRODUCTION-ROADMAP.md` §5.2 C2.2, batched with the smart-charging current-limit
    /// hook and the storage trait per §"B2 — Smart charging" B2.3 so integrators absorb one
    /// break instead of three). Every existing [`ChargePoint`] implementation must add this
    /// method; `Capabilities::default()` (see its docs) is the conservative starting point for
    /// an integrator that hasn't yet audited which capabilities their hardware actually has.
    fn capabilities(&self) -> Capabilities;

    /// The electrical facts OCPP requires this charge point to report - phase counts, connector
    /// types and per-EVSE maximum power (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV1.5).
    ///
    /// Like [`Self::capabilities`], read once during setup and never polled. Unlike it, this has a
    /// **default implementation** returning an all-unknown declaration, so adding it is not a
    /// breaking change: a station that ignores it still registers the required variables, just
    /// with nothing in them - which is what OCPP asks for and is honest about what the firmware
    /// was told.
    ///
    /// Fill it in if a CSMS should be able to see what your hardware actually is. Guessing is
    /// worse than leaving it blank: a CSMS reads `ConnectorType` and `SupplyPhases` to decide what
    /// it can ask this station for.
    fn electrical(&self) -> crate::hardware::ElectricalCharacteristics {
        crate::hardware::ElectricalCharacteristics::default()
    }

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
    ///   fault-reporting event.
    ///
    /// # Why the receiver is an `Arc<Self>`
    ///
    /// The command loop has to outlive this call - `start` must return promptly, because setup
    /// waits for it before registering with the CSMS - so it belongs in a spawned task, and that
    /// task needs to own something that borrows the connectors. With a `&self` receiver every
    /// implementation had to keep its own `Arc` *inside* the struct purely to have something to
    /// clone into that task. Taking `Arc<Self>` here means the handle already exists, and the
    /// whole implementation is:
    ///
    /// ```ignore
    /// async fn start(
    ///     self: Arc<Self>,
    ///     events: HardwareEventSender,
    ///     mut commands: HardwareCommandReceiver,
    /// ) -> Result<(), Self::StartError> {
    ///     spawn(async move {
    ///         while let Ok(command) = commands.recv().await {
    ///             execute_hardware_command(self.evses(), command, &events).await;
    ///         }
    ///     });
    ///     Ok(())
    /// }
    /// ```
    ///
    /// The crate holds the same `Arc`, so this costs no extra allocation - it is the one the
    /// runtime already made.
    ///
    /// # Errors
    ///
    /// Returns once the hardware has started and both channels are being serviced, or `Err` if
    /// startup itself fails - a startup failure is fatal to [`setup`](crate::setup), unlike a
    /// fault reported after startup via `events`, which the state machine handles as a normal (if
    /// unwelcome) transition.
    async fn start(
        self: Arc<Self>,
        events: HardwareEventSender,
        commands: HardwareCommandReceiver,
    ) -> Result<(), Self::StartError>;
}
