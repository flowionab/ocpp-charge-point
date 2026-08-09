//! Periodic event streams functional block: `OpenPeriodicEventStream`/`ClosePeriodicEventStream`/
//! `AdjustPeriodicEventStream`/`GetPeriodicEventStream` inbound, `NotifyPeriodicEventStream`
//! outbound. See `docs/PRODUCTION-ROADMAP.md` B5.6.
//!
//! **2.1 only** - neither 1.6J nor 2.0.1 has any periodic event stream concept at all, so there is
//! no `ocpp_1_6`/`ocpp_2_0_1` projection anywhere in this functional block.
//!
//! A stream (OCPP `ConstantStreamData`) names a [`crate::state::VariableMonitorId`] to report the
//! value of, on an interval. What drives that "on an interval" half - the loop that actually
//! produces a `NotifyPeriodicEventStream` - is [`crate::periodic_event_stream::run_periodic_event_streams`], shaped exactly like
//! [`crate::variable_monitoring::run_periodic_variable_monitors`] and
//! [`crate::reservation::run_reservation_expiry`]: a fixed sweep, skipped entirely while the clock
//! is unsynchronized (see [`crate::clock::is_synchronized`]) since a stream sample's timestamp
//! would otherwise be meaningless.
//!
//! Each due sweep reports exactly one data element per stream - the monitored variable's current
//! `Actual` value, at offset `0` from a `basetime` of "now". OCPP's `PeriodicEventStreamParams.values`
//! (batching several elements into one `NotifyPeriodicEventStream`) is read and stored (so
//! `GetPeriodicEventStream` reports it faithfully) but not honoured by the driver loop - this
//! crate does not yet buffer multiple samples between sends, so every `NotifyPeriodicEventStream`
//! this crate emits carries a single data element and `pending: 0`.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use chrono::{DateTime, Utc};

use crate::actor::ChargePointActor;
use crate::clock::Clock;
use crate::provisioning::Backoff;
use crate::state::{
    ChargePointEvent, OpenPeriodicEventStream, PeriodicEventStreamId, PeriodicEventStreamParams,
    VariableAttributeType, VariableMonitorId,
};

/// The outcome of a CSMS-initiated `OpenPeriodicEventStream`, matching OCPP's `GenericStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenPeriodicEventStreamOutcome {
    /// The stream was opened.
    Accepted,
    /// The request could not be honoured - periodic event streaming isn't available, or
    /// `variable_monitoring_id` doesn't name a currently installed variable monitor, or the store
    /// is already at [`crate::state::StateLimits::max_periodic_event_streams`] and this id isn't
    /// replacing an existing stream.
    Rejected,
}

/// Handles a CSMS-initiated `OpenPeriodicEventStream` against `actor`. Like every handler in this
/// crate, the outcome is decided against the real state (via a trial open on a clone of the
/// store) before anything is dispatched, so the status the CSMS receives is what actually
/// happened - mirrors `crate::tariff::handle_set_default_tariff`.
#[tracing::instrument(skip_all)]
pub async fn handle_open_periodic_event_stream(
    actor: &ChargePointActor,
    id: PeriodicEventStreamId,
    variable_monitoring_id: VariableMonitorId,
    params: PeriodicEventStreamParams,
) -> OpenPeriodicEventStreamOutcome {
    let state = actor.state();
    if !crate::refusal::capability_present(&state.capabilities, "OpenPeriodicEventStream") {
        return OpenPeriodicEventStreamOutcome::Rejected;
    }
    if state
        .variable_monitors
        .get(variable_monitoring_id)
        .is_none()
    {
        return OpenPeriodicEventStreamOutcome::Rejected;
    }
    let mut trial = state.periodic_event_streams.clone();
    match trial.open(OpenPeriodicEventStream {
        id,
        variable_monitoring_id,
        params,
    }) {
        Ok(()) => {
            let _ = actor
                .send(ChargePointEvent::PeriodicEventStreamOpened {
                    id,
                    variable_monitoring_id,
                    params,
                })
                .await;
            OpenPeriodicEventStreamOutcome::Accepted
        }
        Err(_) => OpenPeriodicEventStreamOutcome::Rejected,
    }
}

/// The outcome of a CSMS-initiated `AdjustPeriodicEventStream`, matching OCPP's
/// `GenericStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdjustPeriodicEventStreamOutcome {
    /// The stream's parameters were replaced.
    Accepted,
    /// The request could not be honoured - periodic event streaming isn't available, or `id`
    /// doesn't name a currently open stream.
    Rejected,
}

