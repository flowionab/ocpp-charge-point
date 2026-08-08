//! A simulated charge point that talks to a real CSMS (`docs/PRODUCTION-ROADMAP.md` H2.6).
//!
//! Two audiences, deliberately the same program:
//!
//! - **An integrator's starting point.** It shows the whole std/tokio path - dial a CSMS, register
//!   every functional block, drive hardware events - with the hardware replaced by a simulation.
//!   Swap `SimulatedConnector` for something that drives real relays and the rest stands.
//! - **A soak-test subject** ([H4.1](../docs/PRODUCTION-ROADMAP.md)). It loops sessions forever at
//!   a configurable rate, so it can be left running against a CSMS for days while memory and
//!   behaviour are watched.
//!
//! Its counterpart, [`embedded_bindings`](embedded_bindings.rs), shows what the same charge point
//! costs with `--no-default-features` and no tokio.
//!
//! ```text
//! cargo run --example simulated_charge_point -- ws://localhost:9000/CP001
//! cargo run --example simulated_charge_point -- ws://localhost:9000/CP001 --sessions 5 --seconds 2
//! ```
//!
//! With no arguments it runs entirely offline, driving the state machine with no CSMS at all -
//! which is worth knowing on its own: a charge point whose backend is unreachable still charges
//! cars, and this is the smallest way to see that.

use std::sync::Arc;
use std::time::Duration;

use ocpp_charge_point::executor::TokioExecutor;
use ocpp_charge_point::hardware::{
    Capabilities, ChargePoint, Connector, Evse, HardwareCommandReceiver, HardwareEventSender,
    execute_hardware_command,
};
use ocpp_charge_point::provisioning::TokioBackoff;
use ocpp_charge_point::state::{ChargePointEvent, ConnectorEvent, EvseEvent, IdToken, IdTokenKind};
use ocpp_charge_point::{ChargePointBuilder, ChargePointRuntime};

struct SimulatedConnector;

#[async_trait::async_trait]
impl Connector for SimulatedConnector {
    type Error = core::convert::Infallible;

    async fn lock(&self) -> Result<(), Self::Error> {
        tracing::info!("hardware: lock");
        Ok(())
    }
    async fn unlock(&self) -> Result<(), Self::Error> {
        tracing::info!("hardware: unlock");
        Ok(())
    }
    async fn close_contactor(&self) -> Result<(), Self::Error> {
        tracing::info!("hardware: contactor closed");
        Ok(())
    }
    async fn open_contactor(&self) -> Result<(), Self::Error> {
        tracing::info!("hardware: contactor open");
        Ok(())
    }
    async fn set_current_limit(&self, limit_ma: Option<u32>) -> Result<(), Self::Error> {
        tracing::info!(?limit_ma, "hardware: current limit");
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
        tracing::warn!("hardware: reboot requested");
        Ok(())
    }
}

struct SimulatedChargePoint {
    evses: Arc<Vec<SimulatedEvse>>,
}

#[async_trait::async_trait]
impl ChargePoint<SimulatedEvse, SimulatedConnector> for SimulatedChargePoint {
    type StartError = core::convert::Infallible;

    async fn vendor_name(&self) -> &str {
        "Acme"
    }
    async fn model_name(&self) -> &str {
        "Simulator"
    }
    async fn evses(&self) -> &[SimulatedEvse] {
        &self.evses
    }
    async fn capabilities(&self) -> Capabilities {
        // Only what a simulation can actually back. Smart charging is claimed because
        // `set_current_limit` above really is implemented; a display or persistent storage would
        // be a claim with nothing behind it.
        Capabilities::default().with_smart_charging(true)
    }
    async fn start(
        &self,
        events: HardwareEventSender,
        mut commands: HardwareCommandReceiver,
    ) -> Result<(), Self::StartError> {
        let evses = self.evses.clone();
        tokio::spawn(async move {
            while let Ok(command) = commands.recv().await {
                execute_hardware_command(evses.as_slice(), command, &events).await;
            }
        });
        Ok(())
    }
}

fn charge_point(connectors: usize) -> SimulatedChargePoint {
    SimulatedChargePoint {
        evses: Arc::new(vec![SimulatedEvse {
            connectors: (0..connectors).map(|_| SimulatedConnector).collect(),
        }]),
    }
}

/// One complete charging session, driven as the hardware would report it.
async fn one_session(runtime: &ChargePointRuntime<SimulatedChargePoint>, pause: Duration) {
    let id_token = IdToken {
        value: "04A224B2".into(),
        kind: IdTokenKind::ISO14443,
    };
    for event in [
        ConnectorEvent::CableConnected,
        ConnectorEvent::LockConfirmed,
        ConnectorEvent::IdTokenPresented(id_token.clone()),
        ConnectorEvent::ChargingAuthorized(id_token.clone()),
        ConnectorEvent::ContactorClosed,
        ConnectorEvent::MeterValueSampled(ocpp_charge_point::state::MeterSample {
            energy_wh: 2_400,
            power_w: Some(7_400),
            ..Default::default()
        }),
        ConnectorEvent::ChargingStopped(ocpp_charge_point::state::StopReason::Local),
        ConnectorEvent::ContactorOpened,
        ConnectorEvent::UnlockConfirmed,
        ConnectorEvent::CableDisconnected,
    ] {
        if runtime
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event,
                },
            })
            .await
            .is_err()
        {
            // The actor refusing an event is worth surfacing rather than ignoring - under soak it
            // is how mailbox backpressure (G4.5) would first show itself.
            tracing::error!("the actor refused an event; stopping this session");
            return;
        }
        tokio::time::sleep(pause).await;
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                // The crate logs whole state snapshots at INFO, which is invaluable when debugging one
                // transition and unreadable in a soak run. Default to the example's own output and
                // let `RUST_LOG=ocpp_charge_point=info` opt back in.
                .unwrap_or_else(|_| {
                    "warn,simulated_charge_point=info"
                        .parse()
                        .expect("a valid default filter")
                }),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let address = args.first().filter(|a| !a.starts_with("--")).cloned();
    let sessions = numeric_arg(&args, "--sessions");
    let pause = Duration::from_millis(numeric_arg(&args, "--seconds").unwrap_or(1) * 1000 / 10);

    let runtime = match &address {
        Some(address) => {
            tracing::info!(%address, "connecting to the CSMS");
            ocpp_charge_point::connect_and_setup(
                charge_point(1),
                address,
                None,
                None,
                None,
                TokioExecutor,
                TokioBackoff,
            )
            .await
            .expect("failed to connect - is a CSMS listening there?")
        }
        None => {
            // No backend at all. Worth demonstrating: the state machine is the charge point, and
            // it charges cars whether or not anything is listening.
            tracing::info!("no CSMS address given; running the state machine offline");
            ChargePointBuilder::start(charge_point(1), TokioExecutor)
                .await
                .expect("simulated hardware always starts")
                .build()
        }
    };

    let mut completed = 0u64;
    loop {
        one_session(&runtime, pause).await;
        completed += 1;
        tracing::info!(completed, "session complete");
        if sessions.is_some_and(|limit| completed >= limit) {
            break;
        }
    }
    tracing::info!(completed, "done");
}

/// Reads `--name <number>` out of the argument list, or `None`.
fn numeric_arg(args: &[String], name: &str) -> Option<u64> {
    let index = args.iter().position(|arg| arg == name)?;
    args.get(index + 1)?.parse().ok()
}
