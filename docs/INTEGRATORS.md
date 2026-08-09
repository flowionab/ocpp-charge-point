# Integrator's guide

This is the practical answer to the promise `CLAUDE.md` makes: *"Integrators (hardware
manufacturers) should only ever need to supply hardware bindings (`crate::hardware`).
Everything else — protocol handling, state machines, transaction lifecycle, networking —
is the crate's responsibility."*

It tells you what to implement, which Cargo features to turn on, how those features relate
to what you declare at runtime, and which entry point (`setup()` or `ChargePointBuilder`)
to start from. It does not re-document every trait method — rustdoc
(`cargo doc --open`, or [docs.rs](https://docs.rs/ocpp-charge-point)) already does that, and
keeping a second copy here would only rot. This guide is the map; rustdoc is the territory.

Every claim below was checked against the source in this repository and, where an example
is cited, actually compiled and run — see the note at the end of each section if you want to
reproduce that.

## 1. What must I implement?

`crate::hardware` currently declares eleven traits, plus one crypto helper trait used only by
`SoftKeyStore`. Three are mandatory; the rest are opt-in per capability and come with a `No*`
(or, for `Storage`, `InMemoryStorage`) default so you can leave them out entirely until your
hardware actually needs them.

### Mandatory — every integrator implements these three

| Trait | What it's for |
| --- | --- |
| [`hardware::ChargePoint`](../src/hardware/charge_point.rs) | The top-level binding: vendor/model identity, your fixed list of EVSEs, your static [`Capabilities`](../src/hardware/capabilities.rs), and `start()`, which wires your hardware's event stream to the crate's actor. |
| [`hardware::Evse`](../src/hardware/evse.rs) | One EVSE: its fixed list of connectors, and `reboot()`. |
| [`hardware::Connector`](../src/hardware/connector.rs) | One connector: `lock`/`unlock`, `open_contactor`/`close_contactor`, and `set_current_limit`. |

There is no default implementation for any of these three — the crate has nothing to charge
a car with until you provide them, and no "no-op" version would be honest.

### Opt-in — implement only what your hardware actually has

| Trait | Capability | Default if you don't implement it |
| --- | --- | --- |
| [`hardware::Storage`](../src/hardware/storage.rs) | Durable key-value persistence for state that should survive a restart (offline queues, the local auth list, device model, transactions, …) | [`NoStorage`] — everything still runs, nothing survives a restart. `InMemoryStorage` (std-only) is a middle ground: survives for the life of the process, not a restart. |
| [`hardware::FileTransfer`](../src/hardware/file_transfer.rs) | Uploading logs / diagnostics, downloading firmware images | [`NoFileTransfer`] |
| [`hardware::FirmwareInstaller`](../src/hardware/firmware.rs) | Applying a downloaded firmware image | [`NoFirmwareInstaller`] |
| [`hardware::FirmwarePublisher`](../src/hardware/firmware_publisher.rs) | Acting as a local firmware controller for other charge points (2.x only) | [`NoFirmwarePublisher`] |
| [`hardware::CertificateStore`](../src/hardware/certificate.rs) | Installing/listing/deleting CSMS-managed certificates | [`NoCertificateStore`] |
| [`hardware::KeyStore`](../src/hardware/key_storage.rs) | Private-key storage (secure element or software fallback via `SoftKeyStore`) | [`NoKeyStore`] |
| [`hardware::Display`](../src/hardware/display.rs) | Driver-facing UI messages | [`NoDisplay`] |
| [`hardware::BatterySwapStation`](../src/hardware/battery_swap.rs) | Battery-swap station hardware (2.1 only, niche) | [`NoBatterySwapStation`] |
| [`hardware::Watchdog`](../src/hardware/watchdog.rs) | Hardware watchdog petting | [`NoWatchdog`] |

Pick which of these you implement based on the hardware class table in §2 below, then only
register the matching `ChargePointBuilder` method (§4) for the ones you did implement — an
unregistered block means the CSMS gets a clean `NotImplemented`/`NotSupported`, not a crash
or a silent no-op.

`SoftwareCrypto` (also in `hardware::key_storage`) is not a top-level integration point: it's
the pluggable crypto backend `SoftKeyStore` itself needs, only relevant if you use that
particular `KeyStore` implementation rather than a secure element.

> Note on counting: the roadmap item that requested this guide says "eleven hardware traits."
> That was accurate when it was written; `BatterySwapStation` landed since (roadmap B8.3) and
> is included above, so the current count read straight from `src/hardware/mod.rs` is eleven
> *plus* `BatterySwapStation` — the list above is the complete, current one. Trust the table,
> not the historical number.

## 2. Which Cargo features do I pick?

Two independent groups, both listed in full in `Cargo.toml` with a comment justifying each
one — read those comments; this section only summarizes them.

- **Protocol versions**: `ocpp_1_6`, `ocpp_2_0_1`, `ocpp_2_1` — which CSMS wire protocol(s)
  you can be asked to speak. Pick at least one; more than one is fine and is how you support
  a fleet where you don't control the CSMS version.
- **Functional blocks**: one feature per optional OCPP block (`reservation`,
  `local-auth-list`, `tariff-cost`, `smart-charging`, `firmware-management`,
  `firmware-publishing`, `diagnostics`, `certificate-management`, `variable-monitoring`,
  `display-message`, `payment`, `iso15118`, `der-control`, `battery-swap`,
  `periodic-event-stream`, `certificates`, `key-storage`). Turning one off shrinks a firmware
  image that will never exercise that block; it does not change runtime behaviour for a block
  you keep on (see §3 for the piece that *does* change behaviour).

All of the above are in `default`, plus `tokio-runtime`, `websocket`, and `std` (pulled in
transitively). A plain `ocpp-charge-point = "0.x"` dependency is therefore the maximal build.

Suggested starting points (see `README.md`'s "Recommended feature set per hardware class"
table for the full rationale per block — it's kept current there so it isn't duplicated here):

- **Basic AC wallbox, single connector, no display, no reservation UX**: start from
  `default-features = false`, add your protocol version feature(s) (typically `ocpp_1_6` and/or
  `ocpp_2_0_1`), `std`, `tokio-runtime`, `websocket`. Core alone (BootNotification, Authorize,
  StatusNotification, TransactionEvent, Reset) covers it — no capability features needed.
- **DC fast charger** (unattended, remote-managed): add `reservation`, `local-auth-list`,
  `diagnostics`, `firmware-management`, `variable-monitoring`, `tariff-cost`, `certificates`,
  `smart-charging` on top of the AC wallbox set.
- **MCU / `no_std` build**: `--no-default-features`, plus only the protocol version(s) and
  capability features you need — no `std`, `tokio-runtime`, or `websocket`. See §6 for what you
  then owe the crate (executor, backoff, clock, critical-section backend).

## 3. How do capabilities relate to features?

This is the distinction most likely to trip you up:

- A **Cargo feature** is a compile-time switch. It decides whether the block's *code* exists
  in your binary at all — enabling `display-message` compiles in `crate::display_message` and
  `ChargePointBuilder::display_messages`; disabling it removes them entirely (smaller flash
  image).
- **`hardware::Capabilities`** (returned once from `ChargePoint::capabilities()`) is a
  *runtime* declaration of what your specific piece of hardware can actually do — e.g.
  `has_display: bool`, `reservation: bool`. It's how the crate decides what to advertise to
  the CSMS (`SupportedFeatureProfiles` on 1.6J, the device model's `*Ctrlr.Available` variables
  on 2.x) and, per `setup()`'s own C3.1 handling, whether to register a block's handlers at all.

**Both must agree**, and disagreeing is a real, catchable mistake in either direction:

- Feature on, capability false: compiling in `display-message` but leaving
  `Capabilities::has_display` false means you paid the flash cost for code that will always
  report itself unsupported to the CSMS.
- Capability true, feature off: `Capabilities` itself has no `#[cfg(feature = ...)]` gating, so
  nothing stops you from setting `reservation: true` in a `--no-default-features` build that
  never compiled in `reservation` — but every advertisement surface (`supported_feature_profiles_1_6`,
  the device model, `setup()`'s C3.1 capability-gated registration) checks the Cargo feature via
  a runtime `feature_enabled()` helper as well as the capability bit, so a capability with no
  matching feature is advertised as absent rather than crashing or lying to the CSMS.

Call [`warn_on_feature_mismatches`] once at startup with your `Capabilities` to get a `tracing`
warning for exactly this class of mismatch (both directions) rather than discovering it from
CSMS behaviour.

You can see this exact mismatch fire by running the `simulated_charge_point` example with
default features (everything on) and its stub `Capabilities::default()` (everything false):

```text
$ cargo run --example simulated_charge_point -- --sessions 1
...
WARN ocpp_charge_point::hardware::capabilities: the `firmware-management` Cargo feature is
enabled but hardware declares capability `firmware_management` absent - it will be advertised
to the CSMS as unsupported despite being compiled in
  capability="firmware_management" cargo_feature="firmware-management"
...
```

(That output is what actually printed when this guide was written — reproduce it yourself
with the command above.)

[`CAPABILITY_GATES`] in `src/hardware/capabilities.rs` is the single source of truth mapping
each Cargo feature to its `Capabilities` field and to every advertisement surface that reads
it. A data-driven test, `all_four_capability_propagation_surfaces_agree_with_the_capability_set`
in `src/setup.rs` (the C3.5 "keystone test" from `docs/PRODUCTION-ROADMAP.md` §5.3), drives
`CAPABILITY_GATES` itself with the capability both present and absent and checks that handler
registration, the 2.x device model, 1.6J `SupportedFeatureProfiles`, and 2.x `*Ctrlr.Available`
variables all agree — so a capability added to the table without being reflected everywhere
fails CI rather than shipping a quiet gap.

Practical rule: whatever `Capabilities` your `ChargePoint::capabilities()` returns should be
the honest truth about your hardware, independent of which Cargo features you happened to
compile in. Leave a feature on "just in case" if you like — cargo features are cheap to leave
enabled and `Capabilities::default()` is deliberately all-`false`/conservative — but never
report a capability `true` that your hardware doesn't actually have.

## 4. `setup()` vs `ChargePointBuilder`

- **[`setup()`](../src/setup.rs)** (and its network-aware sibling `connect_and_setup()`) is the
  "everything on" convenience wrapper. It registers every functional block the crate has, in a
  fixed order, against a single CSMS client type bounded by every corresponding trait at once
  (45 trait bounds as of this writing — count them yourself with
  `awk '/N: BootNotifier/,/X: Executor/' src/setup.rs | grep -c '^\s*+'` if you want to verify
  after a future change, and subtract the trailing `Clone + Send + Sync + 'static` — those
  aren't functional-block traits). Use it when your CSMS client (typically one from
  `ocpp-client`, e.g. `ocpp_client::connect_2_1`) implements the full trait set, or when you
  just want the reference "all blocks on" behaviour with the least code.
- **[`ChargePointBuilder`](../src/builder.rs)** registers one functional block at a time
  (`.provisioning(...)`, `.reservation(...)`, `.der_control(...)`, `.certificates(...)`, and so
  on — one method per block, over fifty in total). Use it whenever:
  - your CSMS client only implements a subset of the blocks (a test double, a CSMS that hasn't
    implemented a block yet, a client type you don't control),
  - you want to skip a block outright regardless of Cargo features (e.g. you compiled in
    `reservation` for a shared config but this particular unit has no reservation hardware —
    although the honest fix there is usually `Capabilities::reservation = false` plus `setup()`'s
    own capability-gated registration, see §3),
  - or you're doing anything `setup()` doesn't already do (custom registration order, persistence
    wiring via the `*_persistence`/`*_persisted` methods, which `setup()` does not enable by
    default).

`setup()` is not doing anything magic — it's a thin wrapper that chains `ChargePointBuilder`
calls in a fixed order (read `src/setup.rs` top-to-bottom to see exactly which). Reach for the
builder directly the moment `setup()`'s one-size story stops fitting.

## 5. A working example

Three examples exist under `examples/`. All three were compiled and run against this
worktree while writing this guide — the commands below are exactly what was run.

- **`examples/simple.rs`** — the fullest reference. Shows a from-scratch `ChargePoint`/`Evse`/
  `Connector` implementation plus stand-in implementations of every CSMS-facing trait `setup()`
  needs, wired together with `setup()` itself. Read this first to see the whole shape of an
  integration, including what a real CSMS client (`ocpp_client::connect_2_1` etc.) already gives
  you for free versus what the example fakes for illustration.
- **`examples/embedded_bindings.rs`** — what a `--no-default-features` (`no_std`+`alloc`)
  integration actually costs: a `critical-section` backend, an `Executor`, a `Clock`, and a
  `Backoff`, on top of the same three mandatory hardware traits. Confirmed working:
  ```text
  $ cargo build --example embedded_bindings --no-default-features   # compiles under no_std
  $ cargo run --example embedded_bindings --no-default-features     # runs a full session offline
  ```
  It runs a full plug-in → authorize → charge → unplug script with no CSMS and no tokio, and
  prints each hardware command as it fires.
- **`examples/simulated_charge_point.rs`** — the std/tokio path end-to-end: dial a real CSMS
  over WebSocket (`ocpp_charge_point::connect_and_setup`), or run the same state machine fully
  offline with no backend at all. Also doubles as a soak-test subject (loops sessions
  indefinitely). Confirmed working:
  ```text
  $ cargo run --example simulated_charge_point -- --sessions 1   # offline, one session, exits
  $ cargo run --example simulated_charge_point -- ws://localhost:9000/CP001   # against a real CSMS
  ```
  Run with no arguments it loops forever (that's the soak-test mode) — pass `--sessions N` to
  get a bounded run, as shown above. This is also the example that produced the capability/feature
  mismatch warnings quoted in §3, because its stub hardware returns `Capabilities::default()`
  while the crate builds with every capability feature on by default.

No fourth example was written for this guide, per the roadmap item's own instruction to point
at what exists rather than duplicate it.

## 6. What an integrator must NOT have to do, and the honest edges

You should never need to touch: OCPP message (de)serialization or wire format, WebSocket/
transport connection management, the charge-point/EVSE/connector/transaction state machines,
retry/backoff logic for BootNotification or offline queues, or protocol-version differences
(the crate's internal state model is version-independent; adapters project it down to
1.6J/2.0.1/2.1, never the reverse, per `CLAUDE.md`). If a task feels like it needs any of
that, it's a sign you're fighting the abstraction rather than using it — file it as a roadmap
gap rather than working around it in integration code.

Honest edges, as of this writing:

- **`no_std` is not zero-effort.** `--no-default-features` compiles
  (`cargo check --no-default-features --lib` is a CI gate), but linking a real embedded target
  additionally requires you to register a `critical-section` backend
  (`critical_section::set_impl!`) and supply your own `ocpp_charge_point::executor::Executor`,
  `ocpp_charge_point::provisioning::Backoff`, and `ocpp_charge_point::clock::Clock` (only if you
  register the Availability/Transactions blocks that need a clock). `TokioExecutor`/
  `TokioBackoff`/`SystemClock` cover you for free under `std`/`tokio-runtime`, which is why
  they're in `default`. See `docs/ROADMAP.md` §0 for the detailed history of what's left open
  here (in particular `ChargePointActor::spawn`'s exact bound choices).
- **Durability is opt-in per concern, not all-or-nothing.** Implementing `hardware::Storage`
  alone does nothing until you also call the matching `ChargePointBuilder::*_persistence`/
  `*_persisted` method for each concern you want to survive a restart (transactions, the local
  auth list, the device model, security log, network profiles, charging profiles, boot reason).
  `setup()` does not call any of these for you — if you need persistence, use the builder.
- **Some capability Cargo features gate nothing yet.** `smart-charging`, `diagnostics`'s
  periodic-event-stream sibling, `variable-monitoring`, `payment`, `iso15118`,
  `certificate-management`, `certificates` (see each feature's own comment in `Cargo.toml` for
  precisely what is and isn't implemented behind it — some of these names already gate real code
  for part of their scope and not others, e.g. `diagnostics` covers log upload,
  `GetTransactionStatus`, and `CustomerInformation` today but not 2.1's periodic event streams).
  Enabling one of these today buys you nothing at compile time yet; it's declared so the
  corresponding implementation has a feature flag to land behind later. Check
  `docs/PRODUCTION-ROADMAP.md` for the tracking item if you need one of these blocks now.
- **The hardware trait surface is not yet frozen.** `docs/PRODUCTION-ROADMAP.md`'s H5.5 tracks
  freezing it for 1.0; until then, a new required method can land on an existing trait as a
  breaking change (this has already happened once, for `ChargePoint::capabilities()` — see that
  method's own rustdoc for the precedent). Pin a version if that risk matters to you before 1.0.

[`NoStorage`]: ../src/hardware/storage.rs
[`NoFileTransfer`]: ../src/hardware/file_transfer.rs
[`NoFirmwareInstaller`]: ../src/hardware/firmware.rs
[`NoFirmwarePublisher`]: ../src/hardware/firmware_publisher.rs
[`NoCertificateStore`]: ../src/hardware/certificate.rs
[`NoKeyStore`]: ../src/hardware/key_storage.rs
[`NoDisplay`]: ../src/hardware/display.rs
[`NoBatterySwapStation`]: ../src/hardware/battery_swap.rs
[`NoWatchdog`]: ../src/hardware/watchdog.rs
[`CAPABILITY_GATES`]: ../src/hardware/capabilities.rs
[`warn_on_feature_mismatches`]: ../src/hardware/capabilities.rs
