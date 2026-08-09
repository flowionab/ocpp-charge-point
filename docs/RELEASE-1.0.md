# 1.0 readiness — is the hardware trait surface ready to freeze? (H5.5)

Task ID: **H5.5** (`docs/PRODUCTION-ROADMAP.md` §10.5, row: "1.0 criteria: hardware trait
surface frozen. Land every planned breaking change (B2.3, C2.2, E1.1) before this.").

**This document is an assessment, not a decision already taken.** It does not change any trait —
doing so here would collide with the other agents working in parallel worktrees this round. It
recommends what should land *before* a freeze can honestly be promised, and gives a plain
answer to the freeze/no-freeze question for whoever reads this next.

**Bottom line up front: do not freeze the hardware trait surface yet.** Freeze after the three
items in §3 land (a DER actuation hook, a live payment-status hook, and one production
integration's experience report against `Iso15118Controller`), not on a calendar date. Sections
below lay out the evidence.

---

## 1. What "frozen" would promise, and why that bar matters here

[`docs/SEMVER.md`](SEMVER.md) defines breaking-for-a-trait precisely: a new required method, a
changed signature, a new supertrait bound, or a changed invariant. Freezing the surface means
committing that none of those happen again without a major version bump — which, for a crate
whose integrators are hardware manufacturers who ship firmware to physical devices, is a promise
that an integrator's `impl` written against `1.0` still compiles and behaves the same way against
`1.9`. That is a real cost to break unnecessarily (a firmware re-flash cycle, potentially a
recall), which is exactly why `CLAUDE.md`'s brief for this task frames premature freezing as
"expensive for exactly the people this crate exists to serve." The question is not "can we freeze
today" (yes, mechanically — nothing stops tagging `1.0.0`) but "would that promise survive contact
with the two roadmap items everyone already knows are unfinished."

## 2. Per-trait stability assessment

Seventeen traits, read from `src/hardware/*.rs` (`grep -n '^pub trait' src/hardware/*.rs`) against
`docs/INTEGRATORS.md`'s description of each. "Stable" below means: no roadmap item, module doc, or
recent commit gives a concrete reason to expect a signature change; "at risk" means one does.

| Trait | File | Verdict | Why |
|---|---|---|---|
| `ChargePoint` | `charge_point.rs` | Stable | Mandatory, small (`vendor_name`/`model_name`/`evses`/`capabilities`/`start`). Already took its one known break (`capabilities()`, M1). No open roadmap item targets it further. |
| `Evse` | `evse.rs` | Stable | Mandatory, two methods (`connectors`/`reboot`). No open item. |
| `Connector` | `connector.rs` | **At risk — named in the brief** | Mandatory. `set_current_limit(&self, limit_ma: Option<u32>)` is a single scalar import-current ceiling. 2.1's DER control (`SetDERControl`/discharge limits/per-phase asymmetry) is fully wired on the message side (`src/der_control.rs` equivalent) but has **no trait method that can apply any of it** — `docs/CERTIFICATION.md` §3.1 confirms this is a genuine gap, not a documentation oversight. Closing it plausibly means either a new method on `Connector` or a new sibling trait (`DerActuator`-shaped, per `CERTIFICATION.md` §5's own phrasing) — either is a breaking addition to a mandatory trait's neighborhood integrators already implement. |
| `Storage` | `storage.rs` | Stable | Youngest of the "already broken once" traits (landed at M1, `E1.1`) but the interface itself — `get`/`set`/`remove` on an opaque byte value — is about as small as a durable KV trait gets, and no roadmap item proposes changing its *signature*. Everything called "Storage's outstanding gaps" in `SEMVER.md`'s cross-reference turned out (checked against `docs/PRODUCTION-ROADMAP.md` §7.2/§7.4) to be wiring work — which concern gets persisted, done per-concern via `ChargePointBuilder::*_persistence` methods — not a trait change; every E2/E4 row is closed. |
| `FileTransfer` | `file_transfer.rs` | Stable | `download`/`upload`, streaming-progress shape already accommodates both buffered and streaming implementors (`TransferProgress`). No open item. |
| `FirmwareInstaller` | `firmware.rs` | Stable | Single `install()` method, already accounts for the outcomes it needs (`FirmwareInstallOutcome`). |
| `FirmwareVerifier` | `firmware.rs` | Stable | Single `verify()` method. The *default impl* (`NoFirmwareVerifier`) failing closed is a certification concern (`CERTIFICATION.md` §3.4), not a trait-shape one. |
| `FirmwarePublisher` | `firmware_publisher.rs` | Stable | `publish`/`unpublish`, 2.x local-controller role, narrow and already exercised end to end (B3.4). |
| `CertificateStore` | `certificate.rs` | Watch, not urgent | Largest trait by method count (`install`/`delete`/`installed`/`has_private_key`/`certificate_chain_pem`/`all_certificate_chain_pems`/`expires_at`) and the most recently active file in `hardware/` (six commits touching it in the last month: B4.1, B4.2, F1.3, F2.2, F3.2, F1.3's mutual-TLS blocker fix). Every one of those six added *behavior* (renewal-ahead-of-expiry, trust-store management) on top of the *existing* trait methods rather than changing the trait's signature — the growth so far has been additive-safe. Worth one more security-workstream cycle to see if that holds before calling it stable outright. |
| `KeyStore` | `key_storage.rs` | Watch, not urgent | Newest trait in the surface (`F2.4`, landed within the last month) plus a same-month bug fix (`E2.9`/`F5.4`, "fix a real key-handle collision") to its *implementation*, not its signature. New traits that have shipped one bug fix and zero signature changes are a good sign, but one data point is thin; the honest read is "too young to call frozen; not showing distress either." |
| `SoftwareCrypto` | `key_storage.rs` | Stable | Internal to `SoftKeyStore`, not a top-level integration point per `docs/INTEGRATORS.md` §1 — only relevant to integrators who chose that specific `KeyStore` impl. Low blast radius even if it changed. |
| `OcspChecker` | `ocsp.rs` | Stable | Single `check()` method. `NoOcspChecker`'s no-op default is a certification-completeness concern (`CERTIFICATION.md` §3.4), not a shape concern. |
| `Display` | `display.rs` | Stable | `show`/`supported_formats`, already covers the display-message block's needs (B6.1/B6.2 landed against this exact shape with no trait change needed). |
| `BatterySwapStation` | `battery_swap.rs` | Watch, not urgent | Landed B8.3, within the last month, niche (per `docs/INTEGRATORS.md`'s own "niche" label). No integration report yet from a real swap station to confirm `prepare_swap`'s shape survives contact with real hardware — same caution as `KeyStore`, lower stakes because the hardware class is niche. |
| `Watchdog` | `watchdog.rs` | Stable | `pet()`, as small as a trait gets. No plausible reason to change. |
| `PaymentTerminal` | `payment_terminal.rs` | **At risk — named in the brief** | One method, `info()`, returning static identity only. `CERTIFICATION.md` §3.3 (independently verified against `src/payment.rs`'s module docs and `CAPABILITY_GATED_VARIABLES`) confirms `PaymentCtrlr`'s **22 required live-status variables have no hook to update them from a real terminal** — `ChargePointBuilder::payment` seeds identity once and nothing else. Driving them needs a new method (or a new trait alongside this one) for live status/settlement-triggering events. This is not a hypothetical: it is the one thing standing between "Payment profile blocked" and "Payment profile claimable by a product" per `CERTIFICATION.md` §4 item 7. |
| `Iso15118Controller` | `iso15118.rs` | **At risk — named in the brief, but differently** | One method, `deliver_certificate_response()`. Unlike the other two "at risk" rows, there is no known missing method — the trait's own module docs describe a deliberately narrow scope (relay an EXI blob to the vehicle-facing HLC session, never parse it). The risk here is not "we know it's incomplete," it's "nobody has built a real ISO 15118 HLC stack against it yet" — `CERTIFICATION.md` §3.2 states the crate ships no HLC stack at all, so this trait's only validation so far is the crate's own `NoIso15118Controller`/test-double implementations, not a real PLC/SLAC integration. A trait that has only ever been implemented by its own crate's test doubles has not yet had the chance to be wrong in the way that matters. |

**Count: 14 of 17 stable, 3 flagged.** The 3 are exactly the 3 the task brief named going in
(`Connector`, `PaymentTerminal`, `Iso15118Controller`) — cross-checking against the code confirmed
rather than overturned that framing. Four more (`CertificateStore`, `KeyStore`,
`BatterySwapStation`, and to a lesser extent `Storage`) are recently-active enough to warrant a
"watch, not urgent" label rather than an unqualified "stable," but nothing found in this pass
gives a concrete reason to expect their *signatures* specifically to change — the recent activity
on all of them has been additive (new methods' bodies, new default behavior, bug fixes), not
signature-breaking.

## 3. Breaking changes that should land before any freeze

In priority order — highest confidence that a break is coming, first:

1. **A DER actuation hook on `Connector` (or a new sibling trait).** `CERTIFICATION.md` §3.1
   states this outright: "this is a `crate::hardware` addition (a new trait), which is roadmap
   work, not a documentation fix." A freeze followed immediately by this change would be the
   textbook "freeze that is followed by a breaking change" the task brief warns against — DER
   control's message set is *already shipped* (`SetDERControl`/`GetDERControl`/`ClearDERControl`/
   `ReportDERControl`/`NotifyDERAlarm`/`NotifyDERStartStop`/`AFRRSignal`), so the pressure to wire
   it end-to-end is not speculative, it is a CSMS-visible half-finished feature today.

2. **A live payment-status hook, on `PaymentTerminal` or a new sibling trait.** Same shape of gap
   as DER control: `CERTIFICATION.md` §3.3 states plainly that "even a motivated integrator has no
   live-status trait to implement yet." `NotifySettlement`/`NotifyWebPaymentStarted` are wired on
   the OCPP side (B7.2) with no way for a real terminal to drive them.

3. **A real ISO 15118 HLC integration exercising `Iso15118Controller`**, or, short of a real
   integration, an explicit decision that this trait's narrow scope (opaque EXI relay only) is
   intentional and final. This is different in kind from items 1–2: it may turn out the trait is
   already right and just unvalidated, in which case no *code* change is needed, only evidence.
   Freezing before that evidence exists means freezing an unvalidated guess about what an
   integrator's real HLC stack needs from this boundary.

None of these three should block *this round's* work or any other in-flight worktree — they are
called out here as freeze prerequisites, not as this task's, or anyone's current task's, TODO
list. Consistent with the ground rules, this document recommends without implementing.

A fourth, lower-confidence candidate: **`CertificateStore`'s method count** (7 methods, the
largest trait in the surface) is worth one more security-workstream cycle of quiet before calling
it settled, given how much has landed on it in the past month — not because a specific gap is
known, the way the three above are, but because "large trait, high recent commit velocity" is
itself evidence worth weighing per the task brief's instruction to reason from the observed
breaking-change rate.

## 4. Track record: what the recent rate of change says

`CHANGELOG.md`'s "Breaking" section lists six changes, all still inside unreleased `0.1.0`:
`ChargePoint::capabilities()`, `Connector::set_current_limit`, `hardware::Storage`,
`connect_and_setup`'s `payload_limits`, `firmware_updates`'s `verifier`, and
`ChargePointEffect` losing `Eq`. Of those six, three are `crate::hardware` trait changes in the
strict sense `SEMVER.md` defines (the other three are builder/function-signature or state-type
changes — real breaks, but not the trait-freeze question H5.5 asks about). All three trait breaks
landed together, deliberately batched, at M1 — a genuinely good pattern (one break absorbed
instead of three), but a pattern that only works when the *set* of needed breaks is known in
advance. Items 1–2 in §3 are exactly the kind of not-yet-known-in-advance breaks that pattern is
supposed to prevent from arriving piecemeal after a freeze.

Beyond the changelog's own list: `git log` shows five of the seventeen trait files
(`certificate.rs`, `key_storage.rs`, `battery_swap.rs`, `firmware.rs`'s `FirmwareVerifier`
addition, `payment_terminal.rs`) touched by substantive commits in roughly the last month
(F2.4, F1.3, F2.2, F3.2, B7.2, B8.2, B8.3 per `git log --oneline` against `src/hardware/`).
Most of that activity is additive (new traits, new default behavior, a bug fix), which is the
healthy kind of pre-1.0 churn `SEMVER.md`'s "not breaking" list anticipates and welcomes. But the
sheer density of it — this crate has not gone a month without touching something in
`src/hardware/` since M1 — is itself the evidence the task brief asks to reason from honestly: a
trait surface that has changed shape (not just grown behavior) at roughly the pace of one
`0.MINOR` release per major roadmap milestone is not yet in the "quiet for a while" state a
credible freeze announcement usually rests on, independent of whether any *specific* known gap
remains. Milestones M0–M4 are done and workstreams B–G are closed per the round's own framing, but
"closed" here has meant "the messages are wired," not "every trait behind them is validated by a
real integration" — §2 and §3 above are the concrete cases where that distinction matters.

## 5. What else 1.0 should mean here

The task brief asks whether 1.0 is gated on certification or independent of it. It should be
**independent**, for a reason the crate's own docs already state cleanly: `docs/CERTIFICATION.md`
draws a hard line between "this crate implements the messages" (a `Library` claim, decidable from
this repository) and "a specific product passed the OCTT" (a `Product` claim, decided by a human
signing a certification application after running external tooling this worktree does not have).
Gating a Rust crate's `1.0` on a *product's* certification status would be a category error — the
same crate version can sit under a certified product and an uncertified prototype simultaneously.
H3 (compliance: OCTT runs, the manual test-case sweep) is real, largely unstarted work, but it is
orthogonal to whether the trait surface is stable enough to promise integrators non-breakage.

What 1.0 *should* mean here, beyond the trait freeze:

- **The `SEMVER.md` policy itself starts being honored as written**, not just stated. The document
  already commits pre-1.0 to "no compatibility promise between `0.MINOR` releases at all" — 1.0 is
  where that changes to the standard post-1.0 reading. That's a real behavior change (in tooling:
  `cargo semver-checks` gating a release; in process: an actual `MAJOR` bump the next time
  something in §2's "breaking" list happens), not just a version-number relabeling.
- **A tagged release exists at all.** `CHANGELOG.md`'s opening line states "this crate has not
  made a tagged release yet." 1.0 is necessarily also the *first* release, which raises the bar
  further: there is no `0.9` to have field-tested the freeze candidate against real integrators
  first. That absence of a beta population is itself an argument for not freezing early — the
  three items in §3 are the kind of gap a beta user would surface, and none exists yet to surface
  them from experience rather than internal review.
- **MSRV and no_std claims stay true**, which they currently do (`cargo check
  --no-default-features --lib` and the MCU target both pass as of this worktree — see the
  Verification section below) — this is a maintained invariant, not new work for 1.0, but worth
  stating as part of "what 1.0 means" since a no_std regression at 1.0 would be exactly the kind
  of "confident wrong conclusion" this round's guidance warns against making about anything else.

## 6. Recommendation

**Do not freeze the hardware trait surface at this point.** Freeze after, specifically:

1. `Connector` gains whatever DER-actuation hook the maintainers choose (new method or new
   sibling trait) — landed and given at least one release to settle.
2. `PaymentTerminal` (or a sibling trait) gains a live-status hook driven by a real or
   realistically-modeled terminal.
3. Either a real ISO 15118 HLC integration has exercised `Iso15118Controller`, or the maintainers
   make an explicit, documented call that its current narrow scope is final without that evidence
   — a deliberate decision either way is enough; the freeze just should not happen by default
   while the trait is unvalidated by anything but its own test doubles.

This is a **"freeze after specific work," not a "do not freeze ever"** recommendation — 14 of 17
traits show no concrete reason to expect a signature change, and the three that do are narrow
(each is a bounded, nameable gap, not a sign the whole design is wrong). But "14 of 17 look done"
is not the promise a freeze makes; a freeze promises *all* of them are done, and items 1–2 in §3
are not speculative maybes, they are confirmed-by-this-crate's-own-docs gaps with a known shape
sitting in a mandatory (`Connector`) and a niche-but-named (`PaymentTerminal`) trait respectively.
Landing them first turns "freeze now and hope" into "freeze because the known list is empty,"
which is the only version of this promise worth making to a hardware manufacturer.

Once items 1–3 land, revisit this document rather than treating its "not yet" as permanent —
re-run the per-trait pass in §2 (most rows will not have changed) and confirm the "watch, not
urgent" traits (`CertificateStore`, `KeyStore`, `BatterySwapStation`) have gone quiet, then a
freeze recommendation is very plausibly a "yes."

---

## Verification performed for this document

- `grep -n '^pub trait' src/hardware/*.rs` — confirms seventeen traits, cross-checked against
  `docs/INTEGRATORS.md`'s (undercounted, self-admittedly stale) "eleven plus `BatterySwapStation`"
  count.
- `git log --oneline -- <file>` per hardware trait file, to establish recent-activity ranking.
- Read `docs/CERTIFICATION.md` in full — it independently reached the same three gaps (DER,
  Payment, ISO 15118) from a certification-claims angle; this document reaches them again from a
  trait-freeze angle and finds the two analyses agree, which is corroboration rather than
  duplication (different question, same underlying facts).
- Read `docs/SEMVER.md` in full for the definition of "breaking" applied throughout §2–§4.
- No trait in `src/hardware/` was modified. No commit needed for a documentation-only,
  no-code-change task; a commit was still made per the round's ground rules (§4) to preserve the
  new file.
