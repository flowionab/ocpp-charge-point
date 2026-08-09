# Upstream type/action completeness audit (D2)

> **Stale as of the `ocpp-client` 0.4.0/0.5.0 migration (2026-08-08).** Everything below was
> measured against `ocpp-types` 0.1.2 / `ocpp-client` 0.2.0; `Cargo.toml` now requires
> `ocpp-client` 0.5.0 (`ocpp-types` 0.3.0). Three of its findings have since changed:
>
> - **1.6J security whitepaper (§D2.2, and the 1.6J table).** The audit's central 1.6J finding -
>   that all ten whitepaper messages are absent from `ocpp-types` *entirely*, so closing D2.2
>   means contributing types upstream - no longer holds. `ocpp-types` 0.2.0 generates them and
>   `ocpp-client` 0.4.0 wraps eleven as actions (the ten below plus `ExtendedTriggerMessage`);
>   1.6 went from 28 wired actions to 39. Wiring them in this crate is still open.
> - **2.1 and 2.0.1 action coverage.** The five 2.1 messages listed as missing from
>   `ocpp-client` were added in 0.2.1, and 2.0.1's `SecurityEventNotification` in 0.2.2.
> - **`DataTransfer`.** A new upstream defect applies from 0.4.0 - see
>   [`MIGRATION-ocpp-client-0.4.md`](./MIGRATION-ocpp-client-0.4.md) §"DataTransfer".
>
> **Unchanged in headline size, corrected in cause:** D2.3 (`ChargingProfile` 56 KB by value).
> Re-verified directly against the pinned `ocpp-types` 0.3.0 — the number holds (measured 50,584
> bytes, same order of magnitude) but the mechanism this note previously named
> (`ChargingSchedule` inlining `AbsolutePriceSchedule`/`PriceLevelSchedule` rather than boxing
> them) is only part of the story: `ocpp-client` 0.5.0 always builds `ocpp-types` with `alloc`
> on, under which those three fields are already `Option<T>` over `alloc::Vec`-backed lists, not
> fixed `heapless` capacities. The dominant cost (~78%) is instead this crate's own `wire.rs`
> binding every nested generic `CustomDataType` to the concrete, ~256-byte `CustomData` struct
> rather than `ocpp-types`' own zero-sized `NoCustomData` default, multiplied by every
> spec-bounded `heapless` array the ISO 15118-20 price-schedule subtree still carries regardless
> of the `alloc` feature. Full measured before/after numbers, the isolated cause, and a drafted
> (not filed) upstream report are in [`docs/MEMORY.md`](MEMORY.md#d23--21-chargingprofiles-by-value-size-re-measured).
>
> `ocpp-types` 0.3.0 changed none of this again - that release is purely additive, with the same
> 39 / 64 / 91 actions and no type reshaped - so the corrections above are current.
>
> Re-run the audit against 0.3.0/0.5.0 before planning D2.2; the method below still applies.

See also [`UPSTREAM-POLICY.md`](./UPSTREAM-POLICY.md) (D3.2) for what to do
once a finding here is confirmed: when to upstream a fix versus work around
it locally, and — the failure this document's own header warns about — a
cheap mechanism for re-checking a "blocked" finding before repeating it as
still current.

This document answers [D2 in `PRODUCTION-ROADMAP.md` §6.2](./PRODUCTION-ROADMAP.md#62-d2--type-completeness-audit):
does `ocpp-types` actually contain every message OCPP 1.6J / 2.0.1 / 2.1
define, does `ocpp-client` wrap all of them as callable actions, and does
this crate wire the ones it needs. It is a factual audit, not a plan —
every table below was produced by diffing generated file lists against
extracted spec text, not eyeballed or taken from memory. See
["How this was verified"](#how-this-was-verified) for exact commands.

**Headline: the roadmap's 90/64/28 request-type counts are correct**, and
so are its Appendix A wired-message counts (19/28, 21/63, 22/86). Nothing
in this audit contradicts `PRODUCTION-ROADMAP.md`. One thing it corrects:
`ROADMAP.md` §0 still says "Only the OCPP 2.0.1 spec PDFs are currently
vendored" — that's stale. `docs/OCPP-2.1/` contains the full 2.1 edition-2
spec set (part 0–part 6, including the CSV appendices), and this audit
used it directly.

---

## Versions actually verified against

`Cargo.lock` pins:

| Crate | Pinned version | Local checkout version | Used for this audit |
|---|---|---|---|
| `ocpp-types` | 0.1.2 | 0.1.3 (`/Users/joatin/git/ocpp-types`, HEAD `555edb2`) | **the pinned registry copy**, `~/.cargo/registry/src/.../ocpp-types-0.1.2` |
| `ocpp-client` | 0.2.0 | 0.2.0 (`/Users/joatin/git/ocpp-client`, HEAD `00e75f1`) | either — versions match; registry copy used for byte-identical certainty |
| `rust-ocpp` | not a dependency of this crate or of `ocpp-types`/`ocpp-client` | 3.0.4 checked out locally | secondary cross-check only (see §D2.2 and the 1.6J note below) |

The local `ocpp-types` working copy is **one release ahead** of what's
pinned: 0.1.3 vs 0.1.2. The delta between them
(`3bdaa4e Fix 2.0.1/2.1 DataTransfer.data being generated as
Option<()>; bump to 0.1.3`) is a bugfix to one field's generated type, not
a message-list change — confirmed by counting `*_request.rs` files in both
trees (90/64/28 in each). So the message-completeness numbers below are
identical whichever version is used, but this audit's file contents (e.g.
the `DataTransfer` field question) reflect **0.1.2 as actually pinned**,
per the task's instruction to verify what's really in the lockfile rather
than trust the adjacent checkout.

`ocpp-types` 0.1.2 has no dependency on `rust-ocpp` — it's an independently
generated, `no_std`/no-`alloc`-by-default crate (`ocpp-codegen`, generated
from its own `schemas/`), not a wrapper around `rust-ocpp`. The two are
unrelated implementations of the same spec.

---

## D2.1 — Message coverage tables

### Method

For each protocol version:

1. **Spec message list** — extracted from the vendored spec PDF's "Part 2:
   Specification" message-definitions chapter (each message gets its own
   numbered subsection, e.g. `1.17. GetBaseReport`), via `pdftotext -layout`
   + a TOC-line regex. For 2.1 and 2.0.1 this chapter is present in the
   vendored PDFs and was used directly. **1.6J is not vendored under
   `docs/` at all** — see the dedicated note below.
2. **`ocpp-types` coverage** — did `ls src/v21|v201|v16/*_request.rs` produce
   a file whose name matches the spec message (mechanical
   PascalCase→snake_case normalization, case- and underscore-insensitive
   diff)?
3. **`ocpp-client` coverage** — does `src/ocpp_2_1|ocpp_2_0_1|ocpp_1_6/actions.rs`
   contain a `ocpp_2_1_action!(<Name>, ...)` / `..._send_action!(<Name>, ...)`
   macro invocation for that message?
4. **This crate's wiring** — does `src/` contain a `.on_<snake>(` or
   `.send_<snake>(` call inside a `mod ocpp_1_6 { … }` / `mod ocpp_2_0_1 { … }`
   / `mod ocpp_2_1 { … }` block? (This is the same method
   `PRODUCTION-ROADMAP.md` Appendix A uses; its per-version wired-counts —
   19/28, 21/63, 22/86 — were re-derived independently for this audit with
   a small Python/regex scan of the same call sites and matched exactly.)

### D2.1.a — OCPP 2.1 (90 spec messages)

The 2.1 "Messages" chapter (`OCPP-2.1_edition2_part2_specification.pdf`,
§ starting "1.1. AdjustPeriodicEventStream" … "1.90. VatNumberValidation")
lists **90** messages, matching `ocpp-types` v21's 90 `*_request.rs` files
**exactly by name** with one structural exception:

- **`NotifyPeriodicEventStream`** has no `*_response.rs` file in
  `ocpp-types` — it's modeled as a single `NotifyPeriodicEventStream`
  struct, not a request/response pair. This is not a bug: `ocpp-client`
  0.2.0 has a distinct `ocpp_2_1_send_action!` macro (vs.
  `ocpp_2_1_action!`) specifically for OCPP-J 2.1's `SEND` message type —
  "a single struct … not a Request/Response pair … the spec forbids
  replying to a `SEND`" (its own doc comment). So both crates already
  model this correctly; it just breaks a naive "90 requests, 90 responses"
  count (90 requests, 89 responses + 1 `SEND` payload).

| Layer | Count | Detail |
|---|---|---|
| Spec messages | 90 | verified against `OCPP-2.1_edition2_part2_specification.pdf` §1 (Messages), lines ~355–450 of its TOC |
| `ocpp-types` v21 request types | 90 / 90 | 100% — every spec message has a request type; response types are 89/90 by design (see `NotifyPeriodicEventStream` above) |
| `ocpp-client` 0.2.0 v2.1 actions | 86 / 90 | 85 `ocpp_2_1_action!` (req/resp) + 1 `ocpp_2_1_send_action!` (`NotifyPeriodicEventStream`) |
| This crate wired | 22 / 86 available (22 / 90 of spec) | per Appendix A / re-verified scan |

**Missing from `ocpp-client` (present in `ocpp-types`, absent as a
generated action)** — 5 messages, confirmed exactly matching
`PRODUCTION-ROADMAP.md` §6.1 (D1)'s table:
`GetDERControl`, `SetDERControl`, `SetDisplayMessage`, `TriggerMessage`,
`UpdateDynamicSchedule`. Not a genuine upstream *type* gap (D2's concern) —
the types exist — but a genuine wiring gap in `ocpp-client`, already
tracked as D1.

**Genuinely absent upstream (D2's real concern): none.** Every one of the
90 OCPP 2.1 spec messages has a corresponding type in `ocpp-types` 0.1.2.

**Not wired by this crate, but available to wire** — 64 messages (86
available − 22 wired), listed in full in `PRODUCTION-ROADMAP.md` Appendix
A.3; not repeated here to avoid the two documents drifting out of sync.

### D2.1.b — OCPP 2.0.1 (64 spec messages)

The 2.0.1 "Messages" chapter (`OCPP-2.0.1_edition4_part2_specification.pdf`,
§1.1 "Authorize" … §1.64 "UpdateFirmware") lists **64** messages. Diffed
byte-for-byte (case/underscore-normalized) against `ocpp-types` v201's 64
`*_request.rs` files: **exact match, no discrepancies in either
direction.**

| Layer | Count | Detail |
|---|---|---|
| Spec messages | 64 | verified against spec §1, all 64 named subsections |
| `ocpp-types` v201 request types | 64 / 64 | 100%, exact name match |
| `ocpp-client` 0.2.0 v2.0.1 actions | 63 / 64 | missing only `SecurityEventNotification` (types exist; no macro invocation) — matches D1's finding |
| This crate wired | 21 / 63 available (21 / 64 of spec) | per Appendix A / re-verified scan |

**Genuinely absent upstream: none.** All 64 have types.

**Missing from `ocpp-client`:** `SecurityEventNotification` only — already
tracked as D1.1/D1.2.

### D2.1.c — OCPP 1.6J (28 spec messages, core profile)

**1.6J's specification text is not vendored under `docs/` at all** —
`docs/` has only `OCPP-2.0.1/` and `OCPP-2.1/`. This is the fallback case
the task asked to flag: for 1.6J's message *list*, this audit falls back to
two independent, non-spec sources instead of the spec PDF:

1. `ocpp-types` 0.1.2's own `src/v16/*_request.rs` file list (28 files).
2. `rust-ocpp` 3.0.4's independently-generated `src/v1_6/messages/` module
   list (28 files, checked out at `/Users/joatin/git/rust-ocpp`) — a
   *different* codebase's independent implementation of the same spec, used
   here purely as a second data point, not as an authority. `rust-ocpp` is
   not a dependency of this crate, `ocpp-client`, or `ocpp-types`.

These two independently-authored crates agree on the *set* of 28 core
1.6J message names (mod the `heart_beat` vs `heartbeat` file-naming
difference). That's reasonable circumstantial confidence that 28 is the
correct count for 1.6J's core (non-security-whitepaper) message set, but
it is **not the same rigor** as the PDF-extraction cross-check done for
2.0.1 and 2.1 above, because neither source is the specification text
itself. If a 1.6J spec PDF is added to `docs/` later, this section should
be re-run against it directly.

| Layer | Count | Detail |
|---|---|---|
| "Spec" messages (2 independent proxies, not spec text) | 28 | `ocpp-types` v16 and `rust-ocpp` v1_6 agree on the name set |
| `ocpp-types` v16 request types | 28 / 28 | trivially, since it's one of the two proxies |
| `ocpp-client` 0.2.0 v1.6 actions | 28 / 28 | **all 28 wired as actions** — no gap at this layer |
| This crate wired | 19 / 28 | per Appendix A / re-verified scan |

**Genuinely absent upstream: none identified**, with the above caveat that
"upstream" here was checked against proxies, not the spec text.

**Not wired by this crate:** `ClearCache`, `ClearChargingProfile`,
`DiagnosticsStatusNotification`, `FirmwareStatusNotification`,
`GetCompositeSchedule`, `GetDiagnostics`, `SetChargingProfile`,
`TriggerMessage`, `UpdateFirmware` — 9 messages, matches
`PRODUCTION-ROADMAP.md` Appendix A.1 exactly.

---

## D2.2 — 1.6J security whitepaper extensions

The OCPP 1.6 Security Whitepaper (edition 3, the de-facto "1.6J security
profile" spec) adds a set of messages on top of the 28-message core
profile above. All 10 named in the roadmap task were checked directly
against the pinned `ocpp-types` 0.1.2 `src/v16/` directory listing and the
pinned `ocpp-client` 0.2.0 `src/ocpp_1_6/actions.rs`:

| Message | In `ocpp-types` v16? | In `ocpp-client` v1.6 actions? |
|---|:---:|:---:|
| `SecurityEventNotification` | **No** | No |
| `SignedUpdateFirmware` | **No** | No |
| `SignedFirmwareStatusNotification` | **No** | No |
| `LogStatusNotification` | **No** | No |
| `GetLog` | **No** | No |
| `InstallCertificate` | **No** | No |
| `DeleteCertificate` | **No** | No |
| `GetInstalledCertificateIds` | **No** | No |
| `CertificateSigned` | **No** | No |
| `SignCertificate` | **No** | No |

**All 10 are completely absent from `ocpp-types`' 1.6 module** — not just
unwired actions (like the 2.1/2.0.1 gaps above), but missing types. This
is a genuine, total upstream gap for this specific extension set. (Several
of these names — `SecurityEventNotification`, `GetLog`, `InstallCertificate`,
`DeleteCertificate`, `GetInstalledCertificateIds`, `CertificateSigned`,
`SignCertificate`, `LogStatusNotification` — *do* exist in `ocpp-types` for
v2.0.1 and v2.1, since the same messages are core profile there. It's
specifically the 1.6-security-whitepaper projection of them that's
missing.)

`rust-ocpp` 3.0.4 was also checked as a second data point:

```
$ ls ~/.cargo/registry/.../rust-ocpp-3.0.4/src/v1_6/messages/ | grep -iE \
  'security_event|signed_update|signed_firmware|log_status|get_log|install_cert|delete_cert|get_installed|certificate_signed|sign_cert'
(no output)
```

`rust-ocpp` doesn't have them either — this isn't a gap unique to
`ocpp-types`; the 1.6J security whitepaper extensions appear to be broadly
unimplemented across the Rust OCPP ecosystem, at least in the two crates
checked here.

### The decision this surfaces (not made here, per the task brief)

The roadmap ([D2.2](./PRODUCTION-ROADMAP.md#62-d2--type-completeness-audit),
referenced from [F, Workstream security](./PRODUCTION-ROADMAP.md#8-workstream-f--security))
needs one of:

**Option 1 — contribute upstream.** Cost: 10 new message types
(request+response structs, `no_std`/`alloc` dual variants matching
`ocpp-types`' existing pattern, `serde` support) in `ocpp-types`, plus 10
new `ocpp_1_6_action!` macro invocations in `ocpp-client`, plus whatever
review/release latency an external maintainer introduces (`ocpp-types` and
`ocpp-client` are both external crates this repo doesn't control — see
`D3.2`'s vendor-or-fork contingency). Given the existing codegen pipeline
(`ocpp-codegen`, driven by `schemas/`), this is probably schema-additions
+ regeneration rather than 10 structs written by hand, if the security
whitepaper's message schemas can be sourced in the same format the
existing generator consumes. That schema-sourcing step wasn't
investigated as part of this audit.

**Option 2 — declare 1.6J security profiles out of scope**, and document
it in the README: the charge point supports 1.6J's core message set (28/28
already wired end-to-end in `ocpp-client`, per D2.1.c) but does not, and
will not, offer OCPP's 1.6-era Security Whitepaper features (signed
firmware, remote log retrieval, or CSMS-driven certificate lifecycle) over
1.6J specifically. Cost: any hardware manufacturer targeting a CSMS that
requires the 1.6J security profile (rather than 2.0.1/2.1's built-in
Advanced Security profile, which — per D2.1.a/b above — has full type
coverage already) cannot use this firmware for that deployment. Given the
product goal in `CLAUDE.md` is "OCPP 2.1 first, then 2.0.1 and 1.6J", and
2.x already carries the equivalent messages with full upstream type
support, this option's practical cost may be small — but that's a market
judgment this audit isn't positioned to make.

Evidence either way is now in this table; the decision is the user's/repo
owner's, not this audit's, per the task's brief.

---

## How this was verified

All commands run against `/Users/joatin/git/ocpp-charge-point` on
2026-08-06, using the exact `Cargo.lock`-pinned crates fetched into the
local Cargo registry cache (`~/.cargo/registry/src/index.crates.io-*`), not
the adjacent `ocpp-types`/`ocpp-client` git checkouts (see the versions
table above for why).

```bash
# Confirm pinned versions
grep -A3 'name = "ocpp-types"' Cargo.lock
grep -A3 'name = "ocpp-client"' Cargo.lock

# Locate the exact pinned sources in the registry cache
R=~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f
ls $R | grep -iE 'ocpp-types-0.1.2|ocpp-client-0.2.0|rust-ocpp-3.0.4'

# Request-type counts (the roadmap's 90/64/28 claim)
ls $R/ocpp-types-0.1.2/src/v21  | grep -c '_request.rs$'   # -> 90
ls $R/ocpp-types-0.1.2/src/v201 | grep -c '_request.rs$'   # -> 64
ls $R/ocpp-types-0.1.2/src/v16  | grep -c '_request.rs$'   # -> 28

# Spec message lists (2.1 and 2.0.1 only — vendored PDFs)
cd docs/OCPP-2.1
pdftotext -layout -f 1 -l 40 OCPP-2.1_edition2_part2_specification.pdf - \
  | sed -n '355,460p' | grep -E '^\s*1\.[0-9]+\.' \
  | perl -pe 's/^\s*1\.\d+\.\s*//; s/\s*\.\s\..*$//; s/\.\s*$//'   # -> 90 names

cd ../OCPP-2.0.1
pdftotext -layout -f 1 -l 40 OCPP-2.0.1_edition4_part2_specification.pdf - \
  | sed -n '261,324p' | grep -E '^\s*1\.[0-9]+\.' \
  | perl -pe 's/^\s*1\.\d+\.\s*//; s/\s*\.\s\..*$//; s/\.\s*$//'   # -> 64 names

# Diff spec names against ocpp-types file names (case/underscore-normalized)
diff <(sort spec_names.txt | tr A-Z a-z) \
     <(ls $R/ocpp-types-0.1.2/src/v21/*_request.rs | xargs -n1 basename \
         | sed 's/_request\.rs//' | tr -d '_' | sort)

# ocpp-client action-macro coverage (per version)
awk '/^ocpp_2_1_action!\(|^ocpp_2_1_send_action!\(/{getline; gsub(/,| /,""); print}' \
  $R/ocpp-client-0.2.0/src/ocpp_2_1/actions.rs | sort -u | wc -l   # -> 86

# This crate's wiring (per-version `mod ocpp_1_6/ocpp_2_0_1/ocpp_2_1` blocks,
# same method as PRODUCTION-ROADMAP.md Appendix A)
grep -rln 'mod ocpp_1_6\|mod ocpp_2_0_1\|mod ocpp_2_1' src/
# then per file: extract the named-mod block body and grep '\.(on_|send_)([a-z0-9_]+)\('

# D2.2: security whitepaper extensions — presence check
ls $R/ocpp-types-0.1.2/src/v16/ | grep -iE \
  'security_event|signed_update|signed_firmware|log_status|get_log|install_cert|delete_cert|get_installed|certificate_signed|sign_cert'
# -> no output (all 10 absent)
grep -iE 'SecurityEventNotification|SignedUpdateFirmware|...' \
  $R/ocpp-client-0.2.0/src/ocpp_1_6/actions.rs
# -> no output
```

Full extracted intermediate files (spec name lists, diffs) were produced
in a scratch directory during this audit and are not checked in; re-run
the commands above to regenerate them.
