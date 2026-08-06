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

    /// Limits the current this connector may draw to at most `limit_ma` milliamps (matching
    /// [`MeterSample::current_ma`](crate::state::MeterSample::current_ma)'s unit, for enough
    /// resolution to be useful at typical EV charging currents). Requested via
    /// [`HardwareCommand::SetCurrentLimit`](crate::state::HardwareCommand::SetCurrentLimit).
    ///
    /// This is a hardware hook only (`docs/PRODUCTION-ROADMAP.md` §"B2 — Smart charging",
    /// B2.3): nothing in this crate today decides *what* limit to request - there is no
    /// charging-profile store or composite-schedule evaluation yet (B2.1/B2.2/B2.4), and no
    /// OCPP `SetChargingProfile` handling wired up to call this. Implementors should simply
    /// clamp the connector's contactor to the requested limit (or the nearest limit the
    /// hardware can actually enforce, if it can't hit `limit_ma` exactly) and return `Err` -
    /// never panic or retry internally - if the limit can't be honoured at all, which drives the
    /// connector into [`ConnectorState::Faulted`](crate::state::ConnectorState::Faulted) the
    /// same as any other hardware failure (see `CLAUDE.md`'s error-handling guidance).
    ///
    /// **Breaking change:** this is a new required method on an existing trait, batched with
    /// [`ChargePoint::capabilities`](crate::hardware::ChargePoint::capabilities) and
    /// [`Storage`](crate::hardware::Storage) so integrators absorb one break instead of three
    /// (`docs/PRODUCTION-ROADMAP.md` §5.2 C2.2). A connector with no way to limit current at all
    /// should simply return `Err` for every call - the crate never calls this unless
    /// [`Capabilities::smart_charging`](crate::hardware::Capabilities::smart_charging) was
    /// declared `true`, once the profile/schedule machinery (B2.1/B2.2/B2.4) that would trigger
    /// it exists.
    async fn set_current_limit(&self, limit_ma: u32) -> Result<(), Self::Error>;
}
