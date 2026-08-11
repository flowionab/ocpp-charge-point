# OCPP 2.1 compliance roadmap (workstream CV)

The execution half of `docs/OCPP-2.1-COMPLIANCE-AUDIT.md`. Every item below closes a numbered
finding in that document; the audit says *what is wrong*, this says *what to build and how we know
it is done*. Sibling to `docs/PRODUCTION-ROADMAP.md`, which tracks productionisation rather than
conformance — where the two overlap (B5.x, B7.x) the row here cites it.

**Where the seams are.** Two pieces of groundwork the later rows build on already exist:
`ConnectorPolicy` (`src/state/connector_state.rs`) makes a connector transition a pure function of
`(state, event, policy)`, with the device-model read hoisted into one place — CV2.2 and CV2.5 extend
it rather than reaching into the device model from the state machine. And `DefaultVariable::honoured`
(CV2.1) means flipping a variable from refused to live is a one-word change plus the read that
justifies it.

**Ordering principle:** test surface per unit of effort, then unblocking. CV1 and CV2 come first not
because they are the most interesting but because a compliance test tool exercises the device model
before it exercises anything else, and half of CV2's rows are prerequisites for CV5–CV7.

Status values: **done** · **in progress** · **open**.

---

## CV1 — Device-model completeness

**Closed.** All **122 of 122** required variables are now registered — re-verified against
`dm_components_vars.csv` on this baseline. Closes audit §2.1. Acceptance for the workstream as a
whole: a test asserting that every `Required? = yes` row of
`docs/OCPP-2.1/Appendices_CSV_v2.1/dm_components_vars.csv` is registered on a charge point built
with all capabilities on. Because that CSV is gitignored, the test carries the expected list inline
and cites the CSV — the same stance `CAPABILITY_GATES` already takes.

| ID | Item | Variables | Status |
|---|---|---|---|
| **CV1.1** | **Availability variables** — `AvailabilityState` per charge point / EVSE / connector, kept in step with `LifecycleState`, `EvseStatus` and `ConnectorState`; `Available` (spec meaning: *the component exists*) registered `true` per topology. | `ChargingStation.{AvailabilityState,Available}`, `EVSE.{AvailabilityState,Available}`, `Connector.{AvailabilityState,Available}` | **done** |
| **CV1.2** | `ClockCtrlr.DateTime` — the station's current notion of time, refreshed on every clock sync. Registered `ReadOnly`, not writable: this crate's clock *is* the last `currentTime` the CSMS sent (`TimeSource` = `Heartbeat`), so a CSMS write would be overwritten by the next heartbeat — B05.FR.09 says reject what cannot be honoured. | `ClockCtrlr.DateTime` | **done** |
| **CV1.3** | `NetworkConfiguration` mirror — every occupied configuration slot becomes a component instance carrying all nine required variables, re-derived per applied event and removed when the slot is vacated. `BasicAuthPassword` is registered `WriteOnly` (so `GetVariables` can never disclose it, A01.FR.12) but the write is refused: applying a new password needs reconnect plumbing, which moved to CV10. | 9 variables | **done** |
| **CV1.4** | `MonitoringCtrlr` message-size variables, behind `variable-monitoring`. Declared bounds, not yet enforced — enforcing them is CV2.8. | `ItemsPerMessage[SetVariableMonitoring]`, `BytesPerMessage[SetVariableMonitoring]` | **done** |
| **CV1.5** | Hardware-fact variables — `hardware::ElectricalCharacteristics`, returned by a new **default-implemented** `ChargePoint::electrical()` so adding it breaks no integrator. Registered always, valued only where declared. | `*.SupplyPhases`, `Connector.ConnectorType`, `EVSE.Power` (+ its required `maxLimit` characteristic) | **done** |
| **CV1.6** | Capability-gated blocks — four new `CAPABILITY_GATES` rows (`ac_der`, `dc_der`, `web_payments`, `v2x_charging`) give the components somewhere to hang, then 27 variables behind them. | `V2XChargingCtrlr` (3), `WebPaymentsCtrlr` (5), `ACDERCtrlr` (1), `DCDERCtrlr` (16), `TariffCostCtrlr` fallback messages (2) | **done** |

**CV1.1 also unblocks:** B07's `SummaryInventory` base, which `src/reporting.rs` already looks up by
name and which returned structurally empty before this landed; and CV5, which needs a single place
that knows every connector's current availability.

### CV1.5/CV1.6 — the two decisions worth recording

