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
    /// [`crate::state::ChargingProfile`].
    pub external_charging_limit: Option<ExternalChargingLimit>,
    /// Locally generated capacity currently available to this EVSE - solar, a site battery -
    /// pushed by the same integrator path as [`Self::external_charging_limit`] and reported the
    /// same way, but with `isLocalGeneration = true` (K27.FR.03).
    ///
    /// **A separate slot rather than a flag on the one above**, because K27.FR.05 has the station
    /// hold and report both at once: an EMS that caps the site at 6 kW *and* 2 kW of sun are two
    /// facts, and a single slot would have the second evict the first. They also compose
    /// oppositely - the limit caps, the generation adds - so collapsing them would be exactly the
    /// conflation CV17 exists to undo.
    pub local_generation_limit: Option<ExternalChargingLimit>,
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
    /// This connector's own running-cost calculation, indexed the same as `connectors` (CV8,
    /// I07/I08/I11/I12) - what [`crate::pricing::TransactionCost`] has priced from the meter
    /// samples this connector's active transaction has seen, using whichever tariff currently
    /// prices it (the driver tariff in [`Self::transaction_tariffs`] if one is assigned,
    /// otherwise the applicable entry from [`crate::state::TariffStore`]). `None` when nothing
    /// has priced this transaction yet - no tariff has ever applied to it - and cleared on the
    /// same Started/Ended boundary [`Self::running_costs`] and [`Self::transaction_tariffs`]
    /// are, for the same reason: a new session on this connector must start unpriced, not
    /// inherit a total from whoever charged here last.
    ///
    /// Distinct from [`Self::running_costs`]: that field is what the CSMS *told* this station
    /// (`CostUpdated`); this is what the station worked out for itself. Advanced by
    /// `crate::tariff::advance_running_cost`, which needs a [`crate::clock::Clock`] the state
    /// machine deliberately does not have (see `crate::clock`'s docs), so - unlike most state -
    /// this field is never written from inside [`crate::state::ChargePointState::apply`] itself
    /// on a raw hardware event; it only ever receives the value an adapter already computed.
    pub running_cost: Vec<Option<crate::pricing::TransactionCost>>,
    /// The grand total of [`Self::running_cost`], indexed the same as `connectors` - what the
    /// driver would be billed if the session ended now, with the tariff's own `minCost`/`maxCost`
    /// already applied (I12.FR.17).
    ///
    /// Stored rather than derived because deriving it needs the tariff pricing the transaction,
    /// and resolving that needs a clock the state machine does not have - see
    /// [`ConnectorEvent::RunningCostAdvanced`](crate::state::ConnectorEvent::RunningCostAdvanced).
    /// It exists so a `maxCost` transaction limit can be enforced against the station's *own*
    /// figure where one is available, which is what **E16.FR.16** requires; [`Self::running_costs`]
    /// is the CSMS's figure and E16.FR.15's fallback for a station that prices nothing locally.
    pub running_cost_totals: Vec<Option<f64>>,
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
    /// An authorized start this connector is waiting for a cable to fulfil - **F02, "Remote Start
    /// Transaction - Remote Start First"** and **E03, "Start Transaction - IdToken First"**
    /// (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV7 and CV2.3).
    ///
    /// Both use cases have the same premise: the authorization arrives *before* the driver plugs
    /// in, so the station accepts it, holds it, and starts the transaction when the cable latches.
    /// They differ only in where it came from - a CSMS `RequestStartTransaction` (F02) or a card
    /// presented at the reader and accepted by the CSMS (E03) - and that difference is carried by
    /// [`PendingRemoteStart::remote_start_id`], not by a second slot. One slot means one dispatch
    /// path, one timeout, and no way for the two to disagree about which connector is spoken for.
    ///
    /// Cleared when the cable arrives and the start is dispatched, when the connector *returns* to
    /// `Available` without one, or when `TxCtrlr.EVConnectionTimeOut` expires - see
    /// [`crate::remote_control::run_pending_remote_start_timeouts`], which owns the clock this
    /// state deliberately does not (F02.FR.07/.08, E03.FR.15).
    pub pending_remote_starts: Vec<Option<PendingRemoteStart>>,
    /// The reservation each connector *honoured*, indexed the same as `connectors` - the id of
    /// the reservation that was in force when the cable arrived, kept until the connector returns
    /// to `Available` (CV6).
    ///
    /// Separate from `reservations` because the two have different lifetimes on purpose: a
    /// reservation ends the moment it is honoured (the cable arriving is the reservation doing its
    /// job, so `reservations` clears then), but the transaction that *results* does not exist yet
    /// and still has to report which reservation it consumed (F02.FR.06, H03). Holding the id here
    /// bridges that gap without keeping a spent reservation alive in the store, where it would
    /// look reservable.
    pub honoured_reservations: Vec<Option<i64>>,
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
            local_generation_limit: None,
            connectors: vec![ConnectorState::Available; connector_count],
            transactions: vec![None; connector_count],
            reservations: vec![None; connector_count],
            running_costs: vec![None; connector_count],
            transaction_tariffs: vec![None; connector_count],
            running_cost: vec![None; connector_count],
            running_cost_totals: vec![None; connector_count],
            latest_meter_samples: vec![None; connector_count],
            charging_limits: vec![None; connector_count],
            honoured_reservations: vec![None; connector_count],
            pending_remote_starts: vec![None; connector_count],
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

/// A `RequestStartTransaction` accepted before the cable arrived - see
/// [`EvseState::pending_remote_starts`] (F02, CV7).
#[derive(Debug, Clone, PartialEq)]
pub struct PendingRemoteStart {
    /// The identifier the CSMS supplied for the transaction to run under.
    pub id_token: crate::state::IdToken,
    /// The request's `remoteStartId`, recorded on the transaction this eventually starts so it
    /// can be correlated back (F02.FR.01, CV6).
    pub remote_start_id: Option<i64>,
}
