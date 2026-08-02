# Development Guidance

## Product goal

This crate is firmware for an EV charge point. It owns charge-point behaviour,
including lifecycle and charging state, and presents an OCPP-capable charge
point to a central system.

The primary protocol target is OCPP 2.1. Model the firmware state and behaviour
for OCPP 2.1 first, then support OCPP 2.0.1 and OCPP 1.6J by projecting or
downgrading that state to the capabilities of the negotiated protocol version.

## Architecture

- Use the actor model heavily. Keep state owned by the relevant actor and
  communicate with it through messages rather than shared mutable state.
- The crate is responsible for charge-point, EVSE, connector, transaction, and
  related protocol-facing state and behaviour.
- Keep a protocol-version-independent internal state model. Version adapters
  must translate that model to OCPP 2.1, OCPP 2.0.1, or OCPP 1.6J rather than
  making older protocol limitations leak into the core state machine.
- Integrators should only need to implement the hardware interfaces exposed by
  `crate::hardware`. Do not make them implement protocol, networking, or
  internal state-machine concerns.
- Delegate OCPP networking and transport concerns to the `ocpp-client` crate.
  Do not duplicate connection-management or wire-protocol functionality here.

## Development workflow

All implementation work must follow test-driven development (TDD):

1. Write or update a failing test that specifies the requested behaviour.
2. Implement the smallest change that makes the test pass.
3. Refactor while keeping the test suite green.

Run the focused tests during development and the relevant broader test suite
before handing off a change.
