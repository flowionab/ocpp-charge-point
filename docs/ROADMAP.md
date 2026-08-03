# Roadmap: full OCPP 2.1 charge point coverage

This roadmap tracks the work needed to represent a fully compliant OCPP 2.1
charge point, organized by OCPP **functional block** (the grouping the OCPP
2.0.1/2.1 specification itself uses). Each block lists its purpose, the
internal state/model it needs, the key OCPP messages it drives, current
status in this repo, and notes on projecting it down to OCPP 2.0.1 and
1.6J.

Only the OCPP 2.0.1 spec PDFs are currently vendored under `docs/`. The 2.1
delta is drawn from public knowledge of the 2.1 release and should be
re-verified against the official 2.1 spec text once it's available locally
— items below marked **(verify vs 2.1 spec)** need that check.

Status legend: ✅ done · 🚧 partial · ⬜ not started

---

## 0. Cross-cutting foundation

Not a functional block, but a prerequisite for all of them.

- 🚧 Actor model core (`ChargePointActor`, `ChargePointState`) — exists, but
  only covers lifecycle/EVSE/connector, not transactions or the wider
  device model.
- 🚧 Hardware abstraction (`crate::hardware`: `ChargePoint`, `Evse`,
  `Connector`) — covers lock/unlock/contactor only; no metering, no
  temperature/safety hooks yet.
- 🚧 `ocpp-client` wiring — outbound state → OCPP calls now exist for
  BootNotification, Heartbeat, and StatusNotification (see §2, §7), each
  via a small protocol-agnostic trait (`BootNotifier`, `HeartbeatSender`,
  `StatusNotifier`) implemented for `ocpp-client`'s OCPP 2.1 client. Still
  missing: `setup()` doesn't itself open a live connection - callers must
  construct and pass in an already-connected client - and there is no
  inbound OCPP call handling yet (CSMS-initiated actions like
  `ChangeAvailability`, `RequestStartTransaction`, etc.).
- ⬜ Protocol-version-independent core → version adapters. The state model
  needs to be designed so a single internal representation projects down
  to 1.6J, 2.0.1, and 2.1 wire shapes.
- ⬜ Version negotiation / connection lifecycle (connecting, reconnecting,
  offline message queueing, backoff).
- 🚧 Erratic-hardware fault containment — `Faulted`/`FaultedSafe` exist for
  connectors; not yet generalized to EVSE/charge-point-level hardware
  faults (meter stalls, contactor stick, sensor glitches), and no fault
  injection tests yet.
- ⬜ Rustdoc coverage pass on all public APIs (see `CLAUDE.md` documentation
  standard).
- ⬜ Real `no_std` support. `lib.rs` already gates on `#![cfg_attr(not(feature
  = "std"), no_std)]`, but this is aspirational today: `tokio`'s
  mpsc/broadcast/oneshot/watch/time are hard, unconditional dependencies in
  `actor.rs`, `runtime.rs`, `provisioning.rs` (`TokioBackoff`), and
  `availability.rs` (its status-change broadcast channel); `chrono`'s
  `clock` feature (used in `availability.rs` to timestamp
  StatusNotification) is a hard std dependency too; and our `ocpp-client`
  dependency isn't `default-features = false` - its own `std` and
  `tokio-runtime` features are always on regardless of our `std` feature.
  Closing this gap needs: an `Executor`/`Timer` abstraction (`ocpp-client`
  already has one - see its `Executor`/`Timer` traits and
  `TokioExecutor`/`TokioTimer`) so the actor and any `Backoff` impl don't
  hard-depend on tokio, a non-`clock` way to obtain the current time (an
  injected clock trait, mirroring `Backoff`), and gating `ocpp-client`'s
  features behind our own `std` feature so embedded targets don't pull in
  std/tokio transitively.

---

## 1. Security

Secures the OCPP connection and reports security-relevant events.

