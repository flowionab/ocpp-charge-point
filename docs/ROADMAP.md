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
  `StatusNotifier`) implemented for `ocpp-client`'s OCPP 2.1 client.
  `setup()` itself still only takes an already-connected client, but a new
  `connect_and_setup` (`src/connect.rs`, `std`+`websocket`+`ocpp_2_1`-gated)
  now closes that gap for std/tokio users: it dials `address` with
  `ocpp_client::connect_2_1` and hands the resulting client straight to
  `setup()`, verified end-to-end against a real local WebSocket server in
  `tests/connect_2_1_websocket.rs`. A new `websocket` feature (forwarding
  to `ocpp-client/websocket`, implying `tokio-runtime`) is in `default` for
  zero-config ergonomics; embedded/no_std targets or std users needing a
  non-WebSocket transport still construct their own client and call
  `setup()` directly. Inbound OCPP call handling has now started (see §6 -
  `UnlockConnector`, `RequestStartTransaction`, and `RequestStopTransaction`
  - and §7 - `ChangeAvailability` - which are wired end-to-end); other
  CSMS-initiated actions still aren't. `TriggerMessage` is a special case
  worth calling out here too: it has a protocol-agnostic internal handler
  (§6) but no wire adapter is even possible yet - the OCPP 2.1 message
  types don't exist upstream in `rust-ocpp`/`ocpp-client` at all (unlike
  every other action in this list). `connect_and_setup` only covers OCPP
  2.1 (the crate's primary target per `CLAUDE.md`), not 1.6J/2.0.1.
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
- ✅ Real `no_std` support. `lib.rs` gates on `#![cfg_attr(not(feature =
  "std"), no_std)]` - `cargo check --no-default-features --lib` now
  genuinely compiles this crate under `#![no_std]` (verified directly, not
  just "features happen to be off"), and `tokio` is a fully optional
  dependency behind a `tokio-runtime` feature. What got it there (kept as
  a changelog since each piece landed in its own step):
  - ✅ `ocpp-client = { ..., default-features = false }`, with our own
    features forwarding only what's needed (`ocpp_1_6`/`ocpp_2_0_1`/
    `ocpp_2_1` map to its same-named features; our `std` feature forwards
    to its `std`). `ocpp-client`'s own `std`/`tokio-runtime` are no longer
    pulled in transitively regardless of our feature flags.
  - ✅ `chrono`'s `clock` feature (needed for `Utc::now()`, itself a hard
    std dependency) is now gated behind our own `std` feature rather than
    unconditional. A new `clock::Clock` trait (mirroring `Backoff`) plus a
    `std`-gated `clock::SystemClock` impl replace direct `Utc::now()`
    calls. The two OCPP2_1Client adapters that need a timestamp
    (`StatusNotifier`, `TransactionNotifier`) now live in a
    `with_system_clock` submodule requiring both `ocpp_2_1` **and** `std`;
    `BootNotifier`/`HeartbeatSender`/`Authorizer` needed no timestamp and
    were already std-independent. `cargo build`/`cargo test` with default
    features (`std` off) now succeed cleanly (42 tests), as does
    `--no-default-features` (32 tests) and `--features std` /
    `--all-features`.
  - ✅ `setup()`'s four background loops (heartbeat, status, transaction,
    authorization) go through a new `executor::Executor` trait (mirroring
    `ocpp-client`'s own `Executor`) instead of calling `tokio::spawn`
    directly - `setup()` takes an `executor: X` parameter (plus a
    `backoff: B` parameter, replacing the previously hardcoded
    `TokioBackoff`), so std/tokio users pass
    `executor::TokioExecutor`/`provisioning::TokioBackoff` explicitly and
    embedded targets can supply their own.
  - ✅ `src/sync.rs` (new): no_std+alloc `embassy-sync`-backed replacements
    for `tokio::sync::{oneshot, mpsc, watch, broadcast}` - `OneShot`,
    `Chan` (the actor's mailbox), `Watch{Sender,Receiver}` (the
    `ChargePointState` broadcast `subscribe()`/`state()` relies on), and
    payload-carrying `Broadcast{Sender,Receiver}` (status/transaction/
    authorization fan-out; `HardwareCommandReceiver` too). Mirrors
    `ocpp-client`'s own `src/sync.rs` pattern (`CriticalSectionRawMutex`-
    backed blocking `Mutex`/`Signal`), extended with `Watch` and a
    payload-carrying `Broadcast` that crate didn't need. `actor.rs`/
    `runtime.rs`/`availability.rs`/`transactions.rs`/`authorization.rs`/
    `hardware/command_receiver.rs` all rewired onto these instead of
    `tokio::sync` types. Deliberate simplifications, documented in
    `src/sync.rs`'s module docs: every queue is unbounded (no
    `tokio::sync::broadcast`-style "Lagged" case - a stalled subscriber
    grows its queue instead of dropping messages), and the actor's `run`
    loop no longer detects "all senders dropped" and exit (it used to via
    `mpsc::Receiver::recv() -> None`; now it just parks forever once
    nothing sends - matching `ocpp-client`'s own long-running-task
    philosophy, and not exercised by anything today since actors live for
    the process lifetime in practice).
  - `embassy-sync`/`critical-section` are new unconditional dependencies
    (mirroring `ocpp-client`'s choice to keep them unconditional too).
    `critical-section` needs a backend registered to even *link* (not just
    compile) - our `std` feature forwards to `critical-section/std` for
    that, and **`std` is now in this crate's `default` features** (added
    this step), matching `ocpp-client`'s own convention: `cargo build`/
    `cargo test` with zero flags keeps working with no config, and
    `--no-default-features` is the deliberate no_std opt-in, where an
    embedded target must register its own backend via
    `critical_section::set_impl!` (and supply its own
    `Executor`/`Backoff`/`Clock`). Without a registered backend, the
    library still *compiles* fine (confirmed above) - only *linking* a
    final binary/test needs one, which is exactly the expected no_std
    story.
  - ✅ `tokio` is now a genuinely optional dependency (`optional = true`),
    gated behind a new `tokio-runtime` feature (`tokio-runtime = ["std",
    "dep:tokio"]`, mirroring `ocpp-client`'s own feature of the same
    name). `TokioExecutor`/`TokioBackoff` now require `tokio-runtime`
    specifically (not just `std` - a std binary using a non-tokio async
    executor is a real, supported configuration: it gets `std`'s
    ocpp-client/chrono/critical-section conveniences without pulling in
    tokio at all). Closing this required finishing the one piece left out
    of the previous step: `ChargePointActor::spawn`'s own
    `tokio::spawn(run(...))` call (the actor's core run loop) now takes an
    `executor: &dyn Executor` parameter too (object-safe, so `&dyn` avoids
    a new generic on `ChargePointRuntime<T>`), threaded through
    `ChargePointRuntime::new` and `setup()`. This was no longer optional
    once `tokio` became an optional dependency: without it,
    `ChargePointActor::spawn`'s hardcoded `tokio::spawn` call wouldn't
    even resolve the `tokio` crate name under `--no-default-features`.
    `default` features now list `tokio-runtime` (which implies `std`)
    instead of `std` directly.
  - Verified across three configurations: `cargo check --no-default-features
    --lib` (true no_std, still compiles clean under `#![no_std]`);
    `cargo check --no-default-features --features
    std,ocpp_1_6,ocpp_2_0_1,ocpp_2_1 --lib` (the new intermediate
    "std, no tokio" configuration - compiles clean); and `cargo test`/
    `cargo test --all-features` (43 tests each) plus `cargo run --example
    simple` (runs correctly end-to-end) for the default tokio-runtime
    path.

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
  under the `ocpp_2_1` feature). A live `ocpp-client` WebSocket connection
  is now wired end-to-end via `connect_and_setup` (see §0) for std/tokio
  users; embedded/no_std callers still supply their own connected
  `BootNotifier`/`HeartbeatSender`. Still missing: the Component/Variable
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
  internal model, per the version-adapter principle in `CLAUDE.md`. A
  transaction can now also start via `RequestStartTransaction` (see §6),
  not just a physically presented id token - either way it's the same
  `Started` `TransactionEvent`, since `advance_transaction` triggers on
  any transition into `ConnectorState::Starting` regardless of where it
  came from. It can also be stopped remotely via `RequestStopTransaction`
  (§6), which reuses the existing `ChargingStopped(StopReason::Remote)`
  path unchanged - no new state-machine event was needed there, just a
  new way to reach the existing one. Still missing: `id_token` (needs
  Authorization, §3; a remote-started transaction's token isn't recorded
  either), running totals/energy (needs Meter values, §10), and multiple
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
- Status: 🚧 partial — `UnlockConnector` is implemented, and is also the
  first inbound OCPP call this crate handles at all (previously §0 noted
  none existed). `ocpp-client`'s `Client::on::<A>`/`on_unlock_connector`
  already provide inbound CALL dispatch on the transport side; the new
  piece is `remote_control::handle_unlock_request` (evse_id, connector_id,
  `&ChargePointActor`) - a protocol-agnostic async function, mirroring the
  shape of the other functional-block modules but inbound rather than
  outbound. It rejects out-of-range addresses (`UnknownConnector`) and a
  connector with a transaction in progress
  (`Authorizing`/`Starting`/`Charging`/`Stopping` →
  `OngoingAuthorizedTransaction`, refused rather than interrupted); only a
  `Locked` connector (cable locked, no active transaction) is actually
  unlockable. A new `ConnectorEvent::RemoteUnlockRequested` drives
  `Locked` → `Unlocking` (reusing the existing `Unlocking`/`UnlockConfirmed`
  → `Available` path also used by the fault-clear flow), and the handler
  then awaits the actor's state (via `ChargePointActor::subscribe`) until
  it reaches `Available` (`Unlocked`) or `Faulted`/`FaultedSafe`
  (`UnlockFailed`) - the OCPP `Unlocked` status must reflect an unlock
  that actually happened, not merely one that was requested, so this
  waits for real hardware confirmation rather than answering immediately.
  A protocol-agnostic `UnlockConnectorHandler` trait
  (`register_unlock_connector_handler`), implemented for `ocpp-client`'s
  OCPP 2.1 client, is registered from `setup()` the same way the other
  blocks wire in. `RequestStartTransaction` is implemented too:
  `remote_control::handle_request_start_transaction` (an optional
  `evse_id`, `&ChargePointActor`) finds a `Locked` connector - the one on
  `evse_id`, or, if unspecified, the first `Locked` connector on any EVSE
  - and drives it straight to `Starting` via a new
  `ConnectorEvent::RemoteStartRequested`, deliberately *not* reusing
  `IdTokenPresented`: that event's `Locked` → `Authorizing` transition
  triggers `ChargePointEffect::AuthorizationRequested`, which would fire a
  spurious outbound `Authorize.req` back at the CSMS for a token it just
  told the charge point to accept - the CSMS's own request already *is*
  the authorization decision. `advance_transaction` (in
  `charge_point_state.rs`) was widened from matching only
  `(Authorizing, Starting)` to `(Authorizing | Locked, Starting)` so a
  remote-started transaction gets created and reported the same way a
  physically-authorized one is; it's intentionally not just `(_, Starting)`
  since a connector can self-loop `Starting` → `Starting` (e.g. a meter
  sample applied before the contactor confirms closed) without that being
  a new transaction. Rejects if `evse_id` is out of range or no connector
  addressed by it (or, if unspecified, none at all) is currently `Locked`.
  A `RequestStartTransactionHandler` trait, implemented for `ocpp-client`'s
  OCPP 2.1 client (registering via `Client::on_request_start_transaction`),
  reports the started `Transaction::id` back as the response's
  `transactionId` (stringified) on `Accepted`. `RequestStopTransaction` is
  implemented too, and needed no new `ConnectorEvent`:
  `remote_control::handle_request_stop_transaction` (a `TransactionId`,
  `&ChargePointActor`) finds the connector whose active transaction
  matches the id and, only if it's currently `Charging`, dispatches the
  same `ConnectorEvent::ChargingStopped(StopReason::Remote)` a local stop
  would - `StopReason::Remote` already existed for exactly this case, just
  unreachable until now. Rejects an unknown transaction id, or one that's
  active but not yet `Charging` (e.g. still `Starting`, contactor not
  confirmed closed) - the connector state machine's only stop path is
  `Charging` → `Stopping`, so a not-yet-charging remote-started
  transaction can't be stopped this way yet. A
  `RequestStopTransactionHandler` trait, implemented for `ocpp-client`'s
  OCPP 2.1 client (registering via `Client::on_request_stop_transaction`),
  parses the wire `transactionId` string as a `u64`, treating anything
  that doesn't parse as an unknown transaction. `TriggerMessage` has a
  protocol-agnostic internal handler
  (`remote_control::{TriggerableMessage, handle_trigger_message}`) but
  **no CSMS-facing entry point at all** - blocked upstream, not something
  fixable from this side. `rust-ocpp`'s `v2_1` module (gated behind its
  own `wip_v2_1` feature) never declares a `trigger_message` message
  module in the first place - `v2_1/messages/mod.rs` has no
  `pub mod trigger_message;` line, so there's no
  `TriggerMessageRequest`/`TriggerMessageResponse` to receive at all. This
  is a step behind `NotifyReport`, which at least has an (empty) stub file
  upstream - see `ocpp-client`'s own `CLAUDE.md`, which documents that gap
  the same way. Consequently `ocpp-client`'s OCPP 2.1 actions module has
  no `on_trigger_message`/`send_trigger_message` at all, and no
  `TriggerMessageHandler` trait or `setup()` wiring exists here either -
  adding either now, ahead of something that could implement it, would
  just break every real caller of `setup()` (their `N` could never
  satisfy the bound). What does exist: `TriggerableMessage` covers the
  subset of OCPP's 13-value `MessageTriggerEnumType` this crate can
  actually fulfil today, each backed by an outbound trait it already has
  - `Heartbeat` (via `HeartbeatSender`) and `StatusNotification` (via
  `StatusNotifier`, addressed with the same
  `crate::availability::AvailabilityTarget` `ChangeAvailability` uses -
  charge-point-wide, one EVSE, or one connector). `BootNotification` isn't
  covered - it needs hardware vendor/model strings this module has no
  access to (only `setup()` does). `MeterValues`/`TransactionEvent` aren't
  either - both need a "resend the current snapshot" capability neither
  functional block has (§10's meter reporting and §5's transaction
  reporting are both purely event-driven today). The remaining six wire
  values (log/firmware/certificate triggers, `CustomTrigger`) have no
  supporting functional block at all (§1, §12). A transport failure while
  (re-)sending is logged, not treated as a rejection - `Accepted` here
  means the charge point attempted the trigger, matching how
  `run_status_notifications` already treats a failed report elsewhere in
  this file. `TriggerMessageStatusEnumType::NotImplemented` has no
  equivalent on the internal `TriggerMessageOutcome` (`Accepted`/
  `Rejected` only) - it only makes sense once a wire adapter can receive
  a `MessageTriggerEnumType` this module has no variant for at all, so
  that mapping belongs entirely to the (not yet possible) wire layer.
