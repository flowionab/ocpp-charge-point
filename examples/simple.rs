use std::sync::Arc;
use log::LevelFilter;
use simplelog::{SimpleLogger};
use tokio::signal;
use tokio::time::sleep;
use ocpp_charger::{Charger, Config, OcppNetworkBridge, OutletConfig, State};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _ = SimpleLogger::init(LevelFilter::Info, simplelog::Config::default());

    let config = Arc::new(Config {
        ocpp_endpoint: "wss://ocpp.flowion.app/easee".to_string(),
        ocpp_identity: "EHP2MQ3l".to_string(),
        serial_number: Some("EHP2MQ3l".to_string()),
        model: "Home".to_string(),
        vendor: "Easee".to_string(),
        outlets: vec![
            OutletConfig {
                id: 1,
                max_current: 32.0,
            },
            OutletConfig {
                id: 2,
                max_current: 32.0,
            }
        ],
        ..Default::default()
    });

    let state = State::new();

    let network = OcppNetworkBridge::connect(Arc::clone(&config), state.clone()).await?;

    let charger = Charger::new(config, state, network);

    charger.startup();

    sleep(std::time::Duration::from_secs(1)).await;

    charger.car_connected(1)?;

    sleep(std::time::Duration::from_secs(1)).await;

    charger.blip_rfid_tag("1234567890")?;


    signal::ctrl_c().await?;

    charger.disconnect().await?;

    Ok(())
}