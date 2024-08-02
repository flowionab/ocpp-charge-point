use std::error::Error;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use tokio::time::sleep;
use std::time::Duration;
use chrono::{DateTime, Utc};
use log::{info, warn};
use ocpp_client::Client;
use ocpp_client::rust_ocpp::v1_6::messages::boot_notification::BootNotificationRequest;
use ocpp_client::rust_ocpp::v1_6::messages::heart_beat::{HeartbeatRequest, HeartbeatResponse};
use ocpp_client::rust_ocpp::v1_6::types::RegistrationStatus;
use tokio::sync::Mutex;
use crate::config::Config;
use crate::network_bridge::NetworkBridge;
use crate::state::{ChargerState, OutletState, State};

#[derive(Clone)]
pub struct Charger<N: NetworkBridge> {
    config: Arc<Config>,
    state: State,
    bridge: N
}

impl<N: NetworkBridge> Charger<N> {
    pub fn new(config: Arc<Config>, state: State, bridge: N) -> Self {
        Self {
            config,
            state,
            bridge
        }
    }

    pub async fn disconnect(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.bridge.disconnect().await
    }

    pub fn startup(&self) {
        self.state.update(|mut state| {
            *state = ChargerState::Booting;
        });
        info!("Booting up charger...");
    }



    pub fn car_connected(&self, outlet_id: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("Car connected");
        self.state.update(|mut state| {
            if let ChargerState::Connected { ref mut outlet_states, .. } = state.deref_mut() {
                outlet_states.insert(outlet_id, OutletState::Preparing);
            } else {
                return Err("Charger is not connected to server".into())
            }
            Ok(())
        })
    }

    pub fn blip_rfid_tag(&self, tag: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("Blipping RFID tag '{}'", tag);
        self.state.update(|mut state| {
            if let ChargerState::Connected { ref mut pending_rfid_tag, .. } = state.deref_mut() {
                *pending_rfid_tag = Some(tag.to_string())
            } else {
                return Err("Charger is not connected to server".into())
            }
            Ok(())
        })
    }
}