- Version notes: 1.6J's `RemoteStartTransaction`/`RemoteStopTransaction`
  map directly; `TriggerMessage` payload options differ per version, and -
  once the upstream gap above closes - 1.6J's own `TriggerMessage` is
  already wired up in `ocpp-client` (`ocpp_1_6::actions`), so that version
  isn't blocked the way 2.1 is.

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
  OCPP 2.1 client). Inbound `ChangeAvailability` is now wired too:
  `availability::handle_change_availability_request` (an `AvailabilityTarget`
  - `ChargePoint`/`Evse { evse_id }`/`Connector { evse_id, connector_id }`,
  the collapsed form of OCPP's optional `evse`/`connectorId` addressing -
  plus `available: bool`, and `&ChargePointActor`) is a protocol-agnostic
  async function, mirroring §6's `remote_control::handle_unlock_request`
  but simpler: `SetAvailable`/`SetUnavailable` apply synchronously within
  the actor (no hardware round-trip), so it rejects an out-of-range
  EVSE/connector and otherwise dispatches the matching
  `ChargePointEvent`/`EvseEvent`/`ConnectorEvent::SetAvailable`/
  `SetUnavailable` and immediately accepts - it doesn't wait for a
  confirming state change the way the unlock handler does. A
  `ChangeAvailabilityHandler` trait, implemented for `ocpp-client`'s OCPP
  2.1 client (registering via `Client::on_change_availability`), is wired
  in from `setup()` the same way. Still missing: EVSE-level and
  charge-point-level availability changes (`EvseStatus`/`LifecycleState`
  going Unavailable/Faulted) don't yet fan out to per-connector
  StatusNotifications, `Reserved` is unreachable until §8 Reservation
  exists, and OCPP's `Scheduled` status (deferring a `ChangeAvailability`
  until an in-progress transaction ends, rather than interrupting it) isn't
  modeled - `SetUnavailable` always applies immediately, even mid-transaction.
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
- Internal state needed: `state::MeterSample` (currently just an energy
  register reading); `Transaction::last_meter_sample`;
  `ConnectorEvent::MeterValueSampled`, which the hardware integration
  pushes in (same pattern as `CableConnected`/`IdTokenPresented` - no
  framework-driven polling loop, since `ChargePointRuntime<T>` doesn't hold
  a `'static`-safe shared reference to hardware `T`). A sample is only
  recorded (and reported) while the connector's transaction is `Charging`;
  it's dropped otherwise. Recording a sample bumps `seq_no` and emits
  `TransactionEventKind::Updated(TransactionUpdateReason::MeterValuePeriodic)`,
  which the 2.1 adapter (`transactions::ocpp_2_1::build_meter_values`)
  embeds as the `meterValue` of the next `TransactionEvent`.
- Status: 🟨 in progress — energy register (Wh) sampling and reporting via
  embedded `TransactionEvent.meterValue` is implemented and tested. Not yet
  done: standalone `MeterValues` for 1.6J, additional measurands
  (power, current, voltage, SoC), sampled-data configuration, and
  clock-aligned periodic scheduling (still requires an integrator-owned
  timer today; the crate has no scheduling hook for it).
- Version notes: measurand/unit enums are close to compatible across
  versions; sampling-context differs slightly. 1.6J needs a standalone
  `MeterValues` sender, not yet built.

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
