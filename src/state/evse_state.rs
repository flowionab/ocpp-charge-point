use alloc::vec;
use alloc::vec::Vec;

use crate::state::{ConnectorState, EvseEvent, Reservation, Transaction};

#[derive(Debug, Clone, PartialEq)]
pub struct EvseState {
    pub status: EvseStatus,
    pub connectors: Vec<ConnectorState>,
    /// The active transaction for each connector, indexed the same as `connectors`. `None`
    /// when that connector has no transaction in progress.
    pub transactions: Vec<Option<Transaction>>,
    /// The active reservation for each connector, indexed the same as `connectors`. `None` when
    /// that connector isn't reserved. See `docs/ROADMAP.md` §8.
    pub reservations: Vec<Option<Reservation>>,
    /// The most recent running cost the CSMS reported (OCPP `CostUpdated`) for each connector's
    /// active transaction, indexed the same as `connectors`. `None` when the CSMS hasn't sent
    /// one, or the transaction it was reported for has since ended - a new transaction starts
    /// with no carried-over cost from a previous one on the same connector. See
    /// `docs/ROADMAP.md` §9.
    pub running_costs: Vec<Option<f64>>,
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
            transactions: vec![None; connector_count],
            reservations: vec![None; connector_count],
            running_costs: vec![None; connector_count],
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
