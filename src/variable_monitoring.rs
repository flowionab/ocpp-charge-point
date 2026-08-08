//! Variable monitoring functional block: `SetVariableMonitoring`/`ClearVariableMonitoring`
//! inbound, `NotifyEvent` outbound. See `docs/ROADMAP.md` §2/§14 and
//! `docs/PRODUCTION-ROADMAP.md` §B5 (B5.2).
//!
//! **2.x only.** OCPP 1.6J has no variable monitoring messages and no `NotifyEvent` at all, so -
//! unlike almost every other functional block in this crate - there is no `ocpp_1_6` submodule
//! here to even honestly say "unsupported" from: the messages simply don't exist on that wire.
//!
//! Monitoring **report generation** (`GetMonitoringReport`/`NotifyMonitoringReport`, B5.3) is a
//! separate, still-open concern - this module only sets up, clears, evaluates, and reports
//! monitors; nothing here answers "what monitors are installed" back to the CSMS as a report.

use crate::actor::ChargePointActor;
use crate::clock::Clock;
use crate::provisioning::Backoff;
use crate::state::{
    ChargePointEvent, Component, EventTrigger, MonitorType, SetMonitorRejection,
    TriggeredMonitor, Variable, VariableAttributeType, VariableMonitorId, VariableMonitoringEvent,
};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

#[cfg(feature = "ocpp_2_0_1")]
pub use self::ocpp_2_0_1::Ocpp2_0_1VariableMonitorEventNotifier;
#[cfg(feature = "ocpp_2_1")]
pub use self::ocpp_2_1::Ocpp2_1VariableMonitorEventNotifier;

/// One requested monitor in a `SetVariableMonitoring` request (OCPP `SetMonitoringData`, minus
/// wire-only bookkeeping fields).
#[derive(Debug, Clone, PartialEq)]
pub struct SetMonitorRequest {
    /// The id to replace, if the CSMS supplied one. `None` means "register a new monitor" - see
    /// [`crate::state::VariableMonitorStore::next_id`].
    pub id: Option<VariableMonitorId>,
    /// The component the monitored variable belongs to.
    pub component: Component,
    /// The monitored variable.
    pub variable: Variable,
    /// The requested monitor kind, or `None` when the wire named one this crate doesn't
    /// implement - see [`crate::state::MonitorType`]'s docs for exactly which, and this module's
    /// [`handle_set_variable_monitoring`] for what happens to it (`UnsupportedMonitorType`).
    pub monitor_type: Option<MonitorType>,
    /// The threshold, delta amount, or interval in seconds.
    pub value: f64,
    /// The requested severity (OCPP: 0-9). Carried as the wire's raw `i64` rather than already
    /// validated - [`handle_set_variable_monitoring`] is what rejects one out of range.
    pub severity: i64,
}

/// The outcome of resolving one [`SetMonitorRequest`], matching OCPP's `SetMonitoringStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetMonitorOutcome {
    /// The monitor was installed (or replaced), at this id and severity.
    Accepted {
        /// The monitor's id - the one the CSMS supplied, or one this charge point assigned.
        id: VariableMonitorId,
        /// The accepted severity (always `request.severity`, already validated into range).
        severity: u8,
    },
    /// `component` isn't registered in the device model at all.
    UnknownComponent,
    /// `component` is registered, but not with this `variable`.
    UnknownVariable,
    /// The wire named a monitor type this crate doesn't implement (see
    /// [`crate::state::MonitorType`]'s docs).
    UnsupportedMonitorType,
    /// The variable is registered, but its [`crate::state::VariableCharacteristics::supports_monitoring`]
    /// is `false`; or the request's `severity` was outside OCPP's 0-9 range; or a `Periodic`
    /// monitor's `value` (its interval in seconds) wasn't positive; or the store is at its
    /// configured maximum and this monitor doesn't replace an existing one (see
    /// [`crate::state::SetMonitorRejection::TooManyMonitors`] - OCPP's `SetMonitoringStatusEnum`
    /// has no separate "out of room" status, so this is the closest honest fit).
    Rejected,
    /// A monitor already watches this exact `(component, variable, monitor_type)` combination,
    /// and the request named no id to replace it with.
    Duplicate,
}

/// Handles a CSMS-initiated `SetVariableMonitoring` request against `actor`, resolving every
/// requested item independently and in order - mirroring
/// [`crate::device_model::handle_set_variables`]'s batch semantics. Each accepted item is applied
/// to the store (via [`VariableMonitoringEvent::MonitorSet`]) before moving on to the next, so a
/// later item in the same batch already observes an earlier one's effect.
pub async fn handle_set_variable_monitoring(
    actor: &ChargePointActor,
    requests: Vec<SetMonitorRequest>,
) -> Vec<SetMonitorOutcome> {
    let mut outcomes = Vec::with_capacity(requests.len());
    for request in requests {
        outcomes.push(resolve_and_apply_set(actor, request).await);
    }
    outcomes
}

/// Resolves and (if accepted) applies a single [`SetMonitorRequest`] against `actor`'s current
/// state - unknown component/variable, an unsupported monitor type, a variable that doesn't
/// support monitoring, an out-of-range severity, a non-positive periodic interval, a duplicate,
/// and the store's capacity bound are all checked, in that order, before anything is sent to the
/// actor.
async fn resolve_and_apply_set(
    actor: &ChargePointActor,
    request: SetMonitorRequest,
) -> SetMonitorOutcome {
    let state = actor.state();
    let Some(definition) = state.device_model.get(&request.component, &request.variable) else {
        return if state.device_model.has_component(&request.component) {
            SetMonitorOutcome::UnknownVariable
        } else {
            SetMonitorOutcome::UnknownComponent
        };
    };
    let Some(monitor_type) = request.monitor_type else {
        return SetMonitorOutcome::UnsupportedMonitorType;
    };
    if !definition.characteristics.supports_monitoring {
        return SetMonitorOutcome::Rejected;
    }
    if monitor_type == MonitorType::Periodic && request.value <= 0.0 {
        return SetMonitorOutcome::Rejected;
    }
    let Ok(severity) = u8::try_from(request.severity) else {
        return SetMonitorOutcome::Rejected;
    };
    if severity > 9 {
        return SetMonitorOutcome::Rejected;
    }

    // The id is resolved here, from the same snapshot the accept/reject decision itself is made
    // against - see `VariableMonitorStore::next_id`'s docs for the (narrow, shared-with-every-
    // other-batch-decision-in-this-crate) race that leaves open.
    let id = request.id.unwrap_or_else(|| state.variable_monitors.next_id());
    if let Err(rejection) =
        state
            .variable_monitors
            .precheck(id, &request.component, &request.variable, monitor_type)
    {
        return match rejection {
            SetMonitorRejection::TooManyMonitors => SetMonitorOutcome::Rejected,
            SetMonitorRejection::Duplicate => SetMonitorOutcome::Duplicate,
        };
    }

    let _ = actor
        .send(ChargePointEvent::VariableMonitoring(
            VariableMonitoringEvent::MonitorSet {
                id,
                component: request.component,
                variable: request.variable,
                monitor_type,
                value: request.value,
                severity,
            },
        ))
        .await;

    SetMonitorOutcome::Accepted { id, severity }
}

