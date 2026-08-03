use ocpp_charge_point::availability::StatusNotifier;
use ocpp_charge_point::hardware::{ChargePoint, Connector, Evse};
use ocpp_charge_point::provisioning::{BootNotificationOutcome, BootNotifier, HeartbeatSender};
use ocpp_charge_point::setup;
use ocpp_charge_point::state::{
    ChargePointEvent, ConnectorEvent, ConnectorStatus, EvseEvent, RegistrationStatus,
};

/// A stand-in for a real CSMS connection. Real deployments pass an `ocpp-client` version
/// client (e.g. `ocpp_client::connect_2_1`) instead, which already implements `BootNotifier`,
/// `HeartbeatSender`, and `StatusNotifier`.
#[derive(Clone)]
struct AlwaysAcceptBootNotifier;

#[async_trait::async_trait]
impl BootNotifier for AlwaysAcceptBootNotifier {
    type Error = core::convert::Infallible;

    async fn notify_boot(
        &self,
        _vendor_name: &str,
        _model_name: &str,
    ) -> Result<BootNotificationOutcome, Self::Error> {
        Ok(BootNotificationOutcome {
            status: RegistrationStatus::Accepted,
            interval_secs: 300,
        })
    }
}

#[async_trait::async_trait]
impl HeartbeatSender for AlwaysAcceptBootNotifier {
    type Error = core::convert::Infallible;

    async fn send_heartbeat(&self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl StatusNotifier for AlwaysAcceptBootNotifier {
    type Error = core::convert::Infallible;

    async fn notify_status(
        &self,
        _evse_id: usize,
        _connector_id: usize,
        _status: ConnectorStatus,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

struct SampleChargePoint {
    evses: [SampleEvse; 2],
}

struct SampleEvse {
    connectors: [SampleConnector; 2],
}

struct SampleConnector;

impl SampleEvse {
    pub fn new() -> Self {
        Self {
            connectors: [SampleConnector, SampleConnector],
        }
    }
}

impl SampleChargePoint {
    pub fn new() -> Self {
        Self {
            evses: [SampleEvse::new(), SampleEvse::new()],
        }
    }
}

#[async_trait::async_trait]
impl ChargePoint<SampleEvse, SampleConnector> for SampleChargePoint {
    type StartError = core::convert::Infallible;

    async fn vendor_name(&self) -> &str {
        "Test Vendor"
    }
    async fn model_name(&self) -> &str {
        "Test Model"
    }

    async fn evses(&self) -> &[SampleEvse] {
        return &self.evses;
    }

    async fn start(
        &self,
        _events: ocpp_charge_point::hardware::HardwareEventSender,
        _commands: ocpp_charge_point::hardware::HardwareCommandReceiver,
    ) -> Result<(), Self::StartError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl Evse<SampleConnector> for SampleEvse {
    async fn connectors(&self) -> &[SampleConnector] {
        return &self.connectors;
    }
}

#[async_trait::async_trait]
impl Connector for SampleConnector {
    type Error = core::convert::Infallible;

    async fn lock(&self) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn unlock(&self) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn close_contactor(&self) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn open_contactor(&self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let runtime = setup(SampleChargePoint::new(), AlwaysAcceptBootNotifier).await?;
    runtime
        .send(ChargePointEvent::Evse {
            evse_id: 1,
            event: EvseEvent::Connector {
                connector_id: 1,
                event: ConnectorEvent::CableConnected,
            },
        })
        .await
        .unwrap();

    Ok(())
}
