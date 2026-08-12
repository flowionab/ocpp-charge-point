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
| **CV2.2** | `TxCtrlr.TxStartPoint` **and `TxStopPoint`** — both honoured via `ConnectorPolicy`; the stop point is read by `ends_transaction`. | E01, E02, E03 | **done** |
| **CV2.3** | `TxCtrlr.EVConnectionTimeOut` — a held authorization whose driver never plugs in is released by `run_pending_remote_start_timeouts`, wired into `setup()` on a 5s sweep. Covers **both** F02's held remote start and **E03.FR.15**'s locally presented card, which share one slot. | E03.FR.15, F02.FR.07/08 | **done** |
| **CV2.4** | `TxCtrlr.StopTxOnEVSideDisconnect` — the suspend-vs-stop branch on cable removal, via `ConnectorPolicy` — **and `OCPPCommCtrlr.UnlockOnEVSideDisconnect`**, the unlock half, via the `StoppingLocked` state. | **E09 vs E10**, E09.FR.02/.03 | **done** |
| **CV2.5** | `TxCtrlr.StopTxOnInvalidId`, `MaxEnergyOnInvalidId` — `ConnectorEvent::AuthorizationRevoked` either stops at once or grants the configured last allowance, ending with `stoppedReason = DeAuthorized`. Raised by `deliver_transaction_event` from a rejected `idTokenInfo` on a live session. | E05 | **done** |
| **CV2.6** | Measurand configuration — `SampledDataCtrlr.Tx{Started,Updated,Ended}Measurands`, `AlignedDataCtrlr.Measurands`. Largest row in CV2. The 2.x adapters filter sampled values by the configured list, re-read per message. The four `*Interval` variables stay refused (see below). | J01, J02, J03, F01.FR.14/15 | **done** |
| **CV2.7** | `ChargingStation.MinimumStatusDuration` — status debouncing, via `MinimumStatusDurationNotifier` (`src/availability.rs`). A status the connector has already left is dropped without waiting, so a bouncing connector costs one window rather than one per bounce; `0` short-circuits the path entirely. | G01 | **done** |
| **CV2.8** | `DeviceDataCtrlr` / `LocalAuthListCtrlr` `ItemsPerMessage`/`BytesPerMessage` — `GetVariables`, `SetVariables`, `GetReport`, `SendLocalList` and `SetVariableMonitoring` refuse an oversized request with `OccurrenceConstraintViolation` (items) or `FormatViolation` (bytes), naming the variable and both numbers (`src/message_limits.rs`). | B05, B06.FR.16/17, B07, B08, D01 | **done** |
| **CV2.9** | `AuthCtrlr.OfflineTxForUnknownIdEnabled` — an identifier neither the list nor the cache knows is now accepted offline when the operator opted in, and refused otherwise. Does not override a known rejection. | C15 | **done** |
| **CV2.10** | `SecurityCtrlr.OrganizationName` → the CSR's `O=` RDN, via `CsrSubject::with_organization_name_from_device_model`. A caller-supplied name always wins — the variable is CSMS-writable, so letting it override would let a remote peer redirect which organization the next certificate is issued under. | A02, A03, A00.FR.509 | **done** |
| **CV2.11** | `PaymentCtrlr` live status variables — a default-implemented `PaymentTerminal::status()` lets an integrator with a real terminal drive `Connected`/ICCID/IMSI/`Merchant`, and leaves a station without one reporting honestly. Mirrors how `ChargePoint::electrical()` landed in CV1.5. | C18–C24 | **done** |

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

**`TxStopPoint` is honoured too, and it is not the mirror image of `TxStartPoint`.** A start point
is a condition that *begins* to hold and OCPP starts once every configured one does — so a set
resolves to its *latest* member. A stop point is a condition that *ceases* to hold, and a
transaction cannot outlive the first of its conditions to lapse — so a set resolves to its
**earliest**. `TxStopPoint::from_member_list` therefore takes the minimum where its start-point
counterpart takes the maximum, and `ends_transaction` applies it.

One combination OCPP itself warns about is reachable: `EVConnected` on a station with
`UnlockOnEVSideDisconnect = false` leaves the transaction open forever, because the cable is
deliberately never released. OCPP puts responsibility for sensible start/stop combinations on the
CSMS, and this crate does not second-guess a configuration it was told to honour.

### CV2.3 — where the timing lives, and why not on the request

