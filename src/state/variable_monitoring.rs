//! The variable monitoring engine's protocol-version-independent state: thresholds
//! (`UpperThreshold`/`LowerThreshold`), deltas, and periodic monitors on device-model variables
//! (OCPP `SetVariableMonitoring`/`ClearVariableMonitoring`, reported via `NotifyEvent`). See
//! `docs/ROADMAP.md` §2/§14 and `docs/PRODUCTION-ROADMAP.md` §B5 (B5.2).
//!
//! Monitoring **report generation** (`GetMonitoringReport`/`NotifyMonitoringReport`, B5.3) is a
//! separate, still-open concern: this module tracks monitors and evaluates them, but nothing
//! here answers "what monitors are installed" back to the CSMS as a chunked report.
//!
//! 1.6J has no equivalent of any of this - no monitor messages, no `NotifyEvent` - so there is no
//! `ocpp_1_6` projection anywhere in this functional block, unlike almost everything else in this
//! crate.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::state::{Component, Variable};

/// A [`VariableMonitor`]'s charge-point-assigned identifier (OCPP: a plain `int`,
/// `SetMonitoringData.id`/`SetMonitoringResult.id`/`ClearVariableMonitoringRequest.id`).
///
/// OCPP: "An id SHALL only be given \[by the CSMS\] to replace an existing monitor. The Charging
/// Station handles the generation of id's for new monitors." A CSMS-supplied id that names an
/// unknown monitor is still honoured as that exact id (rather than refused) - a monitor that
/// happens to remember the id an operator expected is more useful than one that doesn't, and OCPP
/// never actually requires the id to have been seen before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct VariableMonitorId(pub i64);

/// What kind of monitor a [`VariableMonitor`] is - the subset of OCPP's `MonitorEnumType` this
/// crate implements.
///
/// Deliberately missing three 2.1 values: `PeriodicClockAligned` (this crate only has plain
/// interval-based `Periodic` - see [`crate::variable_monitoring::run_periodic_variable_monitors`],
/// which fires every `value` seconds from when the monitor was installed rather than aligning to
/// the wall clock), and `TargetDelta`/`TargetDeltaRelative` (2.1's V2X/bidirectional-charging
/// setpoint monitors, which need a `Target` attribute this crate's device model never sets - see
/// `docs/ROADMAP.md` §2). A `SetVariableMonitoring` request naming any of the three is refused
/// with `UnsupportedMonitorType` rather than silently downgraded to something else - see
/// `crate::variable_monitoring::handle_set_variable_monitoring`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorType {
    /// Fires when the variable's value crosses upward through the monitor's threshold `value`.
    UpperThreshold,
    /// Fires when the variable's value crosses downward through the monitor's threshold `value`.
    LowerThreshold,
    /// Fires when the variable's value has moved by at least `value` since the last time this
    /// monitor fired (or since it was installed, for the first evaluation).
    Delta,
    /// Fires every `value` seconds, regardless of whether the value changed - see
    /// `crate::variable_monitoring::run_periodic_variable_monitors`.
    Periodic,
}

/// One registered variable monitor.
///
/// Owned by [`VariableMonitorStore`], itself owned by [`crate::state::ChargePointState`] and
/// mutated only through [`crate::state::VariableMonitoringEvent`], same as every other piece of
/// this crate's state.
#[derive(Debug, Clone, PartialEq)]
pub struct VariableMonitor {
    /// This monitor's id.
    pub id: VariableMonitorId,
    /// The component the monitored variable belongs to.
    pub component: Component,
    /// The monitored variable.
    pub variable: Variable,
    /// What kind of monitor this is.
    pub monitor_type: MonitorType,
    /// The threshold (Upper/LowerThreshold), delta amount (Delta), or interval in seconds
    /// (Periodic) - matches OCPP `SetMonitoringData.value`'s own three-way overload.
    pub value: f64,
    /// The severity to report an event triggered by this monitor at (OCPP: 0-9, 0 highest).
    pub severity: u8,
    /// For a [`MonitorType::Delta`] monitor: the value it last fired against (or was installed
    /// against, before its first evaluation). `None` for every other monitor type, which needs no
    /// baseline to decide whether to fire.
    delta_baseline: Option<f64>,
}

