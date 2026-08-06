## Description

What does this change do, and why?

## Related issues

Closes #

## Checklist

- [ ] Changes follow TDD: a failing test was written/updated first, then the
      smallest change to make it pass.
- [ ] `cargo test` passes locally.
- [ ] `cargo build` succeeds.
- [ ] If this touches `no_std` code paths, `cargo check --no-default-features --lib` passes.
- [ ] Public API changes (anything exported from `lib.rs`, `hardware`
      traits, `state` types, the runtime) have rustdoc comments covering
      behaviour, invariants, and error conditions.
- [ ] Hardware-facing changes treat hardware calls as fallible and drive
      faults into an explicit `Faulted`/`FaultedSafe` state rather than
      panicking or swallowing errors.
- [ ] Protocol-facing changes model state version-independently and
      downgrade/project it for 2.0.1 / 1.6J rather than leaking older
      protocol limitations into the core state machine.
- [ ] Docs updated if relevant (`README.md`, `docs/ROADMAP.md`, etc).

## Testing

How was this verified? Include relevant test names or manual verification
steps.
