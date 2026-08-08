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
| Empty state (incl. built-in device model) | 17.3 KB | 17.6 KB | 19.7 KB |
| Local authorization list, full | 3.1 KB | 10.9 KB | 52.5 KB |
| Device model, full | 22.4 KB | 95.8 KB | 190.7 KB |
| Busy connectors (transaction + reservation each) | 0.1 KB | 0.3 KB | 1.0 KB |
| Charging profiles, full (8 periods each) | 4.7 KB | 4.7 KB | 4.7 KB |
| Status queue, full | 0.8 KB | 3.1 KB | 6.1 KB |
| Transaction queue, full | 6.0 KB | 23.8 KB | 47.6 KB |
| Security queue, full | 5.1 KB | 20.5 KB | 41.1 KB |
| Security log, full | 5.6 KB | 11.3 KB | 45.2 KB |
| **Total retained** | **59.0 KB** | **178.5 KB** | **401.3 KB** |

Read that as: the crate's own defaults need roughly **179 KB of heap** in the
worst case, and a deliberately tightened single-connector wallbox fits in
roughly **59 KB**. Neither figure includes the exclusions above.

The empty-state floor jumped from ~5 KB to ~17 KB when the crate started
registering OCPP's standard configuration variables by default (B1.6: 26 of
them, so that a 1.6J CSMS can read every *required* key without the hardware
binding registering anything). That is the device model's per-variable cost
below in action, and it is the price of protocol compliance rather than of
topology — note how little it moves between a 1-connector and an 8-connector
charge point.

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
- The ~17 KB "empty state" floor is almost entirely the built-in device model's
  26 default variables (B1.6's standard OCPP configuration keys), spread across
  eight `*Ctrlr` components. It is the price of answering OCPP's required
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
