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
ocpp-charge-point = "0.x"
```

For a real `#![no_std]` + `alloc` build, disable default features:

```toml
[dependencies]
ocpp-charge-point = { version = "0.x", default-features = false, features = ["ocpp_2_1"] }
```

`cargo check --no-default-features --lib` compiles this crate under `#![no_std]` - internally it's backed by [`embassy-sync`](https://docs.rs/embassy-sync) (`CriticalSectionRawMutex`-based `Mutex`/`Signal`) instead of `tokio::sync`, so no async runtime is baked in.

To link a `--no-default-features` build, an embedded target must:

* register a [`critical-section`](https://docs.rs/critical-section) backend via `critical_section::set_impl!` (the `std` feature does this automatically via `critical-section`'s own `std` backend),
* implement `ocpp_charge_point::executor::Executor` (how to spawn a background task),
* implement `ocpp_charge_point::provisioning::Backoff` (how to wait between retries), and
* implement `ocpp_charge_point::clock::Clock` (how to get the current time), if using the Availability/Transactions functional blocks.

std/tokio users get all four for free (`TokioExecutor`, `TokioBackoff`, `SystemClock`) behind the `tokio-runtime`/`std` features.

See [`docs/ROADMAP.md`](docs/ROADMAP.md) §0 for the detailed history of what's been abstracted and what's still tracked.

---

## 🔌 Supported Protocols

| Protocol   | Status         |
| ---------- | -------------- |
| OCPP 1.6J  | ✅ Supported    |
| OCPP 2.0.1 | ✅ Supported    |
| OCPP 2.1   | 🚧 In Progress |

---

## 🧱 Capability Cargo Features

On top of the protocol-version features above (`ocpp_1_6`/`ocpp_2_0_1`/`ocpp_2_1`), the crate has one Cargo feature per optional OCPP *functional block*. Both groups are orthogonal - any combination compiles (verified in CI's feature matrix, and via `cargo check --no-default-features --features ...` for representative combinations). All capability features are in `default`, so a plain `ocpp-charge-point = "0.x"` dependency behaves exactly as before this section existed; turning one off is an opt-in decision to shrink a firmware image that will never exercise that block, not a runtime behaviour change - see [`docs/PRODUCTION-ROADMAP.md`](docs/PRODUCTION-ROADMAP.md) §5 for the compile-time-vs-runtime split this is one half of.

Two blocks that already have code today gate that code out entirely when their feature is disabled - the module, its re-exports, and the corresponding `ChargePointBuilder` registration method are all `#[cfg]`'d away, so the linked binary genuinely doesn't contain them:

| Feature      | Gates (today)                                                          |
| ------------ | ----------------------------------------------------------------------- |
| `reservation` | `ocpp_charge_point::reservation` (`ReserveNowHandler`/`CancelReservationHandler`), `ChargePointBuilder::reservation` |
| `local-auth-list` | `ocpp_charge_point::local_authorization_list` (`SendLocalListHandler`/`GetLocalListVersionHandler`), `ChargePointBuilder::local_authorization_list` |
| `tariff-cost` | `ocpp_charge_point::cost` (`CostUpdatedHandler`, OCPP `CostUpdated`), `ChargePointBuilder::cost` |

The remaining features are declared now, ready for their implementation to land behind them, and gate nothing yet: `smart-charging`, `firmware-management`, `diagnostics`, `variable-monitoring`, `display-message`, `payment`, `iso15118`, `der-control`, `battery-swap`, `periodic-event-stream`, `certificates`.

`setup()` and `connect_and_setup()` are this crate's "everything on" convenience wrappers - they bound their CSMS client type by every functional block's trait at once, so they only exist when `reservation`, `local-auth-list`, and `tariff-cost` are all enabled. Disabling any of those three (or wanting to skip a block outright, regardless of feature flags) means driving [`ChargePointBuilder`](src/builder.rs) directly instead, registering only the blocks you need.

### OCPP certification profile mapping

The OCPP 2.1 and 2.0.1 "Part 5 - Certification Profiles" specifications (vendored under [`docs/OCPP-2.1/`](docs/OCPP-2.1/) and [`docs/OCPP-2.0.1/`](docs/OCPP-2.0.1/)) define independently-certifiable certification profiles on top of the mandatory "Core" profile. The table below maps each capability feature to the profile(s) it participates in, so a build can be described (and certified) as, for example, "Core + Reservation + Smart Charging":

| Feature | OCPP 2.1 profile(s) | OCPP 2.0.1 profile(s) | Notes |
| --- | --- | --- | --- |
| `smart-charging` | Smart Charging (2.0.1 / 2.1) | Smart Charging | `SetChargingProfile`, `GetCompositeSchedule`, `GetChargingProfile`, `ClearChargingProfile`; 2.1 adds priority/dynamic profiles and EMS Control. |
| `firmware-management` | Core | Core | Secure Firmware Update is a Core-profile capability, not its own certification profile. |
| `diagnostics` | Core | Core | `GetLog`/log retrieval, `GetTransactionStatus`, and `CustomerInformation` are all listed as Core-profile capabilities. |
| `variable-monitoring` | Advanced Device Management | Advanced Device Management | `SetVariableMonitoring`, `SetMonitoringBase`/`Level`, `GetMonitoringReport`, `NotifyEvent` (feature id `DM-0`). |
| `display-message` | Advanced User Interface | Advanced User Interface | `SetDisplayMessage`/`GetDisplayMessages`/`ClearDisplayMessage`/`NotifyDisplayMessages` (feature id `UI-0`). |
| `reservation` | Reservation | Reservation | `ReserveNow`/`CancelReservation`/`ReservationStatusUpdate` (feature id `R-0`). |
| `local-auth-list` | Local Authorization List Management | Local Authorization List Management | `SendLocalList`/`GetLocalListVersion` (feature id `LA-0`). |
| `tariff-cost` | Advanced User Interface (display/tariff bullets) + Payment (2.1, tariff management messages) | Advanced User Interface | `CostUpdated` and driver-facing tariff/cost display are Advanced User Interface; 2.1's `SetDefaultTariff`/`ChangeTransactionTariff`/`GetTariffs` are listed under Payment (2.1). |
| `payment` | Payment (2.1) | *(not part of 2.0.1)* | Integrated/standalone payment terminal, prepaid card, QR code, settlement (feature id `P-0`). 2.1-only. |
| `iso15118` | ISO 15118 support (2.0.1 / 2.1) | ISO 15118 support | Requires a number of Advanced Security and Smart Charging test cases per the spec's own note; covers both ISO 15118-2 (2.0.1/2.1) and ISO 15118-20 (2.1). |
| `der-control` | DER control (2.1) | *(not part of 2.0.1)* | `SetDERControl`/`GetDERControl`/`ClearDERControl`/`ReportDERControl`, `NotifyDERAlarm`/`NotifyDERStartStop`. 2.1-only. |
| `battery-swap` | Core (feature id `C-76`) | *(not part of 2.0.1)* | `BatterySwap`/`RequestBatterySwap`; a Core-profile optional feature (`BatterySwapCtrlr`), not its own certification profile. 2.1-only. |
| `periodic-event-stream` | Advanced Device Management | *(not modeled as PeriodicEventStream messages in 2.0.1)* | `Open`/`Close`/`Adjust`/`GetPeriodicEventStream`, `NotifyPeriodicEventStream` (feature id `DM-0`, test cases `TC_N_107`-`TC_N_109`). 2.1-only messages. |
| `certificates` | Core (install/retrieve/delete certificates) + ISO 15118 support (EV-side contract/MO/V2G certificates) | Core + ISO 15118 support | `InstallCertificate`/`DeleteCertificate`/`GetInstalledCertificateIds`/`CertificateSigned`/`SignCertificate`/`GetCertificateStatus` are Core; `Get15118EVCertificate` and related ISO 15118 certificate management are under ISO 15118 support. |

Derived from `docs/OCPP-2.1/OCPP-2.1_edition2_part5_certification_profiles.pdf` (Table 1 "Certification profiles", p.4-7, and §3.1 "Optional feature list for charging station", p.8-13) and `docs/OCPP-2.0.1/OCPP-2.0.1_edition4_part5_certification_profiles.pdf` (the equivalent Table 1 and §3.1), read via `pdftotext -layout`.

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
ocpp-charge-point = "0.x"
```

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