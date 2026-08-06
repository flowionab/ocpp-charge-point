# Contributing

Thanks for your interest in contributing to `ocpp-charge-point`. This crate
is firmware for an EV charge point: it owns charge-point behaviour and
presents an OCPP-capable charge point to a central system (CSMS), targeting
OCPP 2.1 first with OCPP 2.0.1 and OCPP 1.6J supported by downgrading that
same internal state.

By participating, you're expected to follow the [Code of Conduct](CODE_OF_CONDUCT.md).

## Reporting bugs and requesting features

Use the [issue templates](.github/ISSUE_TEMPLATE) to file bug reports and
feature requests. **Do not** open a public issue for security
vulnerabilities — see [SECURITY.md](SECURITY.md) for how to report those
privately.

## Development workflow

All implementation work follows test-driven development:

1. Write or update a failing test that specifies the requested behaviour.
2. Implement the smallest change that makes the test pass.
3. Refactor while keeping the test suite green.

### Commands

* Build: `cargo build`
* Test: `cargo test`
* `no_std` check (if your change touches code paths meant to run without
  `std`): `cargo check --no-default-features --lib`

Run the focused tests for the area you're changing during development, and
the full `cargo test` suite before opening a pull request.

## Architecture guidelines

* Use the actor model: keep state owned by the relevant actor and
  communicate through messages rather than shared mutable state.
* Keep a protocol-version-independent internal state model. Version
  adapters translate that model to OCPP 2.1, 2.0.1, or 1.6J — older
  protocol limitations must never leak into the core state machine.
* Integrators (hardware manufacturers) should only need to implement the
  hardware interfaces exposed by `crate::hardware`. Don't make them deal
  with protocol, networking, or internal state-machine concerns.
* Delegate OCPP networking/transport to the `ocpp-client` crate; don't
  duplicate connection-management or wire-protocol functionality here.
* This is embedded firmware working toward genuine `no_std` (+ `alloc`)
  support. Avoid adding unconditional `std`/`tokio` dependencies to new
  code where a `no_std`-friendly alternative is reasonable — see
  `docs/ROADMAP.md` §0 for what's still open.

## Error handling

Hardware is erratic — sensors glitch, contactors stick, meters stall,
connectors bounce. Treat every hardware binding call as fallible:

* Hardware faults must drive the state machine into an explicit faulted
  state (`ConnectorState::Faulted` / `FaultedSafe`) rather than being
  swallowed or left to panic.
* Never let a hardware error take down the actor or the charge point
  process. Contain failures at the boundary and surface them as state
  transitions and OCPP-visible status (`StatusNotification`,
  `SecurityEventNotification`).
* Prefer fail-safe transitions (e.g. open the contactor, then unlock) over
  fail-open ones when recovering from a fault.

## Documentation

All public (external) APIs — anything exported from `lib.rs`, the
`hardware` traits, `state` types, and the runtime — must carry rustdoc
comments explaining behaviour, invariants, and error conditions, not just
signatures. If a public item's contract isn't obvious from its name and
types, document it.

## Submitting a pull request

1. Fork the repo and create a branch off `main`.
2. Make your change following the TDD workflow above.
3. Ensure `cargo test` (and, if relevant, the `no_std` check) passes.
4. Open a pull request; fill in the PR template checklist.
5. Be responsive to review feedback — small, focused PRs are easiest to
   review and merge.

## License

By contributing, you agree that your contributions will be licensed under
the same terms as the project: MIT OR Apache-2.0 (see
[LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE)).