**`ChargePoint::electrical()` has a default implementation.** `capabilities()` was added as a
*required* method and the trait's docs still carry the breaking-change note; this one returns an
all-unknown declaration instead, so a station that ignores it still registers the required variables
with nothing in them — which is what OCPP asks for and is honest about what the firmware was told.
`SupplyPhases` is an enum rather than an integer because OCPP encodes **DC as `0`**, which reads as
an error to anyone who has not read the appendix.

**`EVSE.DischargePower` is not in the gated table, and could not be.** That table registers
charge-point-wide components only (`evse: None` is hardcoded, and gating keys on a row's
`ctrlr_component`), while `DischargePower` is per-EVSE. It lives in CV1.5's electrical path instead,
guarded on `Capabilities::supports_bidirectional_power` — which matches the appendix marking it
`Required? = V2X`: required of a station that can discharge, absent from one that cannot.
Registering it unconditionally would advertise hardware that is not there.

**The four new gate rows add no `Capabilities` field.** `ac_der`/`dc_der` key on `der_control`,
`web_payments` on `payment`, `v2x_charging` on `supports_bidirectional_power`. None of these is a
separate thing an integrator declares — a station doing DER control has DER hardware — they exist
only to give OCPP's required variables a component to hang on. Every value is empty or zero: these
are nameplate figures and deployment settings, and a plausible-looking invention would be worse than
an obviously-unset value a CSMS can see it must configure. `WebPaymentsCtrlr.SharedSecret` is
`WriteOnly`, for the same reason `NetworkConfiguration.BasicAuthPassword` is.

## CV2 — Honour it or reject it

Closes audit §2.2 (39 of 80 registered variables are decorative). The governing requirement is
**B05.FR.09**: a variable the station cannot honour must be `Rejected`, not silently accepted.

CV2.1 is deliberately first and deliberately unglamorous — it converts a *silent* lie into an
*explicit* refusal in one change, which is both correct under B05.FR.09 and a far better failure
mode for an integrator than a setting that appears to take. Every later row then flips one variable
from refused to honoured, by setting `honoured: true` and adding the read that justifies it.

**The audit's "39 decorative variables" was a lower bound, and CV2.1 measured the real number: 30 of
49 built-in defaults.** The audit's detector counted a doc comment or a test as a use; CV2.1's
analysis excluded both, which found `AuthCtrlr.AuthorizeRemoteStart`, `AuthCtrlr.LocalPreAuthorize`,
`OCPPCommCtrlr.ResetRetries` and `LocalAuthListCtrlr.Enabled` among others that the first pass had
cleared.

| ID | Item | Requirements | Status |
|---|---|---|---|
| **CV2.1** | `DefaultVariable::honoured` — the table now records *both* what OCPP says about writing a variable and whether this build acts on it; `false` forces the registration to `ReadOnly`, so the write is `Rejected`. 30 of 49 defaults were decorative; all now refuse. | B05.FR.09 | **done** |
| **CV2.2** | `TxCtrlr.TxStartPoint` — honoured via `ConnectorPolicy::tx_start_point`. **`TxStopPoint` is still refused** (see below). | E01, E02, E03 | **partial** |
| **CV2.3** | `TxCtrlr.EVConnectionTimeOut` — a held remote start whose driver never plugs in is released by `run_pending_remote_start_timeouts`, wired into `setup()` on a 5s sweep. **E03.FR.15** (the same timeout applied to a *locally* presented card that is never followed by a cable) is **not** covered. | F02.FR.07/08 | **partial** |
| **CV2.4** | `TxCtrlr.StopTxOnEVSideDisconnect` — the suspend-vs-stop branch on cable removal, via `ConnectorPolicy`. `UnlockOnEVSideDisconnect` (the unlock half) is **still open**. | **E09 vs E10** | **partial** |
| **CV2.5** | `TxCtrlr.StopTxOnInvalidId`, `MaxEnergyOnInvalidId` — a new `ConnectorEvent::AuthorizationRevoked` either stops at once or grants the configured last allowance, ending with `stoppedReason = DeAuthorized`. **The event has no producer yet** — see below. | E05 | **partial** |
| **CV2.6** | Measurand configuration — `SampledDataCtrlr.Tx{Started,Updated,Ended}Measurands` + intervals, `AlignedDataCtrlr.Measurands`/`TxEndedMeasurands`/`TxEndedInterval`. Largest row in CV2. **Design decided; groundwork landed, filter not yet applied — see below.** | J01, J02, J03, F01.FR.14/15 | **partial** |
| **CV2.7** | `ChargingStation.MinimumStatusDuration` — status debouncing. | G01 | open |
| **CV2.8** | `DeviceDataCtrlr` / `LocalAuthListCtrlr` `ItemsPerMessage`/`BytesPerMessage` — refuse oversized requests with `OccurrenceConstraintViolation`/`FormatViolation`. | B05, B06.FR.16/17, B07, B08, D01 | open |
| **CV2.9** | `AuthCtrlr.OfflineTxForUnknownIdEnabled` — an identifier neither the list nor the cache knows is now accepted offline when the operator opted in, and refused otherwise. Does not override a known rejection. | C15 | **done** |
| **CV2.10** | `SecurityCtrlr.OrganizationName` → the CSR's `O=` RDN, via `CsrSubject::with_organization_name_from_device_model`. A caller-supplied name always wins — the variable is CSMS-writable, so letting it override would let a remote peer redirect which organization the next certificate is issued under. | A02, A03, A00.FR.509 | **done** |
| **CV2.11** | `PaymentCtrlr` live status variables — needs a terminal-status hardware surface. Tracked in `docs/CERTIFICATION.md` §3 as a product-level blocker. | C18–C24 | open |

