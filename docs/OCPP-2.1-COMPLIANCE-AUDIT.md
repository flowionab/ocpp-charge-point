# OCPP 2.1 compliance audit — spec vs. this crate

**Date:** 2026-08-11, §2.13–§2.18 and §3 re-swept 2026-08-12, §2.13–§2.17 closed 2026-08-12,
§2.19–§2.26 opened by the K28/K29, E, C, N and Q sweeps 2026-08-12/13 ·
**Baseline:** `main` @ `4c6abe3`, sweep baseline `02dfa69` · **Spec:** OCPP 2.1 edition 2 (`docs/OCPP-2.1/`, part 2 specification +
part 2 appendices v2.1 CSVs + errata 2026-06).

This document is the *comparison* half of certification readiness: what the spec requires of a
Charging Station, what this crate does, and where the two differ. It does not replace
`docs/CERTIFICATION.md` (which decides *which profiles to claim*) — it feeds it. Running the OCTT
is still H3.1.

Because `docs/OCPP-2.1/` is gitignored, the requirement text itself is **not** reproduced here.
Every finding cites the requirement ID so it can be looked up in a worktree that has the PDFs.

---

## 0. Method, and how far it was verified

The spec's part 2 was extracted with `pdftotext -layout` and parsed into a requirement database:

| Measure | Count |
|---|---|
| Use cases (A01…S04, 19 functional blocks) | **177** |
| Numbered functional requirements (`*.FR.*`) parsed | **1909** |
| …of which state an obligation on the **Charging Station** | **1257** |
| …CSMS-side or informational | 652 |
| Required device-model variables (appendix `dm_components_vars.csv`) | **122** |
| Standardized security event types (appendix `security_events.csv`) | **21** |

Three verification techniques were used, and the findings below are labelled by which applies:

- **[MECHANICAL]** — a machine-checkable diff (registered variables vs. the appendix; wire fields
  that are literal `None`; a device-model variable that appears only at its registration site and
  is never read). These are certain.
- **[READ]** — the module was read and the behaviour confirmed absent or present.
- **[SURVEY]** — the block was surveyed at module/message level only. Absence of a finding in a
  `[SURVEY]` block is *not* evidence of conformance.

**The honest headline: message coverage is complete, behavioural coverage is not.** All 91 OCPP 2.1
actions are wired (`scripts/message-coverage.py`, re-run on this baseline). That is necessary and
not sufficient — a certification test suite drives *behaviour*, and the gaps below are behavioural.

---

## 1. Verified-good

These were checked and found conformant; they are the crate's strongest ground.

| Area | Evidence |
|---|---|
| **Message coverage, 2.1** | 91/91 actions wired. 2.0.1: 64/64. 1.6J: 37/39 (`GetLog`/`LogStatusNotification`, a deliberate scope call — `docs/UPSTREAM-GAPS.md`). |
| **Security event types (A04)** | All 21 appendix event types present in `src/security.rs`, with correct wire spellings (`InvalidCSMSCertificate`, `InvalidTLSVersion`, …) and criticality. |
| **Security profiles (A00 §1.2–1.3, A05)** | All three modelled; profile 3 (mutual TLS) implemented end to end. Downgrade gate matches A05.FR.08/09/10 including the "3 → 2 only, never to 1" rule (`src/security_profile.rs`). |
| **Smart charging composition (K01–K10)** | Real: purpose precedence, stack levels, capping, and a composite schedule projected onto hardware on both change- and time-triggers (`src/smart_charging/`). Not a stub. |
| **Variable monitoring (N02–N08)** | `SetVariableMonitoring`/`SetMonitoringBase`/`SetMonitoringLevel`/`ClearVariableMonitoring`/`GetMonitoringReport`/`NotifyEvent` implemented against a real monitoring engine (`src/variable_monitoring.rs`, 2.8 kLOC). |
| **`SetVariables`/`GetVariables` result plumbing (B05/B06)** | Per-element results, `UnknownComponent`/`UnknownVariable`/`NotSupportedAttributeType`/`RebootRequired` all produced correctly. (Value *validation* is a gap — §2.3.) |
| **Offline authorization ordering (C13/C14 partial, C15)** | Local list before cache before deny, each gated on the real device-model switch (`AuthCtrlr.LocalAuthorizeOffline`, `AuthCacheCtrlr.Enabled`/`LifeTime`). |
| **Plug & Charge (C07)** | Implemented, including the C07.FR.07 refusal to accept a contract offline without `ISO15118Ctrlr.ContractValidationOffline`. |
| **Certificate renewal (A03.FR.02–.10, .24, .25)** | Proactive renewal, validity check, `Rejected` + `InvalidChargingStationCertificate` on a bad cert, reconnect on accept, `DiscardedRenewedClientCertificate`. |

---

## 2. Gaps — ranked by certification impact

### 2.1 ~~Blocking~~ **Closed by CV1** · required device-model variables — **0 of 122 remain** [MECHANICAL]

