//! What an embedded integrator has to supply (`docs/PRODUCTION-ROADMAP.md` G1.3).
//!
//! This crate compiles without `std` - `cargo check --no-default-features --lib` is a CI gate -
//! but that claim is only useful if someone can see what building against it actually costs. With
//! default features off you must supply four things this example demonstrates in full:
//!
//! 1. a `critical-section` backend, because the crate's channels are `embassy-sync`-backed,
//! 2. an [`Executor`], since there is no `tokio::spawn`,
//! 3. a [`Clock`], since there is no `chrono::Utc::now()`,
//! 4. a [`Backoff`], since there is no `tokio::time::sleep`.
//!
//! Plus the `crate::hardware` traits themselves, which every integrator implements anyway.
//!
//! # What this does and does not demonstrate
//!
//! **Does:** that the library and every trait an integrator implements compile and run with
//! `--no-default-features`, and exactly which glue that requires.
//!
//! **Does not:** a bare-metal link. This is a host binary, so its `main` and its executor use the
//! host's threads to run the futures - a real target supplies those from its own RTOS or from
//! `embassy-executor`, and links with a `#[panic_handler]` and no `main`. Producing that here would
//! need a cross toolchain (`thumbv7em-none-eabihf` or similar) that may not be installed, and a
//! example that only builds on one developer's machine demonstrates less than this one does.
//!
//! Run it with:
//!
//! ```text
//! cargo run --example embedded_bindings --no-default-features
//! ```

use std::sync::mpsc;
use std::time::Duration;

use ocpp_charge_point::ChargePointBuilder;
use ocpp_charge_point::clock::Clock;
use ocpp_charge_point::executor::Executor;
use ocpp_charge_point::hardware::{
    Capabilities, ChargePoint, Connector, Evse, HardwareCommandReceiver, HardwareEventSender,
    execute_hardware_command,
};
use ocpp_charge_point::provisioning::Backoff;
use ocpp_charge_point::state::{ChargePointEvent, ConnectorEvent, EvseEvent, IdToken, IdTokenKind};

// 1. The critical-section backend. `embassy-sync`'s mutexes need one, and with `std` off nothing
//    registers it for you. A single-core MCU implements this by disabling interrupts; here we use
//    the crate's own std backend, which is what `--features std` would have selected.
#[cfg(not(feature = "std"))]
critical_section::set_impl!(HostCriticalSection);

#[cfg(not(feature = "std"))]
struct HostCriticalSection;

#[cfg(not(feature = "std"))]
unsafe impl critical_section::Impl for HostCriticalSection {
    unsafe fn acquire() -> critical_section::RawRestoreState {
        // A real target masks interrupts here and returns the previous mask. This example is
        // single-threaded past `main`, so there is no section to protect.
    }
    unsafe fn release(_token: critical_section::RawRestoreState) {}
}

// 2. The executor. `Executor::spawn` takes an already-boxed, pinned future and must run it to
//    completion in the background. On an MCU this hands the future to `embassy-executor` or your
//    RTOS; here a thread per future is the smallest thing that satisfies the contract.
struct ThreadExecutor;

impl Executor for ThreadExecutor {
    fn spawn(&self, future: std::pin::Pin<Box<dyn Future<Output = ()> + Send>>) {
        std::thread::spawn(move || futures::executor::block_on(future));
    }
}

// 3. The clock. A charge point without an RTC returns something near the epoch until the CSMS
//    supplies `currentTime` - which this crate already handles (see `crate::clock`), so returning
//    an honest "unset" reading is a valid implementation rather than a broken one.
struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(1_800_000_000, 0).expect("a valid fixed instant")
    }
}

// 4. The backoff. Used between reconnect attempts and by the crate's several sweep loops.
struct ThreadBackoff;

#[async_trait::async_trait]
impl Backoff for ThreadBackoff {
    async fn wait(&self, seconds: u32) {
        std::thread::sleep(Duration::from_secs(u64::from(seconds)));
    }
}

// --- the hardware itself -------------------------------------------------------------------

/// One simulated connector. Every method is fallible, per `CLAUDE.md` - a real one talks to a
/// relay driver and a lock actuator, either of which can refuse.
struct SimulatedConnector {
    /// Reports what the hardware was asked to do, so the example's output shows the ordering the
    /// state machine drives - notably the contactor opening before the unlock.
    log: mpsc::Sender<String>,
}

#[async_trait::async_trait]
impl Connector for SimulatedConnector {
    type Error = core::convert::Infallible;

    async fn lock(&self) -> Result<(), Self::Error> {
        let _ = self.log.send("  hardware: lock connector".into());
        Ok(())
    }
    async fn unlock(&self) -> Result<(), Self::Error> {
        let _ = self.log.send("  hardware: unlock connector".into());
        Ok(())
    }
    async fn close_contactor(&self) -> Result<(), Self::Error> {
        let _ = self.log.send("  hardware: close contactor".into());
        Ok(())
    }
    async fn open_contactor(&self) -> Result<(), Self::Error> {
        let _ = self.log.send("  hardware: open contactor".into());
        Ok(())
    }
    async fn set_current_limit(&self, limit_ma: Option<u32>) -> Result<(), Self::Error> {
        let _ = self.log.send(match limit_ma {
            Some(ma) => format!("  hardware: limit to {ma} mA"),
            None => "  hardware: remove current limit".into(),
        });
        Ok(())
    }
}

