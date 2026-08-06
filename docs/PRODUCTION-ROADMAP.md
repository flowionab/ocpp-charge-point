# Production readiness roadmap

What it takes to ship this crate as firmware a hardware manufacturer can
deploy on a real charge point and certify: **full OCPP 1.6J, 2.0.1 and 2.1
support, every message in the negotiated version handled, and Cargo/runtime
feature flags that let a build exclude capabilities the hardware doesn't
have.**

This document is scoped to *production readiness*. It is a companion to
[`ROADMAP.md`](./ROADMAP.md), which tracks OCPP 2.1 coverage per functional
block in depth; where a task here has a detailed design discussion there,
it's cross-referenced as "R§n". Where the two disagree on facts, this
document wins — every number and claim below was re-verified against the
working tree, `ocpp-client` 0.2.0 / `ocpp-types` 0.1.2 as locked in
`Cargo.lock`, and the vendored spec appendices, on 2026-08-05.

Status legend: ✅ done · 🚧 partial · ⬜ not started · 🔒 blocked upstream

---

## Contents

- [1. Definition of done](#1-definition-of-done)
- [2. Where we actually are](#2-where-we-actually-are)
- [3. Workstream A — transport, negotiation, connection lifecycle](#3-workstream-a--transport-negotiation-connection-lifecycle)
- [4. Workstream B — message coverage](#4-workstream-b--message-coverage)
- [5. Workstream C — capability and feature-flag model](#5-workstream-c--capability-and-feature-flag-model)
- [6. Workstream D — upstream dependency gaps](#6-workstream-d--upstream-dependency-gaps)
- [7. Workstream E — persistence and durability](#7-workstream-e--persistence-and-durability)
- [8. Workstream F — security](#8-workstream-f--security)
- [9. Workstream G — embedded robustness](#9-workstream-g--embedded-robustness)
- [10. Workstream H — test, compliance, release](#10-workstream-h--test-compliance-release)
- [11. Milestones](#11-milestones)
- [Appendix A — verified message inventory](#appendix-a--verified-message-inventory)

---

## 1. Definition of done

Production readiness is not "all messages implemented" — it's five
independent properties, and the message table is only the first.

| # | Property | Exit criterion |
|---|----------|----------------|
| **1** | **Protocol completeness** | For each of 1.6J / 2.0.1 / 2.1: every message the negotiated version defines is either handled, or refused with the protocol-correct rejection (`NotImplemented` CALLERROR for a message the build doesn't have; a `Rejected`/`NotSupported` *status* for a message it has but whose capability is absent at runtime). No message is silently dropped. |
| **2** | **Capability honesty** | What the charge point *advertises* (1.6J `SupportedFeatureProfiles`, 2.x device-model components/variables, `GetBaseReport`) matches exactly what it will actually do, for every combination of enabled Cargo features and runtime hardware capabilities. |
| **3** | **Durability** | A power cut mid-transaction loses no billable energy and no CSMS-visible state. Everything the spec requires to survive a reboot does. |
| **4** | **Security** | Security profiles 1–3, certificate lifecycle, and the full 2.x security event set — enough for OCPP's Advanced Security certification profile. |
| **5** | **Fitness for the target** | Runs on an MCU: `no_std` + `alloc`, bounded memory, panic-free, survives weeks of flaky connectivity and erratic hardware without operator intervention. |

Plus one process gate: **6 — certifiable.** Passes the OCTT for the
certification profiles we claim, on all three protocol versions.

---

## 2. Where we actually are

### 2.1 Message coverage, verified

Counted by matching every `.on_*(` / `.send_*(` call inside this crate's
per-version adapter modules against the action list `ocpp-client` 0.2.0
generates for that version:

| Version | Wired | Available in `ocpp-client` | Spec messages (approx.) |
|---------|-------|---------------------------|-------------------------|
| **1.6J** | **19** | 28 | 28 core + security-whitepaper extensions |
| **2.0.1** | **21** | 63 | 64 (`ocpp-types` has 64 request types) |
| **2.1** | **22** | 86 | 90+ (`ocpp-types` has 90 request types) |

Two things this table hides, both good news:

- The three version adapters are **not** three implementations. Every
  functional block defines one protocol-agnostic trait
  (`BootNotifier`, `StatusNotifier`, `TransactionNotifier`, …) with a
  per-version `impl` in an `ocpp_1_6` / `ocpp_2_0_1` / `ocpp_2_1`
  submodule. The internal state model really is version-independent, and
  the hard downgrades (1.6J's flat `connectorId` vs the internal
  `(evse_id, connector_id)` pair; 1.6J's `StartTransaction`/`StopTransaction`
  vs 2.x's unified `TransactionEvent`) are already solved and tested.
  Adding a version to an existing block is now a well-trodden path.
- Anything **not** registered is already answered correctly.
  `ocpp-client`'s dispatcher replies to an unknown action with a
  `NotImplemented` CALLERROR (`client.rs:495`). Property 1's "refuse
  correctly" half comes free for compile-time-excluded messages — see
  [C5](#55-c5--unsupported-response-discipline) for the runtime half,
  which does not.

### 2.2 Complete blocks

`SendLocalList` / `GetLocalListVersion` (R§4) and `DataTransfer` (R§16) are
the only functional blocks wired on **all three** versions today.
Provisioning, Availability, Authorization, Reset and Remote control are
close — mostly missing one version each.

### 2.3 Foundation state

| Area | Status | Note |
|------|--------|------|
| Actor model, version-independent state | ✅ | `ChargePointState` owns transactions, reservations, local auth list, cost, reset, device model — all mutated only via `ChargePointEvent`. |
| Hardware abstraction | 🚧 | `ChargePoint` / `Evse` / `Connector`: lock, unlock, contactor, reboot. **No capability model, no current-limit hook, no file transfer, no display, no RTC.** |
| `no_std` | ✅ | Compiles for a real bare-metal target (`thumbv7em-none-eabihf`), not just with features off — that took dropping `tracing`'s default features and a `getrandom` backend cfg ([H1.3](#101-h1--ci-hardening)). `embassy-sync` channels, `tokio` fully optional. |
| Offline queueing | 🚧 | `OfflineQueue` exists and is used by Availability / Transactions / Security. Unbounded and in-RAM — see [G2](#92-g2--bounded-memory). |
| Reconnect resync | ✅ | Fresh BootNotification on every reconnect, all three versions. |
| Persistence | ⬜ | **Nothing survives a restart.** `VariableAttribute::persistent` is recorded and ignored. |
| Test suite | 🚧 | 668 test functions in `src/`, one integration test (`tests/connect_2_1_websocket.rs`). Strong unit coverage, near-zero end-to-end. |
| CI | ✅ | Gating: clippy + fmt + rustdoc, feature matrix, `thumbv7em-none-eabihf`, MSRV 1.88, `cargo-deny`, on PRs too. Coverage reported but not gated ([H1.6](#101-h1--ci-hardening)). |

### 2.4 The structural blocker — resolved

*Was:* `setup()`'s CSMS type parameter carried **21 protocol trait bounds**
(`src/setup.rs:51`), one per handled message family, growing by one per
message added — so a build excluding Smart Charging still had to satisfy a
`SetChargingProfileHandler` bound, and reaching ~86 actions meant ~80 bounds
on one function.

*Now:* [C4](#54-c4--builder-refactor) landed. `ChargePointBuilder`
(`src/builder.rs`) registers one functional block per call, each carrying
only its own bounds; `setup()` survives unchanged as the "everything on"
wrapper. A CSMS client implementing a single block now compiles, which is
what [A2](#3-workstream-a--transport-negotiation-connection-lifecycle) (runtime
adapter-set selection), [C1](#51-c1--cargo-feature-per-functional-block)/[C2](#52-c2--runtime-capability-declaration)
(capability gating) and most of [Workstream B](#4-workstream-b--message-coverage)
were waiting on.

---

## 3. Workstream A — transport, negotiation, connection lifecycle

The connection is a production concern in its own right: a charge point
that can't reliably reconnect is worse than one missing a functional block.

| ID | Task | Status |
|----|------|--------|
| **A1** | `connect_and_setup` for 1.6J and 2.0.1. Today it's `ocpp_2_1`-gated only (`src/connect.rs`); 1.6J and 2.0.1 users must build a client by hand, which contradicts "integrators supply hardware bindings only". | ⬜ |
| **A2** | **Version negotiation.** Offer `ocpp2.1, ocpp2.0.1, ocpp1.6` as WebSocket subprotocols, take what the CSMS picks, and run the matching adapter set. Depends on [C4](#54-c4--builder-refactor) — negotiation means picking an adapter set at runtime, which the current 20-bound monomorphised `setup()` can't express. | ⬜ |
| **A3** | Configurable subprotocol preference order, so an operator can force a version. | ⬜ |
| **A4** | WebSocket keepalive driven by the device model: `OCPPCommCtrlr.WebSocketPingInterval` (1.6J alias already exists) must actually configure ping cadence. | ⬜ |
| **A5** | Reconnect backoff from the device model (`RetryBackOffWaitMinimum`, `RetryBackOffRepeatTimes`, `RetryBackOffRandomRange`) rather than the caller-supplied `Backoff` alone. | ⬜ |
| **A6** | Per-message timeouts and retry: `MessageTimeout`, `TransactionMessageAttempts`, `TransactionMessageRetryInterval` (aliases exist, values inert). | ⬜ |
| **A7** | `MessageAttemptInterval` / queue-depth limits for offline messages; drop policy when the queue is full, and the `MemoryExhaustion` security event when it happens. | ⬜ |
| **A8** | Test the `NotImplemented` CALLERROR path end-to-end from this crate's side, so property 1 is *asserted*, not just inherited. | ⬜ |
| **A9** | Network interface selection / `SetNetworkProfile` application (R§2) — the message handler is B-work; actually switching the active connection to a new profile, with rollback if the new profile fails to connect, is A-work. | ⬜ |

---

## 4. Workstream B — message coverage

The bulk of the remaining work. Organized by OCPP functional block; each row
is a message, the versions it applies to, and where it stands.

Legend per version cell: ✅ wired · ⬜ missing · 🔒 blocked on
[Workstream D](#6-workstream-d--upstream-dependency-gaps) · — not in that
version.

### B1 — Core spine (must be complete for *any* production deployment)

| Message | 1.6J | 2.0.1 | 2.1 | Notes |
|---------|:----:|:-----:|:---:|-------|
| BootNotification | ✅ | ✅ | ✅ | |
| Heartbeat | ✅ | ✅ | ✅ | |
| StatusNotification | ✅ | ✅ | ✅ | 1.6J needs `SuspendedEV`/`SuspendedEVSE` — the internal connector state machine can't distinguish them yet. |
| Authorize | ✅ | ✅ | ✅ | |
| StartTransaction / StopTransaction | ✅ | — | — | |
| TransactionEvent | — | ✅ | ✅ | |
| **MeterValues** (standalone) | ✅ | ⬜ | ⬜ | 2.x sends meter data inside `TransactionEvent` only; periodic/clock-aligned non-transaction sampling needs the standalone message. |
| DataTransfer | ✅ | ✅ | ✅ | |
| ChangeAvailability | ✅ | ✅ | ✅ | |
| Reset | ✅ | ✅ | ✅ | |
| UnlockConnector | ✅ | ✅ | ✅ | |
| RemoteStart/Stop · RequestStart/StopTransaction | ✅ | ✅ | ✅ | |
| **ClearCache** | ⬜ | ⬜ | ⬜ | Needs an authorization cache to clear — the cache itself doesn't exist yet. |
| **TriggerMessage** | ⬜ | ⬜ | 🔒 | Protocol-agnostic handler exists (R§6). 1.6J and 2.0.1 are buildable **today**; 2.1 needs [D1](#61-d1--missing-action-wrappers). |
| GetConfiguration / ChangeConfiguration | ✅ | — | — | 12 of ~46 standard keys aliased. |
| GetVariables / SetVariables | — | ✅ | ✅ | |
| GetBaseReport / GetReport / NotifyReport | — | ✅ | ✅ | |

**B1 tasks:**

- [ ] **B1.1** Standalone `MeterValues` for 2.0.1 and 2.1, driven by
      `AlignedDataCtrlr` / `SampledDataCtrlr` device-model variables.
- [ ] **B1.2** Authorization cache (2.x `AuthCacheCtrlr`, 1.6J
      `AuthorizationCacheEnabled`) + `ClearCache` on all three versions.
- [ ] **B1.3** `TriggerMessage` wire adapters for 1.6J and 2.0.1.
- [ ] **B1.4** `TriggerMessage` for 2.1, after [D1](#61-d1--missing-action-wrappers).
- [ ] **B1.5** Distinguish `SuspendedEV` / `SuspendedEVSE` in
      `ConnectorState`, and map them on all three versions.
- [ ] **B1.6** Complete the 1.6J standard configuration key table —
      12 of ~46 aliased today; all *required* keys must be readable and the
      writable ones must take effect.
- [ ] **B1.7** Register the 2.x required device-model variables. The
      vendored appendix lists **122 required component/variable rows across
      23 components**; the crate registers **2**. Scope per enabled feature
      (a build without Smart Charging owes no `SmartChargingCtrlr`
      variables) — see [C3](#53-c3--capability-propagation).
- [ ] **B1.8** `SetNetworkProfile` handler (R§2), paired with [A9](#3-workstream-a--transport-negotiation-connection-lifecycle).

### B2 — Smart charging (R§11)

Largest genuinely-missing block, and the one a real deployment demands
first: without it there is no load management.

| Message | 1.6J | 2.0.1 | 2.1 |
|---------|:----:|:-----:|:---:|
| SetChargingProfile | ⬜ | ⬜ | ⬜ |
| ClearChargingProfile | ⬜ | ⬜ | ⬜ |
| GetCompositeSchedule | ⬜ | ⬜ | ⬜ |
| GetChargingProfiles / ReportChargingProfiles | — | ⬜ | ⬜ |
| NotifyChargingLimit / ClearedChargingLimit | — | ⬜ | ⬜ |
| NotifyEVChargingNeeds / NotifyEVChargingSchedule | — | ⬜ | ⬜ |
| NotifyPriorityCharging / UsePriorityCharging | — | — | ⬜ |
| PullDynamicScheduleUpdate | — | — | ⬜ |
| UpdateDynamicSchedule | — | — | 🔒 |
| NotifyAllowedEnergyTransfer | — | — | ⬜ |

**B2 tasks:**

- [ ] **B2.1** Charging profile store in `ChargePointState` (stack levels,
      purposes, validity windows, recurrency).
- [ ] **B2.2** Schedule composition: profile stack → composite schedule at
      time `t`, with the 1.6J/2.x precedence rules.
- [x] **B2.3** **New hardware hook** — `Connector::set_current_limit` (or an
      `Evse`-level power limit). Nothing in `crate::hardware` can express
      "limit to N amps" today. Breaking change to the integrator surface;
      batch it with [C2](#52-c2--runtime-capability-declaration)'s
      `Capabilities` change and [E1](#71-e1--storage-trait)'s storage hook
      so integrators absorb *one* break.
- [ ] **B2.4** Composite schedule → hardware limit projection, re-evaluated
      on profile change, schedule period boundary, and transaction start.
- [ ] **B2.5** Per-version adapters for each message above.
- [ ] **B2.6** 2.1 dynamic schedule updates and priority charging.

### B3 — Firmware management (R§12)

| Message | 1.6J | 2.0.1 | 2.1 |
|---------|:----:|:-----:|:---:|
| UpdateFirmware | ⬜ | ⬜ | ⬜ |
| FirmwareStatusNotification | ⬜ | ⬜ | ⬜ |
| PublishFirmware / UnpublishFirmware / PublishFirmwareStatusNotification | — | ⬜ | ⬜ |

- [ ] **B3.1** File-transfer abstraction in `crate::hardware` (fetch a URL
      to storage, report progress). **Shared with [B5](#b5--diagnostics-and-monitoring-r14)** — do
      these two blocks together.
- [ ] **B3.2** Firmware state machine: Downloading → Downloaded →
      Installing → Installed / failure states, each mapped to
      `FirmwareStatusNotification` on all three versions.
- [ ] **B3.3** Signed firmware verification (2.x `signingCertificate` /
      `signature`; 1.6J security whitepaper `SignedUpdateFirmware`), driving
      the `InvalidFirmwareSignature` / `InvalidFirmwareSigningCertificate`
      security events.
- [ ] **B3.4** Local-controller firmware publishing (2.x only).

### B4 — Certificates and ISO 15118 (R§1, R§13)

| Message | 1.6J | 2.0.1 | 2.1 |
|---------|:----:|:-----:|:---:|
| InstallCertificate / DeleteCertificate / GetInstalledCertificateIds | — | ⬜ | ⬜ |
| CertificateSigned / SignCertificate | — | ⬜ | ⬜ |
| GetCertificateStatus | — | ⬜ | ⬜ |
| GetCertificateChainStatus | — | — | ⬜ |
| Get15118EVCertificate | — | ⬜ | ⬜ |

- [ ] **B4.1** Certificate store abstraction (depends on [E1](#71-e1--storage-trait); on
      real hardware this should be able to sit behind a secure element).
- [ ] **B4.2** Install / delete / enumerate, per certificate-use type.
- [ ] **B4.3** CSR generation and `SignCertificate` → `CertificateSigned`
      round trip, including automatic renewal before expiry.
- [ ] **B4.4** OCSP status checking.
- [ ] **B4.5** ISO 15118 Plug & Charge — gate behind a feature flag *and* a
      runtime capability; most chargers don't have it.

### B5 — Diagnostics and monitoring (R§14)

| Message | 1.6J | 2.0.1 | 2.1 |
|---------|:----:|:-----:|:---:|
| GetDiagnostics / DiagnosticsStatusNotification | ⬜ | — | — |
| GetLog / LogStatusNotification | — | ⬜ | ⬜ |
| SetVariableMonitoring / ClearVariableMonitoring | — | ⬜ | ⬜ |
| SetMonitoringBase / SetMonitoringLevel | — | ⬜ | ⬜ |
| GetMonitoringReport / NotifyMonitoringReport | — | ⬜ | ⬜ |
| NotifyEvent | — | ⬜ | ⬜ |
| CustomerInformation / NotifyCustomerInformation | — | ⬜ | ⬜ |
| GetTransactionStatus | — | ⬜ | ⬜ |
| Open/Close/Adjust/Get PeriodicEventStream, NotifyPeriodicEventStream | — | — | ⬜ |

- [ ] **B5.1** Log upload via the [B3.1](#b3--firmware-management-r12) file-transfer abstraction.
- [ ] **B5.2** Variable monitoring engine: thresholds, deltas, periodics on
      device-model variables → `NotifyEvent`.
- [ ] **B5.3** Monitoring report generation, chunked like `NotifyReport`
      already is.
- [ ] **B5.4** `GetTransactionStatus` — cheap, and needed for CSMS
      reconciliation after an offline period.
- [ ] **B5.5** Customer information / GDPR erasure.
- [ ] **B5.6** 2.1 periodic event streams.

### B6 — Display message (R§15)

`SetDisplayMessage`, `GetDisplayMessages`, `ClearDisplayMessage`,
`NotifyDisplayMessages` — 2.x only, all ⬜, and `SetDisplayMessage` for 2.1
is 🔒 on [D1](#61-d1--missing-action-wrappers).

- [ ] **B6.1** Display hardware hook + message store with priority/state.
- [ ] **B6.2** The four messages, gated on a display capability.

### B7 — Tariff, cost and payment (R§9)

| Message | 2.0.1 | 2.1 |
|---------|:-----:|:---:|
| CostUpdated | ✅ | ✅ |
| SetDefaultTariff / ChangeTransactionTariff / ClearTariffs / GetTariffs | — | ⬜ |
| NotifySettlement / NotifyWebPaymentStarted / VatNumberValidation | — | ⬜ |

- [ ] **B7.1** Tariff store and per-transaction tariff assignment (2.1).
- [ ] **B7.2** Payment terminal integration surface (2.1) — feature-flagged;
      `PaymentCtrlr` alone accounts for 22 of the 122 required device-model
      variables, so this is not a small block.

### B8 — Reservation, DER/V2X, battery swap

- [ ] **B8.1** `ReservationStatusUpdate` (2.x) — the only gap in an
      otherwise complete reservation block.
- [ ] **B8.2** DER control (2.1): `ClearDERControl`, `ReportDERControl`,
      `NotifyDERAlarm`, `NotifyDERStartStop`, `AFRRSignal`, plus
      `GetDERControl`/`SetDERControl` (🔒 [D1](#61-d1--missing-action-wrappers)). Feature-flagged; needs bidirectional
      power hardware.
- [ ] **B8.3** Battery swap (2.1): `BatterySwap`, `RequestBatterySwap`.
      Feature-flagged; niche hardware.

---

## 5. Workstream C — capability and feature-flag model

The explicit ask: *feature flags should exclude capabilities the
implementing hardware might not have.* This needs **two** layers, because
two different questions are being asked.

| Layer | Question | Mechanism | Known when |
|-------|----------|-----------|------------|
| **Compile-time** | "Will this *firmware image* ever do smart charging?" | Cargo features | Build time — code is not linked, flash is saved |
| **Runtime** | "Does *this unit* have a display fitted / can this connector unlock?" | `Capabilities` from the hardware binding | Boot time — code is present but the capability is declined |

Both matter. Compile-time alone can't model a product line where one SKU has
a display and another doesn't from the same image; runtime alone means an
MCU build carries DER-control code it will never execute.

### 5.1 C1 — Cargo feature per functional block

- [x] **C1.1** Add one feature per optional block, all in `default` so
      today's users see no change:
      `smart-charging`, `firmware-management`, `diagnostics`,
      `variable-monitoring`, `display-message`, `reservation`,
      `local-auth-list`, `tariff-cost`, `payment`, `iso15118`,
      `der-control`, `battery-swap`, `periodic-event-stream`,
      `certificates`.
- [x] **C1.2** Map each to the OCPP certification profiles it participates
      in, so a build can be described as "Core + Smart Charging" and
      certified as such. `docs/OCPP-2.1/…part5_certification_profiles.pdf`
      and the 2.0.1 equivalent are vendored — derive the mapping from them
      rather than from memory.
- [x] **C1.3** Keep the existing `ocpp_1_6` / `ocpp_2_0_1` / `ocpp_2_1`
      version features orthogonal to the capability features: any
      combination must compile.
- [x] **C1.4** Document the flag matrix in `README.md` with a
      recommended-set-per-hardware-class table.

### 5.2 C2 — Runtime capability declaration

- [x] **C2.1** `hardware::Capabilities` — a plain struct of `bool`s and
      small values (has display, supports bidirectional power, can unlock
      under load, has an RTC, has persistent storage, ISO 15118 support
      level, max current per connector, …).
- [x] **C2.2** `ChargePoint::capabilities()` returning it. Breaking change
      — **batch with [B2.3](#b2--smart-charging-r11) and [E1](#71-e1--storage-trait)**.
- [x] **C2.3** Sensible `Default` so an integrator adding one capability
      doesn't have to enumerate all of them, and so the trait can grow
      without breaking again.
- [x] **C2.4** Validate capabilities against enabled Cargo features at
      startup, and log loudly on contradiction (hardware claims a display,
      `display-message` is off).

### 5.3 C3 — Capability propagation

A capability that isn't advertised consistently is worse than one that's
absent — the CSMS will send messages the charger then fails. Every
advertisement surface must be derived from **one** source of truth:

- [x] **C3.1** Handler registration — an absent capability means the
      handler is never registered, so `ocpp-client` answers
      `NotImplemented` (already correct, see [2.1](#21-message-coverage-verified)).
- [x] **C3.2** 2.x device model — only register components/variables for
      present capabilities, so `GetBaseReport` describes the real machine.
- [x] **C3.3** 1.6J `SupportedFeatureProfiles` — compute from enabled
      features rather than hardcoding.
- [x] **C3.4** 2.x `*Ctrlr.Available` / `.Enabled` variables reflect the
      capability set.
- [x] **C3.5** A single test that asserts all four surfaces agree, run
      across the feature matrix from [H1](#101-h1--ci-hardening).

### 5.4 C4 — Builder refactor

**The unblocker for [A2](#3-workstream-a--transport-negotiation-connection-lifecycle), [C1](#51-c1--cargo-feature-per-functional-block), and most of [Workstream B](#4-workstream-b--message-coverage).**
`setup()`'s 21 protocol trait bounds (`src/setup.rs:51`) make it impossible
to omit a handler, and unworkable at ~80.

- [x] **C4.1** Replace the monolithic bound list with a builder that
      registers handler groups independently — one registration call per
      functional block, each with only that block's bounds. Done:
      `ChargePointBuilder` (`src/builder.rs`), with `start` (hardware +
      subscriptions) then `provisioning` / `status_notifications` /
      `transaction_events` / `authorization` / `security_events` /
      `remote_control` / `availability_control` / `reservation` / `reset` /
      `local_authorization_list` / `device_model` / `cost` / `build`. Each
      method consumes and returns `Self` and carries only its own block's
      bounds, so a client implementing one block compiles — proven by a test
      driving a CSMS type that implements *only* `BootNotifier +
      HeartbeatSender + ReconnectHandler` through to a working runtime, which
      could not satisfy `setup()`'s bound at all.

      Two design points worth recording. `start()` captures `vendor_name`/
      `model_name` as owned `String`s because the later methods aren't generic
      over `E`/`C` and so can't call the hardware traits themselves. And the
      four event subscriptions are taken once, up front (preserving the
      subscribe-before-hardware-start ordering `setup()` always had), which
      makes a *repeat* registration of one of those four blocks a documented
      no-op rather than a second forwarder — spawning a second one would
      silently duplicate every StatusNotification/TransactionEvent/
      SecurityEventNotification on the wire for the life of the process. A
      test asserts exactly one report per status change after registering
      `status_notifications` twice.
- [ ] **C4.2** Feature-gate each registration call, so an excluded block
      contributes no bounds and no code. *Unblocked, not done* — the split is
      what makes gating possible, but the per-block Cargo features themselves
      are [C1](#51-c1--cargo-feature-per-functional-block).
- [ ] **C4.3** Gate registration on runtime capability too, so the same
      image can register a handler on one unit and not another. *Unblocked,
      not done* — a caller can already skip any call conditionally; the
      capability model that would drive that decision is
      [C2](#52-c2--runtime-capability-declaration).
- [x] **C4.4** Keep `setup()` working as a thin "everything on" wrapper —
      no break for existing users. Done: `setup()` keeps its exact signature
      and 21-trait bound, and is now just the builder chain with every block
      registered. Its two original tests pass unchanged, and
      `connect_and_setup` is untouched.

### 5.5 C5 — Unsupported response discipline

Compile-time exclusion is handled. Runtime refusal is not, and the spec is
specific about it — a `NotImplemented` CALLERROR where a `Rejected` status
was required is a certification failure.

- [x] **C5.1** Decide and document, per message, whether a runtime-absent
      capability yields a rejection *status* in a normal response or a
      CALLERROR. (Rule of thumb: if the response schema has a status field
      that can say no, use it.) Done: full decision table below, also
      recorded as a doc comment on `src/refusal.rs`.
- [x] **C5.2** A shared helper so every handler refuses the same way. Done:
      `src/refusal.rs` — `REFUSAL_GATES`/`capability_present` (data-driven:
      one table row per capability-gated message) plus
      `ocpp_2_1_not_supported`/`ocpp_2_0_1_not_supported`/
      `ocpp_1_6_not_supported` for the CALLERROR cases. Wired into
      `ReserveNow`/`CancelReservation` (`src/reservation.rs`), `SendLocalList`
      (`src/local_authorization_list.rs`, new
      `SendLocalListOutcome::NotSupported`), `GetLocalListVersion`
      (`src/local_authorization_list.rs`, all three protocol modules), and
      `CostUpdated` (`src/cost.rs`, both 2.x modules) — the only messages
      whose registered handler can be runtime-absent today (the other
      capability rows in
      [`CAPABILITY_GATES`](#52-c2--runtime-capability-declaration) have
      `has_handler: false`).
- [x] **C5.3** Tests per message asserting the exact refusal shape. Done —
      see `src/refusal.rs`, `src/reservation.rs`, `src/local_authorization_list.rs`,
      `src/cost.rs` test modules; each CALLRESULT-status case asserts the
      specific outcome/status enum variant, each CALLERROR case asserts
      `RpcErrorCode::NotSupported` on the concrete per-version error type.

#### Decision table

Verified against the generated Rust response types in `ocpp-types` 0.1.3
(`~/.cargo/registry/.../ocpp-types-0.1.3/src/{v16,v201,v21}/*_response.rs`
and each version's `common.rs` status enums, since that's what actually
ships on the wire) and cross-checked against the vendored `docs/OCPP-2.1`/
`docs/OCPP-2.0.1` spec sets for the corresponding message definitions. Rule
of thumb: if the response schema has a status field that can say no, refuse
through it (`RefusalShape::CallResultStatus`); if the response schema has no
status field at all, no CALLRESULT can say no, so refusal must be a
CALLERROR (`RpcErrorCode::NotSupported`) instead
(`RefusalShape::CallError`). Rows marked "N/A today" are messages this
crate's `Capabilities` model doesn't gate at runtime yet (no
`CAPABILITY_GATES` row with `has_handler: true`) — the shape shown is what a
future capability addition should target, not something wired up now (see
C5.2's "not yet gated" list above and `CLAUDE.md`'s OUT-OF-SCOPE guidance
against implementing new functional blocks in this step).

| Message | 1.6J | 2.0.1 | 2.1 |
|---|---|---|---|
| `UnlockConnector` | CALLRESULT `UnlockConnectorResponseStatus::NotSupported` — N/A today | CALLRESULT `UnlockStatusEnum::UnlockFailed` (no `NotSupported` variant) — N/A today | CALLRESULT `UnlockStatusEnum::UnlockFailed` — N/A today |
| `RequestStartTransaction` | n/a (1.6J's `RemoteStartTransaction`/`RemoteStartTransactionResponseStatus` is `Accepted`/`Rejected` only) — CALLRESULT `Rejected`, N/A today | CALLRESULT `RequestStartStopStatusEnum::Rejected` — N/A today | CALLRESULT `RequestStartStopStatusEnum::Rejected` — N/A today |
| `RequestStopTransaction` | n/a (`RemoteStopTransaction`, same 2-value enum) — CALLRESULT `Rejected`, N/A today | CALLRESULT `RequestStartStopStatusEnum::Rejected` — N/A today | CALLRESULT `RequestStartStopStatusEnum::Rejected` — N/A today |
| `ChangeAvailability` | CALLRESULT `ChangeAvailabilityResponseStatus::Rejected` — N/A today | CALLRESULT `ChangeAvailabilityStatusEnum::Rejected` — N/A today | CALLRESULT `ChangeAvailabilityStatusEnum::Rejected` — N/A today |
| `ReserveNow` | CALLRESULT `ReserveNowResponseStatus::Rejected` | CALLRESULT `ReserveNowStatusEnum::Rejected` | CALLRESULT `ReserveNowStatusEnum::Rejected` — **wired** (`src/reservation.rs`) |
| `CancelReservation` | CALLRESULT `CancelReservationResponseStatus::Rejected` | CALLRESULT `CancelReservationStatusEnum::Rejected` | CALLRESULT `CancelReservationStatusEnum::Rejected` — **wired** |
| `SendLocalList` | CALLRESULT `SendLocalListResponseStatus::NotSupported` — **wired** | CALLRESULT `SendLocalListStatusEnum::Failed` (no `NotSupported` in 2.x) — **wired** | CALLRESULT `SendLocalListStatusEnum::Failed` — **wired** |
| `GetLocalListVersion` | CALLERROR `NotSupported` (`GetLocalListVersionResponse` is `{ listVersion }` — no status field in any version) — **wired** | CALLERROR `NotSupported` — **wired** | CALLERROR `NotSupported` — **wired** |
| `CostUpdated` | n/a (no `CostUpdated` message in 1.6J — `tariff_and_cost` has no 1.6 feature profile) | CALLERROR `NotSupported` (`CostUpdatedResponse` is `{}` — no status field) — **wired** | CALLERROR `NotSupported` — **wired** |
| `GetVariables`/`SetVariables` | n/a (device model is 2.x-only) | CALLRESULT `GetVariableStatusEnum::Rejected`/`SetVariableStatusEnum::Rejected` — N/A today | same as 2.0.1 |
| `GetBaseReport`/`GetReport` | n/a (2.x-only) | CALLRESULT `GenericDeviceModelStatusEnum::NotSupported` — N/A today | CALLRESULT `GenericDeviceModelStatusEnum::NotSupported` — N/A today |
| `Reset` | CALLRESULT `ResetResponseStatus::Rejected` — N/A today (`Reset` is core, always registered) | CALLRESULT `ResetStatusEnum::Rejected` — N/A today | CALLRESULT `ResetStatusEnum::Rejected` — N/A today |
| `DataTransfer` | CALLRESULT `DataTransferResponseStatus::UnknownVendorId`/`Rejected` — N/A today (vendor-id routing, not a `Capabilities` field) | CALLRESULT `DataTransferStatusEnum::UnknownVendorId`/`Rejected` — N/A today | same as 2.0.1 |
| `Authorize` | CALLRESULT (`AuthorizeResponse.idTagInfo.status`) — not capability-gated, always answers | CALLRESULT `AuthorizationStatusEnum` — not capability-gated | same as 2.0.1 |

Nothing here fell back on assumption where the vendored spec/generated types
didn't settle it — every response type above either has a documented status
enum or a documented-empty body in the generated `ocpp-types` source.

---

## 6. Workstream D — upstream dependency gaps

`ocpp-client` 0.2.0 is missing action wrappers for messages whose types
`ocpp-types` 0.1.2 **already defines** — so these are one macro line each
upstream, not new type work. This is a much smaller blocker than
`ROADMAP.md` §0 currently describes (it says 2.1's `TriggerMessage` types
"don't exist upstream at all"; `ocpp-types-0.1.2/src/v21/trigger_message_request.rs`
exists).

### 6.1 D1 — Missing action wrappers

| Version | Missing wrapper | Types present? |
|---------|-----------------|:--------------:|
| 2.0.1 | `SecurityEventNotification` | yes |
| 2.1 | `TriggerMessage` | yes |
| 2.1 | `SetDisplayMessage` | yes |
| 2.1 | `GetDERControl` | yes |
| 2.1 | `SetDERControl` | yes |
| 2.1 | `UpdateDynamicSchedule` | yes |

- [x] **D1.1** Upstream PR to `ocpp-client` adding the six macro entries.
      **All six claims above verified true** before implementing (types
      present in `ocpp-types` 0.1.2, wrapper absent in `ocpp-client` 0.2.0),
      and all six were genuinely one macro line. Implemented on branch
      `add-missing-action-wrappers` in `/Users/joatin/git/ocpp-client`
      (commit `2c93e83`), each with a `send_*`/`on_*`/`wait_for_*` trio and a
      fake-transport test mirroring that crate's existing pattern; its full
      suite, fmt, clippy and per-version no_std builds are green. **Not
      pushed and no PR opened** — awaiting the go-ahead.
- [x] **D1.2** Bump the dependency and unblock [B1.4](#b1--core-spine-must-be-complete-for-any-production-deployment), [B6](#b6--display-message-r15), [B8.2](#b8--reservation-derv2x-battery-swap), [B2](#b2--smart-charging-r11), and 2.0.1 security events.
      Done: `ocpp-client = "0.2.1"`, which also pulls `ocpp-types` 0.1.2 →
      0.1.3. Nothing in this crate needed changing to absorb either. The six
      wrappers are now *available* here — actually wiring them is Workstream
      B, and each one is now an ordinary `ChargePointBuilder` registration
      method rather than a bound added to `setup()`'s signature.

      The 0.1.3 bump also **partly retires `ROADMAP.md` §16's DataTransfer
      blocker, and the old explanation for it was wrong** — corrected there
      and in `src/data_transfer.rs`. `data` is no longer a bare `Option<()>`
      that codegen couldn't represent: 0.1.3 makes the type generic in its
      payload (`DataTransferRequest<DataTransferRequestData = ()>`). The
      payload still can't cross the wire, but only because `ocpp-client`'s
      action macros name the type bare and monomorphise to that `()`
      default — a small, concrete upstream change now, not a modelling gap.
- [x] **D1.3** Correct `ROADMAP.md` §0's `TriggerMessage` claim. Done — and
      the claim was wronger than this line implies: it blamed `rust-ocpp`,
      which isn't in this crate's dependency graph at all. Corrected in
      `ROADMAP.md` §6, along with §0's stale "only 2.0.1 spec PDFs are
      vendored" note.

### 6.2 D2 — Type completeness audit

- [x] **D2.1** Diff `ocpp-types` v21's 90 request types against the 2.1
      specification's message list; same for v201's 64 and v16's 28.
      Anything genuinely absent upstream is a real blocker and needs to be
      known *now*, not when a certification run hits it. **Done — see
      [`UPSTREAM-GAPS.md`](./UPSTREAM-GAPS.md).** The 90/64/28 counts and
      Appendix A's 19/28, 21/63, 22/86 wired counts all re-derived and
      confirmed. For 2.1 and 2.0.1 the `ocpp-types` message list matches the
      vendored spec text 1:1 — **no genuinely-absent types**, so every 2.x
      gap is a wiring gap, not a blocker. 1.6J has one real blocker: see
      D2.2. (1.6J's spec is not vendored under `docs/`, so its 28 was
      cross-checked against `rust-ocpp` instead — a weaker source, flagged as
      such in the audit.)
- [ ] **D2.2** 1.6J security whitepaper extensions (`SecurityEventNotification`,
      `SignedUpdateFirmware`, `SignedFirmwareStatusNotification`,
      `LogStatusNotification`, `GetLog`, `InstallCertificate`,
      `DeleteCertificate`, `GetInstalledCertificateIds`, `CertificateSigned`,
      `SignCertificate`) are absent from `ocpp-client`'s 1.6 action list
      entirely. Decide: contribute them upstream, or declare 1.6J security
      profiles out of scope — and say so in the README either way.
      **Audited, decision still open** — and the gap is bigger than this
      line assumed: all 10 are missing from `ocpp-types`' v16 module
      *entirely*, not merely unwrapped in `ocpp-client`, so this is type
      work upstream, not another round of D1's macro lines. Absent from
      `rust-ocpp` too, so switching type crates wouldn't help.
      [`UPSTREAM-GAPS.md`](./UPSTREAM-GAPS.md) lays out the cost either way;
      **the user decides.**

### 6.3 D3 — Dependency policy

- [ ] **D3.1** Pin `ocpp-client` to a version range this crate has actually
      tested against; today `"0.2"` accepts any 0.2.x.
- [ ] **D3.2** Vendor-or-fork contingency if upstream PRs stall.
- [x] **D3.3** `cargo-deny` for licences and advisories, in CI. Done
      alongside [H1.5](#101-h1--ci-hardening) — `deny.toml` plus a `deny` job,
      verified locally (`advisories ok, bans ok, licenses ok, sources ok`).

---

## 7. Workstream E — persistence and durability

Nothing survives a restart today. For a device that gets power-cycled by the
grid, by an operator, or by its own `Reset` handler, this is the single
biggest gap between the current crate and a shippable product — a power cut
mid-transaction currently loses the transaction.

### 7.1 E1 — Storage trait

- [x] **E1.1** `hardware::Storage`: `no_std`-friendly, async, key-value,
      explicitly allowed to fail. Failure must degrade (run without
      persistence, raise a security/diagnostic event) rather than panic —
      per `CLAUDE.md`'s error-handling stance.
- [x] **E1.2** Optional: a charge point without storage must still run, with
      the durability guarantees clearly documented as absent.
- [x] **E1.3** `std` reference implementation for tests and desktop
      integrators.

### 7.2 E2 — What must survive

| State | Why | Owner |
|-------|-----|-------|
| In-flight transaction (id, meter start, id token, start time) | Billable energy; resume-or-close on boot | R§5 |
| Transaction sequence numbers / `seqNo` | 2.x `TransactionEvent` ordering | R§5 |
| Device model attributes marked `persistent` | Already flagged in the model, currently ignored | R§2 |
| Local authorization list + version number | Re-download after every boot is unacceptable offline | R§4 |
| Authorization cache | Offline authorization | [B1.2](#b1--core-spine-must-be-complete-for-any-production-deployment) |
| Reservations | Survive a reboot inside the reservation window | R§8 |
| Charging profiles | Load limits must not vanish on reboot | [B2.1](#b2--smart-charging-r11) |
| Offline message queue | Currently RAM-only — a reboot while offline loses every queued report | [G2](#92-g2--bounded-memory) |
| Certificates and keys | Security profile 2/3 | [B4.1](#b4--certificates-and-iso-15118-r1-r13) |
| Security event log | `SecurityLogWasCleared` is only meaningful against a durable log | [F4](#84-f4--security-events) |
| Network profiles | Recover connectivity after a bad profile switch | [A9](#3-workstream-a--transport-negotiation-connection-lifecycle) |
| Boot reason | `BootNotification.reason` must distinguish power-up from a commanded reset | R§2 |

- [ ] **E2.1–E2.12** One task per row above.

### 7.3 E3 — Crash consistency

- [ ] **E3.1** Write ordering / journaling so a power cut mid-write can't
      produce a half-written transaction record.
- [ ] **E3.2** Bound write frequency — flash has finite erase cycles, and a
      meter sample every few seconds will wear it out. Only checkpoint what
      must be recovered, at the cadence it must be recovered at.
- [ ] **E3.3** Schema versioning, so a firmware update can read the previous
      version's state.

### 7.4 E4 — Recovery

- [ ] **E4.1** On boot: reload state, then decide per transaction whether to
      resume or close it out, and report accordingly.
- [ ] **E4.2** Send the correct `BootNotification.reason`
      (`PowerUp`/`RemoteReset`/`ScheduledReset`/…) from persisted context.
- [ ] **E4.3** Replay the offline queue after reboot, preserving order.
- [ ] **E4.4** Power-cut test harness — kill the process at N points across
      a transaction lifecycle and assert recovery at each.

---

## 8. Workstream F — security

Target: OCPP's Advanced Security certification profile on 2.x, and the 1.6J
security whitepaper to whatever extent [D2.2](#62-d2--type-completeness-audit) concludes is feasible.

### 8.1 F1 — Security profiles

- [ ] **F1.1** Profile 1 — HTTP Basic auth over an unsecured connection.
- [ ] **F1.2** Profile 2 — Basic auth over TLS, with CSMS certificate
      validation.
- [ ] **F1.3** Profile 3 — mutual TLS with a charge point certificate.
- [ ] **F1.4** Profile selection and switching via `SetNetworkProfile`, with
      the spec's fallback-to-previous-profile behaviour on failure.

### 8.2 F2 — TLS

- [ ] **F2.1** TLS in the transport path. `ocpp-client`'s `websocket`
      feature already uses `rustls` with webpki roots for the std case;
      embedded needs an `embedded-tls`-shaped alternative — confirm what
      `ocpp-client` actually exposes for no_std before designing around it.
- [ ] **F2.2** Trust store management fed by [B4](#b4--certificates-and-iso-15118-r1-r13)'s installed certificates.
- [ ] **F2.3** Cipher suite and TLS version policy, raising
      `InvalidTLSVersion` / `InvalidTLSCipherSuite` on violation (the wire
      strings are already correct in `src/security.rs:47`).
- [ ] **F2.4** Secure element / key storage abstraction — private keys must
      not be required to sit in flash.

### 8.3 F3 — Credentials

- [ ] **F3.1** Basic-auth password storage and rotation.
- [ ] **F3.2** Certificate renewal ahead of expiry ([B4.3](#b4--certificates-and-iso-15118-r1-r13)).
- [ ] **F3.3** `ReconfigurationOfSecurityParameters` event on every change.

### 8.4 F4 — Security events

18 of the 21 event types in the vendored appendix are modelled in
`SecurityEventType`.

- [ ] **F4.1** Add the missing three: `DiscardedRenewedClientCertificate`,
      `MaintenanceLoginAccepted`, `MaintenanceLoginFailed`.
- [ ] **F4.2** Actually *raise* each event from the code path that detects
      it — most are declared but never emitted.
- [ ] **F4.3** Durable, size-bounded security log ([E2](#72-e2--what-must-survive)), readable via
      `GetLog`.
- [ ] **F4.4** `SecurityEventNotification` for 2.0.1 (after [D1](#61-d1--missing-action-wrappers)) and a
      decision on 1.6J.

### 8.5 F5 — Hardening

- [ ] **F5.1** Threat model document — a certification auditor will ask.
- [ ] **F5.2** Reject oversized/malformed payloads before allocation;
      `MemoryExhaustion` event when limits are hit.
- [ ] **F5.3** Replay protection where the spec requires it
      (`AttemptedReplayAttacks`).
- [ ] **F5.4** Secure-boot integration points for integrators that have it.

---

## 9. Workstream G — embedded robustness

### 9.1 G1 — no_std across the matrix

- [ ] **G1.1** CI job building `--no-default-features` for a real MCU
      target (`thumbv7em-none-eabihf`), not just `cargo check` on the host.
- [ ] **G1.2** Every new feature combination stays no_std-clean.
- [ ] **G1.3** A minimal embedded example, so the claim is demonstrated
      rather than asserted.

### 9.2 G2 — Bounded memory

- [ ] **G2.1** `OfflineQueue` uses an unbounded `VecDeque` — a long outage
      grows it until allocation fails. Bound it, with an explicit
      drop-or-reject policy and a `MemoryExhaustion` security event on
      overflow.
- [ ] **G2.2** Audit every other unbounded collection in
      `ChargePointState` (local auth list, transaction history, device
      model, charging profiles) for a configured maximum.
- [ ] **G2.3** Document peak RAM per configuration so integrators can size
      the part.
- [ ] **G2.4** Measure flash cost per Cargo feature — that's the whole
      point of [C1](#51-c1--cargo-feature-per-functional-block), and it should be a number in the README.

### 9.3 G3 — Time

- [ ] **G3.1** Behaviour with no RTC and no CSMS time yet — transactions
      must still be recordable.
- [ ] **G3.2** Clock sync from `BootNotification`/`Heartbeat` responses,
      raising `SettingSystemTime`.
- [ ] **G3.3** Correct handling of a clock jump mid-transaction (monotonic
      durations, not wall-clock subtraction).

### 9.4 G4 — Failure containment

- [ ] **G4.1** Audit for `unwrap`/`expect`/`panic!` on any path reachable
      from hardware or network input.
- [ ] **G4.2** `#![deny(clippy::unwrap_used, clippy::panic)]` in library
      code, with test-only exemptions.
- [ ] **G4.3** Watchdog hook — the actor should be able to prove liveness to
      hardware.
- [ ] **G4.4** Fault-injection tests: every `hardware` trait method failing,
      timing out, and returning inconsistent state, asserting the state
      machine reaches `Faulted`/`FaultedSafe` fail-safely
      (contactor open *before* unlock) rather than wedging.
- [ ] **G4.5** Actor mailbox backpressure policy — what happens when
      hardware pushes events faster than they're drained.

---

## 10. Workstream H — test, compliance, release

### 10.1 H1 — CI hardening

~~Current CI is `cargo build` + `cargo test` on one target.~~ Rewritten —
`.github/workflows/ci.yaml` now runs six gating jobs plus coverage.

- [x] **H1.1** `cargo clippy -- -D warnings`, `cargo fmt --check`. Both gating,
      for `--all-features --all-targets` *and* `--no-default-features --lib`
      (the no_std paths `--all-features` never compiles). `cargo doc` too,
      so `lib.rs`'s `missing_docs` warning becomes a CI error without
      failing local builds. Getting there needed a real cleanup: 13 clippy
      warnings fixed properly (5 `while let` loops, 4 collapsed `if`s, 2
      extracted type aliases, an `EffectSenders` struct for the actor's
      `run`), with exactly one documented `#[allow]` — on a `Result` shape
      `tungstenite`'s `Callback` trait dictates — plus a whole-repo `cargo
      fmt` pass, kept in its own commit so the churn hides nothing.
- [x] **H1.2** Feature matrix — each version feature alone, each capability
      feature off, `--no-default-features`, and `--all-features`.
      `cargo hack check --each-feature` (not a full powerset: the version
      features are the only genuinely independent axis, and each must compile
      *alone* without secretly depending on another version's module), plus
      the three named runtime configurations — true no_std, std-without-tokio,
      and everything.
- [x] **H1.3** Embedded target build ([G1.1](#91-g1--no_std-across-the-matrix)).
      `thumbv7em-none-eabihf`, and **this had never actually compiled** —
      the no_std claim had only ever been checked on a host target. Two real
      fixes were needed: `tracing` now builds with `default-features = false`
      (its `std` feature pulls `once_cell`, which doesn't compile bare-metal
      at all), and `getrandom` — reached via `ocpp-client` → `uuid`'s `v4` —
      needs `--cfg getrandom_backend="custom"`, the same "the final binary
      supplies this" contract `critical-section` already has. Set in the job,
      not papered over.
- [x] **H1.4** MSRV declared and enforced. `rust-version = "1.88"`, verified
      rather than guessed: 1.87 fails on this crate's own let-chains, and
      dependencies independently require up to 1.87.
- [x] **H1.5** `cargo-deny` ([D3.3](#63-d3--dependency-policy)). `deny.toml`
      added and run locally before committing — `advisories ok, bans ok,
      licenses ok, sources ok`. Permissive-only allow-list, every entry taken
      from a licence actually in the tree; `ignore = []` with a note that
      exceptions get a reason, never silence. This closes D3.3 as well.
- [ ] **H1.6** Coverage reporting, with a floor on the protocol adapters.
      *Partial* — `cargo llvm-cov` runs and reports, but nothing is gated. A
      floor set before a baseline exists is either trivially passable or
      arbitrary; the first green `main` run supplies the number, and this
      stays open until `--fail-under-lines` is in.
- [x] **H1.7** Run on PRs, not just `push`.

### 10.2 H2 — Integration testing

668 unit tests, one integration test. Unit coverage is genuinely good; what's
missing is proof that the pieces work *together* over a real socket.

- [ ] **H2.1** Mock CSMS harness — scripted request/response over a real
      WebSocket, for all three versions. Extend
      `tests/connect_2_1_websocket.rs`.
- [ ] **H2.2** Full-lifecycle scenario tests per version: boot → status →
      plug → authorize → start → meter → stop → unlock.
- [ ] **H2.3** Offline scenarios: disconnect mid-transaction, queue,
      reconnect, verify ordering and no duplication.
- [ ] **H2.4** Version-projection tests — same internal event sequence,
      three protocol versions, assert each wire shape.
- [ ] **H2.5** Power-cut recovery ([E4.4](#74-e4--recovery)).
- [ ] **H2.6** A simulated-hardware charge point in `examples/`, usable as
      an integrator's starting point and as a soak-test subject.

### 10.3 H3 — Compliance

- [ ] **H3.1** OCTT (OCA Compliance Test Tool) runs for 1.6J, 2.0.1, 2.1.
- [ ] **H3.2** Work through `…part6-testcases.pdf` for 2.0.1 and 2.1 — both
      vendored — as a checklist, and track pass/fail per case.
- [ ] **H3.3** Decide which certification profiles to claim per feature set
      ([C1.2](#51-c1--cargo-feature-per-functional-block)) and pass them.
- [ ] **H3.4** Interoperability against at least two independent CSMS
      implementations per version.
- [ ] **H3.5** Re-verify everything in `ROADMAP.md` marked
      "(verify vs 2.1 spec)" against the now-vendored 2.1 specification —
      that caveat predates the PDFs being added.

### 10.4 H4 — Longevity

- [ ] **H4.1** Multi-day soak with induced network flapping.
- [ ] **H4.2** Memory-growth assertion over thousands of transactions.
- [ ] **H4.3** Sustained-throughput test on a multi-EVSE configuration.

### 10.5 H5 — Release

- [ ] **H5.1** Complete rustdoc on every public item — `#![warn(missing_docs)]`
      is on; make it `deny`.
- [ ] **H5.2** Integrator's guide: implement these traits, pick these
      features, here's a working example.
- [ ] **H5.3** Per-version, per-profile support matrix in the README, kept
      honest by [C3.5](#53-c3--capability-propagation)'s test.
- [ ] **H5.4** Semver and MSRV policy; changelog.
- [ ] **H5.5** 1.0 criteria: hardware trait surface frozen. Land every
      planned breaking change ([B2.3](#b2--smart-charging-r11), [C2.2](#52-c2--runtime-capability-declaration), [E1.1](#71-e1--storage-trait)) before this.

---

## 11. Milestones

Ordered by dependency, not by size. Each milestone's exit criterion is
testable.

### M0 — Unblock (small, do first) — ✅ complete (2026-08-06)

[C4](#54-c4--builder-refactor) builder refactor · [D1](#61-d1--missing-action-wrappers) upstream wrappers · [H1](#101-h1--ci-hardening) CI hardening ·
[D2.1](#62-d2--type-completeness-audit) type audit

> **Exit:** handlers register independently with per-block bounds; CI runs
> clippy, fmt and a feature matrix; the full upstream gap list is known.

All three exit conditions met. Two carry-overs, neither blocking M1:
[D1.2](#61-d1--missing-action-wrappers) (bump the dependency) waits on the
`ocpp-client` branch being released, and [H1.6](#101-h1--ci-hardening)'s
coverage floor waits on a baseline. One decision is now the user's:
[D2.2](#62-d2--type-completeness-audit) — 1.6J security whitepaper extensions
are missing upstream as *types*, so contributing them is real work, not a
macro line.

Everything else is cheaper after this. [C4](#54-c4--builder-refactor) in particular converts "add a
message" from "add a bound to a 20-bound signature that every caller must
satisfy" into a local change.

### M1 — Capability model — ✅ complete (2026-08-06)

[C1](#51-c1--cargo-feature-per-functional-block) Cargo features · [C2](#52-c2--runtime-capability-declaration) runtime capabilities · [C3](#53-c3--capability-propagation) propagation ·
[C5](#55-c5--unsupported-response-discipline) refusal discipline

> **Exit:** a build can exclude any optional block; every advertisement
> surface agrees with the real capability set; every unsupported message is
> refused in the protocol-correct shape, with a test proving it.

Do the breaking hardware-trait changes here, together, in one release:
`capabilities()`, `set_current_limit`, `Storage`. Integrators absorb one
break rather than three.

All three breaks were taken together as planned: `ChargePoint::capabilities()`
([C2.2](#52-c2--runtime-capability-declaration)), `Connector::set_current_limit`
([B2.3](#b2--smart-charging-r11)) and `hardware::Storage` ([E1.1](#71-e1--storage-trait)) landed in one
change. Only the *hardware surface* of the latter two is done — B2.1/B2.2/B2.4
(profile store, schedule composition, limit projection) and E2–E4 (wiring
persistence into `ChargePointState`/the offline queue) remain untouched in M3
and M2 respectively. `set_current_limit` therefore has a dispatch path and a
fail-safe error path, but nothing yet *calls* it.

The single source of truth is `CAPABILITY_GATES` in
`src/hardware/capabilities.rs`: capability field ↔ Cargo feature ↔ 2.1
`*Ctrlr` component ↔ 1.6J feature-profile name ↔ `has_handler`. All four
advertisement surfaces derive from it, and
`setup.rs::tests::all_four_capability_propagation_surfaces_agree_with_the_capability_set`
([C3.5](#53-c3--capability-propagation)) is data-driven over the table, so a new capability cannot be
added to one surface and forgotten in the others.

Four honest caveats on the exit criteria, none blocking M2:

- **11 of the 14 capability features gate nothing yet.** Only `reservation`,
  `local-auth-list` and `tariff-cost` have code to compile out; the rest are
  declared against blocks that do not exist. "A build can exclude any optional
  block" is true only of blocks that exist.
- **[C3.3](#53-c3--capability-propagation)'s 1.6J `SupportedFeatureProfiles` is unverified against a
  vendored spec** — only the 2.0.1 and 2.1 specs are under `docs/`, so the
  profile-name list comes from general OCPP 1.6J knowledge. Verify it before
  claiming 1.6J certification.
- **Most of [C5](#55-c5--unsupported-response-discipline)'s decision table is documentation, not live code.** Only the
  three capabilities with real handlers can be registered-but-runtime-absent
  today; the other rows record the shape a future capability must refuse in.
  They are marked N/A in the table rather than implied to be wired.
- **`examples/` and `tests/` assume default features** and do not compile under
  a capability subset. The capability contract is `--lib`-only, and CI checks
  it that way.

### M2 — Durability

[E1](#71-e1--storage-trait)–[E4](#74-e4--recovery) · [G2](#92-g2--bounded-memory) bounded memory · [G3](#93-g3--time) time handling

> **Exit:** power-cut at any point in a transaction loses no billable
> energy; the offline queue survives reboot; memory is bounded under a
> week-long outage.

This is the gap between "a demo" and "a product". It's ahead of most
message coverage on purpose — a charger that handles 86 messages and loses
transactions on power loss is not shippable; one that handles 25 and never
loses a transaction is.

### M3 — Protocol completeness, core

[B1](#b1--core-spine-must-be-complete-for-any-production-deployment) core spine · [B2](#b2--smart-charging-r11) smart charging · [A1](#3-workstream-a--transport-negotiation-connection-lifecycle)–[A9](#3-workstream-a--transport-negotiation-connection-lifecycle) transport ·
[B8.1](#b8--reservation-derv2x-battery-swap) reservation status

> **Exit:** version negotiation works; every Core-profile message is handled
> on all three versions; load management works end to end.

Smart charging is the block real deployments demand first.

### M4 — Security and remote management

[F1](#81-f1--security-profiles)–[F5](#85-f5--hardening) · [B3](#b3--firmware-management-r12) firmware · [B4](#b4--certificates-and-iso-15118-r1-r13) certificates · [B5](#b5--diagnostics-and-monitoring-r14) diagnostics

> **Exit:** security profiles 1–3; signed firmware update over the air;
> log upload; variable monitoring. A field unit can be updated and
> diagnosed remotely — without this, every fault is a truck roll.

### M5 — Full coverage and certification

[B6](#b6--display-message-r15) display · [B7](#b7--tariff-cost-and-payment-r9) tariff/payment · [B8.2](#b8--reservation-derv2x-battery-swap)–[B8.3](#b8--reservation-derv2x-battery-swap) DER, battery swap ·
[B4.5](#b4--certificates-and-iso-15118-r1-r13) ISO 15118 · [B5.6](#b5--diagnostics-and-monitoring-r14) periodic event streams · [H2](#102-h2--integration-testing)–[H5](#105-h5--release)

> **Exit:** every message in all three versions handled or correctly
> refused; OCTT green for the claimed profiles; 1.0 with a frozen hardware
> trait surface.

Everything in M5 is capability-gated and hardware-dependent — a given
product ships the subset its hardware supports. M5 completes the *library*,
not every deployment.

---

## Appendix A — verified message inventory

Method: every `.on_*(` / `.send_*(` call inside a `mod ocpp_1_6` /
`ocpp_2_0_1` / `ocpp_2_1` block in `src/`, matched against the action names
`ocpp-client` 0.2.0 generates per version. Re-run it after any coverage
work; it's the honest number.

### A.1 OCPP 1.6J — 19 of 28 wired

**Wired:** Authorize, BootNotification, CancelReservation,
ChangeAvailability, ChangeConfiguration, DataTransfer, GetConfiguration,
GetLocalListVersion, Heartbeat, MeterValues, RemoteStartTransaction,
RemoteStopTransaction, ReserveNow, Reset, SendLocalList, StartTransaction,
StatusNotification, StopTransaction, UnlockConnector

**Missing:** ClearCache, ClearChargingProfile, DiagnosticsStatusNotification,
FirmwareStatusNotification, GetCompositeSchedule, GetDiagnostics,
SetChargingProfile, TriggerMessage, UpdateFirmware

### A.2 OCPP 2.0.1 — 21 of 63 wired

**Wired:** Authorize, BootNotification, CancelReservation,
ChangeAvailability, CostUpdated, DataTransfer, GetBaseReport,
GetLocalListVersion, GetReport, GetVariables, Heartbeat, NotifyReport,
RequestStartTransaction, RequestStopTransaction, ReserveNow, Reset,
SendLocalList, SetVariables, StatusNotification, TransactionEvent,
UnlockConnector

**Missing:** CertificateSigned, ClearCache, ClearChargingProfile,
ClearDisplayMessage, ClearVariableMonitoring, ClearedChargingLimit,
CustomerInformation, DeleteCertificate, FirmwareStatusNotification,
Get15118EVCertificate, GetCertificateStatus, GetChargingProfiles,
GetCompositeSchedule, GetDisplayMessages, GetInstalledCertificateIds,
GetLog, GetMonitoringReport, GetTransactionStatus, InstallCertificate,
LogStatusNotification, MeterValues, NotifyChargingLimit,
NotifyCustomerInformation, NotifyDisplayMessages, NotifyEVChargingNeeds,
NotifyEVChargingSchedule, NotifyEvent, NotifyMonitoringReport,
PublishFirmware, PublishFirmwareStatusNotification, ReportChargingProfiles,
ReservationStatusUpdate, SetChargingProfile, SetDisplayMessage,
SetMonitoringBase, SetMonitoringLevel, SetNetworkProfile,
SetVariableMonitoring, SignCertificate, TriggerMessage, UnpublishFirmware,
UpdateFirmware

**Also:** `SecurityEventNotification` is in the 2.0.1 spec and in
`ocpp-types` v201, but `ocpp-client` 0.2.0 generates no action for it — see
[D1](#61-d1--missing-action-wrappers).

### A.3 OCPP 2.1 — 22 of 86 wired

**Wired:** Authorize, BootNotification, CancelReservation,
ChangeAvailability, CostUpdated, DataTransfer, GetBaseReport,
GetLocalListVersion, GetReport, GetVariables, Heartbeat, NotifyReport,
RequestStartTransaction, RequestStopTransaction, ReserveNow, Reset,
SecurityEventNotification, SendLocalList, SetVariables, StatusNotification,
TransactionEvent, UnlockConnector

**Missing:** AFRRSignal, AdjustPeriodicEventStream, BatterySwap,
CertificateSigned, ChangeTransactionTariff, ClearCache,
ClearChargingProfile, ClearDERControl, ClearDisplayMessage, ClearTariffs,
ClearVariableMonitoring, ClearedChargingLimit, ClosePeriodicEventStream,
CustomerInformation, DeleteCertificate, FirmwareStatusNotification,
Get15118EVCertificate, GetCertificateChainStatus, GetCertificateStatus,
GetChargingProfiles, GetCompositeSchedule, GetDisplayMessages,
GetInstalledCertificateIds, GetLog, GetMonitoringReport,
GetPeriodicEventStream, GetTariffs, GetTransactionStatus,
InstallCertificate, LogStatusNotification, MeterValues,
NotifyAllowedEnergyTransfer, NotifyChargingLimit, NotifyCustomerInformation,
NotifyDERAlarm, NotifyDERStartStop, NotifyDisplayMessages,
NotifyEVChargingNeeds, NotifyEVChargingSchedule, NotifyEvent,
NotifyMonitoringReport, NotifyPeriodicEventStream, NotifyPriorityCharging,
NotifySettlement, NotifyWebPaymentStarted, OpenPeriodicEventStream,
PublishFirmware, PublishFirmwareStatusNotification,
PullDynamicScheduleUpdate, ReportChargingProfiles, ReportDERControl,
RequestBatterySwap, ReservationStatusUpdate, SetChargingProfile,
SetDefaultTariff, SetMonitoringBase, SetMonitoringLevel, SetNetworkProfile,
SetVariableMonitoring, SignCertificate, UnpublishFirmware, UpdateFirmware,
UsePriorityCharging, VatNumberValidation

**Plus** five messages `ocpp-types` defines but `ocpp-client` generates no
action for: TriggerMessage, SetDisplayMessage, GetDERControl, SetDERControl,
UpdateDynamicSchedule — see [D1](#61-d1--missing-action-wrappers).

### A.4 Other verified figures

| Figure | Value | Source |
|--------|-------|--------|
| Device-model rows in the 2.1 appendix | 438 | `docs/OCPP-2.1/Appendices_CSV_v2.1/dm_components_vars.csv` |
| …marked Required | 122, across 23 components | same |
| …registered by this crate | 2 (`AuthCtrlr.AuthorizeRemoteStart`, `OCPPCommCtrlr.HeartbeatInterval`) | `src/state/device_model.rs` |
| 1.6J standard config keys aliased | 12 | `src/device_model.rs` |
| Security event types in the appendix | 21 | `…/security_events.csv` |
| …modelled in `SecurityEventType` | 18 | `src/state/security_event.rs` |
| Protocol trait bounds on `setup()`'s CSMS parameter | 21 (+ `Clone`/`Send`/`Sync`/`'static`) | `src/setup.rs:51` |
| Test functions in `src/` | 668 | `#[test]` + `#[tokio::test]` |
| Integration tests | 1 | `tests/` |