The obvious design is a timestamp on `PendingRemoteStart`. That needs a clock inside
`handle_request_start_transaction`, which is reached from every protocol version's inbound adapter —
so it would push a `MonotonicClock` through three adapter constructions and their traits to serve one
field. The sweep keeps its own map of when it first saw each held request instead: no signature
changes, and the loop is the only thing that needs the answer because it is the only thing that acts
on it.

The cost is stated rather than hidden: a request is released between `EVConnectionTimeOut` and that
plus one sweep interval. `setup()` sweeps every 5s against a 120s default.

**E03.FR.15 needed no second mechanism.** A card presented on a connector with no cable is held in
the same `pending_remote_starts` slot a `RequestStartTransaction` is, so one sweep covers both: a
driver who authorizes and walks away is deauthorized rather than leaving a live authorization for
whoever plugs in next.

What the sweep must *not* touch is a hold on a connector whose cable is already in — CV7's
authorize-first remote start waits there on the CSMS, not on a driver. `held_starts_awaiting_a_cable`
draws that line; the variable times how long to wait for the EV to connect, and the EV has.

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

**The trigger is `deliver_transaction_event`.** A CSMS blocklist update lands mid-session as a
rejected `idTokenInfo` on the response to an ordinary `Updated` event, and that raises
`ConnectorEvent::AuthorizationRevoked` — so E05 works from the CSMS's own refusal, not only from an
integrator raising the event themselves. Raised at most once per session, because the state machine
ignores a second revocation on a connector that has already left `Charging`.

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

### CV2.6 — what closed it

- `build_meter_values`/`sampled_values` take a `MeasurandSet` in both 2.x adapters (1.6J is out of
  scope — its `MeterValuesSampledData` is a different mechanism).
- **The notifiers hold the actor**, so the config is re-read per message and a CSMS changing a
  measurand list takes effect on the next event. This was the *breaking* part: `with_clock` and its
  three siblings are public constructors and each gained a `ChargePointActor` parameter.
  `connect_and_setup` builds them from the actor, so the default path filters; `setup()` called
  directly with a bare client cannot (one object for 45 traits has no device model to consult), and
  says so in its docs.
- An empty set produces **no `MeterValue` at all**, not a `MeterValue` carrying an empty list of
  readings — the latter says nothing and still costs a wire round trip.
- **The four `*Interval` variables stay refused**, and that is the one part of the row deliberately
  left open: they are a different concern (how *often* to sample, not *what*), and nothing drives
  them yet.

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
Requirements: F01.FR.19/.25, F02.FR.01/.06/.21. Status: **done**.

`remoteStartId` and `reservationId` are now carried on `Transaction` and quoted on every event it
produces, and `triggerReason` derives `RemoteStart` from the presence of a `remoteStartId` — which
only `handle_request_start_transaction` ever sets, so it is the discriminator rather than a second
flag that could disagree with it.

`reservationId` needed a new piece of state. A reservation ends the moment it is honoured (the cable
arriving is the reservation doing its job), so by the time the transaction starts there is nothing
left to read. `EvseState::honoured_reservations` bridges that gap — the id outlives the reservation
until the connector returns to `Available`, without keeping a spent reservation in the store where
it would look reservable.

**`offline` (CV6.1) closed by changing the forwarding contract, not by populating a field.** OCPP's
`offline` flag means the event was generated while the CSMS was unreachable, but the wire message is
built at *send* time — inside the queue's flush — so the encoder could not see whether the event it
was encoding had been queued. `OfflineQueue::marking_offline` is the answer: the queue stamps every
message it has had to hold, `ChargePointBuilder` wires that stamp to
`TransactionEventOccurred::offline`, and the encoder reads a field that is true by the time it gets
there. E12 is unblocked.

## CV7 — `RequestStartTransaction` acceptance rules and F02

Closes audit §2.7. Status: **done**.

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

### CV7 — the authorize-or-not branch (F01.FR.01/.02)

`AuthCtrlr.AuthorizeRemoteStart` is now honoured through `ConnectorPolicy`. Off (the default) is
F01.FR.02: the CSMS's own request stands as the decision and it re-checks the identifier when the
`TransactionEvent` reaches it. On is F01.FR.01: the connector goes to `Authorizing` instead of
`Starting`, takes byte-for-byte the path a presented card takes, and the contactor waits — which is
what "behave as if in response to a local action" means.

