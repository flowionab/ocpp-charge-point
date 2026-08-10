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
///
/// # `Ok` means the hardware moved, not that the command was sent
///
/// This is the sharpest contract in this trait, because getting it wrong fails *open*.
/// [`execute_hardware_command`](crate::hardware::execute_hardware_command) turns an `Ok(())` from
/// [`lock`](Self::lock) directly into
/// [`ConnectorEvent::LockConfirmed`](crate::state::ConnectorEvent::LockConfirmed), and the same
/// for the other three - there is no separate confirmation step. So returning `Ok` once a command
/// has been *issued*, before the lock pin has actually engaged or the contactor has actually
/// closed, tells the state machine something untrue: it will believe an unlocked connector is
/// locked and let a transaction start behind it.
///
/// Await whatever confirmation the hardware offers - a limit switch, an auxiliary contact, a
/// driver acknowledgement - and return `Ok` only once the physical state really is what the
/// method name says. If the hardware offers no feedback at all, say so in your own
/// documentation: that is a property of the machine, not something to paper over here. If
/// confirmation times out, return `Err`, which faults the connector fail-safe.
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
    /// resolution to be useful at typical EV charging currents), or removes any
    /// previously-applied limit when `limit_ma` is `None`. Requested via
    /// [`HardwareCommand::SetCurrentLimit`](crate::state::HardwareCommand::SetCurrentLimit).
    ///
    /// `None` means "no CSMS-imposed limit applies any more - go back to whatever this connector
    /// can do on its own". It is **not** "stop charging": a zero limit is expressed as
    /// `Some(0)`, which OCPP uses to suspend charging without ending the transaction, and the two
    /// must not be conflated. Only the hardware knows its own maximum, which is why this crate
    /// asks for the limit to be *removed* rather than naming a number it would have to invent.
    ///
    /// Called by the charging-limit projection (`docs/PRODUCTION-ROADMAP.md` B2.4) whenever the
    /// composite schedule's limit for this connector changes - a profile installed or cleared, a
    /// schedule period boundary passing, or a transaction starting or ending. The crate never
    /// calls it unless [`Capabilities::smart_charging`](crate::hardware::Capabilities::smart_charging)
    /// was declared `true`.
    ///
    /// Implementors should clamp the connector to the requested limit (or the nearest limit the
    /// hardware can actually enforce, if it can't hit `limit_ma` exactly) and return `Err` -
    /// never panic or retry internally - if the limit can't be honoured at all, which drives the
    /// connector into [`ConnectorState::Faulted`](crate::state::ConnectorState::Faulted) the
    /// same as any other hardware failure (see `CLAUDE.md`'s error-handling guidance).
    ///
    /// **Breaking change:** this method arrived with
    /// [`ChargePoint::capabilities`](crate::hardware::ChargePoint::capabilities) and
    /// [`Storage`](crate::hardware::Storage) (`docs/PRODUCTION-ROADMAP.md` §5.2 C2.2) taking a
    /// bare `u32`; it takes an `Option<u32>` from B2 onwards. The change was made while the hook
    /// still had no caller at all rather than after one existed, since a signature with no way to
    /// say "the limit is gone" would have forced every integrator to guess at their own maximum.
    async fn set_current_limit(&self, limit_ma: Option<u32>) -> Result<(), Self::Error>;
}
