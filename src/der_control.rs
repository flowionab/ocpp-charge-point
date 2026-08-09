//! DER (Distributed Energy Resource) control functional block: `GetDERControl`/`SetDERControl`/
//! `ClearDERControl`/`ReportDERControl`, `NotifyDERAlarm`/`NotifyDERStartStop`, `AFRRSignal`, and
//! `NotifyAllowedEnergyTransfer` (`docs/PRODUCTION-ROADMAP.md` B8.2).
//!
//! **2.1-only.** DER control is one of OCPP 2.1's own additions - neither 1.6J nor 2.0.1 has any
//! of these messages, so unlike most functional blocks in this crate there is no older-protocol
//! shape to downgrade to.
//!
//! See [`crate::state::DERControlStore`] for the store this module fronts, and its docs for why this
//! block **stores and reports** DER controls rather than **actuating** them - the same
//! "store-and-report, not actuate" split [`crate::tariff`] documents for tariffs, for the same
//! reason: this crate has nothing in [`crate::hardware`] that could apply a DER curve or a
//! reactive-power setpoint, and pretending otherwise would be a fidelity claim it cannot honour.
//!
//! # Direction
//!
//! `GetDERControl`, `SetDERControl`, `ClearDERControl` and `AFRRSignal` are CSMS-initiated - this
//! charge point registers a handler and answers. `NotifyAllowedEnergyTransfer` is also
//! CSMS-initiated (the CSMS tells this charge point which energy transfer modes it will accept for
//! a transaction, relevant to an ISO 15118-20 negotiation this crate does not otherwise model).
//! `ReportDERControl` is this charge point's response to `GetDERControl`, sent inline by the same
//! handler. `NotifyDERAlarm` and `NotifyDERStartStop` are charge-point-initiated: this crate
//! exposes [`DERAlarmNotifier`]/[`DERStartStopNotifier`] for a caller to send one when it has
//! something to report, but - per the store-and-report scope above - nothing in this crate
//! spontaneously raises one yet, since nothing here actuates a control in the first place to
//! notice it starting, stopping, or alarming.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use chrono::{DateTime, Utc};

use crate::actor::ChargePointActor;
use crate::state::{
    ChargePointEvent, DERControlId, DERControlKind, DERControlQuery, DERControlRejection,
    InstalledDERControl,
};

/// A grid event/fault an EV reported alongside a `NotifyDERAlarm` - OCPP's `GridEventFaultEnumType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridEventFault {
    /// Imbalance between phase currents.
    CurrentImbalance,
    /// A local emergency condition.
    LocalEmergency,
    /// Input power too low.
    LowInputPower,
    /// Over-current.
    OverCurrent,
    /// Over-frequency.
    OverFrequency,
    /// Over-voltage.
    OverVoltage,
    /// Incorrect phase rotation.
    PhaseRotation,
    /// A remote emergency condition.
    RemoteEmergency,
    /// Under-frequency.
    UnderFrequency,
    /// Under-voltage.
    UnderVoltage,
    /// Imbalance between phase voltages.
    VoltageImbalance,
}

/// A DER alarm condition starting or ending, reported via `NotifyDERAlarm`.
#[derive(Debug, Clone, PartialEq)]
pub struct DERAlarmEvent {
    /// Which kind of control the alarm concerns.
    pub control_type: DERControlKind,
    /// `true` when the alarm condition has ended, `false` when it started.
    pub alarm_ended: bool,
    /// Optional free-text info supplied by the EV/inverter.
    pub extra_info: Option<String>,
    /// The grid event/fault behind the alarm, if named.
    pub grid_event_fault: Option<GridEventFault>,
    /// When the alarm started or ended.
    pub timestamp: DateTime<Utc>,
}

/// A DER control starting or stopping, reported via `NotifyDERStartStop`.
#[derive(Debug, Clone, PartialEq)]
pub struct DERStartStopEvent {
    /// The control that started or stopped - corresponds to a `SetDERControl` `controlId`.
    pub control_id: DERControlId,
    /// `true` if the control has started, `false` if it has ended.
    pub started: bool,
    /// Ids of controls superseded as a result of this one starting.
    pub superseded_ids: Vec<DERControlId>,
    /// When the control started or ended.
    pub timestamp: DateTime<Utc>,
}

/// The outcome of a CSMS-initiated `SetDERControl`, matching OCPP's `DERControlStatusEnum` minus
/// `NotFound` (which only `GetDERControl`/`ClearDERControl` can answer with).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetDERControlOutcome {
    /// The control was installed.
    Accepted,
    /// The control was refused - the store is already at its configured maximum and this control
    /// isn't replacing one already installed. See
    /// [`crate::state::StateLimits::max_der_controls`].
    Rejected,
    /// DER control isn't available on this charge point (C5:
    /// `docs/PRODUCTION-ROADMAP.md` §5.5) - registered because the `der-control` Cargo feature is
    /// on, but the hardware declares [`crate::hardware::Capabilities::der_control`] runtime-absent.
    NotSupported,
}

/// Handles a CSMS-initiated `SetDERControl` against `actor`.
///
/// Validation happens against a copy of the live store, so the CSMS's status reflects the real
/// [`max_der_controls`](crate::state::StateLimits::max_der_controls) bound. Only a control that
/// would actually install is dispatched.
pub async fn handle_set_der_control(
    actor: &ChargePointActor,
    control: InstalledDERControl,
) -> SetDERControlOutcome {
    let state = actor.state();
    if !crate::refusal::capability_present(&state.capabilities, "SetDERControl") {
        return SetDERControlOutcome::NotSupported;
    }
    let mut trial = state.der_controls.clone();
    if let Err(rejection) = trial.install(control.clone()) {
        tracing::warn!(?rejection, "refusing a DER control");
        return match rejection {
            DERControlRejection::TooManyControls => SetDERControlOutcome::Rejected,
        };
    }
    let _ = actor
        .send(ChargePointEvent::DERControlSet(Box::new(control)))
        .await;
    SetDERControlOutcome::Accepted
}

/// The outcome of a CSMS-initiated `ClearDERControl`/`GetDERControl`, matching OCPP's
/// `DERControlStatusEnum` restricted to the three values these two messages actually use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryOutcome {
    /// At least one control matched.
    Found,
    /// Nothing matched the query.
    NotFound,
    /// DER control isn't available on this charge point - see [`SetDERControlOutcome::NotSupported`].
    NotSupported,
}

