# Migrating to `ocpp-client` 0.4.0

> **Status: done** (2026-08-08), except workstream D2.2, which is scoped but deliberately not
> taken — see [What was not done](#what-was-not-done). The plan below is kept as the record of
> what was measured and decided; [What actually happened](#what-actually-happened) records where
> reality differed from it.
>
> **Now on 0.5.0.** After the 0.4.0 port landed, this crate moved straight on to `ocpp-client`
> 0.5.0 (`ocpp-types` 0.3.0). That step needed **no source changes at all** — see
> [Moving on to 0.5.0](#moving-on-to-050). Everything below still describes the 0.4.0 migration,
> which is where all the work was.

Plan for moving this crate from `ocpp-client` 0.2.2 (pinned in `Cargo.lock`) to
0.4.0, which tracks `ocpp-types` 0.1.3 → 0.2.0.

Every number below was measured, not estimated: the bump was applied to
`Cargo.toml` locally, `cargo check` was run, and the diagnostics were
classified. See ["How this was measured"](#how-this-was-measured).

---

## Verdict

**A large but mechanical migration, plus one silent behavioural regression that
must be fixed by hand.**

- **337 compile errors** across 34 files (`--all-features --all-targets`), all
  falling into **five** classes, four of which are mechanical.
- **Zero public-API impact.** No `ocpp-types` type appears in any public
  signature or re-export of this crate (`grep` for `pub` items mentioning
  `ocpp_types` returns nothing; the integration tests, `examples/simple.rs` and
  `tools/flash-probe` never name a wire type). Integrators supplying only
  `crate::hardware` bindings see no change, so this is an internal migration and
  does not force a breaking release of `ocpp-charge-point`.
- **One change compiles cleanly and breaks the firmware**: `disconnect()` is now
  sticky, and `network_switch.rs` uses it to force a redial. See
  [§1](#1-the-silent-regression-fix-this-first).
- It closes two roadmap items that are currently marked 🔒 blocked upstream
  (**A4**, **D2.2**) and inherits five upstream bug fixes that matter for a
  device that runs for weeks against an intermittent CSMS.

Recommended shape: **one PR for the mechanical port** (green build, no
behaviour change), then **separate PRs** for the newly unblocked capabilities.
Do not mix them — the mechanical diff is ~350 sites and will bury anything else.

---

## 1. The silent regression — fix this first

`src/network_switch.rs:485-507` implements `ConnectionCloser::close_connection`
for all three clients as `self.disconnect().await`, and
`run_network_profile_switching` (`:447`) calls it precisely to **force a redial
through the retargeted `ConnectionTarget`** — that is how an OCPP 2.x
`SetNetworkProfile` failover moves the charge point to a different CSMS address
without tearing down the `Client` and losing its handlers.

In 0.3.0, `disconnect()` became sticky and now outranks every automatic recovery
path: the read loop exits instead of redialling, the keepalive task stops,
`force_reconnect()` becomes a no-op, and subsequent `call`/`send_*` return
`ClientError::Closed`.

**Consequence:** after the bump, the first network-profile switch takes the
charge point permanently offline. It requires a reboot to recover.

**This produces no compile error.** It is a pure semantic change to a call that
still type-checks.

**Fix:** `ConnectionCloser::close_connection` must call
`Client::force_reconnect()` — added in 0.3.0 for exactly this use case — not
`disconnect()`. Per the TDD workflow in `CLAUDE.md`, write the failing test
first: `network_switch.rs`'s `RecordingCloser` tests (`:655`) assert the closer
was *called*, which stays green either way, so the new test needs to assert the
client is still usable (not `is_closed()`) after a switch.

Also worth renaming the trait method — `close_connection` will no longer close
anything — but that is a public trait (`pub trait ConnectionCloser`), so treat
the rename as a separate, deliberate API change rather than smuggling it into
the migration PR.

---

## 2. What 0.4.0 brings

### 2.1 Bug fixes inherited for free (all from 0.3.0)

These are all live defects in the firmware today:

| Fix | Why it matters here |
|---|---|
| **Reconnect hot loop.** A peer that accepted the connection and then dropped it was redialled with *no delay at all* — upstream measured ~9,900 connections in 2 seconds. | The triggers are ordinary (CSMS rejecting at the application layer, LB with no live backend). A fleet doing this at once is a self-inflicted DoS. |
| **Every timed-out request leaked a pending-response entry.** | Unbounded map growth on a charge point running for weeks against an intermittent CSMS — exactly this product's duty cycle. |
| **Replacing a handler leaked its task.** | This crate re-registers handlers on reconnect resync paths. |
| **`disconnect()` was undone by the reconnector.** | With default `ConnectOptions` there was previously no way to stop a client at all. |
| **Ping/pong matched positionally**, so one timed-out ping desynced every later ping permanently. | Latent today (nothing schedules pings); would be hit constantly once A4 lands. |
| **Reconnect delays are now jittered** (`ReconnectPolicy::jitter`, default `true`). | Stops a whole fleet retrying the same CSMS in lockstep. Note this crate already models `RetryBackOffRandomRange` itself in `network_switch.rs:474` — check the two do not compound. |

### 2.2 Roadmap items this unblocks

- **A4 — WebSocket keepalive** (`PRODUCTION-ROADMAP.md:137`, 🔒). The blocker
  was literal: "`ocpp-client` 0.2.1's `ConnectOptions` has no ping-interval
  field and its WebSocket transport only *replies* to pings; there is nothing
  to configure from here." 0.3.0 adds `KeepalivePolicy`/`KeepaliveBehavior`,
  `ConnectOptions::keepalive`, and — the part A4 actually needs —
  `Client::ping_interval()`/`set_ping_interval()`, both non-`async` so a
  `GetVariables`/`SetVariables` handler can call them directly. That retires
  the hardcoded `0` for `OCPPCommCtrlr.WebSocketPingInterval`
  (`device_model.rs:1652`, `state/device_model.rs:725`, and the test at
  `connect.rs:502` that documents why it is `0`).
- **D2.2 — 1.6J security whitepaper extensions**
  (`PRODUCTION-ROADMAP.md:1624`, 🔒). All eleven are now generated and wired:
  `CertificateSigned`, `DeleteCertificate`, `ExtendedTriggerMessage`,
  `GetInstalledCertificateIds`, `GetLog`, `InstallCertificate`,
  `LogStatusNotification`, `SecurityEventNotification`, `SignCertificate`,
  `SignedFirmwareStatusNotification`, `SignedUpdateFirmware`. 1.6 goes from 28
  to 39 actions. The doc comments at `firmware/ocpp_1_6.rs:15` and
  `diagnostics/ocpp_1_6.rs:4` that say these do not exist upstream become
  stale.
- **D3.1 — pin a tested version range.** Do it in this PR: the current
  `version = "0.2.2"` requirement accepts any 0.2.x.

### 2.3 What it does *not* fix

- **D2.3 — `ChargingProfile` is 56 KB by value.** Verified against
  `ocpp-types` 0.2.0: `ChargingSchedule` still holds
  `Option<AbsolutePriceSchedule<_>>` and `Option<PriceLevelSchedule<_>>`
  inline, not boxed. The 48 `heapless::String<N>` → `String` conversions
  shrink *some* structs materially (`MessageContent.content` drops from 1000
  inline bytes to a 24-byte `String`), but the structural cause D2.3 names is
  untouched. Re-measure after the port before editing that roadmap row.
- 2.0.1's `SecurityEventNotification` was already fixed in 0.2.2 — nothing new
  here (`PRODUCTION-ROADMAP.md:3147` is already correct).

### 2.4 New capability, no obligation

- **`customData` is now a type parameter** on 2.x markers and message types,
  defaulting to `crate::NoCustomData`. A vendor extension richer than a bare
  `vendorId` can be read/written through `Client::call`/`Client::on` without
  hand-writing an `Action` impl. This crate does not need it — but it is the
  cause of most of the compile errors (§3.1).
- **`chrono` feature** on `ocpp-client`, forwarding to `ocpp-types`. This is
  the clean path for §3.2.

---

## 3. Blast radius, by error class

337 errors, `--all-features --all-targets`. Treat this as a **floor**: rustc
reports one wave per compile, and each fix uncovers more.

| # | Class | Errors | Nature |
|---|---|---|---|
| 3.1 | `customData` type parameter on 2.x types | ~185 | Mechanical, one-line-per-module fix available |
| 3.2 | `dateTime`: `String` → `OcppTimestamp` | ~63 | Mechanical, but needs one design decision |
| 3.3 | 48 fields: `heapless::String<N>` → `String` | 8 | Trivial |
| 3.4 | `RequestedMessage` renamed | 1 import + ~10 uses | Trivial |
| 3.5 | `ReconnectPolicy { jitter }` | 1 | Trivial |

Top files: `smart_charging/ocpp_2_1.rs` (33), `device_model.rs` (26),
`smart_charging/ocpp_2_0_1.rs` (24), `variable_monitoring.rs` (22),
`tariff.rs` (18), `reporting.rs` (16), `remote_control.rs` (15),
`transactions.rs` (14).

### 3.1 The `customData` parameter — and how to not touch 185 sites

The trap: the struct default is `NoCustomData`, but `ocpp-client`'s generated
`send_*`/`on_*`/`wait_for_*` methods are **concrete at `CustomData`**. So every
bare `AuthorizeRequest` this crate writes now means `AuthorizeRequest<NoCustomData>`
and mismatches:

```
expected struct `...v21::NotifyReportRequest<...v21::common::CustomData>`
   found struct `...v21::NotifyReportRequest<NoCustomData>`
```

This surfaces as `E0631` (101, closure argument mismatch in `on_*` handlers),
`E0271` (42, async-block return type), `E0308` (~28, direct argument), and
`E0599` (13, where a `.map(fn_item)` no longer resolves so `.collect()` loses
its `Iterator` bound). The changelog's advice — annotate `let` bindings — covers
only a fraction of these.

**Do not annotate the call sites.** The crate names ~210 distinct v21 types and
~187 v201 types across 113 `use ocpp_client::ocpp_types::v21|v201::{...}` blocks,
almost all inside per-version `mod ocpp_2_1 { … }` / `mod ocpp_2_0_1 { … }`
bodies (see `reporting.rs:831,1183,1466,1700` for the pattern). Instead add a
crate-internal alias module:

```rust
// src/wire.rs
pub(crate) mod v21 {
    use ocpp_client::ocpp_types::v21::common::CustomData;

    pub(crate) use ocpp_client::ocpp_types::v21::*;            // enums, non-generic types

    // Shadow every generic type with the CustomData-bound alias.
    pub(crate) type AuthorizeRequest = ocpp_client::ocpp_types::v21::AuthorizeRequest<CustomData>;
    pub(crate) type NotifyReportRequest = ocpp_client::ocpp_types::v21::NotifyReportRequest<CustomData>;
    // … generated
}
```

An explicit item shadows a glob import, so only the generic types need listing,
and that list is generated mechanically by scanning `ocpp-types` for
`pub struct X<CustomDataType = ...>`. Then each import block changes from
`use ocpp_client::ocpp_types::v21::{A, B, C}` to `use crate::wire::v21::{A, B, C}`
— **113 single-line edits instead of ~185 scattered annotations**, and every
downstream site (struct literals, closure parameter types, helper-fn signatures,
`impl` blocks) compiles unchanged. Type aliases to fully-applied generic structs
work in struct-expression and pattern position, so nothing else moves.

It also fixes the inference problem the changelog warns about: a `let r = AuthorizeRequest { .. }`
using the alias resolves to `<CustomData>` with no annotation.

`v16` needs no aliases — the parameter is 2.x only — so `crate::wire::v16` is a
plain re-export, added for symmetry so all three version modules read the same.

Keep the module `pub(crate)`. Re-exporting it would put `ocpp-types` back in this
crate's public API and hand integrators a protocol concern, against `CLAUDE.md`.

### 3.2 `OcppTimestamp`

Every `dateTime` field is now `ocpp_types::OcppTimestamp` — a validated instant
(`secs: i64`, `nanos: u32`, `offset_minutes: i16`), not a string. Errors split
into producers (23 × `expected OcppTimestamp, found String`, from the 55
`.to_rfc3339()` call sites) and consumers (16 × `&Option<String>` vs
`&Option<OcppTimestamp>`, 12 × `&str` vs `&OcppTimestamp`, plus
`Option::as_deref` no longer applying at `firmware/ocpp_2_0_1.rs:34`,
`firmware/ocpp_2_1.rs:34`, `tariff.rs:650`).

**Decision: enable `ocpp-client/chrono` and convert with `From`/`Into`.**
`ocpp-types` 0.2.0 provides lossless `From<DateTime<Utc>> for OcppTimestamp` and
the reverse (nanosecond precision both ways), plus `FixedOffset` variants that
preserve the peer's written UTC offset. This crate already keeps time as
`chrono::DateTime<Utc>` internally, so:

- producers: `t.to_rfc3339()` → `t.into()`
- consumers: `OcppTimestamp` → `DateTime<Utc>` at the adapter boundary, leaving
  every internal signature that currently takes `&str`/`&Option<String>`
  unchanged in *shape*, just retyped.

Cost check for the `no_std` goal in `CLAUDE.md`: `ocpp-types`' `chrono`
dependency is `optional = true, default-features = false`, and this crate
already depends on `chrono` with `alloc` + `serde`. So add
`chrono = ["ocpp-client/chrono"]` to this crate's feature list, enabled
unconditionally alongside the version features — no `std` leaks in.

**Two behaviour changes to test, not just compile:**

1. **Inbound `dateTime` is now validated.** A malformed timestamp from the CSMS
   surfaces as `ClientError::Decode` where it was previously handed through as a
   string. Per `CLAUDE.md`'s error-handling rules this must not take down the
   actor — check every inbound path that previously tolerated a garbage
   timestamp, and decide per message whether it is a `CallError` or a faulted
   transition.
2. **Timestamps compare as instants.** Two values naming the same moment in
   different UTC offsets are now `==`, where the string comparison said
   otherwise. Anything doing equality or ordering on a wire timestamp
   (transaction event dedup, `GetChargingProfiles` windows, monitoring) changes
   meaning — usually for the better, but assert it.

### 3.3 – 3.5 The small ones

- **heapless → `String`** (8 errors, certificates/CSRs/OCSP/`MessageContent.content`
  and friends): `"...".try_into().unwrap()` → `"...".into()`. Only 8 of this
  crate's 208 `try_into`/`try_from` sites are affected; keep the direct
  `heapless = "0.8"` dependency.
- **`v16::common::RequestedMessage` → `TriggerMessageRequestRequestedMessage`**
  (`remote_control.rs:1840` + ~10 uses at `:1878-2063`). Pure rename, no variants
  changed. Import it `as RequestedMessage` to keep the diff to one line.
- **`ReconnectPolicy` gained `jitter: bool`** (`connect.rs:265`). Take the new
  default (`true`) unless `RetryBackOffRandomRange` already covers it — decide
  explicitly, do not default by accident.

---

## 4. Suggested sequencing

### PR 1 — mechanical port (no behaviour change intended)

1. `Cargo.toml`: `ocpp-client = { version = "0.4.0", default-features = false }`,
   add the `chrono` feature forward. Pin a tested range while here (**D3.1**).
2. Generate `src/wire.rs` from `ocpp-types` 0.2.0; swap the 113 import blocks
   (§3.1). Expect this alone to clear ~185 errors.
3. Timestamps (§3.2): boundary conversions, then the two behavioural tests.
4. §3.3 – 3.5.
5. **Fix `ConnectionCloser` to use `force_reconnect()` (§1)** — test first.
6. Green gates: `cargo test`, then the CI feature matrix, `cargo clippy`, the
   `msrv` job, and the bare-metal check
   (`cargo check --target thumbv7em-none-eabihf --no-default-features`). The
   `no_std` build is the one most likely to surprise — `ocpp-types` made `alloc`
   its own default in 0.2.0, and this crate names features explicitly, so it
   should be inert, but verify rather than assume.

Note that `--lib` does **not** type-check `#[cfg(test)] mod tests` — there are
127 such modules in `src/`. Always measure progress with `--all-targets`.

### PR 2 — A4, WebSocket keepalive

Wire `ConnectOptions::keepalive` and `Client::set_ping_interval()` to
`OCPPCommCtrlr.WebSocketPingInterval`, replacing the registered `0`. Two things
to decide deliberately:

- **0.3.0 changed `ConnectOptions::default()` to enable keepalive** (60 s
  interval, one missed pong tolerated). This crate builds its own
  `ConnectOptions` in `connect.rs:255-270`, so it should set the value from the
  device model rather than inherit the default silently. Note the lower-level
  `Client::from_transport*` constructors still default to *disabled*.
- `connect.rs:97` documents the existing rule that a caller who supplied
  `ConnectOptions` is configuring the transport deliberately and is not
  overridden. Keepalive must follow the same rule.

Then update `PRODUCTION-ROADMAP.md` A4 (🔒 → ✅) and the now-stale note at
`:3066`.

### PR 3+ — D2.2, the eleven 1.6J security actions

A genuine workstream, not a port: it interacts with security profiles
(workstream F), `firmware/ocpp_1_6.rs`, `diagnostics/ocpp_1_6.rs` and the
certificate store. Scope separately. `PRODUCTION-ROADMAP.md:1629`'s open
question — "contribute them upstream, or declare 1.6J security out of scope" —
is now answered by upstream; re-word that row rather than deleting it.

### Docs to refresh afterwards

- `docs/UPSTREAM-GAPS.md` — its version table still says `ocpp-types` 0.1.2 /
  `ocpp-client` 0.2.0 pinned, which is already stale against `Cargo.lock`; the
  1.6J coverage table (`:186`) is invalidated outright by D2.2.
- `PRODUCTION-ROADMAP.md` rows A4, D2.2, D2.3, D3.1, and the 1.6J
  action counts (28 → 39).
- The stale "does not generate" comments at `firmware/ocpp_1_6.rs:15` and
  `diagnostics/ocpp_1_6.rs:4`.

---

## How this was measured

```sh
# Upstream sources, read directly rather than from docs.rs:
curl -sSL -o - https://static.crates.io/crates/ocpp-client/ocpp-client-0.4.0.crate | tar xz
curl -sSL -o - https://static.crates.io/crates/ocpp-types/ocpp-types-0.2.0.crate  | tar xz
diff -rq ocpp-client-0.2.2 ocpp-client-0.4.0     # + CHANGELOG.md, new in 0.4.0

# Error counts, from a real build of this crate with the bump applied:
sed -i '' 's/ocpp-client = { version = "0.2.2"/ocpp-client = { version = "0.4.0"/' Cargo.toml
cargo check --all-features --lib         --message-format=short   # 291
cargo check --all-features --all-targets --message-format=short   # 337
git checkout Cargo.toml Cargo.lock
```

Error classes were derived by grouping the `--message-format=short` output by
error code and by normalised message text; the `customData` and `OcppTimestamp`
diagnoses were confirmed against full rustc output and the generated
`ocpp-types` 0.2.0 sources (`src/v21/authorize_request.rs`,
`src/timestamp.rs`), not inferred from the changelog.


---

## What actually happened

The mechanical port went as planned. Three things did not.

### The `crate::wire` alias module needed two hand-written entries

The generator emits `pub type X = ...::X<CustomData>` for every struct declaring
`CustomDataType`, which assumes that parameter comes first. It does for all 305 types except two
per version: `DataTransferRequest`/`DataTransferResponse` declare their *payload* parameter first
(`DataTransferRequest<DataTransferRequestData = (), CustomDataType = NoCustomData>`).

### `DataTransfer` — an upstream defect in `ocpp-client` 0.4.0

`ocpp-client`'s action macro appends the customData type positionally (`$req<C>`,
`src/ocpp_2_1/actions.rs:101`). For `DataTransfer` that lands on the wrong parameter, so
`send_data_transfer` takes `DataTransferRequest<CustomData, NoCustomData>` — the free-form vendor
payload typed as the `{vendorId}` object, and the real `customData` discarded as `NoCustomData`.

Harmless here today: `crate::data_transfer` sends `data: None`/`custom_data: None` on 2.x and
both skip-serialize when `None`, so the wire bytes are unchanged. The aliases in `crate::wire`
have to name what the client's signatures actually are, and say why. **Worth reporting upstream**
— it silently removes `DataTransfer`'s customData support and mistypes its payload.

### The `disconnect()` regression was real, and there is a second one behind it

`tests/network_profile_switch.rs` — which already encoded the contract — failed exactly as
predicted before the fix: *"the charge point never connected to the address the profile named"*.
`ConnectionCloser` now calls `force_reconnect()`.

That exposed a second, subtler problem. `Client::force_reconnect` tears down whichever connection
is current when the read loop observes the request; it carries no epoch. When the CSMS closes its
own side right after answering `SetNetworkProfile` — which the test's CSMS does, and which is
ordinary behaviour — the client's own redial and the forced one race, and the forced one can land
on the connection the natural redial just established. Two mitigations, one workaround:

- **`ConnectionTarget` now counts redials**, and `run_network_profile_switching` skips the force
  when the connection has already moved during the grace period. This covers the common case but
  not a dead heat, because both delays are ~1 s from nearly the same instant.
- **`register_until_accepted` now backs off exponentially** from
  `FIRST_RETRY_INTERVAL_SECS` (1 s) to `DEFAULT_RETRY_INTERVAL_SECS` (30 s) on transport-level
  failures, instead of waiting a flat 30 s. A `BootNotification` that raced a redial is retried
  promptly; a CSMS that is genuinely down still reaches the same half-minute cadence. This is
  worth having independently of the race: 30 s unregistered on a healthy socket is 30 s during
  which the station cannot start a transaction.
- **The integration test now tolerates more than one arriving connection.** What is guaranteed is
  that the charge point ends up connected to the new address and boots there, not that it gets
  there in one attempt. It passes in ~4.5 s.

A proper fix belongs upstream: `force_reconnect` should carry a generation so a request issued
against connection *n* cannot tear down connection *n+1*.

## What was not done

**D2.2 — the eleven OCPP 1.6 security-whitepaper actions.** `ocpp-client` 0.4.0 makes them
available, which is what the roadmap row was blocked on, but wiring them is a functional-block
workstream rather than part of a port: handler traits, hardware bindings for certificate and
signed-firmware handling, `crate::refusal` entries, per-action tests, and an overlap with
workstream F (security profiles). Bundling it into this change would have buried a ~350-site
mechanical diff under new firmware behaviour.

What was done for it: the roadmap row now says the types exist and the decision is "wire them"
rather than "contribute or declare out of scope"; `docs/UPSTREAM-GAPS.md` carries a staleness
banner; and the notes in `crate::firmware::ocpp_1_6` and `crate::diagnostics::ocpp_1_6` claiming
these messages do not exist upstream have been corrected.

## Verification

| Gate | Result |
|---|---|
| `cargo test --all-features` | 1130 unit + 18 integration tests, all passing |
| `cargo clippy --all-features --all-targets` | clean |
| `cargo clippy --no-default-features --lib` | clean |
| `cargo fmt --check` | clean |
| `cargo check --no-default-features --lib` | clean |
| `cargo check --target thumbv7em-none-eabihf --no-default-features --lib` | clean (with CI's `getrandom_backend="custom"`) |
| `cargo build --example embedded_bindings --no-default-features` | clean |
| `cargo doc --all-features --no-deps` | 23 warnings — **unchanged from the pre-migration baseline**, all pre-existing intra-doc links |
| `cargo hack check --each-feature --no-dev-deps --lib` | fails on `--features ocpp_1_6` alone — **pre-existing**, verified by running it on the base commit. `ocpp-client` gates `OcppVersion` behind its `websocket` feature (identically in 0.2.2 and 0.4.0) while `src/lib.rs:134` re-exports it unconditionally. Unrelated to this migration; worth a separate fix. |


---

## Moving on to 0.5.0

`ocpp-client` 0.5.0 tracks `ocpp-types` 0.3.0 and is a dependency-only release: no file in its
`src/` changed, and upstream's own release is purely additive - no message type, field type or
action changed shape, and the action counts stay 39 / 64 / 91.

**Cost here: one line in `Cargo.toml`.** Not a single source file needed editing, and every gate
passed unchanged on the first run.

Its changelog marks the release BREAKING for one reason only: `ocpp-types` is a *public*
dependency (`pub use ocpp_types;`), so a consumer that also names `ocpp-types` in its own
`Cargo.toml` has to move `0.2` → `0.3` in the same step or end up with two incompatible copies.
**That does not apply to this crate** - verified rather than assumed: `Cargo.toml` has no
`ocpp-types` entry (it appears only in comments), and all 983 references in `src/` go through
`ocpp_client::ocpp_types::..`, funnelled through [`crate::wire`](../src/wire.rs). The direct
`heapless` dependency also still matches upstream's, which is 0.8 in both 0.2.0 and 0.3.0.

### Available but not taken: the `validate` feature

0.5.0 adds an optional `validate` feature forwarding `ocpp-types`' own. It supplies a `Validate`
trait covering the spec constraints the types cannot encode - `maxLength` on the fields too large
to inline as a `heapless::String` (certificates, CSRs, OCSP results), plus every `minItems`,
`minimum`, `maximum` and `multipleOf` - along with `From<ValidationError>` for each version's
error type, which picks the right RPC error code (1.6J spells it `OccurenceConstraintViolation`,
missing the `r` that 2.x restored) and puts the failing JSON path in `errorDetails`.

Left off deliberately: turning it on is a behaviour change, not a port. It is worth its own
change, because it bears directly on `CLAUDE.md`'s error-handling stance - a handler could reject
a malformed payload with `request.validate()?` and answer the correct CALLERROR, instead of the
current position where the length bounds the *type* cannot carry go unchecked. Validation is
never automatic in `ocpp-client` (a `Validate` bound on `Action::Request` would break custom
`Action` impls when any crate in the graph enables the feature), so each call site opts in.