/// Handles a CSMS-initiated `AdjustPeriodicEventStream` against `actor`.
#[tracing::instrument(skip_all)]
pub async fn handle_adjust_periodic_event_stream(
    actor: &ChargePointActor,
    id: PeriodicEventStreamId,
    params: PeriodicEventStreamParams,
) -> AdjustPeriodicEventStreamOutcome {
    let state = actor.state();
    if !crate::refusal::capability_present(&state.capabilities, "AdjustPeriodicEventStream") {
        return AdjustPeriodicEventStreamOutcome::Rejected;
    }
    if state.periodic_event_streams.get(id).is_none() {
        return AdjustPeriodicEventStreamOutcome::Rejected;
    }
    let _ = actor
        .send(ChargePointEvent::PeriodicEventStreamAdjusted { id, params })
        .await;
    AdjustPeriodicEventStreamOutcome::Accepted
}

/// Handles a CSMS-initiated `ClosePeriodicEventStream` against `actor`: closes the stream named
/// `id` if one is open. A no-op (not an error) if none is - mirrors
/// [`crate::state::PeriodicEventStreamStore::close`]'s own tolerance.
///
/// Unlike [`handle_open_periodic_event_stream`]/[`handle_adjust_periodic_event_stream`], this does
/// **not** check [`crate::refusal::capability_present`] itself - `ClosePeriodicEventStreamResponse`
/// has no status field to refuse through, so that check belongs to the wire adapter, which answers
/// a CALLERROR instead (mirrors `crate::tariff::handle_clear_tariffs`).
#[tracing::instrument(skip_all)]
pub async fn handle_close_periodic_event_stream(
    actor: &ChargePointActor,
    id: PeriodicEventStreamId,
) {
    let _ = actor
        .send(ChargePointEvent::PeriodicEventStreamClosed { id })
        .await;
}

/// Handles a CSMS-initiated `GetPeriodicEventStream` against `actor`: every stream currently open,
/// in the order they were opened.
///
/// Unlike [`handle_open_periodic_event_stream`]/[`handle_adjust_periodic_event_stream`], this does
/// **not** check [`crate::refusal::capability_present`] itself - `GetPeriodicEventStreamResponse`
/// has no status field either, only an optional list - so that check belongs to the wire adapter,
/// same reasoning as [`handle_close_periodic_event_stream`].
pub fn handle_get_periodic_event_stream(actor: &ChargePointActor) -> Vec<OpenPeriodicEventStream> {
    actor
        .state()
        .periodic_event_streams
        .iter()
        .cloned()
        .collect()
}

/// One sample [`run_periodic_event_streams`] reports for a due stream - the protocol-independent
/// shape a [`PeriodicEventStreamNotifier`] turns into a `NotifyPeriodicEventStream`.
#[derive(Debug, Clone, PartialEq)]
pub struct PeriodicStreamSample {
    /// The stream this sample belongs to.
    pub id: PeriodicEventStreamId,
    /// The timestamp every `value`'s offset is relative to - "now", at the moment this sample was
    /// taken.
    pub basetime: DateTime<Utc>,
    /// The data elements to report - see this module's docs for why this crate only ever
    /// populates one.
    pub values: Vec<StreamValue>,
    /// How many further data elements are still pending for this stream. Always `0` today - see
    /// this module's docs.
    pub pending: i64,
}

/// One `(offset, value)` pair within a [`PeriodicStreamSample`] (OCPP `StreamDataElement`).
#[derive(Debug, Clone, PartialEq)]
pub struct StreamValue {
    /// Seconds after the sample's `basetime` this value was recorded at.
    pub offset_secs: f64,
    /// The monitored variable's value, as a string - mirrors
    /// [`crate::state::VariableAttribute::value`]'s own representation.
    pub value: String,
}

/// Reports every due open stream to the CSMS via `notifier`. Register via
/// [`crate::builder::ChargePointBuilder::periodic_event_streams`], which also registers the
/// inbound `OpenPeriodicEventStream`/`ClosePeriodicEventStream`/`AdjustPeriodicEventStream`/
/// `GetPeriodicEventStream` handlers.
#[async_trait::async_trait]
pub trait PeriodicEventStreamNotifier {
    /// The error a failed `NotifyPeriodicEventStream` send produces.
    type Error: core::fmt::Display;

    /// Sends one stream sample to the CSMS.
    async fn notify_periodic_event_stream(
        &self,
        sample: PeriodicStreamSample,
    ) -> Result<(), Self::Error>;
}

