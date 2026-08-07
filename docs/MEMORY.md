# Memory budget

How much RAM a charge point built on this crate holds, per configuration, so an
integrator can size the part before committing to hardware. This closes **G2.3**
in [`PRODUCTION-ROADMAP.md`](PRODUCTION-ROADMAP.md) §9.2.

Every number here is **measured, not estimated**: they come from
[`tests/memory_budget.rs`](../tests/memory_budget.rs), which installs a counting
`GlobalAlloc` and reads live requested bytes around each structure it builds.
Regenerate them with

```sh
cargo test --test memory_budget -- --nocapture
```

The test also asserts a ceiling per configuration, so a change that meaningfully
grows retained state fails `cargo test` rather than being discovered on a device.
Raise a ceiling only together with the table below.

## What is counted

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

## Per configuration

Measured on a 64-bit host. See [32-bit targets](#32-bit-targets) below — a
32-bit MCU holds *less*, so these numbers are a safe upper bound.

Each row is filled to its configured worst case: the local authorization list
full of 36-character id tokens (OCPP 2.x's maximum), the device model full at its
configured maximum, every connector holding both an active transaction and a
reservation, and all three offline queues full.

| | Tight AC wallbox | Crate defaults | DC site |
| --- | --- | --- | --- |
| Topology | 1 EVSE × 1 connector | 1 EVSE × 2 connectors | 4 EVSEs × 2 connectors |
| `max_local_authorization_list_entries` | 25 | 100 | 500 |
| `max_device_model_variables` | 64 | 256 | 512 |
| Offline queue capacity (each of 3) | 25 | 100 | 200 |
| Empty state (incl. built-in device model) | 5.0 KB | 5.2 KB | 6.6 KB |
| Local authorization list, full | 3.1 KB | 10.9 KB | 52.5 KB |
| Device model, full | 22.4 KB | 95.8 KB | 190.7 KB |
| Busy connectors (transaction + reservation each) | 0.1 KB | 0.3 KB | 1.0 KB |
| Status queue, full | 0.8 KB | 3.1 KB | 6.1 KB |
| Transaction queue, full | 6.0 KB | 23.8 KB | 47.6 KB |
| Security queue, full | 5.1 KB | 20.5 KB | 41.1 KB |
| **Total retained** | **42.6 KB** | **159.7 KB** | **345.8 KB** |

Read that as: the crate's own defaults need roughly **160 KB of heap** in the
worst case, and a deliberately tightened single-connector wallbox fits in
roughly **43 KB**. Neither figure includes the exclusions above.

### Per-unit costs, for sizing your own configuration

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

An offline queue's `VecDeque` grows by doubling, so a queue configured with
capacity 100 ends up with 128 slots allocated. Round a configured capacity up to
the next power of two when budgeting.

## The device model dominates — and its shape matters

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
- The ~5 KB "empty state" floor is almost entirely the built-in device model's
  two default variables sitting on two separate components (two inner nodes plus
  an outer one). It is the price of having a device model at all, not of the
  charge point's topology — note how little the floor moves between a
  1-connector and an 8-connector configuration.

## 32-bit targets

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

## What is not bounded yet

- **Transient allocation during deserialization.** An over-long CSMS payload or
  persisted record is fully deserialized before this crate's bounds truncate or
  refuse it, so peak transient use is not yet bounded — roadmap **F5.2**.
- **Flash, as opposed to RAM.** Per-Cargo-feature flash cost is roadmap **G2.4**.
- **Long-run growth.** These are worst-case snapshots, not a soak test; the
  assertion that memory doesn't creep across thousands of transactions is roadmap
  **H4.2**.