### CV2.2 — what landed, and the one decision worth knowing

`TxStartPoint` now decides which transition begins a transaction: `EVConnected` (the cable
latches, so the transaction covers a failed authorization too), `Authorized` (the default), or
`PowerPathClosed` (the contactor closes, so an authorized-but-never-energised session produces no
transaction at all). OCPP models it as a set that must *all* hold; the three points this crate can
observe are strictly ordered along a session, so a set resolves to its latest member — which is why
`TxStartPoint` is an ordered enum rather than a set of flags.

**The declared `values_list` is deliberately narrower than OCPP's enum.** `ParkingBayOccupancy`
needs a bay sensor with no binding here, `DataSigned` needs signed meter values this crate does not
produce, and `EnergyTransfer` needs a "current is actually flowing" signal distinct from the
contactor being closed. Declaring them would mean accepting a `SetVariables` and then starting
transactions at some *other* point — the silent lie B05.FR.09 forbids. Declaring only the three that
work makes CV3's validation reject the rest with a reason, and `values_list` is exactly where a
charge point is supposed to say what it accepts.

**`TxStopPoint` is still `honoured: false` and still refuses writes.** Stopping is not the mirror
image of starting: a stop point is a condition that *ceases* to hold, and this crate's stop path is
driven by an explicit `ChargingStopped` from the binding rather than by observing each condition
lapse. Doing it properly is its own row.

### CV2.3 — where the timing lives, and why not on the request

The obvious design is a timestamp on `PendingRemoteStart`. That needs a clock inside
`handle_request_start_transaction`, which is reached from every protocol version's inbound adapter —
so it would push a `MonotonicClock` through three adapter constructions and their traits to serve one
field. The sweep keeps its own map of when it first saw each held request instead: no signature
changes, and the loop is the only thing that needs the answer because it is the only thing that acts
on it.

The cost is stated rather than hidden: a request is released between `EVConnectionTimeOut` and that
plus one sweep interval. `setup()` sweeps every 5s against a 120s default.

**A real bug fell out of testing this.** Recording a held request produced `changed: false` — it
arrives on an idle connector and moves nothing — so the actor never published the new state and
*nothing downstream could see a request was being held*, the sweep included. `apply_connector_event`
now counts it as a change.

### CV2.5 — the allowance is an absolute target, and the event needs a producer

`Transaction::stop_at_energy_wh` stores the meter reading to stop **at**, not the Wh remaining. A
remaining-Wh counter would have to be decremented per sample, so a sample that is dropped,
duplicated or restored from persistence would silently change how much free energy the driver gets.
A target the meter has to reach is the same answer however the samples arrive.

`StopTxOnInvalidId: false` with no allowance configured stops immediately rather than charging
forever — there is nothing to grant.

**What is still missing is the trigger.** `ConnectorEvent::AuthorizationRevoked` is handled
correctly but nothing raises it: the crate does not yet inspect a `TransactionEventResponse`'s
`idTokenInfo` for a rejection on a session already underway. Until that lands, E05 works for an
integrator who raises the event themselves and not from the CSMS's own refusal. That is the next
row.

### CV2.6 — the two decisions, settled

