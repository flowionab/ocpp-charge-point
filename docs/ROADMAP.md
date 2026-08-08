# Roadmap: full OCPP 2.1 charge point coverage

This roadmap tracks the work needed to represent a fully compliant OCPP 2.1
charge point, organized by OCPP **functional block** (the grouping the OCPP
2.0.1/2.1 specification itself uses). Each block lists its purpose, the
internal state/model it needs, the key OCPP messages it drives, current
status in this repo, and notes on projecting it down to OCPP 2.0.1 and
1.6J.

~~Only the OCPP 2.0.1 spec PDFs are currently vendored under `docs/`.~~
**Stale as of 2026-08-06:** `docs/OCPP-2.1/` now holds the full 2.1
edition-2 spec set (parts 0–6, plus the CSV appendices), and
`docs/UPSTREAM-GAPS.md` audits against it directly. Items below still
marked **(verify vs 2.1 spec)** were written before that and have not yet
been re-checked against the real text — the material to do so is now
present, so those markers are a to-do, not a blocker.

Status legend: ✅ done · 🚧 partial · ⬜ not started

---

## 0. Cross-cutting foundation

Not a functional block, but a prerequisite for all of them.

- 🚧 Actor model core (`ChargePointActor`, `ChargePointState`) — no longer
  just lifecycle/EVSE/connector. `ChargePointState` now also owns
  transactions (§5), reservations (§8), the local authorization list (§4),
  running cost (§9), a pending `Reset` (§2), and the Component/Variable
  device model (§2) - all mutated only through `ChargePointEvent`, none of
  it shared mutable state. Still open: persistence (nothing in the state
  model survives a restart today - see `VariableAttribute::persistent`,
  recorded but not acted on), and the `spawn` bound choices noted below.
