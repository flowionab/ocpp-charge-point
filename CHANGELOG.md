# Changelog

All notable changes to this crate are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning follows the pre-1.0 reading
of [Semantic Versioning](https://semver.org/) described in [`docs/SEMVER.md`](docs/SEMVER.md).

This crate has not made a tagged release yet - everything below is still `0.1.0`, unreleased.
It is grouped by development milestone (see [`docs/PRODUCTION-ROADMAP.md`](docs/PRODUCTION-ROADMAP.md))
rather than by date, since milestones are the unit this project actually plans and completes in.
**Breaking** entries are the ones that change what an integrator's existing code must do to keep
compiling or behaving the same way (see [`docs/SEMVER.md`](docs/SEMVER.md) for exactly what that
means for a trait) - each was checked against its actual diff, not inferred from a commit
subject line. This is a reconstruction from `git log`, not every commit: entries below are the
ones that would break or surprise an integrator, or that materially describe what the crate can
now do; purely internal refactors, test additions, and documentation-only commits are omitted
unless they're the easiest way to explain a milestone's scope.

## [Unreleased] — 0.1.0

### Breaking

- **`ChargePoint::capabilities() -> Capabilities`** — the `ChargePoint` hardware trait gained a
  required method reporting a static [`Capabilities`](src/hardware/capabilities.rs) struct (display
  present, bidirectional power, RTC, persistent storage, ISO 15118 level, per-connector current
  ceiling, and one flag per optional functional block). Every existing `ChargePoint` impl needs
  this method added. (M1, capability model)
- **`Connector::set_current_limit`** — the `Connector` hardware trait gained a required method,
  dispatched through a new `HardwareCommand::SetCurrentLimit` at the same `(evse_id,
  connector_id)` granularity as other hardware commands, with failures routed to
  `FaultDetected` like the crate's other fail-safe commands. Landed with only its hardware surface
  wired at first — the smart-charging profile store/composition that calls it landed later in
  the same milestone. (M1, alongside `capabilities()` and `Storage`, deliberately batched into one
  break per M1's own plan)
- **`hardware::Storage`** — a new trait for durable, power-cut-surviving key/value storage, with
  `NoStorage` (default, no-op) and a `std`-gated `InMemoryStorage`. Threading it through
  `ChargePointBuilder`/`ChargePointActor::spawn` added a generic parameter integrators'
  construction code needs to account for even when using `NoStorage`. (M1)
- **`ChargePointEffect` dropped its `Eq` derive** (now `PartialEq` only, still `Debug`/`Clone`) —
  needed once a variant started carrying a `ChargingSchedule` payload, whose periods carry `f64`
  limits, and floats have no total order. Code that relied on `ChargePointEffect: Eq` (e.g. using
  it as a `HashSet`/`BTreeMap` key, or deriving `Eq` on a type that contains it) stops compiling.
  (B2.8, `NotifyChargingLimit`/`ClearedChargingLimit`/`NotifyEVChargingNeeds`/
  `NotifyEVChargingSchedule`)
- **`connect_and_setup` gained a `payload_limits: Option<PayloadLimits>` parameter** (defaulting
  like this function's other `Option` parameters) — part of F5.2's inbound-frame-size ceiling.
  Existing call sites need the new argument. (F5.2, memory-exhaustion hardening)
- **`ChargePointBuilder::firmware_updates` gained a `verifier: V` parameter** (and a `V:
  crate::hardware::FirmwareVerifier` bound), plus a `crate::firmware::SignedUpdateFirmwareHandler`
  bound on its existing CSMS type parameter — signed-firmware verification is now mandatory
  wiring for this registration method; pass `hardware::NoFirmwareVerifier` (fails closed) if the
  charge point never receives signed updates. (B3.3, signed firmware verification)

### Added — by milestone

- **M0 — Unblock**: per-functional-block builder registration (replacing one large multi-bound
  `setup()` signature with independent `ChargePointBuilder` methods), CI running clippy/fmt/a
  Cargo feature matrix, and the upstream (`ocpp-client`) gap list that shaped the rest of the
  roadmap.
- **M1 — Capability model**: one Cargo feature per optional OCPP functional block (`C1`); the
  runtime `Capabilities` struct and `CAPABILITY_GATES` single source of truth mapping a capability
  to its Cargo feature, 2.x `*Ctrlr` device-model component, and 1.6J feature-profile name (`C2`,
  `C3`); a data-driven test (`C3.5`) asserting all four advertisement surfaces agree; and
  protocol-correct refusal of unsupported messages, including CALLERROR for the responses that
  have no status field to carry a rejection in (`C5`). Also landed the three breaking hardware-trait
  changes above, deliberately batched into this one release.
- **M2 — Durability**: the `hardware::Storage`-backed persistence layer for in-flight
  transactions, the offline transaction-event/status/security-event queues, the authorization
  cache, local auth list, reservations, and device-model attributes; crash-consistency via an
  A/B-slot `AtomicStorage` adapter; a power-cut recovery test sweeping every point across a
  transaction session; bounded memory for every growable collection with measured ceilings
  (`docs/MEMORY.md`); and clock handling for a missing RTC, CSMS clock sync, and mid-transaction
  clock jumps.
- **M3 — Protocol completeness, core**: version negotiation across 1.6J/2.0.1/2.1; every
  Core-profile message on all three versions; smart charging (charging profiles, `GetCompositeSchedule`
  composition, hardware current limiting end to end); WebSocket ping-interval keepalive
  (`crate::keepalive`, once `ocpp-client` 0.3.0+ exposed the option); and reservation status
  updates.
- **M4 — Security and remote management**: security profiles 1–3 (profile 3 needs a multi-thread
  Tokio runtime — `rustls`'s synchronous `Signer` is bridged to the async `KeyStore::sign` via
  `block_in_place`); signed firmware update over the air, including 1.6J's Security Whitepaper
  `SignedUpdateFirmware`/`SignedFirmwareStatusNotification`; log upload; variable monitoring
  (`SetVariableMonitoring`/`NotifyEvent` and the rest of the 2.x monitoring engine); certificate
  install/delete/enumerate and CSR round-trip; OCSP status checking
  (`GetCertificateStatus`/`GetCertificateChainStatus`); and the inbound-frame-size ceiling
  (F5.2) that made `connect_and_setup`'s signature change above necessary.
- **Since M4 (pre-M5 polish)**: OCPP 2.1 payment (`NotifySettlement`/`NotifyWebPaymentStarted`/
  `VatNumberValidation`), DER control/V2X (`GetDERControl`/`SetDERControl`/`ClearDERControl`/
  `ReportDERControl`/`NotifyDERAlarm`/`NotifyDERStartStop`/`AFRRSignal`/
  `NotifyAllowedEnergyTransfer`), battery swap (`BatterySwap`/`RequestBatterySwap`), periodic
  event streams, `PublishFirmware`/`UnpublishFirmware`/`PublishFirmwareStatusNotification`, a
  secure-element/key-storage abstraction (`hardware::KeyStore`), the tariff store and
  per-transaction tariff assignment, and the display-message block.
- **`no_std` + `alloc` support**: `cargo check --no-default-features --lib` compiles under
  `#![no_std]`, backed by `embassy-sync` primitives (`src/sync.rs`) instead of `tokio::sync`;
  `tokio` is a fully optional dependency behind the `tokio-runtime` feature (in `default` for
  zero-config ergonomics on a normal host).

### Fixed

- Migrated `ocpp-client` 0.2.2 → 0.5.0 (`ocpp-types` 0.1.3 → 0.3.0) in one change — see
  [`docs/MIGRATION-ocpp-client-0.4.md`](docs/MIGRATION-ocpp-client-0.4.md) for the measured diff.
  Closed **A4** (WebSocket keepalive, `crate::keepalive` driving `OCPPCommCtrlr.WebSocketPingInterval`
  live via `ocpp-client` 0.3.0+'s `Client::ping_interval()`/`set_ping_interval()`) and fixed
  `ConnectionCloser` to force a redial rather than a sticky disconnect on a network-profile switch.
  `ocpp-client` 0.5.0 also started generating 1.6J's `SignedUpdateFirmware`/
  `SignedFirmwareStatusNotification` (previously absent upstream, tracked as a documented gap);
  this crate wired both in a later commit — see B3.3 above.

### Documentation

- [`docs/INTEGRATORS.md`](docs/INTEGRATORS.md): which hardware traits are mandatory vs. opt-in
  (with a `No*` default), recommended Cargo-feature sets per hardware class, the distinction
  between a Cargo feature (compiles code out) and a runtime `Capabilities` flag (declares what
  the hardware can do), and when to use `setup()` vs. `ChargePointBuilder` directly.
- [`docs/THREAT-MODEL.md`](docs/THREAT-MODEL.md): the security posture this crate defends.
- This changelog and [`docs/SEMVER.md`](docs/SEMVER.md) (H5.4).
- README per-version message-coverage numbers and a corrected capability-feature table (H5.3),
  regenerable via `scripts/message-coverage.py`.

---

[Unreleased]: https://github.com/flowionab/ocpp-charge-point/commits/main