/// The outcome of clearing one monitor by id, matching OCPP's `ClearMonitoringStatusEnum` (minus
/// `Rejected`, which this crate never returns - clearing an installed monitor can't fail once
/// it's been found).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearMonitorOutcome {
    /// The monitor was removed.
    Accepted,
    /// No monitor with this id was installed.
    NotFound,
}

/// Handles a CSMS-initiated `ClearVariableMonitoring` request: resolves and clears every id
/// independently, in the order given - mirroring [`handle_set_variable_monitoring`]'s batch
/// semantics. Returns each id paired with its outcome, in request order, so the wire adapter can
/// zip them straight into `ClearMonitoringResult`s.
pub async fn handle_clear_variable_monitoring(
    actor: &ChargePointActor,
    ids: Vec<VariableMonitorId>,
) -> Vec<(VariableMonitorId, ClearMonitorOutcome)> {
    let mut outcomes = Vec::with_capacity(ids.len());
    for id in ids {
        let installed = actor.state().variable_monitors.get(id).is_some();
        if installed {
            let _ = actor
                .send(ChargePointEvent::VariableMonitoring(
                    VariableMonitoringEvent::MonitorCleared { id },
                ))
                .await;
            outcomes.push((id, ClearMonitorOutcome::Accepted));
        } else {
            outcomes.push((id, ClearMonitorOutcome::NotFound));
        }
    }
    outcomes
}

/// Registers this charge point's inbound `SetVariableMonitoring` handling with the CSMS
/// connection. Implemented per protocol version (see the `ocpp_2_1` module). **2.x only** - see
/// this module's docs.
#[async_trait::async_trait]
pub trait SetVariableMonitoringHandler {
    /// Registers a `SetVariableMonitoring` handler with the CSMS connection that dispatches
    /// incoming requests to [`handle_set_variable_monitoring`] against `actor`.
    async fn register_set_variable_monitoring_handler(&self, actor: ChargePointActor);
}

/// Registers this charge point's inbound `ClearVariableMonitoring` handling with the CSMS
/// connection. Implemented per protocol version (see the `ocpp_2_1` module). **2.x only**.
#[async_trait::async_trait]
pub trait ClearVariableMonitoringHandler {
    /// Registers a `ClearVariableMonitoring` handler with the CSMS connection that dispatches
    /// incoming requests to [`handle_clear_variable_monitoring`] against `actor`.
    async fn register_clear_variable_monitoring_handler(&self, actor: ChargePointActor);
}

/// Reports one fired variable monitor to the CSMS via `NotifyEvent`. Implemented per protocol
/// version (see the `ocpp_2_1` module), mirroring [`crate::security::SecurityEventNotifier`].
#[async_trait::async_trait]
pub trait VariableMonitorEventNotifier {
    /// The error type returned if the `NotifyEvent` request itself fails.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Reports `event` to the CSMS as a single-item `NotifyEvent` (`seqNo` 0, no `tbc`).
    async fn notify_variable_monitor_event(
        &self,
        event: &TriggeredMonitor,
    ) -> Result<(), Self::Error>;
}

/// Forwards every threshold/delta monitor firing received on `events` to the CSMS via `notifier`,
/// forever. A periodic monitor never appears on `events` - see
/// [`run_periodic_variable_monitors`], which reports those separately, on its own clock.
///
/// Errors are logged and do not stop the loop - the actor already recorded the trigger; only the
/// CSMS-facing report failed and is not retried here. [`crate::builder::ChargePointBuilder`]
/// callers wanting a failed report *queued* rather than dropped should wrap this the way
/// `setup()` wraps other report streams (`crate::offline_queue`).
pub async fn run_variable_monitor_events<N: VariableMonitorEventNotifier>(
    mut events: crate::sync::BroadcastReceiver<TriggeredMonitor>,
    notifier: &N,
) {
    while let Ok(event) = events.recv().await {
        if let Err(err) = notifier.notify_variable_monitor_event(&event).await {
            tracing::warn!(
                error = %err,
                monitor_id = event.monitor_id.0,
                "variable monitor event notification failed"
            );
        }
    }
}

/// Reports every due [`crate::state::MonitorType::Periodic`] monitor to the CSMS via `notifier`,
/// every `sweep_interval_secs`, forever.
///
/// A periodic monitor is due once its own `value` (its configured interval, in seconds) has
/// elapsed since it last reported - tracked locally by this loop, not in
/// [`crate::state::ChargePointState`] (which is mutated only through
/// [`crate::state::ChargePointEvent`], and a periodic tick is not a state change - nothing about
/// the charge point's own state differs before and after one fires). `sweep_interval_secs`
/// governs how finely that due-check is sampled, not the monitors' own periods - a 1-5 second
/// sweep against monitors configured in the tens of seconds and up keeps drift negligible without
/// busy-spinning, mirroring [`crate::reservation::run_reservation_expiry`]'s fixed-sweep shape
/// rather than computing an exact next-wake time.
///
/// **Skipped for a sweep entirely while the clock is unsynchronized** (see
/// [`crate::clock::is_synchronized`]), matching `run_reservation_expiry`'s own stance: a charge
/// point that doesn't know the time cannot honestly stamp a `NotifyEvent`, and an unsynchronized
/// reading near the epoch would otherwise make every monitor look overdue at once.
///
/// A failed report is logged and the monitor is left due, so it's retried on the very next sweep
/// rather than silently waiting out its full interval again.
pub async fn run_periodic_variable_monitors<N: VariableMonitorEventNotifier, C: Clock, B: Backoff>(
    notifier: &N,
    clock: &C,
    backoff: &B,
    sweep_interval_secs: u32,
    actor: &ChargePointActor,
) {
    let mut last_reported: BTreeMap<VariableMonitorId, chrono::DateTime<chrono::Utc>> =
        BTreeMap::new();
    loop {
        backoff.wait(sweep_interval_secs.max(1)).await;
        let now = clock.now();
        if !crate::clock::is_synchronized(&now) {
            tracing::debug!(
                "skipping the periodic variable monitor sweep: the clock is not synchronized"
            );
            continue;
        }
        for event in due_periodic_events(&actor.state(), now, &last_reported) {
            let monitor_id = event.monitor_id;
            if let Err(err) = notifier.notify_variable_monitor_event(&event).await {
                tracing::warn!(
                    error = %err,
                    monitor_id = monitor_id.0,
                    "periodic variable monitor event notification failed"
                );
                continue;
            }
            last_reported.insert(monitor_id, now);
        }
    }
}