- Messages: `SecurityEventNotification`, `SignCertificate`,
  `CertificateSigned`, `Get15118EVCertificate`, `GetCertificateStatus`,
  `DeleteCertificate`, `InstallCertificate`, `GetInstalledCertificateIds`.
- Internal state needed: certificate store abstraction, security event log,
  security profile (1/2/3) configuration.
- Status: ⬜ not started.
- Version notes: OCPP 1.6J only has basic auth / TLS via security
  whitepaper extensions, no in-band certificate messages — this block
  mostly collapses to "not applicable" under 1.6J.

## 2. Provisioning

Boot, configuration, and the Component/Variable device model.

- Messages: `BootNotification`, `Heartbeat`, `SetVariables`,
  `GetVariables`, `GetBaseReport`, `GetReport`, `NotifyReport`, `Reset`,
  `SetNetworkProfile`.
- Internal state needed: registration status (Pending/Accepted/Rejected),
  device model (Component/Variable graph) with mutability and persistence,
  reset request handling (Immediate/OnIdle) per EVSE and charge-point-wide.
- Status: 🚧 partial — `ChargePointState.registration` (`RegistrationStatus`:
  Accepted/Pending/Rejected) plus `ChargePointEvent::RegistrationStatusReceived`
  land the BootNotification result in the actor's state machine, driven
  through a version-agnostic `provisioning::BootNotifier` trait (implemented
  for `ocpp-client`'s OCPP 2.1 client under the `ocpp_2_1` feature).
  `ChargePointRuntime::register` performs one attempt;
  `ChargePointRuntime::register_until_accepted` retries indefinitely on
  `Pending`/`Rejected` (using the response's `interval_secs`) or on a
  transport failure (using `DEFAULT_RETRY_INTERVAL_SECS`), via a pluggable
  `Backoff` trait (`TokioBackoff` by default) - and now returns the accepted
  `BootNotificationOutcome` (including the heartbeat interval). `setup()`
  calls `register_until_accepted` after hardware start, so a charge point
  doesn't reach `Available` until the CSMS has actually accepted it, then
  spawns `provisioning::run_heartbeat` as a background task that sends a
  `Heartbeat` via a `HeartbeatSender` every accepted interval, forever
  (implemented for `ocpp-client`'s OCPP 2.1 client alongside `BootNotifier`
  under the `ocpp_2_1` feature). Still missing: an actual live `ocpp-client`
  connection wired end-to-end (today callers must supply their own
  connected `BootNotifier`/`HeartbeatSender`), the Component/Variable
  device model, and `Reset`.
- Version notes: 1.6J's `Configuration` key/value model and 2.0.1's
  Component/Variable model both need to be projections of one internal
  config representation; 1.6J has no `GetBaseReport`/structured device
  model, only flat `GetConfiguration`/`ChangeConfiguration`.

## 3. Authorization

Deciding whether an identifier is allowed to start/continue charging.

- Messages: `Authorize`, `IdTokenInfo` on transaction events.
- Internal state needed: an authorization request/response flow, group
  id tokens, cache TTL, offline authorization policy.
- Status: 🚧 partial — `ConnectorState` gained an `Authorizing` step between
  `Locked` and `Starting`: presenting an `IdToken` (`ConnectorEvent::
  IdTokenPresented`) moves a locked connector into `Authorizing` and emits
  a `ChargePointEffect::AuthorizationRequested`; the CSMS's decision comes
  back as `ChargingAuthorized` (→ `Starting`, and now also creates the
  transaction - see §5) or `AuthorizationDenied` (→ back to `Locked`).
  Wired end-to-end the same way as the other blocks: a broadcast channel on
  the actor, a protocol-agnostic `Authorizer` trait
  (`authorization::run_authorization_requests`, which treats a
  transport-level failure as a denial rather than hanging), implemented
  for `ocpp-client`'s OCPP 2.1 client, spawned from `setup()`. Our
  `AuthorizationStatus` is deliberately binary (Accepted/Rejected) — the
  OCPP 2.1 adapter collapses the wire spec's 10-value
  `AuthorizationStatusEnumType` down to this, since nothing downstream
  (yet) distinguishes *why* a token was rejected; `ConcurrentTx` in
  particular is folded into `Rejected` even though the spec's own guidance
  ("advised to not stop charging if status is Accepted or ConcurrentTx")
  suggests it may deserve different treatment once transactions can run
  concurrently per token. Still missing: group id tokens, cache TTL/offline
  authorization policy, and `idTokenInfo`'s richer fields (e.g.
  `evseId`-scoped validity).
- Version notes: 1.6J's `Authorize.req`/`.conf` maps closely; 2.1 adds
  richer `IdTokenInfo` (groups, restrictions) that must downgrade to
  1.6J's flatter `idTagInfo`.

## 4. Local authorization list management

Offline authorization without a CSMS round-trip.

- Messages: `SendLocalList`, `GetLocalListVersion`.
- Internal state needed: versioned local list storage, diffing
  (differential vs full updates).
- Status: ⬜ not started.
- Version notes: present in both 1.6J and 2.0.1/2.1 with compatible
  semantics — low downgrade risk.

## 5. Transactions

The core charging session lifecycle.

- Messages: `TransactionEvent` (Started/Updated/Ended) in 2.x;
  `StartTransaction`/`StopTransaction`/`MeterValues` in 1.6J.
- Internal state needed: a first-class `Transaction` entity (id, start/stop
  trigger reason, stop reason, charging state, linked id token, running
  totals) distinct from `ConnectorState`. Today `ConnectorState::Starting`/
  `Charging` conflate connector and transaction state.
- Status: 🚧 partial — a first-class `Transaction` entity now exists
  (`TransactionId`, `TransactionChargingState`, `StopReason`, `seq_no`),
  separate from `ConnectorState` and tracked per-connector on `EvseState`.
  The connector state machine gained a normal charge-stop path (`Stopping`,
  `Finishing`, `ConnectorEvent::ChargingStopped(StopReason)`) - previously
  the only way out of `Charging` was a hardware fault. `ChargePointState`
  emits `ChargePointEffect::TransactionEvent` on Started (authorized while
  locked), Updated (contactor confirms closed), and Ended (contactor
  confirms open after a normal stop, or immediately on a hardware fault
  while a transaction is active, with `StopReason::EmergencyStop`). Wired
  end-to-end the same way as Provisioning/Availability: a broadcast channel
  on the actor, a protocol-agnostic `TransactionNotifier` trait
  (`transactions::run_transaction_events`), implemented for `ocpp-client`'s
  OCPP 2.1 client, spawned from `setup()`. `StopReason` only covers
  Local/Remote/EVDisconnected/EmergencyStop so far (a subset of the full
  spec `ReasonEnumType`) since RemoteControl/Authorization, which would
  supply richer reasons, don't exist yet; `TriggerReasonEnumType` is
  derived entirely in the OCPP 2.1 adapter rather than carried in the
  internal model, per the version-adapter principle in `CLAUDE.md`. Still
  missing: `id_token` (needs Authorization, §3), running totals/energy
  (needs Meter values, §10), `RequestStartTransaction`/
  `RequestStopTransaction` (needs Remote control, §6), and multiple
  `Updated` events per transaction (today only the single Charging
  transition produces one).
- Version notes: this is the highest-value adapter target — 2.x's single
  `TransactionEvent` stream must project down to 1.6J's discrete
  Start/Stop/MeterValues calls.

## 6. Remote control

CSMS-initiated control of the charge point.

- Messages: `RequestStartTransaction`, `RequestStopTransaction`,
  `UnlockConnector`, `TriggerMessage`.
- Internal state needed: inbound command handling that maps to existing
  `ChargePointEvent`/`ConnectorEvent` variants, plus request/response
  correlation back to the CSMS call.
- Status: ⬜ not started (no inbound OCPP call handling exists yet — see
  §0 network wiring).
- Version notes: 1.6J's `RemoteStartTransaction`/`RemoteStopTransaction`
  map directly; `TriggerMessage` payload options differ per version.

## 7. Availability

Operational availability of charge point / EVSE / connector.

- Messages: `StatusNotification`, `ChangeAvailability`,
  `NotifyEvent` (2.x general event reporting).
- Internal state needed: exists today (`LifecycleState`, `EvseStatus`,
  `ConnectorState`) but nothing emits `StatusNotification` outbound, and
  `ChangeAvailability` isn't wired as an inbound trigger.
- Status: 🚧 partial — outbound `StatusNotification` is wired for
  connectors: `ConnectorState::availability_status()` maps the internal
  connector state down to the 5-value `ConnectorStatus`
  (Available/Occupied/Reserved/Unavailable/Faulted, matching OCPP 2.x's
  `ConnectorStatusEnumType`); `ChargePointState::apply` emits a
  `ChargePointEffect::StatusNotification` exactly when that mapped status
  changes (not on every internal transition, e.g. `Locked` → `Charging`
  stays `Occupied` and reports nothing); the actor broadcasts these on a
  dedicated channel (`ChargePointActor::subscribe_status_notifications`);
  and `setup()` spawns `availability::run_status_notifications` to forward
  them to the CSMS via a `StatusNotifier` (implemented for `ocpp-client`'s
  OCPP 2.1 client). Still missing: EVSE-level and charge-point-level
  availability changes (`EvseStatus`/`LifecycleState` going
  Unavailable/Faulted) don't yet fan out to per-connector
  StatusNotifications, `Reserved` is unreachable until §8 Reservation
  exists, and `ChangeAvailability` isn't wired as an inbound trigger.
- Version notes: status enum values differ between 1.6J and 2.0.1/2.1
  (`Reserved`, `SuspendedEV`, `SuspendedEVSE` etc. only exist from 2.x
  onward at connector level in some cases) — the internal enum must be a
  superset that downgrades cleanly.

## 8. Reservation

Reserving a connector/EVSE ahead of use.

- Messages: `ReserveNow`, `CancelReservation`.
- Internal state needed: reservation entity (id, expiry, id token,
  target EVSE/connector), a `Reserved` connector state, expiry timer.
- Status: ⬜ not started — no `Reserved` state exists in
  `ConnectorState` yet.
- Version notes: broadly compatible between 1.6J and 2.x.

## 9. Tariff and cost

Communicating price/cost to the driver.

- Messages: `NotifyPriceSchedule` (2.1), `CostUpdated`, running cost in
  `TransactionEvent`.
- Internal state needed: tariff model, running-cost accumulation hook.
- Status: ⬜ not started.
- Version notes: **(verify vs 2.1 spec)** — tariff/cost was extended
  significantly across 2.0.1 → 2.1; not present in 1.6J at all (block is
  a no-op under that adapter).

## 10. Meter values

Energy/power measurement reporting.

- Messages: `MeterValues` (1.6J standalone; embedded in `TransactionEvent`
  for 2.x), sampled data configuration, clock-aligned data intervals.
- Internal state needed: a metering hardware trait (currently absent from
  `crate::hardware`), sampled-value buffering, clock-aligned scheduling.
- Status: ⬜ not started — no metering hook exists in the hardware layer
  at all yet.
- Version notes: measurand/unit enums are close to compatible across
  versions; sampling-context differs slightly.

## 11. Smart charging

Charging profiles and schedule negotiation.

- Messages: `SetChargingProfile`, `ClearChargingProfile`,
  `GetChargingProfiles`, `GetCompositeSchedule`, `NotifyChargingLimit`,
  `ReportChargingProfiles`.
- Internal state needed: charging profile store, schedule composition
  logic, external limit inputs (local/grid), a schedule → hardware current
  limit projection.
- Status: ⬜ not started.
- Version notes: 2.1 adds richer profile purposes and DER-linked limits
  **(verify vs 2.1 spec)**; 1.6J smart charging is optional-profile and a
  strict subset.

## 12. Firmware management

Over-the-air firmware updates.

- Messages: `UpdateFirmware`, `FirmwareStatusNotification`,
  `PublishFirmware` (2.x local distribution).
- Internal state needed: firmware update state machine
  (Downloading/Downloaded/Installing/Installed/Failed), signature
  verification hook.
- Status: ⬜ not started.
- Version notes: 1.6J's `FirmwareStatusNotification` status enum is a
  subset of 2.x's.

## 13. ISO 15118 certificate management

Plug-and-charge certificate handling for EVs.

- Messages: `Get15118EVCertificate`, `GetCertificateStatus`,
  `InstallCertificate`, certificate exchange during charging.
- Internal state needed: EV certificate relay hook, dependent on ISO
  15118 support at the hardware/communication controller level.
- Status: ⬜ not started; likely depends on hardware capability detection
  (not all chargers have an ISO 15118 PLC modem).
- Version notes: not applicable to 1.6J.

## 14. Diagnostics

Log/diagnostics retrieval for troubleshooting.

- Messages: `GetLog`, `LogStatusNotification`, (1.6J: `GetDiagnostics`,
  `DiagnosticsStatusNotification`).
- Internal state needed: log upload state machine, log source
  abstraction (what logs exist, how they're packaged).
- Status: ⬜ not started.
- Version notes: 1.6J's diagnostics flow maps to 2.x's `GetLog` flow with
  a narrower log-type set.

## 15. Display message

Messages shown to the driver on charge point UI.

- Messages: `SetDisplayMessage`, `GetDisplayMessages`,
  `ClearDisplayMessage`, `NotifyDisplayMessages`.
- Internal state needed: message store keyed by priority/state,
  hardware hook for rendering (LEDs/screen — ties into the "UI" hardware
  binding mentioned in the README).
- Status: ⬜ not started.
- Version notes: not applicable to 1.6J.

## 16. Data transfer

Vendor-specific extension channel.

- Messages: `DataTransfer`.
- Internal state needed: a pass-through hook so integrators can register
  vendor-specific handlers without the crate needing to understand the
  payload.
- Status: ⬜ not started.
- Version notes: present and compatible across all three versions.

## 17. Bidirectional power / DER control (2.1)

V2X/V2G power export and distributed energy resource control.

- Messages: `NotifyAllowedEnergyTransfer`, DER-related `SetVariables` on
  DER components, DER curve configuration.
- Internal state needed: energy transfer mode negotiation
  (import/export), DER capability model.
- Status: ⬜ not started. **(verify vs 2.1 spec)**
- Version notes: 2.1-only; not applicable to 1.6J/2.0.1. Depends on
  hardware supporting bidirectional power electronics — likely needs a
  capability flag so non-V2X hardware simply reports the block as
  unsupported.

## 18. Battery swap (2.1)

Support for battery-swap style "charging" stations.

- Messages: battery swap specific `NotifyBatterySwap` /
  swap-related transaction extensions. **(verify vs 2.1 spec)**
- Internal state needed: swap station/bay state model, distinct from the
  connector-based charging model.
- Status: ⬜ not started, and likely out of scope unless a hardware
  partner targets battery-swap hardware specifically — flag as
  low-priority pending a concrete use case.

---

## Suggested sequencing

The functional blocks above are independent for planning purposes, but in
practice §0 (foundation) → §2 (Provisioning/BootNotification) → §7
(Availability/StatusNotification) → §5 (Transactions) → §3 (Authorization)
→ §10 (Meter values) form the critical path to a minimally useful charger
that can actually hold a session with a CSMS. Everything else (§1, §4,
§6, §8, §9, §11–18) layers on top once that spine exists.
