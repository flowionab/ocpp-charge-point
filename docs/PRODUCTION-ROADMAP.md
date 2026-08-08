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
| **2.0.1** | **21** | 64 (0.2.2; the 63 here was 0.2.0) | 64 (`ocpp-types` has 64 request types) |
| **2.1** | **22** | 91 (0.2.2; the 86 here was 0.2.0) | 90+ (`ocpp-types` has 90 request types) |

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
| `no_std` | ✅ | Compiles for a real bare-metal target (`thumbv7em-none-eabihf`), not just with features off — that took dropping `tracing`'s default features and a `getrandom` backend cfg ([H1.3](#101-h1--ci-hardening)). `embassy-sync` channels, `tokio` fully optional. Until the [G3.1](#93-g3--time) follow-up, this held only for a build with *no version feature* — every OCPP adapter was `std`-gated dead code, so a bare-metal build could not actually speak OCPP. All three version adapters are now reachable without `std`. |
| Offline queueing | ✅ | `OfflineQueue` is used by Availability / Transactions / Security, bounded ([G2.1](#92-g2--bounded-memory)) with a per-queue overflow policy, and durable across a reboot ([E2.8](#72-e2--what-must-survive)/[E4.3](#74-e4--recovery)). Every other growable collection is audited and bounded too ([G2.2](#92-g2--bounded-memory)), with measured figures in [`docs/MEMORY.md`](MEMORY.md). |
| Reconnect resync | ✅ | Fresh BootNotification on every reconnect, all three versions. |
| Persistence | 🚧 | `hardware::Storage` plus `crate::persistence`: the in-flight transaction and its id counter, all three offline queues, the local auth list, reservations, `persistent` device model attributes, charging profiles, the authorization cache, the boot reason and the security log all survive a restart, each registered per concern on `ChargePointBuilder` (opt-in — `setup()` wires none of them, having no `Storage`). Power-cut recovery is swept at every point of a session ([E4.4](#74-e4--recovery)). Still RAM-only, each blocked on a block that doesn't exist yet: certificates, network profiles. |
| Test suite | 🚧 | 757 test functions in `src/`, three integration tests (`connect_2_1_websocket`, `memory_budget`, `power_cut_recovery`). Strong unit coverage; end-to-end is no longer zero but is still missing a mock CSMS over a real socket ([H2.1](#102-h2--integration-testing)). |
| CI | ✅ | Gating: clippy + fmt + rustdoc, feature matrix, `thumbv7em-none-eabihf`, MSRV 1.88, `cargo-deny`, on PRs too, plus a coverage floor on the protocol adapter files ([H1.6](#101-h1--ci-hardening)). Whole-crate coverage stays informational. |

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
| **A1** | `connect_and_setup` for 1.6J and 2.0.1 — **done.** All three versions run a session; the 1.6J and 2.0.1 paths register blocks through `ChargePointBuilder` (1.6J needs topology-aware wrappers, 2.0.1 lacks `SecurityEventNotification` upstream), which is exactly the limitation [C4](#54-c4--builder-refactor) removed. | ✅ |
| **A2** | **Version negotiation** — **done.** The handshake already offered every compiled-in version; what was missing was running the result, which [A1](#3-workstream-a--transport-negotiation-connection-lifecycle) supplied. `UnsupportedNegotiatedVersion` is now only reachable for a version this build wasn't compiled with. An end-to-end test negotiates 1.6J and asserts the *1.6J* BootNotification shape on the wire, so it proves the right adapter set ran rather than that something connected. | ✅ |
| **A3** | Configurable subprotocol preference order — **done** (`connect_and_setup`'s `versions`, which existed but was only meaningful once [A1](#3-workstream-a--transport-negotiation-connection-lifecycle) made non-2.1 outcomes usable). Tested: naming one version offers only that one, even against a CSMS that would speak another. | ✅ |
| **A4** | WebSocket keepalive — **blocked upstream.** `ocpp-client` 0.2.1's `ConnectOptions` has no ping-interval field and its WebSocket transport only *replies* to pings; there is nothing to configure from here. `WebSocketPingInterval` is registered and readable, with a value of `0` (disabled), which is the honest reading of a cadence this charge point does not drive. Needs an `ocpp-client` change, the same shape as [D1](#61-d1--missing-action-wrappers)'s wrappers. | 🔒 |
| **A5** | Reconnect backoff from the device model — **done.** All three variables are registered, readable, and now applied. `RetryBackOffWaitMinimum`/`RetryBackOffRepeatTimes` become the transport's `ReconnectPolicy` when the caller supplies no `ConnectOptions`, so what a CSMS reads is what the connection does. `RetryBackOffRandomRange` — previously registered as `0` and applied nowhere, because `ocpp-client`'s `ReconnectPolicy` still has no jitter field — turns out **not to need one**: this crate already owns the reconnector (`network_switch::ConnectionTarget`, built for [A9](#3-workstream-a--transport-negotiation-connection-lifecycle)), so the random part is added here, on every redial, before dialling. See below for the seeding problem that is the whole difficulty, and for the split in what a CSMS write reaches. | ✅ |
| **A6** | Per-message timeouts and retry — **done.** `MessageTimeout[Default]` becomes the transport's per-call timeout at connect time (same next-connection caveat as [A5](#3-workstream-a--transport-negotiation-connection-lifecycle)); `MessageAttempts[TransactionEvent]` now caps how many times a queued message is retried, which is **head-of-line unblocking**: without it a message the CSMS will never accept is retried forever at the front of the queue and blocks everything behind it. `MessageAttemptInterval` landed with [A7](#3-workstream-a--transport-negotiation-connection-lifecycle). | ✅ |
| **A7** | `MessageAttemptInterval` / queue-depth limits — **done.** Queue depth is now configurable (`ChargePointBuilder::offline_queue_capacity`, previously hardcoded at the default); `MessageAttemptInterval[TransactionEvent]` drives a retry timer that sweeps every registered queue, re-read each cycle. Drop policy and the `MemoryExhaustion` event on overflow were already done ([G2.1](#92-g2--bounded-memory)) and are unchanged - including the security queue's deliberate exception, which must not report its own overflow through itself. | ✅ |
| **A8** | `NotImplemented` CALLERROR — **done.** An integration test boots a real session over a WebSocket, has the mock CSMS call `GetLog` (a message this build has no handler for), and asserts a CALLERROR with `NotImplemented` comes back. The wait is bounded, because the failure it really guards is *silence*: a CSMS waiting on a response that never arrives is worse than one told no, and an unbounded wait would hang the suite instead of failing it. | ✅ |
| **A9** | Network profile selection *and* application — **done.** `NetworkConfigurationPriority` is a live value (a stored slot joins the order, a vacated one leaves it, the operator's ordering is never rewritten), `network_profile::selected_profile` says which slot it picks, and `network_switch` moves the live connection there and rolls back after `NetworkProfileConnectionAttempts` failures. See below for the rules and the two limits. | ✅ |

**A5's jitter, and why the seed is the whole problem.** A CSMS that goes down takes every charge
point it serves with it, and they all notice within a second of each other. Without jitter they
retry in lockstep — same exponential curve, same starting instant — so the CSMS coming back up
meets its entire fleet at once and goes down again. The fix is a random delay per station, which
means the interesting part is not the generator but its **seed**: a fleet that seeds identically
draws identical "random" delays and is no better off than one with no jitter at all.

Three ingredients are mixed (FNV-1a, then xorshift64\* — this spreads retries, it does not resist
an adversary, and a `rand` dependency for one number on a bare-metal target would be an odd
trade). The CSMS address distinguishes fleets but not stations within one. The Basic-auth
username is, in most OCPP deployments, the charge point's own identity — it is what saves a fleet
that all *boots* at once (a regional power cut), where uptime-derived entropy would correlate.
Wall-clock nanoseconds decorrelate stations whose connections dropped at slightly different
moments, which is the common case and where a few milliseconds is already plenty. None is
sufficient alone.

Jitter is deliberately applied to a profile switch too, which is also a redial: a CSMS that
rewrites a whole fleet's network profile would otherwise point all of them at a new endpoint
simultaneously — the same stampede, aimed somewhere fresh.

**This also splits what a CSMS write reaches.** `initial_delay`/`max_delay` are sealed into the
transport when it is built, so writing them still only affects the next connection.
`RetryBackOffRandomRange` is read from the *live* device model by `run_network_profile_switching`
on every state change (the same path that already re-reads `NetworkProfileConnectionAttempts`), so
a write to it takes effect on the connection already running. Not a design choice so much as a
fact about who owns which half. One consequence worth stating: a caller who supplies their own
`reconnector` instead of letting `connect_and_setup` install one gets no jitter, because there is
no longer anywhere for this crate to add it.

The registered default stays `0`. OCPP names no default, and a charge point that spread its
retries without being asked would be doing something its operator did not choose — but an operator
running a fleet against one CSMS should set it.

**How a switch works, and what it deliberately does not do.** The mechanism is the one the
blocked-upstream note here used to describe: re-point the transport's reconnect target, then close
the connection. `ocpp-client` redials the new address through the **same** `Client`, so every
registered handler, every offline queue and every forwarder survives the move — a switch is
invisible to the rest of the crate, where dropping the client and reconnecting would strand all of
it. This needed `ConnectOptions::reconnector` plus a public `websocket_transport()`, both shipped
in `ocpp-client` 0.2.2 (the change was written here, upstreamed, and released rather than worked
around locally, per `CLAUDE.md`'s "do not duplicate transport concerns").

*Rollback.* A profile the CSMS wrote may simply not work, and a charge point that moved to it and
stayed would be unreachable — unreachable in a way the CSMS cannot fix, because it can no longer
reach the charge point to correct the profile it just wrote. So the target reverts to the last
address that **worked** after `OCPPCommCtrlr`/`NetworkProfileConnectionAttempts` consecutive
failures (OCPP's own count for this, not an invented constant). Two switches without a successful
connection in between keep the original fallback rather than reverting to an address that was never
proven either.

*A one-second grace period* separates the decision from the close. A switch is nearly always
triggered by the CSMS's own `SetNetworkProfile`, and closing immediately races that request's
response out of the socket — leaving the CSMS never knowing whether the profile was accepted, which
is exactly what it needs before it stops expecting this charge point on the old address. There is
nothing to acknowledge instead: OCPP defines no "response flushed" handshake and the transport
exposes none.

Two limits, both real rather than oversights. **Credentials are not carried across a switch** —
they belong to the CSMS that issued them, and sending them to whatever host a profile names would
hand this charge point's password to a different server; combined with `basicAuthPassword` being
dropped on the way in ([B1.8](#b1--core-spine-must-be-complete-for-any-production-deployment)), a
profile whose endpoint demands Basic auth will fail and roll back until workstream F lands. And
**only `connect_and_setup` can switch**: a caller who built their own client owns their own
transport, so they get `selected_profile` and drive their own redial rather than a switch that
silently does nothing.

[A5](#3-workstream-a--transport-negotiation-connection-lifecycle) is *not* finished by this. A9
re-points where a connection goes; the backoff and per-call timeout are fixed in the transport when
it is built, so a CSMS's `RetryBackOff*` write still applies to the next connection rather than the
current one.

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
| StatusNotification | ✅ | ✅ | ✅ | |
| Authorize | ✅ | ✅ | ✅ | |
| StartTransaction / StopTransaction | ✅ | — | — | |
| TransactionEvent | — | ✅ | ✅ | |
| **MeterValues** (standalone) | ✅ | ✅ | ✅ | Driven by `AlignedDataCtrlr.Interval`; 1.6J previously had it only *inside* a transaction. |
| DataTransfer | ✅ | ✅ | ✅ | |
| ChangeAvailability | ✅ | ✅ | ✅ | |
| Reset | ✅ | ✅ | ✅ | |
| UnlockConnector | ✅ | ✅ | ✅ | |
| RemoteStart/Stop · RequestStart/StopTransaction | ✅ | ✅ | ✅ | |
| **ClearCache** | ✅ | ✅ | ✅ | |
| **TriggerMessage** | ✅ | ✅ | ✅ | Heartbeat and StatusNotification are fulfilled; every other `requestedMessage` is refused with `NotImplemented`. |
| GetConfiguration / ChangeConfiguration | ✅ | — | — | Every *required* standard key readable except `ConnectorPhaseRotation`; 23 aliased plus 10 answered from live state. |
| GetVariables / SetVariables | — | ✅ | ✅ | |
| GetBaseReport / GetReport / NotifyReport | — | ✅ | ✅ | |

**B1 tasks:**

- [x] **B1.1** Standalone `MeterValues` — `src/meter_values.rs`, on all three versions, driven by
      `AlignedDataCtrlr`/`Interval`.

      **What was actually missing.** 2.x carries meter data inside `TransactionEvent`, which covers
      everything a *session* measures; what no version could express was OCPP's clock-aligned
      data — readings due on the wall clock whether or not anything is charging, so a CSMS can
      bill standing consumption and reconcile a meter across sessions. 1.6J's standalone
      `MeterValues` existed but only *inside* the transaction notifier, so readings taken between
      sessions had nowhere to go either.

      That needed a reading which outlives its transaction: `EvseState::latest_meter_samples`
      records every `MeterValueSampled` regardless of connector state, alongside (not instead of)
      `Transaction::last_meter_sample`, which still only moves while charging. One existing test
      asserted a sample with no transaction produced *no effects at all*; it now asserts the
      correct behaviour instead — no fabricated `TransactionEvent`, but the reading is kept.

      **`AlignedDataCtrlr.Interval` is live, not decorative.** It is a new built-in device-model
      default (`0` — OCPP's own "disabled", so a charge point nobody configured sends nothing
      rather than picking a drumbeat this crate invented), aliased from 1.6J's
      `ClockAlignedDataInterval`, and read fresh on every cycle exactly as `run_heartbeat` reads
      `HeartbeatInterval` — so enabling or changing it via `SetVariables`/`ChangeConfiguration`
      takes effect on the next cycle without a reboot. While disabled the loop re-checks once a
      minute rather than exiting, which is what makes runtime enabling work at all. This is the
      second variable rescued from the inert-variable trap §2 flagged.

      **Alignment is to the wall clock**, computed against midnight UTC: a 900-second interval
      fires at :00/:15/:30/:45, not 900 seconds after whichever moment this charge point happened
      to boot — two charge points on a site then report at the same instants, which is what makes
      their readings comparable. A boundary that is exactly now sleeps a full interval rather than
      spinning.

      The three adapters reuse `crate::transactions`' existing per-version meter-value builders
      rather than carrying a second copy of the measurand mapping — two mappings of the same
      measurands into the same wire type would be two places for a unit bug to hide. 2.x's
      `MeterValuesRequest` addresses an EVSE with no connector field at all (the spec's shape, not
      a simplification made here); 1.6J's addresses a flat `connectorId`, so it keeps the connector
      half and resolves it through `crate::topology`, dropping — with a warning — a reading whose
      address doesn't exist in the topology rather than letting it fall back to connector `0`,
      which in 1.6J means the charge point itself and would misattribute the energy.

      Deliberately **not** wrapped in an offline queue, unlike the status/transaction/security
      streams: a reading whose entire meaning is "this is the value at 10:15" is worth much less an
      hour later, and the register it came from is cumulative anyway, so the next aligned reading
      subsumes a lost one.

      **Not done: `SampledDataCtrlr`/`TxUpdatedInterval` is still inert.** It governs how often
      *transaction* meter data is reported, and this crate never polls hardware — the integrator
      pushes readings in — so the sampling rate is the binding's choice today. Throttling what
      reaches the CSMS to that interval is real, contained work in the outbound path (where a clock
      exists), and it is left outstanding rather than half-done.
- [x] **B1.2** Authorization cache + `ClearCache` on all three versions —
      `src/state/authorization_cache.rs` and `src/authorization.rs`.

      **The cache is learned; the local authorization list is pushed.** That distinction decides
      the precedence: `SendLocalList` is an operator's advance decision and outranks anything the
      charge point merely observed, so an offline lookup consults the list first and the cache
      second. Only the cache expires.

      **It is consulted, not just filled.** A cache nothing reads would be exactly the
      inert-feature trap §2's variables kept falling into, so this slice also gave
      `run_authorization_requests` an offline path: an `Authorize` that fails at the transport now
      falls back to the list, then the cache, instead of denying outright. Nothing offline having
      an opinion is still a denial - erratic connectivity must not leave a connector waiting, and
      "we don't know" is not "yes". This is also the first time the local authorization list is
      consulted anywhere (R§4 had flagged that it wasn't).

      **Rejections are cached too.** A cache that only remembered acceptances would let a revoked
      card in every time the link drops - the opposite of what caching is for.

      Three device-model variables gate it, all registered as built-in defaults rather than left
      absent with a guessed fallback, so "what will you do offline?" is a question the CSMS can
      actually ask: `AuthCacheCtrlr`/`Enabled` (1.6J's `AuthorizationCacheEnabled` aliases onto
      it), `AuthCacheCtrlr`/`LifeTime` (0 = entries don't age out), and
      `AuthCtrlr`/`LocalAuthorizeOffline` (false disables *both* offline sources).

      Bounded by `StateLimits::max_authorization_cache_entries` (default 50), evicting the least
      recently *authorized* entry - and that wording is exact: `lookup` takes `&self` so it can
      read from a state snapshot, so eviction order tracks CSMS decisions rather than cache hits.
      Documented on the method rather than left to be discovered.

      Expiry follows the same clock-honesty rule as everything else here: `cached_at` comes from
      the caller's `Clock` and is `None` when that clock isn't synchronized, which makes the entry
      non-expiring. On hardware with no RTC the alternatives are a cache that caches nothing or an
      invented age; keeping the entry is also the recoverable error, since `ClearCache` exists.

      `ClearCache` is `Accepted` even when the cache was already empty - the CSMS asked for "no
      cached decisions", and that is the resulting state either way - and `Rejected` only when
      caching is switched off entirely, where accepting would imply a cache this charge point is
      keeping clear.

      Persisting the cache ([E2.5](#72-e2--what-must-survive)) followed immediately, so a reboot
      no longer loses exactly the decisions an offline charge point would have needed.
- [x] **B1.3 / B1.4** `TriggerMessage` wire adapters for all three versions, in
      `src/remote_control.rs` beside the protocol-agnostic `handle_trigger_message` that has been
      waiting for them. **B1.4's upstream dependency was already satisfied**: 2.1's
      `on_trigger_message` was genuinely absent in `ocpp-client` 0.2.0, [D1.1](#61-d1--missing-action-wrappers)
      added it upstream and [D1.2](#61-d1--missing-action-wrappers) bumped this crate to 0.2.1 -
      re-verified against the pinned source here rather than assumed either way - so B1.4 needed
      no further upstream work and landed alongside B1.3 instead of after it.

      Two `requestedMessage` values are fulfilled, `Heartbeat` and `StatusNotification`, because
      those are the two this crate has an outbound path for. Everything else is answered
      `NotImplemented` rather than `Rejected` - the distinction matters, since `Rejected` claims
      the request was understood and refused, while these simply have no functional block behind
      them yet (`BootNotification` needs vendor/model strings this module can't see;
      `MeterValues`/`TransactionEvent` need a resend-current-snapshot capability neither block
      has; the log/firmware/certificate triggers need §1/§12). That is also why
      `TriggerMessageOutcome` still has no `NotImplemented` variant: the decision belongs to the
      wire layer, which is the only place the unsupported values exist.

      Addressing is the usual three-way split. 2.x's optional `evse` maps onto
      `AvailabilityTarget` with the wire's 1-based ids converted to this crate's 0-based ones, and
      an id that cannot exist (`0` or negative) is *rejected* rather than silently widened to "the
      whole charge point" - answering a broader request than the CSMS made would be worse than
      refusing. 1.6J's flat `connectorId` resolves through `crate::topology` and keeps the
      connector half, since 1.6J has no EVSE to lose it to; absent or `0` means the whole charge
      point, per that version's own convention.

      One structural wrinkle 1.6J alone has: `handle_trigger_message` needs a single notifier that
      is both a `HeartbeatSender` and a `StatusNotifier`, and under 1.6J those live on different
      types (the bare client sends heartbeats; status notifications need `Ocpp1_6StatusNotifier`'s
      topology to flatten the address). `Ocpp1_6TriggerMessageHandler` implements both by
      delegating to each, so the shared handler works unchanged rather than growing a
      version-shaped bound.

      Registration is `ChargePointBuilder::trigger_message`, separate from `remote_control`:
      `TriggerMessage` needs a CSMS type that can *send* the triggered messages, and folding that
      bound into `remote_control` would force it on callers who only wanted `UnlockConnector`.
- [x] **B1.5** `ConnectorState` gained `SuspendedEv` and `SuspendedEvse`, reached from `Charging`
      via new `ConnectorEvent::ChargingSuspendedByEv`/`ChargingSuspendedByEvse` and left via
      `ChargingResumed` — pushed in by the hardware binding, which is the only thing that can tell
      *which side* stopped the energy flow.

      **Suspension is a pause inside a running transaction, not a stop.** The contactor stays
      closed, the cable stays locked, and the transaction keeps its id and `seqNo`. A suspended
      connector can also change sides directly (EV pauses, then the EVSE cuts supply too) without
      passing through `Charging` — reporting a spurious "charging" in between would be wrong — and
      still stops, faults and resets through exactly the paths a charging connector does, so no
      fail-safe path gained a second variant to keep in step.

      **The two versions express it in different places, which is the whole reason this needed a
      connector-level distinction rather than a mapping.** 1.6J has wire statuses for it, so
      `availability::ocpp_1_6::map_status` now produces `SuspendedEV`/`SuspendedEVSE` — the two
      values it could never reach before. 2.x moved the distinction onto the transaction's
      `chargingState`, so the connector *status* stays `Occupied` and
      `advance_transaction` reports a `TransactionEvent(Updated, ChargingStateChanged)` carrying
      `SuspendedEV`/`SuspendedEVSE` instead. `TransactionChargingState` already had both variants;
      they were simply unreachable, and the 2.x adapters already mapped them.

      One subtlety the tests pinned down: widening the charging-state arm to cover suspension made
      it match `Charging` → `Charging` self-loops too (a meter sample applied mid-charge doesn't
      change connector state), which bumped `seqNo` and would have sent the CSMS an `Updated`
      event saying nothing had changed. Guarded on an actual state change; an existing test caught
      it.

      A meter reading taken while suspended is still recorded — a suspended session is still a
      session, and the register can move.
- [x] **B1.6** 1.6J standard configuration keys — every **required** key of the Core profile and
      of the optional profiles this crate implements is now readable on a fresh charge point, with
      one documented exception.

      **Readable was the hard part, and it wasn't only the alias table.** An alias maps a 1.6J key
      onto a `(Component, Variable)`; if nothing is *registered* behind it, `GetConfiguration`
      still answers `unknownKey`. So the aliases grew from 14 to 23, and
      `DeviceModel::register_defaults` became a table (`DEFAULT_VARIABLES`, 26 entries) that
      registers the variables behind them. Two tests turn B1.6's requirement into a guarantee
      rather than a claim: every required 1.6J key resolves *and has a value* on a charge point
      straight out of `ChargePointState::new`, and an unfiltered `GetConfiguration` lists them all
      — because that is how a CSMS discovers a charge point. A third asserts every alias has a
      registered variable behind it, since an alias with nothing behind it is worse than no alias.

      **Ten keys are answered from live state instead of being stored**, and for two distinct
      reasons kept distinct in `DerivedKey`'s docs. *Derived*: the answer already exists somewhere
      authoritative and a second copy could disagree — `NumberOfConnectors` (topology),
      `LocalAuthListMaxLength`/`SendLocalListMaxLength`/`MaxChargingProfilesInstalled`
      (`StateLimits`), `SupportedFeatureProfiles` (`Capabilities`). *Advisory*: this crate imposes
      no limit at all, but 1.6J requires the key, so `GetConfigurationMaxKeys`,
      `ChargeProfileMaxStackLevel` and `ChargingScheduleMaxPeriods` report a documented figure a
      CSMS can size requests against, and exceeding it is accepted anyway. Reporting a bound this
      crate does not enforce would have been the dishonest option. A `ChangeConfiguration` on any
      of them is `Rejected` ("it exists, you can't write it"), not `NotSupported`, which would
      claim the charge point had never heard of a key it just reported a value for.

      **Which writable keys take effect is stated, not implied.** Five are *live* — read on the
      path they govern, so a CSMS write changes behaviour on the next cycle: `HeartbeatInterval`,
      `AlignedDataCtrlr.Interval`, `AuthCacheCtrlr.Enabled`, `AuthCacheCtrlr.LifeTime`,
      `AuthCtrlr.LocalAuthorizeOffline`. The rest are *recorded* — stored and reported faithfully,
      not yet consulted. `DEFAULT_VARIABLES`' own docs carry that split, because a required key
      answered honestly-but-inertly is a compliance pass and a behaviour gap at the same time, and
      only one of those is visible from the wire.

      `ConnectorPhaseRotation` remains the one required Core key this crate does not answer, for
      the reason already recorded in R§2: 1.6 packs a per-connector list into one key while 2.x
      models `PhaseRotation` per connector, and that fan-out doesn't fit a static alias. It is
      excluded explicitly in the test's own list rather than quietly missing.

      **Memory moved and the figures were re-measured, not estimated**: registering 26 variables
      by default lifts the empty-state floor from ~5 KB to ~17 KB, so retained-heap totals are now
      ~59 KB / ~179 KB / ~401 KB. `docs/MEMORY.md` and the README carry the new numbers; the
      existing ceilings still hold.
- [x] **B1.7** 2.x required device-model variables — registered, scoped per capability, and
      honest about which are inert.

      Of the vendored 2.1 appendix's **122 required rows across 23 components**, this crate now
      registers every one belonging to a component whose functionality it actually has: 45
      always-on variables in `DEFAULT_VARIABLES` (`OCPPCommCtrlr`, `TxCtrlr`, `SampledDataCtrlr`,
      `AlignedDataCtrlr`, `AuthCtrlr`, `ClockCtrlr`, `SecurityCtrlr`, `DeviceDataCtrlr` …) plus 11
      in `CAPABILITY_GATED_VARIABLES` that arrive only with their capability.

      **56 of the 122 belong to blocks that do not exist here** — `PaymentCtrlr` (22),
      `DCDERCtrlr` (16), `NetworkConfiguration` (9), `WebPaymentsCtrlr` (5), `V2XChargingCtrlr`
      (3), `ISO15118Ctrlr`, `ACDERCtrlr` — and are registered nowhere. That is
      [C3](#53-c3--capability-propagation)'s rule applied consistently rather than an omission:
      those capabilities are `false`, the component reports `Available: false`, and a charge point
      that cannot run a block owes no configuration for it. Two tests hold the line: a capability
      that is on brings every required variable its component owes and a capability that is off
      brings none, and every `CAPABILITY_GATED_VARIABLES` entry names a component some
      `CAPABILITY_GATES` row really gates (dead data there would be indistinguishable from a typo).

      **Every value is one this crate can defend**, which is where most of the work went.
      `SecurityCtrlr.SecurityProfile` is `1`, because TLS and certificates are workstream F and
      claiming 2 or 3 would advertise security this charge point does not have.
      `OCPPCommCtrlr.FileTransferProtocols` is empty, because file transfer arrives with B3/B5.
      `TxCtrlr.TxStartPoint` is `Authorized`, because that is when `advance_transaction` actually
      starts a transaction. `SmartChargingCtrlr.LimitChangeSignificance` is `0`, because the
      projection reports every composed change however small. `SecurityCtrlr.OrganizationName` and
      `TariffCostCtrlr.Currency` are empty rather than invented. `DeviceDataCtrlr.ItemsPerMessage
      [GetReport]` is `16` — `reporting::REPORT_CHUNK_SIZE`, the real figure, not an aspiration.

      Registration makes a variable readable and writable; it does not make it *live*. The five
      live variables are unchanged from B1.6, and `DEFAULT_VARIABLES`' docs keep carrying that
      split rather than letting a full device model imply a fully-implemented charge point.

      **Not done, and worth naming**: the per-EVSE and per-connector required rows (`EVSE.Available`,
      `EVSE.AvailabilityState`, `Connector.Available`, `Connector.ConnectorType`, the `SupplyPhases`
      trio) are *not* registered. Their values live in the connector state machine or in hardware
      the crate cannot see, and a stored copy would be a second source of truth that goes stale the
      moment a connector changes state — the exact failure the 1.6J adapter's derived keys were
      built to avoid. They want the same treatment: a derived-variable path shared by
      `handle_get_variables` and `crate::reporting`, which is a contained follow-up rather than
      something to fake now.

      Memory moved again and was re-measured: the empty-state floor is ~24 KB (from ~17 KB after
      B1.6, ~5 KB before both). Totals barely moved — the device model is filled to
      `max_device_model_variables` either way, so defaults displace filler rather than adding to
      it; what changed is that more of that budget now goes to variables OCPP requires.
      `docs/MEMORY.md` explains that, and the ceilings still hold.
- [x] **B1.8** `SetNetworkProfile` handler — `src/network_profile.rs` and
      `src/state/network_profile.rs`, on 2.0.1 and 2.1 (1.6J has no such message; its network
      configuration lives in the security whitepaper extensions
      [D2.2](#62-d2--type-completeness-audit) covers).

      **This stores profiles; it does not switch connections**, and the boundary is stated in the
      module docs, on the builder method and in the roadmap rather than left to be inferred from a
      working-looking slot store. OCPP says a profile applies to a *future* connection attempt and
      the CSMS orders slots separately, so storing is genuinely the message's job — but a charge
      point built on this crate keeps talking to whatever address its integrator passed
      `connect_and_setup` until the switching loop moves it. Dialling a stored profile, with
      rollback when the new one fails to connect, is
      [A9](#3-workstream-a--transport-negotiation-connection-lifecycle) and is now done.

      Three requests are refused rather than stored, each for a reason the charge point can defend:
      a **negative slot** (addresses nothing), **SOAP transport** (OCPP 2.x is JSON over WebSocket;
      this crate cannot speak it), and a **new slot beyond
      `StateLimits::max_network_profile_slots`** (default 4). Replacing an *occupied* slot always
      succeeds even at the bound — the CSMS is not asking for more storage, and refusing would
      leave the charge point holding a profile the CSMS believes it replaced. A profile naming a
      security profile this crate cannot run *is* stored: staging one for a future firmware is
      legitimate, and what this crate must not do is claim to have applied it.

      **`basicAuthPassword` is dropped on the way in, deliberately.** It is a credential this crate
      cannot use (security profiles are workstream F), and keeping it in state that `GetVariables`
      reports and durable storage writes would be a liability with no upside. `apn`/`vpn` are
      dropped too: they configure an integrator's connectivity stack, not the OCPP application
      layer.

      One version difference worth recording: 2.0.1's `OCPPInterfaceEnum` has eight values where
      2.1 has nine (`Any` is 2.1's addition), and 2.0.1's `NetworkConnectionProfile` has no
      `identity` field at all. The 2.0.1 adapter matches its enum exhaustively with no catch-all,
      so a value added upstream becomes a compile error rather than a silent `Any`.

      **E2.11 (persisting the slots) is unblocked by this** and left outstanding —
      `ChargePointEvent::PersistedNetworkProfilesRestored` is already in place for a restore to
      use. It matters more now that A9 has landed: a charge point that reboots forgets which
      profile it was moved onto and comes back on the address its integrator compiled in.

### B2 — Smart charging (R§11)

Was the largest genuinely-missing block, and the one a real deployment demands
first: without it there is no load management. **Complete as of B2.6**, except
the three notify-flows below that report a limit's *origin* rather than apply
one (`NotifyChargingLimit`/`ClearedChargingLimit`,
`NotifyEVChargingNeeds`/`NotifyEVChargingSchedule`), which have no task of their
own yet and need the EV-side ISO 15118 surface [B4.5](#b4--certificates-and-iso-15118-r1-r13) covers.

| Message | 1.6J | 2.0.1 | 2.1 |
|---------|:----:|:-----:|:---:|
| SetChargingProfile | ✅ | ✅ | ✅ |
| ClearChargingProfile | ✅ | ✅ | ✅ |
| GetCompositeSchedule | ✅ | ✅ | ✅ |
| GetChargingProfiles / ReportChargingProfiles | — | ✅ | ✅ |
| NotifyChargingLimit / ClearedChargingLimit | — | ⬜ | ⬜ |
| NotifyEVChargingNeeds / NotifyEVChargingSchedule | — | ⬜ | ⬜ |
| NotifyPriorityCharging / UsePriorityCharging | — | — | ✅ |
| PullDynamicScheduleUpdate | — | — | ✅ |
| UpdateDynamicSchedule | — | — | ✅ |
| NotifyAllowedEnergyTransfer | — | — | ⬜ |

**B2 tasks:**

- [x] **B2.1** Charging profile store in `ChargePointState` (stack levels,
      purposes, validity windows, recurrency) - `src/state/charging_profile.rs`.
      `ChargingProfileStore` holds `InstalledChargingProfile`s (a profile plus the
      `ChargingProfileScope` - one EVSE, or charge-point-wide, which is OCPP's `evseId`/
      `connectorId` `0` sentinel made explicit), mutated only through
      `ChargePointEvent::ChargingProfileSet`/`ChargingProfilesCleared` like every other piece of
      state here.

      The model carries the **superset** across versions, per `CLAUDE.md`: five purposes (2.1's
      `PriorityCharging` and the external-constraints purpose included), up to three schedules per
      profile (2.x) rather than 1.6J's one, and `ChargingProfileKind`/`RecurrencyKind` in full.
      Two install rules, both from the spec and both applied before the bound is checked so a
      replacement is never refused for being one too many: same profile id replaces (whatever
      scope it was at), and same `(scope, purpose, stackLevel)` replaces (the CSMS is addressing
      one slot).

      **Bounded per G2.2**: `StateLimits::max_charging_profiles`, default 16. A profile beyond it
      is *refused*, not evicted - unlike the offline queues, where dropping the oldest message is
      the lesser evil. Silently forgetting a *limit* would let the charge point draw more current
      than the CSMS last told it to, which is a safety question rather than a data-loss one.
      `tests/memory_budget.rs` measures the full store (~296 B per profile at eight schedule
      periods); the ceilings moved with it.
- [x] **B2.2** Schedule composition: profile stack → composite schedule at
      time `t`, with the 1.6J/2.x precedence rules - `crate::smart_charging::compose`, a pure
      function of the profiles plus a `CompositionContext`, so every rule is testable without a
      clock, a CSMS or hardware (28 tests).

      Three rules, applied per instant: **applicability** (validity window, schedule coverage
      anchored per kind, and - for transaction-scoped purposes - an actually-running transaction,
      with a `TxProfile` naming a different transaction ignored); **selection** (highest purpose
      wins, so `TxProfile` beats `TxDefaultProfile` whatever their stack levels, then highest
      stack level within a purpose); **capping** (the installation limit and external constraints
      bound the result, lowest binding - and a cap with nothing to cap *is* the limit). Instants
      where the result is unchanged are merged, so a cap that moves without ever binding doesn't
      split the composite.

      Two deliberate refusals to guess. **Unit conversion**: amps↔watts needs the supply voltage
      and phase count, which this crate does not have; a caller supplies `SupplyCharacteristics`
      or a schedule in the other unit is skipped, never mis-scaled (assuming 230 V single-phase on
      a 400 V three-phase supply would over-limit by 5×, which is a safety problem, not a billing
      one). **`Relative` anchoring**: those schedules start with their transaction, and
      `Transaction` carries no timestamps by design (E2.1's clock-free state machine), so the
      start time comes from the caller - `None` skips the profile rather than anchoring it to a
      guess.

      A composition-boundary cap (512) bounds the work a pathological profile set can demand of an
      MCU, and logs when it truncates rather than silently shortening the answer.
- [x] **B2.3** **New hardware hook** — `Connector::set_current_limit`, batched with
      [C2](#52-c2--runtime-capability-declaration)'s `Capabilities` change and
      [E1](#71-e1--storage-trait)'s storage hook so integrators absorbed *one* break. Its
      signature widened to `Option<u32>` when [B2.4](#b2--smart-charging-r11) gave it its first
      caller — see that entry for why the change was taken then rather than later.
- [x] **B2.4** Composite schedule → hardware limit projection, re-evaluated
      on profile change, schedule period boundary, and transaction start -
      `crate::smart_charging::run_charging_limit_projection` (state-change driven) and
      `run_charging_limit_schedule` (period-boundary driven), registered together by
      `ChargePointBuilder::smart_charging`.

      **Two loops rather than one**, because the two triggers are unrelated and this crate has no
      `select!` that works on both `no_std` and std without a new dependency. The overlap is free:
      the state machine only dispatches `HardwareCommand::SetCurrentLimit` when the computed limit
      *differs* from what the connector was last asked for, so whichever loop gets there first
      wins and the other's duplicate dies in `ChargePointState`. The timer loop sleeps until the
      exact next boundary (via `Backoff`, capped at 15 minutes) rather than polling.

      **`Connector::set_current_limit` changed signature to `Option<u32>`.** A limit that stops
      applying has to be *removed*, and only the hardware knows its own maximum - a `u32`-only
      hook would have forced every integrator to guess one. `None` means "no CSMS-imposed limit
      any more"; a suspend-charging 0 A period is `Some(0)`, and the two must not be conflated.
      The change was taken now, while the hook still had no caller at all (M1 landed it unwired),
      rather than after integrators had built against it.

      Per-connector `EvseState::charging_limits` (requested) and `applied_charging_limits`
      (hardware-confirmed) are separate side tables: the projection compares against the
      requested one, and a CSMS-facing report should quote the confirmed one.
- [x] **B2.5** Per-version adapters - *for the three messages that make load management work*:
      `SetChargingProfile`, `ClearChargingProfile` and `GetCompositeSchedule`, on **all three
      versions** (`src/smart_charging/ocpp_1_6.rs`, `ocpp_2_0_1.rs`, `ocpp_2_1.rs`), each with a
      protocol-agnostic handler underneath (`handlers.rs`) that decides the outcome against the
      real store *before* dispatching, so the CSMS's status is what actually happened.

      Version-specific work, rather than three copies of one mapping: **1.6J** has no EVSE
      concept, so its flat `connectorId` resolves through `crate::topology` to the owning EVSE
      (the same documented reduction the other 1.6J handlers make) and its single schedule per
      profile means no rate unit to choose between; it is also the one version whose
      `transactionId` is already an integer, so `TxProfile` matching is exact rather than a parse
      of a free-form string. **2.0.1** lacks `PriorityCharging` entirely (it reports as a plain
      `TxProfile` - lossy, tested as such) and its period `limit` is mandatory, so no period is
      ever dropped. **2.1** allows a period with no `limit` at all (its DER cases), which is
      dropped rather than invented, and its dynamic-schedule/price-schedule fields are read past
      rather than half-interpreted.

      One cross-version rule earns its own mention: an `Absolute` schedule with no
      `startSchedule` (1.6J permits it) is stamped with the instant the profile *arrived*, in the
      handler, since re-anchoring at every evaluation would make a schedule with a duration
      restart forever instead of ending.

      **Not done, and each needs its own slice**: `NotifyChargingLimit`/`ClearedChargingLimit`,
      and `NotifyEVChargingNeeds`/`NotifyEVChargingSchedule` (which additionally needs
      EV-supplied ISO 15118 data this crate has no hardware binding for - see
      [B4.5](#b4--certificates-and-iso-15118-r1-r13)).
- [x] **B2.7** `GetChargingProfiles`/`ReportChargingProfiles` on **2.0.1 and 2.1** - the CSMS
      asking what is installed, answered from the store across as many `ReportChargingProfiles`
      messages as it takes. 1.6J has no such message and no way to ask at all.

      `ChargingProfileQuery` is deliberately *not* `ChargingProfileCriteria`: OCPP's get-criterion
      matches a **list** of ids and a list of limit sources where clear matches at most one id and
      no source, and sharing one type would widen the clear path to fields it must never act on -
      clearing by a criterion the CSMS did not send is destructive in a way over-reporting is not.

      Every stored profile reports `chargingLimitSource: CSO`, which is a fact rather than a stub:
      `SetChargingProfile` is the only way a profile gets here, and that is the operator talking
      through the CSMS. The other sources exist so a CSMS filtering on `EMS` is told there are
      none rather than handed all of them. Chunking groups by scope **and** source, because the
      message carries one `evseId` and one `chargingLimitSource` for everything in it, and `tbc`
      clears only on the last message of the whole report - clearing it at the end of each scope
      would tell the CSMS the report had finished while another scope was still coming. An empty
      match sends **no** report at all (the `NoProfiles` status already says so), which is the
      opposite of [`chunk_report`]'s empty `NotifyReport` - and the difference is OCPP's:
      `GetBaseReport` has no "nothing matched" status to answer with.

      Two known limits. The reports are sent from inside the request handler, so they reach the
      CSMS *before* the response it answers - the transport only writes the response once the
      handler returns, and there is no executor at that boundary to defer them onto. `requestId`
      is what correlates the two, and `NotifyReport` has had the same ordering for the same reason
      since it was written. And 2.1's generated `ChargingProfile` is **56 KB by value** (see
      [D2.3](#62-d2--type-completeness-audit)), so building one to send is expensive in a way that
      matters on an MCU.
- [x] **B2.6** 2.1 dynamic schedule updates and priority charging.

      **The upstream blocker this row carried was stale.** `UpdateDynamicSchedule` was marked 🔒
      on [D1](#61-d1--missing-action-wrappers), but the pinned `ocpp-client` is now **0.2.2**, not
      0.2.0 — it generates **91** OCPP 2.1 actions, including all four that were listed as absent
      (`UpdateDynamicSchedule`, `SetDisplayMessage`, `GetDERControl`, `SetDERControl`) and 2.0.1's
      `SecurityEventNotification`. D1.1/D1.2 closed that and the per-row markers were never
      re-swept. Neither [B6](#b6--display-message-r15) nor
      [B8.2](#b8--reservation-derv2x-battery-swap) is blocked either.

      **Priority charging.** `UsePriorityCharging` inbound and `NotifyPriorityCharging` outbound,
      plus the composition gate that makes them mean anything.

      The gate is the part that was a real defect rather than a missing message:
      `PriorityCharging` profiles applied to *any* running transaction the moment they were
      installed, because `applies_to_transaction` treated the purpose exactly like `TxDefault`. A
      priority profile is a **grant**, not another stack level - installing one now changes
      nothing until the CSMS names a transaction, which `CompositionContext::priority_charging`
      and `Transaction::priority_charging` carry. The grant lives on the transaction, so it ends
      with the session rather than leaking into whatever plugs in next, and `#[serde(default)]`
      makes a session persisted before the field existed recover as *ungranted* - the safe
      reading, since the CSMS can no longer see a priority it never re-granted.

      Two decisions worth recording. `NoProfile` is kept distinct from `Rejected` because it is
      the one refusal the CSMS can act on (install a priority profile, then ask again), and
      **deactivation never answers `NoProfile`**: the profile may have been cleared while the
      grant stood, and refusing the withdrawal would leave the CSMS believing a transaction still
      holds a priority it does not. A grant the CSMS *asked* for produces no
      `NotifyPriorityCharging` - reporting a change back to the peer that requested it is noise -
      so the effect fires only for `locally_initiated` changes, exactly the split
      [B8.1](#b8--reservation-derv2x-battery-swap)'s `ReservationEnded` makes.

      **Dynamic charging profiles (OCPP K28)**, implemented against the vendored 2.1 spec's
      K28.FR.01–.15 rather than from the message list: `ChargingProfileKind::Dynamic`,
      `dynUpdateInterval`/`dynUpdateTime`, `UpdateDynamicSchedule` inbound, and a sweep that pulls
      with `PullDynamicScheduleUpdate` for every profile whose own interval has come round.

      A dynamic profile inverts what a charging profile *is*. There is no curve laid out in
      advance - one schedule, one period, starting at 0 (K28.FR.01/.02, enforced on install and
      refused with `reasonCode = "InvalidSchedule"` rather than trimmed to fit, because silently
      dropping periods would apply a limit the CSMS never asked for). Its limit is replaced as it
      goes, in place, by either direction of the same mechanism, which is why one
      protocol-agnostic handler serves both.

      **`dynUpdateTime` is the interesting field, and it does two jobs.** It is the schedule's
      anchor - a dynamic period is active from the moment it arrives, so there is no
      `startSchedule` to measure from, and the charge point stamps it on install (K28.FR.05) and
      on every update (K28.FR.09). It is also the clock a **dead-man's switch** runs on: with a
      `duration` set, a profile whose CSMS stops answering stops applying (K28.FR.13/.15) and
      composition falls through to the next valid profile, with a later update reviving it
      (K28.FR.14) without a reinstall. That makes `duration` mean something quite different on a
      dynamic profile than on a scheduled one - not "when the curve runs out" but "how long one
      pushed limit may be trusted unrefreshed" - and it is the difference between a CSMS outage
      being survivable and it freezing a stale limit onto a connector indefinitely. Computed
      rather than latched, so revival needs no extra event. A refused or empty pull deliberately
      does **not** stamp the timestamp: a CSMS answering `Rejected` forever must not keep a stale
      limit alive by replying at all.

      **What the wire carries that this crate does not.** 2.1's `ChargingScheduleUpdate` also has
      setpoints, discharge limits, reactive setpoints and per-phase `_L2`/`_L3` variants of all of
      them. Every one needs a hardware capability `crate::hardware` cannot express -
      `Connector::set_current_limit` takes a single import limit, with no hook for discharging, a
      setpoint, or driving phases asymmetrically. They are counted, logged and dropped rather than
      stored: keeping values nothing can act on would report a compliance this charge point does
      not have. The gap closes with [B8.2](#b8--reservation-derv2x-battery-swap)'s
      bidirectional-power hardware surface.

      The pull sweep **skips entirely while the clock is unsynchronized**, the same stance
      `run_reservation_expiry` takes: due-ness is `dynUpdateTime + dynUpdateInterval` against now,
      and a charge point that does not know the time would either never pull or storm the CSMS.
      Its interval is how often due-ness is *checked*, not how often a pull happens - each profile
      carries its own, and a charge point with no dynamic profiles installed makes no requests at
      all.

      Registered from `connect.rs`'s 2.1 path rather than `setup()`, because `setup()` is generic
      over a CSMS client that may equally be a 2.0.1 one and a 2.1-only bound there would make the
      whole "everything on" wrapper unusable on 2.0.1 - that is the coupling C4's builder exists
      to avoid. [`ChargePointBuilder::priority_charging`](../src/builder.rs) and
      `dynamic_charging_profiles` are the general entry points.

      **Version projection.** 1.6J and 2.0.1 have neither the kind nor the messages, so their
      adapters never produce a dynamic profile; reporting one installed over a 2.1 connection
      degrades it to `Relative`, which is the closest honest answer (both are anchored to when
      they arrived rather than to a wall-clock start) and the same documented loss `wire_purpose`
      already takes for `PriorityCharging`.

### B3 — Firmware management (R§12)

| Message | 1.6J | 2.0.1 | 2.1 |
|---------|:----:|:-----:|:---:|
| UpdateFirmware | ✅ | ✅ | ✅ |
| FirmwareStatusNotification | ✅ | ✅ | ✅ |
| PublishFirmware / UnpublishFirmware / PublishFirmwareStatusNotification | — | ⬜ | ⬜ |

- [x] **B3.1** File-transfer abstraction in `crate::hardware` — `FileTransfer`, with
      `TransferProgress`/`TransferReport`, `UploadSource`, `LogKind` and a `NoFileTransfer`
      fallback. **Shared with [B5](#b5--diagnostics-and-monitoring-r14)**, which is why it is
      general rather than firmware-shaped.

      **Why it is hardware surface at all**, when `CLAUDE.md` sends every other network concern to
      `ocpp-client`: a file transfer is not an OCPP concern. OCPP hands over a bare URL and says
      nothing about how to fetch it — the scheme is whatever the operator deployed (HTTPS, FTP/S,
      SFTP, a vendor's own), and the credentials, trust store and interface are the integrator's.
      A charge point that hard-coded one HTTP client could not talk to half the deployments that
      exist.

      **No content crosses the boundary on the way down.** `download` returns `Ok(())`, not bytes.
      A firmware image is megabytes against an MCU's kilobytes of RAM, so buffering one to hand
      back would be impossible on the target hardware and pointless anyway — the only thing this
      crate would do with it is give it straight back to be flashed. That is also why
      [`Storage`](#71-e1--storage-trait) is not reused: its values are `Vec<u8>`, the wrong shape
      for something that must never be resident in full.

      **Upload is asymmetric, and `UploadSource` says why.** The security log is *this crate's* —
      bounded, in `SecurityEventLog`, and unproducible by an integrator who is never told a
      security event happened — so it goes out as `Bytes`. A diagnostics log is the integrator's:
      this crate cannot know what a given station considers diagnostic output, and it may be far
      too large to hold. So one arrives as bytes and the other as a name.

      Two contracts stated rather than left implicit: **retries belong to the caller** (OCPP
      carries `retries`/`retryInterval` on the requests that start a transfer, so honouring them
      in the implementor too would multiply out to a count no CSMS asked for), and an
      implementation must be **safe to cancel at an await point** — a CSMS may supersede a
      transfer, which 2.1 names `AcceptedCanceled` — without leaving a half-written artifact a
      later download would mistake for complete.

      `NoFileTransfer` fails rather than succeeding silently, on the same reasoning as
      [`NoStorage`](#71-e1--storage-trait): a charge point reporting `Downloaded` without having
      downloaded anything leaves a CSMS about to install an update that does not exist.

      **Not yet wired to anything** — by design, since its two consumers are the next two tasks.
      A streaming implementor in the tests exercises the shape end to end (chunked reads, an await
      between chunks, progress out, content retained) so the ergonomics are proven rather than
      assumed.
- [x] **B3.2** Firmware state machine, on all three versions, implemented against the vendored
      spec's L01/L02 requirements rather than from the message list.

      `UpdateFirmware` is answered immediately and the update runs on a worker, for the reason
      [B5.1](#b5--diagnostics-and-monitoring-r14) does the same: an update takes minutes and the
      response has to go out first. Every state change is reported (L01.FR.01), all carrying the
      request id that started it (L01.FR.10).

      **Both scheduling points are honoured and announced.** A `retrieveDateTime` in the future
      reports `DownloadScheduled` (L01.FR.13) — a CSMS that heard nothing could not tell a
      scheduled update from a lost one — and an `installDateTime` in the future reports
      `InstallScheduled` *after* the download completes (L01.FR.16), so a late install does not
      hold back the fetch. **An unsynchronized clock treats every schedule as due now**: a charge
      point that cannot know the instant arrived would otherwise never start, and an update that
      never happens is worse than one that happens early — the CSMS asked for it either way.

      **The transaction wait changes availability, and that is L01.FR.07 rather than a
      flourish.** Installation waits for running transactions to end (L01.FR.06), and while
      waiting every EVSE is held `Unavailable` unless the CSMS set
      `AllowNewSessionsPendingFirmwareUpdate` — otherwise a charge point about to reboot keeps
      accepting drivers it is about to cut off. Availability is restored if the install *fails*,
      so a failed update does not leave a station silently out of service, and deliberately **not**
      restored before a reboot, where re-opening for a session the restart would kill is worse
      than coming back up unavailable.

      `InstallRebooting` is reported *before* the reboot (L01.FR.15), because afterwards there is
      no process left to report anything, and the reboot goes through the existing `Reset` path
      rather than a parallel one — that path already drives every connector through the fail-safe
      stop, and `Immediate` is safe there precisely because the transaction wait established there
      is nothing running.

      A second `UpdateFirmware` supersedes the first and answers `AcceptedCanceled` (L01.FR.24),
      with the same monotonic-ticket reasoning as B5.1. `FirmwareUpdateState::triggered_status`
      answers a `TriggerMessage`: `Idle` once the last update installed (L01.FR.25), the last
      status otherwise (L01.FR.26), and `Idle` is the one status sent without a request id
      (L01.FR.20).

      **New hardware surface:** `hardware::FirmwareInstaller`, separate from `FileTransfer`
      because installing is not transferring — a charge point may be able to fetch a file and
      unable to flash one, and the two live in different parts of an integrator's stack. Its
      `RebootRequired` outcome is what lets this crate report `InstallRebooting` before issuing
      the reboot; an implementor must not reboot inside `install`.

      **1.6J is genuinely poorer, and every projection picks the status that is *true* rather than
      convenient.** Its response is `{}` — no status field at all, so acceptance is invisible and a
      refusal shows up only as the notifications that do not follow. It has no request id, no
      `installDate`, and seven statuses rather than fourteen: `DownloadScheduled` → `Idle`
      (nothing has started; `Downloading` would claim a transfer that has not begun),
      `InstallScheduled` → `Downloaded` (exactly the state it is in), `InstallRebooting` →
      `Installing` (`Installed` would have a CSMS record a version this charge point is not
      running). 1.6J's plain `UpdateFirmware` is also unsigned by construction — signing arrives
      with the Security Whitepaper's `SignedUpdateFirmware`, which `ocpp-types` does not generate
      ([D2.2](#62-d2--type-completeness-audit)).

      **Still open, and honestly so:** signature/certificate verification is
      [B3.3](#b3--firmware-management-r12) — it needs crypto and a trust store this crate has no
      hook for, so `signature`/`signingCertificate` are carried through to the integrator untouched
      and `InvalidCertificate` is never returned, since answering it without having checked would
      be a lie in the dangerous direction. Reporting `Installed` *after* a reboot needs a marker
      that survives the restart, the same shape as `BootReasonStore`.

      Registered through `ChargePointBuilder::firmware_updates` only — it needs both halves of the
      firmware hardware surface, which `setup()` has no way to receive.
- [ ] **B3.3** Signed firmware verification (2.x `signingCertificate` /
      `signature`; 1.6J security whitepaper `SignedUpdateFirmware`), driving
      the `InvalidFirmwareSignature` / `InvalidFirmwareSigningCertificate`
      security events.
- [ ] **B3.4** Local-controller firmware publishing (2.x only).

### B4 — Certificates and ISO 15118 (R§1, R§13)

| Message | 1.6J | 2.0.1 | 2.1 |
|---------|:----:|:-----:|:---:|
| InstallCertificate / DeleteCertificate / GetInstalledCertificateIds | — | ✅ | ✅ |
| CertificateSigned / SignCertificate | — | ⬜ | ⬜ |
| GetCertificateStatus | — | ⬜ | ⬜ |
| GetCertificateChainStatus | — | — | ⬜ |
| Get15118EVCertificate | — | ⬜ | ⬜ |

- [x] **B4.1** Certificate store abstraction — `hardware::CertificateStore`, with
      `CertificateUse`, `CertificateHashData`, the two outcome enums, a `NoCertificateStore`
      fallback and `StoredCertificates` over [E1](#71-e1--storage-trait)'s `Storage`.

      **The secure-element note in this row's own text is what shaped it.** A secure element holds
      a private key and will not give it back — it signs on request and the key never leaves the
      chip. Any design where this crate reads a key out of storage and hands it to a TLS stack has
      already given that up. So the store is a **trait the integrator implements**, and the crate
      never sees a private key at all: the only question it asks is `has_private_key()`, a yes/no,
      because security profile 3 needs to know whether a client certificate *can* be presented and
      nothing above needs the key itself.

      `StoredCertificates` is the ready-made implementation over `Storage` for the many charge
      points with no secure element — which is the sense in which B4.1 "depends on E1". It answers
      `has_private_key()` **false**, and that is not a stub: a key in ordinary flash is a key an
      attacker holding the flash has, so a station needing profile 3 wants a secure-element-backed
      implementation of the trait instead. It is bounded (G2.2, 10 by default), replaces rather
      than duplicates on reinstall, survives a reboot, and comes up empty on a corrupt index rather
      than refusing to come up — a charge point that cannot read its certificates should let the
      CSMS reinstall them.

      Certificates are addressed by **hash**, not by a local id, because that is how
      `DeleteCertificate` and `GetInstalledCertificateIds` address them; inventing an id would only
      have to be mapped back. The hashes are computed by whoever parses the certificate, since that
      means parsing X.509 and hashing DER — crypto this crate does not have. `StoredCertificates`
      therefore refuses the bare `install` (a certificate with no hash data would be unaddressable
      and so undeletable, which is worse than refusing it) and offers `install_with_hash` for an
      integrator that can parse but has no secure element.

      The charge point's **own** certificates (`ChargingStation`, `V2GCertificateChain`) are
      listable but not installable: they arrive by `CertificateSigned` in answer to a CSR, so
      accepting one through `InstallCertificate` would mean accepting a certificate for a key pair
      this charge point may not hold.

      **Unblocks** [B4.2](#b4--certificates-and-iso-15118-r1-r13)–B4.4,
      [E2.9](#72-e2--what-must-survive), [F1.3](#81-f1--security-profiles),
      [F2.2](#82-f2--tls) and [F3.2](#83-f3--credentials). Nothing wires it yet — the messages are
      B4.2.
- [x] **B4.2** Install / delete / enumerate, per certificate-use type — `InstallCertificate`,
      `DeleteCertificate` and `GetInstalledCertificateIds` on 2.0.1 and 2.1, answered from
      [B4.1](#b4--certificates-and-iso-15118-r1-r13)'s store.

      **2.x only**: 1.6J's certificate messages live in the Security Whitepaper, which
      `ocpp-types` does not generate ([D2.2](#62-d2--type-completeness-audit)) — the same reason
      `SecurityEventNotification` is 2.x-only here.

      Each handler is a thin decision over the store, because that is where the work belongs:
      parsing X.509, computing hashes and holding keys are the integrator's, possibly a secure
      element's. What this block adds is the OCPP-shaped part — and three of those shapes are
      decisions rather than transcription:

      - **An absent capability answers `DeleteCertificate` with `NotFound`, not `Failed`.** A
        charge point with no store genuinely does not have the certificate; `Failed` would send an
        operator looking for a fault that isn't there.
      - **Holding no certificates is `NotFound`, not an empty list.** That is OCPP's way of saying
        "none", and sending an empty list alongside it would be a second way of saying the same
        thing.
      - **A hash too long for the wire drops its whole entry** rather than being truncated. A
        truncated issuer hash identifies a *different* certificate, and a CSMS acting on it would
        ask to delete the wrong one.

      The charge point's own certificates are refused at this layer too, not just in the store:
      they arrive by `CertificateSigned` in answer to a CSR, so `InstallCertificate` accepting one
      would mean accepting a certificate for a key pair this station may not hold.

      **Version difference:** 2.0.1's enums have no `OEMRootCertificate` (2.1 added it), so an OEM
      root installed over 2.1 is reported to a 2.0.1 CSMS as the manufacturer root — both are "a
      root this station's maker put here", which is the closest 2.0.1 can name. A documented loss,
      like `wire_purpose`'s `PriorityCharging`.

      New `certificate_management` capability and Cargo feature, registered through
      `ChargePointBuilder::certificates`. Chained through `CAPABILITY_GATES` with
      `has_handler: false`, since `setup()` cannot receive a store — C3.5's data-driven test
      enforced that rather than letting it slide.
- [ ] **B4.3** CSR generation and `SignCertificate` → `CertificateSigned`
      round trip, including automatic renewal before expiry.
- [ ] **B4.4** OCSP status checking.
- [ ] **B4.5** ISO 15118 Plug & Charge — gate behind a feature flag *and* a
      runtime capability; most chargers don't have it.

### B5 — Diagnostics and monitoring (R§14)

| Message | 1.6J | 2.0.1 | 2.1 |
|---------|:----:|:-----:|:---:|
| GetDiagnostics / DiagnosticsStatusNotification | ✅ | — | — |
| GetLog / LogStatusNotification | — | ✅ | ✅ |
| SetVariableMonitoring / ClearVariableMonitoring | — | ⬜ | ⬜ |
| SetMonitoringBase / SetMonitoringLevel | — | ⬜ | ⬜ |
| GetMonitoringReport / NotifyMonitoringReport | — | ⬜ | ⬜ |
| NotifyEvent | — | ⬜ | ⬜ |
| CustomerInformation / NotifyCustomerInformation | — | ⬜ | ⬜ |
| GetTransactionStatus | — | ⬜ | ⬜ |
| Open/Close/Adjust/Get PeriodicEventStream, NotifyPeriodicEventStream | — | — | ⬜ |

- [x] **B5.1** Log upload — `GetLog` (2.x) and `GetDiagnostics` (1.6J) end to end, on top of
      [B3.1](#b3--firmware-management-r12)'s `FileTransfer`. Implemented against the vendored
      spec's N01.FR.01–.17.

      **The upload cannot happen in the handler**, which shaped the design. N01's sequence is:
      respond `Accepted` *first*, then report `Uploading`, then upload, then report `Uploaded`. A
      handler doing the transfer before returning would hold the response until a multi-megabyte
      upload over a slow link finished — by which point the CSMS has timed out, and the status
      notifications would arrive *after* the response they are meant to follow. So the handler does
      only what is immediate (decide, name the file) and hands the work to `run_log_uploads`, a
      worker the builder spawns: the same actor discipline as the rest of the crate, where the
      decision is synchronous and the work is a message.

      **`UploadSource` earns its split here.** A `SecurityLog` request renders
      `SecurityEventLog` to bytes and hands them over; a `DiagnosticsLog` request passes a name and
      the integrator streams its own. That is B3.1's asymmetry doing exactly the job it was shaped
      for.

      **What "cancel" can honestly mean** (N01.FR.12): a second `GetLog` answers
      `AcceptedCanceled` and *supersedes* the running upload — its result is discarded rather than
      reported, so the CSMS never sees a stale `Uploaded` for a request it was told was replaced —
      but the transfer is not aborted mid-flight. Aborting needs a `select!` racing the transfer
      against a cancel signal, which this crate does not have on both `no_std` and std (the same
      constraint `run_charging_limit_projection` documents). The supersede is checked at retry
      boundaries. A monotonic ticket rather than a flag, because a flag cannot distinguish
      "someone replaced me" from "someone replaced the one before me".

      Two smaller decisions. **The log is rendered once, before the retry loop** — re-rendering
      between attempts would silently change the file's contents depending on how many times the
      network failed, when what the CSMS asked for was a snapshot. And **an entry with no
      timestamp is kept** when the CSMS narrows the window: it cannot be shown to fall outside
      one, and dropping events a charge point could not time is the wrong bias for a security log,
      where the period around an unset clock is exactly when something interesting happened.

      Format: OCPP explicitly does not prescribe one ("The format of this log file is not
      prescribed"), so this is a decision — tab-separated `timestamp / type / techInfo`, one line
      per event, oldest first. `techInfo` is free-form text off the wire, so tabs and newlines in
      it are replaced rather than escaped: a newline there would otherwise **invent a security
      event that never happened**.

      **Version differences, all one-directional.** 1.6J has no log type (so a 1.6J CSMS cannot
      request the security log at all), no `requestId` to correlate with, and no
      `AcceptedCanceled`; 2.0.1 has no `DataCollectorLog` and no `statusInfo` on the notification.
      2.x's four failure statuses collapse to `UploadFailure`, because distinguishing them would
      mean the `FileTransfer` implementor classifying its own error against a protocol enum — a
      detail B3.1 deliberately keeps off the hardware surface.

      `FileTransferProtocols` (a Required device-model variable) stays empty, and the reason
      changed rather than went away: file transfer now exists, but *which protocols* it speaks is a
      fact about the integrator's binding, so naming HTTP here would advertise something this
      crate neither implements nor can verify.

      Registered through `ChargePointBuilder::log_uploads` only, **not** `setup()` — it needs a
      `FileTransfer` binding `setup()`'s signature has no way to receive, exactly the position
      `Storage` is in. The `diagnostics` capability gate is therefore `has_handler: false`, which
      C3.5's data-driven test enforced rather than let slide.
- [ ] **B5.2** Variable monitoring engine: thresholds, deltas, periodics on
      device-model variables → `NotifyEvent`.
- [ ] **B5.3** Monitoring report generation, chunked like `NotifyReport`
      already is.
- [x] **B5.4** `GetTransactionStatus` (2.x only — 1.6J has no such message) —
      implemented against the vendored spec's E14 "Check transaction status"
      use case and its E14.FR.01–.08.

      **Two questions, not one.** A request naming a `transactionId` asks
      whether *that* transaction is ongoing and whether messages about it are
      queued; a request naming none asks only whether anything at all is
      queued (E14.FR.06–.08 — `ongoingIndicator` must then be *absent*, not
      `false`). `handle_get_transaction_status` takes an
      `Option<TransactionId>` through to the response rather than collapsing
      the two shapes.

      **A finished transaction and an unknown one answer identically, on
      purpose.** E14.FR.01/.03 both require `ongoingIndicator = false`, and
      E14's own remarks say the CSMS isn't meant to tell the two apart. This
      crate's state already matches that for free:
      `advance_transaction`/`ChargePointState` clears a connector's
      transaction slot the moment its `Ended` event fires, so "not present in
      any slot" already means both things at once — no graveyard of finished
      transaction ids needed for a distinction the spec doesn't grant anyway.

      **`messagesInQueue` reads the real backlog.** `ChargePointBuilder` now
      keeps the `Arc<OfflineQueue<TransactionEventOccurred>>`
      `transaction_events`/`transaction_events_persisted` creates (previously
      local to those methods), so `get_transaction_status` can answer from
      the actual queue rather than a fabricated `false`. Registering
      `get_transaction_status` without either of those first still answers
      correctly — `false`, because nothing is ever queued through a queue
      that doesn't exist — just less usefully.
- [ ] **B5.5** Customer information / GDPR erasure.
- [ ] **B5.6** 2.1 periodic event streams.

### B6 — Display message (R§15)

`SetDisplayMessage`, `GetDisplayMessages`, `ClearDisplayMessage`,
`NotifyDisplayMessages` — 2.x only, all ⬜. `SetDisplayMessage` for 2.1 was
marked 🔒 on [D1](#61-d1--missing-action-wrappers); it no longer is. The pinned
`ocpp-client` 0.2.2 generates it (see [B2.6](#b2--smart-charging-r11) for the
full re-sweep), so this block is unblocked, just unwritten.

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

- [x] **B8.1** `ReservationStatusUpdate` (2.x) — the reservation block is now complete.

      Reservations end three ways and only two are reported: the CSMS cancelling one needs no
      report (it asked), and a cable arriving is the reservation being *honoured* rather than
      failing. What is left is `Expired` and `Removed` — the latter being the charge point giving
      up on a connector it can no longer hold, which faulting or being made unavailable is. Both
      come out of `apply_connector_event`, where the reason a reservation slot was cleared is
      still known, as a `ChargePointEffect::ReservationEnded` on its own broadcast channel.

      **The expiry sweep came with it**, and had to: `Reservation`'s own docs recorded that a
      reservation only ended on cancellation or a cable, so an expiry the CSMS was counting on
      never happened and the connector stayed held indefinitely. `run_reservation_expiry` closes
      that. It **skips entirely while the clock is unsynchronized**
      (`clock::is_synchronized`) — a charge point that does not know the time cannot know a
      reservation lapsed, and holding a connector too long is recoverable where releasing a valid
      reservation is not. Relying on an unset RTC reading near the epoch to make nothing expire
      would be relying on which way a broken clock happens to be broken.

      A failed report is logged and dropped rather than queued, deliberately: delivered after an
      outage it would tell the CSMS about a reservation that lapsed an unknown time ago, on a
      connector whose real state the CSMS has since re-learned from the queued, ordered
      `StatusNotification` behind it.

      **Not done:** 2.1's third status, `NoTransaction` (the reservation was honoured but no
      transaction followed). Knowing it needs a timer running from the moment the cable arrives,
      and OCPP names no duration for it — so nothing maps to it rather than something being
      stretched to fit. 1.6J has no `ReservationStatusUpdate` at all, so a 1.6J CSMS learns a
      reservation ended only from the connector's `StatusNotification`; that is a version
      difference, not a gap here.
- [ ] **B8.2** DER control (2.1): `ClearDERControl`, `ReportDERControl`,
      `NotifyDERAlarm`, `NotifyDERStartStop`, `AFRRSignal`, plus
      `GetDERControl`/`SetDERControl` (no longer 🔒 — `ocpp-client` 0.2.2 generates both; see
      [B2.6](#b2--smart-charging-r11)). Feature-flagged; needs bidirectional
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
      and (now) 24-trait bound, and is now just the builder chain with every block
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

- [ ] **D2.3** `ocpp-types` v21's `ChargingProfile` is **56 KB by value** (`ChargingSchedule`
      alone is 18.6 KB, and a profile inlines three of them); 2.0.1's equivalent is 2.6 KB. The
      cause is `ChargingSchedule` inlining `AbsolutePriceSchedule`, `PriceLevelSchedule` and
      `SalesTariff` at their `heapless` capacities rather than behind a `Box`, in a build that
      has `alloc` anyway. Measured, not estimated, while wiring
      [B2.7](#b2--smart-charging-r11): constructing one to send overflows an unoptimised 2 MB
      worker stack (`tests/get_charging_profiles.rs` raises it and says why), and a release build
      is fine only because the temporaries are elided. This is a genuine obstacle to the no_std
      goal - an MCU whose entire stack is 64 KB cannot build a single one of these - and the fix
      is upstream: box the optional price/tariff sub-structures. Affects every 2.1 outbound
      message carrying a profile, not just this one.

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
| Device model attributes marked `persistent` | Already flagged in the model, now acted on | R§2 |
| Local authorization list + version number | Re-download after every boot is unacceptable offline | R§4 |
| Authorization cache | Offline authorization survives a reboot (`persistence::AuthorizationCacheStore`; `ChargePointBuilder::authorization_cache_persistence`) | [B1.2](#b1--core-spine-must-be-complete-for-any-production-deployment) |
| Reservations | Survive a reboot inside the reservation window | R§8 |
| Charging profiles | Load limits survive a reboot (`persistence::ChargingProfileSnapshotStore`; `ChargePointBuilder::charging_profile_persistence`) | [B2.1](#b2--smart-charging-r11) |
| Offline message queue | All three queues now durable (`src/persistence.rs`; `ChargePointBuilder::transaction_events_persisted` / `status_notifications_persisted` / `security_events_persisted`) | [G2](#92-g2--bounded-memory) |
| Certificates and keys | Security profile 2/3 | [B4.1](#b4--certificates-and-iso-15118-r1-r13) |
| Security event log | Durable and size-bounded (`src/security.rs`, `src/persistence.rs`; `ChargePointBuilder::security_log_persisted`) | [F4](#84-f4--security-events) |
| Network profiles | Recover connectivity after a bad profile switch — **now unblocked**: [B1.8](#b1--core-spine-must-be-complete-for-any-production-deployment)'s slot store exists | [A9](#3-workstream-a--transport-negotiation-connection-lifecycle) |
| Boot reason | `BootNotification.reason` must distinguish power-up from a commanded reset | R§2 |

- [x] **E2.1** In-flight transaction — `src/persistence.rs`. Each connector's
      transaction is written through `hardware::Storage` as a versioned JSON
      record (id, id token, charging state, `seqNo`, last meter reading, plus a
      `started_at` stamp and `meter_start` baseline that live on the record
      rather than on `Transaction`, so the state machine stays clock-free).
      Wired via `ChargePointBuilder::transaction_persistence`.
- [x] **E2.2** Transaction sequence numbers / `seqNo` — carried on the same
      record, so a recovered transaction's closing event continues the sequence
      instead of restarting it. The transaction-id counter is persisted
      separately (`ocpp-cp/txn/next-id`), written *before* the transaction that
      consumed it, so a cut between the two can only skip an id, never reuse one.
- [x] **E2.8** Offline message queue — `QueueStore`/
      `run_persisted_offline_queue` in `src/persistence.rs`, built on top of
      `offline_queue::OfflineQueue`'s new `snapshot`/`restore_backlog`. A
      queue's backlog is written as one whole-queue JSON snapshot per storage
      key (not one record per message — an `OfflineQueue` has a single logical
      owner, so there's no per-entry addressing to gain the way
      `TransactionStore` needs per-connector keys), versioned with its own
      `persistence::QUEUE_SCHEMA_VERSION`, discarded on decode failure or a
      mismatched version exactly like `PersistedTransaction`. **All three
      queues are now wired up** — `transaction_events_persisted`,
      `status_notifications_persisted` and `security_events_persisted` on
      `ChargePointBuilder`. The `QueueStore`/`run_persisted_offline_queue`/
      `restore_offline_queue` machinery is generic over any message type with a
      `serde`-able representation (`P: From<M> + Serialize + DeserializeOwned`,
      `M: From<P>`); each queue supplies a hand-written mirror type, since the
      message types carry `crate::state` enums that aren't `serde`-derived:
      `PersistedQueuedTransactionEvent` (mirroring `TransactionEventKind`),
      `PersistedQueuedStatusChange` (mirroring `ConnectorStatus`'s 5 and
      `ConnectorState`'s 13 variants) and `PersistedQueuedSecurityEvent`
      (mirroring `SecurityEventType`'s 18 unit variants plus its
      `Other(String)` payload variant). Every mirror's `From` impls match
      exhaustively with no catch-all arm, so adding a variant to any of those
      state enums is a compile error at the mirror rather than a silent
      data-loss bug, and a round-trip test drives every variant of each.

      Per-queue behaviour differences are preserved across the persisted
      variants, and each is a correctness point rather than a detail: the
      status and security queues keep the default `OverflowPolicy::DropOldest`
      (only the transaction queue opts into `DropNewest`); the status queue
      still sends through `DedupedStatusNotifier`; and the security queue's
      overflow callback still deliberately does *not* raise `MemoryExhaustion`,
      since doing so from the security-event queue's own overflow feeds back
      into that same queue (G2.1) — a regression test now guards that
      specifically.

      **A latent bug this slice had to fix first.** Persisting the status queue
      was pointless while `DedupedStatusNotifier::notify_status`
      (`src/availability.rs`) wrote its `last_sent` cache *before* awaiting the
      inner notifier and never rolled the entry back on failure. Composed with
      `flush_offline_queue` — which peeks the front message, sends, and pops
      only on `Ok` — the first failed attempt cached the status as sent, so
      every retry of that same queued message was judged a duplicate, returned
      `Ok(())` without sending, and was popped as delivered. The
      send-fails-then-retry path is the only reason the queue exists, so the
      bug fired precisely when the queue mattered, silently and with no error
      surfaced; it affected the plain `status_notifications` path too, not just
      the persisted one. The cache is now written only after `inner` accepts
      the notification. The remaining window — two concurrent same-connector
      calls both seeing "not cached" and both sending, since the
      `BlockingMutex`/`RefCell` guard cannot be held across the `.await` — is
      documented on the type rather than papered over; the current wiring (one
      forwarder task per queue) never produces it.

      Write policy: a whole-queue snapshot write is not free (its cost scales
      with the queue's current depth, unlike `TransactionStore`'s
      one-record-per-connector writes), so writes are debounced by mutation
      count rather than unconditional — `queue_persistence_decision` writes
      immediately on the queue's first message (a single queued message must
      never depend on a debounce window to become durable) and on draining
      back to empty (so a stale snapshot never "recovers" messages that were
      actually delivered), and otherwise only once
      `QueueStore::write_threshold` mutations (pushes or deliveries) have
      accumulated since the last write (default 1 — write on every mutation —
      overridable via `QueueStore::with_write_threshold` for integrators who'd
      rather trade a wider loss window for less flash wear during long,
      message-heavy outages, e.g. periodic-meter-reading `TransactionEvent`s
      queued back-to-back). A reconnect-triggered flush reconciles storage
      unconditionally rather than through the threshold, since a reconnect is
      a rare event, not the steady drumbeat the threshold exists to throttle.
      Capacity interaction (G2.1): a restored backlog is replayed through
      `OfflineQueue::restore_backlog`, which pushes each message through the
      queue's normal capacity/`OverflowPolicy` check — a persisted backlog
      that no longer fits (e.g. capacity was lowered since the snapshot was
      written) is trimmed by the same policy live traffic would be, logged
      rather than silently dropped.
- [x] **E2.3** Device model attributes marked `persistent` — `persistence::DeviceModelStore`/
      `restore_device_model`/`run_device_model_persistence` in `src/persistence.rs`, wired via
      `ChargePointBuilder::device_model_persistence`. Only attributes with
      `VariableAttribute::persistent == true` are written, as one whole-snapshot JSON record
      (`ocpp-cp/device-model`) rather than one key per attribute — the rest of the model is
      re-registered by the hardware binding on every boot (`ChargePoint::start`), so persisting it
      too would just be redundant writes of data about to be overwritten anyway.

      Write policy: `device_model_persistence_decision`, a skip-count threshold defaulting to `1`
      (write on every change), overridable via `DeviceModelStore::with_write_threshold`.
      `SetVariables` genuinely can be bursty (a single request may set several variables, or a
      script may issue several requests back to back) in a way `SendLocalList` and
      `ReserveNow`/`CancelReservation` aren't, which is why this concern gets a threshold knob and
      those two don't — but the default stays at `1` rather than defaulting higher: `SetVariables`
      is operator/CSMS configuration traffic, not a hot path driven by charging telemetry the way
      periodic meter values are (the one write this crate already debounces by *magnitude*, via
      `TransactionStore::meter_write_threshold_wh`), so a burst of a handful of variables is at
      most a handful of small JSON writes, not a sustained per-sample cadence.

      Ordering vs the hardware binding's own registration (the unregistered-variable decision):
      `restore_device_model` must run *after* the binding has finished registering its variables
      — `ChargePointBuilder::start` already waits for `ChargePoint::start` to return before any
      `*_persistence` method can be called, which is exactly that ordering. A persisted value only
      lands on a component/variable/attribute-type that's already registered this boot
      (`ChargePointEvent::PersistedDeviceModelAttributesRestored`'s handler reuses
      `DeviceModel::set_attribute_value`, which already no-ops for an unregistered target); one
      that isn't — e.g. a firmware downgrade or hardware reconfiguration that removed a variable —
      is left dormant and logged (`tracing::warn!`) rather than either applied blind or silently
      dropped with no trace. The binding is the source of truth for which variables exist this
      boot, never the persisted record.
- [x] **E2.4** Local authorization list + version number —
      `persistence::LocalAuthorizationListStore`/`restore_local_authorization_list`/
      `run_local_authorization_list_persistence`, wired via
      `ChargePointBuilder::local_authorization_list_persistence`. The whole list is written as one
      JSON snapshot (`ocpp-cp/local-auth-list`) — `SendLocalList` always replaces or resolves to
      the full resulting list in one call (`crate::local_authorization_list::handle_send_local_list`
      resolves a differential update before the state machine ever sees it), so there is no
      per-entry addressing to gain the way `TransactionStore` needs per-connector keys.

      Write policy: unconditional, on every version change — no threshold. `SendLocalList` is a
      rare, CSMS-initiated, operator-driven event, not a hot path; bounding writes here the way
      `persistence_decision` bounds meter-reading writes would be a knob nothing exercises.
- [x] **E2.6** Reservations — `persistence::ReservationStore`/`restore_reservations`/
      `run_reservation_persistence`, wired via `ChargePointBuilder::reservation_persistence`. The
      whole active set (at most one reservation per connector) is written as one JSON snapshot
      (`ocpp-cp/reservations`), for the same per-entry-addressing reasoning as E2.4. This slice
      also gave `Reservation` a real `expires_at: Option<DateTime<Utc>>` field (previously absent
      entirely), needed for the expired-reservation decision. `ReserveNow`'s wire
      `expiryDateTime` (2.x)/`expiryDate` (1.6J) is now threaded through to it —
      `reservation::handle_reserve_now` takes the parsed value and every version adapter in
      `src/reservation.rs` parses it from the wire request (`None` if it doesn't parse as RFC
      3339, treated as "never expires" like any other `None`).

      The expired-reservation decision: a reservation whose `expires_at` had already passed while
      the charge point was off is **not** resurrected as active. `restore_reservations` drops it,
      logs a warning, and never raises
      `ChargePointEvent::PersistedReservationsRestored` for it at all. The *live* expiry sweep that
      was recorded here as missing landed with
      [B8.1](#b8--reservation-derv2x-battery-swap): `reservation::run_reservation_expiry` releases
      a lapsed reservation on an `Executor`/`Backoff`-driven interval and the CSMS is told over
      `ReservationStatusUpdate`, so a reservation created and expiring within a single boot now
      ends on its own. Restoring an expired reservation
      anyway would leave the connector wrongly `Reserved` with nothing left to ever clear it. The
      check
      itself only fires when the supplied `Clock` looks synchronized
      (`crate::clock::is_synchronized`) — hardware with no RTC yet must not have every restored
      reservation discarded as "expired" purely because its unset clock reads before any real
      `expires_at`, the same G3.1 stance `persistence::next_record` already takes for
      `started_at`, applied here as "don't act on a clock reading we don't trust" instead of
      "don't record one". Storage is reconciled to hold only what was actually restored, so a
      stale expired entry isn't re-loaded and re-filtered on every subsequent boot.

      Write policy: unconditional, on every change to the active set — no threshold, for the same
      reasoning as E2.4. There is deliberately no tick-driven live expiry sweep to debounce
      against yet; if one is added later it should reuse `queue_persistence_decision`-style
      debouncing rather than writing on every tick, flagged here so that addition doesn't
      silently inherit an unconditional write policy that was only ever justified for discrete
      CSMS-initiated events.
- [x] **E2.12** Boot reason — `persistence::BootReasonStore` (`ocpp-cp/boot-reason`, one
      whole-record snapshot, same shape as E2.4/E2.6). What's persisted is not a log of every
      reboot, just the single cause of the *next* one: `crate::reset::handle_reset` writes it
      *before* the `ResetRequested` event that may produce an immediate `HardwareCommand::Reboot`
      is even sent to the actor, via a synchronous hook
      (`ChargePointActor::set_boot_reason_recorder`) installed by
      `ChargePointBuilder::boot_reason_persistence` — the one store in this module written inline
      rather than off a state-change subscription, specifically because "soon after" isn't good
      enough here: once `HardwareCommand::Reboot` reaches hardware there is no "after" left to
      still record why.

      The internal model (`state::BootReasonCause`, protocol-version-independent per `CLAUDE.md`)
      has exactly two variants — `RemoteReset` (from `ResetKind::Immediate`) and `ScheduledReset`
      (from `ResetKind::OnIdle`) — since a CSMS `Reset` is the only cause this crate can currently
      produce. Absence of a persisted record (nothing ever written, or a prior boot's cause
      already cleared after acceptance) is itself information: an *uncommanded* restart (power
      cut, watchdog, crash), which every version adapter maps to `BootReasonEnum::Unknown` rather
      than `PowerUp` — this crate cannot tell a clean power-up apart from a crash/watchdog reset
      without a hardware-supplied signal it doesn't have, and `Unknown` is the only variant that
      doesn't overclaim that knowledge. `ApplicationReset`/`LocalReset`/`FirmwareUpdate`/
      `Watchdog`/`Triggered` are never produced — nothing in this crate commands any of those
      today. OCPP 1.6J's `BootNotification.req` has no `reason` field at all (a 2.x addition), so
      this whole slice is a no-op there, by spec, not by omission.

      Clearing timing: the persisted cause is cleared by `ChargePointBuilder::provisioning` only
      once the CSMS has *accepted* the BootNotification carrying it — not before sending, and not
      immediately on boot. A crash between the reboot and a successful registration therefore
      still reports the same commanded cause on the *next* boot too, rather than wrongly falling
      back to "uncommanded" while still mid-recovery. The loaded cause is also fixed for the whole
      process's life once read — a later WS reconnect's resent BootNotification
      (`connection::reregister_on_reconnect`) reports the same reason, not a fresh storage read,
      since a reconnect is a connectivity event, not a new boot. A storage-less charge point (no
      `boot_reason_persistence` call, or `hardware::NoStorage`) now always reports the honest
      uncommanded-restart variant (`Unknown`) instead of the previously-hardcoded `PowerUp` —
      itself a small behavior change, but a strictly more honest one.
- [x] **E2.10** Security event log - `crate::security::SecurityEventLog` (the bounded in-RAM
      ring) plus `persistence::SecurityLogStore`/`restore_security_log`/
      `run_security_log_persistence`, wired via `ChargePointBuilder::security_log_persisted`.

      **Why this is a second consumer of the security-event broadcast, not a reuse of the
      existing one.** E2.8 already persists the offline *security-event queue*, and at a glance
      that looks like the same data. It isn't: the queue holds events **pending delivery** and
      drops each one the moment the CSMS accepts it, so on a healthy connection it is empty
      almost always - exactly when an operator most wants the history. `SecurityLogWasCleared`
      and `GetLog`'s `SecurityLog` upload both presuppose a record that outlives delivery, so
      the log takes its own `subscribe_security_events()` subscription (taken up front in
      `ChargePointBuilder::start`, like the four block subscriptions and the transaction
      persistence one, so an event raised during hardware start-up is logged rather than
      missed). Logging and delivery therefore cannot starve each other, and a charge point may
      register either, both, or neither.

      **Bounded, per G2.2**: a ring of `DEFAULT_SECURITY_LOG_CAPACITY` (50) entries by default,
      overflowing by evicting the **oldest**. Unlike `OfflineQueue` there is no
      `OverflowPolicy` choice here - a log's newest entries are where an incident investigation
      starts, and the alternative (refuse new entries when full) would freeze the log at the
      first event storm and record nothing about what followed. The eviction is logged and the
      trimmed log is what reaches storage, so a stale snapshot can't resurrect an entry the
      live log already dropped (a regression test covers exactly that). `tests/memory_budget.rs`
      now measures the log alongside everything else - see G2.3's updated figures.

      **Write policy: undebounced, deliberately.** `TransactionStore` debounces by energy moved
      and `QueueStore` by mutation count; this one writes the whole log on every recorded event.
      Security events are rare and individually meaningful, there is no high-rate equivalent of
      periodic meter samples for a threshold to protect flash against, and the entry most worth
      having durably is precisely the one immediately preceding whatever took the charge point
      down - so a threshold would buy wear savings that don't matter at the cost of the entries
      that do. `SecurityLogStore::new_atomic` matters more here than elsewhere, too: the log is
      one whole-snapshot record, so a torn write costs the entire history rather than one entry.

      **Timestamps follow G3.1's honesty rule**: `SecurityLogEntry::recorded_at` is stamped from
      the caller-supplied `Clock` only when `crate::clock::is_synchronized` accepts the reading;
      hardware with no RTC records the event in full with no time rather than a fabricated 1970
      one. That is the `PersistedTransaction::started_at` split again - the state machine stays
      clock-free and the timestamp is added at the log's edge.

      `persistence::clear_security_log` clears memory and storage and raises
      `SecurityEventType::SecurityLogWasCleared` through the normal reporting path, so the
      cleared log's first new entry says how the previous history ended. Nothing calls it yet -
      the blocks that would (`GetLog`, customer-information erasure) are [B5.1](#b5--diagnostics-and-monitoring-r14)/[B5.5](#b5--diagnostics-and-monitoring-r14) - which is
      the honest remaining half of [F4.3](#84-f4--security-events): the durable log exists, the `GetLog` reader
      does not.
- [ ] **E2.9** Certificates — **no longer blocked**: [B4.1](#b4--certificates-and-iso-15118-r1-r13)'s `StoredCertificates` is already `Storage`-backed and survives a reboot, so this row is now about a secure-element-backed store's own durability rather than about a store existing. Formerly blocked on
      which does not exist.
- [ ] **E2.11** Network profiles. **No longer blocked** —
      [B1.8](#b1--core-spine-must-be-complete-for-any-production-deployment)'s slot store landed
      and `ChargePointEvent::PersistedNetworkProfilesRestored` is in place for a restore to use.
      Lower value than the other rows until [A9](#3-workstream-a--transport-negotiation-connection-lifecycle):
      profiles this crate does not dial with are worth less after a reboot than a load limit or a
      cached authorization decision.
- [x] **E2.5** Authorization cache — `persistence::AuthorizationCacheStore`/
      `restore_authorization_cache`/`run_authorization_cache_persistence`, wired via
      `ChargePointBuilder::authorization_cache_persistence`. Unblocked by
      [B1.2](#b1--core-spine-must-be-complete-for-any-production-deployment) and closed
      immediately after it, for the same reason [E2.7](#72-e2--what-must-survive) was: the gap it
      left was precisely the case the feature exists for. A charge point that reboots *while its
      CSMS is unreachable* would have come back refusing every card, including ones the CSMS had
      already accepted minutes earlier.

      One whole-cache JSON snapshot per change, versioned and discarded on decode failure or
      version mismatch like every other record here. `AuthorizationCacheEntry` derives `serde`
      directly rather than through a mirror type, matching `LocalListEntry` beside it: every field
      is a scalar or an already-derived state type, so there is no closed wire enum for a mirror
      to protect against drifting.

      **Nothing is filtered by age at boot, deliberately** — unlike
      [E2.6](#72-e2--what-must-survive)'s reservations and [E2.7](#72-e2--what-must-survive)'s
      charging profiles. A cache entry's expiry is evaluated at *lookup* against
      `AuthCacheCtrlr`/`LifeTime`, which is a non-persistent device-model variable back at its
      default until the CSMS re-sends it; filtering at boot would apply whatever lifetime happens
      to be configured *now* to decisions cached under a different one, and would need a clock the
      charge point may not have. Expiry at lookup is correct either way.

      `ClearCache` is a change like any other, so the cleared state is what persists: an operator
      who cleared the cache does not get it back on the next boot. A restore beyond the configured
      bound keeps the most recently authorized entries, dropping from the oldest end.

      Ordering is load-bearing and asserted rather than documented and hoped for: register this
      before `authorization`, and the builder test checks a card accepted before the cut still
      authorizes after it — verified to fail when the restore call is removed.
- [x] **E2.7** Charging profiles — `persistence::ChargingProfileSnapshotStore`/
      `restore_charging_profiles`/`run_charging_profile_persistence`, wired via
      `ChargePointBuilder::charging_profile_persistence`. Unblocked by
      [B2.1](#b2--smart-charging-r11) and closed immediately after it, because the gap it left was
      the sharpest one durability had: a power cut silently *un-limited* a load-managed charge
      point — profiles gone, projection computing nothing, hardware back at its own maximum until
      the CSMS noticed and re-sent.

      One whole-store JSON snapshot per write, versioned with its own
      `CHARGING_PROFILE_SCHEMA_VERSION` and discarded on a decode failure or version mismatch like
      every other record here. The mirror types match exhaustively in both directions (purpose,
      kind, recurrency, rate unit, scope, plus the schedule/period bodies), so adding a variant to
      any of those state enums is a compile error at the mirror rather than a silent data-loss
      bug; a round-trip test drives every variant of each.

      **Recovery goes through the same door a live `SetChargingProfile` does.**
      `ChargePointEvent::PersistedChargingProfilesRestored` installs each profile through
      `ChargingProfileStore::install`, so the replacement rules and the
      `max_charging_profiles` bound hold identically however a profile arrived. A profile
      addressing an EVSE this firmware no longer has (a topology change across the update that
      caused the restart) is discarded and logged; profiles the bound refuses raise
      `MemoryExhaustion` rather than vanishing quietly — a *limit* the CSMS believes is installed
      but that the charge point does not hold is exactly the divergence worth reporting.

      **Expiry follows E2.6's stance, not a new one**: a profile whose `valid_to` has already
      passed is dropped at boot (it could never apply again, and it would hold a slot against the
      bound), but only when `crate::clock::is_synchronized` accepts the reading. On hardware with
      no RTC, "has this expired?" is not a question the charge point can answer yet, and
      discarding a live load limit because an unset clock reads 1970 would be far worse than
      keeping a stale one.

      **Ordering**: register this *before* `smart_charging`, so the restore lands before the
      projection's first evaluation — otherwise the charge point spends its first moments back
      believing nothing limits it. The builder test asserts the recovered limit reaches
      `charging_limits` (i.e. hardware), not merely the store, and was verified to fail when the
      restore call is removed.

      Write policy is deliberately undebounced, like the local authorization list's: profiles
      change only when a CSMS installs or clears one, so there is no high-rate traffic for a
      threshold to protect flash against, and the write that matters is the one immediately before
      the cut.

### 7.3 E3 — Crash consistency

- [x] **E3.1** Write ordering / journaling so a power cut mid-write can't
      produce a half-written transaction record. The record-level defense
      already existed (`PersistedTransaction` discarded on decode failure, the
      counter ordered ahead of the record it belongs to). The remaining gap —
      `Storage::set` being assumed atomic when no flash driver actually
      promises that — is now closed at the `Storage` boundary itself:
      `hardware::AtomicStorage<S>` wraps any `Storage` and turns it into one
      whose `get` never observes a torn write, via an A/B-slot protocol built
      entirely on the existing `get`/`set`/`remove` primitives (no new
      required method, so it doesn't break existing `Storage` implementors).
      Each logical key gets two physical slots (`"{key}.a"`/`"{key}.b"`); a
      write always targets whichever slot *isn't* the current winner, leaving
      the other — and therefore the previous value — untouched, and each
      record carries a sequence number, a kind (value/tombstone, so `remove`
      is a write like any other rather than an in-place delete), and a CRC32.
      A torn write leaves its target slot failing the length check or the
      checksum, so it's simply skipped in favour of the untouched slot with
      the next-highest valid sequence number — `get` after a crash returns
      either the pre-write or post-write value, never a mix.
      `persistence::TransactionStore::new_atomic` wires it in ahead of
      `TransactionStore::new` for real storage. What it still doesn't cover,
      documented on `AtomicStorage` itself: a backend that can corrupt bytes
      it wasn't asked to write (e.g. both slots sharing a flash erase block
      that a power cut interrupts mid-erase — the protocol assumes per-key
      write isolation, which a raw flash region doesn't automatically give
      you), complete-but-wrong records that still checksum fine, and
      concurrent writers to the same key (matches the rest of this crate's
      single-writer-per-key assumption).
- [x] **E3.2** Bound write frequency — `persistence_decision` in
      `src/persistence.rs`. Lifecycle transitions (started, charging-state
      change, ended) always write; a periodic meter reading only writes once the
      energy register has moved `meter_write_threshold_wh` (default 100 Wh) from
      the last reading that reached storage. That bound is exactly the maximum
      billable energy a power cut can lose, and is the knob integrators trade
      against flash wear.
- [x] **E3.3** Schema versioning — every record carries
      `persistence::SCHEMA_VERSION`; a record written by any other version is
      logged and discarded rather than guessed at, so a firmware update reads
      the previous version's state or nothing, never something in between.

### 7.4 E4 — Recovery

- [x] **E4.1** On boot: `persistence::restore_transactions` reloads every
      persisted transaction and hands them to the state machine as a single
      atomic `ChargePointEvent::PersistedTransactionsRestored`, which closes
      each one out as a `TransactionEvent(Ended)` with the new
      `StopReason::PowerLoss` (→ `ReasonEnum::PowerLoss` on all three versions),
      carrying the last meter reading that reached storage so the energy is
      still billable. Records are cleared only *after* the state machine has
      taken ownership, so a cut during recovery re-reports rather than loses.

      The decision is deliberately always *close out*, never *resume*: resuming
      would assert that the EV stayed connected and energy kept flowing while
      the firmware was not running, which nothing in `crate::hardware` can
      currently attest to. Resume needs a hardware hook that can report
      contactor/cable state at boot — worth revisiting alongside M3.
- [x] **E4.2** Send the correct `BootNotification.reason`
      (`RemoteReset`/`ScheduledReset`/`Unknown`) from persisted context —
      built on E2.12. `ChargePointBuilder::boot_reason_persistence` loads the
      persisted cause at build time and `ChargePointBuilder::provisioning`
      feeds it through `provisioning::register_until_accepted` into
      `BootNotifier::notify_boot`'s new `reason: Option<state::BootReasonCause>`
      parameter, which every 2.x version adapter's `build_request` maps onto
      the wire `BootReasonEnum` (`map_reason` in `src/provisioning.rs`'s
      `ocpp_2_1`/`ocpp_2_0_1` modules). `PowerUp` is deliberately never sent —
      see E2.12 for why an uncommanded restart honestly maps to `Unknown`
      instead. OCPP 1.6J's adapter accepts and drops `reason` — that
      protocol's `BootNotification.req` has no such field.
- [x] **E4.3** Replay the offline queue after reboot, preserving order —
      `persistence::restore_transaction_event_queue`,
      `restore_status_notification_queue` and `restore_security_event_queue`,
      each called from its `ChargePointBuilder::*_persisted` method before the
      live forwarder/subscription is wired up, so a message that arrives during
      start-up can never be delivered ahead of an older one the restored
      backlog contains. Restoration goes through
      `OfflineQueue::restore_backlog`, which pushes the persisted messages
      one at a time in their stored order, preserving `TransactionEvent`
      sequencing exactly. Covered end-to-end by
      `persistence::tests::a_queue_interrupted_by_a_power_cut_replays_its_backlog_in_order_after_reboot`
      for the generic machinery, by per-queue power-cut replay tests in
      `persistence::tests` that drive the *real* message and mirror types
      through the real wrappers, and through the builder itself by
      `builder::tests::status_notifications_persisted_survives_a_reboot_and_replays_in_order`
      and its security-event counterpart. All three queues, no carve-out.

      One consequence worth stating: `DedupedStatusNotifier`'s cache is not
      persisted, so after a reboot the first restored status change for a
      connector is always sent even if an identical one was already delivered
      before the cut. Re-reporting a status the CSMS already knows is the safe
      direction; suppressing one it doesn't is not.
- [x] **E4.4** Power-cut test harness — [`tests/power_cut_recovery.rs`](../tests/power_cut_recovery.rs).
      The lifecycle (plug → lock → present → authorize → contactor closed → *n* meter samples →
      stop → contactor open → unlock) is driven once **per cut point**, cutting after 0 steps,
      after 1, after 2, … and asserting the same invariants at each. A "cut" drops the actor and
      every task holding RAM state; only the `Storage` handle crosses into the next boot, and the
      new boot runs `restore_transactions` and nothing else before the assertions.

      Four sweeps, each stating a different half of the guarantee:

      - **Exact recovery** (write threshold 0, so every sample is durable): a cut recovers a
        transaction *iff* one was in flight, the recovered close-out carries exactly the energy
        delivered before the cut, and it is closed out with `PowerLoss` rather than resumed.
      - **Bounded loss** (default 100 Wh threshold, samples stepping 40 Wh so most are
        deliberately *not* written): the recovered reading is never higher than what was
        delivered and never lower by more than the threshold — the exact promise E3.2's flash-wear
        trade-off makes, now asserted rather than asserted-about.
      - **No id reuse**, sweeping all cut points against **one** storage so each boot lands on the
        previous cut's leftovers, the way a field unit accumulates history: every recovered
        transaction id across the whole sweep is unique.
      - **End to end to the CSMS**: the in-flight record (E2.1) and the offline transaction-event
        queue (E2.8/E4.3) running *together*, with the CSMS unreachable before the cut and back
        after it. At every cut point the CSMS ends up seeing the session's `Started` exactly once
        (replayed from the durable backlog), exactly one `Ended` carrying the delivered energy,
        and every event in non-decreasing `seqNo` order. Each half is covered on its own
        elsewhere; nothing tested them composed, which is where a real bug would hide.

      Every reboot additionally asserts recovery is **not** repeatable — a second boot recovers
      nothing — since re-reporting a recovered session would show up at the CSMS as a duplicate
      transaction.

      **The harness was verified against real regressions, not just observed to pass.** Four
      mutations were applied to `src/persistence.rs` in turn and each was caught: `Started` no
      longer written immediately (fails at the authorize cut point), recovery no longer clearing
      the record it took ownership of (fails the double-report assertion), the transaction-id
      counter no longer persisted (fails the id-reuse sweep), and a restored offline backlog
      silently dropped (fails *only* the composed sweep — the one the other three miss). Writing
      the harness also surfaced a modelling error worth recording: the first draft's pre-cut boot
      skipped `restore_transactions`, and ids restarted from 0 — a reminder that
      `restore_transactions`'s documented "must run before any new transaction can start" ordering
      is load-bearing, not advisory.

      Not swept, deliberately: a torn *mid-write* record (that is `AtomicStorage`'s guarantee,
      tested directly in `src/hardware/storage.rs`, and `InMemoryStorage` is atomic by
      construction), and the three E2 rows nothing writes during a transaction (local auth list,
      reservations, device model) — a per-step sweep of those would repeat one storage round-trip
      at every cut point and prove nothing their own end-to-end tests don't.

---

## 8. Workstream F — security

Target: OCPP's Advanced Security certification profile on 2.x, and the 1.6J
security whitepaper to whatever extent [D2.2](#62-d2--type-completeness-audit) concludes is feasible.

### 8.1 F1 — Security profiles

`crate::security_profile` models all three, and enforces the rule that makes the model worth
having. **Partially done: the profile model and its downgrade gate are in; profile 3 waits on
certificates.**

- [x] **F1.1** Profile 1 — HTTP Basic over an unsecured connection. Modelled, with the credential
      rules OCPP actually states: [`ChargePointIdentity`](../src/security_profile.rs) refuses an
      identity containing `:` (A00.FR.204 — Basic joins username and password with a colon, so the
      CSMS would split the pair in the wrong place: a parsing ambiguity with an authentication
      outcome), and `BasicAuthPassword` enforces A00.FR.205's 16–64 characters, **counted in
      characters rather than bytes** so six emoji cannot pass for a long password. It refuses to
      print itself in `Debug`, because every other type here derives `Debug` freely and a
      credential that printed itself would reach a log the first time anything containing it was
      traced. Entropy is *not* checked, and the type says why: a 40-character run of `a` passes
      every mechanical test there is.
- [x] **F1.2** Profile 2 — Basic auth over TLS, modelled and reachable: `ConnectOptions` already
      carries both `username`/`password` and a `rustls::ClientConfig`, so an operator can run
      profile 2 today. Validating the CSMS certificate against a *managed* trust store is
      [F2.2](#82-f2--tls), which needs B4.
- [ ] **F1.3** Profile 3 — mutual TLS with a charge point certificate. **Blocked on
      [B4.1](#b4--certificates-and-iso-15118-r1-r13)** (a client certificate to present) and
      [F2.4](#82-f2--tls) (somewhere to keep the private key). `SecurityProfile::is_implemented`
      reports `false` for it rather than letting a station behave as though it presents a
      certificate it does not have.
- [x] **F1.4** Profile selection and switching via `SetNetworkProfile`, **with the §A05 downgrade
      rule enforced** — which is the substance of this row.

      A security profile may be raised over OCPP but essentially never lowered: dropping to
      profile 1 is forbidden outright, and 3 → 2 only where the operator set
      `AllowSecurityProfileDowngrade`. The reason is worth stating plainly, because it is what the
      check is for: **the CSMS connection is the channel an attacker would use to weaken the
      charge point.** A `SetNetworkProfile` that could silently move a TLS station onto plaintext
      turns one compromised CSMS credential into a fleet moved to cleartext. `SetNetworkProfile`
      now refuses such a write outright rather than warning about it, and the "even with the
      opt-in, never to profile 1" case is tested explicitly against both settings of the flag.

      Measured against the profile **currently in force** (`SecurityCtrlr.SecurityProfile`), not
      against whatever is in the slot being written — writing a fresh slot is as good a way to
      weaken a station as rewriting the live one. `AllowSecurityProfileDowngrade` is registered
      and defaults to `false`, since §A05 makes it an explicit opt-in.

### 8.2 F2 — TLS

- [x] **F2.1** TLS in the transport path — available for std: `ocpp-client` re-exports `rustls`
      and `ConnectOptions::tls_config` takes a `ClientConfig`, which `ConnectionTarget` already
      threads through every redial. Embedded still needs an `embedded-tls`-shaped alternative;
      `ocpp-client` exposes none today, so that remains open upstream rather than here.
- [ ] **F2.2** Trust store management fed by [B4](#b4--certificates-and-iso-15118-r1-r13)'s
      installed certificates. **Blocked on B4.1.**
- [x] **F2.3** TLS version policy — `TlsVersion::is_permitted` encodes OCPP's floor of 1.2, so a
      transport that can report what it negotiated has something to check against and an event to
      raise (`InvalidTLSVersion`/`InvalidTLSCipherSuite` are both modelled and correctly spelled).
      **Not yet raised from a live connection**: `ocpp-client` does not surface the negotiated
      version or cipher suite, so there is nothing to inspect. Modern rustls will not speak below
      1.2 at all, which makes the practical risk small and the reporting gap real.
- [ ] **F2.4** Secure element / key storage abstraction — private keys must not be required to sit
      in flash. Wanted by F1.3; no consumer until then.

### 8.3 F3 — Credentials

- [x] **F3.1** Basic-auth password storage and rotation — `BasicAuthPassword` is the validated
      holder (see F1.1). Rotation is a `SetVariables` write to `SecurityCtrlr.BasicAuthPassword`,
      which F3.3 reports.
- [ ] **F3.2** Certificate renewal ahead of expiry
      ([B4.3](#b4--certificates-and-iso-15118-r1-r13)). **Blocked on B4.**
- [x] **F3.3** `ReconfigurationOfSecurityParameters` on every change — raised by
      [F4.2](#84-f4--security-events) on an accepted `SetNetworkProfile`, which is where a security
      profile and endpoint change.

### 8.4 F4 — Security events

All 21 event types in the vendored appendix are modelled, and OCPP's
**criticality** distinction is now honoured rather than ignored.

- [x] **F4.1** Added the missing three: `DiscardedRenewedClientCertificate`,
      `MaintenanceLoginAccepted`, `MaintenanceLoginFailed`. A test asserts the modelled set is the
      same size as the appendix's, so the next spec revision fails a test rather than going
      unnoticed.
- [x] **F4.2** Raised from the code paths that detect them, and **criticality now decides where an
      event goes** — which turned out to be the substance of this row.

      **The defect.** OCPP A04 splits a security event's two destinations: A04.FR.01 sends
      *critical* events to the CSMS, A04.FR.04 stores *every* event (also non-critical) in the
      security log. This crate sent everything to the CSMS. That is not merely over-reporting: the
      notification queue is bounded and drops its **oldest** entry on overflow (G2.2), and two of
      the non-critical types — `InvalidMessages` and `AttemptedReplayAttacks` — are exactly what a
      remote party can generate at will by throwing malformed frames at the charge point. Sharing
      the queue meant an attacker could flood it and evict a queued `TamperDetectionActivated`
      before the CSMS ever saw it: **silencing the report of their own physical intrusion.**

      `SecurityEventType::is_critical` (transcribed from the appendix's `Critical` column, and
      test-asserted against it row by row) now gates entry to the queue — *before* the push, not
      before the send, because anything allowed into a bounded queue can displace something else.
      `Other` is treated as critical: a vendor event this crate has never heard of is not
      something to quietly downgrade. The regression test was validated by injecting the old
      behaviour, which fails it with the tamper report fully evicted and only `InvalidMessages`
      delivered.

      **Now actually raised:** `StartupOfTheDevice` (after the hardware binding starts — a charge
      point that failed to start has not started), `ResetOrReboot` (before the event that may
      reboot immediately, since afterwards there may be no process left to raise anything), and
      `ReconfigurationOfSecurityParameters` on an accepted `SetNetworkProfile`, which carries the
      security profile and the endpoint this charge point authenticates to. Together with the
      three already raised — `SettingSystemTime`, `MemoryExhaustion`, `SecurityLogWasCleared` —
      that is six of 21.

      **Still not raised, and honestly so:** the certificate, firmware, TLS and authentication
      types need blocks that do not exist ([B3](#b3--firmware-management-r12),
      [B4](#b4--certificates-and-iso-15118-r1-r13), [F1](#81-f1--security-profiles),
      [F2](#82-f2--tls)). `InvalidMessages` and `AttemptedReplayAttacks` need a transport-level
      hook `ocpp-client` does not expose. `TamperDetectionActivated` and the two
      `MaintenanceLogin*` types are the integrator's to raise — only the hardware knows a case was
      opened or who logged in, and OCPP's recommended `techInfo` format for a login
      (`{'user': ..., 'origin': ...}`) is theirs to fill in.
- [ ] **F4.3** Durable, size-bounded security log ([E2](#72-e2--what-must-survive)), readable via
      `GetLog`. *Partial* - the log itself is done ([E2.10](#72-e2--what-must-survive)): bounded,
      durable, restored at boot, and clearable with a `SecurityLogWasCleared` report. What's left
      was the `GetLog` reader that uploads it — **which [B5.1](#b5--diagnostics-and-monitoring-r14)
      now provides**: a `GetLog` with `logType = SecurityLog` renders the durable log and uploads
      it. This row stays open only for the parts of `GetLog` that are not the security log
      (monitoring reports, and 2.1's data-collector log), so the security-log half of F4.3 is
      done.
- [x] **F4.4** `SecurityEventNotification` for 2.0.1 — **done**, and the "after D1" caveat was
      stale: D1 landed and the pinned `ocpp-client` 0.2.2 generates the 2.0.1 action. The wire
      request is field-for-field identical to 2.1's, so the adapter shares `wire_type` exactly as
      that function's docs anticipated, and `connect_and_setup`'s 2.0.1 path registers the block.

      **1.6J: decided, and the answer is no.** 1.6J has no `SecurityEventNotification` in the core
      specification — it arrives only with the OCPP 1.6 Security Whitepaper, whose message set
      `ocpp-types` does not generate ([D2.2](#62-d2--type-completeness-audit)). A 1.6J connection
      records events in the durable log and reports none. That is a version difference, not a gap
      here; closing it means contributing the whitepaper types upstream first, which is D2.2's
      still-open decision.

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

- [x] **G2.1** `OfflineQueue` is now bounded: `DEFAULT_CAPACITY` (100
      messages) unless a caller picks its own via `with_capacity`, with a
      caller-configurable `OverflowPolicy` (`DropOldest` evicts the front
      message to make room, `DropNewest` rejects the incoming one and
      leaves the queue untouched) rather than one policy imposed on every
      message kind. The two lose different things — spelled out in
      `OverflowPolicy`'s rustdoc — so the status and security queues keep
      the default `DropOldest` (a queued `StatusNotification` is superseded
      by whatever the connector's status is once the connection recovers,
      so keeping the newest is what matters) while the transaction queue
      opts into `DropNewest` (a queued `TransactionEvent` carries a
      billable energy reading, so evicting the oldest would permanently
      lose billing data; rejecting the newest only delays fresh activity).
      `OfflineQueue::push` returns whatever message overflow dropped, and
      `run_with_offline_queue` takes an `on_overflow` callback fed that
      message — kept as a plain callback rather than a hard dependency on
      `crate::security` so the queue itself stays protocol/security-agnostic
      and no_std-clean. `ChargePointBuilder` wires the status and
      transaction queues' callbacks to raise a `MemoryExhaustion` security
      event via `report_security_event`; the security-event queue's own
      callback deliberately does *not* do that (it only logs) since raising
      a security event from the security-event queue's own overflow would
      feed back into the same queue and risk an unbounded loop the moment
      that queue is also full. G2.2, below, has since audited every other
      collection in `ChargePointState`, and G2.3 has measured what all of them
      actually retain - the 100-message default costs ~3 KB (status), ~24 KB
      (transaction) and ~21 KB (security) when full, per
      [`docs/MEMORY.md`](MEMORY.md), so it is no longer a bare estimate.
- [x] **G2.2** Audited every collection in `ChargePointState`; the two that
      could grow without limit are now bounded by a caller-configurable
      maximum, carried in `crate::state::StateLimits` and passed once at
      construction (`ChargePointState::with_limits`,
      `ChargePointActor::spawn_with_limits`,
      `ChargePointRuntime::new_with_limits`,
      `ChargePointBuilder::start_with_limits`). There is deliberately no way
      to raise a limit at runtime - a bound a remote peer can move is not a
      bound - and the non-`_with_limits` constructors keep using
      `StateLimits::default()`, so no existing call site moved.

      **Local authorization list** (`LocalAuthorizationList.entries`, grown by
      CSMS `SendLocalList`): bounded by
      `max_local_authorization_list_entries`, default 100 entries (a few KB;
      sized for a private/fleet site's whole card population, which is what
      offline authorization actually exists for - a public site can't usefully
      cache "every driver who might arrive" at any bound). An update that
      wouldn't fit is refused *outright* with the new
      `SendLocalListOutcome::TooManyEntries` -> wire `Failed` in all three
      protocol versions - rather than applied up to the bound: a partially
      applied list rejects id tokens the CSMS believes are cached, which is
      worse for a driver at an offline charge point than a `Failed` the CSMS
      can see and retry with a shorter list. A differential update that only
      replaces or removes entries doesn't grow the list and is still accepted
      when full. `ChargePointState::apply` enforces the same bound by
      truncating, which is the last line of defence rather than the normal
      path: the reachable case is a list restored from durable storage written
      by a build configured with a larger maximum (a firmware update that
      lowered it). Because truncation silently loses authorization decisions,
      it raises a `MemoryExhaustion` security event - the same treatment a
      saturated `OfflineQueue` gets in G2.1, for the same reason: the CSMS is
      the only party that can act on it. Storage then converges on the
      truncated list, since the restore is a state change like any other and
      `run_local_authorization_list_persistence` writes it back at the same
      version.

      **Device model** (`DeviceModel.components`, grown by
      `DeviceModelEvent::VariableRegistered`): bounded by
      `max_device_model_variables`, default 256 `(Component, Variable)` pairs
      - generous headroom over this crate's own registrations (the built-in
      defaults plus one `*Ctrlr.Available` per `CAPABILITY_GATES` entry) for
      whatever a hardware binding adds. Not CSMS-driven (`SetVariables` only
      writes attributes that already exist, and is unaffected by this bound);
      the growth path is an integrator's binding, so a registration past the
      maximum is refused and logged rather than being a protocol-visible
      error, and `apply` reports no state change for it. Redefining an
      already-registered variable never grows the model and is always allowed,
      including at the maximum. A configured maximum below what
      `DeviceModel::new` itself registers is raised to fit the built-in
      defaults - dropping those would leave `GetVariables`/`GetBaseReport`
      reporting an incomplete model rather than bound anything worth bounding.

      **Audited and deliberately left alone**, because they are bounded by
      construction rather than by a configured maximum: `evses`, and each
      `EvseState`'s `connectors`/`transactions`/`reservations`/`running_costs`
      - all sized once from the hardware binding's topology and never grown
      (the state machine's invariants allow one active transaction and one
      reservation per connector). **Transaction history**: there is none to
      bound - `ChargePointState` holds only the current transaction per
      connector, and `crate::persistence`'s `TransactionStore` keeps one
      record per connector, not a log. **Charging profiles**: Smart Charging
      isn't implemented yet (§4 D-workstream), so there is no collection to
      bound; whoever lands `SetChargingProfile` must add a `StateLimits`
      entry with it. The offline report queues carry their own bound (G2.1).

      Still open, and explicitly *not* this entry: an over-long record is
      still fully deserialized before being truncated, so the transient
      allocation is unbounded even though the retained state isn't - bounding
      that is [F5.2](#85-f5--hardening)'s job. G2.3 has since priced both
      defaults (~110 B per local list entry, ~375 B per device model variable
      clustered OCPP-style - see [`docs/MEMORY.md`](MEMORY.md)), so they are
      measured rather than estimated; what remains is that neither maximum is
      advertised to the CSMS yet via the device model
      variables OCPP has for it (`LocalAuthListCtrlr.Entries` /
      `ItemsPerMessage` in 2.x, `LocalAuthListMaxLength` /
      `SendLocalListMaxLength` in 1.6J), so a CSMS discovers the bound by
      being refused rather than by reading it - worth landing with the
      `GetBaseReport` work in §2.
- [x] **G2.3** [`docs/MEMORY.md`](MEMORY.md) documents worst-case retained
      heap per configuration, and the numbers are **measured, not estimated**:
      [`tests/memory_budget.rs`](../tests/memory_budget.rs) installs a counting
      `GlobalAlloc` and reads live requested bytes around each structure, filled
      to its configured bounds (local list full of 36-character id tokens,
      device model full, every connector holding a transaction *and* a
      reservation, all three offline queues full, and - since E2.10 - the
      durable security log full). It runs as part of
      `cargo test`, and asserts a ceiling per configuration - so a change that
      meaningfully grows retained state fails the build instead of being found
      on a device. Headline figures: ~48 KB for a tightened single-connector
      wallbox, ~171 KB at this crate's defaults, ~391 KB for a 4-EVSE DC site
      (64-bit host; a 32-bit MCU holds 0.5-0.85x of each type, so those are
      conservative upper bounds - `size_of` for both targets is tabulated,
      the 32-bit column measured via `cargo check --target
      thumbv7em-none-eabihf`). The doc is explicit about what the figures
      exclude and who owns it: allocator bookkeeping, task stacks, transport
      and TLS buffers, transient (de)serialization.

      **The finding worth acting on:** the device model dominates every
      configuration, and its per-variable cost swings **5.6x** purely on how
      variables are grouped across components - 2090 B/variable at one variable
      per component versus 374 B at eight, because
      `BTreeMap<Component, BTreeMap<Variable, VariableDefinition>>` allocates
      each node at its full branching factor however few entries it holds.
      OCPP's own `*Ctrlr` clustering is the cheap shape, so a binding that
      follows the spec's naming gets the good case for free while one that
      invents a component per sensor pays up to 2 KB a variable. Same reason
      the ~5 KB "empty state" floor is almost entirely the two built-in default
      variables sitting on two separate components, and barely moves between a
      1-connector and an 8-connector charge point. If that 5.6x ever needs
      closing rather than documenting, the fix is a flatter key
      (`BTreeMap<(Component, Variable), _>`) - worth weighing against
      `GetBaseReport`'s need to iterate per component, and deliberately *not*
      done here: G2.3 was to measure and document, not to redesign the model
      on the strength of a first measurement.
- [x] **G2.4** Measured, and the numbers are in the README (plus the full table
      in [`docs/MEMORY.md`](MEMORY.md#flash)).
      [`scripts/flash-cost.sh`](../scripts/flash-cost.sh) builds
      [`tools/flash-probe`](../tools/flash-probe) - a real bare-metal firmware
      image that *exercises* the enabled features - for
      `thumbv7em-none-eabihf` with `opt-level="z"`, fat LTO and
      `--gc-sections`, then reports the flashable image size
      (`objcopy -O binary`, i.e. the bytes you program onto the part), once per
      feature set. The `--quick` form (core + everything) runs in CI's
      `embedded` job: the probe is the only thing in the repository that
      *links* a bare-metal image rather than checking one, so it catches what
      `cargo check` cannot - a missing symbol, a stale `critical-section`
      backend - and it can't silently rot.

      | Feature set | Flash | vs core |
      | --- | --- | --- |
      | Core, no protocol version | 32 KB | - |
      | Core + 1.6J | 174 KB | +141 KB |
      | Core + 2.0.1 | 224 KB | +191 KB |
      | Core + 2.1 | 310 KB | +277 KB |
      | Core + all three versions | 474 KB | +441 KB |
      | Core + 2.1 + `reservation` | 320 KB | +10 KB over 2.1 |
      | Core + 2.1 + `local-auth-list` | 322 KB | +12 KB over 2.1 |
      | Core + 2.1 + `tariff-cost` | 315 KB | +5 KB over 2.1 |
      | Core + 2.1 + the 11 declared-capability features | 311 KB | +1 KB over 2.1 |
      | Everything | 523 KB | +490 KB |

      **What it says about [C1](#51-c1--cargo-feature-per-functional-block).**
      The version-independent core is small (32 KB); the negotiated protocol
      version dominates everything else, and the second and third version cost
      +164 KB on top of 2.1 alone - so on a 512 KB part, "which versions do we
      speak" is the flash decision, and a single-version build is the first
      lever. The three functional blocks that are *genuinely* feature-gated
      today are cheap and behave exactly as C1 intends: 5-12 KB each, absent if
      unused. The other eleven capability features cost ~1 KB **in total**, and
      the honest reading is not "they're free" but "there is nothing behind them
      yet" - they are capability declarations whose functional blocks (workstream
      B) aren't implemented. Each should grow a real number as its block lands,
      which is what this table now exists to catch.

      Deliberately not attempted: splitting a per-version figure into "this
      crate" versus "`ocpp-client`/`ocpp-types`/`serde`". The codecs are only
      reachable *because* the adapters name those message types, so the split
      isn't defensible from a linked-image measurement; `cargo bloat` on the
      probe is the tool for looking inside a number. The figures also exclude
      what the integrator brings (transport, TLS, executor, allocator, reset
      vector/startup, a real panic handler) and assume the probe's release
      profile - both stated in the doc rather than left implied.

      Two measurement traps worth recording, since both silently produce
      *plausible* numbers rather than errors: escaping a spawned future's
      address through a thin pointer discards its vtable, after which LTO proves
      every future body unreachable (the whole image measured 60 bytes), so the
      probe polls each future once instead; and a `GlobalAlloc` that always
      returns null lets LLVM fold every allocation into an abort and every
      caller into dead code, so the probe uses a real bump allocator over a
      static arena. `scripts/flash-cost.sh` also deletes the previous image
      before each build and fails loudly, rather than measuring a stale binary
      when a feature set doesn't compile.

### 9.3 G3 — Time

- [x] **G3.1** Behaviour with no RTC and no CSMS time yet — transactions
      must still be recordable. Wired as far as this crate's *only*
      caller-injectable, potentially-no-RTC `Clock` in the live path:
      `crate::persistence::run_transaction_persistence`/`next_record`
      (`src/persistence.rs`). A `Started` event now checks
      `crate::clock::is_synchronized` on the `Clock` reading before stamping
      `PersistedTransaction::started_at` — an unset RTC's implausible
      reading (Unix epoch or similar, per `Clock`'s own contract) is stored
      as `None`, never as a fabricated-but-plausible-looking date. The
      transaction is still fully written and recoverable either way
      (`transaction`/`meter_start` are unaffected) — only `started_at` is
      left honestly blank pending a real sync, satisfying G3.1's explicit
      "must still be recordable" requirement without inventing a garbage
      timestamp. Once `crate::provisioning`'s time-sync anchor (G3.2, below)
      corrects the clock, later transactions started on the same boot get a
      real `started_at`; a transaction already recorded with `None` is not
      retroactively corrected (`PersistedTransaction::started_at`'s docs
      spell out the reconciliation options, e.g. against the CSMS's own
      `TransactionEvent(Started)` receipt time).

      **Since closed — the CSMS-facing timestamp adapters take a
      caller-supplied `Clock` too.** This entry originally recorded a scope
      decision to leave the `StatusNotification`/`TransactionEvent`/
      `SecurityEventNotification`/report `timestamp` adapters hard-locked to
      `SystemClock`, on the reasoning that "there is no no-RTC path to reach
      them today". That reasoning was circular, and the cost was much larger
      than the entry implied: those eight `#[cfg(feature = "std")] mod
      with_system_clock` modules were each their adapter's *only* consumer, so
      without `std` the adapters were dead code — which meant **no OCPP version
      adapter was reachable in a `no_std` build at all**. A bare-metal build
      compiled the core state machine but could not speak 1.6J, 2.0.1 or 2.1.
      CI caught this the whole time (`-D warnings` dead-code errors under
      `--no-default-features --features ocpp_1_6` / `ocpp_2_0_1` / `ocpp_2_1`)
      and the `feature-matrix` job had simply been red and unread for several
      commits — see [H1.2](#101-h1--ci-hardening).

      Each adapter is now generic over `C: Clock` with a `with_clock(…, clock)`
      constructor available on all targets, and the modules are renamed
      `with_clock` and no longer `std`-gated. Existing `std` callers are
      unaffected: `new(…)` survives as a `#[cfg(feature = "std")]` convenience
      forwarding `SystemClock`, and the direct `impl StatusNotifier for
      OCPP2_1Client`-style impls stay exactly as they were, so
      `setup()`/`connect_and_setup()` needed no signature change and no call
      site moved. Because `ocpp-client`'s client types are foreign and can't
      carry a `clock` field, the 2.x paths are a pair of thin shells — the
      `std` direct impl and the generic wrapper — over one shared
      `build_*_request` function, so the two cannot drift.

      The timestamp policy, which G3.1 previously used as justification: OCPP's
      wire `timestamp` on these messages is mandatory and has no "unknown"
      encoding, so an unsynchronized clock's reading is **sent as-is** — never
      substituted, clamped or omitted — and a `tracing::warn!` records that it
      happened. That differs from `PersistedTransaction::started_at`, which is
      an internal `Option` and so *can* honestly stay blank. The policy is
      documented once on `crate::clock::is_synchronized` and guarded by an
      `an_unsynchronized_clocks_reading_is_still_sent_not_substituted_or_dropped`
      test in each of the four adapter files.
- [x] **G3.2** Clock sync from `BootNotification`/`Heartbeat` responses,
      raising `SettingSystemTime`. Fully wired into the live path.
      `BootNotificationOutcome` grew `current_time: Option<DateTime<Utc>>`,
      and `HeartbeatSender::send_heartbeat` now returns
      `Result<Option<DateTime<Utc>>, Self::Error>` instead of
      `Result<(), Self::Error>`; every `ocpp_1_6`/`ocpp_2_0_1`/`ocpp_2_1`
      adapter in `src/provisioning.rs` parses the wire response's
      `currentTime` via `parse_csms_current_time`, now logging a warning
      (`parse_csms_current_time_logged`) on a non-empty value that fails to
      parse rather than silently treating it as "no sync available". Every
      construction site across `src/builder.rs`, `src/setup.rs`,
      `src/runtime.rs`, `src/connection.rs`, `src/remote_control.rs`, and
      `examples/simple.rs` was updated (`current_time: None` for fakes that
      don't model a CSMS clock).

      `register`/`register_until_accepted`/`run_heartbeat` call
      `evaluate_time_sync` on every response carrying a parseable
      `currentTime` and report `SettingSystemTime` via
      `crate::security::report_security_event` when it returns a step — see
      those functions' docs for the explicit "this crate detects and
      reports; setting the actual system/RTC clock is the integrator's job"
      boundary.

      **Where the sync state lives, and why**: a new
      `crate::state::TimeSyncAnchor { csms_time, recorded_at }` field
      (`ChargePointState::time_sync`, mutated only via the new
      `ChargePointEvent::TimeSynced`, per `CLAUDE.md`'s "state mutations go
      through events" rule) rather than a local inside the heartbeat loop.
      Two reasons: (1) `register`/`register_until_accepted` (BootNotification,
      including a reconnect's fresh one via `reregister_on_reconnect`) and
      `run_heartbeat` (Heartbeat) are separate call paths that both need to
      compare against the *same* anchor for drift detection to mean
      anything — a loop-local would make every reconnect's BootNotification
      look like a first-ever sync again; (2) the actor is this crate's
      designated owner of shared, cross-functional-block state per
      `CLAUDE.md`'s actor-model guidance, and this is exactly that: read by
      Provisioning, written by Provisioning, but conceptually charge-point
      state, not heartbeat-loop-private state. The anchor is *not* the raw
      OCPP-visible `ChargePointState` fields like `registration` — it is
      still scoped as clearly-internal bookkeeping (no version adapter reads
      it to build a wire message), but shares the same actor/event
      infrastructure since that's where cross-call-path shared state
      already lives in this codebase.

      The anchor's `recorded_at` is a `MonotonicInstant` (G3.3's primitive),
      not a second wall-clock reading: `local_time_estimate` advances
      `csms_time` by `MonotonicClock`-measured elapsed time since it was
      recorded, rather than comparing against a fresh `Clock::now()`. This
      is deliberate, not incidental — on hardware with no RTC, `Clock::now()`
      never advances past `clock::unsynchronized_before()` (this crate never
      sets the RTC itself), so comparing a live reading against the CSMS's
      `currentTime` on every single Heartbeat would report a "first sync"
      every cycle, defeating `CLOCK_STEP_THRESHOLD_SECS`'s entire point. The
      monotonic-anchored estimate tracks real elapsed time regardless of
      RTC presence, which is also what makes it correct to keep advancing
      through a `SettingSystemTime` correction mid-session — see G3.3.
      `register`/`register_until_accepted`/`run_heartbeat`/
      `reregister_on_reconnect`/`ChargePointBuilder::provisioning`/
      `crate::setup::setup`/`connect_and_setup` all now take a
      caller-supplied `M: MonotonicClock` parameter, mirroring the existing
      `Backoff`/`Executor` pattern — `crate::clock::SystemMonotonicClock` for
      std/tokio callers, a free-running-timer impl for embedded ones.
      Covered by five new tests in
      `provisioning::time_sync_wiring_tests` (first-sync reporting and
      anchor storage, a consistent second sync staying silent, a genuine
      step being reported, and a heartbeat response advancing the anchor).
- [x] **G3.3** Correct handling of a clock jump mid-transaction (monotonic
      durations, not wall-clock subtraction). `crate::clock::MonotonicClock`
      / `MonotonicInstant` (from the prior commit) is now actually consumed,
      by G3.2's time-sync anchoring above (`local_time_estimate` in
      `src/provisioning.rs`) — the one piece of duration math this round of
      work added is exactly the kind G3.3 exists for (elapsed-time-since-
      last-known-good-sync, immune to a `SettingSystemTime` step happening
      mid-measurement), and it correctly uses
      `MonotonicInstant::duration_since` rather than subtracting two
      `DateTime<Utc>` values. Audited the rest of the tree for other
      wall-clock duration subtraction while in here: **none exists**.
      `Transaction` still carries no timestamps, no code subtracts two
      stored `DateTime<Utc>` readings to compute an elapsed session/interval
      duration, and `evaluate_time_sync`'s own `csms_time - local` (in
      `TimeSyncStep`/its tests) is comparing two *point-in-time* estimates
      to size a clock step, not measuring elapsed real time — a different
      operation that a `MonotonicClock` cannot substitute for (a "how far
      apart are these two clocks' opinions" question inherently needs both
      opinions as wall-clock timestamps). Remains groundwork for whenever
      session-duration/sampling-interval reporting is added.

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
- [x] **H1.6** Coverage reporting, with a floor on the protocol adapters.
      Whole-crate coverage (`cargo llvm-cov --all-features`) stays
      informational, matching the reasoning for not gating the crate-wide
      number in the CI comment. The gate targets the 11 dedicated adapter
      files (`ocpp_1_6.rs` / `ocpp_2_0_1.rs` / `ocpp_2_1.rs` under
      `certificates/`, `diagnostics/`, `firmware/`, `smart_charging/`) —
      llvm-cov has no positive file filter, so the `coverage` job exports
      JSON and aggregates those files' line counts with `jq`. Measured
      baseline on the commit this landed: 2250/2969 lines = 75.78%; floor set
      to 70% to leave headroom rather than pin the exact number. Does *not*
      cover the many other `ocpp_1_6`/`ocpp_2_0_1`/`ocpp_2_1` adapter
      *modules* that live inline inside larger files (e.g.
      `authorization.rs`) — llvm-cov reports at file granularity, so those
      aren't separable from the rest of their file without per-region
      tooling this doesn't attempt.
- [x] **H1.7** Run on PRs, not just `push`.
- [ ] **H1.8** **Make a red gate visible.** `feature-matrix` was failing on
      `main` for at least three consecutive commits before anyone looked; the
      other six jobs were green, so nothing surfaced it. The failure was real
      (see [G3.1](#93-g3--time)) and is now fixed, but the process gap isn't:
      a gating job can go red and stay red unnoticed. Branch protection on
      `main`, or a notification on a failing `main` run, whichever fits.

### 10.2 H2 — Integration testing

514 unit tests, one integration test. Unit coverage is genuinely good; what's
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
- [x] **H2.5** Power-cut recovery — done by [E4.4](#74-e4--recovery)'s sweep
      ([`tests/power_cut_recovery.rs`](../tests/power_cut_recovery.rs)), which is an integration
      test over the public API rather than an in-crate one. It drives the actor and the
      persistence tasks directly rather than a socket, so [H2.1](#102-h2--integration-testing)'s
      mock-CSMS harness would still add the wire-level half.
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
fail-safe error path, but nothing yet *calls* it. (Since updated by M2's first
slice: E2.1/E2.2, E3.2, E3.3 and E4.1 are now done — the in-flight transaction
is persisted and recovered. Later slices closed E2.3, E2.4, E2.6, E2.12 and
E4.2, then E2.8/E4.3 for all three offline queues — transaction-event, status
and security — then E2.10's durable security log. Every remaining E2 row is
blocked on a block that doesn't exist yet: authorization cache
on B1.2, charging profiles on B2.1, certificates on B4.1, network profiles on
B1.8/A9.)

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

### M2 — Durability — ✅ complete (2026-08-07)

[E1](#71-e1--storage-trait)–[E4](#74-e4--recovery) · [G2](#92-g2--bounded-memory) bounded memory · [G3](#93-g3--time) time handling

> **Exit:** power-cut at any point in a transaction loses no billable
> energy; the offline queue survives reboot; memory is bounded under a
> week-long outage.

This is the gap between "a demo" and "a product". It's ahead of most
message coverage on purpose — a charger that handles 86 messages and loses
transactions on power loss is not shippable; one that handles 25 and never
loses a transaction is.

All three exit conditions are met, and the first one is now *swept* rather than
sampled: [E4.4](#74-e4--recovery)'s
[`tests/power_cut_recovery.rs`](../tests/power_cut_recovery.rs) cuts at every point across a
session and asserts recovery at each, including the record and the offline queue composed, and
the harness itself was validated against four injected regressions. [G2](#92-g2--bounded-memory)
bounds every growable collection with measured, ceiling-asserted figures
([`docs/MEMORY.md`](MEMORY.md)); [G3](#93-g3--time) handles a missing RTC, CSMS clock sync and
mid-transaction clock jumps.

Three honest caveats, none of them a hole in the exit criteria:

- **Two E2 rows remain RAM-only**, both still blocked on functional blocks that do not exist yet:
  certificates ([B4.1](#b4--certificates-and-iso-15118-r1-r13)) and network profiles
  ([B1.8](#b1--core-spine-must-be-complete-for-any-production-deployment)/[A9](#3-workstream-a--transport-negotiation-connection-lifecycle)).
  Every row whose block exists is durable: charging profiles
  ([E2.7](#72-e2--what-must-survive)) landed with B2 and the authorization cache
  ([E2.5](#72-e2--what-must-survive)) with B1.2, each closed immediately rather than left as a
  durability gap behind a shipped block.
- **Durability is opt-in per concern.** Every `*_persistence` / `*_persisted` registration on
  `ChargePointBuilder` is a separate call an integrator has to make; `setup()`'s
  "everything on" wrapper does not wire any of them, because it has no `Storage` to wire them
  to. A charge point that never calls them runs exactly as it did before workstream E.
- **"No billable energy lost" is bounded by the write threshold, not zero**, by design
  ([E3.2](#73-e3--crash-consistency)) — at the 100 Wh default a cut can lose up to 100 Wh, which
  E4.4 now asserts as a bound rather than leaving as prose. An integrator wanting exactness sets
  the threshold to 0 and accepts the flash wear.

### M3 — Protocol completeness, core

[B1](#b1--core-spine-must-be-complete-for-any-production-deployment) core spine · [B2](#b2--smart-charging-r11) smart charging · [A1](#3-workstream-a--transport-negotiation-connection-lifecycle)–[A9](#3-workstream-a--transport-negotiation-connection-lifecycle) transport ·
[B8.1](#b8--reservation-derv2x-battery-swap) reservation status

> **Exit:** version negotiation works; every Core-profile message is handled
> on all three versions; load management works end to end.

Smart charging is the block real deployments demand first.

**All three exit conditions are met.** Every A-row is ✅ except
[A4](#3-workstream-a--transport-negotiation-connection-lifecycle), which is 🔒 on an `ocpp-client`
`ConnectOptions` that still has no ping-interval field in 0.2.2 (re-verified at the A5 commit, not
carried over from the old note). That is a keepalive cadence this charge point does not drive, not
a Core-profile message going unhandled, so it does not hold the milestone: `WebSocketPingInterval`
reads `0`, which is the honest value.

One caveat on "load management works end to end": it works for the *limits* this crate can
project. 2.1's setpoints, discharge limits and per-phase asymmetries are parsed and dropped, because
`hardware::Connector::set_current_limit` is a single import limit — see
[B2.6](#b2--smart-charging-r11) and [B8.2](#b8--reservation-derv2x-battery-swap).

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

### A.1 OCPP 1.6J — 28 of 28 wired

**Complete.** Every message OCPP 1.6J's core profile defines is handled. The old "25 of 28"
heading here also disagreed with its own list, which had 24 entries; both are now moot.

**Wired:** Authorize, BootNotification, CancelReservation,
ChangeAvailability, ChangeConfiguration, ClearCache, DataTransfer, GetConfiguration,
ClearChargingProfile, GetCompositeSchedule, GetLocalListVersion, Heartbeat,
MeterValues, RemoteStartTransaction, RemoteStopTransaction, ReserveNow, Reset,
DiagnosticsStatusNotification, GetDiagnostics, SendLocalList, SetChargingProfile,
StartTransaction, StatusNotification, StopTransaction, TriggerMessage,
FirmwareStatusNotification, UnlockConnector, UpdateFirmware

**Missing:** none — 1.6J's core profile is complete.

### A.2 OCPP 2.0.1 — 39 of 64 wired

**Wired:** Authorize, BootNotification, CancelReservation,
ChangeAvailability, ClearCache, ClearChargingProfile, CostUpdated, DataTransfer,
GetBaseReport, GetChargingProfiles, GetCompositeSchedule, GetLocalListVersion,
DeleteCertificate, FirmwareStatusNotification, GetInstalledCertificateIds, GetLog,
GetReport, GetVariables, Heartbeat, InstallCertificate, LogStatusNotification,
NotifyReport, UpdateFirmware,
ReportChargingProfiles, ReservationStatusUpdate, RequestStartTransaction,
MeterValues, RequestStopTransaction, ReserveNow, Reset, SecurityEventNotification,
SendLocalList, SetChargingProfile, SetNetworkProfile, SetVariables,
StatusNotification, TransactionEvent, TriggerMessage, UnlockConnector

**Missing:** CertificateSigned,
ClearDisplayMessage, ClearVariableMonitoring, ClearedChargingLimit,
CustomerInformation, Get15118EVCertificate, GetCertificateStatus,
GetDisplayMessages, GetMonitoringReport, GetTransactionStatus, NotifyChargingLimit,
NotifyCustomerInformation, NotifyDisplayMessages, NotifyEVChargingNeeds,
NotifyEVChargingSchedule, NotifyEvent, NotifyMonitoringReport,
PublishFirmware, PublishFirmwareStatusNotification, SetDisplayMessage,
SetMonitoringBase, SetMonitoringLevel,
SetVariableMonitoring, SignCertificate, UnpublishFirmware

**Note:** `SecurityEventNotification` used to be listed here as present in the
2.0.1 spec and in `ocpp-types` v201 but ungenerated by `ocpp-client` 0.2.0. D1
fixed that upstream; 0.2.2 generates all 64 actions, and F4.4 wired this one.

### A.3 OCPP 2.1 — 43 of 91 wired

**Wired:** Authorize, BootNotification, CancelReservation,
ChangeAvailability, ClearCache, ClearChargingProfile, CostUpdated, DataTransfer,
GetBaseReport, GetChargingProfiles, GetCompositeSchedule, GetLocalListVersion,
DeleteCertificate, FirmwareStatusNotification, GetInstalledCertificateIds, GetLog,
GetReport, GetVariables, Heartbeat, InstallCertificate, LogStatusNotification,
NotifyPriorityCharging, NotifyReport, UpdateFirmware,
ReportChargingProfiles, ReservationStatusUpdate, RequestStartTransaction,
RequestStopTransaction, ReserveNow, Reset, SecurityEventNotification,
MeterValues, PullDynamicScheduleUpdate, SendLocalList, SetChargingProfile,
SetNetworkProfile, SetVariables, StatusNotification, TransactionEvent,
TriggerMessage, UnlockConnector, UpdateDynamicSchedule, UsePriorityCharging

**Missing:** AFRRSignal, AdjustPeriodicEventStream, BatterySwap,
CertificateSigned, ChangeTransactionTariff, ClearDERControl, ClearDisplayMessage, ClearTariffs,
ClearVariableMonitoring, ClearedChargingLimit, ClosePeriodicEventStream,
CustomerInformation, Get15118EVCertificate, GetCertificateChainStatus,
GetCertificateStatus, GetDisplayMessages, GetMonitoringReport,
GetPeriodicEventStream, GetTariffs, GetTransactionStatus,
NotifyAllowedEnergyTransfer, NotifyChargingLimit, NotifyCustomerInformation,
NotifyDERAlarm, NotifyDERStartStop, NotifyDisplayMessages,
NotifyEVChargingNeeds, NotifyEVChargingSchedule, NotifyEvent,
NotifyMonitoringReport, NotifyPeriodicEventStream,
NotifySettlement, NotifyWebPaymentStarted, OpenPeriodicEventStream,
PublishFirmware, PublishFirmwareStatusNotification,
ReportDERControl, RequestBatterySwap,
SetDefaultTariff, SetMonitoringBase, SetMonitoringLevel,
SetVariableMonitoring, SignCertificate, UnpublishFirmware, VatNumberValidation

**Inventory reconciliation.** A.2's two lists account for exactly 64 of 64. A.3's account for 88
of the 91 actions `ocpp-client` 0.2.2 generates — the three-message residue is unaudited and is a
job for [H3.5](#103-h3--compliance)'s re-verification sweep, recorded here rather than papered
over. A.1 reconciles at 28 of 28.

**No message is missing an action wrapper any more.** This list used to name
four (SetDisplayMessage, GetDERControl, SetDERControl, UpdateDynamicSchedule)
plus TriggerMessage; the pinned `ocpp-client` is now **0.2.2**, which generates
**91** OCPP 2.1 actions and includes every one of them. Re-verified by counting
`ocpp_2_1_action!`/`ocpp_2_1_send_action!` invocations in the pinned registry
source at the B2.6 commit. The "86 available" figure above and in §2.1 is
therefore stale in this crate's favour — it should read 91, and the remaining
gap is entirely this crate's to close.

### A.4 Other verified figures

| Figure | Value | Source |
|--------|-------|--------|
| Device-model rows in the 2.1 appendix | 438 | `docs/OCPP-2.1/Appendices_CSV_v2.1/dm_components_vars.csv` |
| …marked Required | 122, across 23 components | same |
| …registered by this crate | 48 always-on (`DEFAULT_VARIABLES`) + 11 capability-gated (`CAPABILITY_GATED_VARIABLES`); the 56 rows belonging to unimplemented blocks are deliberately absent | `src/state/device_model.rs`, `src/device_model.rs` |
| 1.6J standard config keys aliased | 23, plus 10 answered from live state | `src/device_model.rs` |
| Security event types in the appendix | 21 | `…/security_events.csv` |
| …modelled in `SecurityEventType` | 21 (F4.1) | `src/state/security_event.rs` |
| …this crate raises itself | 6 | `StartupOfTheDevice`, `ResetOrReboot`, `SettingSystemTime`, `MemoryExhaustion`, `SecurityLogWasCleared`, `ReconfigurationOfSecurityParameters` |
| Protocol trait bounds on `setup()`'s CSMS parameter | 28 (+ `Clone`/`Send`/`Sync`/`'static`) — three added by B2's handlers, which is exactly the growth [C4](#54-c4--builder-refactor)'s builder exists to keep off everyone else's `N` | `src/setup.rs` |
| Test functions in `src/` | 956 | `#[test]` + `#[tokio::test]`, re-counted at the B5.4 commit (942 at B4.2, 920 at B4.1, 909 at F1–F3, 895 at B3.2, 873 at B5.1, 848 at B3.1, 840 at F4, 833 at A5, 827 at B2.6's dynamic-schedule half, 803 at B2.6's priority-charging half, 792 at B8.1, 784 at B2.7, 769 at A9, 760 at A9's selection half, 750 at A7/A8, 746 at B1.8, 732 at B1.7, 730 at B1.6, 725 at E2.5, 717 at B1.2, 694 at B1.5, 684 at B1.3/B1.4, 672 at B1.1, 658 at E2.7, 646 at B2, 564 at E2.10, 496 at the M2 boot-reason commit; an earlier recorded 668 was wrong) |
| Integration tests | 3 | `tests/` (`connect_2_1_websocket`, `memory_budget`, `power_cut_recovery`) |