`Central` and `NoAuthorization` identifiers are exempt however the variable is set
(`IdTokenKind::requires_authorization`), because a `Central` token *is* the CSMS's own decision and
`NoAuthorization` names a bay that authorizes nobody.

**The `remoteStartId` had to be made to survive the round trip.** The transaction does not exist
until the decision comes back, so the id is held in the connector's pending slot and read at
transaction start — the same gap `honoured_reservations` bridges for a reservation id. It is dropped
on refusal, or it would attach itself to whatever the next driver starts on that connector. A new
`AcceptedPendingAuthorization` outcome distinguishes this from `AcceptedPendingCable`; both project
onto OCPP's `Accepted`.

**`AuthCtrlr.DisableRemoteAuthorization` is a different question and gets its own path.** Despite
the name it has nothing to do with remote *starts*: it forbids issuing `AuthorizeRequest`s at all,
leaving the local authorization list and the authorization cache as the only sources. It is
deliberately not implemented as "pretend to be offline" — two of `offline_decision`'s three switches
describe outage behaviour and would be wrong here, so `local_only_decision` is separate.
`LocalAuthorizeOffline` does not gate it (the link is not down); `AuthCacheCtrlr.Enabled` does (a
disabled cache is not a source); `OfflineTxForUnknownIdEnabled` does not (there is no outage to
strand anyone in). A Plug & Charge presentation is refused rather than answered locally, for the
same reason the offline contract path refuses.

### CV7 — F01.FR.22's group half, and the bug underneath it

`Reservation` now carries `group_id_token`, from `ReserveNowRequest.groupIdToken` (2.x) or
`parentIdTag` (1.6J — the same field under an older name). A start is refused only when *neither*
the identifier nor the group matches. Absence is not a wildcard in either direction: two
reservations naming no group are not thereby in the same group. 1.6J's `RemoteStartTransaction`
carries no parent tag, so on that version an idToken mismatch still refuses.

**Implementing it surfaced two real defects, both older than this row:**

- **The FR.21/.22 identity check was unreachable.** `can_start_here` excluded `ConnectorState::
  Reserved` by state before ever comparing identifiers, so a reservation refused a remote start from
  the very driver it was made for — the one thing a reservation exists to enable. `Reserved` is now
  an admissible state subject to the match, and a bay this driver reserved is preferred over a
  merely free one so the reservation is consumed rather than stranded.
- **Any event landing on a reserved connector wiped the reservation record.** `*slot =
  reservation_made` ran on every event that left the connector `Reserved`, and only
  `ConnectorEvent::Reserved` carries one — so an idle connector's next meter sample left the bay
  reserved with nothing saying for whom. Now only an event that actually carries a reservation
  records one.

## CV8 — Tariff pricing model and local cost calculation

Closes audit §2.8, and the largest single item in this roadmap. `state::Tariff` must carry the priced
structure (energy / charging-time / idle-time / fixed-fee components and their conditions) before
either of these is possible:

- `GetTariffs` round-tripping what `SetDefaultTariff` installed (I09).
- Any running cost derived locally (I07–I12), which in turn is what I01–I06 display.

Sequenced last among the behavioural items because nothing else depends on it and it is the only one
that needs a new domain model rather than a new read of an existing one. Status: **done** — model and engine in `42ccfdb`, wiring in `c837fa1`.

`state::Tariff` now carries the priced structure, so `SetDefaultTariff` → `GetTariffs` round-trips
(I09), and `crate::pricing` turns a tariff plus meter readings into totals, usage and the
`CostDetailsType` breakdown. Fixed-point throughout, truncating toward zero so an ambiguous
fraction never overcharges; the three genuinely ambiguous readings in the spec are resolved and
marked in the module docs.

**Wired.** `cost_details` and `transactionInfo.tariffId` are populated, and `EvseState::running_cost`
holds the station's own figure alongside the CSMS-told `running_costs`. `effective_tariff` resolves
the driver tariff first and the store's default otherwise, re-resolved per event rather than pinned
at transaction start — one mechanism covering I07, I08 and I11, including a scheduled default whose
`validFrom` falls mid-session.

Four scope cuts, each documented where the code makes them: reservation-time dimensions are never
charged (nothing tracks how long a reservation preceded the transaction that honoured it); a tariff
arriving mid-session prices only what follows, there being no meter history to back-price against;
power/current cost dimensions are unreported, having no verified wire example for the unit
conversion; and clearing the only tariff pricing a transaction freezes its cost rather than
continuing.