/// A stream is due once its own `params.interval_secs` (or
/// [`crate::state::DEFAULT_PERIODIC_EVENT_STREAM_INTERVAL_SECS`], if unset) has elapsed since it
/// last reported - tracked locally by [`run_periodic_event_streams`], not in
/// [`crate::state::ChargePointState`] (a periodic tick is not a state change), mirroring
/// [`crate::variable_monitoring::run_periodic_variable_monitors`]'s own local tracking exactly.
///
/// A stream never reported before is due immediately, for the same reason a periodic variable
/// monitor is: this charge point cannot know how long it's been open.
///
/// Skips a stream whose monitor no longer exists, or whose monitored variable no longer has an
/// `Actual` attribute to read - reporting a value this charge point doesn't have would mean
/// inventing one.
fn due_periodic_stream_samples(
    state: &crate::state::ChargePointState,
    now: DateTime<Utc>,
    last_reported: &BTreeMap<PeriodicEventStreamId, DateTime<Utc>>,
) -> Vec<PeriodicStreamSample> {
    let mut due = Vec::new();
    for stream in state.periodic_event_streams.iter() {
        let interval_secs = stream
            .params
            .interval_secs
            .unwrap_or(crate::state::DEFAULT_PERIODIC_EVENT_STREAM_INTERVAL_SECS)
            .max(1);
        let is_due = last_reported
            .get(&stream.id)
            .is_none_or(|last| (now - *last).num_seconds() >= interval_secs);
        if !is_due {
            continue;
        }
        let Some(monitor) = state.variable_monitors.get(stream.variable_monitoring_id) else {
            continue;
        };
        let Some(definition) = state
            .device_model
            .get(&monitor.component, &monitor.variable)
        else {
            continue;
        };
        let Some(attribute) = definition.attribute(VariableAttributeType::Actual) else {
            continue;
        };
        due.push(PeriodicStreamSample {
            id: stream.id,
            basetime: now,
            values: alloc::vec![StreamValue {
                offset_secs: 0.0,
                value: attribute.value.clone(),
            }],
            pending: 0,
        });
    }
    due
}

/// Runs the sweep `due_periodic_stream_samples` (private) describes, forever, every `sweep_interval_secs`.
///
/// **Skipped entirely while the clock is unsynchronized** (see [`crate::clock::is_synchronized`]),
/// exactly like `run_reservation_expiry`/`run_periodic_variable_monitors`: a charge point that
/// does not know the time cannot honestly stamp a `NotifyPeriodicEventStream`'s `basetime`.
///
/// A failed report is logged and the stream is left due, so it's retried on the very next sweep
/// rather than silently waiting out its full interval again.
pub async fn run_periodic_event_streams<N: PeriodicEventStreamNotifier, C: Clock, B: Backoff>(
    actor: &ChargePointActor,
    notifier: &N,
    clock: &C,
    backoff: &B,
    sweep_interval_secs: u32,
) {
    let mut last_reported: BTreeMap<PeriodicEventStreamId, DateTime<Utc>> = BTreeMap::new();
    loop {
        backoff.wait(sweep_interval_secs.max(1)).await;
        let now = clock.now();
        if !crate::clock::is_synchronized(&now) {
            tracing::debug!(
                "skipping the periodic event stream sweep: the clock is not synchronized"
            );
            continue;
        }
        for sample in due_periodic_stream_samples(&actor.state(), now, &last_reported) {
            let id = sample.id;
            if let Err(err) = notifier.notify_periodic_event_stream(sample).await {
                tracing::warn!(
                    error = %err,
                    stream_id = id.0,
                    "periodic event stream notification failed"
                );
                continue;
            }
            last_reported.insert(id, now);
        }
    }
}

/// Registers this charge point's inbound `OpenPeriodicEventStream` handling with the CSMS
/// connection. Implemented per protocol version (see the `ocpp_2_1` module) - **2.1 only**, per
/// this module's docs.
#[async_trait::async_trait]
pub trait OpenPeriodicEventStreamHandler {
    /// Registers an `OpenPeriodicEventStream` handler with the CSMS connection that dispatches
    /// incoming requests to [`handle_open_periodic_event_stream`] against `actor`.
    async fn register_open_periodic_event_stream_handler(&self, actor: ChargePointActor);
}

