# ⚡ OCPP Charge Point

> **A Rust framework for building complete OCPP-enabled charge point firmware.**

[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT%20%2F%20Apache--2.0-blue.svg)](#license)
[![Crates.io](https://img.shields.io/crates/v/ocpp-charge-point)](https://crates.io/crates/ocpp-charge-point)
[![Documentation](https://docs.rs/ocpp-charge-point/badge.svg)](https://docs.rs/ocpp-charge-point)
[![.github/workflows/ci.yaml](https://github.com/flowionab/ocpp-charge-point/actions/workflows/ci.yaml/badge.svg)](https://github.com/flowionab/ocpp-charge-point/actions/workflows/ci.yaml)
[![no_std](https://img.shields.io/badge/no__std-supported-blue.svg)](#-no_std-support)

---

## 🚀 Overview

**OCPP Charge Point** is a Rust framework for building complete EV charger firmware.

The goal is to make developing custom OCPP-compatible charging hardware as simple as implementing the required hardware bindings.

Instead of building the complete charging stack from scratch, manufacturers and developers can focus on their hardware:

* GPIO control
* Contactor switching
* Metering
* Connector handling
* LEDs and user interfaces
* Hardware-specific drivers

while OCPP Charge Point provides the application layer responsible for:

* Charge point state management
* Charging workflows
* Connector lifecycle
* Transaction handling
* Backend communication

Built on top of the Flowion Rust charging stack, it provides a modern foundation for creating reliable and maintainable OCPP-compatible charging solutions.

---

## ✨ Features

* 🦀 Native Rust implementation
* ⚡ Complete charge point application framework
* 🔌 OCPP 1.6J support
* 🚀 OCPP 2.0.1 support
* 🔮 OCPP 2.1 support in progress
* 💾 Embedded-friendly architecture
* 🔋 `no_std` compatible design
* 🌐 WebSocket and embedded transport support
* 🔄 Charge point state management
* 🔌 Connector lifecycle handling
* ⚡ Transaction management
* 📊 Meter value reporting
* 🛡️ Hardware abstraction layer
* 🧩 Pluggable hardware bindings

---

## 🤔 Why OCPP Charge Point?

Building an OCPP charger from scratch requires implementing many complex pieces:

* OCPP communication
* Connection management
* Charging state machines
* Transaction handling
* Error management
* Backend integration
* Hardware interaction

OCPP Charge Point separates these responsibilities.

| Traditional Charger Development         | OCPP Charge Point                |
| --------------------------------------- | -------------------------------- |
| Build complete software stack           | ✅ Use a ready framework          |
| Implement charging workflows yourself   | ✅ Built-in charge point logic    |
| Create state machines manually          | ✅ Framework-managed states       |
| Hardware tightly coupled to application | ✅ Hardware abstraction           |
| Difficult testing                       | ✅ Designed for automated testing |
| Vendor-specific implementations         | ✅ Open Rust architecture         |

---

## 🌐 Flowion Rust Charging Stack

OCPP Charge Point is part of the **Flowion Rust Charging Stack**, designed to provide reusable building blocks for EV charging development.

The stack separates communication, application logic, and hardware integration:

```text
┌──────────────────────────────────────┐
│          Charge Point Hardware       │
│                                      │
│  MCU, GPIO, Meter, Contactor, UI     │
└──────────────────┬───────────────────┘
                   │
                   │ Hardware bindings
                   │
┌──────────────────▼───────────────────┐
│          ocpp-charge-point            │
│                                      │
│  • Charge point state machine        │
│  • Connector management              │
│  • Transactions                      │
│  • Charging workflows                │
└──────────────────┬───────────────────┘
                   │
                   │
┌──────────────────▼───────────────────┐
│           ocpp-client                 │
│                                      │
│  • OCPP communication                │
│  • Transport abstraction             │
│  • Connection management             │
└──────────────────────────────────────┘
```

---

## 🔌 ocpp-client

**OCPP communication and transport layer**

Repository:

https://github.com/flowionab/ocpp-client

`ocpp-client` provides the communication foundation required to connect a charge point to a CSMS.

It provides:

* WebSocket communication
* Embedded transports
* Connection lifecycle management
* Message routing
* `no_std` support

OCPP Charge Point focuses on charger behavior, while `ocpp-client` handles communication.

---

## 🔋 Hardware Integration

OCPP Charge Point intentionally does not assume a specific hardware platform.

Instead, hardware manufacturers provide implementations for their own hardware.

Examples of hardware bindings include:

* 🔌 Contactor control
* ⚡ Energy metering
* 🔋 Charging current control
* 🚦 Connector state detection
* 💡 LEDs and user interface
* 🌡️ Temperature monitoring
* 🛡️ Safety monitoring

This allows the same charging logic to run across different hardware platforms.

---

## 🧩 Supported Hardware

OCPP Charge Point is designed to support a variety of embedded platforms.

The framework is hardware-independent and relies on hardware abstraction layers to integrate with specific devices.

Current targets:

| Platform                     | Status         |
| ---------------------------- | -------------- |
| STM32                        | 🚧 In Progress |
| Other ARM Cortex-M platforms | Planned        |

Additional platforms can be supported by implementing the required hardware interfaces.

---

## 🔩 no_std Support

OCPP Charge Point is built to run on microcontrollers without an OS.

By default, the crate builds with the `tokio-runtime` feature (which implies `std`) enabled, so `cargo build`/`cargo test` work with zero configuration on a normal host:

```toml
[dependencies]
ocpp-charge-point = "0.1"
```

For a real `#![no_std]` + `alloc` build, disable default features:

```toml
[dependencies]
ocpp-charge-point = { version = "0.1", default-features = false, features = ["ocpp_2_1"] }
```

`cargo check --no-default-features --lib` compiles this crate under `#![no_std]` - internally it's backed by [`embassy-sync`](https://docs.rs/embassy-sync) (`CriticalSectionRawMutex`-based `Mutex`/`Signal`) instead of `tokio::sync`, so no async runtime is baked in.

To link a `--no-default-features` build, an embedded target must:

* register a [`critical-section`](https://docs.rs/critical-section) backend via `critical_section::set_impl!` (the `std` feature does this automatically via `critical-section`'s own `std` backend),
* implement `ocpp_charge_point::executor::Executor` (how to spawn a background task),
* implement `ocpp_charge_point::provisioning::Backoff` (how to wait between retries), and
* implement `ocpp_charge_point::clock::Clock` (how to get the current time), if using the Availability/Transactions functional blocks.

std/tokio users get all four for free (`TokioExecutor`, `TokioBackoff`, `SystemClock`) behind the `tokio-runtime`/`std` features.

See [`docs/ROADMAP.md`](docs/ROADMAP.md) §0 for the detailed history of what's been abstracted and what's still tracked.

### 📏 Memory and flash footprint

Every collection that can grow is bounded by a configured maximum (`state::StateLimits` for the local authorization list and device model, `offline_queue::OfflineQueue`'s capacity for the offline report queues, `security::SecurityEventLog`'s capacity for the durable security log, `state::StateLimits::max_charging_profiles` for charging profiles), so peak memory is a property of your configuration rather than of how much a CSMS sends you.

Measured worst-case retained heap, filled to those bounds:

| Configuration | Retained heap |
| --- | --- |
| Tight AC wallbox (1 connector, 25 list entries, 64 device model variables) | **~63 KB** |
| Crate defaults (2 connectors, 100 entries, 256 variables) | **~184 KB** |
| DC site (4 EVSEs × 2 connectors, 500 entries, 512 variables) | **~404 KB** |

These are 64-bit host figures and a conservative upper bound for a 32-bit MCU, which holds less. They exclude task stacks, transport/TLS buffers, and allocator overhead.

Measured flash, as a linked `thumbv7em-none-eabihf` image (`opt-level="z"`, fat LTO, `--gc-sections`):

| Feature set | Flash |
| --- | --- |
| Core, no protocol version (state machine, actor, hardware dispatch) | **32 KB** |
| Core + OCPP 1.6J | **174 KB** |
| Core + OCPP 2.0.1 | **224 KB** |
| Core + OCPP 2.1 | **310 KB** |
| Core + all three versions | **474 KB** |
| Everything (all versions + every capability feature) | **523 KB** |

The negotiated protocol version is the decision that dominates: the second and third version cost +164 KB on top of 2.1 alone, so a single-version build is the first lever to pull on a 512 KB part. The individually gated functional blocks are cheap by comparison — `reservation` +10 KB, `local-auth-list` +12 KB, `tariff-cost` +5 KB. These exclude your transport, TLS, executor, allocator and startup code.

[`docs/MEMORY.md`](docs/MEMORY.md) has the full breakdown for both RAM and flash, the per-unit costs for sizing your own configuration, and one finding worth reading before writing a hardware binding: **how you group device model variables across components changes their RAM cost by up to 5.6×**. Regenerate the numbers with `cargo test --test memory_budget -- --nocapture` (RAM, also gates against regressions) and `scripts/flash-cost.sh` (flash).

---

## 🔌 Supported Protocols and message coverage

| Protocol   | Status      | Messages wired |
| ---------- | ----------- | --------------- |
| OCPP 1.6J  | ✅ Supported | 30 / 39 |
| OCPP 2.0.1 | ✅ Supported | 63 / 64 |
| OCPP 2.1   | ✅ Supported | 90 / 91 |

"Messages wired" counts, for each protocol version, how many of `ocpp-client` 0.5.0's generated
actions have a corresponding `.on_*(`/`.send_*(` call somewhere in this crate's per-version
adapter code. It is **not** the same claim as "this build is certifiable for profile X" - a
message being wired means an adapter exists for it, not that OCTT has been run against it or that
every field/edge case in the spec's test cases is handled. See
[H3](docs/PRODUCTION-ROADMAP.md#103-h3--compliance) for the compliance work (OCTT, the part-6
test-case sweep, interoperability testing) that turns "wired" into "certifiable", and
[`docs/CERTIFICATION.md`](docs/CERTIFICATION.md) (H3.3) for which certification profiles this
crate can honestly claim today, per feature set, and which ones need either an OCTT run or
integrator hardware this crate cannot supply on its own.

**Regenerate this row before trusting an old copy of it** - the numbers move as adapters land:

```sh
python3 scripts/message-coverage.py
```

The script reads the actual `send_x`/`on_x` method names out of `ocpp-client`'s own generated
`actions.rs` for each version (rather than guessing them by snake-casing the action name, which
undercounts on acronyms like `GetDERControl` -> `on_get_der_control` or `AFRRSignal` ->
`send_afrr_signal`), and self-checks that every wired call in `src/` is attributed to a known
action - see the script's own docstring for the exact traps it accounts for. A handful of
messages are deliberately not wired yet; run the script for the current list (it prints one per
version) rather than trusting a hardcoded list here.

---

## 🧱 Capability Cargo Features

On top of the protocol-version features above (`ocpp_1_6`/`ocpp_2_0_1`/`ocpp_2_1`), the crate has one Cargo feature per optional OCPP *functional block*. Both groups are orthogonal - any combination compiles (verified in CI's feature matrix, and via `cargo check --no-default-features --features ...` for representative combinations). All capability features are in `default`, so a plain `ocpp-charge-point = "0.1"` dependency behaves exactly as before this section existed; turning one off is an opt-in decision to shrink a firmware image that will never exercise that block, not a runtime behaviour change - see [`docs/PRODUCTION-ROADMAP.md`](docs/PRODUCTION-ROADMAP.md) §5 for the compile-time-vs-runtime split this is one half of.

Eight blocks gate real code today - the module, its re-exports, and (where one exists) the
corresponding `ChargePointBuilder` registration method are all `#[cfg]`'d away when the feature is
disabled, so the linked binary genuinely doesn't contain them (see `src/lib.rs`/`src/builder.rs`
for the exact `#[cfg(feature = "...")]` attributes this table is read off):

| Feature      | Gates (today)                                                          |
| ------------ | ----------------------------------------------------------------------- |
| `reservation` | `ocpp_charge_point::reservation` (`ReserveNowHandler`/`CancelReservationHandler`), `ChargePointBuilder::reservation` |
| `local-auth-list` | `ocpp_charge_point::local_authorization_list` (`SendLocalListHandler`/`GetLocalListVersionHandler`), `ChargePointBuilder::local_authorization_list` |
| `tariff-cost` | `ocpp_charge_point::cost` (`CostUpdatedHandler`) + `ocpp_charge_point::tariff` (`SetDefaultTariffHandler`/`ChangeTransactionTariffHandler`/`ClearTariffsHandler`/`GetTariffsHandler`), `ChargePointBuilder::cost`/`ChargePointBuilder::tariffs` |
| `display-message` | `ocpp_charge_point::display_message` (`SetDisplayMessageHandler`/`ClearDisplayMessageHandler`/`GetDisplayMessagesHandler`), `ChargePointBuilder::display_messages` |
| `der-control` | `ocpp_charge_point::der_control` (`SetDERControlHandler`/`ClearDERControlHandler`/`GetDERControlHandler`/`AfrrSignalHandler`/`NotifyAllowedEnergyTransferHandler`), `ChargePointBuilder::der_control` |
| `payment` | `ocpp_charge_point::payment`, `ChargePointBuilder::payment` |
| `periodic-event-stream` | `ocpp_charge_point::periodic_event_stream` (`OpenPeriodicEventStreamHandler`/`ClosePeriodicEventStreamHandler`/`AdjustPeriodicEventStreamHandler`/`GetPeriodicEventStreamHandler`), `ChargePointBuilder::periodic_event_streams` |
| `battery-swap` | `ocpp_charge_point::battery_swap` (`RequestBatterySwapHandler`), `ChargePointBuilder::battery_swap` |

The remaining declared features gate nothing at compile time today - their modules and
`ChargePointBuilder` registration methods (where either exists) compile unconditionally, so
toggling the feature currently has no effect on the linked binary: `smart-charging`
(`crate::smart_charging` and `ChargePointBuilder::smart_charging` are fully implemented but
unconditional), `firmware-management` (`ChargePointBuilder::firmware_updates`),
`diagnostics` (`ChargePointBuilder::log_uploads`), `variable-monitoring`
(`crate::variable_monitoring` is also fully implemented and unconditional), `certificate-management`
(`ChargePointBuilder::certificates`), `ocsp-checking` (`ChargePointBuilder::ocsp_status`/
`ocsp_chain_status`), `key-storage`, `iso15118`, `certificates`. In every one of these cases the
Cargo feature only affects what [`hardware::Capabilities`](src/hardware/capabilities.rs)
*advertises* through [`CAPABILITY_GATES`](src/hardware/capabilities.rs) (device-model
`*Ctrlr.Available` variables, 1.6J `SupportedFeatureProfiles`) - it does not add or remove code
from the binary. See `Cargo.toml`'s own per-feature doc comments for the current, authoritative
statement of what each one does, since that's the file most likely to be updated the moment a
feature's status changes.

`setup()` and `connect_and_setup()` are this crate's "everything on" convenience wrappers - they bound their CSMS client type by every functional block's trait at once, so they only exist when `reservation`, `local-auth-list`, `tariff-cost`, and `periodic-event-stream` are all enabled. Disabling any of those (or wanting to skip a block outright, regardless of feature flags) means driving [`ChargePointBuilder`](src/builder.rs) directly instead, registering only the blocks you need.

### OCPP certification profile mapping

The OCPP 2.1 and 2.0.1 "Part 5 - Certification Profiles" specifications (available from the [Open Charge Alliance](https://openchargealliance.org/protocols/open-charge-point-protocol/); this project reads them from `docs/OCPP-2.1/` and `docs/OCPP-2.0.1/`, which are gitignored, so they are in neither this repository nor the published crate - obtain your own copies to check this table against) define independently-certifiable certification profiles on top of the mandatory "Core" profile. The table below maps each [`CAPABILITY_GATES`](src/hardware/capabilities.rs) entry to the profile(s)/component(s) it participates in, so a build can be described as, for example, "Core + Reservation + Smart Charging" - **describing** it that way is not the same as being able to **certify** it that way; see [H3](docs/PRODUCTION-ROADMAP.md#103-h3--compliance) for the gap.

| Capability (`CAPABILITY_GATES` name) | Cargo feature | 2.x `*Ctrlr` component | 1.6J feature profile | OCPP 2.1 profile | OCPP 2.0.1 profile | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| `reservation` | `reservation` | `ReservationCtrlr` | `Reservation` | Reservation | Reservation | `ReserveNow`/`CancelReservation`/`ReservationStatusUpdate` (feature id `R-0`). |
| `local_auth_list` | `local-auth-list` | `LocalAuthListCtrlr` | `LocalAuthListManagement` | Local Authorization List Management | Local Authorization List Management | `SendLocalList`/`GetLocalListVersion` (feature id `LA-0`). |
| `smart_charging` | `smart-charging` | `SmartChargingCtrlr` | `SmartCharging` | Smart Charging | Smart Charging | `SetChargingProfile`, `GetCompositeSchedule`, `GetChargingProfile`, `ClearChargingProfile`; 2.1 adds priority/dynamic profiles and EMS Control. Feature flag currently gates nothing (see above) - the code is real. |
| `tariff_and_cost` | `tariff-cost` | `TariffCostCtrlr` | *(none)* | Advanced User Interface (display/cost) + Payment (2.1 tariff messages) | Advanced User Interface | `CostUpdated` and driver-facing cost display are Advanced User Interface; 2.1's `SetDefaultTariff`/`ChangeTransactionTariff`/`GetTariffs` are listed under Payment (2.1). |
| `has_display` | `display-message` | `DisplayMessageCtrlr` | *(none)* | Advanced User Interface | Advanced User Interface | `SetDisplayMessage`/`GetDisplayMessages`/`ClearDisplayMessage`/`NotifyDisplayMessages` (feature id `UI-0`). 2.x only. |
| `diagnostics` | `diagnostics` | *(none in 2.1 appendix)* | `FirmwareManagement` | Core | Core | `GetLog`/log retrieval, `GetTransactionStatus`, and `CustomerInformation` are all Core-profile capabilities. |
| `firmware_management` | `firmware-management` | *(none in 2.1 appendix)* | `FirmwareManagement` | Core | Core | Secure Firmware Update is a Core-profile capability, not its own certification profile. |
| `firmware_publishing` | `firmware-publishing` | *(none defined in 2.1 appendix)* | *(none - predates the local-controller concept)* | *(no dedicated profile - local-controller behaviour)* | *(not part of 2.0.1)* | `PublishFirmware`/`UnpublishFirmware`/`PublishFirmwareStatusNotification`. 2.x only. |
| `certificate_management` | `certificate-management` | *(none - lives on `SecurityCtrlr`)* | *(none - Security Whitepaper, not a core feature profile)* | Core | Core | `InstallCertificate`/`DeleteCertificate`/`GetInstalledCertificateIds`/`CertificateSigned`/`SignCertificate` are Core-profile capabilities. |
| `ocsp_checking` | `ocsp-checking` | *(none - lives on `SecurityCtrlr`)* | *(none)* | Core | Core | `GetCertificateStatus`/`GetCertificateChainStatus`. Deliberately its own gate, independent of `certificate_management` - see that field's doc comment in `src/hardware/capabilities.rs`. |
| `key_storage` | `key-storage` | *(none - underpins `SecurityCtrlr` security profile 3)* | *(none)* | *(no messages consume this yet)* | *(no messages consume this yet)* | Backing for mutual-TLS signing and CSR generation; no OCPP message is gated on it today. |
| `payment` | `payment` | `PaymentCtrlr` | *(none - 2.1 only)* | Payment | *(not part of 2.0.1)* | `NotifySettlement`/`NotifyWebPaymentStarted`/`VatNumberValidation` (feature id `P-0`). 2.1-only. |
| `der_control` | `der-control` | *(not verified against the real appendix - recorded `None` rather than guessed)* | *(none - 2.1 only)* | DER control | *(not part of 2.0.1)* | `SetDERControl`/`GetDERControl`/`ClearDERControl`/`ReportDERControl`, `NotifyDERAlarm`/`NotifyDERStartStop`, `AFRRSignal`. 2.1-only. |
| `battery_swap` | `battery-swap` | *(not verified against the real appendix - recorded `None` rather than guessed)* | *(none - 2.1 only)* | Core (feature id `C-76`) | *(not part of 2.0.1)* | `BatterySwap`/`RequestBatterySwap`; a Core-profile optional feature, not its own certification profile. 2.1-only. |
| `periodic_event_stream` | `periodic-event-stream` | *(none defined in 2.1 appendix)* | *(none - 2.1 only)* | Advanced Device Management | *(no PeriodicEventStream concept in 2.0.1)* | `Open`/`Close`/`Adjust`/`GetPeriodicEventStream`, `NotifyPeriodicEventStream` (feature id `DM-0`, test cases `TC_N_107`-`TC_N_109`). 2.1-only. |

Not in `CAPABILITY_GATES` yet, so not part of [C3.5](docs/PRODUCTION-ROADMAP.md#53-c3--capability-propagation)'s cross-surface consistency check, even
though real adapter code exists: `variable_monitoring` (`SetVariableMonitoring`/
`SetMonitoringBase`/`Level`/`GetMonitoringReport`/`NotifyEvent`, Advanced Device Management /
feature id `DM-0` on both 2.1 and 2.0.1) and `certificates`/`iso15118` (declared `Capabilities`
fields and Cargo features with no `CAPABILITY_GATES` row or implementation behind them yet -
`Get15118EVCertificate` is the one message this leaves unwired on 2.0.1 and 2.1, per the coverage
table above).

`*Ctrlr` component names are sourced from `docs/OCPP-2.1/Appendices_CSV_v2.1/dm_components_vars.csv`
where that checkout is available; entries recorded `None` rather than guessed are exactly the ones
`src/hardware/capabilities.rs` itself documents as unverified (see [`docs/OCPP-2.1/` and
`docs/OCPP-2.0.1/` being gitignored](docs/INTEGRATORS.md)). 1.6J profile names are the standard
`SupportedFeatureProfiles` values (Core, FirmwareManagement, LocalAuthListManagement, Reservation,
SmartCharging, RemoteTrigger). Certification-profile columns derived from
`docs/OCPP-2.1/OCPP-2.1_edition2_part5_certification_profiles.pdf` (Table 1 "Certification
profiles", p.4-7, and §3.1 "Optional feature list for charging station", p.8-13) and
`docs/OCPP-2.0.1/OCPP-2.0.1_edition4_part5_certification_profiles.pdf` (the equivalent Table 1 and
§3.1), read via `pdftotext -layout` when those (gitignored) vendored copies are present locally -
re-derive rather than trust a stale copy of this table if you don't have them.

**To regenerate this table**: cross-reference `CAPABILITY_GATES` in
[`src/hardware/capabilities.rs`](src/hardware/capabilities.rs) (capability name, Cargo feature,
`ctrlr_component`, `feature_profile_1_6`) against the certification-profile PDFs above for the
profile columns. [C3.5](docs/PRODUCTION-ROADMAP.md#53-c3--capability-propagation)'s test
(`src/setup.rs`) guarantees the first four columns can't silently drift from the code; nothing
currently guarantees the profile columns stay in sync with the PDFs, so treat them as best-effort
until [H3.3](docs/PRODUCTION-ROADMAP.md#103-h3--compliance) formally decides which profiles this
crate claims.

### Recommended feature set per hardware class

A concrete image never needs every block. Some starting points (start from `default-features = false` and add the version feature(s) your CSMS speaks, plus the capabilities below):

| Hardware class | Suggested capability features | Rationale |
| --- | --- | --- |
| Simple AC wallbox (home/residential, single connector, no display) | *(none - Core only)* | No reservation UX, no display, no payment hardware, no bidirectional power. Core alone (BootNotification, Authorize, StatusNotification, TransactionEvent, Reset) covers it. |
| Public AC charge point (RFID/app auth, optionally a display, pay-by-app) | `reservation`, `local-auth-list`, `variable-monitoring`, `display-message`, `diagnostics` | Public sites typically want reservation and offline authorization (local list) support, plus fleet diagnostics; a display, if fitted, needs `display-message`. |
| DC fast charger (unattended, high utilization, remote diagnostics, firmware fleet management) | `reservation`, `local-auth-list`, `diagnostics`, `firmware-management`, `variable-monitoring`, `tariff-cost`, `certificates`, `smart-charging` | Unattended DC sites lean on remote diagnostics/firmware and smart charging (load management across a site); `certificates` matters once TLS client-cert rotation is in play. |
| V2X-capable / bidirectional (vehicle-to-grid, DER-aware, payment terminal) | All of the above, plus `payment`, `iso15118`, `der-control`, `periodic-event-stream` | V2X needs ISO 15118-20 (`iso15118`) for the EV negotiation, `der-control` for grid-services participation, and `periodic-event-stream` for the higher-rate telemetry DER/V2X operation expects; `payment` if the unit has an integrated terminal. |

`battery-swap` is niche enough (battery-swap stations specifically) that it's not part of any of the four classes above - enable it only for that product line.

---

## 🎯 Use Cases

OCPP Charge Point is designed for:

* 🚗 AC charge points
* ⚡ DC fast chargers
* 🏭 Charging hardware manufacturers
* 🔋 Embedded EV charging projects
* 🧪 Hardware-in-the-loop testing
* 🎓 Learning OCPP charger development
* 🏢 Custom charging solutions

---

## 📦 Installation

Add the dependency:

```toml
[dependencies]
ocpp-charge-point = "0.1"
```

For a full walkthrough of what to implement, which Cargo features to pick, and how the
provided examples demonstrate each, see [`docs/INTEGRATORS.md`](docs/INTEGRATORS.md).

---

## 🚀 Getting Started

A typical implementation consists of:

1. Select your hardware platform
2. Implement hardware bindings
3. Configure your OCPP backend
4. Start the charge point runtime

Example:

```rust
let charger = ChargePoint::new(
    hardware,
    ocpp_client,
);

charger.run().await?;
```

---

## 🛡️ Security

[`docs/THREAT-MODEL.md`](docs/THREAT-MODEL.md) is this crate's threat model: the assets worth
protecting, the trust boundaries, the threat actors, and — for each threat — what mitigates it
today, what does not, and which roadmap task tracks the gap. Written for a certification auditor
or an integrator's security reviewer, and grounded in the actual code rather than general
EV-charging security advice.

---

## 🧪 Testing

OCPP Charge Point is designed to support:

* Integration testing
* Hardware testing
* Simulator-based testing
* CSMS validation
* Automated CI testing

It can be combined with:

* Flowion Charge Point Simulator
* Custom CSMS implementations
* Real charging hardware

---

## 🛣️ Roadmap

Planned improvements:

* 🔮 Complete OCPP 2.1 support
* 🔋 More hardware examples
* 🧩 Plugin architecture
* 🧪 Hardware simulation layer
* 📚 More embedded examples
* 🛠️ Reference hardware implementations

---

## 🤝 Contributing

Contributions are welcome!

You can help by:

* 🐛 Reporting bugs
* 💡 Suggesting features
* 📝 Improving documentation
* 🔧 Adding hardware integrations
* 🚀 Submitting pull requests

---

## 🔖 Versioning

This crate is pre-1.0 and has been making breaking changes regularly. See
[`docs/SEMVER.md`](docs/SEMVER.md) for what that means for code implementing `crate::hardware`
traits specifically, the MSRV commitment, and what changes before the hardware trait surface
freezes at 1.0. See [`CHANGELOG.md`](CHANGELOG.md) for what has actually broken so far, and
[`docs/RELEASE-1.0.md`](docs/RELEASE-1.0.md) for the per-trait stability assessment behind the
1.0 freeze decision (current recommendation: not yet — three named gaps first).

---

## 📄 License

OCPP Charge Point is dual licensed:

* MIT License
* Apache License 2.0

You may choose either license.

---

## 🏢 About Flowion

**OCPP Charge Point** is developed by **Flowion AB** as part of our mission to make EV charging development more accessible.

By combining Rust, embedded development, and open standards such as OCPP, Flowion aims to provide developers and manufacturers with modern building blocks for creating reliable charging infrastructure.

---

## ⭐ Support the Project

If you find this project useful:

* ⭐ Star the repository
* 🐛 Report issues
* 💡 Suggest improvements
* 🤝 Contribute

Together we can make EV charging development easier and more accessible.