/// Every periodic monitor whose interval has elapsed since `last_reported`, as the
/// [`TriggeredMonitor`]s [`run_periodic_variable_monitors`] should report this sweep. A monitor
/// never reported before is due immediately - the charge point cannot know how long it's been
/// configured, so waiting out a full interval before ever reporting would be a worse default than
/// reporting once right away.
///
/// Skips a monitor whose variable no longer has an `Actual` attribute to read (deregistered since
/// the monitor was installed) - reporting a value this charge point doesn't have would mean
/// inventing one.
fn due_periodic_events(
    state: &crate::state::ChargePointState,
    now: chrono::DateTime<chrono::Utc>,
    last_reported: &BTreeMap<VariableMonitorId, chrono::DateTime<chrono::Utc>>,
) -> Vec<TriggeredMonitor> {
    let mut due = Vec::new();
    for monitor in state.variable_monitors.periodic() {
        let period_secs = monitor.value.max(1.0) as i64;
        let is_due = last_reported
            .get(&monitor.id)
            .is_none_or(|last| (now - *last).num_seconds() >= period_secs);
        if !is_due {
            continue;
        }
        let Some(definition) = state.device_model.get(&monitor.component, &monitor.variable)
        else {
            continue;
        };
        let Some(attribute) = definition.attribute(VariableAttributeType::Actual) else {
            continue;
        };
        due.push(TriggeredMonitor {
            monitor_id: monitor.id,
            component: monitor.component.clone(),
            variable: monitor.variable.clone(),
            actual_value: attribute.value.clone(),
            severity: monitor.severity,
            trigger: EventTrigger::Periodic,
        });
    }
    due
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::TokioExecutor;
    use crate::state::{DeviceModelEvent, VariableAttribute, VariableCharacteristics, VariableDataType, VariableMutability};

    fn component(name: &str) -> Component {
        Component {
            name: name.into(),
            instance: None,
            evse: None,
        }
    }

    fn variable(name: &str) -> Variable {
        Variable {
            name: name.into(),
            instance: None,
        }
    }

    async fn register_monitorable_variable(actor: &ChargePointActor, component_name: &str, variable_name: &str) {
        actor
            .send(ChargePointEvent::DeviceModel(
                DeviceModelEvent::VariableRegistered {
                    component: component(component_name),
                    variable: variable(variable_name),
                    characteristics: VariableCharacteristics {
                        data_type: VariableDataType::Decimal,
                        unit: None,
                        min_limit: None,
                        max_limit: None,
                        values_list: None,
                        supports_monitoring: true,
                    },
                    attributes: alloc::vec![VariableAttribute {
                        attribute_type: VariableAttributeType::Actual,
                        value: "0".into(),
                        mutability: VariableMutability::ReadWrite,
                        persistent: false,
                        constant: false,
                        requires_reboot: false,
                    }],
                },
            ))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn setting_a_monitor_on_a_monitorable_variable_is_accepted() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        register_monitorable_variable(&actor, "EVSE", "Temperature").await;

        let outcomes = handle_set_variable_monitoring(
            &actor,
            alloc::vec![SetMonitorRequest {
                id: None,
                component: component("EVSE"),
                variable: variable("Temperature"),
                monitor_type: Some(MonitorType::UpperThreshold),
                value: 60.0,
                severity: 2,
            }],
        )
        .await;

        assert!(matches!(
            outcomes.as_slice(),
            [SetMonitorOutcome::Accepted { severity: 2, .. }]
        ));
        assert_eq!(actor.state().variable_monitors.len(), 1);
    }

    #[tokio::test]
    async fn setting_a_monitor_on_a_variable_that_does_not_support_monitoring_is_rejected() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcomes = handle_set_variable_monitoring(
            &actor,
            alloc::vec![SetMonitorRequest {
                id: None,
                component: component("OCPPCommCtrlr"),
                variable: variable("HeartbeatInterval"),
                monitor_type: Some(MonitorType::UpperThreshold),
                value: 60.0,
                severity: 2,
            }],
        )
        .await;

        assert_eq!(outcomes, alloc::vec![SetMonitorOutcome::Rejected]);
    }

    #[tokio::test]
    async fn setting_a_monitor_on_an_unknown_component_is_reported() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcomes = handle_set_variable_monitoring(
            &actor,
            alloc::vec![SetMonitorRequest {
                id: None,
                component: component("Nonexistent"),
                variable: variable("X"),
                monitor_type: Some(MonitorType::UpperThreshold),
                value: 1.0,
                severity: 2,
            }],
        )
        .await;

        assert_eq!(outcomes, alloc::vec![SetMonitorOutcome::UnknownComponent]);
    }

    #[tokio::test]
    async fn an_unsupported_monitor_type_is_reported() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        register_monitorable_variable(&actor, "EVSE", "Temperature").await;

        let outcomes = handle_set_variable_monitoring(
            &actor,
            alloc::vec![SetMonitorRequest {
                id: None,
                component: component("EVSE"),
                variable: variable("Temperature"),
                monitor_type: None,
                value: 1.0,
                severity: 2,
            }],
        )
        .await;

        assert_eq!(
            outcomes,
            alloc::vec![SetMonitorOutcome::UnsupportedMonitorType]
        );
    }

    #[tokio::test]
    async fn an_out_of_range_severity_is_rejected() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        register_monitorable_variable(&actor, "EVSE", "Temperature").await;

        let outcomes = handle_set_variable_monitoring(
            &actor,
            alloc::vec![SetMonitorRequest {
                id: None,
                component: component("EVSE"),
                variable: variable("Temperature"),
                monitor_type: Some(MonitorType::UpperThreshold),
                value: 1.0,
                severity: 42,
            }],
        )
        .await;

        assert_eq!(outcomes, alloc::vec![SetMonitorOutcome::Rejected]);
    }

    #[tokio::test]
    async fn a_non_positive_periodic_interval_is_rejected() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        register_monitorable_variable(&actor, "EVSE", "Temperature").await;

        let outcomes = handle_set_variable_monitoring(
            &actor,
            alloc::vec![SetMonitorRequest {
                id: None,
                component: component("EVSE"),
                variable: variable("Temperature"),
                monitor_type: Some(MonitorType::Periodic),
                value: 0.0,
                severity: 2,
            }],
        )
        .await;

        assert_eq!(outcomes, alloc::vec![SetMonitorOutcome::Rejected]);
    }

    #[tokio::test]
    async fn a_duplicate_monitor_is_reported() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        register_monitorable_variable(&actor, "EVSE", "Temperature").await;
        handle_set_variable_monitoring(
            &actor,
            alloc::vec![SetMonitorRequest {
                id: None,
                component: component("EVSE"),
                variable: variable("Temperature"),
                monitor_type: Some(MonitorType::UpperThreshold),
                value: 60.0,
                severity: 2,
            }],
        )
        .await;

        let outcomes = handle_set_variable_monitoring(
            &actor,
            alloc::vec![SetMonitorRequest {
                id: None,
                component: component("EVSE"),
                variable: variable("Temperature"),
                monitor_type: Some(MonitorType::UpperThreshold),
                value: 70.0,
                severity: 3,
            }],
        )
        .await;

        assert_eq!(outcomes, alloc::vec![SetMonitorOutcome::Duplicate]);
    }

    #[tokio::test]
    async fn setting_an_existing_id_replaces_it_rather_than_duplicating() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        register_monitorable_variable(&actor, "EVSE", "Temperature").await;
        let first = handle_set_variable_monitoring(
            &actor,
            alloc::vec![SetMonitorRequest {
                id: None,
                component: component("EVSE"),
                variable: variable("Temperature"),
                monitor_type: Some(MonitorType::UpperThreshold),
                value: 60.0,
                severity: 2,
            }],
        )
        .await;
        let SetMonitorOutcome::Accepted { id, .. } = first[0] else {
            panic!("expected Accepted");
        };

        let second = handle_set_variable_monitoring(
            &actor,
            alloc::vec![SetMonitorRequest {
                id: Some(id),
                component: component("EVSE"),
                variable: variable("Temperature"),
                monitor_type: Some(MonitorType::UpperThreshold),
                value: 80.0,
                severity: 4,
            }],
        )
        .await;

        assert_eq!(
            second,
            alloc::vec![SetMonitorOutcome::Accepted { id, severity: 4 }]
        );
        assert_eq!(actor.state().variable_monitors.len(), 1);
        assert_eq!(actor.state().variable_monitors.get(id).unwrap().value, 80.0);
    }

    #[tokio::test]
    async fn clearing_an_installed_monitor_is_accepted() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        register_monitorable_variable(&actor, "EVSE", "Temperature").await;
        let set = handle_set_variable_monitoring(
            &actor,
            alloc::vec![SetMonitorRequest {
                id: None,
                component: component("EVSE"),
                variable: variable("Temperature"),
                monitor_type: Some(MonitorType::UpperThreshold),
                value: 60.0,
                severity: 2,
            }],
        )
        .await;
        let SetMonitorOutcome::Accepted { id, .. } = set[0] else {
            panic!("expected Accepted");
        };

        let outcomes = handle_clear_variable_monitoring(&actor, alloc::vec![id]).await;

        assert_eq!(outcomes, alloc::vec![(id, ClearMonitorOutcome::Accepted)]);
        assert!(actor.state().variable_monitors.get(id).is_none());
    }

    #[tokio::test]
    async fn clearing_an_unknown_monitor_reports_not_found() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcomes = handle_clear_variable_monitoring(
            &actor,
            alloc::vec![crate::state::VariableMonitorId(999)],
        )
        .await;

        assert_eq!(
            outcomes,
            alloc::vec![(
                crate::state::VariableMonitorId(999),
                ClearMonitorOutcome::NotFound
            )]
        );
    }

    #[tokio::test]
    async fn setting_a_threshold_monitor_and_crossing_it_reports_a_notify_event() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        register_monitorable_variable(&actor, "EVSE", "Temperature").await;
        handle_set_variable_monitoring(
            &actor,
            alloc::vec![SetMonitorRequest {
                id: None,
                component: component("EVSE"),
                variable: variable("Temperature"),
                monitor_type: Some(MonitorType::UpperThreshold),
                value: 50.0,
                severity: 3,
            }],
        )
        .await;

        let mut events = actor.subscribe_variable_monitor_events();
        actor
            .send(ChargePointEvent::DeviceModel(
                DeviceModelEvent::AttributeValueSet {
                    component: component("EVSE"),
                    variable: variable("Temperature"),
                    attribute_type: VariableAttributeType::Actual,
                    value: "75".into(),
                },
            ))
            .await
            .unwrap();

        let triggered = events.recv().await.unwrap();
        assert_eq!(triggered.actual_value, "75");
        assert_eq!(triggered.severity, 3);
        assert_eq!(triggered.trigger, EventTrigger::Alerting);
    }

    #[tokio::test]
    async fn due_periodic_events_reports_a_never_reported_monitor_immediately() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        register_monitorable_variable(&actor, "EVSE", "Temperature").await;
        handle_set_variable_monitoring(
            &actor,
            alloc::vec![SetMonitorRequest {
                id: None,
                component: component("EVSE"),
                variable: variable("Temperature"),
                monitor_type: Some(MonitorType::Periodic),
                value: 60.0,
                severity: 5,
            }],
        )
        .await;

        let now = "2026-01-01T00:00:00Z".parse().unwrap();
        let due = due_periodic_events(&actor.state(), now, &BTreeMap::new());

        assert_eq!(due.len(), 1);
        assert_eq!(due[0].trigger, EventTrigger::Periodic);
    }

    #[tokio::test]
    async fn due_periodic_events_skips_a_monitor_reported_within_its_interval() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        register_monitorable_variable(&actor, "EVSE", "Temperature").await;
        let set = handle_set_variable_monitoring(
            &actor,
            alloc::vec![SetMonitorRequest {
                id: None,
                component: component("EVSE"),
                variable: variable("Temperature"),
                monitor_type: Some(MonitorType::Periodic),
                value: 60.0,
                severity: 5,
            }],
        )
        .await;
        let SetMonitorOutcome::Accepted { id, .. } = set[0] else {
            panic!("expected Accepted");
        };

        let last_reported_at = "2026-01-01T00:00:00Z".parse().unwrap();
        let mut last_reported = BTreeMap::new();
        last_reported.insert(id, last_reported_at);
        let now = "2026-01-01T00:00:30Z".parse().unwrap();

        let due = due_periodic_events(&actor.state(), now, &last_reported);

        assert!(due.is_empty());
    }

    #[tokio::test]
    async fn due_periodic_events_reports_again_once_the_interval_elapses() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        register_monitorable_variable(&actor, "EVSE", "Temperature").await;
        let set = handle_set_variable_monitoring(
            &actor,
            alloc::vec![SetMonitorRequest {
                id: None,
                component: component("EVSE"),
                variable: variable("Temperature"),
                monitor_type: Some(MonitorType::Periodic),
                value: 60.0,
                severity: 5,
            }],
        )
        .await;
        let SetMonitorOutcome::Accepted { id, .. } = set[0] else {
            panic!("expected Accepted");
        };

        let last_reported_at = "2026-01-01T00:00:00Z".parse().unwrap();
        let mut last_reported = BTreeMap::new();
        last_reported.insert(id, last_reported_at);
        let now = "2026-01-01T00:01:01Z".parse().unwrap();

        let due = due_periodic_events(&actor.state(), now, &last_reported);

        assert_eq!(due.len(), 1);
    }
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use super::{
        ClearMonitorOutcome, ClearVariableMonitoringHandler, EventTrigger, MonitorType,
        SetMonitorOutcome, SetMonitorRequest, SetVariableMonitoringHandler, TriggeredMonitor,
        VariableMonitorEventNotifier, VariableMonitorId, handle_clear_variable_monitoring,
        handle_set_variable_monitoring,
    };
    use crate::actor::ChargePointActor;
    use crate::clock::Clock;
    use crate::state::{Component, Variable};
    use alloc::boxed::Box;
    use alloc::string::ToString;
    use alloc::vec::Vec;
    use ocpp_client::ClientError;
    use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};
    use ocpp_client::ocpp_types::v21::common::{
        ClearMonitoringResult, ClearMonitoringStatusEnum, EVSE, EventData, EventNotificationEnum,
        EventTriggerEnum, MonitorEnum, SetMonitoringData, SetMonitoringResult,
        SetMonitoringStatusEnum,
    };
    use ocpp_client::ocpp_types::v21::{
        ClearVariableMonitoringResponse, NotifyEventRequest, SetVariableMonitoringResponse,
    };

    /// Truncates/bounds `value` to fit a `heapless::String<N>` on the wire, matching this crate's
    /// established truncate-over-drop convention - mirrors
    /// `crate::device_model::ocpp_2_1::truncate_to_byte_boundary`/`crate::reporting`'s
    /// `bounded_string`, duplicated here for the same reason those siblings duplicate it.
    fn bounded_string<const N: usize>(value: &str) -> heapless::String<N> {
        let mut end = value.len().min(N);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        heapless::String::try_from(&value[..end]).expect("truncated to fit the wire bound")
    }

    fn map_component(component: &ocpp_client::ocpp_types::v21::common::Component) -> Component {
        let evse = component.evse.as_ref().map(|evse| {
            let evse_id = usize::try_from(evse.id).unwrap_or(usize::MAX);
            let connector_id = evse.connector_id.and_then(|id| usize::try_from(id).ok());
            (evse_id, connector_id)
        });
        Component {
            name: component.name.to_string(),
            instance: component
                .instance
                .as_ref()
                .map(|instance| instance.to_string()),
            evse,
        }
    }

    fn build_component(component: &Component) -> ocpp_client::ocpp_types::v21::common::Component {
        ocpp_client::ocpp_types::v21::common::Component {
            custom_data: None,
            evse: component.evse.map(|(evse_id, connector_id)| EVSE {
                connector_id: connector_id.and_then(|id| i64::try_from(id).ok()),
                custom_data: None,
                id: i64::try_from(evse_id).unwrap_or(i64::MAX),
            }),
            instance: component.instance.as_deref().map(bounded_string::<50>),
            name: bounded_string::<50>(&component.name),
        }
    }

    fn map_variable(variable: &ocpp_client::ocpp_types::v21::common::Variable) -> Variable {
        Variable {
            name: variable.name.to_string(),
            instance: variable
                .instance
                .as_ref()
                .map(|instance| instance.to_string()),
        }
    }

    fn build_variable(variable: &Variable) -> ocpp_client::ocpp_types::v21::common::Variable {
        ocpp_client::ocpp_types::v21::common::Variable {
            custom_data: None,
            instance: variable.instance.as_deref().map(bounded_string::<50>),
            name: bounded_string::<50>(&variable.name),
        }
    }

    /// `None` for `PeriodicClockAligned`/`TargetDelta`/`TargetDeltaRelative` - see
    /// `crate::state::MonitorType`'s docs.
    fn map_monitor_type(kind: &MonitorEnum) -> Option<MonitorType> {
        match kind {
            MonitorEnum::UpperThreshold => Some(MonitorType::UpperThreshold),
            MonitorEnum::LowerThreshold => Some(MonitorType::LowerThreshold),
            MonitorEnum::Delta => Some(MonitorType::Delta),
            MonitorEnum::Periodic => Some(MonitorType::Periodic),
            MonitorEnum::PeriodicClockAligned
            | MonitorEnum::TargetDelta
            | MonitorEnum::TargetDeltaRelative => None,
        }
    }

    /// The inverse of [`map_monitor_type`] - only ever needed to build a wire request in this
    /// module's own round-trip tests (production code never re-emits a monitor type it received;
    /// it only ever reports `severity`/`id`/`status` back, via `build_set_monitoring_result`).
    #[cfg(test)]
    fn wire_monitor_type(kind: MonitorType) -> MonitorEnum {
        match kind {
            MonitorType::UpperThreshold => MonitorEnum::UpperThreshold,
            MonitorType::LowerThreshold => MonitorEnum::LowerThreshold,
            MonitorType::Delta => MonitorEnum::Delta,
            MonitorType::Periodic => MonitorEnum::Periodic,
        }
    }

    fn parse_set_monitoring_data(item: &SetMonitoringData) -> SetMonitorRequest {
        SetMonitorRequest {
            id: item.id.map(VariableMonitorId),
            component: map_component(&item.component),
            variable: map_variable(&item.variable),
            monitor_type: map_monitor_type(&item.r#type),
            value: item.value,
            severity: item.severity,
        }
    }

    fn map_set_monitoring_status(outcome: SetMonitorOutcome) -> SetMonitoringStatusEnum {
        match outcome {
            SetMonitorOutcome::Accepted { .. } => SetMonitoringStatusEnum::Accepted,
            SetMonitorOutcome::UnknownComponent => SetMonitoringStatusEnum::UnknownComponent,
            SetMonitorOutcome::UnknownVariable => SetMonitoringStatusEnum::UnknownVariable,
            SetMonitorOutcome::UnsupportedMonitorType => {
                SetMonitoringStatusEnum::UnsupportedMonitorType
            }
            SetMonitorOutcome::Rejected => SetMonitoringStatusEnum::Rejected,
            SetMonitorOutcome::Duplicate => SetMonitoringStatusEnum::Duplicate,
        }
    }

    fn build_set_monitoring_result(
        item: &SetMonitoringData,
        outcome: SetMonitorOutcome,
    ) -> SetMonitoringResult {
        let status = map_set_monitoring_status(outcome);
        let (id, severity) = match outcome {
            // An id is only ever returned on success, per OCPP - reporting one otherwise would
            // imply a monitor exists that doesn't.
            SetMonitorOutcome::Accepted { id, severity } => (Some(id.0), i64::from(severity)),
            _ => (None, item.severity),
        };
        SetMonitoringResult {
            component: item.component.clone(),
            custom_data: None,
            id,
            severity,
            status,
            status_info: None,
            r#type: item.r#type.clone(),
            variable: item.variable.clone(),
        }
    }

    fn map_clear_monitoring_status(outcome: ClearMonitorOutcome) -> ClearMonitoringStatusEnum {
        match outcome {
            ClearMonitorOutcome::Accepted => ClearMonitoringStatusEnum::Accepted,
            ClearMonitorOutcome::NotFound => ClearMonitoringStatusEnum::NotFound,
        }
    }

    fn map_trigger(trigger: EventTrigger) -> EventTriggerEnum {
        match trigger {
            EventTrigger::Alerting => EventTriggerEnum::Alerting,
            EventTrigger::Delta => EventTriggerEnum::Delta,
            EventTrigger::Periodic => EventTriggerEnum::Periodic,
        }
    }

    /// Builds a single-item `NotifyEvent` request for `event`, stamped `event_id`/`now` by the
    /// caller (see [`Ocpp2_1VariableMonitorEventNotifier`]) - always `seqNo` 0 with no `tbc`,
    /// since this crate reports one trigger per `NotifyEvent` rather than batching several into
    /// one message.
    fn build_notify_event_request(
        event: &TriggeredMonitor,
        event_id: i64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> NotifyEventRequest {
        let timestamp = now.to_rfc3339();
        NotifyEventRequest {
            custom_data: None,
            event_data: alloc::vec![EventData {
                actual_value: bounded_string::<2500>(&event.actual_value),
                cause: None,
                cleared: None,
                component: build_component(&event.component),
                custom_data: None,
                event_id,
                event_notification_type: EventNotificationEnum::CustomMonitor,
                severity: Some(i64::from(event.severity)),
                tech_code: None,
                tech_info: None,
                timestamp: timestamp.clone(),
                transaction_id: None,
                trigger: map_trigger(event.trigger),
                variable: build_variable(&event.variable),
                variable_monitoring_id: Some(event.monitor_id.0),
            }],
            generated_at: timestamp,
            seq_no: 0,
            tbc: None,
        }
    }

    #[async_trait::async_trait]
    impl SetVariableMonitoringHandler for OCPP2_1Client {
        async fn register_set_variable_monitoring_handler(&self, actor: ChargePointActor) {
            self.on_set_variable_monitoring(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let parsed: Vec<SetMonitorRequest> = request
                        .set_monitoring_data
                        .iter()
                        .map(parse_set_monitoring_data)
                        .collect();
                    let outcomes = handle_set_variable_monitoring(&actor, parsed).await;
                    let set_monitoring_result = request
                        .set_monitoring_data
                        .iter()
                        .zip(outcomes)
                        .map(|(item, outcome)| build_set_monitoring_result(item, outcome))
                        .collect();
                    Ok(SetVariableMonitoringResponse {
                        custom_data: None,
                        set_monitoring_result,
                    })
                }
            })
            .await;
        }
    }

    #[async_trait::async_trait]
    impl ClearVariableMonitoringHandler for OCPP2_1Client {
        async fn register_clear_variable_monitoring_handler(&self, actor: ChargePointActor) {
            self.on_clear_variable_monitoring(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let ids: Vec<VariableMonitorId> =
                        request.id.iter().map(|id| VariableMonitorId(*id)).collect();
                    let outcomes = handle_clear_variable_monitoring(&actor, ids).await;
                    let clear_monitoring_result = outcomes
                        .into_iter()
                        .map(|(id, outcome)| ClearMonitoringResult {
                            custom_data: None,
                            id: id.0,
                            status: map_clear_monitoring_status(outcome),
                            status_info: None,
                        })
                        .collect();
                    Ok(ClearVariableMonitoringResponse {
                        clear_monitoring_result,
                        custom_data: None,
                    })
                }
            })
            .await;
        }
    }

    /// The next `NotifyEvent.eventData.eventId` this charge point will use on a 2.1 connection -
    /// process-wide rather than per-notifier-instance, since what OCPP needs is every event this
    /// charge point ever reports to be uniquely numbered, not a count scoped to any particular
    /// `struct`. A `static` gives every [`Ocpp2_1VariableMonitorEventNotifier`] (and the bare
    /// [`OCPP2_1Client`] convenience `impl` below, which - unlike a `with_clock` wrapper - has
    /// nowhere else to keep counter state) the same shared sequence.
    static NEXT_EVENT_ID: core::sync::atomic::AtomicI64 = core::sync::atomic::AtomicI64::new(1);

    /// Consumes and returns the next value from [`NEXT_EVENT_ID`].
    fn next_event_id() -> i64 {
        NEXT_EVENT_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
    }

    /// Wraps an [`OCPP2_1Client`] with the [`Clock`] that stamps every `NotifyEvent` - the same
    /// `with_clock` shape every other timestamped adapter in this crate uses (see
    /// [`crate::meter_values::Ocpp2_1MeterValuesNotifier`]).
    pub struct Ocpp2_1VariableMonitorEventNotifier<C> {
        client: OCPP2_1Client,
        clock: C,
    }

    impl<C: Clock> Ocpp2_1VariableMonitorEventNotifier<C> {
        /// Wraps `client`, stamping every `NotifyEvent` from `clock`.
        pub fn with_clock(client: OCPP2_1Client, clock: C) -> Self {
            Self { client, clock }
        }
    }

    #[cfg(feature = "std")]
    impl Ocpp2_1VariableMonitorEventNotifier<crate::clock::SystemClock> {
        /// [`Self::with_clock`] with [`crate::clock::SystemClock`].
        pub fn new(client: OCPP2_1Client) -> Self {
            Self::with_clock(client, crate::clock::SystemClock)
        }
    }

    #[async_trait::async_trait]
    impl<C: Clock + Send + Sync> VariableMonitorEventNotifier
        for Ocpp2_1VariableMonitorEventNotifier<C>
    {
        type Error = ClientError<OCPP2_1Error>;

        async fn notify_variable_monitor_event(
            &self,
            event: &TriggeredMonitor,
        ) -> Result<(), Self::Error> {
            let request = build_notify_event_request(event, next_event_id(), self.clock.now());
            self.client.send_notify_event(request).await?;
            Ok(())
        }
    }

    /// The `std` convenience: `OCPP2_1Client` itself implements `VariableMonitorEventNotifier`,
    /// stamping from [`crate::clock::SystemClock`] - mirrors
    /// [`crate::security::SecurityEventNotifier`]'s same convenience `impl`.
    #[cfg(feature = "std")]
    #[async_trait::async_trait]
    impl VariableMonitorEventNotifier for OCPP2_1Client {
        type Error = ClientError<OCPP2_1Error>;

        async fn notify_variable_monitor_event(
            &self,
            event: &TriggeredMonitor,
        ) -> Result<(), Self::Error> {
            let request =
                build_notify_event_request(event, next_event_id(), crate::clock::SystemClock.now());
            self.send_notify_event(request).await?;
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_monitor_type_round_trips_through_the_wire_enum() {
            for kind in [
                MonitorType::UpperThreshold,
                MonitorType::LowerThreshold,
                MonitorType::Delta,
                MonitorType::Periodic,
            ] {
                assert_eq!(map_monitor_type(&wire_monitor_type(kind)), Some(kind));
            }
        }

        #[test]
        fn unsupported_wire_monitor_types_map_to_none() {
            assert_eq!(map_monitor_type(&MonitorEnum::PeriodicClockAligned), None);
            assert_eq!(map_monitor_type(&MonitorEnum::TargetDelta), None);
            assert_eq!(map_monitor_type(&MonitorEnum::TargetDeltaRelative), None);
        }

        #[test]
        fn every_set_monitoring_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_set_monitoring_status(SetMonitorOutcome::Accepted {
                    id: VariableMonitorId(1),
                    severity: 5
                }),
                SetMonitoringStatusEnum::Accepted
            );
            assert_eq!(
                map_set_monitoring_status(SetMonitorOutcome::UnknownComponent),
                SetMonitoringStatusEnum::UnknownComponent
            );
            assert_eq!(
                map_set_monitoring_status(SetMonitorOutcome::UnknownVariable),
                SetMonitoringStatusEnum::UnknownVariable
            );
            assert_eq!(
                map_set_monitoring_status(SetMonitorOutcome::UnsupportedMonitorType),
                SetMonitoringStatusEnum::UnsupportedMonitorType
            );
            assert_eq!(
                map_set_monitoring_status(SetMonitorOutcome::Rejected),
                SetMonitoringStatusEnum::Rejected
            );
            assert_eq!(
                map_set_monitoring_status(SetMonitorOutcome::Duplicate),
                SetMonitoringStatusEnum::Duplicate
            );
        }

        #[test]
        fn every_clear_monitoring_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_clear_monitoring_status(ClearMonitorOutcome::Accepted),
                ClearMonitoringStatusEnum::Accepted
            );
            assert_eq!(
                map_clear_monitoring_status(ClearMonitorOutcome::NotFound),
                ClearMonitoringStatusEnum::NotFound
            );
        }

        #[test]
        fn an_accepted_result_carries_the_assigned_id_and_severity() {
            let item = SetMonitoringData {
                component: ocpp_client::ocpp_types::v21::common::Component {
                    custom_data: None,
                    evse: None,
                    instance: None,
                    name: heapless::String::try_from("EVSE").unwrap(),
                },
                custom_data: None,
                id: None,
                periodic_event_stream: None,
                severity: 9,
                transaction: None,
                r#type: MonitorEnum::UpperThreshold,
                value: 1.0,
                variable: ocpp_client::ocpp_types::v21::common::Variable {
                    custom_data: None,
                    instance: None,
                    name: heapless::String::try_from("Temperature").unwrap(),
                },
            };

            let result = build_set_monitoring_result(
                &item,
                SetMonitorOutcome::Accepted {
                    id: VariableMonitorId(7),
                    severity: 2,
                },
            );

            assert_eq!(result.id, Some(7));
            assert_eq!(result.severity, 2);
            assert_eq!(result.status, SetMonitoringStatusEnum::Accepted);
        }

        #[test]
        fn a_rejected_result_carries_no_id_and_echoes_the_requested_severity() {
            let item = SetMonitoringData {
                component: ocpp_client::ocpp_types::v21::common::Component {
                    custom_data: None,
                    evse: None,
                    instance: None,
                    name: heapless::String::try_from("EVSE").unwrap(),
                },
                custom_data: None,
                id: None,
                periodic_event_stream: None,
                severity: 9,
                transaction: None,
                r#type: MonitorEnum::UpperThreshold,
                value: 1.0,
                variable: ocpp_client::ocpp_types::v21::common::Variable {
                    custom_data: None,
                    instance: None,
                    name: heapless::String::try_from("Temperature").unwrap(),
                },
            };

            let result = build_set_monitoring_result(&item, SetMonitorOutcome::Rejected);

            assert_eq!(result.id, None);
            assert_eq!(result.severity, 9);
        }
    }
}