> **Partially closed.** CV1.1 landed the five availability variables struck through below (plus the
> optional `Connector.AvailabilityState`, which B07.FR.09 wants even though the appendix marks it
> **Closed.** CV1.1 the availability pair, CV1.2 `ClockCtrlr.DateTime`, CV1.3 the nine per-slot
> `NetworkConfiguration` ones, CV1.4 `MonitoringCtrlr`, CV1.5 the hardware facts, CV1.6 the
> capability-gated blocks. 49 missing at the time of the audit; **0** now, re-verified against
> `dm_components_vars.csv`.

`GetBaseReport(FullInventory)` is one of the first things an OCTT run does, and the appendix marks
these `Required? = yes`. They are absent from both `DEFAULT_VARIABLES` (`src/state/device_model.rs`)
and `CAPABILITY_GATED_VARIABLES` (`src/device_model.rs`):

| Component | Missing required variables |
|---|---|
| `ChargingStation` | ~~`AvailabilityState`~~, ~~`Available`~~, `SupplyPhases` |
| `EVSE` | ~~`AvailabilityState`~~, ~~`Available`~~, `Power`, `SupplyPhases` |
| `Connector` | ~~`Available`~~, `ConnectorType`, `SupplyPhases` |
| `ClockCtrlr` | ~~`DateTime`~~ |
| `NetworkConfiguration` | ~~`ApnEnabled`, `BasicAuthPassword`, `MessageTimeout`, `OcppCsmsUrl`, `OcppInterface`, `OcppTransport`, `OcppVersion`, `SecurityProfile`, `VpnEnabled`~~ |
| `MonitoringCtrlr` | ~~`ItemsPerMessage[SetVariableMonitoring]`~~, ~~`BytesPerMessage[SetVariableMonitoring]`~~ |
| `TariffCostCtrlr` | `TariffFallbackMessage[<language>]`, `TotalCostFallbackMessage[<language>]` |
| `V2XChargingCtrlr` | `Enabled`, `SupportedOperationModes`, `SupportedEnergyTransferModes` |
| `WebPaymentsCtrlr` | `URLTemplate`, `SharedSecret`, `TOTPVersion`, `Length`, `ValidityTime` |
| `ACDERCtrlr` | `ModesSupported` |
| `DCDERCtrlr` | 16 inverter nameplate/limit variables |

(Checked against both literal tables *and* the programmatic path — `capability_gate_events`
(`src/device_model.rs:517`) registers a `*Ctrlr.Available` per `CAPABILITY_GATES` row at runtime, and
no row above is one of those.)

Most are cheap: the crate already *owns* the facts behind `ChargingStation`/`EVSE`/`Connector`
availability, `ClockCtrlr.DateTime`, and every `NetworkConfiguration` value — they simply were never
projected into the device model. The DER/`WebPayments` ones are integrator hardware facts and belong
behind their capability gates.

`src/reporting.rs`'s `SummaryInventory` already looks for `AvailabilityState`/`Available` by name, so
before CV1.1 it returned an empty summary report on a station that had never registered them — i.e.
B07's summary base was structurally empty. It now carries the charge point, every EVSE and every
connector, which is what B07.FR.09 defines that base to be.

### 2.2 ~~Blocking~~ **Refusal closed by CV2.1; honouring is per-row** · registered variables that are decorative [MECHANICAL]

A CSMS can `SetVariables` these to any value; the value is stored, reported back, and changes
nothing. Several are behaviourally load-bearing in the OCTT:

> **Status.** CV2.1 measured the real figure — **30 of 49** built-in defaults — and made every one of
> them refuse a `SetVariables` instead of accepting it (B05.FR.09). Each row below is still
> *unhonoured*; what changed is that a CSMS is now told so. CV2.9 (`OfflineTxForUnknownIdEnabled`)
> and CV2.10 (`SecurityCtrlr.OrganizationName`) have since been made live.
>
> **39 was a lower bound.** The detector flags a variable whose name appears *nowhere* outside its
> registration site, so it counts a doc comment or a test as a use. `AuthCtrlr.AuthorizeRemoteStart`
> is the known example it misses: registered, mentioned only in comments and tests, and — per its own
> doc comment — never consulted. A stricter sweep belongs to CV2.1.

| Variable | Use cases it governs | Consequence today |
|---|---|---|
| ~~`TxCtrlr.TxStartPoint`~~ **CV2.2** | E01, E02, E03, F02 | Honoured. `EVConnected`, `Authorized` and `PowerPathClosed` each pick a different transition; the three OCPP values this crate cannot observe are refused rather than ignored. |
| `TxCtrlr.TxStopPoint` | E06, E09, E10, F03 | Same, for stop. |
| `TxCtrlr.EVConnectionTimeOut` | E03.FR.15, F02.FR.07/08 | No plug-in timeout exists; a remote start that is never plugged in never ends. |
| ~~`TxCtrlr.StopTxOnEVSideDisconnect`~~ **CV2.4** | **E09 vs E10** | *Corrected finding:* cable-disconnect-while-charging was a **no-op**, so the crate owned neither branch — the integrator's binding decided by choosing which event to send. Both branches are now the crate's, selected by the variable. |
| ~~`TxCtrlr.StopTxOnInvalidId`, `TxCtrlr.MaxEnergyOnInvalidId`~~ **CV2.5** | E05 | Honoured — stop at once, or grant the configured allowance and end with `DeAuthorized`. The revocation event still has no producer: the crate does not inspect a `TransactionEventResponse`'s `idTokenInfo`. |
| `AuthCtrlr.OfflineTxForUnknownIdEnabled` | C15 | Offline transaction for an unknown id not gated on its own switch. |
| `SampledDataCtrlr.Tx{Started,Updated,Ended}Measurands`, `TxUpdatedInterval`, `TxEndedInterval` | J02, J03, F01.FR.14/15 | No measurand configuration at all — the CSMS cannot say what to sample. (Known and documented in `src/meter_values.rs`.) |
| `AlignedDataCtrlr.Measurands`, `TxEndedMeasurands`, `TxEndedInterval` | J01, J03 | Same, for clock-aligned data. `Interval` *is* honoured. |
| `OCPPCommCtrlr.OfflineThreshold` | B04.FR.01/02 | See §2.5. |
| `OCPPCommCtrlr.UnlockOnEVSideDisconnect` | E09 | Unlock-on-disconnect policy ignored. |
| `ChargingStation.MinimumStatusDuration` | G01 | No status debouncing; a bouncing connector floods the CSMS. |
| `DeviceDataCtrlr.ItemsPerMessage`, `BytesPerMessage` | B06.FR.16/17, B05, B07, B08 | Oversized requests are not refused with `OccurrenceConstraintViolation`/`FormatViolation`. |
| `LocalAuthListCtrlr.ItemsPerMessage`, `BytesPerMessage` | D01 | Same, for `SendLocalList`. |
| `SecurityCtrlr.OrganizationName` | A02/A03 | The CSR builder supports `organizationName` but never sources it from the device model, so `SignCertificate` CSRs omit the O= RDN the CA policy expects. |
| `TariffCostCtrlr.Currency`, `TariffFallbackMessage`, `TotalCostFallbackMessage` | I04, I05 | See §2.7. |
| `PaymentCtrlr.*` (11 variables) | C18–C24 | Placeholders, not driven by a terminal — already flagged in `docs/CERTIFICATION.md`. |

The generic risk here is B05.FR.09: a variable the station cannot actually honour is supposed to be
**`Rejected`**, not silently accepted. Every row above is currently accepted.

### 2.3 ~~Blocking~~ **Closed by CV3** · `SetVariables` performed no value validation (B05.FR.07/B05.FR.08) [READ]

**Fixed.** `validate_value` now checks type, range and membership before the write, and a
`VARIABLE_BOUNDS` table declares the bounds OCPP states. Original finding follows.

`resolve_and_apply_set` checked mutability and `constant`, then wrote the
string through unchanged. It never consults `VariableCharacteristics::data_type`, `min_limit`,
`max_limit`, or `values_list`. `SetVariables(OCPPCommCtrlr.HeartbeatInterval = "banana")` answers
`Accepted`. B05.FR.07 (badly formatted → `Rejected`) and B05.FR.08 (out of range → `Rejected`) are
both unimplemented, and every default variable is registered with `min_limit: None, max_limit: None,
values_list: None`, so there is nothing to validate against even if the check existed.

### 2.4 ~~Blocking~~ **Closed by CV4** · nothing gated outbound traffic on boot acceptance (B01.FR.08, B02.FR.02, B03.FR.02) [READ]

**Fixed.** Queues are gated on acceptance, waiting does not burn a message's attempt cap, and
acceptance kicks a flush so a held backlog does not then sit silent. `RequestStartTransaction`/
`RequestStopTransaction` refuse while `Pending`. Original finding follows.

`ChargePointState.registration` was recorded but never read outside tests. The offline-queue retry
timer (`ChargePointBuilder::offline_queue_retries`, `src/builder.rs:357`) and
`run_offline_queue_retries` (`src/offline_queue.rs:379`) flush unconditionally.

B01.FR.08 is explicit that between power-on and an `Accepted`/`Pending` BootNotification the station
sends nothing else — *"This includes cached OCPP messages that are still present in the Charging
Station from prior sessions."* A station that reboots with a backlog and gets `Pending` will emit
queued `TransactionEvent`s immediately. B03.FR.02 (`Rejected` → total silence until the retry
interval) has the same hole.

Related and also missing: **B02.FR.05** — while `Pending`, `RequestStartTransaction` and
`RequestStopTransaction` must both answer `Rejected`. `handle_request_start_transaction`
(`src/remote_control.rs:153`) does not consult the registration status.

### 2.5 ~~Blocking~~ **Closed by CV5** · no connector-state resynchronisation after boot or outage (B01.FR.05, B04.FR.01/02) [READ]

**Fixed.** `ChargePointBuilder::connector_status_resynchronisation` sweeps every connector on
acceptance and after a long outage; a short outage correctly reports nothing extra, because the
queued changes already are the report B04.FR.02 asks for. Original finding follows.

Status was reported on *change* only (`run_status_notifications`). There was
no path that reports the current state of every connector after a BootNotification is accepted
(B01.FR.05), and none that implements B04's two-way split after an outage: report **all** connectors
when the offline period exceeded `OCPPCommCtrlr.OfflineThreshold`, only the **changed** ones
otherwise. `reregister_on_reconnect` (`src/connection.rs:65`) re-sends BootNotification and stops
there.

(2.1 prefers `NotifyEvent(variable.name = "AvailabilityState")` for this and marks
`StatusNotification` deprecated, but explicitly still permits `StatusNotification` — so the *message*
choice is not itself a defect. The missing resynchronisation is.)

### 2.6 ~~High~~ **Mostly closed by CV6** · remote-start metadata was never carried into `TransactionEvent` (F01/F02) [MECHANICAL]

**Fixed, except `offline`.** `remoteStartId` and `reservationId` are carried on `Transaction` and
quoted on every event; `triggerReason` reports `RemoteStart` for a remotely started transaction.
`offline` remains open as CV6.1 — the wire message is built at send time, inside the queue flush, so
the encoder cannot currently see that the event it is encoding was held. Original finding follows.

In `src/transactions.rs` the wire `transaction_info` was built with:

- `remote_start_id: None` (lines 351, 920) — violates **F01.FR.25**, **F02.FR.01**, **F02.FR.21**.
- `reservation_id: None` — violates **F02.FR.06**, and breaks the H03 reservation→transaction link.
- `offline: None` — **E12** (informing the CSMS that a transaction occurred while offline) cannot be
  expressed.

Also `trigger_reason_for` (`src/transactions.rs:202`) hardcodes `Started → Authorized`, so a
remotely-started transaction never reports `triggerReason = RemoteStart` (**F01.FR.19**,
**F02.FR.21**).

### 2.7 ~~High~~ **Mostly closed by CV7** · `RequestStartTransaction` acceptance rules (F01.FR.21–.24, F02.FR.23–.26) [READ]

**Fixed.** F02 now works: a request with no cable yet is accepted and held, then dispatched when the
driver plugs in. The four rejection conditions are checked explicitly. **`EVConnectionTimeOut`
(F02.FR.07/.08) is still open** as CV2.3 — a held request is never released by a timer. Original
finding follows.

`handle_request_start_transaction` accepted only if it could find a connector in
`ConnectorState::Locked`, and otherwise returned `Rejected`. Two consequences:

1. **F02 ("Remote Start First") does not work.** The spec's whole point is that the request arrives
   *before* the cable is plugged in; the station accepts, waits `EVConnectionTimeOut`, and either
   starts on plug-in or ends with `stoppedReason = Timeout`. Today it is rejected outright.
2. The specific rejection conditions the OCTT drives — EVSE `Reserved` for a different
   idToken/groupIdToken (FR.21/.22), `Unavailable`/`Faulted` (FR.23), `Occupied` with an already
   authorized transaction (FR.24) — are not distinguished; they happen to be rejected only as a
   side effect of "not `Locked`".

Related unimplemented switches: `AuthCtrlr.AuthorizeRemoteStart` (F01.FR.01/.02, F02.FR.09/.10 — the
authorize-or-not branch) *is* registered but is never consulted, as its own doc comment at
`src/state/device_model.rs:329` says. `DisableRemoteAuthorization` is absent entirely.

### 2.8 ~~High~~ **Closed by CV8** · local cost calculation (I01–I12) [READ]

**Fixed.** `state::Tariff` carries the priced structure, `GetTariffs` round-trips (I09), and
`crate::pricing` prices a session locally — fixed-point, truncating so an ambiguous fraction never
overcharges. I07/I08/I11/I12 are closed. I01–I06 remain a *product* claim: they are what a driver
display shows, and this crate ships no display. Original finding follows.

`src/state/tariff.rs` deliberately modelled a tariff as `{id, currency, valid_from}` and dropped the
priced structure (energy/time/fixed-fee components and their conditions). Consequences:

- **I07–I12** (the entire "Local Cost Calculation" group) are unimplemented — no running cost is
  derived from a tariff.
- **I09 `GetTariffs` cannot round-trip.** The CSMS sets a tariff with `SetDefaultTariff` and gets
  back a tariff without its prices. An OCTT case that sets and reads back will fail.
- **I01–I06** (driver-visible tariff and running/final cost) depend on the above plus a display.

`CostUpdated` (the CSMS *telling* the station a cost) works and is unaffected.

### 2.9 Medium · `SignCertificate` retry discipline (A02.FR.17–.19, A03.FR.17–.19) [MECHANICAL]

`CertSigningWaitMinimum` and `CertSigningRepeatTimes` appear nowhere in `src/`. The required
behaviour — resend `SignCertificate` after `CertSigningWaitMinimum`, doubling the back-off each time
no `CertificateSigned` arrives, stopping at `CertSigningRepeatTimes` until a `TriggerMessage`
restarts it — is not implemented, and neither variable is registered.

`MaxCertificateChainSize` (A02.FR.16/A03.FR.16) is also absent; that one is a `MAY`.

### 2.10 ~~Medium~~ **Closed by CV10** · security log entry on credential change (A01.FR.11) [READ]

**Fixed.** Rotation is real, persisted through `KeyStore`, applied on next connect, rolled back on
repeated failure, and logged without the value. Original finding follows.

A `SetVariables` writing `SecurityCtrlr.BasicAuthPassword` must be logged in the security log
(A01.FR.11) without disclosing the value (A01.FR.12). No such event is raised. The redaction half is
fine by construction — the value is never logged — but the required *record* is missing. (This
compounds with §2.1: `NetworkConfiguration.BasicAuthPassword` is not registered at all, so the
CSMS-facing write path from A01.FR.02 does not exist either.)

### 2.11 Medium · G05 (Lock Failure) has no signal [MECHANICAL]

`LockFailure` appears nowhere. The crate has `ConnectorState::Faulted`, so a binding *can* express a
fault, but there is no lock-specific path and no way for a CSMS to distinguish a lock failure.

### 2.12 Known and previously documented

Carried forward from `docs/CERTIFICATION.md` §3 — re-confirmed as still current, not re-derived:

- **DER control (R01–R05)**: messages wired, but no `crate::hardware` trait can *apply* a curve or
  setpoint. A station stores and reports a control it never enacts.
- **Payment (C18–C24)**: `PaymentCtrlr`'s status variables are placeholders, not terminal-driven.
- **Firmware signature verification (L01)**: `NoFirmwareVerifier` refuses every signed update — a
  fail-safe default, but a product claiming L01 must supply a real verifier.
- **OCSP (M06/M07)**: `NoOcspChecker` is a no-op default with the same caveat.
- **ISO 15118 HLC (K15–K20, Q, R)**: the crate relays EXI opaquely and does not run a 15118 session
  state machine; Q/R conformance is a product claim resting on the integrator's stack.

### 2.13 ~~High~~ **Closed by CV13** · an external charging limit is reported to the CSMS but never enforced (K11.FR.01, K12.FR.01, K13.FR.01, K27.FR.01) [READ]

**Fixed.** `smart_charging::external_charging_limits` turns whichever limits are in force for an
EVSE — the station-wide one and the EVSE's own — into `ExternalConstraints` capping profiles, and
`composing_profiles` joins them onto `charging_profiles.applying_to()`. Both composition sites use
it: the projection that drives `Connector::set_current_limit`, and `GetCompositeSchedule`, so what
the CSMS is *shown* stays the same curve the charge point will *apply*. Withdrawing the limit
releases the connector without the CSMS reinstalling anything (K13.FR.01, now non-vacuously).

Two cases still report without binding, both by construction and both stated in
`ExternalChargingLimit`'s docs: a limit carrying no `schedule` has no value to enforce (recording
one now warns), and a watt-denominated limit on a projection built without `SupplyCharacteristics`
is skipped rather than mis-scaled — the same rule a watt-denominated CSMS profile already gets.