Both were taken deliberately rather than defaulted into, because each has a wrong answer that only
shows up later.

**1. The config reaches the encoder through the notifier, which holds the actor.**
`Ocpp2_1TransactionEventNotifier` and its 2.0.1 sibling gain a `ChargePointActor` and read the
device model at send time, so a CSMS changing a measurand list takes effect on the next event — the
same contract every other live variable in this crate has. `build_meter_values` keeps taking the
filter as a parameter so it stays the pure function its docs promise.

*Rejected: filtering when the sample is stored.* It needs no plumbing, which is why it is tempting,
and it is wrong twice over. `SampledDataCtrlr` and `AlignedDataCtrlr` filter *different messages*
from the same stored sample, so one filter at the storage point cannot serve both — and discarding
measurands at storage would take the energy reading CV2.5's `stop_at_energy_wh` check depends on.

**2. The measurand defaults are registered non-empty, so an empty list honestly means empty.**
`DEFAULT_VARIABLES` declares what this firmware actually samples
(`Energy.Active.Import.Register`, plus `Power.Active.Import` where the crate produces it) rather
than an empty string. That keeps the literal spec reading — an empty `MemberList` selects nothing —
while an unconfigured station still reports.

*Rejected: treating empty as "send everything".* It is the easy upgrade path, but it makes the
variable lie in the one direction that matters: a CSMS that deliberately clears the list to stop
meter data would keep receiving it. Registering a real default costs one honest claim in the
defaults table about what this firmware samples, which is the same bar every other row in that
table is held to.

### CV2.6 — what has landed

- **`MeasurandSet`** (`crate::meter_values`), parsed from a `MemberList`, plus
  `transaction_event_measurands` / `aligned_measurands` which read the right variable for a message
  kind. Public and tested-by-construction; nothing calls them yet.
- **Real defaults on the measurand variables**, per the decision above, so an empty list can
  honestly mean empty. `SUPPORTED_MEASURANDS` narrows their declared `values_list` to the five this
  firmware produces, so a `SetVariables` naming any other measurand is now `Rejected` with a reason
  rather than accepted and never sampled — that part of CV2.6 is live today.

### Remaining work for CV2.6

- `build_meter_values`/`sampled_values` take a `MeasurandSet` in both 2.x adapters (1.6J is out of
  scope — its `MeterValuesSampledData` is a different mechanism).
- **The notifiers hold the actor.** This is the part that stalled and it is a *breaking* change, not
  a threading exercise: `Ocpp2_1MeterValuesNotifier::with_clock` and its three siblings are public
  constructors, so each gains a `ChargePointActor` parameter and every call site follows. Budget for
  the API break and a CHANGELOG entry.
- An empty set must produce **no `MeterValue` at all**, not a `MeterValue` carrying an empty list of
  readings — the latter says nothing and still costs a wire round trip.
- The four `*Interval` variables are a separate concern (how *often* to sample, not *what*) and stay
  refused under CV2.1 until something drives them.

## CV3 — `SetVariables` value validation

Closes audit §2.3. Two halves, and the second is the larger one:

1. A validation function consulting `VariableCharacteristics::{data_type, min_limit, max_limit,
   values_list}`, returning `Rejected` with a `statusInfo` per B05.FR.07 (badly formatted) and
   B05.FR.08 (out of range).
2. **Populating those characteristics.** Every default is registered today with all three set to
   `None`, so there is nothing to validate against until each variable declares its type bounds.

Acceptance: `SetVariables(OCPPCommCtrlr.HeartbeatInterval = "banana")` → `Rejected`; a boolean
variable rejects anything but `true`/`false`; an `OptionList` rejects a value outside its
`values_list`. Status: **done**.

Landed as `validate_value` (`src/device_model.rs`) plus a `VARIABLE_BOUNDS` table
(`src/state/device_model.rs`). The bar for a row in that table is deliberately **that OCPP states
the bound**, not that it seems sensible — an invented limit is a `Rejected` a CSMS could not have
predicted, and worse than no limit because `GetVariables` would then report the value it just
refused. Type checking (from `data_type`, which every default already declared) covers B05.FR.07
across the whole model immediately; range and membership cover the rows the table names.

## CV4 — Gate outbound traffic on boot acceptance

Closes audit §2.4. `ChargePointState.registration` is recorded but never read. Needs:

- A single predicate ("may this charge point send a CALL right now?") consulted by every offline
  queue flush path — the retry timer, the reconnect flush, and the forwarder.