/// Which class of monitor the CSMS wants active - OCPP `MonitoringBaseEnum`
/// (`SetMonitoringBase`). See [`VariableMonitorStore::set_base`] for exactly what changes in this
/// crate's store when each variant is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MonitoringBase {
    /// Every monitor - hardwired, factory-preconfigured, and CSMS-installed - stays active. This
    /// crate has no hardwired or factory-preconfigured monitors of its own (every monitor a CSMS
    /// can see came from `SetVariableMonitoring`), so setting `All` is a no-op: nothing is
    /// cleared.
    #[default]
    All,
    /// Only the factory-default monitor set should remain active. This crate ships no factory
    /// default monitors, so honestly reflecting that means clearing every CSMS-installed monitor -
    /// leaving any of them running after this request would misreport the charge point's monitor
    /// set to the CSMS.
    FactoryDefault,
    /// Only hardwired monitors should remain active. This crate has no hardwired monitors either,
    /// so - like [`Self::FactoryDefault`] - honouring this honestly means clearing every
    /// CSMS-installed monitor.
    HardWiredOnly,
}

/// Why [`VariableMonitorStore::precheck`] refuses a brand new monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetMonitorRejection {
    /// The store already holds [`crate::state::StateLimits::max_variable_monitors`] monitors and
    /// this one does not replace any of them (G2.2: a bound a remote peer can push past is not a
    /// bound).
    TooManyMonitors,
    /// A monitor already watches this exact `(component, variable, monitor_type)` combination,
    /// and the request named no id to replace it with.
    Duplicate,
}

/// Every variable monitor installed on the charge point.
///
/// Bounded by [`crate::state::StateLimits::max_variable_monitors`] (G2.2). Owned by
/// [`crate::state::ChargePointState`] and mutated only through
/// [`crate::state::VariableMonitoringEvent`].
#[derive(Debug, Clone, PartialEq)]
pub struct VariableMonitorStore {
    monitors: BTreeMap<VariableMonitorId, VariableMonitor>,
    next_id: i64,
    max_monitors: usize,
    base: MonitoringBase,
    level: u8,
}

impl VariableMonitorStore {
    /// An empty store holding at most `max_monitors` monitors (clamped to at least 1), with the
    /// OCPP defaults [`MonitoringBase::All`] and a report-everything severity level (`9`, the
    /// lowest/least-restrictive value - see [`Self::is_reportable`]).
    pub fn with_limit(max_monitors: usize) -> Self {
        Self {
            monitors: BTreeMap::new(),
            next_id: 1,
            max_monitors: max_monitors.max(1),
            base: MonitoringBase::All,
            level: 9,
        }
    }

    /// The currently configured monitoring base (OCPP `SetMonitoringBase`) - see
    /// [`MonitoringBase`].
    pub fn base(&self) -> MonitoringBase {
        self.base
    }

    /// Applies a `SetMonitoringBase` request: records `base`, and - for
    /// [`MonitoringBase::FactoryDefault`]/[`MonitoringBase::HardWiredOnly`] - clears every
    /// installed monitor, since this crate has no hardwired/factory-preconfigured monitor set of
    /// its own to fall back to instead (see [`MonitoringBase`]'s variant docs on why leaving
    /// CSMS-installed monitors running in that case would misreport this charge point's monitor
    /// set). Returns whether anything actually changed - the base itself, the store being
    /// cleared, or both.
    pub fn set_base(&mut self, base: MonitoringBase) -> bool {
        let base_changed = self.base != base;
        self.base = base;
        let cleared = match base {
            MonitoringBase::All => false,
            MonitoringBase::FactoryDefault | MonitoringBase::HardWiredOnly => {
                let had_any = !self.monitors.is_empty();
                self.monitors.clear();
                had_any
            }
        };
        base_changed || cleared
    }