/// Registers this charge point's inbound `ClosePeriodicEventStream` handling with the CSMS
/// connection. **2.1 only**.
#[async_trait::async_trait]
pub trait ClosePeriodicEventStreamHandler {
    /// Registers a `ClosePeriodicEventStream` handler with the CSMS connection that dispatches
    /// incoming requests to [`handle_close_periodic_event_stream`] against `actor`.
    async fn register_close_periodic_event_stream_handler(&self, actor: ChargePointActor);
}

/// Registers this charge point's inbound `AdjustPeriodicEventStream` handling with the CSMS
/// connection. **2.1 only**.
#[async_trait::async_trait]
pub trait AdjustPeriodicEventStreamHandler {
    /// Registers an `AdjustPeriodicEventStream` handler with the CSMS connection that dispatches
    /// incoming requests to [`handle_adjust_periodic_event_stream`] against `actor`.
    async fn register_adjust_periodic_event_stream_handler(&self, actor: ChargePointActor);
}

/// Registers this charge point's inbound `GetPeriodicEventStream` handling with the CSMS
/// connection. **2.1 only**.
#[async_trait::async_trait]
pub trait GetPeriodicEventStreamHandler {
    /// Registers a `GetPeriodicEventStream` handler with the CSMS connection that dispatches
    /// incoming requests to [`handle_get_periodic_event_stream`] against `actor`.
    async fn register_get_periodic_event_stream_handler(&self, actor: ChargePointActor);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::TokioExecutor;
    use crate::hardware::Capabilities;
    use crate::state::{
        Component, DeviceModelEvent, MonitorType, Variable, VariableAttribute,
        VariableAttributeType, VariableCharacteristics, VariableDataType, VariableMutability,
    };
    use crate::variable_monitoring::{
        SetMonitorOutcome, SetMonitorRequest, handle_set_variable_monitoring,
    };

    async fn actor_with_capability() -> ChargePointActor {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        actor
            .send(ChargePointEvent::CapabilitiesDeclared(Capabilities {
                periodic_event_stream: true,
                ..Default::default()
            }))
            .await
            .unwrap();
        actor
    }

    fn temperature_component() -> Component {
        Component {
            name: "EVSE".into(),
            instance: None,
            evse: None,
        }
    }

    fn temperature_variable() -> Variable {
        Variable {
            name: "Temperature".into(),
            instance: None,
        }
    }