An external limit is still not persisted across a reboot — it never was — so a restart drops both
the limit and the enforcement of it. That leaves the station's behaviour and its own reports
consistent with each other, which is what this finding was about, but it does mean an EMS must
re-push after a power cut. Worth a row of its own if a deployment needs it; not part of this one.

K11.FR.04/K13.FR.03 (`triggerReason = ChargingRateChanged`) stay open as §2.17/CV18; enforcement was
their prerequisite, not their implementation. Original finding follows.

Found by CV12's K sweep. An integrator's energy-management binding pushes
`ChargePointEvent::ExternalChargingLimitSet`; `ChargePointState::set_external_charging_limit`
(`src/state/charge_point_state.rs:2274`) records it on `EvseState::external_charging_limit` (or
`station_external_charging_limit`) and pushes exactly one effect — the
`NotifyChargingLimit` that tells the CSMS about it. Nothing else happens.

**The limit is never applied.** `smart_charging::projection` composes from
`state.charging_profiles.applying_to(evse_id)` alone, and `ChargingProfileStore` holds *installed
charging profiles*; the external-limit field is a separate slot that `compose` has no way to see.
So a station under a 6 kW EMS limit tells the CSMS it is limited to 6 kW and keeps drawing whatever
the CSMS profile allowed. K11.FR.01 is a `SHALL`, and this is the failure direction that matters:
reporting a limit that is not enforced is worse than not supporting external limits at all, because
the CSMS's own load calculation now double-counts a reduction that never happened.

