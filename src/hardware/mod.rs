//! The hardware abstraction layer: the traits an integrator implements to plug real (or
//! simulated) hardware into this crate, and the small pieces of glue connecting them to the
//! charge point's actor. See [`ChargePoint`] for the entry point.
//!
//! Per `CLAUDE.md`, this is the crate's primary - ideally *only* - integration surface:
//! protocol handling, state machines, transaction lifecycle, and networking are this crate's own
//! responsibility, not something an integrator needs to touch.

mod charge_point;
mod command_executor;
mod command_receiver;
mod connector;
mod event_sender;
mod evse;

pub use self::charge_point::ChargePoint;
pub use self::command_executor::execute_hardware_command;
pub use self::command_receiver::HardwareCommandReceiver;
pub use self::connector::Connector;
pub use self::event_sender::HardwareEventSender;
pub use self::evse::Evse;