    async fn install_monitor(actor: &ChargePointActor) -> VariableMonitorId {
        actor
            .send(ChargePointEvent::DeviceModel(
                DeviceModelEvent::VariableRegistered {
                    component: temperature_component(),
                    variable: temperature_variable(),
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
                        value: "21.5".into(),
                        mutability: VariableMutability::ReadWrite,
                        persistent: false,
                        constant: false,
                        requires_reboot: false,
                    }],
                },
            ))
            .await
            .unwrap();

        let outcomes = handle_set_variable_monitoring(
            actor,
            alloc::vec![SetMonitorRequest {
                id: None,
                component: temperature_component(),
                variable: temperature_variable(),
                monitor_type: Some(MonitorType::Periodic),
                value: 30.0,
                severity: 5,
            }],
        )
        .await;
        let outcome = outcomes.into_iter().next().unwrap();
        let SetMonitorOutcome::Accepted { id, .. } = outcome else {
            panic!("expected the test monitor to be accepted");
        };
        id
    }

    fn params(interval_secs: Option<i64>) -> PeriodicEventStreamParams {
        PeriodicEventStreamParams {
            interval_secs,
            values: None,
        }
    }

    #[tokio::test]
    async fn open_installs_a_stream_against_an_existing_monitor() {
        let actor = actor_with_capability().await;
        let monitor_id = install_monitor(&actor).await;

        let outcome = handle_open_periodic_event_stream(
            &actor,
            PeriodicEventStreamId(1),
            monitor_id,
            params(Some(30)),
        )
        .await;

        assert_eq!(outcome, OpenPeriodicEventStreamOutcome::Accepted);
        assert_eq!(actor.state().periodic_event_streams.len(), 1);
    }

    #[tokio::test]
    async fn open_is_rejected_without_the_capability() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome = handle_open_periodic_event_stream(
            &actor,
            PeriodicEventStreamId(1),
            VariableMonitorId(1),
            params(None),
        )
        .await;

        assert_eq!(outcome, OpenPeriodicEventStreamOutcome::Rejected);
    }

    #[tokio::test]
    async fn open_is_rejected_for_an_unknown_monitor() {
        let actor = actor_with_capability().await;

        let outcome = handle_open_periodic_event_stream(
            &actor,
            PeriodicEventStreamId(1),
            VariableMonitorId(999),
            params(None),
        )
        .await;

        assert_eq!(outcome, OpenPeriodicEventStreamOutcome::Rejected);
        assert!(actor.state().periodic_event_streams.is_empty());
    }

    #[tokio::test]
    async fn open_beyond_the_bound_is_rejected() {
        let actor = actor_with_capability().await;
        let monitor_id = install_monitor(&actor).await;
        for i in 0..actor.state().periodic_event_streams.max_streams() {
            handle_open_periodic_event_stream(
                &actor,
                PeriodicEventStreamId(i as i64),
                monitor_id,
                params(None),
            )
            .await;
        }

        let outcome = handle_open_periodic_event_stream(
            &actor,
            PeriodicEventStreamId(9999),
            monitor_id,
            params(None),
        )
        .await;

        assert_eq!(outcome, OpenPeriodicEventStreamOutcome::Rejected);
    }

    #[tokio::test]
    async fn adjust_replaces_an_open_streams_params() {
        let actor = actor_with_capability().await;
        let monitor_id = install_monitor(&actor).await;
        handle_open_periodic_event_stream(
            &actor,
            PeriodicEventStreamId(1),
            monitor_id,
            params(Some(30)),
        )
        .await;

        let outcome =
            handle_adjust_periodic_event_stream(&actor, PeriodicEventStreamId(1), params(Some(5)))
                .await;

        assert_eq!(outcome, AdjustPeriodicEventStreamOutcome::Accepted);
        assert_eq!(
            actor
                .state()
                .periodic_event_streams
                .get(PeriodicEventStreamId(1))
                .unwrap()
                .params
                .interval_secs,
            Some(5)
        );
    }

    #[tokio::test]
    async fn adjust_an_unknown_stream_is_rejected() {
        let actor = actor_with_capability().await;

        let outcome = handle_adjust_periodic_event_stream(
            &actor,
            PeriodicEventStreamId(999),
            params(Some(5)),
        )
        .await;

        assert_eq!(outcome, AdjustPeriodicEventStreamOutcome::Rejected);
    }

    #[tokio::test]
    async fn adjust_is_rejected_without_the_capability() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome =
            handle_adjust_periodic_event_stream(&actor, PeriodicEventStreamId(1), params(Some(5)))
                .await;

        assert_eq!(outcome, AdjustPeriodicEventStreamOutcome::Rejected);
    }

    #[tokio::test]
    async fn close_removes_an_open_stream() {
        let actor = actor_with_capability().await;
        let monitor_id = install_monitor(&actor).await;
        handle_open_periodic_event_stream(
            &actor,
            PeriodicEventStreamId(1),
            monitor_id,
            params(None),
        )
        .await;

        handle_close_periodic_event_stream(&actor, PeriodicEventStreamId(1)).await;

        assert!(actor.state().periodic_event_streams.is_empty());
    }

    #[tokio::test]
    async fn closing_an_unknown_stream_is_a_no_op() {
        let actor = actor_with_capability().await;

        handle_close_periodic_event_stream(&actor, PeriodicEventStreamId(999)).await;

        assert!(actor.state().periodic_event_streams.is_empty());
    }

    #[tokio::test]
    async fn get_reports_every_open_stream() {
        let actor = actor_with_capability().await;
        let monitor_id = install_monitor(&actor).await;
        handle_open_periodic_event_stream(
            &actor,
            PeriodicEventStreamId(1),
            monitor_id,
            params(None),
        )
        .await;
        handle_open_periodic_event_stream(
            &actor,
            PeriodicEventStreamId(2),
            monitor_id,
            params(None),
        )
        .await;

        let streams = handle_get_periodic_event_stream(&actor);

        assert_eq!(streams.len(), 2);
    }

    #[tokio::test]
    async fn a_stream_never_reported_before_is_due_immediately() {
        let actor = actor_with_capability().await;
        let monitor_id = install_monitor(&actor).await;
        handle_open_periodic_event_stream(
            &actor,
            PeriodicEventStreamId(1),
            monitor_id,
            params(Some(30)),
        )
        .await;

        let now = chrono::Utc::now();
        let due = due_periodic_stream_samples(&actor.state(), now, &BTreeMap::new());

        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, PeriodicEventStreamId(1));
        assert_eq!(due[0].values.len(), 1);
        assert_eq!(due[0].values[0].value, "21.5");
        assert_eq!(due[0].pending, 0);
    }

    #[tokio::test]
    async fn a_stream_reported_recently_is_not_due_again() {
        let actor = actor_with_capability().await;
        let monitor_id = install_monitor(&actor).await;
        handle_open_periodic_event_stream(
            &actor,
            PeriodicEventStreamId(1),
            monitor_id,
            params(Some(30)),
        )
        .await;

        let now = chrono::Utc::now();
        let mut last_reported = BTreeMap::new();
        last_reported.insert(PeriodicEventStreamId(1), now);

        let due = due_periodic_stream_samples(&actor.state(), now, &last_reported);

        assert!(due.is_empty());
    }

    #[tokio::test]
    async fn a_stream_past_its_interval_is_due_again() {
        let actor = actor_with_capability().await;
        let monitor_id = install_monitor(&actor).await;
        handle_open_periodic_event_stream(
            &actor,
            PeriodicEventStreamId(1),
            monitor_id,
            params(Some(30)),
        )
        .await;

        let last_tick = chrono::Utc::now();
        let mut last_reported = BTreeMap::new();
        last_reported.insert(PeriodicEventStreamId(1), last_tick);
        let now = last_tick + chrono::Duration::seconds(31);

        let due = due_periodic_stream_samples(&actor.state(), now, &last_reported);

        assert_eq!(due.len(), 1);
    }

    #[tokio::test]
    async fn a_stream_whose_monitor_disappeared_is_skipped() {
        let actor = actor_with_capability().await;
        let monitor_id = install_monitor(&actor).await;
        handle_open_periodic_event_stream(
            &actor,
            PeriodicEventStreamId(1),
            monitor_id,
            params(Some(30)),
        )
        .await;
        actor
            .send(ChargePointEvent::VariableMonitoring(
                crate::state::VariableMonitoringEvent::MonitorCleared { id: monitor_id },
            ))
            .await
            .unwrap();

        let due = due_periodic_stream_samples(&actor.state(), chrono::Utc::now(), &BTreeMap::new());

        assert!(due.is_empty());
    }
}