- The `TriggerMessage`/`GetBaseReport`/`GetReport` exception from B02.FR.02.
- `RequestStartTransaction`/`RequestStopTransaction` answering `Rejected` while `Pending`
  (B02.FR.05).
- Total silence until the retry interval expires on `Rejected` (B03.FR.02).

Acceptance: a station rebooted with a non-empty offline queue that receives `Pending` sends nothing
but `BootNotification` until accepted. Status: **done**.

The gate is a property of the *queue* (`OfflineQueue::gated_on_registration`) rather than of each
flush call, because all four things that can start a flush — a new report, a reconnect, the retry
timer, and a direct `flush_offline_queue` — have to honour the same rule and only some of them hold
anything that could answer the question. Gating the **drain** rather than the **push** is what makes
it safe: reports still accumulate while the station waits, and go out in order once accepted.

Two things fell out of building it that were not in the original plan:

- **Waiting must not burn a message's attempt cap.** The gate is checked before
  `record_failed_attempt`, so a long `Pending` cannot silently eat the backlog it is protecting.
- **Acceptance has to kick a flush.** Holding the queue is only half the requirement; without
  `ChargePointBuilder::spawn_acceptance_flush`, a station that booted with a backlog would hold it
  until the *next* report, reconnect or retry sweep — indefinitely on a quiet station that
  registered no retry timer. Waiting to be allowed to send is correct; staying silent afterwards is
  not.

B02.FR.05 (`RequestStartTransaction`/`RequestStopTransaction` → `Rejected` while `Pending`) is in
`src/remote_control.rs`.

## CV5 — Connector-state resynchronisation

Closes audit §2.5. Depends on **CV1.1**.

- After a BootNotification is accepted, report every connector's current state (B01.FR.05).
- After an outage, report **all** connectors if the offline period exceeded
  `OCPPCommCtrlr.OfflineThreshold`, only the **changed** ones otherwise (B04.FR.01/02) — which means
  tracking when the connection dropped, not just that it came back.

2.1 prefers `NotifyEvent(AvailabilityState)` and deprecates `StatusNotification` for this, but still
permits it; emitting `NotifyEvent` is a follow-on, not part of the acceptance criterion. Status:
**done**.

Landed as `ChargePointBuilder::connector_status_resynchronisation`, wired into `setup()` after
`status_notifications` so a reconnect flushes the queued changes before any sweep can overtake them.

**B04.FR.02 needed no code, and that is the interesting part.** The connectors that changed while
offline are exactly what the offline queue is holding, so the reconnect flush already reports those
and only those. Sweeping unconditionally would report connectors that did *not* change — which is
what that requirement forbids. So the outage length is measured precisely in order to decide whether
to do nothing.

The outage is measured from `time_sync`'s anchor — the last BootNotification or Heartbeat response,
the only two messages carrying the CSMS's `currentTime` and so the only two moments this crate can
prove it was in contact. **Granularity is one heartbeat interval**, documented on
`crate::connection::outage_seconds` rather than hidden: an idle-but-online station last heard from
the CSMS up to `HeartbeatInterval` ago, so a real outage can read longer than it was. The
overestimate costs at most a redundant full sweep, which is the safe direction — re-reporting a
connector is merely redundant, failing to re-report one leaves the CSMS wrong. A station that has
never synced reads as `u64::MAX` and sweeps, same reasoning.

## CV6 — Remote-start metadata on `TransactionEvent`

Closes audit §2.6. Carry `remoteStartId`, `reservationId` and `offline` on `Transaction` and populate
them on the wire; derive `triggerReason = RemoteStart` for a remotely started transaction instead of
the hardcoded `Authorized`. Mechanical, and it unblocks E12 and the H03 reservation link.
Requirements: F01.FR.19/.25, F02.FR.01/.06/.21. Status: **done**, except `offline`.

`remoteStartId` and `reservationId` are now carried on `Transaction` and quoted on every event it
produces, and `triggerReason` derives `RemoteStart` from the presence of a `remoteStartId` — which
only `handle_request_start_transaction` ever sets, so it is the discriminator rather than a second
flag that could disagree with it.

`reservationId` needed a new piece of state. A reservation ends the moment it is honoured (the cable
arriving is the reservation doing its job), so by the time the transaction starts there is nothing
left to read. `EvseState::honoured_reservations` bridges that gap — the id outlives the reservation
until the connector returns to `Available`, without keeping a spent reservation in the store where
it would look reservable.

