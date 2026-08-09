# Development Guidance

## Product goal

This crate is firmware for an EV charge point. It owns charge-point behaviour,
including lifecycle and charging state, and presents an OCPP-capable charge
point to a central system (CSMS).

The primary protocol target is a fully compliant **OCPP 2.1** implementation.
Model the firmware state and behaviour for OCPP 2.1 first, then support
OCPP 2.0.1 and OCPP 1.6J by projecting or downgrading that state to the
capabilities of the negotiated protocol version — never the other way
around.

Integrators (hardware manufacturers) should only ever need to supply
hardware bindings (`crate::hardware`). Everything else — protocol handling,
state machines, transaction lifecycle, networking — is the crate's
responsibility.

## Architecture

- Use the actor model heavily. Keep state owned by the relevant actor and
  communicate with it through messages rather than shared mutable state.
- The crate is responsible for charge-point, EVSE, connector, transaction,
  and related protocol-facing state and behaviour.
- Keep a protocol-version-independent internal state model. Version adapters
  must translate that model to OCPP 2.1, OCPP 2.0.1, or OCPP 1.6J rather than
  making older protocol limitations leak into the core state machine.
- Integrators should only need to implement the hardware interfaces exposed
  by `crate::hardware`. Do not make them implement protocol, networking, or
  internal state-machine concerns.
- Delegate OCPP networking and transport concerns to the `ocpp-client` crate.
  Do not duplicate connection-management or wire-protocol functionality
  here. `ocpp-client` is the network bridge; this crate is the application
  layer sitting on top of it.
- A charge point must be reachable by a CSMS speaking OCPP 1.6J, 2.0.1, or
  2.1. The negotiated version is a property of the connection, not of the
  firmware's internal model — the same internal state must be representable
  (with graceful downgrade) across all three.
- This is embedded firmware: the long-term goal is genuine `no_std` (+
  `alloc`) support so it runs on microcontrollers without an OS, and this
  is now real: `cargo check --no-default-features --lib` compiles under
  `#![no_std]` (channels are `embassy-sync`-backed, not `tokio::sync` -
  see `src/sync.rs`), and `tokio` is a fully optional dependency behind a
  `tokio-runtime` feature (`TokioExecutor`/`TokioBackoff` require it
  specifically). `tokio-runtime` (which implies `std`) is in this crate's
  `default` features for zero-config ergonomics - true no_std requires
  `--no-default-features` plus registering a `critical-section` backend
  and supplying your own `Executor`/`Backoff`/`Clock`. New code should
  avoid adding unconditional `std`/`tokio` dependencies where a
  no_std-friendly alternative is reasonable (see `docs/ROADMAP.md` §0 for
  what's still open, e.g. `ChargePointActor::spawn`'s exact bound choices).

## Error handling

Hardware is erratic: sensors glitch, contactors stick, meters stall,
connectors bounce. Treat every hardware binding call as fallible and assume
it can fail, time out, or report an inconsistent state at any point.

- Hardware faults must drive the state machine into an explicit faulted
  state (see `ConnectorState::Faulted` / `FaultedSafe`) rather than being
  swallowed or left to panic.
- Never let a hardware error take down the actor or the charge point
  process. Contain failures at the boundary and surface them as state
  transitions and OCPP-visible status (e.g. `StatusNotification`,
  `SecurityEventNotification`) instead.
- Prefer fail-safe transitions (e.g. open the contactor, then unlock) over
  fail-open ones when recovering from a fault.

## Logging and tracing

A charge point is a box on a wall that nobody can attach a debugger to. The
log is the only diagnostic instrument the field engineer has, and it competes
for flash, bandwidth and battery with the job the device is actually doing.
Both facts drive the rules below.

### Levels

The level is a promise about *who reads it*, not about how the author felt.

- `error!` — a bug in this firmware, or a fault that lost data. Someone should
  open an issue. Never use it for a hardware fault the state machine already
  handles: that is what `ConnectorState::Faulted` and `SecurityEventNotification`
  are for.
- `warn!` — degraded but handled: a CSMS value was rejected or clamped, a
  persisted record failed to decode, a retry was needed. The charge point kept
  working and the operator should know why it is behaving oddly.
- `info!` — station-wide lifecycle only: booted, registered, went available or
  unavailable, faulted, connected to or lost the CSMS, a security event. An
  operator must be able to leave a site running at `INFO` indefinitely, so
  nothing per-connector, per-message or per-meter-sample belongs here.
- `debug!` — one line per event applied, per OCPP message handled, per
  hardware command dispatched. Names *what* happened, never the payload.
- `trace!` — the payload: full request/response bodies, the whole
  `ChargePointState`, decoded certificates. Assume nobody runs this in
  production and that every line is expensive.

The single sharpest rule: **a `{:?}` of a large type belongs at `trace!`.**
`ChargePointState` renders to roughly eight kilobytes. Logging it per event, as
the actor once did at `INFO`, is most of the cost of having logs at all on an
MCU.

### Fields over prose

Prefer a low-cardinality `&'static str` field to an interpolated message, so a
log can be filtered and aggregated rather than grepped:

```rust
tracing::debug!(event = event.name(), "applying charge point event");   // yes
tracing::debug!("applying {:?}", event);                                // no
```

`ChargePointEvent::name`/`evse_id`/`connector_id` and `ChargePointEffect::name`
exist for exactly this. Their matches are exhaustive with no wildcard arm on
purpose — a new variant must be a compile error, not a mislabelled log line.

### `#[instrument]`

Always `#[instrument(skip_all, ...)]`, then add back the few fields worth
recording. The bare attribute records *every* argument via `Debug`, which
re-creates both problems above at once: kilobyte payloads and personal data.

```rust
#[instrument(skip_all, fields(evse_id, connector_id))]   // yes
#[instrument]                                            // no
```

Never hold a span guard (`.entered()`) across an `.await`. Either use
`span.in_scope(..)` around the synchronous part, or attach the span to the
future with `.instrument(span)`.

### Personal data

An `IdToken` value is the number on the card in a driver's wallet. `IdToken`'s
`Debug` is hand-written to redact it (see `crate::state::IdToken`), so anything
containing one is safe to log by construction; do not undo that by logging
`id_token.value` directly. The same applies to anything else that identifies a
driver — settlement identifiers, VAT numbers, `CustomerInformation` payloads.
The off-by-default `unredacted-logs` feature restores full values for local
bring-up only.

### `no_std`

`tracing` is a dependency with `default-features = false`; keep it that way.
Macros and `#[instrument]` both work under `no_std`, and a callsite with no
subscriber installed costs an atomic load, so instrumenting a path is cheap —
*formatting* it is what is not. That is what the level rules are protecting.

## Documentation

All public (external) APIs — anything exported from `lib.rs`, the
`hardware` traits, `state` types, and the runtime — must carry rustdoc
comments explaining behaviour, invariants, and error conditions, not just
signatures. If a public item's contract isn't obvious from its name and
types, document it.

## Development workflow

All implementation work must follow test-driven development (TDD):

1. Write or update a failing test that specifies the requested behaviour.
2. Implement the smallest change that makes the test pass.
3. Refactor while keeping the test suite green.

Run the focused tests during development and the relevant broader test
suite (`cargo test`) before handing off a change.

## Commands

- Build: `cargo build`
- Test: `cargo test`