#[cfg(feature = "ocpp_2_1")]
pub mod ocpp_2_1 {
    //! OCPP 2.1 wire adapter for `OpenPeriodicEventStream`/`ClosePeriodicEventStream`/
    //! `AdjustPeriodicEventStream`/`GetPeriodicEventStream`/`NotifyPeriodicEventStream` - the only
    //! version with periodic event stream messages at all (see this module's parent docs).

    use super::{
        AdjustPeriodicEventStreamHandler, AdjustPeriodicEventStreamOutcome,
        ClosePeriodicEventStreamHandler, GetPeriodicEventStreamHandler,
        OpenPeriodicEventStreamHandler, OpenPeriodicEventStreamOutcome,
        PeriodicEventStreamNotifier, PeriodicStreamSample, StreamValue,
        handle_adjust_periodic_event_stream, handle_close_periodic_event_stream,
        handle_get_periodic_event_stream, handle_open_periodic_event_stream,
    };
    use crate::actor::ChargePointActor;
    use crate::state::{
        OpenPeriodicEventStream, PeriodicEventStreamId, PeriodicEventStreamParams,
        VariableMonitorId,
    };
    use alloc::boxed::Box;
    use alloc::vec::Vec;

    use crate::wire::v21::common::{ConstantStreamData, GenericStatusEnum, StreamDataElement};
    use crate::wire::v21::{
        AdjustPeriodicEventStreamRequest, AdjustPeriodicEventStreamResponse,
        ClosePeriodicEventStreamRequest, ClosePeriodicEventStreamResponse,
        GetPeriodicEventStreamRequest, GetPeriodicEventStreamResponse,
        NotifyPeriodicEventStream as WireNotifyPeriodicEventStream, OpenPeriodicEventStreamRequest,
        OpenPeriodicEventStreamResponse,
    };
    use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};

    fn wire_params_to_internal(
        params: &crate::wire::v21::common::PeriodicEventStreamParams,
    ) -> PeriodicEventStreamParams {
        PeriodicEventStreamParams {
            interval_secs: params.interval,
            values: params.values,
        }
    }

    fn internal_params_to_wire(
        params: PeriodicEventStreamParams,
    ) -> crate::wire::v21::common::PeriodicEventStreamParams {
        crate::wire::v21::common::PeriodicEventStreamParams {
            custom_data: None,
            interval: params.interval_secs,
            values: params.values,
        }
    }

    fn stream_to_wire(stream: &OpenPeriodicEventStream) -> ConstantStreamData {
        ConstantStreamData {
            custom_data: None,
            id: stream.id.0,
            params: internal_params_to_wire(stream.params),
            variable_monitoring_id: stream.variable_monitoring_id.0,
        }
    }

    async fn handle_open(
        actor: &ChargePointActor,
        request: &OpenPeriodicEventStreamRequest,
    ) -> Result<OpenPeriodicEventStreamResponse, OCPP2_1Error> {
        let id = PeriodicEventStreamId(request.constant_stream_data.id);
        let variable_monitoring_id =
            VariableMonitorId(request.constant_stream_data.variable_monitoring_id);
        let params = wire_params_to_internal(&request.constant_stream_data.params);
        let outcome =
            handle_open_periodic_event_stream(actor, id, variable_monitoring_id, params).await;
        let status = match outcome {
            OpenPeriodicEventStreamOutcome::Accepted => GenericStatusEnum::Accepted,
            OpenPeriodicEventStreamOutcome::Rejected => GenericStatusEnum::Rejected,
        };
        Ok(OpenPeriodicEventStreamResponse {
            custom_data: None,
            status,
            status_info: None,
        })
    }

    #[async_trait::async_trait]
    impl OpenPeriodicEventStreamHandler for OCPP2_1Client {
        async fn register_open_periodic_event_stream_handler(&self, actor: ChargePointActor) {
            self.on_open_periodic_event_stream(move |request, _client| {
                let actor = actor.clone();
                async move { handle_open(&actor, &request).await }
            })
            .await;
        }
    }

    async fn handle_adjust(
        actor: &ChargePointActor,
        request: &AdjustPeriodicEventStreamRequest,
    ) -> Result<AdjustPeriodicEventStreamResponse, OCPP2_1Error> {
        let id = PeriodicEventStreamId(request.id);
        let params = wire_params_to_internal(&request.params);
        let outcome = handle_adjust_periodic_event_stream(actor, id, params).await;
        let status = match outcome {
            AdjustPeriodicEventStreamOutcome::Accepted => GenericStatusEnum::Accepted,
            AdjustPeriodicEventStreamOutcome::Rejected => GenericStatusEnum::Rejected,
        };
        Ok(AdjustPeriodicEventStreamResponse {
            custom_data: None,
            status,
            status_info: None,
        })
    }

    #[async_trait::async_trait]
    impl AdjustPeriodicEventStreamHandler for OCPP2_1Client {
        async fn register_adjust_periodic_event_stream_handler(&self, actor: ChargePointActor) {
            self.on_adjust_periodic_event_stream(move |request, _client| {
                let actor = actor.clone();
                async move { handle_adjust(&actor, &request).await }
            })
            .await;
        }
    }

    /// `ClosePeriodicEventStreamResponse` carries no status field, so a runtime-absent capability
    /// must refuse with a CALLERROR rather than an optimistic empty result - mirrors
    /// `crate::tariff::ocpp_2_1::handle_clear`.
    async fn handle_close(
        actor: &ChargePointActor,
        request: &ClosePeriodicEventStreamRequest,
    ) -> Result<ClosePeriodicEventStreamResponse, OCPP2_1Error> {
        if !crate::refusal::capability_present(
            &actor.state().capabilities,
            "ClosePeriodicEventStream",
        ) {
            return Err(crate::refusal::ocpp_2_1_not_supported(
                "ClosePeriodicEventStream",
            ));
        }
        handle_close_periodic_event_stream(actor, PeriodicEventStreamId(request.id)).await;
        Ok(ClosePeriodicEventStreamResponse { custom_data: None })
    }

    #[async_trait::async_trait]
    impl ClosePeriodicEventStreamHandler for OCPP2_1Client {
        async fn register_close_periodic_event_stream_handler(&self, actor: ChargePointActor) {
            self.on_close_periodic_event_stream(move |request, _client| {
                let actor = actor.clone();
                async move { handle_close(&actor, &request).await }
            })
            .await;
        }
    }

    /// `GetPeriodicEventStreamResponse` also carries no status field - same reasoning as
    /// [`handle_close`].
    fn handle_get(
        actor: &ChargePointActor,
        _request: &GetPeriodicEventStreamRequest,
    ) -> Result<GetPeriodicEventStreamResponse, OCPP2_1Error> {
        if !crate::refusal::capability_present(
            &actor.state().capabilities,
            "GetPeriodicEventStream",
        ) {
            return Err(crate::refusal::ocpp_2_1_not_supported(
                "GetPeriodicEventStream",
            ));
        }
        let streams = handle_get_periodic_event_stream(actor);
        let constant_stream_data = if streams.is_empty() {
            None
        } else {
            Some(streams.iter().map(stream_to_wire).collect())
        };
        Ok(GetPeriodicEventStreamResponse {
            constant_stream_data,
            custom_data: None,
        })
    }

    #[async_trait::async_trait]
    impl GetPeriodicEventStreamHandler for OCPP2_1Client {
        async fn register_get_periodic_event_stream_handler(&self, actor: ChargePointActor) {
            self.on_get_periodic_event_stream(move |request, _client| {
                let actor = actor.clone();
                async move { handle_get(&actor, &request) }
            })
            .await;
        }
    }

    #[async_trait::async_trait]
    impl PeriodicEventStreamNotifier for OCPP2_1Client {
        type Error = ocpp_client::ClientError<OCPP2_1Error>;

        async fn notify_periodic_event_stream(
            &self,
            sample: PeriodicStreamSample,
        ) -> Result<(), Self::Error> {
            let data: Vec<StreamDataElement> = sample
                .values
                .into_iter()
                .map(|StreamValue { offset_secs, value }| StreamDataElement {
                    custom_data: None,
                    t: offset_secs,
                    v: value,
                })
                .collect();
            self.send_notify_periodic_event_stream(WireNotifyPeriodicEventStream {
                basetime: sample.basetime.into(),
                custom_data: None,
                data,
                id: sample.id.0,
                pending: sample.pending,
            })
            .await
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::executor::TokioExecutor;
        use crate::hardware::Capabilities;
        use crate::state::ChargePointEvent;
        use crate::wire::v21::RpcErrorCode;

        fn wire_open_request(
            id: i64,
            variable_monitoring_id: i64,
        ) -> OpenPeriodicEventStreamRequest {
            OpenPeriodicEventStreamRequest {
                constant_stream_data: ConstantStreamData {
                    custom_data: None,
                    id,
                    params: crate::wire::v21::common::PeriodicEventStreamParams {
                        custom_data: None,
                        interval: Some(30),
                        values: None,
                    },
                    variable_monitoring_id,
                },
                custom_data: None,
            }
        }

        #[tokio::test]
        async fn open_is_rejected_when_the_capability_is_absent() {
            let actor = ChargePointActor::spawn([1], &TokioExecutor);
            let request = wire_open_request(1, 1);

            let response = handle_open(&actor, &request).await.unwrap();

            assert_eq!(response.status, GenericStatusEnum::Rejected);
        }

        #[tokio::test]
        async fn open_is_rejected_for_an_unknown_monitor_over_the_wire() {
            let actor = ChargePointActor::spawn([1], &TokioExecutor);
            actor
                .send(ChargePointEvent::CapabilitiesDeclared(Capabilities {
                    periodic_event_stream: true,
                    ..Default::default()
                }))
                .await
                .unwrap();
            let request = wire_open_request(1, 999);

            let response = handle_open(&actor, &request).await.unwrap();

            assert_eq!(response.status, GenericStatusEnum::Rejected);
        }

        #[tokio::test]
        async fn close_is_a_call_error_when_the_capability_is_absent() {
            let actor = ChargePointActor::spawn([1], &TokioExecutor);
            let request = ClosePeriodicEventStreamRequest {
                custom_data: None,
                id: 1,
            };

            let result = handle_close(&actor, &request).await;

            assert_eq!(result.unwrap_err().code, RpcErrorCode::NotSupported);
        }

        #[tokio::test]
        async fn get_is_a_call_error_when_the_capability_is_absent() {
            let actor = ChargePointActor::spawn([1], &TokioExecutor);
            let request = GetPeriodicEventStreamRequest { custom_data: None };

            let result = handle_get(&actor, &request);

            assert_eq!(result.unwrap_err().code, RpcErrorCode::NotSupported);
        }

        #[tokio::test]
        async fn get_reports_no_data_with_nothing_open() {
            let actor = ChargePointActor::spawn([1], &TokioExecutor);
            actor
                .send(ChargePointEvent::CapabilitiesDeclared(Capabilities {
                    periodic_event_stream: true,
                    ..Default::default()
                }))
                .await
                .unwrap();
            let request = GetPeriodicEventStreamRequest { custom_data: None };

            let response = handle_get(&actor, &request).unwrap();

            assert_eq!(response.constant_stream_data, None);
        }
    }
}
