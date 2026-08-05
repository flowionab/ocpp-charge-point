use alloc::boxed::Box;

/// A single physical connector on an [`Evse`](crate::hardware::Evse) - the cable lock and
/// contactor an integrator wires up to real hardware.
///
/// Every method here is invoked by [`execute_hardware_command`](crate::hardware::execute_hardware_command)
/// in response to a [`HardwareCommand`](crate::state::HardwareCommand) the connector's internal
/// state machine emitted - implementors should perform the requested physical action and
/// nothing else; the crate, not the hardware binding, decides *when* each action happens.
///
/// Treat every call as fallible: sensors glitch, contactors stick, and locks jam. Return `Err`
/// rather than panicking or retrying internally - a failed call drives the connector into
/// [`ConnectorState::Faulted`](crate::state::ConnectorState::Faulted) (see `CLAUDE.md`'s
/// error-handling guidance), which is the correct, OCPP-visible way to surface a hardware
/// problem. Never let an error here take down the process.
#[async_trait::async_trait]
pub trait Connector {
    /// The error type returned by a failed hardware operation on this connector.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Engages the cable lock, physically preventing the cable from being unplugged.
    /// Requested when a connector transitions from `Connected` to `Locked`.
    async fn lock(&self) -> Result<(), Self::Error>;

    /// Releases the cable lock. Requested when unlocking a `Locked` connector with no active
    /// transaction (locally, via `UnlockConnector`, or after a fault clears).
    async fn unlock(&self) -> Result<(), Self::Error>;

    /// Closes the contactor, allowing energy to flow to the EV. Requested when a transaction is
    /// authorized and ready to start charging.
    async fn close_contactor(&self) -> Result<(), Self::Error>;

    /// Opens the contactor, stopping energy flow. Requested when a transaction stops (normally
    /// or due to a fault) and whenever a fault is detected, regardless of transaction state.
    async fn open_contactor(&self) -> Result<(), Self::Error>;
}