    /// The severity threshold at or below which a triggered monitor is reported to the CSMS
    /// (OCPP `SetMonitoringLevel`) - see [`Self::is_reportable`].
    pub fn level(&self) -> u8 {
        self.level
    }

    /// Sets [`Self::level`] (OCPP `SetMonitoringLevel`; already validated into range `0..=9` by
    /// [`crate::variable_monitoring::handle_set_monitoring_level`]). Returns whether it actually
    /// changed.
    pub fn set_level(&mut self, severity: u8) -> bool {
        let changed = self.level != severity;
        self.level = severity;
        changed
    }

    /// Whether a monitor firing at `severity` should be reported to the CSMS at all, per the
    /// currently configured [`Self::level`] - OCPP: "the Charging Station SHALL only report
    /// events with a severity number lower than or equal to this severity." Consulted by
    /// [`crate::variable_monitoring::run_variable_monitor_events`]/
    /// [`crate::variable_monitoring::run_periodic_variable_monitors`] before ever calling the
    /// notifier, so a level tightened by the CSMS actually suppresses reports rather than being a
    /// value this crate remembers but never acts on.
    pub fn is_reportable(&self, severity: u8) -> bool {
        severity <= self.level
    }

    /// The configured maximum - see [`crate::state::StateLimits::max_variable_monitors`].
    pub fn max_monitors(&self) -> usize {
        self.max_monitors
    }

    /// How many monitors are installed.
    pub fn len(&self) -> usize {
        self.monitors.len()
    }

    /// Whether nothing is installed.
    pub fn is_empty(&self) -> bool {
        self.monitors.is_empty()
    }

    /// Every installed monitor.
    pub fn installed(&self) -> impl Iterator<Item = &VariableMonitor> {
        self.monitors.values()
    }

    /// The monitor with this id, if installed.
    pub fn get(&self, id: VariableMonitorId) -> Option<&VariableMonitor> {
        self.monitors.get(&id)
    }

    /// The id that would be assigned to a brand new monitor right now - i.e. the store has never
    /// used it and never will unless [`Self::set`] is called with it.
    ///
    /// Used by [`crate::variable_monitoring::handle_set_variable_monitoring`] to resolve an
    /// auto-assigned id up front, from the same state snapshot its accept/reject decision is made
    /// against, so the id it reports back to the CSMS is the one [`Self::set`] actually uses. Two
    /// `SetVariableMonitoring` requests racing between that snapshot and the resulting
    /// [`crate::state::ChargePointEvent`] being applied could in principle both peek the same id;
    /// like every other batch decision in this crate (`crate::device_model::handle_set_variables`
    /// included), the decision is made against a snapshot and applied optimistically rather than
    /// through a lock held across the round trip.
    pub fn next_id(&self) -> VariableMonitorId {
        VariableMonitorId(self.next_id)
    }

    /// Whether installing a monitor with id `id` for `(component, variable, monitor_type)` should
    /// be refused, and why. `Ok(())` covers both an accepted brand new monitor and a replacement
    /// of an already-installed `id` (which can never be refused for capacity or duplication - see
    /// [`crate::state::StateLimits::max_charging_profiles`]'s sibling reasoning: replacing what's
    /// already there never grows the store).
    pub fn precheck(
        &self,
        id: VariableMonitorId,
        component: &Component,
        variable: &Variable,
        monitor_type: MonitorType,
    ) -> Result<(), SetMonitorRejection> {
        if self.monitors.contains_key(&id) {
            return Ok(());
        }
        let duplicate = self.monitors.values().any(|monitor| {
            monitor.component == *component
                && monitor.variable == *variable
                && monitor.monitor_type == monitor_type
        });
        if duplicate {
            return Err(SetMonitorRejection::Duplicate);
        }
        if self.monitors.len() >= self.max_monitors {
            return Err(SetMonitorRejection::TooManyMonitors);
        }
        Ok(())
    }