- 🚧 Hardware abstraction (`crate::hardware`: `ChargePoint`, `Evse`,
  `Connector`) — lock/unlock/contactor, plus metering pushed in by the
  integrator (`ConnectorEvent::MeterValueSampled`, §10) and `Evse::reboot`
  for `Reset` (§2). Adding `reboot` (and its `Evse::Error` associated type)
  was a deliberate breaking change to the integrator surface, taken because
  `execute_hardware_command` only ever receives `&[E]`. Still missing:
  temperature/safety hooks, and any hardware-capability model (nothing lets
  hardware declare that it can't do bidirectional power, ISO 15118, or a
  display - see §13, §15, §17).
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
  `setup()` directly. Inbound OCPP call handling is now broad rather than
  just "started": `UnlockConnector`, `RequestStartTransaction`,
  `RequestStopTransaction` (§6), `ChangeAvailability` (§7), `ReserveNow`,
  `CancelReservation` (§8), `SendLocalList`, `GetLocalListVersion` (§4),
  `CostUpdated` (§9), and `GetVariables`, `SetVariables`, `GetBaseReport`,
  `GetReport`, `Reset` (§2) are all wired end-to-end through `setup()`.
  What's still unwired is listed per block below - chiefly `SetNetworkProfile`
  (§2), the certificate messages (§1), and everything in §11-§18.
  `TriggerMessage` is a special case
  worth calling out here too: it has a protocol-agnostic internal handler
  (§6) but no 2.1 wire adapter is possible yet - the OCPP 2.1 message
  types don't exist upstream in `rust-ocpp`/`ocpp-client` at all (unlike
  every other action in this list). Note this blocker is narrower than it
  used to read: `OCPP2_0_1Client` and `OCPP1_6Client` *do* have real
  `on_trigger_message`/`send_trigger_message` methods in `ocpp-client`
  0.2.0 (re-verified against the vendored source), so 2.0.1 and 1.6J
  adapters are buildable today - see §6. `connect_and_setup` only covers
  OCPP 2.1 (the crate's primary target per `CLAUDE.md`), not 1.6J/2.0.1.
- 🚧 Protocol-version-independent core → version adapters. The pattern
  itself already existed implicitly - every functional block already
  defines a protocol-agnostic trait (`BootNotifier`, `StatusNotifier`,
  `TransactionNotifier`, etc.) with a per-version `impl ... for
  OCPP2_1Client` in its own `ocpp_2_1` submodule - but until now every
  single one of those `impl`s targeted `OCPP2_1Client` only (verified
  directly - grepped for `impl .* for OCPP1_6Client`/`OCPP2_0_1Client`
  across the whole crate: zero hits before this). So the "single internal
  representation projects down to 1.6J/2.0.1/2.1" claim was aspirational,
  not demonstrated. First real projections landed for Provisioning (§2):
  `provisioning.rs` now has `ocpp_1_6`/`ocpp_2_0_1` submodules alongside
  the existing `ocpp_2_1` one, each with its own `build_request`/
  `map_status` and `BootNotifier`/`HeartbeatSender` impls for
  `OCPP1_6Client`/`OCPP2_0_1Client` - the same internal
  `BootNotificationOutcome`/`RegistrationStatus` now genuinely projects to
  all three wire shapes, not just 2.1's. 2.0.1's `BootNotificationRequest`
  turned out byte-for-byte identical in shape to 2.1's (same
  `ChargingStation`/`BootReasonEnum`/`RegistrationStatusEnum`), so that
  adapter is close to a copy of the 2.1 one; 1.6J's differs more (no
  `reason` field - that's a 2.x addition - and flattens
  `charging_station.vendor_name`/`model` into top-level
  `charge_point_vendor`/`charge_point_model`), which is exactly the kind
  of version-specific projection this bullet is about. Each new submodule
  is compiled and tested in isolation too (`cargo check --no-default-
  features --features std,ocpp_1_6 --lib` / `...,ocpp_2_0_1...`), not
  just alongside `ocpp_2_1`, so they don't secretly depend on it.
  Availability's `StatusNotifier` - the harder case flagged above, since
  1.6J addresses connectors with a single flat `connectorId: i64` (no
  EVSE concept at all) while this crate's internal model addresses every
  connector as an `(evse_id, connector_id)` pair - now has a real 1.6J
  adapter too: `ConnectorStatusChanged`/`StatusNotifier::notify_status`
  gained a `connector_state: ConnectorState` field/parameter alongside
  the existing coarse `status: ConnectorStatus`, giving version adapters
  access to the full internal state. A new
  `availability::Ocpp1_6StatusNotifier` wraps `OCPP1_6Client` together
  with the charge point's connector topology (each EVSE's connector
  count, captured once at construction - the topology `notify_status`'s
  per-call `(evse_id, connector_id)` alone can't supply) and uses a new
  `flatten_connector_id` helper (1-based, summing prior EVSEs' connector
  counts) to translate to 1.6J's numbering, plus a `map_status` that
  reads the richer `ConnectorState` directly (not the already-collapsed
  `ConnectorStatus`) to produce 1.6J's fuller status enum
  (`Preparing`/`Charging`/`Finishing`/`Unavailable`/`Faulted`/
  `Reserved`/`Available`, and - since B1.5 - `SuspendedEV`/`SuspendedEVSE`,
  which the connector state machine now distinguishes via
  `ConnectorState::SuspendedEv`/`SuspendedEvse`).

  The emission-cadence gap this first landed with is now closed too:
  `ChargePointState::apply_connector_event` fires `StatusNotification`
  on every actual `ConnectorState` transition (`transition.changed`) now,
  not only ones that cross a coarse `ConnectorStatus` boundary - so a
  session's `Locked` -> `Authorizing` -> `Starting` -> `Charging`
  progression (all `Occupied`) reports each step, letting 1.6J's adapter
  report `Preparing` -> `Charging` correctly instead of getting stuck on
  whichever `ConnectorState` happened to be current the one time
  `Occupied` was first entered. Two tests encoding the old, coarser
  cadence as a guarantee needed rewriting to match
  (`state::charge_point_state::tests` had one asserting *no*
  notification for `Connected` -> `Locked`; `actor::tests` had exact
  effect-vector assertions missing the now-additional notifications for
  `Locked` -> `Authorizing` and `Authorizing` -> `Starting`) - both now
  assert the richer, correct behavior instead of just being made to pass.
  That wire-traffic tradeoff is now closed too, without touching
  `OCPP2_1Client`/`OCPP2_0_1Client` or `setup()`'s signature: a new
  `availability::DedupedStatusNotifier<N>` wraps *any* `StatusNotifier`
  (protocol-agnostic - it has no idea `N` might be an OCPP client at
  all) and only forwards a call when `status` actually differs from the
  last one seen for that connector (or is the first one ever seen for
  it), using a small `BTreeMap<(usize, usize), ConnectorStatus>` cache
  behind the same `embassy-sync` `CriticalSectionRawMutex`-backed
  blocking mutex `src/sync.rs`'s own primitives are built on (so it
  stays no_std-safe without pulling in `std`/`tokio`). `setup()` wraps
  `csms` in this specifically for its `StatusNotifier` use
  (`DedupedStatusNotifier::new(csms.clone())`) - every other trait call
  still goes straight to `csms` unwrapped - so 2.1/2.0.1 deployments
  going through `setup()` keep exactly the wire cadence they had before
  `ChargePointState` started reporting every transition, with no changes
  needed to either client type or to what `setup()` requires callers to
  pass. `Ocpp1_6StatusNotifier` is deliberately *not* wrapped in this
  anywhere - it needs every call, including ones where `status` repeats
  but `connector_state` doesn't, which is the entire reason the
  emission-cadence fix happened. 4 new tests cover: first-call-always-
  forwards, a repeated status is suppressed, a genuinely different
  status forwards again, and connectors are deduped independently of
  each other.

  Transactions' `TransactionNotifier` now has a 1.6J adapter too - the
  hardest case yet, for a reason neither `BootNotifier` nor
  `StatusNotifier` ran into: **the CSMS assigns the transaction id, not
  the charge point.** `StartTransaction.conf` returns a CSMS-picked
  `transactionId` that every later `MeterValues`/`StopTransaction` for
  that session must use instead of this crate's own `TransactionId` -
  the opposite of every other identifier in this crate (and of 2.x's own
  `TransactionEvent`, where the charge point mints the id). Closing that
  gap needed real state, not just a mapping function:
  `transactions::Ocpp1_6TransactionNotifier` wraps `OCPP1_6Client` with a
  `TransactionId -> i64` cache (same `embassy-sync`-backed blocking
  mutex as `DedupedStatusNotifier`'s), populated when `StartTransaction`
  returns and consulted - then removed - on `Ended`. It also reuses
  `Ocpp1_6StatusNotifier`'s connector-topology wrapping for
  `StartTransaction`/`MeterValues`'s flat `connectorId`; the shared
  logic (`flatten_ocpp_1_6_connector_id`) moved out of `availability.rs`
  into a new `src/topology.rs` so both adapters call the same function
  instead of carrying their own copies - the first time this crate has
  shared logic across two version-adapter modules rather than letting
  each carry its own small duplicate, since a topology bug in one place
  would need finding and fixing in both otherwise.

  Building `StartTransaction.req` exposed a real, pre-existing gap:
  1.6J needs `idTag` (who's charging) and `meterStart` (the energy
  register reading at session start), and `Transaction` had neither.
  `idTag` got a real fix, not a placeholder: `Transaction` gained an
  `id_token: Option<IdToken>` field, threaded through both ways a
  transaction can start - `ConnectorEvent::ChargingAuthorized`/
  `RemoteStartRequested` now carry the `IdToken` (previously unit
  variants), recorded by `state::charge_point_state::advance_transaction`
  when it builds the new `Transaction`. This is a real, if scoped,
  change to the protocol-version-independent core, not just another
  adapter - and since `IdToken` isn't `Copy` (it owns a `String`),
  `Transaction`/`TransactionEventOccurred` lost their `Copy` derive too,
  which rippled into `.clone()`/`.as_ref()` fixups across ~6 files
  wherever code relied on implicit copies (`cost.rs`, `remote_control.rs`,
  the state machine's own `advance_transaction`/`apply_meter_sample`,
  and test fixtures throughout). `meterStart` didn't get an equally real
  fix: this crate's `MeterSample` is only ever recorded once charging is
  already under way (see §10 below), so there's no reading captured *at*
  `Started` to report - `Ocpp1_6TransactionNotifier` falls back to `0`,
  documented as a known limitation rather than silently wrong. One more
  real fix along the way: 1.6J's `IdTag` caps identifiers at 20 bytes,
  tighter than 2.x's 255-byte `IdTokenType.idToken` - a legitimately
  longer identifier is truncated to fit rather than dropping the whole
  `StartTransaction`/`StopTransaction`.

  2.0.1's coverage is now essentially finished, closing out the "every
  adapter targets `OCPP2_1Client` only" gap this bullet opened with:
  `OCPP2_0_1Client` now implements `ReconnectHandler`, `Authorizer`,
  `StatusNotifier`, `ChangeAvailabilityHandler`, `TransactionNotifier`,
  `UnlockConnectorHandler`, `RequestStartTransactionHandler`,
  `RequestStopTransactionHandler`, `ReserveNowHandler`,
  `CancelReservationHandler`, `SendLocalListHandler`,
  `GetLocalListVersionHandler`, `CostUpdatedHandler`,
  `DataTransferSender`, and `DataTransferRegistrar` - every one of
  `setup()`'s required traits except `SecurityEventNotifier` (verified
  directly, not assumed: a scratch `assert_bound::<OCPP2_0_1Client>()`
  against `setup()`'s exact trait bound, minus `SecurityEventNotifier`,
  compiled clean). Almost every block's 2.0.1 wire shape turned out
  identical or near-identical to 2.1's - the real, version-specific work
  concentrated in one place: **2.0.1's `IdToken.type` is a closed
  8-value enum (`IdTokenEnum`), not 2.1's free-form string**, hit first
  in `authorization::ocpp_2_0_1::map_id_token_kind` (falling back to
  `Central` for `DirectPayment`/`EVCCID`/`Vin`, which 2.0.1 has no
  variant for at all - the same real, honest gap 1.6J's status/id-tag
  adapters already had to make similar calls for), then reused - not
  re-derived - by `remote_control::ocpp_2_0_1` (the reverse direction:
  wire enum back to internal kind, for `RequestStartTransaction`'s
  inbound token) and by `reservation::ocpp_2_0_1`/
  `local_authorization_list::ocpp_2_0_1` (both import
  `remote_control::ocpp_2_0_1::map_id_token_kind` directly rather than
  each carrying a fourth copy - the first time this crate's version
  adapters share a *mapping function*, not just a topology helper like
  `flatten_ocpp_1_6_connector_id`). `security.rs`'s `wire_type` also
  moved to module-level scope, ready for a 2.0.1 adapter to reuse the
  moment one becomes possible, instead of staying buried inside
  `ocpp_2_1` where a future 2.0.1 module couldn't reach it.

  **`SecurityEventNotifier` is the one block that could not be ported,
  and it's a real upstream wall, not a gap this crate left unclosed**:
  `ocpp-client` 0.2.0 simply does not implement `SecurityEventNotification`
  for OCPP 2.0.1 at all - verified directly, not assumed, by grepping its
  complete `ocpp_2_0_1::actions` list (66 actions, covering every other
  message this crate needs, from `BootNotification` through
  `TransactionEvent` to `SendLocalList` - just not this one). There is no
  `Action` type or `send_*`/`on_*` method to call, so there is nothing
  for an adapter in this crate to wrap; per `CLAUDE.md`'s "delegate
  wire-protocol concerns to `ocpp-client`," this has to be fixed
  upstream, not worked around here. Until then, `setup()` (and
  `connect_and_setup`'s `UnsupportedNegotiatedVersion` handling for a
  2.0.1-negotiated connection) still can't accept a bare
  `OCPP2_0_1Client` - it's one trait short. See `docs/ROADMAP.md` §1.

  1.6J's coverage is now essentially finished too: `OCPP1_6Client` (or a
  topology-aware wrapper around it, for the handlers whose request
  addresses a connector) implements `Authorizer`, `UnlockConnectorHandler`,
  `ChangeAvailabilityHandler`, `RequestStartTransactionHandler`,
  `RequestStopTransactionHandler`, `ReserveNowHandler`,
  `CancelReservationHandler`, `SendLocalListHandler`,
  `GetLocalListVersionHandler`, `DataTransferSender`, and
  `DataTransferRegistrar` - every block except `SecurityEventNotifier`/
  `CostUpdatedHandler`, which don't apply to 1.6J at all (no such messages
  exist pre-2.x; not a gap, a real spec boundary). 1.6J's defining
  difference from 2.x runs through this whole batch: **it has no EVSE
  concept and addresses connectors with a single flat `connectorId: i64`**,
  so every handler whose 2.x counterpart takes an `evseId` needs
  [`crate::topology::unflatten_ocpp_1_6_connector_id`] (the wire-to-internal
  reverse of the `flatten_ocpp_1_6_connector_id` the outbound adapters
  already had) to resolve one, wrapped in a small per-block struct
  (`crate::remote_control::Ocpp1_6RemoteControlHandler`,
  `crate::availability::Ocpp1_6ChangeAvailabilityHandler`,
  `crate::reservation::Ocpp1_6ReserveNowHandler`) that captures
  `connector_counts` the same way the outbound `Ocpp1_6StatusNotifier`/
  `Ocpp1_6TransactionNotifier` already did - handlers that need no
  topology at all (`RequestStopTransactionHandler`,
  `SendLocalListHandler`/`GetLocalListVersionHandler`,
  `DataTransferSender`/`DataTransferRegistrar`) are implemented directly
  on `OCPP1_6Client` instead, with no wrapper.

  A few of 1.6J's request shapes need *more* precision than the internal
  model's handlers accept, not less: `RemoteStartTransactionRequest`'s
  optional `connectorId` and `ReserveNowRequest`'s mandatory one (`0`
  meaning "the Charge Point may choose") both address one specific flat
  connector, but `handle_request_start_transaction`/`handle_reserve_now`
  only target at EVSE granularity (picking the first matching connector on
  it themselves) - the same granularity every 2.x adapter's `evseId`
  already works at. Rather than widen those internal functions for one
  version, the resolved `connectorId` is unflattened down to its EVSE half
  and the specific connector within it is dropped, documented at the call
  site as a deliberate reduction, not an oversight.

  1.6J's `IdTag`/`AuthorizeRequest`/`local_authorization_list` items carry
  no type/kind metadata at all (unlike every later version's
  `IdTokenType`), so a new [`crate::id_tag::map_id_token`] (the reverse of
  the `map_id_tag` the outbound 1.6J adapters already shared) fills in
  `IdTokenKind::Central` for every inbound identifier this block receives
  - the closest fit for "an identifier the CSMS itself is presenting,"
  reused by `remote_control::ocpp_1_6`, `reservation::ocpp_1_6`, and
  `local_authorization_list::ocpp_1_6` rather than triplicated. One
  genuine version-shape win surfaced in `data_transfer.rs`: 1.6J's
  `DataTransferRequest`/`DataTransferResponse.data` is a real
  `Option<String>`, not the `Option<()>` every 2.x binding collapsed to
  (see that module's top-level docs) - so the 1.6J adapter is the only one
  of the three that actually carries a payload across the wire instead of
  silently dropping it.
- ✅ Version negotiation / connection lifecycle (connecting, reconnecting,
  offline message queueing, backoff). Reconnecting-with-backoff turned out
  to already exist entirely inside `ocpp-client` 0.2 (verified directly
  against the pinned dependency, not assumed): `connect_1_6`/
  `connect_2_0_1`/`connect_2_1` build a `Client` with automatic reconnect
  (`ConnectOptions::reconnect`, `ReconnectPolicy` exponential backoff)
  enabled *by default* - this crate's own `connect_and_setup` was already
  forwarding `ConnectOptions` straight through, so that part was working
  unnoticed. What `ocpp-client` explicitly leaves to the caller (its own
  `Client::on_reconnect` docs say so directly: "this crate does not
  re-run BootNotification or replay any state on its own") is
  resynchronizing application state after a reconnect - a real gap, since
  nothing in this crate was calling `on_reconnect` at all. Closed via a
  new `src/connection.rs`: a protocol-agnostic `ReconnectHandler` trait
  (`register_reconnect_handler`, wrapping `Client::on_reconnect`,
  implemented for `OCPP2_1Client`) and `reregister_on_reconnect`, which
  re-runs BootNotification-until-accepted (`provisioning::
  register_until_accepted`, now a free function taking `&ChargePointActor`
  instead of a `ChargePointRuntime` method, so both `ChargePointRuntime`
  and this new caller share the same implementation) every time the
  connection comes back after dropping - never on the initial connect,
  which already gets its own registration in `setup()`. Wired into
  `setup()` automatically for every csms type meeting the new
  `ReconnectHandler` bound.

  Version negotiation is now wired up too: `connect_and_setup` dials via
  `ocpp_client::connect` (not the hardcoded `connect_2_1` it used
  before), offering a caller-supplied `versions: Option<&[OcppVersion]>`
  - or every version compiled into the build, if `None` - and letting
  the CSMS pick one over the WebSocket subprotocol handshake, per RFC
  6455. This is real negotiation, not a stub: the handshake genuinely
  offers whichever versions are asked for and the CSMS genuinely
  chooses. What it can't do yet is anything useful with a 1.6J/2.0.1
  outcome - `setup()` requires a client implementing every functional
  block's trait, and today only `OCPP2_1Client` does (see the
  "protocol-version-independent core" item above for the running
  tally). Rather than pretend otherwise, `connect_and_setup` now returns
  a new `ConnectAndSetupError::UnsupportedNegotiatedVersion(OcppVersion)`
  when the CSMS picks 1.6J or 2.0.1 - a real, valid handshake outcome
  this crate simply can't run a session in yet, surfaced as an explicit
  error instead of a confusing later failure or a silent wrong-version
  attempt. Callers who only want 2.1 (this function's behavior before
  version negotiation existed) pass `Some(&[OcppVersion::V2_1])`. A new
  integration test (`tests/connect_2_1_websocket.rs`, alongside the
  existing happy-path one) drives a real WebSocket handshake where the
  mock CSMS negotiates 1.6J and asserts `connect_and_setup` returns that
  error rather than hanging or panicking - the two together are the
  first end-to-end proof that negotiation actually round-trips over a
  real connection, not just that the types compile.

  Offline message queueing is now closed too. `ocpp-client`'s
  `Client::call`/`send_notification` still write straight to whatever
  transport is currently installed and fail immediately if it's down -
  that's `ocpp-client`'s behavior to own, not this crate's to duplicate
  (per `CLAUDE.md`'s "delegate networking to `ocpp-client`" guidance) -
  so the fix lives at this crate's own layer instead: a new
  `src/offline_queue.rs` with `OfflineQueue<M>` (a small FIFO queue,
  generic over the message type, built on the same `embassy-sync`
  blocking mutex `DedupedStatusNotifier`'s cache uses) and two functions
  - `run_with_offline_queue` (a drop-in replacement for the existing
  `run_status_notifications`-style forwarding loops, queuing a failed
  send instead of just logging it) and `flush_offline_queue` (drains the
  backlog, in order, stopping at the first still-failing message rather
  than skipping ahead and misordering e.g. `TransactionEvent`s the CSMS
  relies on arriving in sequence). `setup()` now wires all three of
  Status/Transaction/Security through an `OfflineQueue`: a failed report
  is retried both the next time a new report of that kind comes in *and*
  - via a `register_reconnect_handler` callback registered alongside the
  forwarder - the moment the connection itself reconnects, so a queued
  report doesn't sit waiting for an unrelated event to trigger a retry.
  `DedupedStatusNotifier` is wrapped in `Arc` so the live forwarder and
  the reconnect-flush closure share the exact same dedup cache instead
  of each getting its own (which would have let a status re-forward
  after reconnect even though the live path had already deduped it
  away). The original `run_status_notifications`/`run_transaction_events`/
  `run_security_events` functions are unchanged and still exported -
  `setup()` just doesn't use them anymore - for callers who genuinely
  want the simpler fire-and-forget behavior. 3 new tests in
  `offline_queue.rs` cover: a successful send is delivered without ever
  being queued; a failed send is queued and both it and a later message
  get delivered, in order, once sending starts succeeding again; and a
  flush stops at the first failure rather than skipping ahead to a later
  message that might currently succeed.
- ✅ Erratic-hardware fault containment generalized beyond connectors.
  `Faulted`/`FaultedSafe` already existed per-connector; `EvseEvent::
  FaultDetected`/`FaultCleared` and top-level `ChargePointEvent::
  HardwareFault`/`FaultCleared` existed in the event model too, but
  previously only flipped `EvseStatus`/`LifecycleState` without touching
  the connectors underneath - a hardware fault reported at EVSE or
  charge-point scope (e.g. a shared meter stall, a contactor bank fault)
  didn't actually open any contactors, which violated `CLAUDE.md`'s
  fail-safe-over-fail-open guidance. `ChargePointState::apply` now
  cascades: an EVSE fault forces every connector it owns through the same
  `ConnectorEvent::FaultDetected` path a direct connector fault takes
  (`Faulted` + `OpenContactor` + ending any active transaction), reusing
  the extracted `apply_connector_event` helper so the effects are
  identical either way; a charge-point-wide `HardwareFault` cascades that
  to every EVSE in turn. Recovery cascades symmetrically but stays
  fail-safe: `FaultCleared` only advances a connector past `FaultedSafe`
  once its contactor has actually confirmed open, so connectors that
  haven't yet reported `ContactorOpened` are correctly left `Faulted`
  even after the fault is cleared at their EVSE/charge point. No new
  hardware-facing API was needed - integrators already push these events
  via `HardwareEventSender::send` (any `ChargePointEvent`), just nothing
  drove them from EVSE/charge-point scope with a real effect before now.
  Fault injection tests cover: an EVSE fault opening every connector's
  contactor and ending in-flight transactions; a partial-recovery case
  where only the connector that confirmed its contactor open unlocks; and
  the charge-point-wide fault/clear cascade across multiple EVSEs. Meter
  stalls, contactor stick, and sensor glitches themselves still have no
  dedicated detection logic (that's a hardware-integration concern, not a
  state-machine one) - this closes the "faults don't propagate" gap, not
  "we detect every possible fault".
- ✅ Rustdoc coverage pass on all public APIs (see `CLAUDE.md` documentation
  standard). `lib.rs` now carries `#![warn(missing_docs)]`, and every item it
  flagged (219 warnings, across ~30 files - every public trait/method,
  enum/variant, and struct/field reachable from the crate root) got a real
  doc comment explaining behaviour, not just a signature restatement:
  what an enum variant means, what a trait method does and when it's
  called, what a struct field represents and how it's indexed. Two
  crate-level gaps also got closed: a top-level `//!` crate doc on
  `lib.rs`, and `//!`-less re-exports (`actor`, `state`) got a one-line
  `///` at their `pub mod` declaration instead. `missing_docs` stays a
  `warn`, not a `deny` - `cargo build`/`clippy` won't fail CI on it, but
  any new public item without docs will show up as a warning going
  forward. Verified via `RUSTFLAGS="-W missing_docs" cargo build
  --all-features --lib` (0 warnings, down from 219), `cargo doc
  --all-features --no-deps` (clean), and the full test suite plus both
  no_std configurations (`--no-default-features --lib` and
  `--no-default-features --features std,ocpp_1_6,ocpp_2_0_1,ocpp_2_1
  --lib`) unaffected, since this was a docs-only change with no logic
  touched.
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
- **Security profiles** are modelled in `crate::security_profile`, and the rule
  that makes the model worth having is enforced: a profile may be raised over
  OCPP but essentially never lowered (§A05). Dropping to profile 1 is refused
  outright, 3 → 2 only with `AllowSecurityProfileDowngrade`. That check exists
  because the CSMS connection is the channel an attacker would use to weaken the
  station - a `SetNetworkProfile` that could move a TLS charge point onto
  plaintext turns one compromised credential into a fleet on cleartext.
  Credentials come with the rules OCPP states: an identity containing `:` is
  refused (Basic auth would split the pair in the wrong place) and a password
  must be 16-64 characters, counted in characters rather than bytes. Profiles 1
  and 2 are runnable today; profile 3 needs a client certificate and a key store,
  and `SecurityProfile::is_implemented` says so rather than letting a station
  behave as though it presents a certificate it has not got.
- Status: 🚧 partial — `SecurityEventNotification` (outbound only) is
  implemented on **both 2.1 and 2.0.1**; the certificate messages (`SignCertificate`,
  `CertificateSigned`, `Get15118EVCertificate`, `GetCertificateStatus`,
  `DeleteCertificate`, `InstallCertificate`, `GetInstalledCertificateIds`)
  are not. A **certificate store** now exists (`hardware::CertificateStore`,
  B4.1) - a trait the integrator implements, so it can sit behind a secure
  element, with `StoredCertificates` over `hardware::Storage` for hardware
  without one. The crate never sees a private key: the only question it asks is
  whether one exists, because security profile 3 needs to know whether a client
  certificate can be presented and nothing above needs the key. What is still
  missing is the crypto several of them need
  (parsing X.509, computing hashes, signing) - which is why the store takes hash
  data from whoever parsed the certificate rather than deriving it.
  `InstallCertificate`, `DeleteCertificate` and `GetInstalledCertificateIds` are
  wired on 2.0.1 and 2.1 (B4.2); `SignCertificate`/`CertificateSigned`,
  `GetCertificateStatus` and `Get15118EVCertificate` are not, and need the
  signing and OCSP work B4.3/B4.4 cover. 1.6J has none of them - they are
  Security Whitepaper messages, which `ocpp-types` does not generate. A
  `SecurityEvent` (`event_type: SecurityEventType`, `tech_info: Option<
  String>`) is reported via a new `src/security.rs` module, wired the same
  way as every other outbound report: a protocol-agnostic
  `SecurityEventNotifier` trait, implemented for `ocpp-client`'s OCPP 2.1 and
  2.0.1 clients, forwarded by `run_security_events` (spawned from `setup()`) over a
  new dedicated broadcast channel on the actor
  (`ChargePointActor::subscribe_security_events`). `SecurityEventType`
  covers OCPP's standardized "Security events" list (all 21 values, e.g.
  `TamperDetectionActivated`, `InvalidCsmsCertificate`,
  `MemoryExhaustion`) plus `Other(String)` for vendor-specific/uncovered
  ones - mirroring `StopReason`'s "subset of the full spec enum" pattern.

  **Criticality decides where an event goes**, per OCPP A04:
  `SecurityEventType::is_critical` (transcribed from the spec appendix's own
  `Critical` column) gates entry to the CSMS notification queue, while the log
  keeps everything. That is a security property, not tidiness - the queue is
  bounded and evicts its oldest entry, and `InvalidMessages` /
  `AttemptedReplayAttacks` are precisely what a remote party can generate at
  will, so sharing the queue let a flood push a queued
  `TamperDetectionActivated` out before the CSMS saw it.

  Six events are now raised by this crate itself: `StartupOfTheDevice`,
  `ResetOrReboot`, `SettingSystemTime`, `MemoryExhaustion`,
  `SecurityLogWasCleared` and `ReconfigurationOfSecurityParameters` (on an
  accepted `SetNetworkProfile`). The rest still cannot be: there's no
  certificate handling (the rest of this block), no firmware update flow (§12),
  and no TLS-layer visibility (that lives in `ocpp-client`, not here); the
  tamper and maintenance-login events are the integrator's to raise, since only
  the hardware knows a case was opened or who logged in. **1.6J reports none of
  them** - `SecurityEventNotification` is a Security Whitepaper message, not a
  core 1.6J one, and `ocpp-types` does not generate that message set. A durable, size-bounded **security log** now sits alongside
  the reporting pipeline (`security::SecurityEventLog` plus
  `persistence::SecurityLogStore`, wired via
  `ChargePointBuilder::security_log_persisted`): every raised event is recorded
  and written through to storage whether or not the CSMS ever accepts it, and
  restored at the next boot, which is what makes `SecurityLogWasCleared`
  (raised by `persistence::clear_security_log`) mean anything. The `GetLog`
  upload that would read it is still missing (§14).
  `security::report_security_event(actor, event)`
  is the one public entry point - callable by hardware (e.g. a tamper
  switch, the same way `MeterValueSampled` is pushed in) or by future
  functional blocks once they exist, but nothing calls it today. Version
  notes below on the certificate messages still apply for the eventual rest
  of this block. **2.0.1 is blocked upstream, not by this crate**: unlike
  every other block ported to 2.0.1 in §0's "protocol-version-independent
  core" writeup, `SecurityEventNotifier` has no `OCPP2_0_1Client` impl,
  because `ocpp-client` 0.2.0 doesn't implement the `SecurityEventNotification`
  action for OCPP 2.0.1 at all (verified directly against its
  `ocpp_2_0_1::actions` list, not assumed) - there's no `Action`
  type/`send_*`/`on_*` method to wrap. Fixing this needs an `ocpp-client`
  release, not a change here.
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
  `BootNotifier`/`HeartbeatSender`.

  The Component/Variable device model now exists. `state::DeviceModel` (a
  `BTreeMap<Component, BTreeMap<Variable, VariableDefinition>>`, so iteration
  order is stable - `GetBaseReport` depends on that) is owned by
  `ChargePointState` and mutated only through
  `ChargePointEvent::DeviceModel(DeviceModelEvent)`, like every other piece of
  state here; nothing hands out a mutable handle to it. `Component` addresses
  EVSEs with this crate's own `Option<(usize, Option<usize>)>`, never OCPP's
  wire `EVSE` type, so the model stays protocol-version-independent by
  construction. A hardware binding extends it by pushing
  `DeviceModelEvent::VariableRegistered` through the same
  `HardwareEventSender` it already uses for everything else - no new
  integration surface. `VariableAttribute` carries one field OCPP's wire
  attribute doesn't have, `requires_reboot`, so `SetVariableStatusEnum`'s
  `RebootRequired` has something concrete to key off instead of being
  permanently unreachable. The built-in default set covers OCPP's own
  standardized variables - 45 always-on plus 11 that arrive with their
  capability, between them every *required* row of the 2.1 appendix belonging
  to a component this crate implements (B1.7), and every *required* 1.6J
  configuration key (B1.6). Integrators register what their hardware exposes on
  top of that. Required rows for blocks this crate does not have
  (`PaymentCtrlr`, the DER controllers, ISO 15118, network configuration) are
  deliberately absent: their capability is `false`, the component reports
  `Available: false`, and a charge point that cannot run a block owes no
  configuration for it. Per-EVSE and per-connector required rows
  (`EVSE.Available`, `Connector.ConnectorType`, `SupplyPhases`) are still
  missing, and want a derived-variable path rather than a stored copy that
  would go stale.

  `device_model::handle_get_variables`/`handle_set_variables` resolve each
  item of a batch independently (a batch never fails outright), with adapters
  for 2.1 and 2.0.1 - whose `GetVariables`/`SetVariables` wire shapes turned
  out identical apart from `SetVariableData.attributeValue`'s bound (2500 vs
  1000 bytes), which doesn't matter here since this crate only reads it.
  1.6J has no device model at all, so its adapter projects onto flat
  `GetConfiguration`/`ChangeConfiguration`. That projection needs a
  key-naming convention, and the first cut - a dotted `Component.Variable`
  form - was *not* interoperable: a real 1.6J CSMS asks for
  `HeartbeatInterval`, not `OCPPCommCtrlr.HeartbeatInterval`, so every
  standard key came back in `unknownKey` and every `ChangeConfiguration` on
  one was rejected. There is now a `STANDARD_KEY_ALIASES` table consulted in
  both directions, covering 12 standard 1.6J keys taken from
  `docs/OCPP-2.0.1/Appendices_CSV_v1.5/dm_components_vars.csv` (row numbers
  cited per entry in the source) - e.g. `HeartbeatInterval` ->
  `OCPPCommCtrlr`/`HeartbeatInterval`, `MeterValueSampleInterval` ->
  `SampledDataCtrlr`/`TxUpdatedInterval`, `AuthorizeRemoteTxRequests` ->
  `AuthCtrlr`/`AuthorizeRemoteStart`. `encode_key` emits the real 1.6J name
  when an alias exists, so an unfiltered `GetConfiguration` returns names a
  1.6J CSMS recognises; anything unmapped still degrades to the dotted form
  rather than breaking. The table now covers every **required** 1.6J key
  (Core, plus the optional profiles this crate implements) - 23 aliased onto
  real device model variables that `DeviceModel::register_defaults` registers,
  plus 10 answered from live state (topology, `StateLimits`, `Capabilities`, or
  a documented advisory figure where this crate imposes no limit at all). Which
  of the writable ones actually *take effect* is recorded in
  `DEFAULT_VARIABLES`' docs rather than left to be discovered: five are live,
  the rest are stored and reported faithfully but not yet consulted. `ConnectorPhaseRotation` is
  explicitly excluded, not silently mismapped: 1.6 packs a per-connector list
  into a single key while 2.0.1 models `PhaseRotation` per connector, and
  that fan-out doesn't fit a static `key -> (Component, Variable)` entry.

  `HeartbeatInterval` is a live variable, not a decorative one. The accepted
  BootNotification interval is written into it after registration, and
  `run_heartbeat` re-reads it from `actor.state()` on every cycle, so a CSMS
  changing it via `SetVariables` (or 1.6J `ChangeConfiguration`) takes effect
  without a reboot. A missing, unparseable, or zero value falls back to the
  boot-notification interval - specifically so a bad value can't turn the
  loop into a busy-spin. Without this the whole model would have been
  cosmetic: a CSMS would have gotten `Accepted` and seen nothing change,
  which is worse than a rejection.

  Device-model *reporting* is in `reporting.rs`: `GetBaseReport`, `GetReport`
  and the resulting multi-part `NotifyReport`s, for 2.1 and 2.0.1.
  `chunk_report` is pure and wire-type-free, splitting a report into
  `ReportChunk { seq_no, tbc, entries }` (16 entries per chunk, matching
  `ocpp-types`' own non-`alloc` `NotifyReportRequest` default `heapless::Vec`
  capacity) so `seqNo`/`tbc` correctness is unit-testable without touching
  OCPP types. `ComponentCriterion` matching follows the spec's B08
  requirements table, including its asymmetry: a missing
  `Active`/`Available`/`Enabled` variable still counts as a match, a missing
  `Problem` variable does not. `EmptyResultSet` is returned - and no
  `NotifyReport` sent - when a `GetReport` filter matches nothing. Two
  documented judgement calls: `SummaryInventory` is approximated by a fixed
  list of well-known status variable names, since this crate's device model
  has no general "abnormal state" concept to derive it from; and a `GetReport`
  carrying both criteria and an explicit component/variable list is genuinely
  ambiguous in the spec text, resolved here as a union. 1.6J gets no adapter
  at all - it has no structured device model or report mechanism, and its flat
  `GetConfiguration` already covers the same ground.

  `Reset` is implemented for all three versions. `ResetKind`
  (`Immediate`/`OnIdle`) and `ResetTarget` (whole charge point, or one EVSE)
  are protocol-agnostic; a single `ChargePointState.pending_reset` tracks a
  deferred one, re-evaluated at the end of every `apply()` so it can't be
  silently lost, with a later request superseding a pending one rather than
  queueing. An `Immediate` reset does *not* route through
  `Faulted`/`FaultedSafe` - that would misreport a scheduled reboot as a
  hardware fault to the CSMS. It instead drives any cable-engaged connector
  into the existing `Stopping` (open contactor) -> `Finishing` (unlock)
  pipeline a normal stop already uses, satisfying `CLAUDE.md`'s fail-safe
  ordering by reusing that path rather than building a parallel one. A new
  `StopReason::Reset` keeps the resulting `TransactionEvent(Ended)` from
  misreporting `EmergencyStop`. Reboot reaches hardware as
  `HardwareCommand::Reboot { evse_id }` through the existing
  `execute_hardware_command` dispatch loop, backed by a new required
  `Evse::reboot()` (plus an `Evse::Error` associated type) - **a breaking
  change to the hardware trait**, chosen over a `ChargePoint`-level hook
  because `execute_hardware_command` only ever receives `&[E]`, so a
  charge-point-level reboot would have needed new plumbing through
  `ChargePoint::start`'s contract; a charge-point-wide reset simply expands to
  one `Reboot` per EVSE. A failed reboot surfaces as `EvseEvent::FaultDetected`,
  never a panic. 1.6J's projection is the lossy one: it has no EVSE targeting
  (always charge-point-wide) and no `Scheduled` status (collapses to
  `Accepted`), and its `Hard`/`Soft` axis isn't literally
  `Immediate`/`OnIdle` - neither 1.6J type waits for the station to go idle
  on its own. `Hard` -> `Immediate` / `Soft` -> `OnIdle` is documented in
  `reset.rs` as the closest match on the axis `ResetKind` actually models
  (urgent-and-abrupt vs. graceful-and-deferred), not as an exact equivalence.
  2.1's `ImmediateAndResume` projects down to `Immediate`, since nothing here
  models resuming a transaction across a reboot.

  `SetNetworkProfile` is wired for 2.0.1 and 2.1 (`crate::network_profile`,
  B1.8): profiles are stored in bounded configuration slots, reported, and
  refused when this charge point could never use them (SOAP transport, a
  negative slot, a new slot past the bound). Storing one does **not** switch the
  live connection - that is A9, still open - and `basicAuthPassword` is dropped
  rather than kept, being a credential this crate cannot use. 1.6J has no such
  message. `NetworkConfigurationPriority` is live - a stored slot joins the
  order and a vanished one leaves it, without ever reordering what the operator
  set - `network_profile::selected_profile` answers which profile the CSMS wants
  in force, and `network_switch` moves the live connection onto it, rolling back
  to the last working address after `NetworkProfileConnectionAttempts` failures
  (A9). An integrator who built their own client has no transport this crate can
  re-point and reads the selection to drive their own redial. Variable
  monitoring (`SetVariableMonitoring`/`ClearVariableMonitoring`, reported via
  `NotifyEvent`) is now wired too - see §14 (B5.2) for the engine itself;
  `GetMonitoringReport`/`NotifyMonitoringReport` remain open (B5.3).
  Two items previously listed here are done: device-model persistence across
  restarts (E2.3 - `VariableAttribute::persistent` is acted on now), and the
  standard 1.6J configuration keys (B1.6 - every *required* key is readable
  except `ConnectorPhaseRotation`, which is excluded for the reason above).
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
  concurrently per token. An **authorization cache** now backs offline
  operation (`state::AuthorizationCache`, B1.2): every CSMS decision is
  remembered - rejections included, or a revoked card would get in every time
  the link drops - and an `Authorize` that fails at the transport falls back to
  the local authorization list first (pushed by the operator, so authoritative)
  and the cache second, with `AuthCtrlr`/`LocalAuthorizeOffline`,
  `AuthCacheCtrlr`/`Enabled` and `AuthCacheCtrlr`/`LifeTime` gating both. That
  is also the first thing that consults the local list at all (see §4).
  `ClearCache` is wired on all three versions. Still missing: group id tokens,
  `idTokenInfo`'s richer fields (e.g. `evseId`-scoped validity), The cache is durable
  (`persistence::AuthorizationCacheStore`, wired via
  `ChargePointBuilder::authorization_cache_persistence`), so a charge point that
  reboots while its CSMS is unreachable still recognises the cards it knew;
  entry expiry stays a lookup-time question rather than a boot-time filter,
  since `AuthCacheCtrlr`/`LifeTime` is itself not persistent.
- Version notes: 1.6J's `Authorize.req`/`.conf` maps closely; 2.1 adds
  richer `IdTokenInfo` (groups, restrictions) that must downgrade to
  1.6J's flatter `idTagInfo`.

## 4. Local authorization list management

Offline authorization without a CSMS round-trip.

- Messages: `SendLocalList`, `GetLocalListVersion`.
- Internal state needed: versioned local list storage, diffing
  (differential vs full updates).
- Status: 🚧 partial — a `LocalAuthorizationList` (`version`, `entries: Vec<
  LocalListEntry>`) lives on `ChargePointState`, alongside `registration` -
  charge-point-wide, not per-connector. Each `LocalListEntry` collapses OCPP's
  `IdTokenInfo` down to the same binary `AuthorizationStatus` the
  Authorization functional block already uses (§3), for the same reason:
  nothing downstream distinguishes richer decisions yet. Wired end-to-end via
  a new `src/local_authorization_list.rs` module: a protocol-agnostic
  `handle_send_local_list`/`handle_get_local_list_version`,
  `SendLocalListHandler`/`GetLocalListVersionHandler` traits, implemented for
  `ocpp-client`'s OCPP 2.1 client (`Client::on_send_local_list`/
  `on_get_local_list_version`), wired in from `setup()`. `GetLocalListVersion`
  needs no actor round trip - `handle_get_local_list_version` just reads
  `actor.state()` directly, unlike every other handler in this crate.
  `SendLocalList`'s `updateType` is resolved to one of two internal shapes
  before it reaches the state machine: `Full` (replaces the list outright,
  adopting the request's `versionNumber` unconditionally) or `Differential`
  (a list of per-entry `Upsert`/`Remove` changes, computed from whether each
  wire `AuthorizationData` carries an `idTokenInfo` or not). A `Differential`
  update is only applied if its `versionNumber` is exactly the charge point's
  current version + 1 - anything else means an earlier update was missed (or
  the CSMS is out of sync) and reports `VersionMismatch` rather than risking
  applying changes on top of an unknown base; `SendLocalListStatusEnum`'s
  third value, `Failed`, isn't reachable, since the list is a plain in-memory
  `Vec` with nothing else that can fail. The list is now consulted: an `Authorize`
  that fails at the transport falls back to it before the authorization cache
  (§3, B1.2), gated on `AuthCtrlr`/`LocalAuthorizeOffline`. "The CSMS is
  unreachable" is still not tracked as *state* - the fallback triggers per
  failed request rather than from a connection-state machine - which is enough
  for offline authorization but not for anything that needs to know the link is
  down before it tries. Still missing: persisting the list is done (E2.4), but
  nothing pre-authorizes from it while online (`LocalPreAuthorize`).
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
  new way to reach the existing one. `Transaction` now records `id_token`
  too - both the physically-presented path (`ChargingAuthorized`) and the
  CSMS-initiated one (`RequestStartTransaction` → `RemoteStartRequested`)
  carry the `IdToken` through to `advance_transaction`, closing the gap
  §0's 1.6J `TransactionNotifier` adapter needed (`StartTransaction.req`'s
  `idTag`) - see §0 for the fuller writeup, including the `Copy` ripple
  that came with it. Still missing: running totals/energy (needs Meter
  values, §10, which is also why 1.6J's `StartTransaction.req.meterStart`
  falls back to `0` today), and multiple `Updated` events per transaction
  (today only the single Charging transition produces one).
