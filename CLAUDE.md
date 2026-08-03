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
  `alloc`) support so it runs on microcontrollers without an OS. This is
  **not** the current state — `tokio` is still a hard, unconditional
  dependency in several places — but new code should avoid adding further
  unconditional `std`/`tokio` dependencies where a no_std-friendly
  alternative is reasonable, and existing hard dependencies should be
  treated as a tracked gap (see `docs/ROADMAP.md` §0), not the intended
  end state.

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
