#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod actor;
pub mod authorization;
pub mod availability;
pub mod clock;
#[cfg(all(feature = "std", feature = "websocket", feature = "ocpp_2_1"))]
mod connect;
pub mod executor;
pub mod hardware;
pub mod provisioning;
pub mod remote_control;
pub mod reservation;
mod runtime;
mod setup;
pub mod state;
pub mod sync;
pub mod transactions;

#[cfg(all(feature = "std", feature = "websocket", feature = "ocpp_2_1"))]
pub use self::connect::{ConnectAndSetupError, connect_and_setup};
pub use self::runtime::ChargePointRuntime;
pub use self::setup::setup;