K13.FR.01 ("SHALL NOT limit charging anymore based on the previously received limit") passes only
vacuously — there was never a limit in force to stop applying.

Not a regression and not a mis-claim: `docs/PRODUCTION-ROADMAP.md` B2.8 scoped these four messages
as "notify-flows that report a limit's *origin* rather than apply one", and delivered exactly that.
What was never done is ask what K11 requires *behaviourally* of a station that sends them — which is
precisely the residual risk §3 was written to name.

### 2.14 ~~High~~ **Closed by CV15** · transaction limits are entirely unimplemented (E16, 20 FRs) [MECHANICAL]

**Fixed, with one ceiling of the four declared unsupported rather than half-done.**
`state::TransactionLimit` is a real internal type on `Transaction`. The CSMS sets one on a
`TransactionEventResponse` (FR.02) and an integrator sets one on the driver's behalf; either is
filtered to what this build enforces (FR.13), clamped so a local one never exceeds the CSMS's
(FR.04), confirmed back exactly once with `triggerReason = LimitSet` (FR.01/.03), and enforced.
Reaching a ceiling commands 0 A, moves the connector to `SuspendedEVSE` and reports the trigger
reason that names *which* ceiling (FR.05); raising it past where the transaction stands resumes
(FR.14); setting one below where it already stands binds at once (FR.10). Cost uses the station's
own running total where a tariff prices the session and the CSMS's figure otherwise, which is
FR.16 and FR.15 in that order.