/// Handles a CSMS-initiated `ClearDERControl` against `actor`. Reports [`QueryOutcome::NotFound`]
/// when the query matches nothing, so a CSMS can tell "cleared" from "there was nothing to clear".
pub async fn handle_clear_der_control(
    actor: &ChargePointActor,
    query: DERControlQuery,
) -> QueryOutcome {
    let state = actor.state();
    if !crate::refusal::capability_present(&state.capabilities, "ClearDERControl") {
        return QueryOutcome::NotSupported;
    }
    if state.der_controls.matching(&query).is_empty() {
        return QueryOutcome::NotFound;
    }
    let _ = actor
        .send(ChargePointEvent::DERControlsCleared { query })
        .await;
    QueryOutcome::Found
}

/// Handles a CSMS-initiated `GetDERControl` against `actor`, returning the controls that match -
/// what the charge point then reports back in one or more `ReportDERControl` messages - alongside
/// the status the response itself should carry.
pub fn handle_get_der_control(
    actor: &ChargePointActor,
    query: &DERControlQuery,
) -> (QueryOutcome, Vec<InstalledDERControl>) {
    let state = actor.state();
    if !crate::refusal::capability_present(&state.capabilities, "GetDERControl") {
        return (QueryOutcome::NotSupported, Vec::new());
    }
    let matched: Vec<InstalledDERControl> = state
        .der_controls
        .matching(query)
        .into_iter()
        .cloned()
        .collect();
    let outcome = if matched.is_empty() {
        QueryOutcome::NotFound
    } else {
        QueryOutcome::Found
    };
    (outcome, matched)
}

/// The most controls carried in a single `ReportDERControl` message.
///
/// OCPP bounds each of `ReportDERControlRequest`'s eight per-kind arrays at 24 entries; this stays
/// well under that per message (mixing kinds freely, unlike
/// [`crate::smart_charging::chunk_charging_profile_report`], which has to keep one `evseId`/
/// `chargingLimitSource` per message - `ReportDERControl` carries no such per-message scope, so
/// there is nothing forcing chunks to share a kind).
pub const DER_CONTROL_REPORT_CHUNK_SIZE: usize = 8;

/// Splits `controls` into the sequence of `ReportDERControl` messages that answers one
/// `GetDERControl`. An empty input produces **no** chunks - the response's own `NotFound` already
/// says so, and a report afterwards would contradict it (mirrors
/// [`crate::smart_charging::handle_get_charging_profiles`]'s reasoning).
pub fn chunk_der_control_report(controls: &[InstalledDERControl]) -> Vec<Vec<InstalledDERControl>> {
    if controls.is_empty() {
        return Vec::new();
    }
    controls
        .chunks(DER_CONTROL_REPORT_CHUNK_SIZE)
        .map(<[InstalledDERControl]>::to_vec)
        .collect()
}

/// The outcome of a CSMS-initiated `AFRRSignal`, matching OCPP's `GenericStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfrrSignalOutcome {
    /// The signal was recorded.
    Accepted,
    /// DER control isn't available on this charge point - `GenericStatusEnum` has no dedicated
    /// `NotSupported` value, so an absent capability collapses to `Rejected` here, unlike
    /// [`SetDERControlOutcome::NotSupported`].
    Rejected,
}

/// Handles a CSMS-initiated `AFRRSignal` against `actor`, recording it as the charge point's
/// latest known signal - see [`crate::state::AfrrSignal`].
pub async fn handle_afrr_signal(
    actor: &ChargePointActor,
    signal: i64,
    timestamp: DateTime<Utc>,
) -> AfrrSignalOutcome {
    let state = actor.state();
    if !crate::refusal::capability_present(&state.capabilities, "AFRRSignal") {
        return AfrrSignalOutcome::Rejected;
    }
    let _ = actor
        .send(ChargePointEvent::AfrrSignalReceived { signal, timestamp })
        .await;
    AfrrSignalOutcome::Accepted
}

/// The outcome of a CSMS-initiated `NotifyAllowedEnergyTransfer`, matching OCPP's
/// `NotifyAllowedEnergyTransferStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyAllowedEnergyTransferOutcome {
    /// The notification was accepted.
    Accepted,
    /// DER control isn't available, or the named transaction id doesn't parse.
    Rejected,
}

/// Handles a CSMS-initiated `NotifyAllowedEnergyTransfer` against `actor`.
///
/// This crate does not yet model an ISO 15118-20 charging-needs negotiation
/// (`docs/ROADMAP.md`'s ISO 15118 workstream) for the allowed modes to feed into, so - like
/// [`crate::state::AfrrSignal`] - there is nothing to store them against yet beyond validating the
/// shape of the request: the named transaction id must at least parse. A future negotiation layer
/// is the right place to remember *which* modes were allowed for *which* transaction; this handler
/// only answers honestly that the message arrived.
pub async fn handle_notify_allowed_energy_transfer(
    actor: &ChargePointActor,
    transaction_id: &str,
) -> NotifyAllowedEnergyTransferOutcome {
    let state = actor.state();
    if !crate::refusal::capability_present(&state.capabilities, "NotifyAllowedEnergyTransfer") {
        return NotifyAllowedEnergyTransferOutcome::Rejected;
    }
    if transaction_id.parse::<u64>().is_err() {
        return NotifyAllowedEnergyTransferOutcome::Rejected;
    }
    NotifyAllowedEnergyTransferOutcome::Accepted
}

/// Registers this charge point's inbound `SetDERControl` handling with the CSMS connection.
#[async_trait::async_trait]
pub trait SetDERControlHandler {
    /// Registers a `SetDERControl` handler dispatching against `actor`.
    async fn register_set_der_control_handler(&self, actor: ChargePointActor);
}

/// Registers this charge point's inbound `ClearDERControl` handling with the CSMS connection.
#[async_trait::async_trait]
pub trait ClearDERControlHandler {
    /// Registers a `ClearDERControl` handler dispatching against `actor`.
    async fn register_clear_der_control_handler(&self, actor: ChargePointActor);
}

/// Registers this charge point's inbound `GetDERControl` handling with the CSMS connection - also
/// responsible for sending the `ReportDERControl` message(s) that answer it.
#[async_trait::async_trait]
pub trait GetDERControlHandler {
    /// Registers a `GetDERControl` handler dispatching against `actor`.
    async fn register_get_der_control_handler(&self, actor: ChargePointActor);
}

/// Registers this charge point's inbound `AFRRSignal` handling with the CSMS connection.
#[async_trait::async_trait]
pub trait AfrrSignalHandler {
    /// Registers an `AFRRSignal` handler dispatching against `actor`.
    async fn register_afrr_signal_handler(&self, actor: ChargePointActor);
}

