use std::collections::BTreeMap;
use std::ops::DerefMut;
use std::sync::Arc;
use std::time::Duration;
use chrono::{DateTime, Utc};
use log::{info, warn};
use ocpp_client::Client;
use ocpp_client::rust_ocpp::v1_6::messages::authorize::AuthorizeRequest;
use ocpp_client::rust_ocpp::v1_6::messages::boot_notification::BootNotificationRequest;
use ocpp_client::rust_ocpp::v1_6::messages::heart_beat::HeartbeatRequest;
use ocpp_client::rust_ocpp::v1_6::messages::status_notification::StatusNotificationRequest;
use ocpp_client::rust_ocpp::v1_6::types::{AuthorizationStatus, ChargePointStatus, RegistrationStatus};
use tokio::sync::Mutex;
use tokio::time::sleep;
use crate::Config;
use crate::network_bridge::NetworkBridge;
use crate::state::{ChargerState, OutletState, State, StateUpdate};

#[derive(Clone)]
pub struct OcppNetworkBridge {
    client: Client,
    state: State,
    last_time_message_sent: Arc<Mutex<DateTime<Utc>>>,
    config: Arc<Config>
}

impl OcppNetworkBridge {
    pub async fn connect(config: Arc<Config>, state: State) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let endpoint = format!("{}/{}", config.ocpp_endpoint, config.ocpp_identity);
        info!("Connecting to {}...", endpoint);
        let client = ocpp_client::connect(&endpoint).await?;

        match &client { Client::OCPP1_6(client) => {
            info!("Established a OCPP 1.6 connection");
            client.inspect_raw_message(|action, message| async move {
                info!("Got message '{}' with payload '{:?}'", action, message);
            }).await;

            client.on_get_configuration(|message, _| async move {
                info!("Got get_configuration message: {:?}", message);
                Ok(Default::default())
            }).await
        }
            Client::OCPP2_0_1(_) => {}
        }

        let mut recv = state.subscribe();

        let result = Self {
            last_time_message_sent: Arc::new(Mutex::new(Utc::now())),
            client,
            state,
            config
        };

        let s = result.clone();
        tokio::spawn(async move {
            while let Ok(StateUpdate {old, new}) = recv.recv().await {
                if new == ChargerState::Booting {
                    if let Err(err) = s.perform_startup_handshake().await {
                        warn!("Failed to perform startup handshake: {:?}", err);
                    }
                }

                if let ChargerState::Connected { ref outlet_states, ref pending_rfid_tag, .. } = new {
                    if old == ChargerState::Booting {
                        // Let's send initial outlet states
                        if let Err(err) = s.send_initial_status_updates(&outlet_states).await {
                            warn!("Failed to send initial status updates: {:?}", err);
                        }
                    }


                    if let ChargerState::Connected { outlet_states : ref old_outlet_state, pending_rfid_tag: ref old_pending_rfid_tag, .. } = old {
                        for (outlet_id, state) in outlet_states.iter() {
                            if let Some(old_state) = old_outlet_state.get(outlet_id) {
                                if state != old_state {
                                    info!("Outlet {} changed state to {:?}", outlet_id, state);
                                    if let Err(err) = s.send_outlet_status_update(*outlet_id, state).await {
                                        warn!("Failed to send status updates: {:?}", err);
                                    }
                                }
                            }
                        }

                        if new.ocpp_1_6_charge_point_status() != old.ocpp_1_6_charge_point_status() {
                            info!("Charge point status changed to {:?}", new.ocpp_1_6_charge_point_status());
                            if let Err(err) = s.send_charge_point_status_update(new.ocpp_1_6_charge_point_status()).await {
                                warn!("Failed to send status updates: {:?}", err);
                            }
                        }

                        if pending_rfid_tag != old_pending_rfid_tag {
                            if let Some(tag) = pending_rfid_tag {
                                info!("Pending RFID tag changed to {:?}", tag);
                                if let Err(err) = s.authorize_tag(tag).await {
                                    warn!("Failed to authorize tag: {:?}", err);
                                }
                            }
                        }
                    }
                }


            }
        });

        result.start_heartbeat_thread();