Two things worth reading in the roadmap's CV15 section rather than inferring: **E16.FR.06 is
unreachable here** (the suspend-vs-end branch needs `TxStopPoint = EnergyTransfer`, which this
crate's `values_list` refuses, so suspension is always the answer), and **`maxTime` needed a
clock** — it is the only ceiling that cannot be decided from a meter reading. **CV21 has since
closed it**: `crate::transactions::run_transaction_time_limits` measures the elapsed time, and
`TxCtrlr.SupportedLimits` gains `maxTime` when that sweep is spawned, so a build without it
neither advertises the ceiling nor records one (E16.FR.12/.13).

C17 (prepaid) is no longer stranded: its whole mechanism is a CSMS-set `maxCost`, which now
records, confirms and binds. Original finding follows.

`TransactionLimit` occurs once in `src/`, as a type alias in `src/wire.rs:1083`. It is never
constructed, never read from a `TransactionEventResponse`, and never enforced.
`TriggerReasonEnum::LimitSet` occurs zero times, and `transactions::trigger_reason_for` has no arm
that could produce it.

So none of E16 holds: the station never reports a limit it set (E16.FR.01), never honours one the
CSMS set in a `TransactionEventResponse` (E16.FR.02/.03), and never stops a transaction on reaching
a cost, energy, SoC or time limit. This also strands **C17 (authorization with prepaid card)**,
whose whole mechanism is a CSMS-set `maxCost` — and it is the one gap that directly limits CV8's
value, since the crate now computes a running cost locally (`EvseState::running_cost`) and still has
no way to act on a cost ceiling.

### 2.25 Low · a periodic event stream batches by interval only, never by value count (N15.FR.07/.08) [READ]

Found by CV12.5's N sweep, and already self-documented in `crate::periodic_event_stream`'s module
docs. `PeriodicEventStreamParams.values` is stored and reported faithfully by
`GetPeriodicEventStream`, but the driver loop sends exactly one data element per sweep, so:

- **N15.FR.08** ("send as soon as `params.values` values are available") never fires - the interval
  is the only trigger.
- **N15.FR.07** ("no more than `params.values` values at a time") holds trivially, one being no
  more than any positive number.
- **N15.FR.04**'s `pending` is honest at `0`: nothing is buffered, so nothing is pending.

The failure mode is over-sending rather than silence: an `interval` of `0` is clamped to one
second, so a CSMS that configured a stream purely by value count still receives data - roughly one
message per second carrying one element, where it asked for one message per `values` elements. On a
metered or constrained link that is the difference the batching was for.

N09-N14 were read and hold: `GetCustomerInformation`/`ClearCustomerInformation` answer against this
crate's real state rather than a fabricated store (`crate::customer_information`'s own docs walk
each place), and the stream lifecycle messages (open, get, close, adjust) are implemented against a
real store with a `StateLimits` bound.

### 2.26 Medium · `IdToken.additionalInfo` is not modelled, so Q01.FR.02's EVCCID cannot be reported [MECHANICAL]

Found by CV12.6's Q sweep. `crate::state::IdToken` is `{ value, kind }`; OCPP's `IdTokenType` also
carries `additionalInfo`, a list of `(additionalIdToken, type)` pairs, and every wire encoder in
this crate sets `additional_info: None`.

Q01.FR.02 needs exactly that field: on an ISO 15118-20 transaction the station must put the EVCCID
into `idToken.additionalInfo.additionalIdToken` with `type = "EVCCID"` on the
`TransactionEvent(Started)`, because the CSMS uses it to decide whether to allow bidirectional
transfer. `IdTokenKind::EVCCID` exists as a *kind* - an identifier that **is** an EVCCID - which is
a different thing from an identifier *accompanied by* one.

Q01's other requirements rest on an ISO 15118-20 stack this crate does not run (§3's standing
note), so they are the product's. This one is not: the field is on a message this crate builds, so
no integrator can supply it from outside. Q01.FR.01 is CSMS-side.

### 2.24 High · `hardware::PaymentTerminal` can be read but not driven, so C19-C23's station-side requirements are unreachable [READ]

Found by CV12.4's C sweep. The trait has two methods - `info()` and `status()` - and both *ask*
the terminal something. Nothing on it *tells* the terminal anything, and the C19-C23 requirements
are almost entirely instructions to a terminal:

| Requirement | Asks the station to |
|---|---|
| C19.FR.01 | instruct the terminal to release the authorization amount and cancel the payment |
| C21.FR.01 | settle the total cost of the transaction via the terminal |
| C21.FR.06 | *not* settle via the terminal when the CSMS settles instead |
| C23 | raise the authorized amount during a session (incremental authorization) |

The *reporting* half exists and is good: `crate::payment::report_settlement` sends
`NotifySettlement` with the status, amount, time, transaction and `pspRef` C21.FR.02 asks for, and
`report_web_payment_started`/`validate_vat_number` cover their own messages. So an integrator can
drive their terminal themselves and hand this crate the outcome to report. What they cannot do is
implement a `crate::hardware` trait and have the crate carry out C19-C23 - which is the promise
`CLAUDE.md` makes ("integrators should only ever need to supply hardware bindings").

The same shape as §2.18's missing renegotiation surface and the DER actuation trait, and the third
member of that set. `docs/CERTIFICATION.md` §3 names a payment blocker, but the one it names -
"nothing drives the live status variables from a real payment terminal" - **was closed by CV2.11**
and is stale; this is the gap that actually remains.

C24 (ad hoc payment via a stand-alone terminal) is the one member of the group this shape does not
block, since a stand-alone terminal authorizes on its own account and the station's part is to
report - which it can.

### 2.21 Medium · a saturated offline queue drops exactly the messages E11.FR.05 says to keep [READ]