    /// Registers (or replaces) `id`'s monitor. The one way to add or redefine a monitor - see
    /// this method's caller, [`crate::state::ChargePointState::apply`].
    ///
    /// Returns whether the store actually changed. A brand new monitor that would push the store
    /// past [`Self::max_monitors`] is refused (and logged) rather than applied - a defensive
    /// backstop for callers that skip [`Self::precheck`], mirroring
    /// [`crate::state::DeviceModel::register`]'s same stance. Advances the internal id counter
    /// past `id.0` so a future auto-assigned id (see [`Self::next_id`]) never collides with one a
    /// CSMS supplied explicitly.
    pub fn set(
        &mut self,
        id: VariableMonitorId,
        component: Component,
        variable: Variable,
        monitor_type: MonitorType,
        value: f64,
        severity: u8,
    ) -> bool {
        let is_new = !self.monitors.contains_key(&id);
        if is_new && self.monitors.len() >= self.max_monitors {
            tracing::warn!(
                monitor_id = id.0,
                component = component.name.as_str(),
                variable = variable.name.as_str(),
                max_monitors = self.max_monitors,
                "refusing to register a variable monitor - the configured maximum is reached"
            );
            return false;
        }
        self.monitors.insert(
            id,
            VariableMonitor {
                id,
                component,
                variable,
                monitor_type,
                value,
                severity,
                delta_baseline: None,
            },
        );
        if id.0 >= self.next_id {
            self.next_id = id.0 + 1;
        }
        true
    }

    /// Removes `id`'s monitor. Returns whether anything was actually removed - a `ClearVariableMonitoring`
    /// naming an id this store doesn't have reports `NotFound` rather than `Accepted`, per OCPP.
    pub fn clear(&mut self, id: VariableMonitorId) -> bool {
        self.monitors.remove(&id).is_some()
    }

    /// Every currently-installed [`MonitorType::Periodic`] monitor - read by
    /// [`crate::variable_monitoring::run_periodic_variable_monitors`] on its own sweep, never by
    /// [`Self::evaluate`].
    pub fn periodic(&self) -> impl Iterator<Item = &VariableMonitor> {
        self.monitors
            .values()
            .filter(|monitor| monitor.monitor_type == MonitorType::Periodic)
    }

    /// Evaluates every threshold/delta monitor watching `(component, variable)` against the
    /// transition from `old_value` (the variable's previous `Actual` value, if it parsed as a
    /// number and existed) to `new_value` (its new one), mutating each triggered `Delta`
    /// monitor's baseline as it fires. Returns the ids of the monitors that fired, in no
    /// particular order.
    ///
    /// Called from [`crate::state::ChargePointState::apply`] on
    /// [`crate::state::DeviceModelEvent::AttributeValueSet`]'s `Actual` attribute - see that
    /// event's docs and `crate::device_model`'s module docs for why a value change is the trigger
    /// point. Never evaluates [`MonitorType::Periodic`] monitors, which report on their own clock
    /// regardless of whether the value ever changes - see [`Self::periodic`].
    ///
    /// A threshold monitor fires when the new value has crossed its threshold and the old one
    /// (if any) hadn't - so it fires once per crossing, not on every subsequent write that stays
    /// on the same side of the threshold. A `None` `old_value` (the variable's first-ever numeric
    /// write, or a write whose previous value didn't parse as a number) is treated as "not yet
    /// past the threshold", so a variable that starts out already over/under it does fire on that
    /// first write - the charge point cannot know whether the crossing happened before or during
    /// this write, and reporting it is the safer of the two readings.
    pub fn evaluate(
        &mut self,
        component: &Component,
        variable: &Variable,
        old_value: Option<f64>,
        new_value: f64,
    ) -> Vec<VariableMonitorId> {
        let mut triggered = Vec::new();
        for monitor in self
            .monitors
            .values_mut()
            .filter(|monitor| &monitor.component == component && &monitor.variable == variable)
        {
            let fires = match monitor.monitor_type {
                MonitorType::UpperThreshold => {
                    new_value >= monitor.value && old_value.is_none_or(|old| old < monitor.value)
                }
                MonitorType::LowerThreshold => {
                    new_value <= monitor.value && old_value.is_none_or(|old| old > monitor.value)
                }
                MonitorType::Delta => {
                    let baseline = *monitor
                        .delta_baseline
                        .get_or_insert(old_value.unwrap_or(new_value));
                    let crossed = (new_value - baseline).abs() >= monitor.value.abs();
                    if crossed {
                        monitor.delta_baseline = Some(new_value);
                    }
                    crossed
                }
                MonitorType::Periodic => false,
            };
            if fires {
                triggered.push(monitor.id);
            }
        }
        triggered
    }
}