**`offline` is not done and is now CV6.1.** OCPP's `offline` flag means the event was generated
while the CSMS was unreachable, but the wire message is built at *send* time — inside the queue's
flush — so the encoder cannot see whether the event it is encoding was queued. Wiring it needs the
queue to tell the encoder, which is a change to the forwarding contract rather than a field to
populate. E12 stays open on it.

## CV7 — `RequestStartTransaction` acceptance rules and F02

Closes audit §2.7. Status: **mostly done** — F02 works; the timeout does not.

**F02 now works at all, which it did not before.** A `RequestStartTransaction` for a connector with
no cable is accepted and held (`EvseState::pending_remote_starts`); when the driver plugs in and the
cable latches, the held request is dispatched through the *same* path a local start takes, so the
transaction gets identical bookkeeping, hardware commands and status effects — including the
`remoteStartId` from the original request (CV6). A new `AcceptedPendingCable` outcome distinguishes
"accepted, no transaction id yet" from "accepted, here it is"; both project onto OCPP's `Accepted`.

The rejection conditions F01.FR.21–.24 name are now checked explicitly rather than falling out of
"no connector happened to be latched": EVSE unavailable or faulted, connector mid-session, an
already-authorized transaction, or a reservation held for a different identifier. A latched
connector is preferred over an idle one, so the common case still starts immediately.

**Still open, and split out rather than glossed:**

- **F01.FR.22's group-id half** — this crate's `Reservation` models an `idToken` only, so a
  `groupIdToken` match cannot be distinguished. An idToken mismatch already refuses, which is the
  conservative side of that rule.
- **`AuthorizeRemoteStart` / `DisableRemoteAuthorization`** (F01.FR.01/.02) — the authorize-or-not
  branch is untouched.

## CV8 — Tariff pricing model and local cost calculation

Closes audit §2.8, and the largest single item in this roadmap. `state::Tariff` must carry the priced
structure (energy / charging-time / idle-time / fixed-fee components and their conditions) before
either of these is possible:

- `GetTariffs` round-tripping what `SetDefaultTariff` installed (I09).
- Any running cost derived locally (I07–I12), which in turn is what I01–I06 display.

Sequenced last among the behavioural items because nothing else depends on it and it is the only one
that needs a new domain model rather than a new read of an existing one. Status: **open**.

## CV9–CV11 — contained items

| ID | Item | Requirements | Status |
|---|---|---|---|
| **CV9** | `SignCertificate` retry discipline: resend after `CertSigningWaitMinimum`, doubling the back-off, stopping at `CertSigningRepeatTimes` until a `TriggerMessage` restarts it. Register both variables. `MaxCertificateChainSize` (a MAY) alongside. | A02.FR.17–.19, A03.FR.17–.19 | open |
| **CV10** | Make `NetworkConfiguration.BasicAuthPassword` writable — apply a new password to the connection and reconnect (A01.FR.02) — and log the change in the security log without disclosing the value. CV1.3 registered the variable and refuses the write; this is what makes the write real. | A01.FR.02, A01.FR.11/.12 | open |
| **CV11** | A lock-failure signal distinct from a generic fault. | G05 | open |

## CV12 — Requirement-level sweeps for the unverified blocks

Closes audit §3, and the largest *risk* in the roadmap even though it builds nothing: 1085 of the
1257 station-side requirements were surveyed rather than verified. Order by requirement count and by
how much of the block is already claimed:

1. **K** (317) — the composition engine is real, but K11–K14, K16–K20 and K21–K29 are unverified.
2. **E** (196) — E11, E13, E14, E16, E17.
3. **C** (186) — C16 (`MasterPassGroupId` is absent entirely), C17, C19–C25.
4. **N** (156) — N09–N15.
5. **Q** (77), then the remaining blocks.

Each sweep produces findings in the audit document's format, and any new gap gets a CV row here.
Status: **open**.

---

## Sequencing

```
CV1.1 ──┬─> CV5
        └─> (B07 SummaryInventory)
CV1.3 ────> CV10
CV2.1 ────> CV2.2 … CV2.10   (each flips one variable from refused to honoured)
CV2.3 ────> CV7
CV3, CV4, CV6, CV9, CV11      independent
CV8                            independent, largest
CV12                           continuous, in parallel
```

An OCTT run (`docs/PRODUCTION-ROADMAP.md` H3.1) is worth doing once CV1–CV7 are closed. Before that
it would mostly restate this document.
