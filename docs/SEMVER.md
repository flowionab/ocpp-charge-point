# Semver and MSRV policy

This crate is `0.1.0` and has shipped no tagged release yet - every change described here and in
[`CHANGELOG.md`](../CHANGELOG.md) so far lives under an unreleased `0.1.0`. This document exists
because that has not been a low-cost fact for integrators: recent weeks alone changed
`ChargePoint::capabilities()`, `Connector::set_current_limit`, `hardware::Storage`,
`connect_and_setup`'s parameter list, `ChargePointBuilder::firmware_updates`'s parameter list, and
`ChargePointEffect`'s derives - each one a real edit to code an integrator wrote. Pre-1.0 semver
gives license to break things; it does not excuse skipping a stated policy about what "breaking"
means here, or a record of what already broke.

## What semver means before 1.0

This crate follows [Semantic Versioning](https://semver.org/), with the standard pre-1.0
reading: for a `0.MINOR.PATCH` version,

- **`MINOR` bumps may break the public API.** Cargo treats `0.1.x` and `0.2.x` as incompatible by
  default, exactly as it treats `1.x` and `2.x` post-1.0. This is where the breaking changes in
  the changelog land.
- **`PATCH` bumps are additive/non-breaking**: bug fixes, new opt-in functionality (a new trait
  with a `No*`/default-safe implementation, a new `Capabilities` field via
  `#[non_exhaustive]` + `Default`, a new enum variant behind `#[non_exhaustive]`), and
  documentation.
- There is currently no compatibility promise *between* `0.MINOR` releases at all. That is a
  cost this document exists to eventually reduce (see
  [Where this is going](#where-this-is-going)), not to hide.

## What counts as breaking for a trait an integrator implements

Integrators only ever implement `crate::hardware` traits (`ChargePoint`, `Evse`, `Connector`,
`Storage`, `FileTransfer`, `FirmwareInstaller`, `FirmwareVerifier`, `FirmwarePublisher`,
`CertificateStore`, `KeyStore`, `SoftwareCrypto`, `OcspClient`, `Display`, `BatterySwapStation`,
`PaymentTerminal`, `Watchdog`, and any that join them) - see [`docs/INTEGRATORS.md`](INTEGRATORS.md).
Every other public item (state types, protocol adapters, the actor, the builder) is this crate's
own responsibility to evolve; a trait an integrator's `impl` block must satisfy is held to a
stricter bar because breaking it means someone else's code stops compiling with no warning period.

**Breaking** (requires a `MINOR` bump):

- Adding a **required** method (no default body) to an existing trait - every existing `impl`
  now fails to compile.
- Changing an existing method's signature: parameter types, return type, added/removed
  parameters, or a new bound on `Self`/an associated type.
- Removing a trait, a method, or a `No*` default implementation.
- Changing a trait's supertraits (adding a new supertrait bound has the same effect as adding a
  required method).
- Changing what a documented invariant promises (e.g. a method that was infallible becoming
  fallible, or vice versa) even if the signature is unchanged in Rust's eyes - this is a
  behavioural break, and gets called out as one in the changelog even though `cargo semver-checks`
  would not flag it.

**Not breaking** (fine in a `PATCH`, though this crate is not yet making `PATCH`-vs-`MINOR`
promises in practice - see above):

- Adding a method **with a default body** to an existing trait - existing `impl`s keep compiling
  unchanged, and get the new behaviour only if they override it.
- Adding a new, independent trait, provided it ships with a `No*`/no-op default so existing
  integrators are not forced to implement it.
- Adding a new field to a `#[non_exhaustive]` struct (see [`Capabilities`](../src/hardware/capabilities.rs)
  for the pattern this crate uses precisely so this is possible) or a new variant to a
  `#[non_exhaustive]` enum.
- Loosening a bound (e.g. `T: Send` to no bound) or widening an accepted input type.
- Anything entirely inside a private module, or behind a Cargo feature an integrator did not
  enable.

This mirrors ordinary Rust semver-for-traits guidance; the only thing specific to this crate is
which traits the rule applies to (`crate::hardware`, plus the handler traits like
`ClearCacheHandler` that `ChargePointBuilder` registration methods bound against - both are
"things an integrator's code must satisfy").

## MSRV

`rust-version = "1.88"` in `Cargo.toml`, enforced on every push by the `msrv` CI job. The comment
next to it in `Cargo.toml` records *why* 1.88 specifically: `src/state/` uses let-chains (stable
only from 1.88), and by the time that was measured, several transitive dependencies already
required 1.86/1.87 anyway - 1.88 cost nothing additional at the time it was set.

**An MSRV bump is treated as breaking** for the same reason a trait method addition is: it can
turn a project that compiled yesterday into one that does not, with no code change on the
integrator's side. Concretely:

- Raising `rust-version` is only done in a `MINOR` release, called out explicitly in the
  changelog's own section for it (see [`CHANGELOG.md`](../CHANGELOG.md)'s format), never folded
  silently into an "also bumped some deps" entry.
- Lowering `rust-version` (a dependency drops its own MSRV, or a feature gate that needed a newer
  language feature is removed) is not breaking and can happen in any release.
- Embedded targets (`--no-default-features`, `thumbv7em-none-eabihf`) get the same MSRV as the
  `std` build - this crate does not maintain two MSRVs. `scripts/`'s CI-mirroring checks (see
  `CLAUDE.md`'s "Hard-won lessons" for why the MCU target needs its own `cargo check`, not just
  `--no-default-features --lib` on the host) run at the same Rust version as everything else.

## Cross-reference: the hardware trait surface freezes at 1.0

[`docs/PRODUCTION-ROADMAP.md`](PRODUCTION-ROADMAP.md)'s **H5.5** names "hardware trait surface
frozen" as a 1.0 criterion, landing every planned breaking change first (smart-charging's
remaining pieces, the runtime-capability-declaration follow-ups, and `Storage`'s outstanding
gaps). Read together with this document: **before 1.0, expect the `crate::hardware` traits in
particular to keep changing** - they are exactly the surface this policy holds to the strictest
bar, and exactly the surface H5.5 is waiting to stop changing before this crate can respect the
stronger promise a `1.x` version implies (no breaking change without a `2.0`). Pinning an exact
`0.MINOR` version and reading the changelog before bumping it is the realistic integrator posture
until then.

## Where this is going

Once the hardware trait surface is frozen (H5.5) and a 1.0 is cut, this crate moves to the
standard post-1.0 reading of semver: `PATCH` for fixes, `MINOR` for additive/non-breaking
changes, `MAJOR` for anything in the "breaking" list above. Nothing about *what counts as
breaking* changes at that point - only the version-number consequence of triggering it does.

## Regenerating the "what actually broke" record

[`CHANGELOG.md`](../CHANGELOG.md) is reconstructed from `git log`, not hand-maintained prose. To
check it for a specific suspected break or extend it:

```sh
# Full history of a public item's defining file, to see every change to it:
git log --oneline -- src/hardware/mod.rs

# Search commit messages (subject + body) for an explicit breaking-change note:
git log --all --grep="breaking" -i --format="%H %s"

# Diff a specific commit's effect on a trait signature:
git show <commit> -- src/builder.rs
```

Every entry in the changelog's "Breaking" bullets was verified this way against the actual diff,
not inferred from the commit subject line alone - do the same before adding a new one.