`TariffCostCtrlr.Currency` and the two fallback messages remain unhonoured — this work gave them no
consumer. They feed I04/I05, which is what a driver *display* shows, and this crate renders nothing
itself.

## CV9–CV11 — contained items

| ID | Item | Requirements | Status |
|---|---|---|---|
| **CV9** | `SignCertificate` retry discipline: resend after `CertSigningWaitMinimum`, doubling the back-off, stopping at `CertSigningRepeatTimes` until a `TriggerMessage` restarts it. Register both variables. `MaxCertificateChainSize` (a MAY) alongside. | A02.FR.17–.19, A03.FR.17–.19 | **done** |
| **CV10** | **done** — make `NetworkConfiguration.BasicAuthPassword` writable — apply a new password to the connection and reconnect (A01.FR.02) — and log the change in the security log without disclosing the value. CV1.3 registered the variable and refuses the write; this is what makes the write real. | A01.FR.02, A01.FR.04, A01.FR.11/.12 | **done** |
| **CV11** | A lock-failure signal distinct from a generic fault: `ConnectorEvent::LockFailed` takes the identical fail-safe path a fault does (G05.FR.01 is precisely "SHALL NOT start charging"), and what makes it distinguishable to the CSMS is the connector's `ConnectorPlugRetentionLock`/`Problem` variable and the hard-wired `NotifyEvent` that go with it (G05.FR.02) — not a different connector state, there being no safer state than the one a fault already produces. | G05 | **done** |

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
Status: **in progress** — K swept (2026-08-12), E/C/N/Q given a mechanism-level pass.

| ID | Sweep | Status |
|---|---|---|
| **CV12.1** | **K — smart charging.** Swept K11–K29 requirement by requirement. Six findings, audit §2.13/§2.16/§2.17/§2.18. | **done** |
| **CV12.2** | **K28/K29 (23 FRs), plus K21's last six.** Dynamic charging profiles from the CSMS and from an external system; the mechanism exists (`ChargingProfileKind::Dynamic`, `dynUpdateInterval`, ~150 references), so this is a behaviour read rather than a search. The K21 remainder includes FR.04's *application* — whether `PriorityCharging` actually displaces an active `Tx` profile in composition, which the sweep checked only at the response level. | open |
| **CV12.3** | **E** (196) — E11, E13, E14, E17 at requirement level. E16 needs no sweep: audit §2.14 found it absent outright. | open |
| **CV12.4** | **C** (186) — C19–C24 at requirement level. C16, C17 and C25 need no sweep: absent or blocked, per audit §3.2. | open |
| **CV12.5** | **N** (156) — N09–N15. Every mechanism is present; this is a read for conformance, not for existence. | open |
| **CV12.6** | **Q** (77) — Q01 only. Q02–Q08 (50 FRs) are blocked on the DER/V2X actuation trait and are not worth reading until it exists. | open |
| **CV12.7** | **I, L, M, O, P, G, H, J, S** (349 combined) — untouched. | open |

### CV12.1 — what the K sweep actually found, and why 317 was the wrong number

The block's headline count made it the largest risk in this document. Classifying every use case by
*who the requirement addresses* shrinks it sharply, and moves what is left into two piles:

- **6 FRs are not addressed to a Charging Station at all.** K14 names the Local Controller
  throughout, and K24/K25/K26 carry no requirements of their own — the spec says each is "already
  covered in use case K14". K23 likewise defers to K11/K12. Four of the nine use cases in the
  "EMS topologies" band are therefore nothing this crate can implement or fail.
- **78 FRs (K16–K20) are unreachable by anyone**, not merely unimplemented here — see CV16.

What remains is 59 station-side FRs, of which 36 were reached this sweep and 23 (K28/K29) are
CV12.2. K21 came out clean as far as it was read — FR.01–.04 match `handle_use_priority_charging`
response for response, including the "EVSE #0 or the EVSE of the transaction" scoping that
`applying_to` gets right — and K22's local trigger is reachable because that function is public, so
an integrator's button binding can drive it. K21's remaining six FRs are not read; they are folded
into CV12.2 rather than counted as clean.

**The one that mattered is CV13** — the only finding in the sweep where the charge point told the
CSMS something untrue about its own behaviour. It is now closed.

## CV13–CV18 — what the K sweep opened

