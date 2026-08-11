# OCPP 2.1 compliance audit — spec vs. this crate

**Date:** 2026-08-11 · **Baseline:** `main` @ `4c6abe3` · **Spec:** OCPP 2.1 edition 2
(`docs/OCPP-2.1/`, part 2 specification + part 2 appendices v2.1 CSVs + errata 2026-06).

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

### 2.8 High · local cost calculation is absent (I01–I12) [READ]

`src/state/tariff.rs` deliberately models a tariff as `{id, currency, valid_from}` and drops the
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

### 2.10 Medium · security log entry on credential change (A01.FR.11) [READ]

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

---

## 3. Blocks surveyed but not requirement-verified

Absence of findings below means *not checked at requirement level*, not *conformant*. These carry the
largest residual risk:

| Block | FRs | Depth |
|---|---|---|
| K — Smart charging | 317 | `[SURVEY]` + composition engine `[READ]`. K11–K14 (external limits), K16–K20 (renegotiation, 15118-20 control modes), K21–K29 (priority, dynamic profiles, EMS topologies) unverified. |
| E — Transactions | 196 | `[READ]` for the specific findings above; E11, E13, E14, E16, E17 unverified at requirement level. |
| C — Authorization | 186 | `[READ]` for C07/C13–C15; C16 (master pass — `MasterPassGroupId` absent), C17, C19–C25 unverified. |
| N — Diagnostics/monitoring | 156 | `[SURVEY]`; the engine looks complete but N09–N15 unverified. |
| Q — V2X | 77 | `[SURVEY]` only. |
| I, L, M, O, P, G, H, J, S | 349 combined | `[SURVEY]` only. |

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
