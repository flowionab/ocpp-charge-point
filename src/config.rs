use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};
use crate::state::OutletState;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Config {
    pub ocpp_endpoint: String,
    pub ocpp_identity: String,
    pub ocpp_password: Option<String>,
    pub serial_number: Option<String>,
    pub vendor: String,
    pub firmware_version: Option<String>,
    pub model: String,
    pub iccid: Option<String>,
    pub imsi: Option<String>,
    pub meter_serial_number: Option<String>,
    pub meter_type: Option<String>,
    pub outlets: Vec<OutletConfig>,
}

impl Config {
    pub fn default_easee_home(endpoint: &str, identity: &str) -> Self {
        Self {
            ocpp_endpoint: endpoint.to_string(),
            ocpp_identity: identity.to_string(),
            ocpp_password: None,
            serial_number: Some(identity.to_string()),
            vendor: "easee".to_string(),
            firmware_version: None,
            model: "Easee Home".to_string(),
            iccid: None,
            imsi: None,
            meter_serial_number: None,
            meter_type: None,
            outlets: vec![OutletConfig {
                id: 1,
                max_current: 32.0,
            }],
        }
    }

    pub fn get_initial_outlet_states(&self) -> BTreeMap<u64, OutletState> {
        self.outlets.iter().map(|outlet| {
            (outlet.id, OutletState::Available)
        }).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct OutletConfig {
    pub id: u64,
    pub max_current: f64,
}