| ID | Item | Requirements | Status |
|---|---|---|---|
| **CV13** | **Enforce an external charging limit, don't just report it.** `external_charging_limits` turns the recorded limits — the station-wide one and the EVSE's own — into `ExternalConstraints` capping profiles, and `composing_profiles` joins them onto `charging_profiles.applying_to()` at both composition sites: the projection that drives hardware, and `GetCompositeSchedule`, so the CSMS is shown the curve the station will apply. Audit §2.13. | K11.FR.01, K12.FR.01, K13.FR.01, K27.FR.01 | **done** |
| **CV14** | **Sweep `CAPABILITY_GATED_VARIABLES` the way CV2.1 swept `DEFAULT_VARIABLES`.** `CapabilityGatedVariable::honoured` now records, per row, whether this build makes the value mean anything, and `false` forces the registration to `ReadOnly` — the same lever `register_defaults` pulls. 24 of the 26 writable rows refuse the write; the two `ISO15118Ctrlr` rows stay writable. Audit §2.15. | B05.FR.09 | **done** |
| **CV19** | **Three station-owned counters registered at 0 and never updated** — `LocalAuthListCtrlr.Entries`, `SmartChargingCtrlr.Entries[ChargingProfiles]`, `DisplayMessageCtrlr.DisplayMessages`. `ChargePointState::sync_inventory_counters` now re-derives all three per applied event, the way the availability and network-configuration variables already were. Found by CV14's sweep. | D01, K01, B6 | **done** |
| **CV15** | **Transaction limits (E16).** `state::TransactionLimit` is a real internal type on `Transaction`, set by the CSMS off a `TransactionEventResponse` or locally on the driver's behalf, confirmed once with `triggerReason = LimitSet`, and enforced: reaching a ceiling commands 0 A, moves the connector to `SuspendedEVSE` and reports the trigger reason naming *which* ceiling. Cost, energy and state of charge are enforced; `maxTime` is declared unsupported rather than half-done (see CV21). Audit §2.14. | E16.FR.01/.02/.03/.04/.05/.10/.13/.14/.15/.16/.17, C17 | **done** |
| **CV21** | **`maxTime` transaction limits.** The one E16 ceiling CV15 left out, and the only one that cannot be decided from a meter reading: E16.FR.09 measures it from the transaction's start, and the state machine is clock-free by design. Needs a clock-driven sweep of the kind `run_pending_remote_start_timeouts` already is, plus the transaction start times such a sweep would have to keep (as `ChargingLimitProjection` already does for `Relative` profiles). `TxCtrlr.SupportedLimits` omits `maxTime` until then, so a CSMS is told not to send one and a station handed one anyway neither records nor confirms it (E16.FR.12/.13). | E16.FR.09, `maxTime` | open |
| **CV16** | **A renegotiation surface in `crate::hardware`.** K16.FR.02 is a `SHALL` on the station whenever the composite schedule changes, and `Iso15118Controller` has one method — a certificate hook. No integrator can satisfy K16–K20 through this crate today. Audit §2.18. | K16, K17 (33 FRs) | open |
| **CV17** | **`LocalGeneration` has its own internal purpose, and it *adds* rather than caps.** The `_` catch-all made it an `ExternalConstraints` cap, so 2 kW of sun under a 5 kW `TxDefaultProfile` charged at 2 kW instead of K27's own 7 kW — a behaviour bug, not the reporting one this row was filed as. `ExternalChargingLimit` carries `is_local_generation`, held in its own slot per scope so a constraint and capacity can both be in force (K27.FR.05), and `isLocalGeneration` is now stated on the wire in both directions. Audit §2.16. | K27.FR.01/.02/.03/.05, §K.3.6 | **done** |
| **CV20** | **Two K27/K10 remainders CV17 did not take.** (a) K27.FR.02 wants an EMS-pushed `LocalGeneration` schedule reported by `GetChargingProfiles`, but external limits are deliberately not stored as profiles and their synthetic ids are negative — reporting them needs a positive reserved id range first. (b) K10.FR.04/.08/.09 have `ClearChargingProfile` disregard `ExternalConstraints` **and** `LocalGeneration`; `ChargingProfileCriteria::matches` excludes neither, so a CSMS that installs one (K01.FR.06 says it shall not, but the station receives what it receives) can clear it. | K27.FR.02, K10.FR.04/.08/.09 | open |
| **CV18** | **`triggerReason = ChargingRateChanged`.** Sent when an external limit is set or released, the composed rate actually moves, and a transaction is running — all three preconditions the two requirements state. The CSMS-caused case stays unsent: K01.FR.61 makes it a `MAY`, and the CSMS installed the schedule whose boundaries it would be told about. `ConnectorEvent::CurrentLimitComputed` carries `externally_caused`, set by the projection, which is the only place that can see the difference. Audit §2.17. | K11.FR.04, K13.FR.03 | **done** |

