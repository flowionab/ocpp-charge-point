//! DER (Distributed Energy Resource) control state - OCPP 2.1's `SetDERControl`/`GetDERControl`/
//! `ClearDERControl`/`ReportDERControl` (`docs/PRODUCTION-ROADMAP.md` B8.2).
//!
//! **2.1-only.** Unlike most functional blocks in this crate, there is no older-protocol shape to
//! project this down to - neither 1.6J nor 2.0.1 has any concept of DER control at all, so this
//! module's model is free to stay close to the 2.1 wire shape rather than mediate between
//! versions.
//!
//! # Store-and-report, not actuate
//!
//! [`crate::hardware::Connector::set_current_limit`] is a single scalar *import* current limit.
//! DER controls carry curves (voltage/frequency ride-through, Volt-Var, Volt-Watt), fixed power
//! factor and reactive power setpoints, and export/discharge limits - none of which that hook (or
//! anything else in [`crate::hardware`]) can express. `docs/PRODUCTION-ROADMAP.md` B2.6/M3 already
//! records the same gap for smart-charging setpoints.
//!
//! This module therefore stores exactly what the CSMS installs and reports it back faithfully
//! through [`DERControlStore`] - it does not attempt to apply any control to hardware. That is a
//! deliberate, visible decision: nothing here claims to curtail, export, or ride through a grid
//! event, and no hardware trait pretends otherwise. A future actuation layer is free to read this
//! store; this crate does not yet write to hardware from it.
//!
//! # Relationship to [`Capabilities::supports_bidirectional_power`](crate::hardware::Capabilities::supports_bidirectional_power)
//!
//! The two capabilities are orthogonal, not one a specialisation of the other, and this crate does
//! not couple them: a hardware binding may declare `der_control` without
//! `supports_bidirectional_power` (e.g. import-side curtailment curves like `FreqWatt` or
//! `PowerMonitoringMustTrip` make sense on a unidirectional inverter), and a binding could in
//! principle support bidirectional power with no DER curve/curtailment story at all (a V2H unit
//! that only ever discharges on a fixed local schedule, never negotiating a grid-code curve with
//! the CSMS). Some individual control kinds (`LimitMaxDischarge`, `FixedPFAbsorb`/`FixedPFInject`'s
//! "absorbing vs injecting" framing) only make physical sense on hardware that can export at all -
//! but since this module never actuates anything, it has nothing to refuse on that basis today;
//! an actuation layer built on top of this store is the right place to reject a discharge-shaped
//! control against import-only hardware, not this one.

use alloc::string::String;
use alloc::vec::Vec;
use chrono::{DateTime, Utc};

/// Unique id of a DER control setting (`controlId` on the wire - a UUID or similar, up to 36
/// characters).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DERControlId(pub String);

/// Which kind of DER control a setting is - OCPP's `DERControlEnumType`. Determines which
/// [`DERControlSettings`] variant a [`InstalledDERControl`] carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DERControlKind {
    /// Grid entry conditions (voltage/frequency window, delay, ramp rate).
    EnterService,
    /// Frequency-droop response.
    FreqDroop,
    /// Frequency-watt curve.
    FreqWatt,
    /// Fixed power factor while absorbing (importing) reactive power.
    FixedPFAbsorb,
    /// Fixed power factor while injecting (exporting) reactive power.
    FixedPFInject,
    /// Fixed reactive power setpoint.
    FixedVar,
    /// Default ramp-rate gradient.
    Gradients,
    /// High-frequency must-trip curve.
    HFMustTrip,
    /// High-frequency may-trip curve.
    HFMayTrip,
    /// High-voltage must-trip curve.
    HVMustTrip,
    /// High-voltage momentary cessation curve.
    HVMomCess,
    /// High-voltage may-trip curve.
    HVMayTrip,
    /// Limit on maximum discharge power.
    LimitMaxDischarge,
    /// Low-frequency must-trip curve.
    LFMustTrip,
    /// Low-voltage must-trip curve.
    LVMustTrip,
    /// Low-voltage momentary cessation curve.
    LVMomCess,
    /// Low-voltage may-trip curve.
    LVMayTrip,
    /// Power-monitoring must-trip curve.
    PowerMonitoringMustTrip,
    /// Volt-Var curve.
    VoltVar,
    /// Volt-Watt curve.
    VoltWatt,
    /// Watt-power-factor curve.
    WattPF,
    /// Watt-Var curve.
    WattVar,
}

