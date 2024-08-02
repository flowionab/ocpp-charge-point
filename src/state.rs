use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use ocpp_client::rust_ocpp::v1_6;
use ocpp_client::rust_ocpp::v1_6::types::ChargePointStatus;
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct State {
    charger_state: Arc<Mutex<ChargerState>>,
    sender: broadcast::Sender<StateUpdate>,
}

impl State {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(16);
        Self {
            charger_state: Arc::new(Mutex::new(ChargerState::default())),
            sender
        }
    }

    pub fn update<F: FnOnce(MutexGuard<ChargerState>) -> R, R>(&self, callback: F) -> R {
        let lock = self.charger_state.lock().unwrap();
        let old_state: ChargerState = lock.clone();
        let result = callback(lock);
        let lock = self.charger_state.lock().unwrap();
        let new_state: ChargerState = lock.clone();
        if old_state != new_state {
            let _ = self.sender.send(StateUpdate {
                old: old_state,
                new: new_state
            });
        }

        result
    }

    pub fn read(&self) -> ChargerState {
        self.charger_state.lock().unwrap().clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<StateUpdate> {
        self.sender.subscribe()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ChargerState {
    Shutdown,
    Booting,
    Connected {
        heartbeat_interval: i64,
        outlet_states: BTreeMap<u64, OutletState>,
        pending_rfid_tag: Option<String>,
    },
    Maintenance
}

impl ChargerState {
    pub fn ocpp_1_6_charge_point_status(&self) -> v1_6::types::ChargePointStatus {
        match self {
            ChargerState::Shutdown => ChargePointStatus::Unavailable,
            ChargerState::Booting => ChargePointStatus::Unavailable,
            ChargerState::Connected { outlet_states, .. } => {
                if outlet_states.values().any(|state| state == &OutletState::Faulted) {
                    ChargePointStatus::Faulted
                } else {
                    ChargePointStatus::Available
                }
            },
            ChargerState::Maintenance => ChargePointStatus::Unavailable,
        }

    }
}

impl Default for ChargerState {
    fn default() -> Self {
        Self::Shutdown
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum OutletState {
    Available,
    Preparing,
    Faulted,
}

#[derive(Clone, Debug)]
pub struct StateUpdate {
    pub old: ChargerState,
    pub new: ChargerState,
}

impl OutletState {
    pub fn ocpp_1_6_status(&self) -> ChargePointStatus {
        match self {
            OutletState::Available => ChargePointStatus::Available,
            OutletState::Preparing => ChargePointStatus::Preparing,
            OutletState::Faulted => ChargePointStatus::Faulted,
        }
    }
}