**Sequencing:** CV13 before CV18 (nothing changes a rate externally until the limit is enforced),
and both are now done, as are CV14, CV15 and CV17. **What is left of this group is CV16 (a
`crate::hardware` addition that should be taken together with the DER actuation trait
`docs/CERTIFICATION.md` §3 names — one considered break rather than two), and the three rows the
finished work opened: CV20 and CV21** (CV19 is closed).

### CV15 — the three decisions, and the one limit it does not support

**E16.FR.06 is unreachable on this station, so FR.05 is the only branch.** The suspend-vs-end
choice turns on `TxCtrlr.TxStopPoint` containing `EnergyTransfer`, and `TX_START_STOP_POINTS`
deliberately narrows the variable's `values_list` to the three points this crate can observe —
`EnergyTransfer` needs a "current is actually flowing" signal no hardware binding provides, so a
`SetVariables` naming it is `Rejected` by CV3. A station can therefore never be configured into
FR.06's branch, and reaching a ceiling always suspends.

**Suspending means commanding zero, not recording a state.** `ChargingSuspendedByEvse` models
*hardware told us it paused* and issues no command; a transaction limit needs the station to stop
the energy itself. The 0 A command is issued from the state machine rather than from
`crate::smart_charging`'s projection, so a build without the smart-charging feature still enforces
the limit it accepted — and resuming withdraws the command (`None`) rather than naming a current,
because what the connector may draw is composition's answer, not this path's.

**The cost a `maxCost` is measured against is stated, not derived.** E16.FR.15/.16 split it: a
locally priced session uses the station's own running cost, and one the CSMS prices uses its
`totalCost`/`CostUpdated`. Deriving the first needs the tariff in force, and resolving *that* needs
a clock — so `ConnectorEvent::RunningCostAdvanced` now carries the total alongside the cost, and
`EvseState::running_cost_totals` holds it. The adapter that computes the cost already has the
tariff and the clock in hand; making it state the figure is cheaper and more honest than having
the state machine guess or skip the check.

**`maxTime` is not supported, and says so.** It is the one ceiling that cannot be decided from a
meter reading. Rather than half-enforce it, `TxCtrlr.SupportedLimits` names the three that are
enforced, and E16.FR.12/.13 do the rest — the same stance CV14 took, applied to a limit instead of
a variable. **CV21** is the row that would close it.

### CV17 — the row that was filed as reporting fidelity and turned out to be behaviour

The audit read `map_purpose`'s `_` arm as a round-trip problem: `LocalGeneration` arrives as
`ExternalConstraints`, so `GetChargingProfiles` cannot report it as itself and `NotifyChargingLimit`
cannot flag it. Both true. What neither the audit nor this row said is what that arm did to the
*limit the connector runs at*, because it takes the spec text to see it:

> If a charging profile of chargingProfilePurpose = LocalGeneration is active for the EVSE, then
> this capacity is **added on top of** the calculated composite schedule. — 2.1 Part 2 §K.3.6

`ExternalConstraints` caps. So a station handed K27's own example — 2 kW of local generation under a
5 kW `TxDefaultProfile` — charged at **2 kW instead of 7 kW**. It is the CV13 pattern (the station
does something other than what it tells the CSMS) with the sign flipped: it under-delivers rather
than over-draws, so it is safe, merely wrong, and invisible to anyone not reading §K.3.6.

Three decisions worth recording:

- **Composition gained a third rule.** A purpose now caps the result, competes for it by stack
  level, or adds to it, and `adds_to_the_result` names the third the way `caps_the_result` named
  the first. A test asserts every purpose is exactly one of the three, so a purpose added later
  cannot quietly become two.
- **Local generation is added *after* the caps.** §K.3.6 adds it to the composite, and K27's own
  diagram has a 7 kW grid connection precisely because the 2 kW never crosses it. Adding before
  the cap would have the installation limit clip away capacity that does not come through it.