/// One point on a DER curve - `x`/`y` per OCPP's `DERCurvePointsType`. A curve carries at most 10
/// of these on the wire (`heapless::Vec<_, 10>` in `ocpp-types`); this crate's model doesn't repeat
/// that bound at the type level (it isn't a growable collection a peer can keep pushing into after
/// installation - see [`DERControlStore`]'s own bound), but a wire adapter constructing one from a
/// `SetDERControl` request only ever has that many to hand across anyway.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DERCurvePoint {
    /// The X-axis (independent) value.
    pub x: f64,
    /// The Y-axis (dependent) value, in `y_unit`.
    pub y: f64,
}

/// The unit a [`DERCurveSettings::y_unit`]/[`FixedVarSettings::unit`] value is expressed in -
/// OCPP's `DERUnitEnumType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DERUnit {
    /// No unit applies (e.g. a power-factor curve, where the Y axis is already dimensionless).
    NotApplicable,
    /// Percentage of maximum active power.
    PctMaxW,
    /// Percentage of maximum reactive power.
    PctMaxVar,
    /// Percentage of available active power.
    PctWAvail,
    /// Percentage of available reactive power.
    PctVarAvail,
    /// Percentage of effective (nominal) voltage.
    PctEffectiveV,
}

/// A DER curve (OCPP's `DERCurveType`) - the payload for every [`DERControlKind`] that isn't one
/// of the fixed-value settings below.
///
/// `hysteresis`, `reactivePowerParams` and `voltageParams` - OCPP fields that refine *how* a curve
/// is followed (return-to-normal ramp rate, Volt-Var reference adjustment, fault-ride-through
/// behaviour) - are read past rather than modeled, for the same reason the module's docs give for
/// not actuating at all: this crate has nothing yet that would use them, and keeping them would be
/// a fidelity claim this store cannot honestly make. A `ReportDERControl` built from this omits
/// them, exactly mirroring what was dropped on the way in.
#[derive(Debug, Clone, PartialEq)]
pub struct DERCurveSettings {
    /// The curve's data points, in the order the CSMS supplied them.
    pub curve_data: Vec<DERCurvePoint>,
    /// Priority of this curve (0 = highest).
    pub priority: i64,
    /// How long this curve stays active, in seconds. Only absent when [`InstalledDERControl::is_default`].
    pub duration_secs: Option<f64>,
    /// Open-loop response time in seconds.
    pub response_time_secs: Option<f64>,
    /// When this curve becomes active. Only absent when [`InstalledDERControl::is_default`].
    pub start_time: Option<DateTime<Utc>>,
    /// The unit the curve's Y axis is expressed in.
    pub y_unit: DERUnit,
}

/// Grid entry conditions (OCPP's `EnterServiceType`).
#[derive(Debug, Clone, PartialEq)]
pub struct EnterServiceSettings {
    /// Priority of this setting (0 = highest).
    pub priority: i64,
    /// Enter-service voltage high threshold.
    pub high_voltage: f64,
    /// Enter-service voltage low threshold.
    pub low_voltage: f64,
    /// Enter-service frequency high threshold.
    pub high_freq: f64,
    /// Enter-service frequency low threshold.
    pub low_freq: f64,
    /// Enter-service delay, in seconds.
    pub delay_secs: Option<f64>,
    /// Enter-service ramp rate, in seconds.
    pub ramp_rate_secs: Option<f64>,
    /// Enter-service randomized delay, in seconds.
    pub random_delay_secs: Option<f64>,
}

/// A fixed power factor setting (OCPP's `FixedPFType`) - used for both `FixedPFAbsorb` and
/// `FixedPFInject`; [`InstalledDERControl::kind`] is what distinguishes them.
#[derive(Debug, Clone, PartialEq)]
pub struct FixedPfSettings {
    /// Priority of this setting (0 = highest).
    pub priority: i64,
    /// Power factor, cos(phi), between 0 and 1.
    pub displacement: f64,
    /// `true` when absorbing (importing) reactive power, `false` when injecting (exporting) it.
    pub excitation: bool,
    /// How long this setting stays active, in seconds.
    pub duration_secs: Option<f64>,
    /// When this setting becomes active.
    pub start_time: Option<DateTime<Utc>>,
}