- Version notes: this was the highest-value adapter target — 2.x's single
  `TransactionEvent` stream projects down to 1.6J's discrete Start/Stop/
  MeterValues calls via `transactions::Ocpp1_6TransactionNotifier` (see
  §0) - the one real wrinkle being that 1.6J's CSMS assigns the
  transaction id instead of the charge point, requiring the adapter to
  track that mapping itself.

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
  that doesn't parse as an unknown transaction. `TriggerMessage` is wired on
  all three versions (`remote_control::{TriggerableMessage,
  handle_trigger_message}` plus per-version adapters and
  `ChargePointBuilder::trigger_message`): `Heartbeat` and `StatusNotification`
  are fulfilled, every other `requestedMessage` is refused with
  `NotImplemented` rather than `Rejected`, and an address that cannot exist is
  rejected rather than widened to the whole charge point.

  **This paragraph's long-standing "blocked upstream on 2.1" claim was
  wrong, and is corrected here (D1.3).** It used to say `rust-ocpp`'s
  `v2_1` module never declares a `trigger_message` message module, so
  there were no `TriggerMessageRequest`/`TriggerMessageResponse` types to
  receive at all. Two things are wrong with that. First, `rust-ocpp` is
  not in this crate's dependency graph — the types come from
  `ocpp-types`, an independently generated crate, and
  `ocpp-types-0.1.2/src/v21/trigger_message_request.rs` **exists**
  (verified against the copy `Cargo.lock` actually pins; see
  `docs/UPSTREAM-GAPS.md`). Second, what was genuinely missing was only
  `ocpp-client`'s *action wrapper* — one macro line, not a type gap. That
  wrapper now exists (see `PRODUCTION-ROADMAP.md` D1), so 2.1 is no longer
  blocked either, pending a released `ocpp-client` this crate can depend
  on. 2.0.1 and 1.6J were never blocked: `OCPP2_0_1Client` and
  `OCPP1_6Client` have had real `on_trigger_message`/`send_trigger_message`
  methods all along.

  So all three versions are now buildable, and what's left is ordinary
  work in this crate, not an upstream wall. The `setup()` bound problem
  that also held this up — adding a `TriggerMessageHandler` bound ahead of
  a type that could satisfy it would break every caller of `setup()` — is
  itself resolved: `ChargePointBuilder` (`PRODUCTION-ROADMAP.md` C4) lets
  a block be registered with only its own bounds, so a `trigger_message`
  registration method can land without touching anyone else's `N`. The
  old reasoning for waiting, kept for the record: adding either now, ahead
  of something that could implement it, would just break every real caller
  of `setup()` (their `N` could never
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
- Version notes: status enum values differ between 1.6J and 2.0.1/2.1, and not
  in the direction the old note here assumed: **1.6J is the richer one** for
  suspension. It has `SuspendedEV`/`SuspendedEVSE` connector statuses, while
  2.x dropped them from connector status and moved the distinction onto the
  transaction's `chargingState`. This crate's `ConnectorState` carries the
  distinction (B1.5) and each adapter reports it where its own version expects
  it - 1.6J in the status, 2.x in the `TransactionEvent`.

## 8. Reservation

Reserving a connector/EVSE ahead of use.

- Messages: `ReserveNow`, `CancelReservation`, `ReservationStatusUpdate` (2.x).
- Internal state needed: reservation entity (id, expiry, id token,
  target EVSE/connector), a `Reserved` connector state, expiry timer.
- Status: 🚧 partial — `ConnectorState` gained a `Reserved` state, reachable
  only from `Available` (`ConnectorEvent::Reserved(Reservation)`) and left via
  `ConnectorEvent::ReservationCancelled`/`ReservationExpired` (back to
  `Available`) or
  `ConnectorEvent::CableConnected` (straight into the normal `Connected` flow,
  same as plugging into an `Available` connector - the reservation is cleared
  either way). `availability_status()` now actually reaches
  `ConnectorStatus::Reserved`, previously unreachable per §7's own notes. A
  first-class `Reservation` entity (`ReservationId`, `id_token`) exists,
  separate from `ConnectorState`, tracked per-connector on `EvseState`
  alongside `transactions` - mirroring how `Transaction` was added for §5.
  Wired end-to-end the same way as Availability/RemoteControl: a
  protocol-agnostic `reservation::handle_reserve_now`/
  `handle_cancel_reservation`, `ReserveNowHandler`/`CancelReservationHandler`
  traits, implemented for `ocpp-client`'s OCPP 2.1 client (registering via
  `Client::on_reserve_now`/`on_cancel_reservation`), wired in from `setup()`.
  Unlike `ChangeAvailability`/`RequestStartTransaction`, OCPP's `ReserveNow`
  has no `connectorId` addressing at all (`evseId` is optional, with no
  finer-grained field) - `handle_reserve_now` finds the first `Available`
  connector on the addressed EVSE (or, if unspecified, the first `Available`
  connector on any EVSE), matching `RequestStartTransaction`'s "first locked
  connector" pattern but for `Available` instead. When no connector is
  `Available`, the outcome reports the most informative wire status
  (`Faulted`/`Unavailable`/`Occupied`, in that priority order) rather than
  collapsing everything to a bare rejection. `CancelReservation` finds the
  connector by `ReservationId` and frees it.

  Expiry works: `reservation::run_reservation_expiry` sweeps on an
  `Executor`/`Backoff`-driven interval and releases a reservation past its
  `expiryDateTime`, skipping the sweep entirely while the clock is
  unsynchronized - a charge point that does not know the time cannot know a
  reservation lapsed, and holding a connector too long is recoverable where
  releasing a valid reservation is not. The CSMS is told over
  `ReservationStatusUpdate` (2.x; 1.6J has no such message), as it is when the
  charge point *removes* a reservation because the connector faulted or was
  made unavailable. A cancellation the CSMS sent, and a cable arriving to
  honour the reservation, are deliberately not reported.

  Still missing: 2.1's `NoTransaction` status (needs a timer from the moment
  the cable arrives, for a duration OCPP does not name),
  `groupIdToken` (not modeled, same gap as §3), `connectorType` filtering
  (hardware doesn't expose connector type yet), and matching the presented
  `id_token` against the reservation's on cable connection - the CSMS's own
  `Authorize` decision (§3) is still what accepts or rejects the token, same
  as an unreserved connector; the charge point doesn't locally enforce "only
  the reserving token may use this connector".
- Version notes: broadly compatible between 1.6J and 2.x.

## 9. Tariff and cost

Communicating price/cost to the driver.

- Messages: `NotifyPriceSchedule` (2.1), `CostUpdated`, running cost in
  `TransactionEvent`.
- Internal state needed: tariff model, running-cost accumulation hook.
- Status: 🚧 partial — `CostUpdated` (inbound only) is implemented:
  `EvseState` gained a `running_costs: Vec<Option<f64>>` side-table,
  indexed the same as `connectors`/`transactions` - a `CostUpdated`'s
  `totalCost` is recorded there via a new `ConnectorEvent::CostUpdated(f64)`,
  applied only while a transaction is actually active on that connector, and
  automatically cleared when that transaction starts or ends so a new
  session on the same connector never inherits a stale cost. Wired via a
  new `src/cost.rs` module: `handle_cost_updated` finds the connector
  running the addressed `transactionId` (mirroring
  `remote_control::handle_request_stop_transaction`'s `find_transaction`),
  and `CostUpdatedHandler`, implemented for `ocpp-client`'s OCPP 2.1 client,
  wired in from `setup()`. `CostUpdatedResponse` carries no status at all -
  the CSMS is informing the charge point, not asking permission - so there
  is no rejection to report; the internal `CostUpdateOutcome` exists purely
  for this module's own tests. Building this exposed a real, now-fixed bug:
  `ChargePointState`/`ChargePointEvent`/`EvseState`/`ConnectorEvent` all
  dropped their `Eq` derive (now `PartialEq` only) since `f64` isn't `Eq`;
  more importantly, a `CostUpdated` doesn't change `ConnectorState` itself,
  so `ChargePointEffect::StateChanged` wasn't being emitted for it and the
  actor's watch channel (what `ChargePointActor::state()` reads) never
  picked up the new cost - fixed by folding "did a cost actually get
  recorded" into the connector-event handler's `changed` result alongside
  the connector's own state transition. Still missing: `NotifyPriceSchedule`
  - **verified directly against the pinned `ocpp-client`/`ocpp-types`
  (not assumed)**, it doesn't exist there at all yet, the same upstream gap
  that blocks `TriggerMessage` (§6) - and outbound cost reporting via
  `TransactionEvent.cost_details`, which needs an actual tariff/pricing
  model (charging periods, dimensions) this crate has no reason to build
  without a consumer for it yet.
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
- Status: 🟨 in progress — `MeterSample` now carries the energy register plus
  four optional measurands (`power_w`, `current_ma`, `voltage_v`,
  `soc_percent`) - all `Option`, since a hardware integration that can't
  measure a given quantity simply omits it rather than fabricating a value.
  `transactions::ocpp_2_1::build_meter_values` emits one `sampledValue` per
  measurand actually present (`EnergyActiveImportRegister`,
  `PowerActiveImport`, `CurrentImport`, `Voltage`, `SoC`), still embedded in
  `TransactionEvent.meterValue` the same way energy-only sampling was
  wired. `current_ma` (milliamps) is the one field with a non-obvious unit -
  chosen over whole amps for enough resolution at typical EV charging
  currents; the wire adapter divides by 1000 before reporting. Standalone `MeterValues` now exists for all three versions
  (`crate::meter_values`), driven by the `AlignedDataCtrlr`/`Interval` device
  model variable (1.6J's `ClockAlignedDataInterval` aliases onto it) and
  aligned to the wall clock rather than to boot time, so readings taken between
  sessions - and by a charge point with nothing plugged in at all - reach the
  CSMS. `EvseState::latest_meter_samples` keeps the reading that makes that
  possible, recorded on every sample regardless of connector state. Sampled-data
  configuration (`SampledDataCtrlr`/`TxUpdatedInterval`) is still inert: this
  crate never polls hardware, so how often a reading arrives remains the
  binding's choice - throttling what reaches the CSMS to that interval is the
  outstanding half.

  Clock-aligned scheduling no longer needs an integrator-owned timer: the loop
  sleeps on the same caller-supplied `Backoff` the heartbeat uses, computing
  each wait from the next wall-clock boundary. (§8's reservation expiry still
  has no such driver.) Still missing: per-phase measurements
  (`SampledValue.phase` is always `None`), and the sampled-data throttling
  noted above.
- Version notes: measurand/unit enums are close to compatible across
  versions; sampling-context differs slightly. 2.x's `MeterValuesRequest`
  addresses an EVSE with no connector field at all, so a multi-connector EVSE's
  readings are indistinguishable on that wire - the spec's shape, not a
  simplification this crate made; 1.6J addresses a flat `connectorId` and so
  keeps the finer address.

## 11. Smart charging

Charging profiles and schedule negotiation.

- Messages: `SetChargingProfile`, `ClearChargingProfile`,
  `GetChargingProfiles`, `GetCompositeSchedule`, `NotifyChargingLimit`,
  `ReportChargingProfiles`.
- Internal state needed: charging profile store, schedule composition
  logic, external limit inputs (local/grid), a schedule → hardware current
  limit projection.
- Status: 🚧 partial — the load-management spine is complete on all three
  versions. `state::ChargingProfileStore` (bounded by
  `StateLimits::max_charging_profiles`, mutated only via
  `ChargePointEvent::ChargingProfileSet`/`ChargingProfilesCleared`) holds a
  version-independent profile model carrying the superset of what the three
  versions express: five purposes, up to three schedules per profile,
  absolute/recurring/relative kinds, validity windows.
  `smart_charging::compose` turns whatever applies to a connector into one
  composite curve - purpose precedence (`TxProfile` beats `TxDefaultProfile`
  whatever their stack levels), stack level within a purpose, then the
  installation limit and external constraints as caps, with a cap that has
  nothing to cap becoming the limit itself. It is pure, so all of that is
  tested without a clock, a CSMS or hardware.

  `smart_charging::run_charging_limit_projection`/`run_charging_limit_schedule`
  push the result at `hardware::Connector::set_current_limit` - one loop on
  state changes, one on schedule period boundaries, deduped by the state
  machine so only a genuine change reaches hardware. That hook's signature
  widened to `Option<u32>` here: a limit that stops applying must be
  *removed*, and only the hardware knows its own maximum (a suspend-charging
  0 A period stays `Some(0)` - the two must not be conflated).

  `SetChargingProfile`, `ClearChargingProfile` and `GetCompositeSchedule` are
  wired end-to-end for 1.6J, 2.0.1 and 2.1, each through a protocol-agnostic
  handler that decides the outcome against the real store before dispatching.
  1.6J needs its flat `connectorId` resolved through `crate::topology` to the
  owning EVSE (its profiles then scope per-EVSE, the same reduction the other
  1.6J handlers make); 2.0.1 has no `PriorityCharging` to report (it degrades
  to `TxProfile`); 2.1 permits a period with no limit at all, which is dropped
  rather than invented.

  Two things this block deliberately refuses to guess: amps↔watts conversion
  without caller-supplied `SupplyCharacteristics` (a wrong voltage/phase
  assumption over-limits by 5× - a safety question, not a billing one), and a
  `Relative` schedule's anchor when the transaction's start time isn't known.

  `GetChargingProfiles`/`ReportChargingProfiles` is wired on 2.0.1 and 2.1
  (1.6J has no such message): the CSMS asks what is installed and the answer
  comes from the store, chunked by scope and source across as many
  `ReportChargingProfiles` as it takes.

  2.1's **priority charging** is wired both ways: `UsePriorityCharging` inbound,
  `NotifyPriorityCharging` outbound for a grant the charge point made itself.
  The messages were the smaller half - the gate behind them was a real defect.
  A `PriorityCharging` profile used to apply the moment it was installed, to
  whatever transaction happened to be running, because composition treated the
  purpose exactly like `TxDefaultProfile`. It is a *grant*, not another stack
  level: `Transaction::priority_charging` now carries it, so a priority profile
  sits inert until the CSMS names a transaction, and the grant ends with that
  session rather than leaking into the next driver's.

  2.1's **dynamic charging profiles** (OCPP K28) are wired too, and they invert
  what a charging profile is: no curve laid out in advance, just one schedule of
  one period whose limit the CSMS replaces as it goes - pushed with
  `UpdateDynamicSchedule`, or pulled with `PullDynamicScheduleUpdate` when the
  profile's own `dynUpdateInterval` comes round. `dynUpdateTime` does double
  duty: it anchors the period (which is active from the moment it arrives, so
  there is no `startSchedule` to measure from) and it runs a **dead-man's
  switch** - with a `duration` set, a profile whose CSMS stops answering stops
  applying and composition falls through to the next valid one, reviving on the
  next update without a reinstall. So `duration` means something different here
  than on a scheduled profile: not "when the curve runs out" but "how long one
  pushed limit may be trusted unrefreshed". A CSMS outage releases the
  connector instead of freezing a stale limit onto it.

  What 2.1 sends that this crate deliberately drops: the setpoints, discharge
  limits and per-phase (`_L2`/`_L3`) variants a `ChargingScheduleUpdate` may
  carry. Each needs a hardware capability `crate::hardware` cannot express -
  `set_current_limit` is a single import limit - so they are counted, logged and
  dropped rather than stored as values nothing can act on. They land with §17's
  bidirectional-power surface.

  Still missing: `NotifyChargingLimit`/`ClearedChargingLimit` and
  `NotifyEVChargingNeeds`/`NotifyEVChargingSchedule` - all four report where a
  limit *came from* rather than applying one, and the EV-side pair needs the ISO
  15118 surface §13 covers. Nothing here is blocked upstream. The profile
  store itself is durable (`persistence::ChargingProfileSnapshotStore`, wired
  via `ChargePointBuilder::charging_profile_persistence`): installed load
  limits survive a power cut and are restored before the projection's first
  evaluation, so a restart cannot silently un-limit a load-managed charge
  point.
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
- Status: 🚧 partial - **the update flow is wired end to end on all three
  versions** (B3.2). `UpdateFirmware` is answered immediately and the update runs
  on a worker: download, wait, install, reporting every state change as a
  `FirmwareStatusNotification` with the request id that started it.

  Both of OCPP's scheduling points are honoured *and announced* -
  `DownloadScheduled` for a future `retrieveDateTime`, `InstallScheduled` for a
  future `installDateTime` - because a CSMS that heard nothing could not tell a
  scheduled update from a lost one. An unsynchronized clock treats every schedule
  as due now: a charge point that cannot know the instant arrived would otherwise
  never start, and an update that never happens is worse than one that happens
  early.

  Installation waits for running transactions, and while waiting every EVSE is
  held unavailable unless the CSMS allows new sessions - a charge point about to
  reboot should not keep taking drivers it is about to cut off. Availability is
  restored if the install fails, and deliberately not before a reboot.
  `InstallRebooting` is reported before the reboot, which then goes through the
  existing `Reset` path rather than a parallel one, so the fail-safe stop still
  applies.

  Two hardware traits carry this: `FileTransfer` (§12/§14's shared abstraction)
  fetches the image and `FirmwareInstaller` flashes it, kept separate because a
  charge point may be able to do one and not the other. Its `RebootRequired`
  outcome is what lets the crate announce the restart before causing it.

  Still missing: signature and certificate verification (needs crypto and a trust
  store this crate has no hook for - the fields are carried through to the
  integrator untouched), reporting `Installed` after the reboot (needs a marker
  that survives it), and 2.x's local-controller firmware publishing.
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
- Status: 🚧 partial - **log upload is wired end to end on all three versions**
  (B5.1). `GetLog` (2.x) and `GetDiagnostics` (1.6J) are answered immediately and
  the transfer runs on its own task, because OCPP's N01 sequence requires the
  response to precede the `Uploading` notification - a handler that uploaded
  before returning would deliver its status reports after the response they are
  meant to follow. `FileTransfer::upload` (§12) takes this crate's own security
  log as rendered bytes and an integrator-held diagnostics log by name, which is
  the split that lets one abstraction serve both blocks.

  A second `GetLog` supersedes the first (`AcceptedCanceled`): the superseded
  upload's result is discarded rather than reported, so the CSMS never sees a
  stale `Uploaded` for a request it was told was replaced. The transfer itself is
  not aborted mid-flight - that needs a `select!` this crate does not have on both
  no_std and std - so the supersede takes effect at a retry boundary.

  1.6J is genuinely poorer rather than differently named: no log type (so a 1.6J
  CSMS cannot ask for the security log at all), no `requestId` to correlate
  notifications with, and no `AcceptedCanceled`. Still missing here: monitoring
  reports, `GetTransactionStatus`, customer information, and 2.1's periodic
  event streams.

  **Variable monitoring (B5.2) is wired end to end, 2.x only** -
  `SetVariableMonitoring`/`ClearVariableMonitoring` inbound, `NotifyEvent`
  outbound. Thresholds (`UpperThreshold`/`LowerThreshold`) and deltas are
  evaluated the moment a device-model variable's `Actual` attribute changes
  (`crate::state::ChargePointState::apply` on `DeviceModelEvent::AttributeValueSet`
  - the one place a value change originates, per `crate::device_model`'s own
  docs), so no separate polling loop is needed for them; periodic monitors are
  swept on their own clock by `crate::variable_monitoring::run_periodic_variable_monitors`,
  since nothing about the charge point's state changes when a periodic
  interval merely elapses. A monitor is only accepted on a variable whose
  `VariableCharacteristics::supports_monitoring` is `true` - none of this
  crate's own built-in default variables set it, so a hardware binding
  registering a variable worth alerting on (a temperature, a voltage) is what
  turns it on. 1.6J has none of these messages at all, so there is no
  `ocpp_1_6` projection to write. `GetMonitoringReport`/`NotifyMonitoringReport`
  (B5.3) remain open - this only sets up, evaluates, and reports monitors, not
  what's installed.
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
- Status: 🚧 partial — a new `src/data_transfer.rs` module handles both
  directions (`DataTransferSender::transfer_data` outbound,
  `DataTransferRegistrar::register_data_transfer_handler` inbound,
  implemented for `ocpp-client`'s OCPP 2.1 client). Unlike every other
  CSMS-initiated action this crate handles, `DataTransfer` is explicitly
  vendor-defined, so this module deliberately isn't wired into `setup()`
  like `ReserveNow`/`ChangeAvailability`/etc. are - only integrators that
  actually use a vendor extension call
  `register_data_transfer_handler`/`transfer_data` directly against their
  own (cloned) CSMS client handle, supplying their own
  `DataTransferHandler` impl for inbound dispatch (the crate can't decide
  Accepted/Rejected/UnknownVendorId/UnknownMessageId on its own - that's
  the whole point of the block). **Known limitation - still real, but the
  cause moved (re-verified against `ocpp-types` 0.1.3 / `ocpp-client` 0.2.1,
  now that this crate has bumped to them):** the 2.x payload still can't
  cross the wire, so `vendor_id`/`message_id` routing and
  Accepted/Rejected/Unknown* outcomes work today while the actual payload
  doesn't.

  What changed is *why*. The old reading - "`ocpp-types` collapsed `data` to
  `Option<()>` because codegen couldn't represent an arbitrary JSON value,
  with no generic escape hatch" - was true of 0.1.2 and is now wrong. 0.1.3
  makes the type generic in the payload with `()` merely as the default:
  `pub struct DataTransferRequest<DataTransferRequestData = ()>`. The
  escape hatch exists. The remaining blocker is one level down the stack:
  `ocpp-client` 0.2.1's `ocpp_2_1_action!`/`ocpp_2_0_1_action!` entries
  name `DataTransferRequest`/`DataTransferResponse` bare, so
  `send_data_transfer`/`on_data_transfer` monomorphise to the `()` default
  and there's no way to ask them for anything else.

  So this is no longer "wait for upstream to model the type at all" - it's
  a concrete, small piece of work: make `ocpp-client`'s DataTransfer action
  generic over its payload (or add a `send_data_transfer_with`-style
  variant), then have this crate's 2.x adapters pass their existing raw-JSON
  `Option<String>`. This crate's own
  `DataTransferMessage`/`DataTransferResult.data` are already real
  `Option<String>` fields waiting for it. 1.6J is unaffected and has carried
  a real payload all along.
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

## Failure containment

Not an OCPP functional block, but the property `CLAUDE.md`'s error-handling
section asks for, now enforced rather than asserted (G4):

- `#![deny(clippy::unwrap_used, clippy::panic)]` holds over library code, so a
  panic on a path a glitching sensor or a hostile CSMS can reach is a compile
  error. The three sites that existed were compile-time constants; each now
  carries a `const` assertion, so outgrowing a wire bound fails the build rather
  than a charge point in the field.
- `hardware::Watchdog` is fed from the actor's run loop and nowhere else, once
  per applied event and *after* its effects are dispatched. A timer-fed watchdog
  proves the timer is alive; this proves the actor is draining its mailbox,
  which is the thing that actually matters when a connector is energised.
- The actor mailbox is bounded and **refuses** rather than dropping. It carries
  order-dependent state-machine transitions, so dropping either end can wedge a
  connector in a state the hardware has already left; senders are told instead.
- Fault-injection tests cover every fallible hardware method, and assert the
  ordering that makes a fail-safe transition safe - contactor open *before*
  unlock, since releasing a latch while current flows exposes a live pin.

## Examples

Two, deliberately the same charge point seen from opposite ends:

- `examples/embedded_bindings.rs` builds with `--no-default-features` and
  supplies everything that configuration needs and nothing else does - a
  `critical-section` backend, an `Executor`, a `Clock` and a `Backoff`. It makes
  the no_std claim demonstrable rather than asserted, and CI builds it so it
  cannot rot. It is a host binary, not a bare-metal link; the file says so.
- `examples/simulated_charge_point.rs` is the std/tokio path an integrator would
  copy: dial a CSMS, register every block, loop sessions. With no address it runs
  offline, which is the point worth showing - the state machine is the charge
  point, and it charges cars whether or not a backend is listening.

Both put the command loop in `ChargePoint::start`, which is where the trait's
contract puts it, and both print the hardware calls in order - so the fail-safe
ordering the state machine drives, contactor open before unlock, is visible
rather than merely tested.

## Suggested sequencing

The functional blocks above are independent for planning purposes, but in
practice §0 (foundation) → §2 (Provisioning/BootNotification) → §7
(Availability/StatusNotification) → §5 (Transactions) → §3 (Authorization)
→ §10 (Meter values) form the critical path to a minimally useful charger
that can actually hold a session with a CSMS. Everything else (§1, §4,
§6, §8, §9, §11–18) layers on top once that spine exists.

That spine is now in place: every block on it is at least 🚧 with its core
flow wired end-to-end through `setup()`, across 1.6J/2.0.1/2.1. The most
useful next chunks, roughly in order of value per unit of work:

1. **§11 Smart charging** — the largest genuinely-missing block, and the
   one a real deployment is most likely to demand next (load management).
   Needs a charging-profile store, schedule composition, and a schedule →
   hardware current-limit projection, which in turn needs a new hardware
   hook (nothing in `crate::hardware` can express "limit to N amps" today).
2. **§2's leftovers** — `SetNetworkProfile`, device-model persistence, and
   sampled-data configuration actually driving §10's sampling (see the
   inert-variable note there).
3. **`TriggerMessage` for 2.0.1 and 1.6J** (§6) — cheap: the
   protocol-agnostic handler already exists and the upstream types turn out
   to be present for both versions; only 2.1 is blocked.
4. **§12 Firmware management / §14 Diagnostics** — both need a file-transfer
   abstraction this crate doesn't have yet, so doing them together and
   sharing that abstraction is worth more than doing either alone.

§13 (ISO 15118), §17 (DER/V2X) and §18 (battery swap) all depend on
hardware capabilities most chargers don't have, and want the
capability-model gap noted in §0's hardware bullet closed first.
