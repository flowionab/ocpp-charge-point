use alloc::vec;
use alloc::vec::Vec;

use crate::state::{
    ConnectorState, EvseEvent, ExternalChargingLimit, MeterSample, Reservation, Tariff, Transaction,
};

/// The internal state of one EVSE (Electric Vehicle Supply Equipment): its own availability/
/// fault status, plus one entry per connector it owns in each of the parallel `Vec`s below.
#[derive(Debug, Clone, PartialEq)]
pub struct EvseState {
    /// This EVSE's own availability/fault status, independent of any individual connector.
    pub status: EvseStatus,
    /// An external charging limit currently in force on this EVSE (from something other than a
    /// CSMS charging profile - typically a local energy-management system), reported via
    /// `NotifyChargingLimit`/`ClearedChargingLimit`. `None` when nothing external is currently
    /// limiting this EVSE. See [`ExternalChargingLimit`]'s docs for why this is not a
    /// [`crate::state::ChargingProfile`] and does not feed the composite-schedule projection.
    pub external_charging_limit: Option<ExternalChargingLimit>,
    /// This EVSE's connectors, indexed by `connector_id` as used throughout this crate and OCPP.
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
    /// The driver tariff assigned to each connector's active transaction (OCPP 2.1
    /// `ChangeTransactionTariff`), indexed the same as `connectors`. `None` when the CSMS hasn't
    /// assigned one, or the transaction it was assigned to has since ended - mirrors
    /// [`Self::running_costs`] exactly, for the same reason: a new transaction on the same
    /// connector must never inherit a previous driver's tariff. See `docs/ROADMAP.md` §9 and
    /// `docs/PRODUCTION-ROADMAP.md` B7.1.
    pub transaction_tariffs: Vec<Option<Tariff>>,
    /// The most recent meter reading each connector's hardware pushed in, indexed the same as
    /// `connectors` - recorded whether or not a transaction is running on it, unlike
    /// [`Transaction::last_meter_sample`](crate::state::Transaction::last_meter_sample), which is
    /// only updated while one is actively charging.
    ///
    /// This is what standalone `MeterValues` reports (`docs/ROADMAP.md` §10,
    /// `docs/PRODUCTION-ROADMAP.md` B1.1): OCPP's clock-aligned data is due on the wall clock
    /// whether or not anything is charging, so it needs a reading that outlives the transaction
    /// that happened to produce it. `None` until the hardware has pushed its first
    /// [`ConnectorEvent::MeterValueSampled`](crate::state::ConnectorEvent::MeterValueSampled) for
    /// that connector.
    pub latest_meter_samples: Vec<Option<MeterSample>>,
    /// The current limit most recently *requested* of each connector's hardware, in milliamps,
    /// indexed the same as `connectors`. `None` means no installed charging profile imposes a
    /// limit on that connector - see [`ConnectorEvent::CurrentLimitComputed`](crate::state::ConnectorEvent::CurrentLimitComputed)
    /// and `docs/ROADMAP.md` §11.
    ///
    /// This is what the projection compares against to decide whether hardware needs telling at
    /// all, so it is deliberately the *requested* value rather than the confirmed one: a
    /// connector whose hardware failed to apply a limit is driven into `Faulted` by the usual
    /// hardware-error path, and re-requesting the same failing limit on every state change would
    /// add nothing.
    pub charging_limits: Vec<Option<u32>>,
    /// The current limit each connector's hardware most recently *confirmed* applying, in
    /// milliamps, indexed the same as `connectors` - see
    /// [`ConnectorEvent::CurrentLimitConfirmed`](crate::state::ConnectorEvent::CurrentLimitConfirmed).
    /// Lags `charging_limits` by one hardware round-trip, and is what a CSMS-facing report should
    /// quote when it wants to say what the connector is *actually* limited to.
    pub applied_charging_limits: Vec<Option<u32>>,
}

/// One EVSE's own availability/fault status, independent of any individual connector's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvseStatus {
    /// The EVSE is available for use.
    Available,
    /// The EVSE has been made unavailable (OCPP `ChangeAvailability`), or a fault on it has
    /// cleared and is awaiting an explicit `SetAvailable` to resume.
    Unavailable,
    /// A hardware fault affecting this whole EVSE is active.
    Faulted,
}

impl EvseState {
    /// A fresh, available EVSE with `connector_count` connectors, each `Available` with no
    /// transaction/reservation/running cost.
    pub fn new(connector_count: usize) -> Self {
        Self {
            status: EvseStatus::Available,
            external_charging_limit: None,
            connectors: vec![ConnectorState::Available; connector_count],
            transactions: vec![None; connector_count],
            reservations: vec![None; connector_count],
            running_costs: vec![None; connector_count],
            transaction_tariffs: vec![None; connector_count],
            latest_meter_samples: vec![None; connector_count],
            charging_limits: vec![None; connector_count],
            applied_charging_limits: vec![None; connector_count],
        }
    }

    /// Applies an event addressed directly at this EVSE's own `status` (not one of its
    /// connectors - see [`EvseEvent::Connector`], handled instead by
    /// [`crate::state::ChargePointState::apply`]). Returns whether `status` actually changed.
    pub fn apply(&mut self, event: EvseEvent) -> bool {
        match event {
            EvseEvent::SetAvailable => set_if_changed(&mut self.status, EvseStatus::Available),
            EvseEvent::SetUnavailable => set_if_changed(&mut self.status, EvseStatus::Unavailable),
            EvseEvent::FaultDetected => set_if_changed(&mut self.status, EvseStatus::Faulted),
            EvseEvent::FaultCleared => set_if_changed(&mut self.status, EvseStatus::Unavailable),
            // `Connector { .. }` is dispatched by `ChargePointState::apply_connector_event`, and
            // `EVChargingNeedsReported`/`EVChargingScheduleReported` are handled directly by
            // `ChargePointState::apply` (they need to push a
            // `ChargePointEffect::SmartChargingNotification`, which this method, returning only a
            // `bool`, has no way to do) - neither reaches here.
            EvseEvent::Connector { .. }
            | EvseEvent::EVChargingNeedsReported(_)
            | EvseEvent::EVChargingScheduleReported(_) => false,
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
