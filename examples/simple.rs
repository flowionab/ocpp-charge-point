use ocpp_charge_point::hardware::{ChargePoint, Connector, Evse};
use ocpp_charge_point::setup;

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

    let _runtime = setup(SampleChargePoint::new()).await?;

    Ok(())
}
