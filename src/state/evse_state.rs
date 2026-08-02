use alloc::vec;
use alloc::vec::Vec;

use crate::state::{ConnectorState, EvseEvent};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvseState {
    pub status: EvseStatus,
    pub connectors: Vec<ConnectorState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvseStatus {
    Available,
    Unavailable,
    Faulted,
}

impl EvseState {
    pub fn new(connector_count: usize) -> Self {
        Self {
            status: EvseStatus::Available,
            connectors: vec![ConnectorState::Available; connector_count],
        }
    }

    pub fn apply(&mut self, event: EvseEvent) -> bool {
        match event {
            EvseEvent::SetAvailable => set_if_changed(&mut self.status, EvseStatus::Available),
            EvseEvent::SetUnavailable => set_if_changed(&mut self.status, EvseStatus::Unavailable),
            EvseEvent::FaultDetected => set_if_changed(&mut self.status, EvseStatus::Faulted),
            EvseEvent::FaultCleared => set_if_changed(&mut self.status, EvseStatus::Unavailable),
            EvseEvent::Connector { .. } => false,
        }
    }
}

fn set_if_changed<T: PartialEq>(current: &mut T, next: T) -> bool {
    if *current == next {
        false
    } else {
        *current = next;
        true
    }
}
