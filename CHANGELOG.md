# Changelog

All notable changes to this crate are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning follows the pre-1.0 reading
of [Semantic Versioning](https://semver.org/) described in [`docs/SEMVER.md`](docs/SEMVER.md).

`0.1.0` is this crate's first release, so everything below is the history that led to it rather
than a delta against a previous version. It is grouped by development milestone (see [`docs/PRODUCTION-ROADMAP.md`](docs/PRODUCTION-ROADMAP.md))
rather than by date, since milestones are the unit this project actually plans and completes in.
**Breaking** entries are the ones that change what an integrator's existing code must do to keep
compiling or behaving the same way (see [`docs/SEMVER.md`](docs/SEMVER.md) for exactly what that
means for a trait) - each was checked against its actual diff, not inferred from a commit
subject line. This is a reconstruction from `git log`, not every commit: entries below are the
ones that would break or surprise an integrator, or that materially describe what the crate can
now do; purely internal refactors, test additions, and documentation-only commits are omitted
unless they're the easiest way to explain a milestone's scope.

## [0.1.0] — 2026-08-10

First published release. The **Breaking** entries below are pre-release history — changes made
while the crate was unpublished, listed because anyone who tracked `main` lived through them.
Nothing here breaks an earlier *release*, because there was none.

### Packaging

- **`certificate-management` joined the `default` feature set.** It gates
  `ChargePointBuilder::certificates` (`InstallCertificate`/`DeleteCertificate`/
  `GetInstalledCertificateIds`), and was the one capability feature left out of `default` — which
  made that builder method invisible to anyone building with default features. A test
  (`every_capability_gate_feature_is_in_the_default_feature_set`) now fails if a gate drifts back
  out.
- **docs.rs builds with `--all-features`**, and editor-local `.idea/` no longer ships in the
  published tarball.

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
- **`ChargePoint::start` takes `self: Arc<Self>`** rather than `&self`. The command loop it spawns
  has to outlive the call, so with `&self` every implementation kept an `Arc` inside its own struct
  purely to have something to move into that task — both bundled examples did it. The runtime
  already holds an `Arc<T>` and now hands that same one to `start`, so bindings go back to plain
  owned fields; `ChargePointRuntime` stores `Arc<T>` to match. (H5.6, hardware trait
  implementability)
- **Five hardware accessors are plain `fn`, not `async fn`**: `ChargePoint::vendor_name`,
  `model_name`, `evses` and `capabilities`, plus `Evse::connectors`. None of them awaited or
  failed. `connectors()` is the one that mattered — `execute_hardware_command` calls it per
  command, so an `async fn` returning a fixed slice boxed a future per contactor operation. Impls
  need the `async` keyword removed. (H5.6)
- **`CertificateStore::has_private_key` is now `has_client_private_key`.** A real store holds
  several keys and only the client certificate's answers the question this method is asked. Rename
  the method in existing impls. (H5.6)
- **`hardware::Watchdog` lost its `Send + Sync` supertrait** — the only trait in `crate::hardware`
  to have had one. The bound now sits on the actor's `Arc<dyn Watchdog + Send + Sync>`, where the
  sharing actually happens. (H5.6)
- **`Authorizer` gained an `authorize_contract` method with a default implementation** — see
  Plug & Charge below. The default refuses locally, so an existing impl keeps compiling but
  declines every contract-certificate authorization until it implements the method.
  `run_authorization_requests` also gained a `Sync` bound, which `ChargePointBuilder` already
  required of every authorizer. (B4.6)

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
- **Plug & Charge authorization (OCPP use case C07, 2.0.1 and 2.1)**: `Authorize` now carries the
  ISO 15118 contract certificate (`certificate`/`iso15118CertificateHashData`) and reads the
  response's `certificateStatus`, where both were previously hardcoded `None` and dropped
  respectively. The presentation is a first-class connector event —
  `ConnectorEvent::ContractCertificatePresented { id_token, certificate }` reaches `Authorizing`
  by the same transition a card tap uses, sent on the `HardwareEventSender` an integrator's HLC
  stack already has, so no new hardware trait and no change to `Iso15118Controller`. Acceptance
  needs both of the CSMS's answers: `ContractAuthorization` keeps token status and certificate
  status apart because C07.FR.13/FR.14 make them genuinely independent. Offline, C07.FR.07
  overrides this crate's own offline fallback — a contract presentation with no CSMS is refused
  and does not consult the authorization cache or local list.
  `ISO15118Ctrlr.CentralContractValidationAllowed` is registered and read (C07.FR.06), `false` by
  default like every other capability-gated value, with a withheld chain logged rather than
  silently dropped. 1.6J downgrades instead of refusing: its `Authorize` has no certificate
  fields, so the eMAID goes as a plain `idTag` with a warning saying plainly that nothing
  validated the contract. (B4.6)
- **Capability propagation for ISO 15118 and display messages**: `iso15118_support` gained its
  `CAPABILITY_GATES` row (`ISO15118Ctrlr`, charge-point-initiated so `has_handler: false`, no 1.6J
  profile, and the component's required `ContractValidationOffline` variable) — B4.5 had wired
  `Get15118EVCertificate` but left the capability out of the table, so a station with a PLC modem
  advertised nothing and one without it left the component unknown rather than honestly
  unavailable. It is the only gate whose capability is an enum; both non-`None` levels count as
  support. `DisplayMessageCtrlr` now registers all five of its required variables rather than only
  `DisplayMessages`: `SupportedPriorities`/`SupportedStates` are held in step with
  `MessagePriority::ALL`/`MessageState::ALL` by a test, and `SupportedFormats` is seeded empty and
  overwritten by `ChargePointBuilder::display_messages` from `Display::supported_formats`, which
  is what that trait's docs already claimed happened. A CSMS can read the format and state limits
  instead of discovering them through a refusal. (C3)
- **Logging and personal-data redaction**: `IdToken` has a hand-written `Debug` that redacts the
  card number, so anything containing one is safe to log by construction rather than by each call
  site remembering. The new off-by-default **`unredacted-logs`** Cargo feature restores full values
  for local bring-up against a bench CSMS; never ship an image with it on. Handlers carry
  `#[instrument(skip_all)]`, log levels follow the rules now written down in `CLAUDE.md` (an 8 KiB
  `{:?}` of `ChargePointState` belongs at `trace!`, not `info!`), and `tracing_test_support` makes
  log level and content testable rather than conventional.
- **`no_std` + `alloc` support**: `cargo check --no-default-features --lib` compiles under
  `#![no_std]`, backed by `embassy-sync` primitives (`src/sync.rs`) instead of `tokio::sync`;
  `tokio` is a fully optional dependency behind the `tokio-runtime` feature (in `default` for
  zero-config ergonomics on a normal host).

### Fixed

- **`--features iso15118` with neither `ocpp_2_0_1` nor `ocpp_2_1` failed to compile.** The
  module's three shared helpers are used only by its version adapters, so with both adapters gated
  out they tripped `dead_code` under `-D warnings`. They now carry the same `cfg` the adapters do.
- **The flash figures in [`docs/MEMORY.md`](docs/MEMORY.md), the README and the roadmap were
  measured against a stale `ocpp-client`.** `tools/flash-probe` — a separate workspace, so it
  resolves its own direct dependency — still pinned 0.2.1 after the library moved to 0.5.0, which
  put both majors in one graph and stopped the probe building at all. Bumped to 0.5.0, brought the
  probe up to the current hardware traits, and re-measured every row: the version-independent core
  is 92 KB (not 32 KB) and all three protocol versions together are 558 KB, which no longer fits a
  512 KB part before a transport and TLS.
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

[0.1.0]: https://github.com/flowionab/ocpp-charge-point/releases/tag/v0.1.0
