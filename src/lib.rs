//! Firmware for an EV charge point: charge-point lifecycle and charging-state behaviour,
//! presented to a central system (CSMS) as an OCPP-capable charge point.
//!
//! The primary target is a fully compliant OCPP 2.1 implementation; OCPP 2.0.1 and 1.6J are
//! supported by projecting the same protocol-version-independent internal state down to each
//! version's capabilities. Integrators only need to implement the hardware bindings exposed by
//! [`hardware`] - protocol handling, state machines, transaction lifecycle, and networking are
//! this crate's own responsibility. See the repository's `CLAUDE.md` for the full architectural
//! guidance this crate follows.
#![cfg_attr(not(feature = "std"), no_std)]
#![warn(missing_docs)]

extern crate alloc;

/// The charge point's actor: owns [`state::ChargePointState`] and serializes every
/// [`state::ChargePointEvent`] applied to it. See [`actor::ChargePointActor`].
pub mod actor;
pub mod authorization;
pub mod availability;
mod builder;
pub mod certificates;
pub mod clock;
// `connect_and_setup` hands its negotiated client straight to `setup()`, so it needs the same
// `reservation`/`local-auth-list`/`tariff-cost` features `setup()` itself does - see the `setup`
// module gate below.
#[cfg(all(
    feature = "std",
    feature = "websocket",
    feature = "ocpp_2_1",
    feature = "reservation",
    feature = "local-auth-list",
    feature = "tariff-cost"
))]
mod connect;
pub mod connection;
#[cfg(feature = "tariff-cost")]
pub mod cost;
pub mod data_transfer;
pub mod device_model;
pub mod diagnostics;
pub mod executor;
pub mod firmware;
pub mod hardware;
#[cfg(feature = "ocpp_1_6")]
mod id_tag;
#[cfg(feature = "local-auth-list")]
pub mod local_authorization_list;
pub mod meter_values;
pub mod network_profile;
#[cfg(feature = "websocket")]
pub mod network_switch;
pub mod offline_queue;
pub mod persistence;
pub mod provisioning;
pub mod refusal;
pub mod remote_control;
pub mod reporting;
#[cfg(feature = "reservation")]
pub mod reservation;
pub mod reset;
mod runtime;
pub mod security;
pub mod security_profile;
pub mod smart_charging;
// `setup()` is the "everything on" wrapper (see its module docs): it bounds its CSMS type by
// every functional block's traits at once, including the three that are genuinely gated behind
// Cargo features today (reservation, local-auth-list, tariff-cost). Compiling it with any of
// those off would mean calling handler methods on modules that no longer exist, so the whole
// module - and the `setup` re-export below - only exists when all three are enabled.
// `ChargePointBuilder` is unaffected: each of its registration methods is independently gated
// (see `builder.rs`), so callers who disable a capability feature simply skip that method.
#[cfg(all(
    feature = "reservation",
    feature = "local-auth-list",
    feature = "tariff-cost"
))]
mod setup;
/// The protocol-version-independent internal state model: [`state::ChargePointState`], its
/// per-EVSE/per-connector state machines, and the events/effects that drive and observe them.
pub mod state;
pub mod sync;
#[cfg(feature = "ocpp_1_6")]
mod topology;
pub mod transactions;

pub use self::builder::ChargePointBuilder;
#[cfg(all(
    feature = "std",
    feature = "websocket",
    feature = "ocpp_2_1",
    feature = "reservation",
    feature = "local-auth-list",
    feature = "tariff-cost"
))]
pub use self::connect::{ConnectAndSetupError, connect_and_setup};
pub use self::runtime::ChargePointRuntime;
#[cfg(all(
    feature = "reservation",
    feature = "local-auth-list",
    feature = "tariff-cost"
))]
pub use self::setup::setup;
