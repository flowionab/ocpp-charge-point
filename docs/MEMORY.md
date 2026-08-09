# Memory budget

How much RAM and flash a charge point built on this crate needs, so an integrator
can size the part before committing to hardware. RAM is **G2.3** and flash is
**G2.4** in [`PRODUCTION-ROADMAP.md`](PRODUCTION-ROADMAP.md) §9.2; both are
measured, neither is estimated.

- [RAM](#ram) — worst-case retained heap per configuration
- [Flash](#flash) — image size per Cargo feature set

## RAM

Every RAM number here comes from
[`tests/memory_budget.rs`](../tests/memory_budget.rs), which installs a counting
`GlobalAlloc` and reads live requested bytes around each structure it builds.
Regenerate them with

```sh
cargo test --test memory_budget -- --nocapture
```

The test also asserts a ceiling per configuration, so a change that meaningfully
grows retained state fails `cargo test` rather than being discovered on a device.
Raise a ceiling only together with the table below.

### What is counted

Counted: bytes this crate requests from the allocator and has not freed while the
state is alive — the `Vec`/`VecDeque`/`BTreeMap`/`String` backing storage that
scales with the configured bounds.

**Not** counted, and to be budgeted separately:

| Not counted | Whose it is |
| --- | --- |
| Allocator bookkeeping: chunk headers, size-class rounding, fragmentation | your allocator — add headroom, 10–20% is typical |
| Task stacks | your `Executor` — one stack per spawned functional block |
| Transport buffers, TLS session state | `ocpp-client` and the transport it dials |
| Transient per-message (de)serialization | peaks with the largest single message, not with retained state; bounding it is roadmap F5.2 |
| Flash/persistence buffers | your `Storage` implementation |

The figures are therefore a **floor for the application layer**, and the right
input to a RAM budget rather than the whole budget.

### Per configuration

Measured on a 64-bit host. See [32-bit targets](#32-bit-targets) below — a
32-bit MCU holds *less*, so these numbers are a safe upper bound.

Each row is filled to its configured worst case: the local authorization list
full of 36-character id tokens (OCPP 2.x's maximum), the device model full at its
configured maximum, every connector holding both an active transaction and a
reservation, all three offline queues full, the durable security log full, and
the charging profile store full.

| | Tight AC wallbox | Crate defaults | DC site |
| --- | --- | --- | --- |
| Topology | 1 EVSE × 1 connector | 1 EVSE × 2 connectors | 4 EVSEs × 2 connectors |
| `max_local_authorization_list_entries` | 25 | 100 | 500 |
| `max_device_model_variables` | 64 | 256 | 512 |
| Offline queue capacity (each of 3) | 25 | 100 | 200 |
| Security log capacity | 25 | 50 | 200 |
| `max_charging_profiles` | 16 | 16 | 16 |
| Empty state (incl. built-in device model) | 27.5 KB | 27.8 KB | 29.9 KB |
| Local authorization list, full | 3.1 KB | 10.9 KB | 52.5 KB |
| Device model, full | 22.4 KB | 95.8 KB | 190.7 KB |
| Busy connectors (transaction + reservation each) | 0.1 KB | 0.3 KB | 1.0 KB |
| Charging profiles, full (8 periods each) | 4.7 KB | 4.7 KB | 4.7 KB |
| Status queue, full | 0.8 KB | 3.1 KB | 6.1 KB |
| Transaction queue, full | 6.0 KB | 23.8 KB | 47.6 KB |
| Security queue, full | 5.1 KB | 20.5 KB | 41.1 KB |
| Security log, full | 5.6 KB | 11.3 KB | 45.2 KB |
| **Total retained** | **63.0 KB** | **183.5 KB** | **404.2 KB** |

Read that as: the crate's own defaults need roughly **184 KB of heap** in the
worst case, and a deliberately tightened single-connector wallbox fits in
roughly **63 KB**. Neither figure includes the exclusions above.

The empty-state floor went from ~5 KB to ~28 KB as the crate started registering
OCPP's standard variables by default — B1.6's 1.6J required configuration keys,
B1.7's 2.x required variables, then A5's reconnect-backoff trio: 48 in total. That is the
device model's per-variable cost below in action, and it is the price of
protocol compliance rather than of topology: note how little the floor moves
between a 1-connector and an 8-connector charge point.

The *totals* barely moved, which is worth understanding rather than glossing
over: the device model is filled to `max_device_model_variables` either way, so
built-in defaults displace filler rather than adding to it. What actually
changed is the split — a charge point now spends more of its device-model budget
on variables OCPP requires and less on whatever the hardware binding registers.
A binding with many variables of its own should raise the bound accordingly.

The charging profile store is the one row that does not scale with the
configuration: `max_charging_profiles` defaults to 16 whatever the topology, so a
big site pays the same ~5 KB a wallbox does. Raise it on a site whose CSMS
actually drives per-connector schedules — at ~296 B per profile (eight schedule
periods each), even quadrupling it costs under 15 KB.

The security log is the one row an integrator sizes on a different axis from the
rest: it retains history whether or not those events ever reached the CSMS, so
its bound answers "how much history is worth keeping" rather than "how long an
outage must be absorbed" — see
[`SecurityEventLog`](../src/security.rs). A charge point with no compliance
reason to keep a long trail can cut it well below the default 50.

#### Per-unit costs, for sizing your own configuration

Derived from the same measurements, so you can price a bound before setting it:

| Unit | Cost | Notes |
| --- | --- | --- |
| Local authorization list entry | ~110 B | with a 36-character id token; scales down with shorter tokens |
| Device model variable | ~375 B | when clustered 8 to a component — see below, this one varies a lot |
| Active transaction | ~64 B | its id token's `String` allocation; the rest is inline in the already-allocated vector |
| Reservation | ~64 B | same |
| Queued status notification | ~31 B | no owned strings — just the deque slot |
| Queued transaction event | ~240 B | id token plus the deque slot |
| Queued security event | ~205 B | with `techInfo` text; less without |
| Security log entry | ~226 B | queued security event plus a recorded-at timestamp |
| Charging profile | ~296 B | with 8 schedule periods; a period is ~24 B of that |

An offline queue's `VecDeque` grows by doubling, so a queue configured with
capacity 100 ends up with 128 slots allocated. Round a configured capacity up to
the next power of two when budgeting. The security log is a `VecDeque` too and
behaves the same way.

### The device model dominates — and its shape matters

The device model is the largest single consumer in every configuration, and its
cost per variable depends heavily on how variables are distributed across
components. The model is a `BTreeMap<Component, BTreeMap<Variable,
VariableDefinition>>`, and a `BTreeMap` node is allocated at its full branching
factor no matter how few entries it holds — so a component holding one variable
pays for a whole node.

256 variables, clustered N to a component:

| Variables per component | Total | Per variable |
| --- | --- | --- |
| 1 | 535.2 KB | 2090 B |
| 4 | 158.0 KB | 617 B |
| 8 | 95.8 KB | 374 B |
| 16 | 123.1 KB | 480 B |

**A 5.6× spread, from nothing but how the same variables are grouped.** OCPP's
own model clusters variables onto standardized `*Ctrlr` components
(`OCPPCommCtrlr`, `SampledDataCtrlr`, `AuthCtrlr`, …), which is both spec-correct
and the cheap shape here — so a hardware binding that follows OCPP's naming gets
the good case for free. A binding that invents one component per sensor pays up
to 2 KB per variable and will blow a budget sized from the table above.

Two consequences worth stating plainly:

- Register related variables on a **shared** component. This is the single
  highest-leverage memory decision a hardware binding makes.
- The ~28 KB "empty state" floor is almost entirely the built-in device model's
  48 default variables (OCPP's standard configuration keys), spread across
  nine `*Ctrlr` components. It is the price of answering OCPP's required
  configuration keys, not of the charge point's topology — note how little the
  floor moves between a 1-connector and an 8-connector configuration.

### 32-bit targets

`size_of` for the types that dominate, measured on the host and on
`thumbv7em-none-eabihf`:

| Type | 64-bit host | thumbv7em (32-bit) |
| --- | --- | --- |
| `ChargePointState` | 176 B | 120 B |
| `EvseState` | 104 B | 52 B |
| `DeviceModel` | 32 B | 16 B |
| `Component` | 72 B | 36 B |
| `Variable` | 48 B | 24 B |
| `VariableDefinition` | 112 B | 80 B |
| `Transaction` | 112 B | 96 B |
| `Reservation` | 56 B | 40 B |
| `LocalListEntry` | 40 B | 20 B |
| `ConnectorStatusChanged` | 24 B | 12 B |
| `TransactionEventOccurred` | 136 B | 112 B |
| `SecurityEvent` | 48 B | 24 B |

Heap use scales with these, since what is allocated is `n × size_of::<T>()` for
the vectors and deques, and node-sized multiples of key+value for the maps. Every
type is smaller on a 32-bit target — between 0.5× and 0.85× — so **the 64-bit
table above is a conservative upper bound for an MCU**, which is the direction
you want a budget to be wrong in. If you need a tighter figure than "the 64-bit
number, minus up to a third", measure on your own target: the harness is portable
apart from its `std`-only counting allocator, which a bare-metal build can
replace with its own.

The string payloads (id tokens, component/variable names, `techInfo`) are
target-independent — a 36-byte token is 36 bytes everywhere.

---

## Flash

Measured by [`scripts/flash-cost.sh`](../scripts/flash-cost.sh), which builds
[`tools/flash-probe`](../tools/flash-probe) — a real bare-metal firmware image
that exercises the enabled features — for `thumbv7em-none-eabihf` with
`opt-level="z"`, fat LTO and `--gc-sections`, then reports the size of the
flashable image (`objcopy -O binary`, i.e. the bytes you program onto the part):

```sh
scripts/flash-cost.sh          # the whole table
scripts/flash-cost.sh --quick  # core + everything, what CI runs
```

| Feature set | Flash | vs core |
| --- | --- | --- |
| Core, no protocol version | 32 KB | — |
| Core + OCPP 1.6J | 174 KB | +141 KB |
| Core + OCPP 2.0.1 | 224 KB | +191 KB |
| Core + OCPP 2.1 | 310 KB | +277 KB |
| Core + all three versions | 474 KB | +441 KB |
| Core + 2.1 + `reservation` | 320 KB | +10 KB over 2.1 |
| Core + 2.1 + `local-auth-list` | 322 KB | +12 KB over 2.1 |
| Core + 2.1 + `tariff-cost` | 315 KB | +5 KB over 2.1 |
| Core + 2.1 + the 11 declared-capability features | 311 KB | +1 KB over 2.1 |
| Everything | 523 KB | +490 KB |

"Core" is the version-independent charge point: the state machine, the actor, the
hardware binding and dispatch, and every functional block that isn't
feature-gated. **32 KB** — that part is small.

### What this tells you

**The protocol version is the decision that matters.** A 2.1-only charge point is
310 KB; supporting all three versions is 474 KB, so the second and third version
cost **+164 KB** on top of 2.1 alone (less than the 609 KB the three would sum to,
because they share this crate's internal model and much of `ocpp-types`). On a
512 KB part, all-three-versions plus TLS plus your own application is a tight fit;
a single negotiated version is the lever to pull first, and it's a one-line
feature change.

The per-version figure is the *whole* wire stack — this crate's adapters plus the
`ocpp-client` code and `ocpp-types`/`serde` monomorphizations they pull in —
because that is what a charge point must flash to speak the version at all. Don't
read it as "this crate's adapter code is 277 KB"; the split isn't cleanly
attributable, since the codecs are only reachable *because* the adapters name
those message types. `cargo bloat` on the probe is the tool if you want to see
inside the number.

**The functional-block features are cheap: 5–12 KB each.** Reservation, local
authorization list and tariff/cost are genuinely feature-gated, so a charge point
that doesn't need one doesn't flash it.

**The other 11 capability features currently cost ~1 KB in total, because they
gate almost no code yet.** `smart-charging`, `firmware-management`, `diagnostics`,
`variable-monitoring`, `display-message`, `payment`, `iso15118`, `der-control`,
`battery-swap`, `periodic-event-stream` and `certificates` exist today as
capability *declarations* — they appear in the capability table and gate nothing
else, because the functional blocks behind them aren't implemented (roadmap
workstream B). The honest reading of that row is not "these features are free" but
"there is nothing behind them yet"; expect each to grow a real number as its block
lands, which is exactly what this table is for.

### What the flash figures exclude

Everything an integrator brings: a real transport (the probe's never sends) and
its TLS stack, a real async executor (embassy or otherwise), a real heap
allocator, the reset vector and startup code (`cortex-m-rt`), and a panic handler
that does something. Add those to the numbers above, don't read them as a whole
firmware.

They also assume the release profile the probe uses — `opt-level = "z"`, `lto =
true`, `codegen-units = 1`, `panic = "abort"`, `strip = true`. A debug build, thin
LTO, or `panic = "unwind"` is a different and much larger number.

---

## Reconnect-churn growth (H4.3, root-causing H4.1's open finding)

[H4.1](PRODUCTION-ROADMAP.md#104-h4--longevity)'s long soak
(`tests/network_flapping_soak.rs`) recorded a small, consistent memory-growth
trend at 400+ reconnect rounds — a few hundred bytes per reconnect, well inside
its 4 MB tolerance but not root-caused, with three named candidates: this
crate's `ConnectionTarget`, `ocpp-client`'s reconnect path, or benign allocator
fragmentation from thousands of short-lived TCP connections.

**Reproduced at larger scale.** A 500-round run of the same soak (release
profile) produced 9,611 reconnects and 2,500 transactions: retained memory read
2,660,711 B after 250 rounds and 4,701,966 B after 500 — roughly 425 B per
reconnect over the second half, still comfortably inside the 4 MB tolerance.

**Isolated to reconnects, not transaction volume.** `tests/memory_growth.rs`
(H4.2) already drives 3,000 transactions with zero reconnects and finds ~0 net
growth (97 B of noise). `tests/reconnect_leak_bisect.rs` adds the missing data
point directly: `bare_reconnect_churn_grows_without_any_transaction_traffic`
runs this crate's full `connect_and_setup`/`ConnectionTarget` stack against a
mock CSMS that answers exactly one `BootNotification` per connection and then
closes the socket — no `StatusNotification`, no `TransactionEvent`, nothing for
any offline queue or the security log to touch. Result: 600 → 1,201 reconnects
grew retained memory by 60,931 B, ~101 B/reconnect, with literally zero
transaction traffic. Transaction/offline-queue volume is therefore not a
contributing factor — the growth axis is reconnect count.

**Ruled out analytically: benign allocator fragmentation.** Both this file's
counting `GlobalAlloc` and H4.1's own measure *live requested-minus-freed
bytes* — bytes whose `alloc` has been observed but whose matching `dealloc`
has not. Fragmentation (unusable gaps between still-allocated blocks) does not
change this number: a fragmented allocator can waste address space without a
single byte counted here failing to be freed. A monotonic climb in this
specific metric can only mean a genuine allocation that is never freed, so
"benign fragmentation" is not an available explanation for what was actually
observed, independent of its magnitude.

**Isolated to `ocpp-client`, not this crate's `ConnectionTarget`.**
`tests/reconnect_leak_bisect.rs`'s second test,
`bare_ocpp_client_reconnect_churn_with_no_charge_point_crate_involved`, removes
this crate entirely: it drives `ocpp_client::connect_2_1` directly with a
`Reconnector` that calls `ocpp_client::websocket_transport` straight (no
`ConnectionTarget`, no `SizeLimitedStream`), and sends `BootNotification` by
hand after each reconnect signal — no `ocpp_charge_point` code runs at all
beyond that one call. Result: 600 → 1,200 reconnects grew retained memory by
105,612 B, ~176 B/reconnect — the same phenomenon, at least as large, with
none of this crate's reconnect-adjacent code in the loop. Reading
`ConnectionTarget`'s own state (`src/network_switch.rs`) confirms why: it is a
handful of fixed-size fields (`String`s replaced in place, a `u64` counter, an
`Option<ChargePointActor>`) with no collection that grows with dial count.

**Conclusion — and how far it can actually be trusted.** Growth scaling with reconnect count is
real and has been observed independently, but **the magnitude does not reproduce reliably, and
the figure above should not be quoted as settled.** Three measurements of the same test on the
same commit:

| Run | Result |
|-----|--------|
| H4.3's own run | ~176 B/reconnect |
| Independent verification, 4 runs at 40 / 1,200 / 1,200 / 3,000 rounds | **0 B/reconnect, every time** |
| A third run, 600 → 1,200 reconnects | 125.3 B/reconnect |

Two of three runs show linear growth of the same order; one careful re-run at four different
scales shows none at all. That spread is the finding. It means the effect is real but
**conditional on something not yet identified** — machine load, allocator behaviour under
concurrency, or timing of the redial loop are all live candidates — and that any single number
here is an artefact of one run.

What *is* established, and holds regardless:

- **Not fragmentation.** `live()` counts allocated-minus-freed bytes, so fragmentation cannot
  produce a climb in this metric (see above).
- **Not this crate's `ConnectionTarget`.** It holds a handful of fixed-size fields with no
  collection that grows with dial count, and the bare-`ocpp-client` reproduction excludes it
  entirely.
- **Not `ocpp-client`'s obvious per-reconnect state.** `pending_responses` is removed on both the
  success and timeout paths, `pong_waiters` is cleared on every reconnect, and both broadcast
  registries are subscribed once per client lifetime — all confirmed by inspection.

The remaining uninstrumented candidate is `tokio-tungstenite`'s per-handshake allocation.

**Next step — reproduce it reliably before doing anything else.** An earlier revision of this
section recommended filing an upstream bug against `ocpp-client` 0.5.0 with this test as a
"minimal reproduction". That recommendation is withdrawn: a reproduction that yields zero on
four consecutive independent runs is not one, and filing it would cost an upstream maintainer
real time chasing a result they cannot see. Establish what makes the growth appear and disappear
first; only then is there something worth reporting.

Note also that `bare_reconnect_churn_grows_without_any_transaction_traffic` originally *asserted*
it would reach its full reconnect target inside a 300 s deadline, and hard-failed after ten
minutes on a loaded machine — discarding a perfectly well-defined bytes-per-reconnect ratio
because the count fell short. It now measures the ratio over whatever reconnects it achieves and
only requires enough of them to be meaningful.

## D2.3 — 2.1 `ChargingProfile`'s by-value size, re-measured

The roadmap row and `docs/UPSTREAM-GAPS.md`'s D2.3 note were last measured against `ocpp-types`
0.2.0. Re-measured directly against the pinned **0.3.0** (`cargo test --lib size_measurements --
--nocapture`, `src/state/charging_profile.rs`):

| Type (this crate's actual binding, `CustomDataType = CustomData`) | 0.3.0 size |
|---|---|
| 2.1 `ChargingProfile` | **50,584 bytes** |
| 2.1 `ChargingSchedule` | 16,616 bytes |
| 2.1 `AbsolutePriceSchedule` | 15,064 bytes |
| 2.1 `PriceLevelSchedule` | 376 bytes |
| 2.1 `SalesTariff` | 368 bytes |
| 2.0.1 `ChargingProfile` | 2,616 bytes |
| 2.0.1 `ChargingSchedule` | 736 bytes |
| 1.6J `ChargingProfile` | 176 bytes |
| 1.6J `ChargingSchedule` | 88 bytes |
| This crate's internal `ChargingProfile` (`state::charging_profile`) | 96 bytes |
| This crate's internal `ChargingSchedule` | 72 bytes |

**The headline number has not gone stale — 50.6 KB is still in the same range as the roadmap's
"56 KB" — but the roadmap's stated *cause* is incomplete.** `ocpp-client` 0.5.0 requests
`ocpp-types`'s `alloc` feature unconditionally (`default-features = false, features = ["serde",
"alloc"]`), and Cargo feature unification means that request applies everywhere this crate is
built, including its own `--no-default-features`/MCU configuration (which is no_std **+ alloc**,
never allocator-free — see `CLAUDE.md`). Under `alloc`, `ChargingSchedule`'s three inlined
sub-schedules already use plain `Option<T>` over `alloc::vec::Vec`-backed lists rather than
const-generic `heapless` capacities — so the *specific* mechanism D2.3 originally named (a fixed
`heapless` cap on the top-level lists) is not what's compiled here at all.

Isolating the actual cost (`most_of_the_size_is_custom_data_not_array_capacity`, same test module) by re-measuring with
`CustomDataType = NoCustomData` instead of this crate's actual `CustomData` binding:

| Type | with `CustomData` (this crate) | with `NoCustomData` |
|---|---|---|
| `ChargingProfile` | 50,584 | 11,240 |
| `ChargingSchedule` | 16,616 | 3,592 |
| `AbsolutePriceSchedule` | 15,064 | 3,104 |

Swapping only the `customData` binding accounts for **~78% of the size** (50,584 → 11,240). The
reason: `ocpp-types`' `CustomData` is `{ vendor_id: heapless::String<255> }`, about 256 bytes,
versus the zero-sized `NoCustomData` `ocpp-types` itself defaults every struct's
`CustomDataType` generic to. `AbsolutePriceSchedule` (and everything it reaches — `RationalNumber`,
`PriceRule`, `PriceRuleStack`, `TaxRule`, `OverstayRule`, `OverstayRuleList`,
`AdditionalSelectedServices`) carries a `customData: Option<CustomDataType>` field per the OCPP
schema, and because `ocpp-client`'s action macro parameterizes a whole request tree by one
`CustomDataType` (`SetChargingProfileRequest<CustomData>` forces every nested struct's generic to
the same concrete `CustomData`, not independently choosable), that ~256-byte cost is paid at
**every** nesting site. Several of those sites sit inside spec-bounded `heapless` arrays that are
`heapless` regardless of the `alloc` feature (`TaxRule` × 10, `AdditionalSelectedServices` × 5,
`OverstayRule` × 5, `PriceRule` × 8 inside each `PriceRuleStack`) — so the ~256-byte `CustomData`
cost multiplies by each array's fixed capacity rather than being paid once. That compounding,
not the alloc/heapless question the roadmap row named, is what actually produces a five-digit
byte count.

The remaining ~11 KB with `NoCustomData` is the genuine "spec-bounded arrays are `heapless`
regardless of `alloc`" cost (`TaxRule`/`AdditionalSelectedServices`/`OverstayRule`/`PriceRule`
capacities) — real, but an order of magnitude smaller than the headline figure, and not what a
`Box` around the three inlined schedules (D2.3's originally proposed fix) would address on its
own, since `Option<AbsolutePriceSchedule<CustomData>>` is already indirection-free-by-value only
at the top level; boxing it would remove `ChargingSchedule`'s ~15 KB `AbsolutePriceSchedule`
inline cost but not the same compounding inside `PriceRuleStack`'s heap-allocated `Vec` elements
(each individually still ~9 KB with the current `CustomData` binding, ~2 KB with `NoCustomData`,
paid per-element on the heap rather than inline — real but not part of this static `size_of`
number).

**Where the cost actually lands at runtime — confirmed, not assumed:**

- `crate::state::ChargingProfileStore` (bounded by `StateLimits::max_charging_profiles`, default
  16) **does not hold the wire type**. It holds this crate's own
  `state::charging_profile::ChargingProfile`/`ChargingSchedule` (96 / 72 bytes, see table above)
  — a protocol-independent model that a version adapter reduces the wire type into on the way in
  (`crate::smart_charging::ocpp_2_1` and siblings), dropping `AbsolutePriceSchedule`/
  `PriceLevelSchedule`/`SalesTariff`/`digestValue` entirely, since this crate does not implement
  ISO 15118-20 price-schedule relay. 16 installed profiles therefore cost on the order of 2.7 KB
  retained, not ~900 KB — the store was never exposed to D2.3's cost.
- The real exposure is **transient**: `ocpp-client`'s inbound handler deserializes one
  `SetChargingProfileRequest<CustomData>` by value (`serde_json::from_value::<A::Request>`)
  before this crate's handler ever runs (`src/payload_limit.rs`, F5.2, documents this same
  boundary and already cites D2.3 by the pre-existing 56 KB figure — now confirmed accurate to
  within the same order of magnitude, ~50.6 KB, even though the mechanism it names needs the
  correction above). One inbound frame therefore costs one ~50.6 KB stack/heap value regardless
  of how small the actual JSON was, for the fraction of a millisecond between deserialization and
  this crate's adapter reducing it to the ~96-byte internal shape and dropping the rest.

**Mitigation landed this round:** none beyond what F5.2 already provides
(`crate::payload_limit`'s frame-size ceiling, which bounds the wire bytes but not the decoded
struct's fixed cost) — this crate has no hook earlier than `ocpp-client`'s own deserialization
(same conclusion `src/payload_limit.rs`'s module docs already reach, independently, for the
general case). There is no local, in-crate representation change available: the store already
uses the minimal shape, and the transient cost is entirely inside `ocpp-client`/`ocpp-types`
before this crate's code runs.

**Upstream report prepared, not filed** (this round has no issue-filing access, and a previous
round's upstream report was withdrawn for resting on an unreproduced number — this one is
reproduced twice, with and without the `CustomData` substitution, via
`cargo test --lib -p ocpp-charge-point size_measurements -- --nocapture` and
`most_of_the_size_is_custom_data_not_array_capacity` in `src/state/charging_profile.rs`):

- **Title:** `ocpp-types` 0.3.0: OCPP 2.1 `ChargingProfile` is ~50 KB by value, ~78% of which is
  one `Option<CustomData>` field (`heapless::String<255>`, ~256 bytes) repeated at every nesting
  site inside the ISO 15118-20 price-schedule subtree, multiplied by that subtree's own
  `heapless`-capacity arrays.
- **Repro:** `cargo test` a crate depending on `ocpp-types = { version = "0.3.0", features =
  ["alloc"] }` with `core::mem::size_of::<ocpp_types::v21::common::ChargingProfile<ocpp_types::v21::common::CustomData>>()`
  vs. the same with `NoCustomData` substituted for the generic parameter — 50,584 vs. 11,240 for
  `ChargingProfile`; 15,064 vs. 3,104 for `AbsolutePriceSchedule` alone.
- **Suggested fixes, either would help, upstream is free to choose:** (a) box the `customData`
  field on struct definitions nested inside spec-bounded `heapless` arrays that are themselves
  nested inside other structs' fields (`RationalNumber`, `PriceRule`, `TaxRule`, `OverstayRule`,
  `AdditionalSelectedServices` — the sites that get multiplied by a surrounding array capacity),
  so the ~256-byte cost is paid once per allocation rather than once per array slot; or (b)
  reduce `AbsolutePriceSchedule`/`PriceLevelSchedule`/`SalesTariff`'s own `heapless` array
  capacities (`TaxRule` × 10, `AdditionalSelectedServices` × 5, `OverstayRule` × 5, `PriceRule` ×
  8) to whatever ISO 15118-20 actually bounds them at in practice, if lower than the current
  defaults; or (c), most directly addressing the roadmap row as originally written, box
  `ChargingSchedule`'s three `Option<T>` inlined schedules
  (`absolute_price_schedule`/`price_level_schedule`/`sales_tariff`) so a `ChargingSchedule` that
  carries none of them (the common case for a plain limit-only profile) pays only a pointer,
  which would remove `ChargingSchedule`'s ~15 KB `AbsolutePriceSchedule` inline cost even before
  (a) or (b). This crate's own adapters never populate any of the three, so a boxed
  representation costs it nothing on the send path either.

## What is not measured or bounded yet

- **Transient allocation during deserialization.** An over-long CSMS payload or
  persisted record is fully deserialized before this crate's bounds truncate or
  refuse it, so peak transient RAM use is not yet bounded — roadmap **F5.2**.
- **Long-run growth.** The RAM figures are worst-case snapshots, not a soak test;
  the assertion that memory doesn't creep across thousands of transactions is
  roadmap **H4.2**.
- **Where the per-version flash goes.** The table attributes flash per feature
  set, not per crate within a feature set — see the note under
  [What this tells you](#what-this-tells-you).
- **The 11 declared-capability features' real cost**, which only exists once the
  functional blocks behind them do (roadmap workstream B).