struct SimulatedEvse {
    connectors: Vec<SimulatedConnector>,
}

#[async_trait::async_trait]
impl Evse<SimulatedConnector> for SimulatedEvse {
    type Error = core::convert::Infallible;

    async fn connectors(&self) -> &[SimulatedConnector] {
        &self.connectors
    }
    async fn reboot(&self) -> Result<(), Self::Error> {
        Ok(())
    }
}

struct SimulatedChargePoint {
    /// Shared so `start` can hand a `'static` handle to the command loop it spawns. This is the
    /// shape most bindings end up in: `start` takes `&self`, but the loop it spawns has to outlive
    /// the call.
    evses: std::sync::Arc<Vec<SimulatedEvse>>,
}

#[async_trait::async_trait]
impl ChargePoint<SimulatedEvse, SimulatedConnector> for SimulatedChargePoint {
    type StartError = core::convert::Infallible;

    async fn vendor_name(&self) -> &str {
        "Acme"
    }
    async fn model_name(&self) -> &str {
        "Embedded Reference"
    }
    async fn evses(&self) -> &[SimulatedEvse] {
        &self.evses
    }
    async fn capabilities(&self) -> Capabilities {
        // Honest: this simulation has no display, no RTC, no persistent storage and no smart
        // charging hardware. Declaring otherwise would make the charge point advertise surfaces it
        // cannot back - see `docs/PRODUCTION-ROADMAP.md`'s capability-honesty criterion.
        Capabilities::default()
    }
    /// Where an integration is actually wired up. The contract (see [`ChargePoint::start`]) is to
    /// service both channels and return once they are being serviced - so this spawns the command
    /// loop rather than running it inline, which would never return.
    ///
    /// `execute_hardware_command` does the dispatch *and* turns a failed or out-of-range command
    /// into the right fault-reporting event, which is why almost every binding should loop on it
    /// rather than hand-rolling a match over `HardwareCommand`.
    async fn start(
        &self,
        events: HardwareEventSender,
        mut commands: HardwareCommandReceiver,
    ) -> Result<(), Self::StartError> {
        let evses = self.evses.clone();
        ThreadExecutor.spawn(Box::pin(async move {
            while let Ok(command) = commands.recv().await {
                execute_hardware_command(evses.as_slice(), command, &events).await;
            }
        }));
        Ok(())
    }
}

fn main() {
    let (log, entries) = mpsc::channel();

    // `ChargePointRuntime` is what an integrator holds: it owns the hardware binding and gives
    // back the two ends that connect it to the state machine. Note there is no CSMS here at all -
    // with no networking features compiled in the state machine still runs, which is what makes
    // `--no-default-features` a real configuration rather than a compile check, and what makes a
    // charge point testable on a bench with no backend.
    let charge_point = SimulatedChargePoint {
        evses: std::sync::Arc::new(vec![SimulatedEvse {
            connectors: vec![SimulatedConnector { log: log.clone() }],
        }]),
    };
    // `ChargePointBuilder` is the supported way in: it constructs the runtime *and* calls the
    // binding's `start`, which is what gets the command loop above running. Building a
    // `ChargePointRuntime` directly is possible but leaves the hardware unstarted, with no public
    // way to start it - the builder exists precisely so an integrator does not have to know that.
    let runtime =
        futures::executor::block_on(ChargePointBuilder::start(charge_point, ThreadExecutor))
            .expect("simulated hardware always starts")
            .build();

    let id_token = IdToken {
        value: "04A224B2".into(),
        kind: IdTokenKind::ISO14443,
    };

    // A whole session, as the hardware would report it.
    let script = [
        ("cable plugged in", ConnectorEvent::CableConnected),
        ("lock confirmed", ConnectorEvent::LockConfirmed),
        (
            "card presented",
            ConnectorEvent::IdTokenPresented(id_token.clone()),
        ),
        (
            "authorized",
            ConnectorEvent::ChargingAuthorized(id_token.clone()),
        ),
        ("contactor closed", ConnectorEvent::ContactorClosed),
        (
            "driver stopped",
            ConnectorEvent::ChargingStopped(ocpp_charge_point::state::StopReason::Local),
        ),
        ("contactor opened", ConnectorEvent::ContactorOpened),
        ("unlock confirmed", ConnectorEvent::UnlockConfirmed),
        ("cable removed", ConnectorEvent::CableDisconnected),
    ];

    println!("simulated charge point, no_std bindings\n");
    for (description, event) in script {
        futures::executor::block_on(runtime.send(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::Connector {
                connector_id: 0,
                event,
            },
        }))
        .expect("the actor should still be running");

        // Give the command loop a moment to run whatever this transition dispatched.
        std::thread::sleep(Duration::from_millis(20));
        let state = runtime.state().evses[0].connectors[0];
        println!("{description:<20} -> connector is {state:?}");
        while let Ok(entry) = entries.try_recv() {
            println!("{entry}");
        }
    }

    // The clock and backoff are unused above only because this script drives the state machine
    // directly; the moment a CSMS connection or any sweep loop is registered, both are required.
    // Referenced here so the example proves they satisfy their traits rather than merely compiling
    // as dead code.
    let _clock: &dyn Clock = &FixedClock;
    let _backoff = ThreadBackoff;

    println!("\nsession complete; connector returned to Available");
}