/// The OCPP 2.0.1 projection - a close copy of the `ocpp_2_1` module, since 2.0.1's
/// `SetVariableMonitoring`/`ClearVariableMonitoring`/`NotifyEvent` wire shapes are effectively
/// identical to 2.1's. The one real difference: 2.0.1's `EventData` has no `severity` field (that
/// is 2.1-only) - `variableMonitoringId` and the monitor's own severity, carried on
/// `SetMonitoringResult`, are what a 2.0.1 CSMS uses instead to know how urgent an event is.
#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1 {
    use super::{
        ClearMonitorOutcome, ClearVariableMonitoringHandler, EventTrigger, MonitorType,
        SetMonitorOutcome, SetMonitorRequest, SetVariableMonitoringHandler, TriggeredMonitor,
        VariableMonitorEventNotifier, VariableMonitorId, handle_clear_variable_monitoring,
        handle_set_variable_monitoring,
    };
    use crate::actor::ChargePointActor;
    use crate::clock::Clock;
    use crate::state::{Component, Variable};
    use alloc::boxed::Box;
    use alloc::string::ToString;
    use alloc::vec::Vec;
    use ocpp_client::ClientError;
    use ocpp_client::ocpp_2_0_1::{OCPP2_0_1Client, OCPP2_0_1Error};
    use ocpp_client::ocpp_types::v201::common::{
        ClearMonitoringResult, ClearMonitoringStatusEnum, EVSE, EventData, EventNotificationEnum,
        EventTriggerEnum, MonitorEnum, SetMonitoringData, SetMonitoringResult,
        SetMonitoringStatusEnum,
    };
    use ocpp_client::ocpp_types::v201::{
        ClearVariableMonitoringResponse, NotifyEventRequest, SetVariableMonitoringResponse,
    };

    fn bounded_string<const N: usize>(value: &str) -> heapless::String<N> {
        let mut end = value.len().min(N);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        heapless::String::try_from(&value[..end]).expect("truncated to fit the wire bound")
    }

    fn map_component(component: &ocpp_client::ocpp_types::v201::common::Component) -> Component {
        let evse = component.evse.as_ref().map(|evse| {
            let evse_id = usize::try_from(evse.id).unwrap_or(usize::MAX);
            let connector_id = evse.connector_id.and_then(|id| usize::try_from(id).ok());
            (evse_id, connector_id)
        });
        Component {
            name: component.name.to_string(),
            instance: component
                .instance
                .as_ref()
                .map(|instance| instance.to_string()),
            evse,
        }
    }

    fn build_component(
        component: &Component,
    ) -> ocpp_client::ocpp_types::v201::common::Component {
        ocpp_client::ocpp_types::v201::common::Component {
            custom_data: None,
            evse: component.evse.map(|(evse_id, connector_id)| EVSE {
                connector_id: connector_id.and_then(|id| i64::try_from(id).ok()),
                custom_data: None,
                id: i64::try_from(evse_id).unwrap_or(i64::MAX),
            }),
            instance: component.instance.as_deref().map(bounded_string::<50>),
            name: bounded_string::<50>(&component.name),
        }
    }

    fn map_variable(variable: &ocpp_client::ocpp_types::v201::common::Variable) -> Variable {
        Variable {
            name: variable.name.to_string(),
            instance: variable
                .instance
                .as_ref()
                .map(|instance| instance.to_string()),
        }
    }

    fn build_variable(variable: &Variable) -> ocpp_client::ocpp_types::v201::common::Variable {
        ocpp_client::ocpp_types::v201::common::Variable {
            custom_data: None,
            instance: variable.instance.as_deref().map(bounded_string::<50>),
            name: bounded_string::<50>(&variable.name),
        }
    }

    /// `None` for `PeriodicClockAligned` - see `crate::state::MonitorType`'s docs. 2.0.1 has no
    /// `TargetDelta`/`TargetDeltaRelative` at all (those are 2.1's V2X additions), so this match
    /// is exhaustive without them.
    fn map_monitor_type(kind: &MonitorEnum) -> Option<MonitorType> {
        match kind {
            MonitorEnum::UpperThreshold => Some(MonitorType::UpperThreshold),
            MonitorEnum::LowerThreshold => Some(MonitorType::LowerThreshold),
            MonitorEnum::Delta => Some(MonitorType::Delta),
            MonitorEnum::Periodic => Some(MonitorType::Periodic),
            MonitorEnum::PeriodicClockAligned => None,
        }
    }

    /// The inverse of [`map_monitor_type`] - only ever needed to build a wire request in this
    /// module's own round-trip tests (production code never re-emits a monitor type it received;
    /// it only ever reports `severity`/`id`/`status` back, via `build_set_monitoring_result`).
    #[cfg(test)]
    fn wire_monitor_type(kind: MonitorType) -> MonitorEnum {
        match kind {
            MonitorType::UpperThreshold => MonitorEnum::UpperThreshold,
            MonitorType::LowerThreshold => MonitorEnum::LowerThreshold,
            MonitorType::Delta => MonitorEnum::Delta,
            MonitorType::Periodic => MonitorEnum::Periodic,
        }
    }

    fn parse_set_monitoring_data(item: &SetMonitoringData) -> SetMonitorRequest {
        SetMonitorRequest {
            id: item.id.map(VariableMonitorId),
            component: map_component(&item.component),
            variable: map_variable(&item.variable),
            monitor_type: map_monitor_type(&item.r#type),
            value: item.value,
            severity: item.severity,
        }
    }

    fn map_set_monitoring_status(outcome: SetMonitorOutcome) -> SetMonitoringStatusEnum {
        match outcome {
            SetMonitorOutcome::Accepted { .. } => SetMonitoringStatusEnum::Accepted,
            SetMonitorOutcome::UnknownComponent => SetMonitoringStatusEnum::UnknownComponent,
            SetMonitorOutcome::UnknownVariable => SetMonitoringStatusEnum::UnknownVariable,
            SetMonitorOutcome::UnsupportedMonitorType => {
                SetMonitoringStatusEnum::UnsupportedMonitorType
            }
            SetMonitorOutcome::Rejected => SetMonitoringStatusEnum::Rejected,
            SetMonitorOutcome::Duplicate => SetMonitoringStatusEnum::Duplicate,
        }
    }

    fn build_set_monitoring_result(
        item: &SetMonitoringData,
        outcome: SetMonitorOutcome,
    ) -> SetMonitoringResult {
        let status = map_set_monitoring_status(outcome);
        let (id, severity) = match outcome {
            SetMonitorOutcome::Accepted { id, severity } => (Some(id.0), i64::from(severity)),
            _ => (None, item.severity),
        };
        SetMonitoringResult {
            component: item.component.clone(),
            custom_data: None,
            id,
            severity,
            status,
            status_info: None,
            r#type: item.r#type.clone(),
            variable: item.variable.clone(),
        }
    }

    fn map_clear_monitoring_status(outcome: ClearMonitorOutcome) -> ClearMonitoringStatusEnum {
        match outcome {
            ClearMonitorOutcome::Accepted => ClearMonitoringStatusEnum::Accepted,
            ClearMonitorOutcome::NotFound => ClearMonitoringStatusEnum::NotFound,
        }
    }

    fn map_trigger(trigger: EventTrigger) -> EventTriggerEnum {
        match trigger {
            EventTrigger::Alerting => EventTriggerEnum::Alerting,
            EventTrigger::Delta => EventTriggerEnum::Delta,
            EventTrigger::Periodic => EventTriggerEnum::Periodic,
        }
    }

    fn build_notify_event_request(
        event: &TriggeredMonitor,
        event_id: i64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> NotifyEventRequest {
        let timestamp = now.to_rfc3339();
        NotifyEventRequest {
            custom_data: None,
            event_data: alloc::vec![EventData {
                actual_value: bounded_string::<2500>(&event.actual_value),
                cause: None,
                cleared: None,
                component: build_component(&event.component),
                custom_data: None,
                event_id,
                event_notification_type: EventNotificationEnum::CustomMonitor,
                tech_code: None,
                tech_info: None,
                timestamp: timestamp.clone(),
                transaction_id: None,
                trigger: map_trigger(event.trigger),
                variable: build_variable(&event.variable),
                variable_monitoring_id: Some(event.monitor_id.0),
            }],
            generated_at: timestamp,
            seq_no: 0,
            tbc: None,
        }
    }

    #[async_trait::async_trait]
    impl SetVariableMonitoringHandler for OCPP2_0_1Client {
        async fn register_set_variable_monitoring_handler(&self, actor: ChargePointActor) {
            self.on_set_variable_monitoring(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let parsed: Vec<SetMonitorRequest> = request
                        .set_monitoring_data
                        .iter()
                        .map(parse_set_monitoring_data)
                        .collect();
                    let outcomes = handle_set_variable_monitoring(&actor, parsed).await;
                    let set_monitoring_result = request
                        .set_monitoring_data
                        .iter()
                        .zip(outcomes)
                        .map(|(item, outcome)| build_set_monitoring_result(item, outcome))
                        .collect();
                    Ok(SetVariableMonitoringResponse {
                        custom_data: None,
                        set_monitoring_result,
                    })
                }
            })
            .await;
        }
    }

    #[async_trait::async_trait]
    impl ClearVariableMonitoringHandler for OCPP2_0_1Client {
        async fn register_clear_variable_monitoring_handler(&self, actor: ChargePointActor) {
            self.on_clear_variable_monitoring(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let ids: Vec<VariableMonitorId> =
                        request.id.iter().map(|id| VariableMonitorId(*id)).collect();
                    let outcomes = handle_clear_variable_monitoring(&actor, ids).await;
                    let clear_monitoring_result = outcomes
                        .into_iter()
                        .map(|(id, outcome)| ClearMonitoringResult {
                            custom_data: None,
                            id: id.0,
                            status: map_clear_monitoring_status(outcome),
                            status_info: None,
                        })
                        .collect();
                    Ok(ClearVariableMonitoringResponse {
                        clear_monitoring_result,
                        custom_data: None,
                    })
                }
            })
            .await;
        }
    }

    /// Mirrors [`super::ocpp_2_1::NEXT_EVENT_ID`] - a separate process-wide sequence for a 2.0.1
    /// connection's own `NotifyEvent` stream. See that constant's docs for why a `static` rather
    /// than per-instance state.
    static NEXT_EVENT_ID: core::sync::atomic::AtomicI64 = core::sync::atomic::AtomicI64::new(1);

    /// Consumes and returns the next value from [`NEXT_EVENT_ID`].
    fn next_event_id() -> i64 {
        NEXT_EVENT_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
    }

    /// Wraps an [`OCPP2_0_1Client`] with the [`Clock`] that stamps every `NotifyEvent` - mirrors
    /// [`super::ocpp_2_1::Ocpp2_1VariableMonitorEventNotifier`].
    pub struct Ocpp2_0_1VariableMonitorEventNotifier<C> {
        client: OCPP2_0_1Client,
        clock: C,
    }

    impl<C: Clock> Ocpp2_0_1VariableMonitorEventNotifier<C> {
        /// Wraps `client`, stamping every `NotifyEvent` from `clock`.
        pub fn with_clock(client: OCPP2_0_1Client, clock: C) -> Self {
            Self { client, clock }
        }
    }

    #[cfg(feature = "std")]
    impl Ocpp2_0_1VariableMonitorEventNotifier<crate::clock::SystemClock> {
        /// [`Self::with_clock`] with [`crate::clock::SystemClock`].
        pub fn new(client: OCPP2_0_1Client) -> Self {
            Self::with_clock(client, crate::clock::SystemClock)
        }
    }

    #[async_trait::async_trait]
    impl<C: Clock + Send + Sync> VariableMonitorEventNotifier
        for Ocpp2_0_1VariableMonitorEventNotifier<C>
    {
        type Error = ClientError<OCPP2_0_1Error>;

        async fn notify_variable_monitor_event(
            &self,
            event: &TriggeredMonitor,
        ) -> Result<(), Self::Error> {
            let request = build_notify_event_request(event, next_event_id(), self.clock.now());
            self.client.send_notify_event(request).await?;
            Ok(())
        }
    }

    /// The `std` convenience: `OCPP2_0_1Client` itself implements `VariableMonitorEventNotifier`,
    /// stamping from [`crate::clock::SystemClock`].
    #[cfg(feature = "std")]
    #[async_trait::async_trait]
    impl VariableMonitorEventNotifier for OCPP2_0_1Client {
        type Error = ClientError<OCPP2_0_1Error>;

        async fn notify_variable_monitor_event(
            &self,
            event: &TriggeredMonitor,
        ) -> Result<(), Self::Error> {
            let request =
                build_notify_event_request(event, next_event_id(), crate::clock::SystemClock.now());
            self.send_notify_event(request).await?;
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_monitor_type_round_trips_through_the_wire_enum() {
            for kind in [
                MonitorType::UpperThreshold,
                MonitorType::LowerThreshold,
                MonitorType::Delta,
                MonitorType::Periodic,
            ] {
                assert_eq!(map_monitor_type(&wire_monitor_type(kind)), Some(kind));
            }
        }

        #[test]
        fn periodic_clock_aligned_maps_to_none() {
            assert_eq!(map_monitor_type(&MonitorEnum::PeriodicClockAligned), None);
        }

        #[test]
        fn every_clear_monitoring_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_clear_monitoring_status(ClearMonitorOutcome::Accepted),
                ClearMonitoringStatusEnum::Accepted
            );
            assert_eq!(
                map_clear_monitoring_status(ClearMonitorOutcome::NotFound),
                ClearMonitoringStatusEnum::NotFound
            );
        }
    }
}
