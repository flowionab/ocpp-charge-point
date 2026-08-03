# ⚡ OCPP Charge Point

> **A Rust framework for building complete OCPP-enabled charge point firmware.**

[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT%20%2F%20Apache--2.0-blue.svg)](#license)
[![Crates.io](https://img.shields.io/crates/v/ocpp-charge-point)](https://crates.io/crates/ocpp-charge-point)
[![Documentation](https://docs.rs/ocpp-charge-point/badge.svg)](https://docs.rs/ocpp-charge-point)
[![.github/workflows/ci.yaml](https://github.com/flowionab/ocpp-charge-point/actions/workflows/ci.yaml/badge.svg)](https://github.com/flowionab/ocpp-charge-point/actions/workflows/ci.yaml)

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

## 🔌 Supported Protocols

| Protocol   | Status         |
| ---------- | -------------- |
| OCPP 1.6J  | ✅ Supported    |
| OCPP 2.0.1 | ✅ Supported    |
| OCPP 2.1   | 🚧 In Progress |

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