/// Registers this charge point's inbound `NotifyAllowedEnergyTransfer` handling with the CSMS
/// connection.
#[async_trait::async_trait]
pub trait NotifyAllowedEnergyTransferHandler {
    /// Registers a `NotifyAllowedEnergyTransfer` handler dispatching against `actor`.
    async fn register_notify_allowed_energy_transfer_handler(&self, actor: ChargePointActor);
}

/// Sends a `NotifyDERAlarm` reporting a DER alarm condition starting or ending.
///
/// Nothing in this crate calls this spontaneously yet - see the module's docs for why - but the
/// message is fully wired for a caller (a future hardware-triggered alarm source) to use.
#[async_trait::async_trait]
pub trait DERAlarmNotifier {
    /// What went wrong reporting the alarm.
    type Error: core::fmt::Display;

    /// Reports `event`.
    async fn notify_der_alarm(&self, event: DERAlarmEvent) -> Result<(), Self::Error>;
}

/// Sends a `NotifyDERStartStop` reporting a DER control starting or stopping.
///
/// Nothing in this crate calls this spontaneously yet - see the module's docs for why - but the
/// message is fully wired for a caller to use.
#[async_trait::async_trait]
pub trait DERStartStopNotifier {
    /// What went wrong reporting the change.
    type Error: core::fmt::Display;

    /// Reports `event`.
    async fn notify_der_start_stop(&self, event: DERStartStopEvent) -> Result<(), Self::Error>;
}

/// OCPP 2.1 wire adapter for every message this module handles - the only version with a DER
/// control block at all (see the module's parent docs).
#[cfg(feature = "ocpp_2_1")]
pub mod ocpp_2_1 {
    use super::{
        AfrrSignalHandler, AfrrSignalOutcome, ClearDERControlHandler, DERAlarmEvent,
        DERAlarmNotifier, DERStartStopEvent, DERStartStopNotifier, GetDERControlHandler,
        GridEventFault, NotifyAllowedEnergyTransferHandler, NotifyAllowedEnergyTransferOutcome,
        QueryOutcome, SetDERControlHandler, SetDERControlOutcome, chunk_der_control_report,
        handle_afrr_signal, handle_clear_der_control, handle_get_der_control,
        handle_notify_allowed_energy_transfer, handle_set_der_control,
    };
    use crate::actor::ChargePointActor;
    use crate::state::{
        DERControlId, DERControlKind, DERControlQuery, DERControlSettings, DERCurvePoint,
        DERCurveSettings, DERUnit, EnterServiceSettings, FixedPfSettings, FixedVarSettings,
        FreqDroopSettings, GradientSettings, InstalledDERControl, LimitMaxDischargeSettings,
    };
    use alloc::boxed::Box;
    use alloc::vec::Vec;