/// A fixed reactive power setpoint (OCPP's `FixedVarType`).
#[derive(Debug, Clone, PartialEq)]
pub struct FixedVarSettings {
    /// Priority of this setting (0 = highest).
    pub priority: i64,
    /// Signed percentage target (-100 to 100): negative charges, positive discharges.
    pub setpoint: f64,
    /// The unit `setpoint` is expressed in.
    pub unit: DERUnit,
    /// How long this setting stays active, in seconds.
    pub duration_secs: Option<f64>,
    /// When this setting becomes active.
    pub start_time: Option<DateTime<Utc>>,
}

/// A frequency-droop response setting (OCPP's `FreqDroopType`).
#[derive(Debug, Clone, PartialEq)]
pub struct FreqDroopSettings {
    /// Priority of this setting (0 = highest).
    pub priority: i64,
    /// Over-frequency start of droop.
    pub over_freq: f64,
    /// Over-frequency droop per unit.
    pub over_droop: f64,
    /// Under-frequency start of droop.
    pub under_freq: f64,
    /// Under-frequency droop per unit.
    pub under_droop: f64,
    /// Open-loop response time, in seconds.
    pub response_time_secs: f64,
    /// How long this setting stays active, in seconds.
    pub duration_secs: Option<f64>,
    /// When this setting becomes active.
    pub start_time: Option<DateTime<Utc>>,
}

/// A default ramp-rate gradient (OCPP's `GradientType`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GradientSettings {
    /// Priority of this setting (0 = highest).
    pub priority: i64,
    /// Default ramp rate, in seconds (0 if not applicable).
    pub gradient: f64,
    /// Soft-start ramp rate, in seconds (0 if not applicable).
    pub soft_gradient: f64,
}

/// A maximum-discharge-power limit (OCPP's `LimitMaxDischargeType`).
#[derive(Debug, Clone, PartialEq)]
pub struct LimitMaxDischargeSettings {
    /// Priority of this setting (0 = highest).
    pub priority: i64,
    /// Percentage (0-100) of rated maximum discharge power above which the
    /// `power_monitoring_must_trip` curve becomes active. Only meaningful for
    /// [`DERControlKind::PowerMonitoringMustTrip`].
    pub pct_max_discharge_power: Option<f64>,
    /// The power-monitoring must-trip curve, when this setting carries one.
    pub power_monitoring_must_trip: Option<DERCurveSettings>,
    /// How long this setting stays active, in seconds.
    pub duration_secs: Option<f64>,
    /// When this setting becomes active.
    pub start_time: Option<DateTime<Utc>>,
}

/// The kind-specific payload of one DER control setting.
///
/// Which variant is populated must agree with the [`InstalledDERControl::kind`] it accompanies -
/// enforced by the OCPP 2.1 wire adapter (`crate::der_control`'s `ocpp_2_1` submodule), which is
/// the only place a [`InstalledDERControl`] is ever constructed from an untrusted `SetDERControl`
/// request.
#[derive(Debug, Clone, PartialEq)]
pub enum DERControlSettings {
    /// A curve - see [`DERCurveSettings`].
    Curve(DERCurveSettings),
    /// Grid entry conditions - see [`EnterServiceSettings`].
    EnterService(EnterServiceSettings),
    /// A fixed power factor - see [`FixedPfSettings`].
    FixedPf(FixedPfSettings),
    /// A fixed reactive power setpoint - see [`FixedVarSettings`].
    FixedVar(FixedVarSettings),
    /// A frequency-droop response - see [`FreqDroopSettings`].
    FreqDroop(FreqDroopSettings),
    /// A default ramp-rate gradient - see [`GradientSettings`].
    Gradient(GradientSettings),
    /// A maximum-discharge-power limit - see [`LimitMaxDischargeSettings`].
    LimitMaxDischarge(LimitMaxDischargeSettings),
}