/// An event mutating [`VariableMonitorStore`], applied by
/// [`crate::state::ChargePointState::apply`] via [`crate::state::ChargePointEvent::VariableMonitoring`].
#[derive(Debug, Clone, PartialEq)]
pub enum VariableMonitoringEvent {
    /// Registers (or replaces) one monitor - OCPP `SetVariableMonitoring`. `id` is always
    /// resolved by the caller before this is sent (either the id the CSMS supplied, or one peeked
    /// from [`VariableMonitorStore::next_id`]) - see that method's docs for why.
    MonitorSet {
        /// The monitor's id.
        id: VariableMonitorId,
        /// The component the monitored variable belongs to.
        component: Component,
        /// The monitored variable.
        variable: Variable,
        /// What kind of monitor this is.
        monitor_type: MonitorType,
        /// The threshold, delta amount, or interval in seconds.
        value: f64,
        /// The severity to report a triggered event at.
        severity: u8,
    },
    /// Removes one monitor by id - OCPP `ClearVariableMonitoring`. A no-op (see
    /// [`VariableMonitorStore::clear`]) if `id` isn't installed.
    MonitorCleared {
        /// The monitor to remove.
        id: VariableMonitorId,
    },
    /// Sets the monitoring base - OCPP `SetMonitoringBase`. See
    /// [`VariableMonitorStore::set_base`] for what this actually does to the store.
    BaseSet {
        /// The new base.
        base: MonitoringBase,
    },
    /// Sets the monitoring level - OCPP `SetMonitoringLevel`. See
    /// [`VariableMonitorStore::set_level`].
    LevelSet {
        /// The new severity threshold, already validated into OCPP's `0..=9` range by
        /// [`crate::variable_monitoring::handle_set_monitoring_level`].
        severity: u8,
    },
}

/// One `NotifyEvent` the charge point owes the CSMS.
///
/// Produced three ways: [`ChargePointState::apply`](crate::state::ChargePointState::apply) raises
/// this as a [`ChargePointEffect::VariableMonitorTriggered`](crate::state::ChargePointEffect::VariableMonitorTriggered)
/// for a threshold/delta monitor evaluated against a device-model value change (see
/// [`VariableMonitorStore::evaluate`]) and for a hard-wired notification the firmware raises
/// itself (G05's lock failure - see [`Self::monitor_id`]);
/// `crate::variable_monitoring::run_periodic_variable_monitors` builds one directly, on its own
/// clock, for a periodic monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggeredMonitor {
    /// The monitor that fired, or `None` for a **hard-wired notification** - one this firmware
    /// raises on its own rather than because a CSMS-configured monitor crossed a threshold.
    ///
    /// OCPP models the two as different `eventNotificationType`s (`CustomMonitor` against
    /// `HardWiredNotification`) and makes `variableMonitoringId` optional for exactly this reason.
    /// A hard-wired notification is not suppressed by `MonitoringLevel` either: nobody configured
    /// it, so there is no configured severity to compare against - see
    /// [`VariableMonitorStore::is_reportable`].
    pub monitor_id: Option<VariableMonitorId>,
    /// The monitored component.
    pub component: Component,
    /// The monitored variable.
    pub variable: Variable,
    /// The variable's current `Actual` value, as a wire-formatted string (this crate's device
    /// model already stores every attribute value this way - see
    /// [`crate::state::VariableAttribute::value`]).
    pub actual_value: alloc::string::String,
    /// The severity the monitor was configured to report at.
    pub severity: u8,
    /// What kind of trigger this was - OCPP `EventTriggerEnumType`.
    pub trigger: EventTrigger,
}

