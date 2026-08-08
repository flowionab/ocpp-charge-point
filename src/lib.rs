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
// G4.2: `CLAUDE.md` requires that a hardware fault never take down the charge point, and that
// every hardware binding call be treated as fallible. These two lints turn that from a stance into
// a compiler error - a panic on a path a glitching sensor or a hostile CSMS can reach is a charge
// point that stops charging cars.
//
// `not(test)` rather than an allow-list: test code panics on purpose (that is what an assertion
// is), and a `#[cfg(test)]` module is not shipped. Every remaining exemption in library code is a
// site-level `#[allow]` carrying a comment that says why the panic is unreachable.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::panic))]

extern crate alloc;

/// The charge point's actor: owns [`state::ChargePointState`] and serializes every
/// [`state::ChargePointEvent`] applied to it. See [`actor::ChargePointActor`].
pub mod actor;
pub mod authorization;
pub mod availability;
/// Battery Swap functional block (OCPP 2.1 only, `docs/PRODUCTION-ROADMAP.md` B8.3):
/// `RequestBatterySwap`/`BatterySwap`, for battery-swap station hardware. Behind the
/// `battery-swap` Cargo feature - see [`battery_swap`]'s own docs for why.
#[cfg(feature = "battery-swap")]
pub mod battery_swap;
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
    feature = "tariff-cost",
    feature = "periodic-event-stream"
))]
mod connect;
pub mod connection;
#[cfg(feature = "tariff-cost")]
pub mod cost;
pub mod customer_information;
pub mod data_transfer;
pub mod device_model;
pub mod diagnostics;
#[cfg(feature = "display-message")]
pub mod display_message;
pub mod executor;
pub mod firmware;
pub mod hardware;
#[cfg(feature = "ocpp_1_6")]
mod id_tag;
pub mod keepalive;
#[cfg(feature = "local-auth-list")]
pub mod local_authorization_list;
pub mod meter_values;
pub mod network_profile;
#[cfg(feature = "websocket")]
pub mod network_switch;
pub mod offline_queue;
/// A configurable ceiling on inbound OCPP-J WebSocket frame size, and the transport-level guard
/// that enforces it (F5.2). See [`payload_limit`]'s own docs for exactly what "enforces" covers.
pub mod payload_limit;
/// Periodic event streams functional block: `OpenPeriodicEventStream`/
/// `ClosePeriodicEventStream`/`AdjustPeriodicEventStream`/`GetPeriodicEventStream` inbound,
/// `NotifyPeriodicEventStream` outbound. See [`periodic_event_stream`]'s own docs for why this is
/// 2.1-only.
#[cfg(feature = "periodic-event-stream")]
pub mod periodic_event_stream;
pub mod persistence;
pub mod provisioning;
pub mod publish_firmware;
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
    feature = "tariff-cost",
    feature = "periodic-event-stream"
))]
mod setup;
/// The protocol-version-independent internal state model: [`state::ChargePointState`], its
/// per-EVSE/per-connector state machines, and the events/effects that drive and observe them.
pub mod state;
pub mod sync;
/// Tariff and cost functional block: the tariff store and per-transaction tariff assignment
/// (OCPP 2.1's `SetDefaultTariff`/`ChangeTransactionTariff`/`ClearTariffs`/`GetTariffs`). See
/// [`tariff`]'s own docs for why this is 2.1-only and stores/reports rather than computes a cost.
#[cfg(feature = "tariff-cost")]
pub mod tariff;
#[cfg(feature = "ocpp_1_6")]
mod topology;
pub mod transaction_status;
pub mod transactions;
pub mod variable_monitoring;
mod wire;

pub use self::builder::ChargePointBuilder;
#[cfg(all(
    feature = "std",
    feature = "websocket",
    feature = "ocpp_2_1",
    feature = "reservation",
    feature = "local-auth-list",
    feature = "tariff-cost",
    feature = "periodic-event-stream"
))]
pub use self::connect::{ConnectAndSetupError, connect_and_setup};
pub use self::runtime::ChargePointRuntime;
#[cfg(all(
    feature = "reservation",
    feature = "local-auth-list",
    feature = "tariff-cost",
    feature = "periodic-event-stream"
))]
pub use self::setup::setup;
/// The OCPP versions [`connect_and_setup`] can be asked to offer.
///
/// Re-exported because [`connect_and_setup`]'s signature names it: without this, no caller outside
/// this crate could construct the `versions` argument without taking a direct dependency on
/// `ocpp-client` and matching its exact version - which is precisely the coupling
/// `CLAUDE.md` asks this crate to absorb on an integrator's behalf.
///
/// Gated to match the upstream type, which is itself feature-gated: re-exporting it
/// unconditionally broke the `--no-default-features` build, since a no_std charge point compiles
/// none of the version adapters it enumerates.
#[cfg(any(feature = "ocpp_1_6", feature = "ocpp_2_0_1", feature = "ocpp_2_1"))]
pub use ocpp_client::OcppVersion;