Found by CV12.3's E sweep. `OfflineQueue`'s two overflow policies are `DropOldest` ("evict the
oldest queued message") and `DropNewest` ("reject the new message"), and E11.FR.05 names both as
the things not to do:

> When dropping TransactionEventRequest(eventType = Updated) messages, the Charging Station SHALL
> drop intermediate messages first (2nd message, 4th message, 6th message etc.), not start dropping
> messages from the start or stop adding messages to the queue.

The rule exists to protect the two messages a billing system cannot reconstruct - the `Started` and
the `Ended` - by thinning the interchangeable `Updated`s between them. This crate's policies do the
opposite at whichever end they act on: `DropOldest` loses the `Started`, `DropNewest` loses the
`Ended`, and neither distinguishes an `Updated` from either. `DropNewest` is the policy the docs
recommend for transaction events, so the recommended configuration is the one that loses the
message closing the session.

Dropping at all is a `MAY` (E11.FR.04), so a station that never drops is conformant - but this one
drops, and FR.05 constrains how. The fix is a policy that knows the message kind, which the queue
deliberately does not (it is generic over `M`); the transaction queue would need to supply a
predicate the way it already supplies `mark_offline`.

### 2.22 Low · the transaction-event retry interval does not escalate (E13.FR.03) [READ]

Found by CV12.3. E13.FR.03: "The Charging Station SHALL wait as many seconds as specified in its
MessageAttemptIntervalTransactionEvent key, **multiplied by the number of preceding transmissions
of this same message**" - the spec's own example is 60 s, then 120 s, then discard.

`run_offline_queue_retries` waits `message_attempt_interval_secs` before *every* sweep, with no
per-message attempt counter feeding a multiplier. E13.FR.02 (retry up to `MessageAttempts`) and
E13.FR.04 (discard after the final attempt) are both implemented; only the escalation is not, so a
CSMS that is rejecting messages is retried more often than the spec prescribes - which is load on
exactly the CSMS the back-off exists to spare.

### 2.23 Medium · E17 (resuming a transaction after interruption) is absent - 17 FRs [MECHANICAL]

Found by CV12.3. `TxResumptionTimeout`, `TxAllowEnergyTransferResumption` and
`triggerReason = TxResumed` occur zero times in `src/`. This crate closes a recovered transaction
out instead: `crate::persistence` reports it `Ended` with `StopReason::PowerLoss` on the next boot
(`docs/PRODUCTION-ROADMAP.md` §7.4, E4.1).

That is a deliberate, documented choice and it is safe - a session that cannot be resumed is closed
honestly rather than left half-alive - but E17 is a 2.1 use case whose requirements are `SHALL`s,
and no audit row said so until this sweep. Closing it needs the resumption timeout, the charging
state to be restored per E17.FR.11-13's three-way branch on
`TxAllowEnergyTransferResumption`, and the `TxResumed` event. The persisted record already carries
what E17.FR.01 asks to be stored.

### 2.19 Medium · `ChargingSchedulePeriod.operationMode` is never read, so K29.FR.03's delegation does not happen [MECHANICAL]

Found by CV12.2's K28/K29 sweep. 2.1 added `operationMode` to a schedule period, and `map_schedule`
(`src/smart_charging/ocpp_2_1.rs`) builds a `ChargingSchedulePeriod` from `start_period`, `limit`
and `number_phases` only - the field is neither read on the way in nor set on the way out (three
literal `operation_mode: None` at the encode sites, zero reads).

The concrete cost is **K29.FR.03**: a CSMS may install a `TxProfile`/`TxDefaultProfile` with
`chargingProfileKind = Dynamic` and `operationMode = ExternalLimits`/`ExternalSetpoint`, which
means *this profile's limit is whatever the on-site system says it is*. A station that ignores the
field applies the numbers the CSMS happened to put in the single period instead, and never
delegates - so the CSMS believes an EMS is driving the limit while the station holds a static one.
The same class as §2.16: the station's behaviour and its report describe different charge points.

Table 95 of the spec maps `operationMode` per `chargingProfilePurpose`, so the field also governs
setpoint semantics more broadly (`CentralSetpoint`, `LocalFrequency`, `Idle`, …). This crate models
limits only and says so - `DynamicScheduleUpdate::carried_unprojectable_values` already logs when
an update carried setpoints it has no hardware hook for - so the wider gap is declared. K29.FR.03
is the part that is not.

### 2.20 Low · `SmartChargingCtrlr.SupportedAdditionalPurposes` is not registered, so the CSMS is not told which 2.1 purposes this station supports (K21.FR.10) [MECHANICAL]

Found by CV12.2. The variable lists "the additional chargingProfilePurposes, that have been
introduced in OCPP 2.1, that are supported by the Charging Station", and K21.FR.10's note names it
as how a station reports support for `UsePriorityCharging`. It occurs zero times in `src/`.

This station supports two of them: `PriorityCharging` (K21/K22, implemented) and - since CV17 -
`LocalGeneration` (K27). Both work, and neither is advertised, so a CSMS following the spec's own
discovery path concludes it may not send either. `Required? = no` in the appendix, so this is not a
conformance violation; it is the same shape as CV14's decorative variables with the sign reversed -
a capability the station has and does not claim, rather than one it claims and does not have.

### 2.15 ~~Medium~~ **Closed by CV14** · `CAPABILITY_GATED_VARIABLES` was never swept by CV2.1 — 19 of 26 writable rows accept a write and discard it (B05.FR.09) [MECHANICAL]

**Fixed.** `CapabilityGatedVariable::honoured` now records per row whether this build makes the
value mean anything, and `capability_gate_events` narrows an unhonoured row to `ReadOnly` before it
reaches the device model — the same lever `DeviceModel::register_defaults` pulls on the other table,
so both tables now answer B05.FR.09 the same way. 24 of the 26 writable rows refuse the write; the
two `ISO15118Ctrlr` rows, which `crate::authorization` genuinely reads, stay writable.

The count below held exactly, and the split between its two "not honoured" reasons is preserved in
the field's docs rather than flattened: the 19 are refused because nothing reads them, the five
`Merchant` instances because the terminal owns them and a poll would overwrite a CSMS write within
one sweep (CV1.2's `ClockCtrlr.DateTime` rule).

The field is filled in on all 71 rows, not just the 26 writable ones, matching what
`DefaultVariable::honoured` already records for its read-only rows. Doing that turned up a separate
finding, **CV19 — since closed**: `LocalAuthListCtrlr.Entries`,
`SmartChargingCtrlr.Entries[ChargingProfiles]` and `DisplayMessageCtrlr.DisplayMessages` were
registered at 0 and never updated, so a CSMS asking how full the station is got 0 whatever was
installed. `LocalAuthListCtrlr.Entries` additionally carried a comment claiming
`ChargePointState::apply`'s `LocalListUpdated` arm kept it in step; that arm does not touch the
device model. `sync_inventory_counters` now re-derives all three per applied event, and all three
rows are `honoured: true`. Original finding follows.

CV2.1 added `DefaultVariable::honoured` and swept `DEFAULT_VARIABLES` (`src/state/device_model.rs`).
`CapabilityGatedVariable` (`src/device_model.rs:36`) has no such field, so the same question was
never asked of the second table — and 26 of its 72 rows are registered `ReadWrite`.

Re-running CV2.1's own detector against it, with the stricter rule CV2.1 adopted (a doc comment or a
test asserting the variable is *registered* does not count as a use):

| Verdict | Rows |
|---|---|
| **Decorative** — writable, read nowhere | **19** |
| Station-written — the terminal drives them, so a CSMS write is accepted and then overwritten | 5 (`PaymentCtrlr.Merchant[Id\|TaxId\|Name\|Address\|City]`) |
| Genuinely honoured | 2 (`ISO15118Ctrlr.ContractValidationOffline`, `.CentralContractValidationAllowed`) |

The 19: `SmartChargingCtrlr.LimitChangeSignificance`, `DisplayMessageCtrlr.Language`,
`TariffCostCtrlr.{Currency,TariffFallbackMessage×2,TotalCostFallbackMessage×2}`,
`PaymentCtrlr.{Enabled,AuthorizeDirectPayment,AuthorizationAmount,PaymentDetails,SettlementByCSMS,ReceiptServerUrl,ReceiptByCSMS}`,
`V2XChargingCtrlr.Enabled`, `WebPaymentsCtrlr.{URLTemplate,TOTPVersion,Length,ValidityTime}`.

`SmartChargingCtrlr.LimitChangeSignificance` is the one the appendix marks `Required? = yes`, and
the one with a behavioural consequence beyond the false `Accepted`: a CSMS setting it to 20% is told
the threshold took, and the station goes on reporting every change. Three of the `TariffCostCtrlr`
rows are the same ones CV8 recorded as unhonoured — this finding is about the *refusal*, not the
honouring.

Two rows also caught CV2.1's original blind spot exactly: `LimitChangeSignificance` and `Language`
each appear outside their registration site only inside a test asserting they are registered.

### 2.16 ~~Medium~~ **Closed by CV17 — and it was a behaviour finding, not a reporting one** · `LocalGeneration` is collapsed into `ExternalConstraints` and cannot round-trip (K27.FR.02/.03/.05) [READ]

**Fixed, and the finding below understates what was wrong.** This was filed as a round-trip
problem. It was also the composite limit: `ExternalConstraints` *caps* the result, while §K.3.6 has
local generation **added on top of** it, so a station handed K27's own example — 2 kW of local
generation under a 5 kW `TxDefaultProfile` — computed 2 kW where the spec computes 7 kW. Safe (it
under-draws) but wrong, and invisible without the spec text in hand.

`ChargingProfilePurpose::LocalGeneration` now exists internally; composition gained a third rule
(`adds_to_the_result`) applied after the caps; `map_purpose`'s `_` arm is gone in favour of
exhaustive arms, so the next purpose 2.1 adds is a compile error rather than a silent collapse; and
2.0.1/1.6J project it onto their external-constraints equivalent with the sign-inversion stated in
each adapter's docs. `ExternalChargingLimit` carries `is_local_generation`, held in a slot of its
own per scope so a constraint and locally generated capacity can be in force together (K27.FR.05),
and `isLocalGeneration` is stated on the 2.1 wire in both directions (K27.FR.03).

Two remainders were filed as CV20 and are **now closed too**. K27.FR.02's report needed no
"positive, CSMS-addressable ids" after all — `ChargingProfileType.id` is documented as "Id can have
a negative value. This is useful to distinguish charging profiles from an external actor (external
constraints) from charging profiles received from CSMS", so the negative range CV13 already chose
is the spec's own convention and the ids go out unchanged. `GetChargingProfiles` now reports the
external limits in force, carrying the external system's `chargingLimitSource`. K10.FR.04/.08/.09
are closed by excluding both purposes from `ChargingProfileCriteria::matches`. Original finding
follows.

`smart_charging::ocpp_2_1::map_purpose` maps 2.1's `LocalGeneration` purpose onto this crate's
`ChargingProfilePurpose::ExternalConstraints` through a `_` catch-all arm, and `wire_purpose` maps
that back to `ChargingStationExternalConstraints`. There is no internal purpose for local generation,
so:

- **K27.FR.02** — `GetChargingProfiles` cannot report the profile with
  `chargingProfilePurpose = LocalGeneration`; it comes back as external constraints.
- **K27.FR.03/.05** — `NotifyChargingLimit` must carry `isLocalGeneration = true` and must group the
  two purposes separately. `wire_charging_limit` hardcodes `is_local_generation: None`, and
  `ExternalChargingLimit` has no field that could carry it.
- A `ClearChargingProfile` filtered on either purpose necessarily hits both.

The `_` catch-all is also the mechanism: it silently absorbs any purpose 2.1 adds later, which is the
failure mode `CLAUDE.md`'s "exhaustive matches with no wildcard arm" rule exists to prevent.

### 2.17 ~~Medium~~ **Closed by CV18** · no `triggerReason = ChargingRateChanged` (K11.FR.04, K13.FR.03) [MECHANICAL]

**Fixed.** `TransactionUpdateReason::ChargingRateChanged` exists and maps to `TriggerReasonEnum::
ChargingRateChanged` on both 2.x wires. It is emitted on all three of the requirements'
preconditions together: an external control system caused the change, the composed rate actually
moved, and a transaction is ongoing.

The narrowness the finding below predicted held. `ConnectorEvent::CurrentLimitComputed` gained an
`externally_caused` flag, set by the projection — which is the only place that can tell the two
apart, since by the time a limit reaches the state machine it is a number and nothing about the
number says where it came from. The projection compares the external limits in force on an EVSE
against the ones the previous evaluation saw, which is exactly K11.FR.01 (set) and K13.FR.01
(released), the two events the requirements hang off. A schedule period boundary inside a CSMS
profile moves the rate without moving those, and stays unreported per K01.FR.61's `MAY`.

Not persisted: a rate change moves `seq_no` and nothing else recoverable, and an energy manager may
move a limit every few minutes, so writing one would be flash wear for a fresher sequence number.
Original finding follows.

`ChargingRateChanged` occurs nowhere in `src/`, and `TransactionUpdateReason` has only
`ChargingStateChanged` and `MeterValuePeriodic`.

The distinction matters and was checked rather than assumed: **K01.FR.61 makes this a `MAY`** for a
rate change the CSMS itself caused, so not sending it there is conformant. **K11.FR.04 and K13.FR.03
make it a `SHALL`** when an external control system changed the rate. Those two are unmet — and now
*reachable*: CV13 closed §2.13, so an external limit does change the rate, which is what makes this
the next thing in the K block worth doing (CV18).

### 2.18 Medium · ISO 15118 renegotiation has no hardware surface at all (K16, K17 — 33 FRs) [READ]

`hardware::Iso15118Controller` has exactly one method, `deliver_certificate_response`, and
`renegotiat` matches nothing anywhere in `src/`. K16.FR.02 is a `SHALL` on the Charging Station
whenever the composite schedule changes ("initiate schedule renegotiation with EV"), and K16.FR.03
requires handing the composite schedule to the EV.

This is the *same shape* of blocker as DER actuation in §2.12, and belongs beside it: not "the
integrator must supply a stack" but "there is no trait for the integrator to implement". A product
with a complete HLC stack still could not satisfy K16/K17 through this crate today. `docs/
CERTIFICATION.md` §3 item 2 names the ISO 15118 dependency but describes it as an integrator-stack
gap; it is also a missing surface here.

---

## 3. Requirement-level sweeps (workstream CV12)

Absence of findings below still means *not checked at requirement level*, not *conformant*. What
changed on 2026-08-12 is that the **K block's sweep is done** and the rest have had a mechanism-level
pass that says which use cases are worth a requirement-level read at all.

### 3.1 K — Smart charging: swept

The headline is that **317 was a misleading number**, and the residual risk in K is much smaller than
it looked — but concentrated. Classifying K11–K29 by who the requirement actually addresses:

| Class | Use cases | FRs | What it means |
|---|---|---|---|
| **Not addressed to a Charging Station** | K14, K24, K25, K26 | 6 | Every requirement names the **Local Controller**. K24/K25/K26 carry no requirements of their own — the spec says each is "already covered in use case K14". Nothing to implement in charge-point firmware. |
| **Deferred to another use case** | K23 | 0 | "Already covered in use cases K11 and K12" — so §2.13 is K23's finding too. |
| **Blocked on a hardware surface that does not exist** | K16, K17, K18, K19, K20 | 78 | ISO 15118 renegotiation and 15118-20 scheduled/dynamic control modes. See §2.18: not merely "needs an integrator's HLC stack" but *no trait to implement*. |
| **Station-side, verified this sweep** | K11, K12, K13, K21, K22, K27 | 36 | Findings §2.13, §2.16, §2.17. K21.FR.01–.04 check out against `handle_use_priority_charging`: `NoProfile` when no `PriorityCharging` profile applies to the transaction's EVSE or the charge point, `Rejected` on an unknown transaction, and `applying_to` covering "EVSE #0 or the EVSE of the transaction" exactly. Both reason codes are optional in the spec and are not sent. K21's remaining six FRs and FR.04's *application* (which rests on `PriorityCharging` outranking `Tx` in purpose precedence) were not read. K22's local trigger is reachable because `handle_use_priority_charging` is public, so a button binding can drive it — K22.FR.01's "notify user it is not capable" is a `hardware::Display` concern a product owns. |
| **Station-side, still unverified** | K28, K29 | 23 | Dynamic charging profiles from the CSMS and from an external system. `ChargingProfileKind::Dynamic` and `dynUpdateInterval` are present and referenced ~150 times, so this is a read of behaviour rather than a search for a mechanism. **The one piece of K left.** |

Two conclusions worth carrying into H3.1's OCTT planning: **Smart Charging as a certifiable profile
rests on K01–K13 and K21–K29**, of which only K28/K29 are now unread — and the 78 FRs of 15118-20
control modes are unreachable for any product until a renegotiation surface exists, whatever stack
the integrator brings.

### 3.2 E, C, N, Q — mechanism-level pass, requirement-level reads outstanding

Each named use case was checked for whether its mechanism exists at all. That is weaker than a
requirement read and is labelled `[MECHANICAL]` where the answer was a count, but it is what decides
where a requirement read is worth the time.

| Block | Use case | Mechanism | Verdict |
|---|---|---|---|
| **E** | E11 connection loss | offline queue, transaction continues | present; requirement read outstanding |
| | E13 message not accepted | `TransactionEventResponse` handling | present; requirement read outstanding |
| | E14 check transaction status | `handle_get_transaction_status` (48 refs) | present; requirement read outstanding |
| | **E16 fixed cost/energy/SoC/time** | `TransactionLimit`, `triggerReason = LimitSet` | **present since CV15** — cost, energy and SoC enforced; `maxTime` declared unsupported (CV21). See §2.14. |
| | E17 resume after forced reboot | power-cut recovery, swept by `tests/power_cut_recovery.rs` | present, and the strongest-evidenced row here |
| **C** | **C16 master pass** | `MasterPassGroupId` | **absent** — 0 occurrences, as first recorded in §2.2's era and re-confirmed |
| | **C17 prepaid card** | rests entirely on E16's `maxCost` | unblocked by CV15 — `maxCost` records, confirms and binds; C17's own requirements not yet swept |
| | C19–C23 cancellation/settlement | `NotifySettlement`, `SettlementStatus` (57 refs) | present; requirement read outstanding |
| | C24 stand-alone terminal | `PaymentTerminal` + CV2.11's `status()` | present since CV2.11; requirement read outstanding |
| | **C25 ad hoc payment via QR code** | `WebPaymentsCtrlr`, `QRCode` | **absent** — 0 occurrences of `QRCode`; all five `WebPaymentsCtrlr` variables are decorative (§2.15). 27 FRs. |
| **N** | N09/N10 customer information | `CustomerInformation` (177 refs) | present; requirement read outstanding |
| | N11 frequent periodic monitoring | `variable_monitoring` engine | present; requirement read outstanding |
| | N12–N15 periodic event streams | `PeriodicEventStream` (307 refs) | present; requirement read outstanding |
| **Q** | Q01 V2X authorization | authorization path | present; requirement read outstanding |
| | Q02–Q08 V2X control | DER/V2X actuation | **blocked on the same missing trait as §2.12's DER row.** `V2XChargingCtrlr.Enabled` is decorative (§2.15), so a station cannot even be told to enable it. 50 FRs. |

| Block | FRs | Depth |
|---|---|---|
| I, L, M, O, P, G, H, J, S | 349 combined | `[SURVEY]` only — untouched by this sweep. |

---

## 4. Suggested order of work

1. **§2.1 + §2.2 (variables)** — largest OCTT surface per unit of effort, and §2.2 is mostly
   *deleting* a false claim (reject what you can't honour) or wiring a read that already has a
   consumer.
2. **§2.3 (`SetVariables` validation)** — self-contained; needs `min_limit`/`max_limit`/`values_list`
   populated on the defaults, then one validation function.
3. **§2.4 + §2.5 (boot gating and status resync)** — both are small state-machine reads on
   `ChargePointState.registration`, plus one "report every connector" pass.
4. **§2.6 (remote-start metadata)** — plumbing `remoteStartId`/`reservationId`/`offline`/
   `triggerReason` through `Transaction`. Mechanical, and unblocks E12 and H03.
5. **§2.7 (F02)** — genuine state-machine work: a pending-remote-start state with a timeout.
6. **§2.8 (tariffs)** — the largest single item; needs a real pricing model before I07–I12 or a
   round-tripping `GetTariffs` are possible.
7. **§2.9–§2.11** — contained.
8. **§3** — requirement-level verification sweeps for the unverified blocks, K and E first.

Only after 1–7 is an OCTT run (H3.1) likely to be informative rather than a list of the above.

**Items 1–7 are now closed** (CV1–CV11), and item 8's K half is done. The order that follows from
the sweep, by certification impact rather than by size:

9. ~~**§2.13 (external limits are not enforced)**~~ — **closed by CV13.** It was the only finding
   here where the station told the CSMS something untrue about its own behaviour, and it was the
   prerequisite for §2.17, which is now reachable.
10. ~~**§2.15 (the second variable table)**~~ — **closed by CV14.** It was the same one-word-per-row
    change CV2.1 was, on the table CV2.1 never reached, and it turned up **CV19** (three station-owned
    counters registered at 0 that nothing ever updated), since closed.
11. ~~**§2.14 (E16 transaction limits)**~~ — **closed by CV15**, which unblocked C17 and gave CV8's
    locally computed cost a ceiling to act on. `maxTime` is declared unsupported rather than
    half-enforced; see CV21.
12. ~~**§2.16**~~ — **closed by CV17**, which turned out to be a behaviour fix: local generation
    adds capacity, and collapsing it onto `ExternalConstraints` had it capping instead. **§2.17**
    ~~**§2.17**~~ — **closed by CV18**, reachable only once §2.13 made an external limit change the
    rate at all.
13. **§2.18** — a `crate::hardware` addition, so it belongs with the DER actuation trait in a single
    considered break rather than on its own.