/// What kind of trigger produced a [`TriggeredMonitor`] (OCPP `EventTriggerEnumType`, minus
/// `HardWired` values this crate never raises - nothing in this functional block reports a
/// hard-wired notification, only ones a monitor produced).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventTrigger {
    /// A threshold monitor crossed its `UpperThreshold`/`LowerThreshold`.
    Alerting,
    /// A delta monitor's value moved by at least its configured delta.
    Delta,
    /// A periodic monitor's own interval elapsed.
    Periodic,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component() -> Component {
        Component {
            name: "EVSE".into(),
            instance: None,
            evse: None,
        }
    }

    fn variable() -> Variable {
        Variable {
            name: "Temperature".into(),
            instance: None,
        }
    }

    #[test]
    fn a_fresh_store_assigns_ids_starting_at_one() {
        let store = VariableMonitorStore::with_limit(10);

        assert_eq!(store.next_id(), VariableMonitorId(1));
    }

    #[test]
    fn setting_a_monitor_makes_it_lookupable_and_advances_the_next_id() {
        let mut store = VariableMonitorStore::with_limit(10);
        let id = store.next_id();

        let changed = store.set(
            id,
            component(),
            variable(),
            MonitorType::UpperThreshold,
            50.0,
            5,
        );

        assert!(changed);
        assert_eq!(store.get(id).unwrap().value, 50.0);
        assert_eq!(store.next_id(), VariableMonitorId(id.0 + 1));
    }

    #[test]
    fn an_explicit_csms_supplied_id_advances_the_counter_past_it() {
        let mut store = VariableMonitorStore::with_limit(10);

        store.set(
            VariableMonitorId(100),
            component(),
            variable(),
            MonitorType::Delta,
            1.0,
            5,
        );

        assert_eq!(store.next_id(), VariableMonitorId(101));
    }

    #[test]
    fn precheck_refuses_a_duplicate_component_variable_monitor_type() {
        let mut store = VariableMonitorStore::with_limit(10);
        let first = store.next_id();
        store.set(
            first,
            component(),
            variable(),
            MonitorType::UpperThreshold,
            50.0,
            5,
        );

        let second = store.next_id();
        let result = store.precheck(
            second,
            &component(),
            &variable(),
            MonitorType::UpperThreshold,
        );

        assert_eq!(result, Err(SetMonitorRejection::Duplicate));
    }

    #[test]
    fn precheck_allows_a_different_monitor_type_on_the_same_variable() {
        let mut store = VariableMonitorStore::with_limit(10);
        let first = store.next_id();
        store.set(
            first,
            component(),
            variable(),
            MonitorType::UpperThreshold,
            50.0,
            5,
        );

        let second = store.next_id();
        let result = store.precheck(
            second,
            &component(),
            &variable(),
            MonitorType::LowerThreshold,
        );

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn precheck_allows_replacing_an_existing_id_even_at_the_maximum() {
        let mut store = VariableMonitorStore::with_limit(1);
        let id = store.next_id();
        store.set(id, component(), variable(), MonitorType::Delta, 1.0, 5);

        let result = store.precheck(id, &component(), &variable(), MonitorType::Delta);

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn precheck_refuses_a_brand_new_monitor_past_the_maximum() {
        let mut store = VariableMonitorStore::with_limit(1);
        let id = store.next_id();
        store.set(id, component(), variable(), MonitorType::Delta, 1.0, 5);

        let next = store.next_id();
        let other_variable = Variable {
            name: "Other".into(),
            instance: None,
        };
        let result = store.precheck(next, &component(), &other_variable, MonitorType::Delta);

        assert_eq!(result, Err(SetMonitorRejection::TooManyMonitors));
    }

    #[test]
    fn setting_past_the_maximum_is_refused_and_leaves_the_store_alone() {
        let mut store = VariableMonitorStore::with_limit(1);
        let id = store.next_id();
        assert!(store.set(id, component(), variable(), MonitorType::Delta, 1.0, 5));

        let other_variable = Variable {
            name: "Other".into(),
            instance: None,
        };
        let refused = store.set(
            store.next_id(),
            component(),
            other_variable,
            MonitorType::Delta,
            1.0,
            5,
        );

        assert!(!refused);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn clearing_an_installed_monitor_removes_it() {
        let mut store = VariableMonitorStore::with_limit(10);
        let id = store.next_id();
        store.set(id, component(), variable(), MonitorType::Delta, 1.0, 5);

        assert!(store.clear(id));
        assert!(store.get(id).is_none());
    }

    #[test]
    fn clearing_an_unknown_monitor_is_a_no_op() {
        let mut store = VariableMonitorStore::with_limit(10);

        assert!(!store.clear(VariableMonitorId(999)));
    }

    #[test]
    fn a_fresh_store_defaults_to_base_all_and_level_nine() {
        let store = VariableMonitorStore::with_limit(10);

        assert_eq!(store.base(), MonitoringBase::All);
        assert_eq!(store.level(), 9);
        // Level 9 is the least restrictive value - everything 0-9 should still report.
        assert!((0..=9).all(|severity| store.is_reportable(severity)));
    }

    #[test]
    fn setting_base_to_all_does_not_clear_installed_monitors() {
        let mut store = VariableMonitorStore::with_limit(10);
        let id = store.next_id();
        store.set(id, component(), variable(), MonitorType::Delta, 1.0, 5);

        let changed = store.set_base(MonitoringBase::All);

        assert!(!changed, "All is a no-op when the base was already All");
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn setting_base_to_factory_default_clears_every_installed_monitor() {
        let mut store = VariableMonitorStore::with_limit(10);
        let id = store.next_id();
        store.set(id, component(), variable(), MonitorType::Delta, 1.0, 5);

        let changed = store.set_base(MonitoringBase::FactoryDefault);

        assert!(changed);
        assert!(store.is_empty());
        assert_eq!(store.base(), MonitoringBase::FactoryDefault);
    }

    #[test]
    fn setting_base_to_hard_wired_only_also_clears_every_installed_monitor() {
        let mut store = VariableMonitorStore::with_limit(10);
        let id = store.next_id();
        store.set(id, component(), variable(), MonitorType::Delta, 1.0, 5);

        let changed = store.set_base(MonitoringBase::HardWiredOnly);

        assert!(changed);
        assert!(store.is_empty());
    }

    #[test]
    fn setting_base_to_factory_default_on_an_already_empty_store_still_reports_the_base_change() {
        let mut store = VariableMonitorStore::with_limit(10);

        let changed = store.set_base(MonitoringBase::FactoryDefault);

        assert!(
            changed,
            "the base itself changed even though nothing was cleared"
        );
    }

    #[test]
    fn setting_the_level_narrows_what_is_reportable() {
        let mut store = VariableMonitorStore::with_limit(10);

        let changed = store.set_level(3);

        assert!(changed);
        assert_eq!(store.level(), 3);
        assert!(store.is_reportable(0));
        assert!(store.is_reportable(3));
        assert!(!store.is_reportable(4));
        assert!(!store.is_reportable(9));
    }

    #[test]
    fn setting_the_level_to_its_current_value_reports_no_change() {
        let mut store = VariableMonitorStore::with_limit(10);

        assert!(!store.set_level(9));
    }

    #[test]
    fn an_upper_threshold_monitor_fires_once_on_crossing_and_not_again_while_above() {
        let mut store = VariableMonitorStore::with_limit(10);
        let id = store.next_id();
        store.set(
            id,
            component(),
            variable(),
            MonitorType::UpperThreshold,
            50.0,
            5,
        );

        let first = store.evaluate(&component(), &variable(), Some(40.0), 60.0);
        assert_eq!(first, alloc::vec![id]);

        // Still above the threshold - no old->new crossing happened this time.
        let second = store.evaluate(&component(), &variable(), Some(60.0), 70.0);
        assert!(second.is_empty());
    }

    #[test]
    fn an_upper_threshold_monitor_fires_again_after_dropping_back_below() {
        let mut store = VariableMonitorStore::with_limit(10);
        let id = store.next_id();
        store.set(
            id,
            component(),
            variable(),
            MonitorType::UpperThreshold,
            50.0,
            5,
        );

        store.evaluate(&component(), &variable(), Some(40.0), 60.0);
        store.evaluate(&component(), &variable(), Some(60.0), 40.0);
        let third = store.evaluate(&component(), &variable(), Some(40.0), 55.0);

        assert_eq!(third, alloc::vec![id]);
    }

    #[test]
    fn a_lower_threshold_monitor_fires_on_a_downward_crossing() {
        let mut store = VariableMonitorStore::with_limit(10);
        let id = store.next_id();
        store.set(
            id,
            component(),
            variable(),
            MonitorType::LowerThreshold,
            10.0,
            5,
        );

        let triggered = store.evaluate(&component(), &variable(), Some(20.0), 5.0);

        assert_eq!(triggered, alloc::vec![id]);
    }

    #[test]
    fn a_first_ever_value_already_past_the_threshold_fires() {
        let mut store = VariableMonitorStore::with_limit(10);
        let id = store.next_id();
        store.set(
            id,
            component(),
            variable(),
            MonitorType::UpperThreshold,
            50.0,
            5,
        );

        let triggered = store.evaluate(&component(), &variable(), None, 100.0);

        assert_eq!(triggered, alloc::vec![id]);
    }

    #[test]
    fn a_delta_monitor_fires_once_the_cumulative_move_reaches_its_delta() {
        let mut store = VariableMonitorStore::with_limit(10);
        let id = store.next_id();
        store.set(id, component(), variable(), MonitorType::Delta, 5.0, 5);

        // First evaluation just seeds the baseline at the old value.
        let first = store.evaluate(&component(), &variable(), Some(100.0), 102.0);
        assert!(first.is_empty());

        let second = store.evaluate(&component(), &variable(), Some(102.0), 104.0);
        assert!(second.is_empty());

        let third = store.evaluate(&component(), &variable(), Some(104.0), 106.0);
        assert_eq!(third, alloc::vec![id]);
    }

    #[test]
    fn a_delta_monitor_rebaselines_after_firing() {
        let mut store = VariableMonitorStore::with_limit(10);
        let id = store.next_id();
        store.set(id, component(), variable(), MonitorType::Delta, 5.0, 5);

        store.evaluate(&component(), &variable(), Some(100.0), 106.0);
        // That fired and rebaselined at 106. A further move of 5 fires again.
        let after = store.evaluate(&component(), &variable(), Some(106.0), 111.0);

        assert_eq!(after, alloc::vec![id]);
    }

    #[test]
    fn periodic_monitors_never_fire_from_evaluate() {
        let mut store = VariableMonitorStore::with_limit(10);
        let id = store.next_id();
        store.set(id, component(), variable(), MonitorType::Periodic, 60.0, 5);

        let triggered = store.evaluate(&component(), &variable(), Some(0.0), 1000.0);

        assert!(triggered.is_empty());
    }

    #[test]
    fn periodic_lists_only_periodic_monitors() {
        let mut store = VariableMonitorStore::with_limit(10);
        let periodic_id = store.next_id();
        store.set(
            periodic_id,
            component(),
            variable(),
            MonitorType::Periodic,
            60.0,
            5,
        );
        let threshold_id = store.next_id();
        store.set(
            threshold_id,
            component(),
            Variable {
                name: "Other".into(),
                instance: None,
            },
            MonitorType::UpperThreshold,
            1.0,
            5,
        );

        let periodic: Vec<_> = store.periodic().map(|monitor| monitor.id).collect();

        assert_eq!(periodic, alloc::vec![periodic_id]);
    }

    #[test]
    fn a_monitor_on_a_different_variable_never_evaluates() {
        let mut store = VariableMonitorStore::with_limit(10);
        store.set(
            store.next_id(),
            component(),
            variable(),
            MonitorType::UpperThreshold,
            50.0,
            5,
        );

        let other_variable = Variable {
            name: "Other".into(),
            instance: None,
        };
        let triggered = store.evaluate(&component(), &other_variable, Some(0.0), 100.0);

        assert!(triggered.is_empty());
    }
}