    use crate::wire::v21::common::{
        DERControlEnum, DERControlStatusEnum, DERCurve, DERCurveGet, DERCurvePoints, DERUnitEnum,
        EnterService, EnterServiceGet, FixedPF, FixedPFGet, FixedVar, FixedVarGet, FreqDroop,
        FreqDroopGet, GenericStatusEnum, Gradient, GradientGet, GridEventFaultEnum,
        LimitMaxDischarge, LimitMaxDischargeGet, NotifyAllowedEnergyTransferStatusEnum,
    };
    use crate::wire::v21::{
        AFRRSignalRequest, AFRRSignalResponse, ClearDERControlRequest, ClearDERControlResponse,
        GetDERControlRequest, GetDERControlResponse, NotifyAllowedEnergyTransferRequest,
        NotifyAllowedEnergyTransferResponse, NotifyDERAlarmRequest, NotifyDERStartStopRequest,
        ReportDERControlRequest, SetDERControlRequest, SetDERControlResponse,
    };
    use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};

    fn map_kind(kind: &DERControlEnum) -> DERControlKind {
        match kind {
            DERControlEnum::EnterService => DERControlKind::EnterService,
            DERControlEnum::FreqDroop => DERControlKind::FreqDroop,
            DERControlEnum::FreqWatt => DERControlKind::FreqWatt,
            DERControlEnum::FixedPFAbsorb => DERControlKind::FixedPFAbsorb,
            DERControlEnum::FixedPFInject => DERControlKind::FixedPFInject,
            DERControlEnum::FixedVar => DERControlKind::FixedVar,
            DERControlEnum::Gradients => DERControlKind::Gradients,
            DERControlEnum::HFMustTrip => DERControlKind::HFMustTrip,
            DERControlEnum::HFMayTrip => DERControlKind::HFMayTrip,
            DERControlEnum::HVMustTrip => DERControlKind::HVMustTrip,
            DERControlEnum::HVMomCess => DERControlKind::HVMomCess,
            DERControlEnum::HVMayTrip => DERControlKind::HVMayTrip,
            DERControlEnum::LimitMaxDischarge => DERControlKind::LimitMaxDischarge,
            DERControlEnum::LFMustTrip => DERControlKind::LFMustTrip,
            DERControlEnum::LVMustTrip => DERControlKind::LVMustTrip,
            DERControlEnum::LVMomCess => DERControlKind::LVMomCess,
            DERControlEnum::LVMayTrip => DERControlKind::LVMayTrip,
            DERControlEnum::PowerMonitoringMustTrip => DERControlKind::PowerMonitoringMustTrip,
            DERControlEnum::VoltVar => DERControlKind::VoltVar,
            DERControlEnum::VoltWatt => DERControlKind::VoltWatt,
            DERControlEnum::WattPF => DERControlKind::WattPF,
            DERControlEnum::WattVar => DERControlKind::WattVar,
        }
    }

    fn wire_kind(kind: DERControlKind) -> DERControlEnum {
        match kind {
            DERControlKind::EnterService => DERControlEnum::EnterService,
            DERControlKind::FreqDroop => DERControlEnum::FreqDroop,
            DERControlKind::FreqWatt => DERControlEnum::FreqWatt,
            DERControlKind::FixedPFAbsorb => DERControlEnum::FixedPFAbsorb,
            DERControlKind::FixedPFInject => DERControlEnum::FixedPFInject,
            DERControlKind::FixedVar => DERControlEnum::FixedVar,
            DERControlKind::Gradients => DERControlEnum::Gradients,
            DERControlKind::HFMustTrip => DERControlEnum::HFMustTrip,
            DERControlKind::HFMayTrip => DERControlEnum::HFMayTrip,
            DERControlKind::HVMustTrip => DERControlEnum::HVMustTrip,
            DERControlKind::HVMomCess => DERControlEnum::HVMomCess,
            DERControlKind::HVMayTrip => DERControlEnum::HVMayTrip,
            DERControlKind::LimitMaxDischarge => DERControlEnum::LimitMaxDischarge,
            DERControlKind::LFMustTrip => DERControlEnum::LFMustTrip,
            DERControlKind::LVMustTrip => DERControlEnum::LVMustTrip,
            DERControlKind::LVMomCess => DERControlEnum::LVMomCess,
            DERControlKind::LVMayTrip => DERControlEnum::LVMayTrip,
            DERControlKind::PowerMonitoringMustTrip => DERControlEnum::PowerMonitoringMustTrip,
            DERControlKind::VoltVar => DERControlEnum::VoltVar,
            DERControlKind::VoltWatt => DERControlEnum::VoltWatt,
            DERControlKind::WattPF => DERControlEnum::WattPF,
            DERControlKind::WattVar => DERControlEnum::WattVar,
        }
    }

    fn map_unit(unit: &DERUnitEnum) -> DERUnit {
        match unit {
            DERUnitEnum::NotApplicable => DERUnit::NotApplicable,
            DERUnitEnum::PctMaxW => DERUnit::PctMaxW,
            DERUnitEnum::PctMaxVar => DERUnit::PctMaxVar,
            DERUnitEnum::PctWAvail => DERUnit::PctWAvail,
            DERUnitEnum::PctVarAvail => DERUnit::PctVarAvail,
            DERUnitEnum::PctEffectiveV => DERUnit::PctEffectiveV,
        }
    }

    fn wire_unit(unit: DERUnit) -> DERUnitEnum {
        match unit {
            DERUnit::NotApplicable => DERUnitEnum::NotApplicable,
            DERUnit::PctMaxW => DERUnitEnum::PctMaxW,
            DERUnit::PctMaxVar => DERUnitEnum::PctMaxVar,
            DERUnit::PctWAvail => DERUnitEnum::PctWAvail,
            DERUnit::PctVarAvail => DERUnitEnum::PctVarAvail,
            DERUnit::PctEffectiveV => DERUnitEnum::PctEffectiveV,
        }
    }

    /// `NotifyDERAlarm` is charge-point-initiated only (see the module's docs), so this adapter
    /// only ever needs the internal-to-wire direction - there is no inbound `GridEventFaultEnum`
    /// to map back.
    fn wire_grid_event_fault(fault: GridEventFault) -> GridEventFaultEnum {
        match fault {
            GridEventFault::CurrentImbalance => GridEventFaultEnum::CurrentImbalance,
            GridEventFault::LocalEmergency => GridEventFaultEnum::LocalEmergency,
            GridEventFault::LowInputPower => GridEventFaultEnum::LowInputPower,
            GridEventFault::OverCurrent => GridEventFaultEnum::OverCurrent,
            GridEventFault::OverFrequency => GridEventFaultEnum::OverFrequency,
            GridEventFault::OverVoltage => GridEventFaultEnum::OverVoltage,
            GridEventFault::PhaseRotation => GridEventFaultEnum::PhaseRotation,
            GridEventFault::RemoteEmergency => GridEventFaultEnum::RemoteEmergency,
            GridEventFault::UnderFrequency => GridEventFaultEnum::UnderFrequency,
            GridEventFault::UnderVoltage => GridEventFaultEnum::UnderVoltage,
            GridEventFault::VoltageImbalance => GridEventFaultEnum::VoltageImbalance,
        }
    }

    fn map_curve(curve: &DERCurve) -> DERCurveSettings {
        DERCurveSettings {
            curve_data: curve
                .curve_data
                .iter()
                .map(|point| DERCurvePoint {
                    x: point.x,
                    y: point.y,
                })
                .collect(),
            priority: curve.priority,
            duration_secs: curve.duration,
            response_time_secs: curve.response_time,
            start_time: curve.start_time.map(Into::into),
            y_unit: map_unit(&curve.y_unit),
        }
    }

    /// `hysteresis`/`reactivePowerParams`/`voltageParams` are always `None` here - see
    /// [`crate::state::DERCurveSettings`]'s docs for why they're read past.
    fn wire_curve(curve: &DERCurveSettings) -> DERCurve {
        let mut curve_data: heapless::Vec<DERCurvePoints, 10> = heapless::Vec::new();
        for point in &curve.curve_data {
            if curve_data
                .push(DERCurvePoints {
                    custom_data: None,
                    x: point.x,
                    y: point.y,
                })
                .is_err()
            {
                tracing::warn!("truncating a reported DER curve to the 10 points 2.1 can carry");
                break;
            }
        }
        DERCurve {
            curve_data,
            custom_data: None,
            duration: curve.duration_secs,
            hysteresis: None,
            priority: curve.priority,
            reactive_power_params: None,
            response_time: curve.response_time_secs,
            start_time: curve.start_time.map(Into::into),
            voltage_params: None,
            y_unit: wire_unit(curve.y_unit),
        }
    }

    fn map_enter_service(settings: &EnterService) -> EnterServiceSettings {
        EnterServiceSettings {
            priority: settings.priority,
            high_voltage: settings.high_voltage,
            low_voltage: settings.low_voltage,
            high_freq: settings.high_freq,
            low_freq: settings.low_freq,
            delay_secs: settings.delay,
            ramp_rate_secs: settings.ramp_rate,
            random_delay_secs: settings.random_delay,
        }
    }

    fn wire_enter_service(settings: &EnterServiceSettings) -> EnterService {
        EnterService {
            custom_data: None,
            delay: settings.delay_secs,
            high_freq: settings.high_freq,
            high_voltage: settings.high_voltage,
            low_freq: settings.low_freq,
            low_voltage: settings.low_voltage,
            priority: settings.priority,
            ramp_rate: settings.ramp_rate_secs,
            random_delay: settings.random_delay_secs,
        }
    }

    fn map_fixed_pf(settings: &FixedPF) -> FixedPfSettings {
        FixedPfSettings {
            priority: settings.priority,
            displacement: settings.displacement,
            excitation: settings.excitation,
            duration_secs: settings.duration,
            start_time: settings.start_time.map(Into::into),
        }
    }

    fn wire_fixed_pf(settings: &FixedPfSettings) -> FixedPF {
        FixedPF {
            custom_data: None,
            displacement: settings.displacement,
            duration: settings.duration_secs,
            excitation: settings.excitation,
            priority: settings.priority,
            start_time: settings.start_time.map(Into::into),
        }
    }

    fn map_fixed_var(settings: &FixedVar) -> FixedVarSettings {
        FixedVarSettings {
            priority: settings.priority,
            setpoint: settings.setpoint,
            unit: map_unit(&settings.unit),
            duration_secs: settings.duration,
            start_time: settings.start_time.map(Into::into),
        }
    }

    fn wire_fixed_var(settings: &FixedVarSettings) -> FixedVar {
        FixedVar {
            custom_data: None,
            duration: settings.duration_secs,
            priority: settings.priority,
            setpoint: settings.setpoint,
            start_time: settings.start_time.map(Into::into),
            unit: wire_unit(settings.unit),
        }
    }

    fn map_freq_droop(settings: &FreqDroop) -> FreqDroopSettings {
        FreqDroopSettings {
            priority: settings.priority,
            over_freq: settings.over_freq,
            over_droop: settings.over_droop,
            under_freq: settings.under_freq,
            under_droop: settings.under_droop,
            response_time_secs: settings.response_time,
            duration_secs: settings.duration,
            start_time: settings.start_time.map(Into::into),
        }
    }

    fn wire_freq_droop(settings: &FreqDroopSettings) -> FreqDroop {
        FreqDroop {
            custom_data: None,
            duration: settings.duration_secs,
            over_droop: settings.over_droop,
            over_freq: settings.over_freq,
            priority: settings.priority,
            response_time: settings.response_time_secs,
            start_time: settings.start_time.map(Into::into),
            under_droop: settings.under_droop,
            under_freq: settings.under_freq,
        }
    }

    fn map_gradient(settings: &Gradient) -> GradientSettings {
        GradientSettings {
            priority: settings.priority,
            gradient: settings.gradient,
            soft_gradient: settings.soft_gradient,
        }
    }

    fn wire_gradient(settings: &GradientSettings) -> Gradient {
        Gradient {
            custom_data: None,
            gradient: settings.gradient,
            priority: settings.priority,
            soft_gradient: settings.soft_gradient,
        }
    }

    fn map_limit_max_discharge(settings: &LimitMaxDischarge) -> LimitMaxDischargeSettings {
        LimitMaxDischargeSettings {
            priority: settings.priority,
            pct_max_discharge_power: settings.pct_max_discharge_power,
            power_monitoring_must_trip: settings.power_monitoring_must_trip.as_ref().map(map_curve),
            duration_secs: settings.duration,
            start_time: settings.start_time.map(Into::into),
        }
    }

    fn wire_limit_max_discharge(settings: &LimitMaxDischargeSettings) -> LimitMaxDischarge {
        LimitMaxDischarge {
            custom_data: None,
            duration: settings.duration_secs,
            pct_max_discharge_power: settings.pct_max_discharge_power,
            power_monitoring_must_trip: settings
                .power_monitoring_must_trip
                .as_ref()
                .map(wire_curve),
            priority: settings.priority,
            start_time: settings.start_time.map(Into::into),
        }
    }

    fn wire_id(id: &DERControlId) -> heapless::String<36> {
        heapless::String::try_from(id.0.as_str()).unwrap_or_default()
    }

    /// Builds the kind-specific payload for a `SetDERControl` request, matching
    /// `request.control_type` against whichever of the request's optional fields carries the
    /// corresponding payload - see [`crate::state::DERControlSettings`]'s docs for the
    /// controlType<->field correspondence this mirrors.
    fn map_settings(
        kind: DERControlKind,
        request: &SetDERControlRequest,
    ) -> Option<DERControlSettings> {
        match kind {
            DERControlKind::EnterService => request
                .enter_service
                .as_ref()
                .map(|s| DERControlSettings::EnterService(map_enter_service(s))),
            DERControlKind::FixedPFAbsorb => request
                .fixed_p_f_absorb
                .as_ref()
                .map(|s| DERControlSettings::FixedPf(map_fixed_pf(s))),
            DERControlKind::FixedPFInject => request
                .fixed_p_f_inject
                .as_ref()
                .map(|s| DERControlSettings::FixedPf(map_fixed_pf(s))),
            DERControlKind::FixedVar => request
                .fixed_var
                .as_ref()
                .map(|s| DERControlSettings::FixedVar(map_fixed_var(s))),
            DERControlKind::FreqDroop => request
                .freq_droop
                .as_ref()
                .map(|s| DERControlSettings::FreqDroop(map_freq_droop(s))),
            DERControlKind::Gradients => request
                .gradient
                .as_ref()
                .map(|s| DERControlSettings::Gradient(map_gradient(s))),
            DERControlKind::LimitMaxDischarge => request
                .limit_max_discharge
                .as_ref()
                .map(|s| DERControlSettings::LimitMaxDischarge(map_limit_max_discharge(s))),
            _ => request
                .curve
                .as_ref()
                .map(|s| DERControlSettings::Curve(map_curve(s))),
        }
    }

    fn set_response(status: DERControlStatusEnum, reason: Option<&str>) -> SetDERControlResponse {
        SetDERControlResponse {
            custom_data: None,
            status,
            status_info: reason.and_then(status_info),
            // Supersession isn't modeled - see `DERControlStore::install`'s docs.
            superseded_ids: None,
        }
    }

    fn status_info(reason_code: &str) -> Option<crate::wire::v21::common::StatusInfo> {
        Some(crate::wire::v21::common::StatusInfo {
            additional_info: None,
            custom_data: None,
            reason_code: heapless::String::try_from(reason_code).ok()?,
        })
    }

    async fn handle_set(
        actor: &ChargePointActor,
        request: &SetDERControlRequest,
    ) -> SetDERControlResponse {
        let kind = map_kind(&request.control_type);
        let Some(settings) = map_settings(kind, request) else {
            // OCPP names no reason code for "the payload for `controlType` was missing" - see
            // `set_response`'s caller in `crate::smart_charging::ocpp_2_1` for the same policy
            // against inventing one the spec doesn't define.
            return set_response(DERControlStatusEnum::Rejected, None);
        };
        let control = InstalledDERControl {
            id: DERControlId(request.control_id.as_str().into()),
            kind,
            is_default: request.is_default,
            settings,
        };
        let outcome = handle_set_der_control(actor, control).await;
        match outcome {
            SetDERControlOutcome::Accepted => set_response(DERControlStatusEnum::Accepted, None),
            SetDERControlOutcome::Rejected => set_response(DERControlStatusEnum::Rejected, None),
            SetDERControlOutcome::NotSupported => {
                set_response(DERControlStatusEnum::NotSupported, None)
            }
        }
    }

    #[async_trait::async_trait]
    impl SetDERControlHandler for OCPP2_1Client {
        async fn register_set_der_control_handler(&self, actor: ChargePointActor) {
            self.on_set_der_control(move |request, _client| {
                let actor = actor.clone();
                async move { Ok::<_, OCPP2_1Error>(handle_set(&actor, &request).await) }
            })
            .await;
        }
    }

    fn clear_response(status: DERControlStatusEnum) -> ClearDERControlResponse {
        ClearDERControlResponse {
            custom_data: None,
            status,
            status_info: None,
        }
    }

    fn map_query(
        control_id: Option<&heapless::String<36>>,
        control_type: Option<&DERControlEnum>,
        is_default: Option<bool>,
    ) -> DERControlQuery {
        DERControlQuery {
            id: control_id.map(|id| DERControlId(id.as_str().into())),
            kind: control_type.map(map_kind),
            is_default,
        }
    }

    async fn handle_clear(
        actor: &ChargePointActor,
        request: &ClearDERControlRequest,
    ) -> ClearDERControlResponse {
        let query = map_query(
            request.control_id.as_ref(),
            request.control_type.as_ref(),
            Some(request.is_default),
        );
        let outcome = handle_clear_der_control(actor, query).await;
        match outcome {
            QueryOutcome::Found => clear_response(DERControlStatusEnum::Accepted),
            QueryOutcome::NotFound => clear_response(DERControlStatusEnum::NotFound),
            QueryOutcome::NotSupported => clear_response(DERControlStatusEnum::NotSupported),
        }
    }

    #[async_trait::async_trait]
    impl ClearDERControlHandler for OCPP2_1Client {
        async fn register_clear_der_control_handler(&self, actor: ChargePointActor) {
            self.on_clear_der_control(move |request, _client| {
                let actor = actor.clone();
                async move { Ok::<_, OCPP2_1Error>(handle_clear(&actor, &request).await) }
            })
            .await;
        }
    }

    /// Builds one `ReportDERControl` message's worth of `controls`, partitioned into
    /// `ReportDERControlRequest`'s per-kind arrays.
    fn wire_report(
        request_id: i64,
        tbc: bool,
        controls: &[InstalledDERControl],
    ) -> ReportDERControlRequest {
        let mut curve = Vec::new();
        let mut enter_service = Vec::new();
        let mut fixed_p_f_absorb = Vec::new();
        let mut fixed_p_f_inject = Vec::new();
        let mut fixed_var = Vec::new();
        let mut freq_droop = Vec::new();
        let mut gradient = Vec::new();
        let mut limit_max_discharge = Vec::new();

        for installed in controls {
            let id = wire_id(&installed.id);
            match &installed.settings {
                DERControlSettings::Curve(settings) => curve.push(DERCurveGet {
                    curve: wire_curve(settings),
                    curve_type: wire_kind(installed.kind),
                    custom_data: None,
                    id,
                    is_default: installed.is_default,
                    is_superseded: false,
                }),
                DERControlSettings::EnterService(settings) => enter_service.push(EnterServiceGet {
                    custom_data: None,
                    enter_service: wire_enter_service(settings),
                    id,
                }),
                DERControlSettings::FixedPf(settings) => {
                    let entry = FixedPFGet {
                        custom_data: None,
                        fixed_p_f: wire_fixed_pf(settings),
                        id,
                        is_default: installed.is_default,
                        is_superseded: false,
                    };
                    if installed.kind == DERControlKind::FixedPFAbsorb {
                        fixed_p_f_absorb.push(entry);
                    } else {
                        fixed_p_f_inject.push(entry);
                    }
                }
                DERControlSettings::FixedVar(settings) => fixed_var.push(FixedVarGet {
                    custom_data: None,
                    fixed_var: wire_fixed_var(settings),
                    id,
                    is_default: installed.is_default,
                    is_superseded: false,
                }),
                DERControlSettings::FreqDroop(settings) => freq_droop.push(FreqDroopGet {
                    custom_data: None,
                    freq_droop: wire_freq_droop(settings),
                    id,
                    is_default: installed.is_default,
                    is_superseded: false,
                }),
                DERControlSettings::Gradient(settings) => gradient.push(GradientGet {
                    custom_data: None,
                    gradient: wire_gradient(settings),
                    id,
                }),
                DERControlSettings::LimitMaxDischarge(settings) => {
                    limit_max_discharge.push(LimitMaxDischargeGet {
                        custom_data: None,
                        id,
                        is_default: installed.is_default,
                        is_superseded: false,
                        limit_max_discharge: wire_limit_max_discharge(settings),
                    });
                }
            }
        }

        ReportDERControlRequest {
            curve: (!curve.is_empty()).then_some(curve),
            custom_data: None,
            enter_service: (!enter_service.is_empty()).then_some(enter_service),
            fixed_p_f_absorb: (!fixed_p_f_absorb.is_empty()).then_some(fixed_p_f_absorb),
            fixed_p_f_inject: (!fixed_p_f_inject.is_empty()).then_some(fixed_p_f_inject),
            fixed_var: (!fixed_var.is_empty()).then_some(fixed_var),
            freq_droop: (!freq_droop.is_empty()).then_some(freq_droop),
            gradient: (!gradient.is_empty()).then_some(gradient),
            limit_max_discharge: (!limit_max_discharge.is_empty()).then_some(limit_max_discharge),
            request_id,
            tbc: Some(tbc),
        }
    }

    /// Sends the controls a `GetDERControl` matched as one or more `ReportDERControl` messages. A
    /// message that fails to send is logged and the rest still go, mirroring
    /// [`crate::smart_charging::ocpp_2_1`]'s `send_charging_profile_report`.
    async fn send_der_control_report(
        client: &OCPP2_1Client,
        request_id: i64,
        controls: &[InstalledDERControl],
    ) {
        let chunks = chunk_der_control_report(controls);
        let last = chunks.len().saturating_sub(1);
        for (index, chunk) in chunks.iter().enumerate() {
            let request = wire_report(request_id, index != last, chunk);
            if let Err(err) = client.send_report_der_control(request).await {
                tracing::warn!(error = %err, "failed to send a ReportDERControl message");
            }
        }
    }

    fn get_response(status: DERControlStatusEnum) -> GetDERControlResponse {
        GetDERControlResponse {
            custom_data: None,
            status,
            status_info: None,
        }
    }

    #[async_trait::async_trait]
    impl GetDERControlHandler for OCPP2_1Client {
        async fn register_get_der_control_handler(&self, actor: ChargePointActor) {
            self.on_get_der_control(move |request: GetDERControlRequest, client| {
                let actor = actor.clone();
                async move {
                    let query = map_query(
                        request.control_id.as_ref(),
                        request.control_type.as_ref(),
                        request.is_default,
                    );
                    let (outcome, matched) = handle_get_der_control(&actor, &query);
                    // The reports go out *before* this response - see
                    // `crate::smart_charging::ocpp_2_1`'s `GetChargingProfiles` handler for why
                    // that ordering is fine: `requestId` correlates the two.
                    if let QueryOutcome::Found = outcome {
                        send_der_control_report(&client, request.request_id, &matched).await;
                    }
                    Ok::<_, OCPP2_1Error>(match outcome {
                        QueryOutcome::Found => get_response(DERControlStatusEnum::Accepted),
                        QueryOutcome::NotFound => get_response(DERControlStatusEnum::NotFound),
                        QueryOutcome::NotSupported => {
                            get_response(DERControlStatusEnum::NotSupported)
                        }
                    })
                }
            })
            .await;
        }
    }

    fn afrr_response(status: GenericStatusEnum) -> AFRRSignalResponse {
        AFRRSignalResponse {
            custom_data: None,
            status,
            status_info: None,
        }
    }

    #[async_trait::async_trait]
    impl AfrrSignalHandler for OCPP2_1Client {
        async fn register_afrr_signal_handler(&self, actor: ChargePointActor) {
            self.on_afrr_signal(move |request: AFRRSignalRequest, _client| {
                let actor = actor.clone();
                async move {
                    let outcome =
                        handle_afrr_signal(&actor, request.signal, request.timestamp.into()).await;
                    Ok::<_, OCPP2_1Error>(match outcome {
                        AfrrSignalOutcome::Accepted => afrr_response(GenericStatusEnum::Accepted),
                        AfrrSignalOutcome::Rejected => afrr_response(GenericStatusEnum::Rejected),
                    })
                }
            })
            .await;
        }
    }

    fn allowed_energy_transfer_response(
        status: NotifyAllowedEnergyTransferStatusEnum,
    ) -> NotifyAllowedEnergyTransferResponse {
        NotifyAllowedEnergyTransferResponse {
            custom_data: None,
            status,
            status_info: None,
        }
    }

    #[async_trait::async_trait]
    impl NotifyAllowedEnergyTransferHandler for OCPP2_1Client {
        async fn register_notify_allowed_energy_transfer_handler(&self, actor: ChargePointActor) {
            self.on_notify_allowed_energy_transfer(
                move |request: NotifyAllowedEnergyTransferRequest, _client| {
                    let actor = actor.clone();
                    async move {
                        let outcome = handle_notify_allowed_energy_transfer(
                            &actor,
                            request.transaction_id.as_str(),
                        )
                        .await;
                        Ok::<_, OCPP2_1Error>(match outcome {
                            NotifyAllowedEnergyTransferOutcome::Accepted => {
                                allowed_energy_transfer_response(
                                    NotifyAllowedEnergyTransferStatusEnum::Accepted,
                                )
                            }
                            NotifyAllowedEnergyTransferOutcome::Rejected => {
                                allowed_energy_transfer_response(
                                    NotifyAllowedEnergyTransferStatusEnum::Rejected,
                                )
                            }
                        })
                    }
                },
            )
            .await;
        }
    }

    #[async_trait::async_trait]
    impl DERAlarmNotifier for OCPP2_1Client {
        type Error = ocpp_client::ClientError<OCPP2_1Error>;

        async fn notify_der_alarm(&self, event: DERAlarmEvent) -> Result<(), Self::Error> {
            self.send_notify_der_alarm(NotifyDERAlarmRequest {
                alarm_ended: Some(event.alarm_ended),
                control_type: wire_kind(event.control_type),
                custom_data: None,
                extra_info: event
                    .extra_info
                    .as_deref()
                    .and_then(|info| heapless::String::try_from(info).ok()),
                grid_event_fault: event.grid_event_fault.map(wire_grid_event_fault),
                timestamp: event.timestamp.into(),
            })
            .await
            .map(|_| ())
        }
    }

    #[async_trait::async_trait]
    impl DERStartStopNotifier for OCPP2_1Client {
        type Error = ocpp_client::ClientError<OCPP2_1Error>;

        async fn notify_der_start_stop(&self, event: DERStartStopEvent) -> Result<(), Self::Error> {
            let superseded_ids: Vec<_> = event.superseded_ids.iter().map(wire_id).collect();
            self.send_notify_der_start_stop(NotifyDERStartStopRequest {
                control_id: wire_id(&event.control_id),
                custom_data: None,
                started: event.started,
                superseded_ids: (!superseded_ids.is_empty()).then_some(superseded_ids),
                timestamp: event.timestamp.into(),
            })
            .await
            .map(|_| ())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::executor::TokioExecutor;
        use crate::hardware::Capabilities;
        use crate::state::ChargePointEvent;

        async fn actor_with(capabilities: Capabilities) -> ChargePointActor {
            let actor = ChargePointActor::spawn([1], &TokioExecutor);
            actor
                .send(ChargePointEvent::CapabilitiesDeclared(capabilities))
                .await
                .unwrap();
            actor
        }

        fn der_capable() -> Capabilities {
            Capabilities {
                der_control: true,
                ..Default::default()
            }
        }

        fn gradient_request(control_id: &str, is_default: bool) -> SetDERControlRequest {
            SetDERControlRequest {
                control_id: heapless::String::try_from(control_id).unwrap(),
                control_type: DERControlEnum::Gradients,
                curve: None,
                custom_data: None,
                enter_service: None,
                fixed_p_f_absorb: None,
                fixed_p_f_inject: None,
                fixed_var: None,
                freq_droop: None,
                gradient: Some(Gradient {
                    custom_data: None,
                    gradient: 5.0,
                    priority: 0,
                    soft_gradient: 1.0,
                }),
                is_default,
                limit_max_discharge: None,
            }
        }

        #[tokio::test]
        async fn set_der_control_installs_and_reports_accepted() {
            let actor = actor_with(der_capable()).await;
            let response = handle_set(&actor, &gradient_request("a", false)).await;
            assert_eq!(response.status, DERControlStatusEnum::Accepted);
            assert_eq!(actor.state().der_controls.len(), 1);
        }

        #[tokio::test]
        async fn set_der_control_refuses_when_capability_absent() {
            let actor = actor_with(Capabilities::default()).await;
            let response = handle_set(&actor, &gradient_request("a", false)).await;
            assert_eq!(response.status, DERControlStatusEnum::NotSupported);
            assert!(actor.state().der_controls.is_empty());
        }

        #[tokio::test]
        async fn set_der_control_rejects_a_mismatched_payload() {
            let actor = actor_with(der_capable()).await;
            let mut request = gradient_request("a", false);
            request.gradient = None;
            let response = handle_set(&actor, &request).await;
            assert_eq!(response.status, DERControlStatusEnum::Rejected);
        }

        #[tokio::test]
        async fn get_der_control_reports_nothing_installed_as_not_found() {
            let actor = actor_with(der_capable()).await;
            let request = GetDERControlRequest {
                control_id: None,
                control_type: None,
                custom_data: None,
                is_default: None,
                request_id: 1,
            };
            let query = DERControlQuery {
                id: request
                    .control_id
                    .as_ref()
                    .map(|id| DERControlId(id.as_str().into())),
                kind: request.control_type.as_ref().map(map_kind),
                is_default: request.is_default,
            };
            let (outcome, matched) = handle_get_der_control(&actor, &query);
            assert_eq!(outcome, QueryOutcome::NotFound);
            assert!(matched.is_empty());
        }

        #[tokio::test]
        async fn clear_der_control_reports_not_found_for_no_match() {
            let actor = actor_with(der_capable()).await;
            let response = handle_clear(
                &actor,
                &ClearDERControlRequest {
                    control_id: Some(heapless::String::try_from("nonexistent").unwrap()),
                    control_type: None,
                    custom_data: None,
                    is_default: false,
                },
            )
            .await;
            assert_eq!(response.status, DERControlStatusEnum::NotFound);
        }

        #[tokio::test]
        async fn clear_der_control_removes_a_matching_control() {
            let actor = actor_with(der_capable()).await;
            handle_set(&actor, &gradient_request("a", false)).await;
            let response = handle_clear(
                &actor,
                &ClearDERControlRequest {
                    control_id: Some(heapless::String::try_from("a").unwrap()),
                    control_type: None,
                    custom_data: None,
                    is_default: false,
                },
            )
            .await;
            assert_eq!(response.status, DERControlStatusEnum::Accepted);
            assert!(actor.state().der_controls.is_empty());
        }

        #[tokio::test]
        async fn afrr_signal_is_recorded_when_der_control_is_available() {
            let actor = actor_with(der_capable()).await;
            let response = handle_afrr_signal(&actor, 42, chrono::Utc::now()).await;
            assert_eq!(response, AfrrSignalOutcome::Accepted);
            assert_eq!(actor.state().afrr_signal.unwrap().signal, 42);
        }

        #[tokio::test]
        async fn afrr_signal_is_rejected_when_capability_absent() {
            let actor = actor_with(Capabilities::default()).await;
            let response = handle_afrr_signal(&actor, 42, chrono::Utc::now()).await;
            assert_eq!(response, AfrrSignalOutcome::Rejected);
            assert!(actor.state().afrr_signal.is_none());
        }

        #[tokio::test]
        async fn notify_allowed_energy_transfer_accepts_a_parseable_transaction_id() {
            let actor = actor_with(der_capable()).await;
            let outcome = handle_notify_allowed_energy_transfer(&actor, "7").await;
            assert_eq!(outcome, NotifyAllowedEnergyTransferOutcome::Accepted);
        }

        #[tokio::test]
        async fn notify_allowed_energy_transfer_rejects_an_unparseable_transaction_id() {
            let actor = actor_with(der_capable()).await;
            let outcome = handle_notify_allowed_energy_transfer(&actor, "not-a-number").await;
            assert_eq!(outcome, NotifyAllowedEnergyTransferOutcome::Rejected);
        }

        #[test]
        fn every_kind_round_trips_through_the_wire_enum() {
            for kind in [
                DERControlKind::EnterService,
                DERControlKind::FreqDroop,
                DERControlKind::FreqWatt,
                DERControlKind::FixedPFAbsorb,
                DERControlKind::FixedPFInject,
                DERControlKind::FixedVar,
                DERControlKind::Gradients,
                DERControlKind::HFMustTrip,
                DERControlKind::HFMayTrip,
                DERControlKind::HVMustTrip,
                DERControlKind::HVMomCess,
                DERControlKind::HVMayTrip,
                DERControlKind::LimitMaxDischarge,
                DERControlKind::LFMustTrip,
                DERControlKind::LVMustTrip,
                DERControlKind::LVMomCess,
                DERControlKind::LVMayTrip,
                DERControlKind::PowerMonitoringMustTrip,
                DERControlKind::VoltVar,
                DERControlKind::VoltWatt,
                DERControlKind::WattPF,
                DERControlKind::WattVar,
            ] {
                assert_eq!(map_kind(&wire_kind(kind)), kind);
            }
        }

        #[test]
        fn every_unit_round_trips_through_the_wire_enum() {
            for unit in [
                DERUnit::NotApplicable,
                DERUnit::PctMaxW,
                DERUnit::PctMaxVar,
                DERUnit::PctWAvail,
                DERUnit::PctVarAvail,
                DERUnit::PctEffectiveV,
            ] {
                assert_eq!(map_unit(&wire_unit(unit)), unit);
            }
        }

        #[test]
        fn a_curve_with_more_points_than_the_wire_holds_is_truncated_not_dropped() {
            let settings = DERCurveSettings {
                curve_data: alloc::vec![DERCurvePoint { x: 1.0, y: 2.0 }; 15],
                priority: 0,
                duration_secs: None,
                response_time_secs: None,
                start_time: None,
                y_unit: DERUnit::NotApplicable,
            };
            let wire = wire_curve(&settings);
            assert_eq!(wire.curve_data.len(), 10);
        }

        #[tokio::test]
        async fn a_report_der_control_chunk_carries_every_matched_kind() {
            let actor = actor_with(der_capable()).await;
            handle_set(&actor, &gradient_request("a", false)).await;
            let query = DERControlQuery::default();
            let (outcome, matched) = handle_get_der_control(&actor, &query);
            assert_eq!(outcome, QueryOutcome::Found);
            let request = wire_report(1, false, &matched);
            assert!(request.gradient.is_some());
            assert!(request.curve.is_none());
        }
    }
}