        Ok(result)
    }

    fn start_heartbeat_thread(&self) {
        let s = self.clone();
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(1)).await;
                let should_send = {
                    let state = s.state.read();
                    let lock2 = s.last_time_message_sent.lock().await;
                    if let ChargerState::Connected { heartbeat_interval, .. } = state {
                        (Utc::now() - *lock2).num_seconds() > heartbeat_interval
                    } else {
                        false
                    }
                };
                if should_send {
                    info!("Sending heartbeat");
                    match &s.client {
                        Client::OCPP1_6(client) => {
                            let request = HeartbeatRequest {};
                            match client.send_heartbeat(request).await {
                                Ok(_response) => {
                                    let mut lock = s.last_time_message_sent.lock().await;
                                    *lock = Utc::now();
                                }
                                Err(err) => {
                                    warn!("Failed to send heartbeat: {:?}", err);
                                }
                            };
                        }
                        Client::OCPP2_0_1(_) => {}
                    }
                }
            }
        });
    }

    async fn authorize_tag(&self, tag: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match &self.client {
            Client::OCPP1_6(client) => {
                let request = AuthorizeRequest {
                    id_tag: tag.to_owned()
                };
                info!("Authorizing tag '{}'", tag);
                let result = client.send_authorize(request).await??;

                if result.id_tag_info.status == AuthorizationStatus::Accepted {
                    info!("Tag '{}' authorized", tag);
                    self.state.update(|mut state| {
                        if let ChargerState::Connected { ref mut pending_rfid_tag, .. } = state.deref_mut() {
                            *pending_rfid_tag = None;
                        }
                    });
                } else {
                    info!("Tag '{}' not authorized", tag);
                }
            }
            Client::OCPP2_0_1(_) => {}
        }
        Ok(())
    }

    async fn send_outlet_status_update(&self, outlet_id: u64, state: &OutletState) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match &self.client {
            Client::OCPP1_6(client) => {
                let request = StatusNotificationRequest {
                    connector_id: outlet_id as u32,
                    error_code: Default::default(),
                    info: None,
                    status: state.ocpp_1_6_status(),
                    timestamp: Some(Utc::now()),
                    vendor_id: None,
                    vendor_error_code: None,
                };
                info!("Sending status update for outlet {} with status: {:?}", request.connector_id, request.status);
                client.send_status_notification(request).await??;
            }
            Client::OCPP2_0_1(_) => {}
        }
        Ok(())
    }

    async fn send_charge_point_status_update(&self, status: ChargePointStatus) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match &self.client {
            Client::OCPP1_6(client) => {
                let request = StatusNotificationRequest {
                    connector_id: 0,
                    error_code: Default::default(),
                    info: None,
                    status,
                    timestamp: Some(Utc::now()),
                    vendor_id: None,
                    vendor_error_code: None,
                };
                info!("Sending status update for charge point with status: {:?}", request.status);
                client.send_status_notification(request).await??;
            }
            Client::OCPP2_0_1(_) => {}
        }
        Ok(())
    }

    async fn send_initial_status_updates(&self, outlets: &BTreeMap<u64, OutletState>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match &self.client {
            Client::OCPP1_6(client) => {
                let request = StatusNotificationRequest {
                    connector_id: 0,
                    error_code: Default::default(),
                    info: None,
                    status: ChargePointStatus::Available,
                    timestamp: Some(Utc::now()),
                    vendor_id: None,
                    vendor_error_code: None,
                };
                info!("Sending status update for charge point with status: {:?}", request.status);
                client.send_status_notification(request).await??;

                for (id, state) in outlets {
                    let request = StatusNotificationRequest {
                        connector_id: *id as u32,
                        error_code: Default::default(),
                        info: None,
                        status: ChargePointStatus::Available,
                        timestamp: Some(Utc::now()),
                        vendor_id: None,
                        vendor_error_code: None,
                    };
                    info!("Sending status update for outlet {} with status: {:?}", request.connector_id, request.status);
                    client.send_status_notification(request).await??;
                }
            }
            Client::OCPP2_0_1(_) => {}
        }
        Ok(())
    }

    async fn perform_startup_handshake(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("Handshaking with server...");
        match &self.client {
            Client::OCPP1_6(client) => {
                loop {
                    let request = BootNotificationRequest {
                        charge_box_serial_number: self.config.serial_number.to_owned(),
                        charge_point_model: self.config.model.to_string(),
                        charge_point_serial_number: self.config.serial_number.to_owned(),
                        charge_point_vendor: self.config.vendor.to_string(),
                        firmware_version: self.config.firmware_version.to_owned(),
                        iccid: self.config.iccid.to_owned(),
                        imsi: self.config.imsi.to_owned(),
                        meter_serial_number: self.config.meter_serial_number.to_owned(),
                        meter_type: self.config.meter_type.to_owned(),
                    };
                    let response = client.send_boot_notification(request).await??;

                    match response.status {
                        RegistrationStatus::Accepted => {
                            info!("Server handshake completed");
                            self.state.update(|mut state| {
                                *state = ChargerState::Connected {
                                    heartbeat_interval: response.interval as i64,
                                    outlet_states: self.config.get_initial_outlet_states(),
                                    pending_rfid_tag: None
                                };
                            });
                            return Ok(())
                        }
                        RegistrationStatus::Pending => {
                            info!("Server set the handshake to pending, trying again in {} seconds...", response.interval);
                            sleep(Duration::from_secs(response.interval as u64)).await;
                            continue
                        }
                        RegistrationStatus::Rejected => {
                            info!("Server rejected the handshake, trying again in {} seconds...", response.interval);
                            sleep(Duration::from_secs(response.interval as u64)).await;
                            continue
                        }
                    }
                }
            }
            Client::OCPP2_0_1(_) => {
                Ok(())
            }
        }

    }
}

#[async_trait::async_trait]
impl NetworkBridge for OcppNetworkBridge {

    async fn disconnect(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match &self.client { Client::OCPP1_6(client) => {
            client.disconnect().await?;
        }
            Client::OCPP2_0_1(_) => {}
        }
        info!("Charger disconnected from CSMS");
        Ok(())
    }
}