- **`ExternalChargingLimit` got a second slot rather than a flag.** K27.FR.05 describes a station
  holding a constraint and local generation at once, from the same source — one slot would have had
  the second evict the first, and `ExternalChargingLimitCleared` would have had no way to say which
  of the two went away. Hence `is_local_generation` on the clear event as well, even though OCPP's
  own `ClearedChargingLimit` has no such field.

**Two K27/K10 remainders are deliberately not in CV17** — see CV20. Neither is a regression: both
were true before this row and are true after it.

### CV14 — what the sweep found, and the two rows it did not expect

The audit's arithmetic held: 26 writable rows, 19 decorative, 5 station-written, 2 honoured. The
five station-written rows are `PaymentCtrlr.Merchant[Id|TaxId|Name|Address|City]`, and they are
refused for a different reason than the other 19 — not "nothing reads it" but "the terminal owns
it": `apply_payment_terminal_status` fills all five when the terminal registers and again on every
`payment_status_updates` sweep, so an accepted CSMS write would be silently replaced minutes later.
That is `ClockCtrlr.DateTime`'s situation, and CV1.2's answer applies unchanged.

The field is set on all 71 rows rather than only the writable ones, matching what
`DefaultVariable::honoured` already records for its own read-only rows: `true` where the build
enforces the value or keeps it in step with the fact it reports, `false` where it is a placeholder
that makes the component complete. Filling it in is what turned up **CV19** — and one of those
three counters was worse than stale, because `LocalAuthListCtrlr.Entries` carried a comment
claiming `ChargePointState::apply`'s `LocalListUpdated` arm kept it current. Nothing in that arm
touches the device model. The comment is now the truth, and **CV19 has since made the value one**:
all three counters are re-derived per applied event by `sync_inventory_counters`.

---

## Sequencing

```
CV1.1 ──┬─> CV5
        └─> (B07 SummaryInventory)
CV1.3 ────> CV10
CV2.1 ────> CV2.2 … CV2.11   (each flips one variable from refused to honoured)
CV2.3 ────> CV7
CV3, CV4, CV6, CV9, CV11      independent
CV8                            independent, largest
CV12                           continuous, in parallel
```

Everything above the line is done. **CV1–CV11 are closed.** What is open is CV12's remaining sweeps
and four of the six rows its first sweep opened, plus the one CV14 added on its way past:

```
CV12.1 (K, done) ──┬─> CV13 (done) ──> CV18 (done)
                   ├─> CV17 (done) ──> CV20   the K27/K10 remainders it did not take
                   ├─> CV14 (done) ──> CV19 (done)
                   ├─> CV15 (done) ──> CV21   the maxTime ceiling it declined to half-do
                   └─> CV16        a crate::hardware break — take with the DER trait
CV12.2 … CV12.7                    the rest of the sweeps, continuous
```

An OCTT run (`docs/PRODUCTION-ROADMAP.md` H3.1) is worth doing once CV1–CV7 are closed. Before that
it would mostly restate this document.

**CV1–CV7 are now closed**, so that run is the next thing this workstream is waiting on rather than
a future one — and the K sweep sharpened what it should take first. **CV13 was the one finding that
should not wait for it** — a station that reports an external charging limit it never applies gives
the CSMS's load calculation a reduction that did not happen, which is worse than not supporting
external limits at all — and it is now done, which removes the one blocker in front of that run.
The rest of the sweep's findings are either done (CV14), bounded and
well-understood (CV15, CV17, CV18), or a hardware-surface decision (CV16) that belongs with the two
`crate::hardware` additions `docs/CERTIFICATION.md` §3 already names as outright blockers — a DER
actuation surface, and the payment terminal work CV2.11 half-answers. CV16 makes that list three.

None of CV13–CV18 blocks Core, Reservation or Local Authorization List Management. **CV13, CV17 and
CV18 do touch Smart Charging**, which `docs/CERTIFICATION.md` §4 puts in the first group of claims
to pursue. CV13 and CV17 are both closed, and CV17 turned out to matter more than this paragraph
used to say: it was filed as reporting fidelity and was in fact the composite limit being computed
wrongly whenever local generation was in play (see its section above). What is left against that
claim is CV18 (a trigger reason) and CV20's two remainders — reporting fidelity in the way CV17 was
only believed to be.

One caveat on the four `*Interval` variables under CV2.6: they remain refused, so a station is
conformant in *what* it samples but not yet configurable in *how often*. That is a known, declared
refusal rather than a silent one, which is what B05.FR.09 asks for.