/// One DER control setting as installed by `SetDERControl`.
#[derive(Debug, Clone, PartialEq)]
pub struct InstalledDERControl {
    /// This control's identifier.
    pub id: DERControlId,
    /// Which kind of control this is.
    pub kind: DERControlKind,
    /// `true` if this is a default control (applies absent any scheduled one), `false` if it is a
    /// scheduled control.
    pub is_default: bool,
    /// The kind-specific payload.
    pub settings: DERControlSettings,
}

/// Why [`DERControlStore::install`] refused a control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DERControlRejection {
    /// The store is already holding [`crate::state::StateLimits::max_der_controls`] controls and
    /// this one is not replacing any of them by id (G2.2: a bound a remote peer can push past is
    /// not a bound).
    TooManyControls,
}

/// Selects which stored controls a `GetDERControl`/`ClearDERControl` addresses. Every `Some` field
/// narrows the match; an all-`None` query matches everything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DERControlQuery {
    /// Match only the control with this id.
    pub id: Option<DERControlId>,
    /// Match only controls of this kind.
    pub kind: Option<DERControlKind>,
    /// Match only controls whose `is_default` equals this.
    pub is_default: Option<bool>,
}

impl DERControlQuery {
    /// Whether `installed` is selected by this query.
    pub fn matches(&self, installed: &InstalledDERControl) -> bool {
        self.id.as_ref().is_none_or(|id| *id == installed.id)
            && self.kind.is_none_or(|kind| kind == installed.kind)
            && self
                .is_default
                .is_none_or(|is_default| is_default == installed.is_default)
    }
}

/// Every DER control setting installed on the charge point.
///
/// Bounded by [`crate::state::StateLimits::max_der_controls`] (G2.2). Owned by
/// [`crate::state::ChargePointState`] and mutated only through
/// [`crate::state::ChargePointEvent::DERControlSet`]/
/// [`DERControlsCleared`](crate::state::ChargePointEvent::DERControlsCleared), like every other
/// store in this crate.
#[derive(Debug, Clone, PartialEq)]
pub struct DERControlStore {
    controls: Vec<InstalledDERControl>,
    max_controls: usize,
}

impl DERControlStore {
    /// An empty store holding at most `max_controls` controls (clamped to at least 1).
    pub fn with_limit(max_controls: usize) -> Self {
        Self {
            controls: Vec::new(),
            max_controls: max_controls.max(1),
        }
    }

    /// The configured maximum - see [`crate::state::StateLimits::max_der_controls`].
    pub fn max_controls(&self) -> usize {
        self.max_controls
    }

    /// Every installed control, in installation order.
    pub fn installed(&self) -> &[InstalledDERControl] {
        &self.controls
    }

    /// How many controls are installed.
    pub fn len(&self) -> usize {
        self.controls.len()
    }

    /// Whether nothing is installed.
    pub fn is_empty(&self) -> bool {
        self.controls.is_empty()
    }

    /// Installs `control`, replacing any existing control with the same id - `SetDERControl`
    /// names its target by `controlId`, so re-sending one is an update, not a duplicate.
    ///
    /// This crate does not model OCPP's priority-based supersession (a higher-priority curve
    /// displacing a lower-priority one of the same kind): doing so honestly needs the activation
    /// logic this module deliberately doesn't have yet (see the module's "store-and-report, not
    /// actuate" docs), so `SetDERControlResponse.supersededIds` is always left empty by the wire
    /// adapter rather than guessed at here.
    pub fn install(&mut self, control: InstalledDERControl) -> Result<(), DERControlRejection> {
        let replaced = self.controls.iter().any(|c| c.id == control.id);
        if !replaced && self.controls.len() >= self.max_controls {
            return Err(DERControlRejection::TooManyControls);
        }
        self.controls.retain(|c| c.id != control.id);
        self.controls.push(control);
        Ok(())
    }

    /// Every control matching `query`, in installation order.
    pub fn matching(&self, query: &DERControlQuery) -> Vec<&InstalledDERControl> {
        self.controls
            .iter()
            .filter(|installed| query.matches(installed))
            .collect()
    }

    /// Removes every control matching `query`, returning the ids removed (empty means the CSMS's
    /// `ClearDERControl` matched nothing, which OCPP reports as `NotFound`).
    pub fn clear(&mut self, query: &DERControlQuery) -> Vec<DERControlId> {
        let mut removed = Vec::new();
        self.controls.retain(|installed| {
            if query.matches(installed) {
                removed.push(installed.id.clone());
                false
            } else {
                true
            }
        });
        removed
    }
}

