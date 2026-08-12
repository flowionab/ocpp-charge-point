# Certification — which profiles to claim (H3.3)

Task ID: **H3.3** (`docs/PRODUCTION-ROADMAP.md` §10.3). This document does the *deciding* half
of "decide which certification profiles to claim per feature set (C1.2) and pass them" — the
*passing* half needs the OCA Compliance Test Tool (OCTT) and belongs to **H3.1**, which this
document unblocks. It does not edit `docs/ROADMAP.md` or `docs/PRODUCTION-ROADMAP.md`.

**Audience**: whoever signs the certification application — this is a recommendation for a human
to accept or reject, not a decision already made. Every claim below is grounded in a specific
module, test, or doc in this repo as of this writing (`main`, last re-verified against
`scripts/message-coverage.py`'s **2026-08-09** run: 1.6J 37/39, 2.0.1 64/64, 2.1 91/91). Where
something is unverifiable from this worktree (`docs/OCPP-2.1/`/`docs/OCPP-2.0.1/` are gitignored
— see below), that is stated rather than guessed.

**The central distinction this document draws**: a profile that only needs code this crate ships
is claimable **by the library** — any integrator gets it "for free" once they enable the feature.
A profile that needs hardware only the integrator can supply (a display, a payment terminal, a
DER actuator) is claimable **by a product** built on this crate, never by the crate itself. Mixing
those two up is exactly the kind of overstatement that fails an audit after money has been spent,
so §2 below states, for every profile, which of the two applies.

---

## 0. Source material and its limits

- **1.6J feature profiles**: `Core`, `FirmwareManagement`, `LocalAuthListManagement`,
  `Reservation`, `SmartCharging`, `RemoteTrigger` — the standard `SupportedFeatureProfiles`
  values, also reproduced in `src/hardware/capabilities.rs`'s `CAPABILITY_GATES` doc comment.
- **2.0.1 / 2.1 certification profiles**: Core (mandatory) plus independently-certifiable add-ons.
  The authoritative list for 2.1 is `docs/OCPP-2.1/OCPP-2.1_edition2_part5_certification_profiles.pdf`
  Table 1 and its 2.0.1 equivalent — **both are gitignored and were not present in this worktree**
  (`.gitignore:18`; see `docs/PRODUCTION-ROADMAP.md`'s M4/M5 status on this exact gap). The names
  used below (Reservation, Local Authorization List Management, Smart Charging, Advanced User
  Interface, Payment, DER control, Advanced Device Management, Core's optional-feature-list items
  such as Battery Swap, and **Advanced Security**) come from the README's C1.2 mapping table
  (`README.md` "OCPP certification profile mapping"), which itself was built by a previous round
  reading those PDFs via `pdftotext -layout` — not re-derived by this task from the PDFs directly,
  because they are absent here too. Treat the profile *names* as reliable (they match the README
  table's citations and `CAPABILITY_GATES`' `feature_profile_1_6`/`ctrlr_component` fields, which
  a passing C3.5 test keeps in sync with the code) and the PDF page/section citations as
  best-effort until someone with the vendored PDFs re-verifies them.
- **Message coverage**: `scripts/message-coverage.py`, re-run above. 2.0.1 and 2.1 are message-
  complete; 1.6J is missing `GetLog`/`LogStatusNotification` only, a deliberate scope decision
  (D2.2, `docs/UPSTREAM-GAPS.md`) — 1.6J's Security Whitepaper (which those two messages are part
  of) is a separate, larger gap addressed in §3 below, not merely those two messages.
- **`CAPABILITY_GATES`** (`src/hardware/capabilities.rs`): single source of truth for capability
  name → Cargo feature → 2.x `*Ctrlr` component → 1.6J feature profile, cross-checked by a
  passing test (C3.5) across the feature matrix. This document's per-profile table in §2 is built
  from it plus the README's profile-column additions.

---

## 1. Two things a "profile" means here — read this before §2

OCPP's own certification scheme names two different kinds of thing that this crate's docs have
sometimes both called "profiles", and conflating them is the fastest way to make an inaccurate
claim:

1. **1.6J feature profiles** (`SupportedFeatureProfiles`): a charge point *advertises* a fixed
   list of these strings and is expected to implement every message the profile lists.
2. **2.0.1 / 2.1 certification profiles**: OCA's Part 5 test-suite groupings for the OCTT. A
   charge point (or, more precisely, a specific firmware image / product configuration) is
   certified against one or more of these by *running* the OCTT and submitting the results — this
   crate implementing a profile's messages is necessary but not sufficient for a certification
   claim.

Both are "profiles" a build can be *described* as supporting; only the second is something OCA
issues a certificate for, and only after H3.1's OCTT runs and H3.2's manual test-case sweep pass.
This document decides which profiles are worth pursuing that process for — it is not itself a
certification.

---

## 2. Per-profile claimability

Legend: **Library** = claimable by this crate as shipped, any integrator inherits it by enabling
the feature. **Product** = the crate provides the protocol handling but the claim depends on
hardware/integration work only the integrator can do. **Blocked** = not honestly claimable by
either today; see §3 for why.

### 1.6J feature profiles

| Profile | Claimable? | Cargo feature(s) | Hardware traits needed | Notes |
|---|---|---|---|---|
| **Core** | **Library** | none (always on with `ocpp_1_6`) | none beyond the base `hardware::ChargePoint`/`Connector`/etc. surface | Mandatory profile; 1.6J is 37/39 message-complete, and the two gaps (`GetLog`/`LogStatusNotification`) are Security Whitepaper messages, not Core. Core itself should be message-complete — re-verify against the OCTT's actual Core test cases in H3.2, since "wired" and "passes the part-6 test cases" are not proven equivalent yet. |
| **FirmwareManagement** | **Product** | `firmware-management` | `hardware::FileTransfer` (download), `hardware::FirmwareInstaller` (install), optionally `hardware::FirmwareVerifier` (signature check) | The messages are fully wired (`src/firmware.rs`). But `FirmwareVerifier`'s fail-safe default, `NoFirmwareVerifier`, **never validates a signature** — it refuses every signed update. A product claiming this profile with signed-firmware expectations must supply a real verifier; one that doesn't is honestly claiming "firmware transfer and install", not "signed firmware update". |
| **LocalAuthListManagement** | **Library** | `local-auth-list` | none beyond `hardware::Storage` if persistence across reboot matters | `SendLocalList`/`GetLocalListVersion` fully wired, in-memory by default, `Storage`-backed if the integrator opts in. No hardware dependency beyond the base connector/IdToken surface. |
| **Reservation** | **Library** | `reservation` | none beyond base surface (uses `Connector`'s existing state) | `ReserveNow`/`CancelReservation`/`ReservationStatusUpdate` fully wired. |
| **SmartCharging** | **Library** | `smart-charging` (currently gates only advertisement, not code — see README's "gates nothing at compile time" note) | `hardware::Connector::set_current_limit` | Composition, stacking, and 1.6J's charging-profile shape are all implemented and real (`src/smart_charging.rs`, `src/charging_profile.rs`); the Cargo feature is advisory today rather than `#[cfg]`-gating the module. Still **Library**-claimable because the code ships unconditionally and only needs a `Connector` that can actually clamp current — which every implementor of the base trait must provide something for. |
| **RemoteTrigger** | **Library** | none (part of `ocpp_1_6`) | none | `TriggerMessage` is wired (`src/remote_control.rs`, `Ocpp1_6TriggerMessageHandler`) and registered by `ChargePointBuilder::trigger_message`/`setup()`. |

### 2.0.1 / 2.1 certification profiles

| Profile | Claimable? | Cargo feature(s) | Hardware traits needed | Notes |
|---|---|---|---|---|
| **Core** | **Library** | none | base surface | 2.0.1 is 64/64, 2.1 is 91/91 message-complete. This is the strongest claim in the document — full message coverage, verified by a script, on both versions. |
| **Reservation** | **Library** | `reservation` | none beyond base | Same code path as 1.6J's Reservation profile; `ReservationCtrlr` is the mirrored 2.x device-model component. |
| **Local Authorization List Management** | **Library** | `local-auth-list` | none beyond base | Same code path as 1.6J. |
| **Smart Charging** | **Library** | `smart-charging` | `Connector::set_current_limit` | 2.1 adds priority charging and dynamic profiles (K28) on top of the 1.6J baseline, both implemented (`docs/PRODUCTION-ROADMAP.md` B2 rows). Same "feature flag is advisory" caveat as 1.6J's row. |
| **Advanced User Interface** | **Product** | `display-message`, `tariff-cost` | `hardware::` display surface implied by integrator UX (no dedicated trait beyond message handling — the message handlers exist, but *presenting* a message is the integrator's device) | `SetDisplayMessage`/`GetDisplayMessages`/`ClearDisplayMessage`/`CostUpdated` are wired, but this profile is fundamentally about the driver-visible result, which lives in hardware this crate cannot see. Claimable by a product with a real display; not by the library alone. |
| **Payment** | **Product** | `payment` | integrator's payment terminal integration (no formal `PaymentTerminal` status-reporting trait yet — `ChargePointBuilder::payment` only seeds identity variables) | 2.1-only. `PaymentCtrlr`'s **live status variables are placeholders**, not driven from a real terminal (confirmed current: `src/payment.rs`'s module docs point at `PaymentCtrlr`'s 22 required variables in `CAPABILITY_GATED_VARIABLES`, and no code path updates them from a live device). A product cannot honestly claim this profile until it wires a real terminal's status into those variables — this is squarely a **blocker**, not a "Product can claim it today" row; see §3. |
| **DER control** | **Blocked** | `der-control` | *no trait exists that can apply a curve or setpoint* | 2.1-only. The messages (`SetDERControl`/`GetDERControl`/`ClearDERControl`/`ReportDERControl`, `NotifyDERAlarm`/`NotifyDERStartStop`, `AFRRSignal`) are all wired and a CSMS can install a control — but nothing in `crate::hardware` can *act* on it. A charge point running this code faithfully stores and reports a curve it never applies. This is not a "Product" row (an integrator cannot bridge the gap themselves without this crate first exposing a trait to implement) — it is a genuine capability gap. See §3. |
| **Advanced Device Management** (periodic event streams, variable monitoring) | **Product**, partially **Library** | `periodic-event-stream`, `variable-monitoring` | none beyond base for variable monitoring; `hardware::` telemetry source for periodic streams' actual sampled values | `Open`/`Close`/`Adjust`/`GetPeriodicEventStream`/`NotifyPeriodicEventStream` and `SetVariableMonitoring`/`SetMonitoringBase`/`Level`/`GetMonitoringReport`/`NotifyEvent` are all wired. Variable monitoring is pure device-model bookkeeping this crate owns end to end — **Library**. Periodic event streams report values sourced from wherever the integrator's telemetry lives — **Product**, though the plumbing (the stream lifecycle itself) is this crate's. Both now have `CAPABILITY_GATES` rows and are therefore covered by C3.5's cross-surface consistency test (an earlier revision of this document said neither was — that was already stale when written): `variable_monitoring` gates `MonitoringCtrlr` and its handler registration; `periodic_event_stream` gates handler registration only, with `ctrlr_component: None`, because the 2.1 appendix names no component for streams — `MonitoringCtrlr.Available` governs variable monitoring itself, and inventing a second component to advertise would be a claim the spec does not define. |
| **Advanced Security** (2.x's security profile, the closest 2.x equivalent to 1.6J's Security Whitepaper) | **Blocked as a whole; several sub-claims are Library or Product** | `certificate-management`, `ocsp-checking`, `key-storage` | `hardware::CertificateStore`, `hardware::KeyStore`, `hardware::FirmwareVerifier`, `hardware::OcspChecker` | See §3 — this is the profile with the most moving parts and the most honest caveats. Security profiles 1–3 (transport) are done (F1.1–F1.3, all three modelled and profile 3 — mutual TLS — implemented). Certificate lifecycle messages (`InstallCertificate`/`DeleteCertificate`/`GetInstalledCertificateIds`/`CertificateSigned`/`SignCertificate`) are wired. But firmware signature verification and OCSP both have fail-safe *no-op* defaults (`NoFirmwareVerifier`, `NoOcspChecker`) that an audit would immediately notice unless a real integrator implementation is substituted — see §3. |
| **Core's optional feature list — Battery Swap** (feature id C-76) | **Product** | `battery-swap` | `hardware::BatterySwapStation` | 2.1-only, not a standalone certification profile — an optional item within Core's feature list per the README's citation. `RequestBatterySwap` is wired; the swap sequencing depends entirely on the station having a real swap mechanism, so this is squarely a **Product** claim, and a niche one (battery-swap stations only). |

### Not in `CAPABILITY_GATES`, not evaluated as a profile claim

`certificates` is a declared `Capabilities` field and Cargo feature with no `CAPABILITY_GATES`
row — not omitted from the tables above by accident, but because there is not yet enough there to
evaluate a claim against.

`iso15118` **does** have a gate row now (`ISO15118Ctrlr`, `has_handler: false` because every
message in the block is charge-point-initiated), so its capability propagates to the device model
like every other block's: a station declaring `Iso15118SupportLevel::Iso15118_2`/`_20` reports
`ISO15118Ctrlr.Available: true` and the component's one required variable
(`ContractValidationOffline`, registered `false`), and one declaring `None` reports
`Available: false` and nothing else. That closes the *advertisement* half of the gap. It does not
close §3's ISO 15118 entry, which is about the HLC stack this crate does not ship — the reason
Plug & Charge still isn't ranked as a claim below.

---

## 3. Blockers, named honestly

These are the four items the task brief calls out by name, cross-referenced against current code
rather than the (partly stale) `docs/THREAT-MODEL.md` prose that inspired the brief — two of the
four are *more* resolved than the threat model currently states, and it's worth flagging that
drift for whoever maintains that document next:

1. **DER control cannot actuate.** Confirmed in `src/hardware/capabilities.rs`'s `der_control`
   row and the roadmap's M5 caveats: the message set is wired end-to-end, but no
   `crate::hardware` trait exists for applying a curve or setpoint to real equipment. A CSMS can
   install a DER control and this crate will faithfully store and report it while never acting on
   it. **This blocks any DER control claim outright** — there is no "Product" escape hatch here,
   because there is no trait for a product's integrator to implement yet. Fixing this is a
   `crate::hardware` addition (a new trait), which is roadmap work, not a documentation fix.

2. **ISO 15118 carries EXI opaquely.** `hardware::Iso15118SupportLevel` and the `Get15118EVCertificate`
   message path exist, but Plug & Charge itself needs an integrator's High-Level Communication
   (HLC) stack behind `Iso15118Controller` — this crate transports the EXI blob without
   interpreting it. Not evaluated as a claimable profile above for exactly this reason: there is
   no OCPP "ISO 15118" certification profile as such, but any claim resting on Plug & Charge
   working (implicitly, parts of Advanced Security and the V2X-flavoured feature set in the
   README's hardware-class table) needs this integrator stack present, and none ships with this
   crate.

   **Sharper than "needs an integrator stack", as of the 2026-08-12 K sweep.** For *renegotiation*
   specifically — K16 and K17, 33 FRs, plus K18–K20's 15118-20 control modes — there is no trait to
   implement: `hardware::Iso15118Controller` has exactly one method, `deliver_certificate_response`,
   and nothing in `src/` mentions renegotiation at all. K16.FR.02 is a `SHALL` on the Charging
   Station whenever the composite schedule changes. So this item is **two** gaps, not one: an
   integrator-stack dependency for Plug & Charge, and a missing `crate::hardware` surface for
   renegotiation that puts K16–K20 out of reach of any product. See
   `docs/OCPP-2.1-COMPLIANCE-AUDIT.md` §2.18 and roadmap CV16 — which is the same shape of blocker
   as item 1's DER actuation trait, and should be taken with it as one break.

3. **`PaymentCtrlr`'s live status variables are placeholders.** Verified current in
   `src/payment.rs`'s module docs: the 22 required variables `CAPABILITY_GATED_VARIABLES` defines
   for `PaymentCtrlr` exist as device-model bookkeeping, but nothing drives them from a real
   payment terminal — `ChargePointBuilder::payment` only seeds identity, not live status. This is
   the reason the Payment profile is listed as blocked rather than merely "Product" in §2: even a
   motivated integrator has no live-status trait to implement yet, the same shape of gap as DER
   control.

4. **Firmware signature verification and OCSP are delegated to integrator traits whose `No*`
   defaults do nothing protective.** Verified current: `NoFirmwareVerifier::verify` always
   returns a failure (fail-*safe*, not fail-open — it blocks every signed update rather than
   accepting one unchecked, per `src/hardware/firmware.rs`'s own docs), and `NoOcspChecker::check`
   always returns `OcspVerdict::Unknown` (`src/hardware/ocsp.rs`). Neither default is a security
   hole by itself — both fail closed rather than open — but **neither does anything useful
   either**, so a certification claim resting on either capability being *present* rather than
   merely *safe-when-absent* requires the integrator to have supplied a real implementation.
   `docs/THREAT-MODEL.md` §4.2 currently states firmware verification "is modelled but never
   raised anywhere in this crate" — **that is stale**: `src/firmware.rs`'s worker does call
   `FirmwareVerifier::verify` and does raise `InvalidFirmwareSignature`/
   `InvalidFirmwareSigningCertificate` today (B3.3 landed since that threat-model text was
   written). The mechanism is real; only the *default* implementation behind it is a no-op. Worth
   a follow-up fix to `docs/THREAT-MODEL.md` itself, outside this document's scope.

   Similarly, `docs/THREAT-MODEL.md` §4.1 currently states "security profile 3 (mutual TLS) is
   not implemented" — also stale as of F1.3 landing (`docs/PRODUCTION-ROADMAP.md` §8.1): mutual
   TLS is implemented, gated on the station actually holding a certificate
   (`SecurityProfile::is_usable`). Both of these are drift in a document this task was told to
   cross-reference, not new findings about the certification question itself, but an auditor
   reading both documents together would notice the contradiction, so it is recorded here rather
   than silently worked around.

---

## 4. Recommended claim set

Ranked by effort-to-value, given everything above. "Effort" here means work remaining before an
honest OCTT run; "value" means how much of the addressable market (per the README's hardware-
class table) the claim unlocks.

### Pursue now (H3.1 should run the OCTT against these first)

1. **1.6J Core, 2.0.1 Core, 2.1 Core.** Highest value (every deployment needs Core) and lowest
   remaining effort — message coverage is complete on 2.x and 37/39 on 1.6J with the two gaps
   outside Core. This is the claim the rest of the roadmap has been building toward and the one
   least likely to surface surprises in H3.2's manual test-case sweep.
2. **Reservation, Local Authorization List Management, Smart Charging** (both 1.6J feature
   profile and 2.x certification profile forms). All three are **Library**-claimable — no
   integrator hardware dependency beyond the base traits every implementor already provides —
   fully message-complete, and covered by C3.5's cross-surface consistency test. Cheapest
   incremental claims after Core, and they cover the "public AC" and "DC fast charger" rows of
   the README's hardware-class table.

   > **Smart Charging carries a caveat since 2026-08-12**, from the K-block requirement sweep in
   > `docs/OCPP-2.1-COMPLIANCE-AUDIT.md` §3.1. Reservation and Local Authorization List Management
   > are unaffected. Three findings touch this claim, and the first is the one to settle before
   > booking OCTT time: an **external charging limit is reported to the CSMS but never enforced**
   > (§2.13, roadmap CV13) — K11.FR.01 is a `SHALL`, and a station that says it is limited and is
   > not is a worse failure than one that does not support external limits. `LocalGeneration`
   > cannot round-trip (§2.16) and `triggerReason = ChargingRateChanged` is absent (§2.17). The
   > composition engine itself (K01–K10) remains verified-good, and K28/K29 are still unread.
3. **RemoteTrigger** (1.6J). Zero marginal cost — no feature flag, no hardware dependency,
   already wired and registered by default.

### Pursue next, with a named prerequisite

4. **FirmwareManagement** (1.6J) / the firmware half of Core's optional features on 2.x. Message-
   complete, but only claim it for a *specific product* once that product supplies a real
   `FirmwareVerifier` — claiming it for the library alone overstates what `NoFirmwareVerifier`
   does. Cheap for a product that already has firmware-signing infrastructure (most do, for
   non-OCPP reasons); expensive for one that doesn't.
5. **Variable Monitoring** (the Library-claimable half of Advanced Device Management). Fully
   wired and self-contained. The prerequisite this entry originally named — a `CAPABILITY_GATES`
   row so the claim rests on C3.5's verified-consistency footing rather than on inspection — is
   **already met**: `variable_monitoring` gates `MonitoringCtrlr` and its handler registration,
   and C4.3 made that registration runtime-gated to match. On the evidence, this belongs with the
   "pursue first" group above; it is left here only because nothing has re-ranked the list since.

### Do not claim yet — and why

6. **Advanced Security.** The transport half (security profiles 1–3) is genuinely done and worth
   noting in marketing material, but the profile as OCA defines it bundles certificate lifecycle,
   firmware signing, and OCSP together, and two of those three currently have no-op integrator
   defaults. Claiming Advanced Security today would pass only if the specific unit under test had
   a real `FirmwareVerifier`/`OcspChecker`/`KeyStore` wired in — which makes this a **Product**
   claim at best, and one this document recommends *not* pursuing at the library level at all.
   Revisit once F-workstream security items close further and, ideally, once `docs/THREAT-MODEL.md`
   is refreshed to stop understating what's already implemented (§3 above).
7. **Payment.** Blocked outright per §3 — there is no live-status trait yet, so no integrator can
   bridge the gap. Not worth OCTT time until that trait exists.
8. **DER control.** Blocked outright per §3, same reasoning as Payment — no trait to actuate a
   curve exists. Do not claim; do not spend OCTT time on it until a `hardware::DerActuator`-shaped
   trait (or equivalent) lands.
9. **Advanced User Interface, Periodic event streams (the Product half of Advanced Device
   Management), Battery Swap.** All three are legitimately **Product**, not **Library**, claims —
   correct to support in code, wrong to certify without a specific integration in hand. Recommend
   these live in a product's own certification paperwork, referencing this crate's message
   coverage, rather than in this crate's own claim set. Battery swap in particular is niche enough
   (per the README's own framing) that it likely never belongs in a general-purpose claim.
10. **1.6J's Security Whitepaper profile as such.** Two messages short (`GetLog`/
    `LogStatusNotification`) and, per `docs/UPSTREAM-GAPS.md`'s D2.2 analysis, a much bigger
    unimplemented surface behind those two (signed firmware, remote log retrieval, CSMS-driven
    certificate lifecycle over 1.6J specifically) than the message count suggests. That document
    already recommends declaring 1.6J security out of scope in favor of 2.0.1/2.1's Advanced
    Security profile; this document does not see new evidence to override that recommendation,
    and Advanced Security itself is not ready yet either (item 6) — so 1.6J security is simply
    not on the roadmap of things to pursue.

### One profile this document explicitly declines to rank

**ISO 15118 / Plug & Charge** is not an OCPP certification profile in its own right (it's a
separate ISO standard family with its own conformance program), so it does not appear in the
ranked list above. It is, however, a prerequisite baked into parts of Advanced Security and the
README's V2X hardware class, and per §3 it needs an integrator HLC stack this crate does not ship
— worth stating plainly rather than folding into either "pursue" or "don't pursue".

---

## 5. What would change this recommendation

- **Vendoring or fetching `docs/OCPP-2.1/`/`docs/OCPP-2.0.1/`'s Part 5 PDFs** so the profile names
  and page citations in §2 can be re-verified directly rather than via the README's prior
  transcription — flagged as an M5 blocker already (`docs/PRODUCTION-ROADMAP.md`'s "spec material
  is gitignored" bullet); this document inherits that same limitation.
- **A `hardware::DerActuator`-shaped trait** (or whatever the maintainers name it) landing would
  move DER control from "Blocked" to at least "Product".
- **A live payment-status trait** would do the same for Payment.
- **A real `FirmwareVerifier`/`OcspChecker` reference implementation** (even an example one, not
  necessarily shipped) would let item 6 above move from "do not claim" to "Product, with a named
  prerequisite" alongside FirmwareManagement.
- **H3.2's manual test-case sweep** may surface Core-profile gaps this message-coverage-based
  analysis cannot see — "every message is wired" is necessary, not sufficient, for passing the
  OCTT's actual test cases, and this document says so rather than treating coverage numbers as a
  compliance guarantee.