/// The most recent automatic frequency restoration reserve signal the CSMS pushed (OCPP 2.1
/// `AFRRSignal`) - a grid-balancing setpoint. Recorded as a single value on
/// [`crate::state::ChargePointState`] rather than a growable collection: only the latest signal is
/// ever meaningful, and nothing in this crate acts on it yet (see the module's "store-and-report,
/// not actuate" docs), so there is nothing to bound.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AfrrSignal {
    /// The signal value, per `v2xSignalWattCurve`'s own units.
    pub signal: i64,
    /// When the signal becomes active.
    pub timestamp: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control(id: &str, kind: DERControlKind, is_default: bool) -> InstalledDERControl {
        InstalledDERControl {
            id: DERControlId(id.into()),
            kind,
            is_default,
            settings: DERControlSettings::Gradient(GradientSettings {
                priority: 0,
                gradient: 1.0,
                soft_gradient: 2.0,
            }),
        }
    }

    #[test]
    fn a_fresh_store_is_empty() {
        let store = DERControlStore::with_limit(4);
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert_eq!(store.max_controls(), 4);
    }

    #[test]
    fn installing_a_control_makes_it_visible() {
        let mut store = DERControlStore::with_limit(4);
        store
            .install(control("a", DERControlKind::Gradients, false))
            .unwrap();
        assert_eq!(store.len(), 1);
        assert_eq!(store.installed()[0].id, DERControlId("a".into()));
    }

    #[test]
    fn reinstalling_the_same_id_replaces_rather_than_duplicates() {
        let mut store = DERControlStore::with_limit(4);
        store
            .install(control("a", DERControlKind::Gradients, false))
            .unwrap();
        store
            .install(control("a", DERControlKind::EnterService, true))
            .unwrap();
        assert_eq!(store.len(), 1);
        assert_eq!(store.installed()[0].kind, DERControlKind::EnterService);
        assert!(store.installed()[0].is_default);
    }

    #[test]
    fn the_bound_is_a_bound() {
        let mut store = DERControlStore::with_limit(2);
        store
            .install(control("a", DERControlKind::Gradients, false))
            .unwrap();
        store
            .install(control("b", DERControlKind::Gradients, false))
            .unwrap();
        let result = store.install(control("c", DERControlKind::Gradients, false));
        assert_eq!(result, Err(DERControlRejection::TooManyControls));
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn replacing_an_existing_id_is_allowed_even_at_the_bound() {
        let mut store = DERControlStore::with_limit(1);
        store
            .install(control("a", DERControlKind::Gradients, false))
            .unwrap();
        assert!(
            store
                .install(control("a", DERControlKind::EnterService, true))
                .is_ok()
        );
    }

    #[test]
    fn matching_and_clearing_by_query() {
        let mut store = DERControlStore::with_limit(4);
        store
            .install(control("a", DERControlKind::Gradients, false))
            .unwrap();
        store
            .install(control("b", DERControlKind::EnterService, true))
            .unwrap();

        let matched = store.matching(&DERControlQuery {
            is_default: Some(true),
            ..Default::default()
        });
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].id, DERControlId("b".into()));

        let removed = store.clear(&DERControlQuery {
            is_default: Some(true),
            ..Default::default()
        });
        assert_eq!(removed, alloc::vec![DERControlId("b".into())]);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn clearing_nothing_matching_reports_no_removals() {
        let mut store = DERControlStore::with_limit(4);
        store
            .install(control("a", DERControlKind::Gradients, false))
            .unwrap();
        let removed = store.clear(&DERControlQuery {
            id: Some(DERControlId("nonexistent".into())),
            ..Default::default()
        });
        assert!(removed.is_empty());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn an_empty_query_matches_everything() {
        let mut store = DERControlStore::with_limit(4);
        store
            .install(control("a", DERControlKind::Gradients, false))
            .unwrap();
        store
            .install(control("b", DERControlKind::EnterService, true))
            .unwrap();
        assert_eq!(store.matching(&DERControlQuery::default()).len(), 2);
    }
}
