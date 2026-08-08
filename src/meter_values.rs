//! Standalone `MeterValues` - meter readings reported outside a transaction
//! (`docs/ROADMAP.md` §10, `docs/PRODUCTION-ROADMAP.md` B1.1).
//!
//! # Why this exists alongside `TransactionEvent`
//!
//! 2.x carries meter data *inside* `TransactionEvent`, which covers everything a session
//! measures. What it cannot express is OCPP's **clock-aligned data**: readings due on the wall
//! clock (every 15 minutes on the quarter hour, say) whether or not anything is charging, so that
//! a CSMS can bill standing consumption and reconcile a meter across sessions. That needs the
//! standalone message, and a reading that outlives the transaction which happened to produce it -
//! [`crate::state::EvseState::latest_meter_samples`].
//!
//! # What drives it
//!
//! `AlignedDataCtrlr`/`Interval`, read fresh from the device model on every cycle exactly as
//! [`crate::provisioning::run_heartbeat`] reads `HeartbeatInterval` - so a CSMS enabling or
//! changing it via `SetVariables` (2.x) or `ChangeConfiguration` (1.6J's
//! `ClockAlignedDataInterval`, aliased onto the same variable) takes effect on the next cycle
//! without a reboot. It defaults to `0`, which OCPP defines as "clock-aligned data is disabled" -
//! so a charge point that registers this block but is never configured for aligned data sends
//! nothing, which is the correct behaviour rather than a silent no-op.
//!
//! The companion variable `SampledDataCtrlr`/`TxUpdatedInterval` governs how often *transaction*
//! meter data is reported, and is **not** honoured yet: this crate never polls hardware for a
//! reading - the integrator pushes them in via
//! [`ConnectorEvent::MeterValueSampled`](crate::state::ConnectorEvent::MeterValueSampled) - so the
//! sampling rate is the binding's choice today. Throttling what reaches the CSMS to that interval
//! is real, contained work, and it is listed as outstanding rather than papered over here.

use alloc::boxed::Box;
use alloc::vec::Vec;
use chrono::{DateTime, Timelike, Utc};

use crate::actor::ChargePointActor;
use crate::clock::Clock;
use crate::provisioning::Backoff;
use crate::state::{Component, MeterSample, Variable, VariableAttributeType};

#[cfg(feature = "ocpp_1_6")]
pub use self::ocpp_1_6::Ocpp1_6MeterValuesNotifier;
#[cfg(feature = "ocpp_2_0_1")]
pub use self::ocpp_2_0_1::Ocpp2_0_1MeterValuesNotifier;
#[cfg(feature = "ocpp_2_1")]
pub use self::ocpp_2_1::Ocpp2_1MeterValuesNotifier;

/// How long [`run_aligned_meter_values`] sleeps before re-reading the interval while clock-aligned
/// data is disabled (`AlignedDataCtrlr`/`Interval` absent or `0`).
///
/// A minute costs nothing and means a CSMS that enables aligned data mid-session sees it start
/// within a minute rather than at the next reboot. Polling a device-model variable is a
/// `state()` read, not a wire round trip.
const DISABLED_POLL_SECS: u32 = 60;

/// Reports a meter reading to the CSMS via standalone `MeterValues`. Implemented per protocol
/// version, mirroring [`crate::availability::StatusNotifier`].
#[async_trait::async_trait]
pub trait MeterValuesNotifier {
    /// The error type returned if the `MeterValues` request itself fails.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Reports `sample`, taken on the given connector, as a clock-aligned reading.
    ///
    /// Implementations stamp the reading with their own [`Clock`]; the sample itself carries no
    /// timestamp, because the state machine that recorded it is deliberately clock-free (see
    /// [`crate::clock`]).
    async fn send_meter_values(
        &self,
        evse_id: usize,
        connector_id: usize,
        sample: MeterSample,
    ) -> Result<(), Self::Error>;
}

/// The `(Component, Variable)` this crate's device model uses for the clock-aligned data interval
/// - see [`crate::state::DeviceModel::register_defaults`].
fn aligned_data_interval_variable() -> (Component, Variable) {
    (
        Component {
            name: "AlignedDataCtrlr".into(),
            instance: None,
            evse: None,
        },
        Variable {
            name: "Interval".into(),
            instance: None,
        },
    )
}

/// Reads `actor`'s current `AlignedDataCtrlr`/`Interval`, in whole seconds.
///
/// `None` means clock-aligned data is off - the variable isn't registered, has no `Actual`
/// attribute, doesn't parse, or parses to `0`, which OCPP itself defines as disabled. A `0` would
/// also turn the loop below into a busy-spin, so the two readings of "0" agree.
pub fn aligned_data_interval_secs(actor: &ChargePointActor) -> Option<u32> {
    let (component, variable) = aligned_data_interval_variable();
    let state = actor.state();
    let value = state
        .device_model
        .get(&component, &variable)?
        .attribute(VariableAttributeType::Actual)?
        .value
        .parse::<u32>()
        .ok()?;
    (value != 0).then_some(value)
}

/// Seconds from `now` until the next instant aligned to `interval_secs` **on the wall clock** -
/// the "clock-aligned" in clock-aligned data.
///
/// A 900-second interval fires at :00, :15, :30 and :45 past the hour, not 900 seconds after
/// whenever this charge point happened to boot; two charge points on the same site report at the
/// same instants, which is what makes the readings comparable. Alignment is computed against
/// midnight UTC, so intervals that divide a day evenly (the ones OCPP's own examples use) land
/// where an operator expects.
///
/// Always at least 1, so a boundary that is exactly now sleeps rather than spinning.
pub fn secs_until_next_aligned(now: DateTime<Utc>, interval_secs: u32) -> u32 {
    let interval = i64::from(interval_secs.max(1));
    let since_midnight =
        i64::from(now.hour()) * 3_600 + i64::from(now.minute()) * 60 + i64::from(now.second());
    let into_period = since_midnight % interval;
    u32::try_from(interval - into_period).unwrap_or(1).max(1)
}

/// Sends a standalone `MeterValues` for every connector with a known reading, every
/// `AlignedDataCtrlr`/`Interval` seconds, aligned to the wall clock - forever.
///
/// Reads the interval fresh on every cycle (see the module docs), and re-checks once a minute
/// while it is `0`/absent rather than exiting, so the block can be enabled at runtime. A send failure is logged and does not stop the loop: the next aligned reading is
/// still due on schedule. Callers wanting a failed reading *queued* rather than dropped should
/// wrap this the way [`crate::setup`] wraps the other report streams
/// ([`crate::offline_queue`]) - clock-aligned data is deliberately not queued by default, since a
/// reading whose whole meaning is "this is the value at 10:15" is worth much less an hour later,
/// and the meter register it came from is cumulative anyway.
pub async fn run_aligned_meter_values<N: MeterValuesNotifier, B: Backoff, C: Clock>(
    notifier: &N,
    backoff: &B,
    clock: &C,
    actor: &ChargePointActor,
) {
    loop {
        let Some(interval_secs) = aligned_data_interval_secs(actor) else {
            backoff.wait(DISABLED_POLL_SECS).await;
            continue;
        };
        backoff
            .wait(secs_until_next_aligned(clock.now(), interval_secs))
            .await;
        for (evse_id, connector_id, sample) in aligned_readings(actor) {
            if let Err(err) = notifier
                .send_meter_values(evse_id, connector_id, sample)
                .await
            {
                tracing::warn!(
                    error = %err,
                    evse_id,
                    connector_id,
                    "standalone MeterValues failed"
                );
            }
        }
    }
}

/// Every connector's latest reading, as `(evse_id, connector_id, sample)`. Connectors whose
/// hardware has never pushed one are skipped - reporting a reading this charge point does not have
/// would mean inventing a number.
fn aligned_readings(actor: &ChargePointActor) -> Vec<(usize, usize, MeterSample)> {
    let state = actor.state();
    let mut readings = Vec::new();
    for (evse_id, evse) in state.evses.iter().enumerate() {
        for (connector_id, sample) in evse.latest_meter_samples.iter().enumerate() {
            if let Some(sample) = sample {
                readings.push((evse_id, connector_id, *sample));
            }
        }
    }
    readings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::TokioExecutor;
    use crate::state::{
        ChargePointEvent, ConnectorEvent, DeviceModelEvent, EvseEvent, VariableAttribute,
        VariableDataType, VariableMutability,
    };

    fn at(hour: u32, minute: u32, second: u32) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&alloc::format!(
            "2026-03-04T{hour:02}:{minute:02}:{second:02}Z"
        ))
        .unwrap()
        .with_timezone(&Utc)
    }

    async fn set_interval(actor: &ChargePointActor, secs: &str) {
        let (component, variable) = aligned_data_interval_variable();
        let _ = actor
            .send(ChargePointEvent::DeviceModel(
                DeviceModelEvent::AttributeValueSet {
                    component,
                    variable,
                    attribute_type: VariableAttributeType::Actual,
                    value: secs.into(),
                },
            ))
            .await;
    }

    #[test]
    fn a_quarter_hourly_interval_lands_on_the_quarter_hour() {
        assert_eq!(secs_until_next_aligned(at(10, 0, 0), 900), 900);
        assert_eq!(secs_until_next_aligned(at(10, 7, 30), 900), 450);
        assert_eq!(secs_until_next_aligned(at(10, 14, 59), 900), 1);
        assert_eq!(secs_until_next_aligned(at(10, 15, 0), 900), 900);
    }

    #[test]
    fn alignment_is_to_the_wall_clock_not_to_when_the_charge_point_started() {
        // Two charge points, booted at different times, both report at :00 and :30.
        assert_eq!(secs_until_next_aligned(at(9, 3, 0), 1_800), 27 * 60);
        assert_eq!(secs_until_next_aligned(at(9, 44, 0), 1_800), 16 * 60);
    }

    #[test]
    fn a_boundary_that_is_exactly_now_sleeps_rather_than_spinning() {
        // Midnight is aligned to every interval, so the naive answer would be 0.
        assert_eq!(secs_until_next_aligned(at(0, 0, 0), 900), 900);
        // And a degenerate interval can't produce a zero-length sleep either.
        assert!(secs_until_next_aligned(at(10, 0, 0), 0) >= 1);
    }

    #[tokio::test]
    async fn the_interval_is_read_from_the_device_model_and_zero_means_disabled() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        // The built-in default is `0` - clock-aligned data is off until a CSMS asks for it.
        assert_eq!(aligned_data_interval_secs(&actor), None);

        set_interval(&actor, "900").await;
        assert_eq!(aligned_data_interval_secs(&actor), Some(900));

        set_interval(&actor, "0").await;
        assert_eq!(aligned_data_interval_secs(&actor), None);

        // A value a CSMS could write that doesn't parse must not be honoured as some other number.
        set_interval(&actor, "every quarter hour").await;
        assert_eq!(aligned_data_interval_secs(&actor), None);
    }

    #[tokio::test]
    async fn the_interval_variable_is_registered_by_default_and_is_writable() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let (component, variable) = aligned_data_interval_variable();
        let state = actor.state();
        let definition = state
            .device_model
            .get(&component, &variable)
            .expect("AlignedDataCtrlr.Interval should be a built-in default");

        assert_eq!(
            definition.characteristics.data_type,
            VariableDataType::Integer
        );
        let attribute = definition
            .attribute(VariableAttributeType::Actual)
            .expect("an Actual attribute");
        assert_eq!(attribute.value, "0");
        assert_eq!(attribute.mutability, VariableMutability::ReadWrite);
        // Nothing about the value is worth surviving a reboot - the CSMS re-sends it, and a stale
        // interval is worse than none.
        let _: &VariableAttribute = attribute;
    }

    #[tokio::test]
    async fn only_connectors_with_a_reading_are_reported() {
        let actor = ChargePointActor::spawn([2], &TokioExecutor);

        assert!(aligned_readings(&actor).is_empty());

        let _ = actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 1,
                    event: ConnectorEvent::MeterValueSampled(MeterSample {
                        energy_wh: 4_200,
                        ..Default::default()
                    }),
                },
            })
            .await;

        let readings = aligned_readings(&actor);
        assert_eq!(readings.len(), 1);
        assert_eq!(readings[0].0, 0);
        assert_eq!(readings[0].1, 1);
        assert_eq!(readings[0].2.energy_wh, 4_200);
    }

    #[tokio::test]
    async fn a_reading_taken_with_no_transaction_is_still_reported() {
        // The whole point of clock-aligned data: it does not need a session.
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let _ = actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::MeterValueSampled(MeterSample {
                        energy_wh: 12_000,
                        ..Default::default()
                    }),
                },
            })
            .await;

        assert!(actor.state().evses[0].transactions[0].is_none());
        assert_eq!(aligned_readings(&actor).len(), 1);
    }

    /// A `MeterValuesNotifier` recording what it was asked to send.
    #[derive(Clone, Default)]
    struct RecordingNotifier {
        sent: alloc::sync::Arc<std::sync::Mutex<Vec<(usize, usize, i64)>>>,
    }

    #[async_trait::async_trait]
    impl MeterValuesNotifier for RecordingNotifier {
        type Error = core::convert::Infallible;

        async fn send_meter_values(
            &self,
            evse_id: usize,
            connector_id: usize,
            sample: MeterSample,
        ) -> Result<(), Self::Error> {
            self.sent
                .lock()
                .unwrap()
                .push((evse_id, connector_id, sample.energy_wh));
            Ok(())
        }
    }

    /// A [`Backoff`] that records what it was asked to wait and returns immediately, so a loop
    /// under test runs at full speed while its *intended* schedule stays assertable.
    #[derive(Clone, Default)]
    struct RecordingBackoff {
        waits: alloc::sync::Arc<std::sync::Mutex<Vec<u32>>>,
    }

    #[async_trait::async_trait]
    impl Backoff for RecordingBackoff {
        async fn wait(&self, seconds: u32) {
            self.waits.lock().unwrap().push(seconds);
            tokio::task::yield_now().await;
        }
    }

    struct FixedClock(DateTime<Utc>);

    impl Clock for FixedClock {
        fn now(&self) -> DateTime<Utc> {
            self.0
        }
    }

    #[tokio::test]
    async fn the_loop_polls_while_disabled_and_reports_once_enabled() {
        let actor = alloc::sync::Arc::new(ChargePointActor::spawn([1], &TokioExecutor));
        let notifier = RecordingNotifier::default();
        let backoff = RecordingBackoff::default();
        let _ = actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::MeterValueSampled(MeterSample {
                        energy_wh: 4_200,
                        ..Default::default()
                    }),
                },
            })
            .await;

        let task_actor = actor.clone();
        let task_notifier = notifier.clone();
        let task_backoff = backoff.clone();
        let task = tokio::spawn(async move {
            run_aligned_meter_values(
                &task_notifier,
                &task_backoff,
                &FixedClock(at(10, 7, 30)),
                &task_actor,
            )
            .await;
        });

        // Disabled: the loop parks on the poll interval and sends nothing.
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(notifier.sent.lock().unwrap().is_empty());
        assert!(
            backoff
                .waits
                .lock()
                .unwrap()
                .iter()
                .all(|secs| *secs == DISABLED_POLL_SECS)
        );

        // Enabled: the next cycle waits until the aligned boundary, then reports.
        set_interval(&actor, "900").await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        task.abort();

        let sent = notifier.sent.lock().unwrap().clone();
        assert!(!sent.is_empty());
        assert_eq!(sent[0], (0, 0, 4_200));
        assert!(
            backoff.waits.lock().unwrap().contains(&450),
            "the loop should have waited until 10:15, 450 s away, not a whole interval"
        );
    }
}

/// OCPP 2.1's standalone `MeterValues`.
#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use super::MeterValuesNotifier;
    use crate::clock::{Clock, is_synchronized};
    use crate::state::MeterSample;
    use alloc::boxed::Box;
    use ocpp_client::ClientError;
    use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};
    use ocpp_client::ocpp_types::v21::MeterValuesRequest;

    /// Builds the wire request for one reading. The `meterValue` body itself comes from
    /// [`crate::transactions`]' own builder rather than a second copy - see that function's docs.
    fn build_meter_values_request<C: Clock>(
        clock: &C,
        evse_id: usize,
        sample: MeterSample,
    ) -> MeterValuesRequest {
        let now = clock.now();
        if !is_synchronized(&now) {
            tracing::warn!(
                timestamp = %now,
                "MeterValues timestamp sourced from an unsynchronized clock"
            );
        }
        MeterValuesRequest {
            custom_data: None,
            // 2.x numbers EVSEs from 1; `0` addresses the charge point itself, which cannot have
            // taken a connector reading.
            evse_id: evse_id as i64 + 1,
            meter_value: crate::transactions::ocpp_2_1::build_meter_values(Some(sample), now),
        }
    }

    /// Wraps an [`OCPP2_1Client`] with the [`Clock`] that stamps each reading - the same
    /// `with_clock` shape every other timestamped adapter in this crate uses, and reachable
    /// without `std` for the same reason (G3.1).
    pub struct Ocpp2_1MeterValuesNotifier<C> {
        client: OCPP2_1Client,
        clock: C,
    }

    impl<C: Clock> Ocpp2_1MeterValuesNotifier<C> {
        /// Wraps `client`, stamping every reading from `clock`.
        pub fn with_clock(client: OCPP2_1Client, clock: C) -> Self {
            Self { client, clock }
        }
    }

    #[cfg(feature = "std")]
    impl Ocpp2_1MeterValuesNotifier<crate::clock::SystemClock> {
        /// [`Self::with_clock`] with [`crate::clock::SystemClock`].
        pub fn new(client: OCPP2_1Client) -> Self {
            Self::with_clock(client, crate::clock::SystemClock)
        }
    }

    #[async_trait::async_trait]
    impl<C: Clock + Send + Sync> MeterValuesNotifier for Ocpp2_1MeterValuesNotifier<C> {
        type Error = ClientError<OCPP2_1Error>;

        async fn send_meter_values(
            &self,
            evse_id: usize,
            _connector_id: usize,
            sample: MeterSample,
        ) -> Result<(), Self::Error> {
            // 2.x's `MeterValuesRequest` addresses an EVSE, with no connector field at all - a
            // multi-connector EVSE's readings are therefore indistinguishable on this wire, which
            // is the spec's shape rather than a simplification made here.
            self.client
                .send_meter_values(build_meter_values_request(&self.clock, evse_id, sample))
                .await?;
            Ok(())
        }
    }

    /// The `std` convenience: a bare client stamps from [`crate::clock::SystemClock`].
    #[cfg(feature = "std")]
    #[async_trait::async_trait]
    impl MeterValuesNotifier for OCPP2_1Client {
        type Error = ClientError<OCPP2_1Error>;

        async fn send_meter_values(
            &self,
            evse_id: usize,
            _connector_id: usize,
            sample: MeterSample,
        ) -> Result<(), Self::Error> {
            self.send_meter_values(build_meter_values_request(
                &crate::clock::SystemClock,
                evse_id,
                sample,
            ))
            .await?;
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use chrono::{DateTime, Utc};

        struct FixedClock(DateTime<Utc>);

        impl Clock for FixedClock {
            fn now(&self) -> DateTime<Utc> {
                self.0
            }
        }

        fn fixed() -> DateTime<Utc> {
            DateTime::parse_from_rfc3339("2026-03-04T10:15:00Z")
                .unwrap()
                .with_timezone(&Utc)
        }

        #[test]
        fn a_reading_reaches_the_wire_request_with_its_evse_and_timestamp() {
            let request = build_meter_values_request(
                &FixedClock(fixed()),
                0,
                MeterSample {
                    energy_wh: 4_200,
                    power_w: Some(7_400),
                    ..Default::default()
                },
            );

            assert_eq!(request.evse_id, 1, "2.x numbers EVSEs from 1");
            assert_eq!(request.meter_value.len(), 1);
            assert_eq!(request.meter_value[0].timestamp, fixed().to_rfc3339());
            // Energy plus the one optional measurand the sample carried.
            assert_eq!(request.meter_value[0].sampled_value.len(), 2);
        }

        #[test]
        fn an_unsynchronized_clocks_reading_is_still_sent_not_substituted_or_dropped() {
            let unset_rtc = FixedClock(DateTime::<Utc>::from_timestamp(0, 0).unwrap());
            assert!(!is_synchronized(&unset_rtc.now()));

            let request = build_meter_values_request(&unset_rtc, 0, MeterSample::default());

            assert_eq!(request.meter_value[0].timestamp, unset_rtc.0.to_rfc3339());
        }
    }
}

/// OCPP 2.0.1's standalone `MeterValues` - byte-for-byte the same shape as 2.1's here, so this
/// mirrors `super::ocpp_2_1` exactly.
#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1 {
    use super::MeterValuesNotifier;
    use crate::clock::{Clock, is_synchronized};
    use crate::state::MeterSample;
    use alloc::boxed::Box;
    use ocpp_client::ClientError;
    use ocpp_client::ocpp_2_0_1::{OCPP2_0_1Client, OCPP2_0_1Error};
    use ocpp_client::ocpp_types::v201::MeterValuesRequest;

    fn build_meter_values_request<C: Clock>(
        clock: &C,
        evse_id: usize,
        sample: MeterSample,
    ) -> MeterValuesRequest {
        let now = clock.now();
        if !is_synchronized(&now) {
            tracing::warn!(
                timestamp = %now,
                "MeterValues timestamp sourced from an unsynchronized clock"
            );
        }
        MeterValuesRequest {
            custom_data: None,
            evse_id: evse_id as i64 + 1,
            meter_value: crate::transactions::ocpp_2_0_1::build_meter_values(Some(sample), now),
        }
    }

    /// Wraps an [`OCPP2_0_1Client`] with the [`Clock`] that stamps each reading.
    pub struct Ocpp2_0_1MeterValuesNotifier<C> {
        client: OCPP2_0_1Client,
        clock: C,
    }

    impl<C: Clock> Ocpp2_0_1MeterValuesNotifier<C> {
        /// Wraps `client`, stamping every reading from `clock`.
        pub fn with_clock(client: OCPP2_0_1Client, clock: C) -> Self {
            Self { client, clock }
        }
    }

    #[cfg(feature = "std")]
    impl Ocpp2_0_1MeterValuesNotifier<crate::clock::SystemClock> {
        /// [`Self::with_clock`] with [`crate::clock::SystemClock`].
        pub fn new(client: OCPP2_0_1Client) -> Self {
            Self::with_clock(client, crate::clock::SystemClock)
        }
    }

    #[async_trait::async_trait]
    impl<C: Clock + Send + Sync> MeterValuesNotifier for Ocpp2_0_1MeterValuesNotifier<C> {
        type Error = ClientError<OCPP2_0_1Error>;

        async fn send_meter_values(
            &self,
            evse_id: usize,
            _connector_id: usize,
            sample: MeterSample,
        ) -> Result<(), Self::Error> {
            self.client
                .send_meter_values(build_meter_values_request(&self.clock, evse_id, sample))
                .await?;
            Ok(())
        }
    }

    /// The `std` convenience, mirroring 2.1's.
    #[cfg(feature = "std")]
    #[async_trait::async_trait]
    impl MeterValuesNotifier for OCPP2_0_1Client {
        type Error = ClientError<OCPP2_0_1Error>;

        async fn send_meter_values(
            &self,
            evse_id: usize,
            _connector_id: usize,
            sample: MeterSample,
        ) -> Result<(), Self::Error> {
            self.send_meter_values(build_meter_values_request(
                &crate::clock::SystemClock,
                evse_id,
                sample,
            ))
            .await?;
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use chrono::{DateTime, Utc};

        struct FixedClock(DateTime<Utc>);

        impl Clock for FixedClock {
            fn now(&self) -> DateTime<Utc> {
                self.0
            }
        }

        #[test]
        fn a_reading_reaches_the_wire_request_with_its_evse_and_timestamp() {
            let fixed = DateTime::parse_from_rfc3339("2026-03-04T10:15:00Z")
                .unwrap()
                .with_timezone(&Utc);

            let request = build_meter_values_request(
                &FixedClock(fixed),
                2,
                MeterSample {
                    energy_wh: 4_200,
                    ..Default::default()
                },
            );

            assert_eq!(request.evse_id, 3);
            assert_eq!(request.meter_value[0].timestamp, fixed.to_rfc3339());
        }
    }
}

/// OCPP 1.6J's standalone `MeterValues`.
///
/// This is the version that actually needed a standalone sender: 1.6J's `MeterValues` was already
/// wired *inside* the transaction notifier (for readings during a session), leaving readings taken
/// between sessions with nowhere to go. It also addresses a flat `connectorId` rather than an
/// EVSE, so unlike the 2.x adapters this one keeps the connector half of the address instead of
/// discarding it - and needs the charge point's topology to translate.
#[cfg(feature = "ocpp_1_6")]
mod ocpp_1_6 {
    use super::MeterValuesNotifier;
    use crate::clock::{Clock, is_synchronized};
    use crate::state::MeterSample;
    use crate::topology::flatten_ocpp_1_6_connector_id;
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use ocpp_client::ClientError;
    use ocpp_client::ocpp_1_6::{OCPP1_6Client, OCPP1_6Error};
    use ocpp_client::ocpp_types::v16::MeterValuesRequest;

    /// Wraps an [`OCPP1_6Client`] with the connector topology its flat `connectorId` addressing
    /// needs, and the [`Clock`] that stamps each reading - the same pairing
    /// [`crate::transactions::Ocpp1_6TransactionNotifier`] uses.
    #[derive(Clone)]
    pub struct Ocpp1_6MeterValuesNotifier<C> {
        client: OCPP1_6Client,
        connector_counts: Vec<usize>,
        clock: C,
    }

    impl<C: Clock> Ocpp1_6MeterValuesNotifier<C> {
        /// Wraps `client`, resolving connector addresses against `connector_counts` (each EVSE's
        /// connector count, in `evse_id` order) and stamping readings from `clock`.
        pub fn with_clock(
            client: OCPP1_6Client,
            connector_counts: impl IntoIterator<Item = usize>,
            clock: C,
        ) -> Self {
            Self {
                client,
                connector_counts: connector_counts.into_iter().collect(),
                clock,
            }
        }
    }

    #[cfg(feature = "std")]
    impl Ocpp1_6MeterValuesNotifier<crate::clock::SystemClock> {
        /// [`Self::with_clock`] with [`crate::clock::SystemClock`].
        pub fn new(
            client: OCPP1_6Client,
            connector_counts: impl IntoIterator<Item = usize>,
        ) -> Self {
            Self::with_clock(client, connector_counts, crate::clock::SystemClock)
        }
    }

    /// Builds the wire request, or `None` if the address doesn't exist in this topology (which
    /// would otherwise be reported against connector 0 - the charge point itself - and misattribute
    /// the reading).
    fn build_meter_values_request<C: Clock>(
        clock: &C,
        connector_counts: &[usize],
        evse_id: usize,
        connector_id: usize,
        sample: MeterSample,
    ) -> Option<MeterValuesRequest> {
        let connector_id = flatten_ocpp_1_6_connector_id(connector_counts, evse_id, connector_id)?;
        let now = clock.now();
        if !is_synchronized(&now) {
            tracing::warn!(
                timestamp = %now,
                "MeterValues timestamp sourced from an unsynchronized clock"
            );
        }
        Some(MeterValuesRequest {
            connector_id,
            meter_value: crate::transactions::ocpp_1_6::build_meter_values(sample, now),
            // No transaction: that is exactly what makes this reading standalone. 1.6J's
            // `transactionId` is optional for precisely this case.
            transaction_id: None,
        })
    }

    #[async_trait::async_trait]
    impl<C: Clock + Send + Sync> MeterValuesNotifier for Ocpp1_6MeterValuesNotifier<C> {
        type Error = ClientError<OCPP1_6Error>;

        async fn send_meter_values(
            &self,
            evse_id: usize,
            connector_id: usize,
            sample: MeterSample,
        ) -> Result<(), Self::Error> {
            let Some(request) = build_meter_values_request(
                &self.clock,
                &self.connector_counts,
                evse_id,
                connector_id,
                sample,
            ) else {
                tracing::warn!(
                    evse_id,
                    connector_id,
                    "dropping a standalone MeterValues for a connector this topology has no 1.6J \
                     address for"
                );
                return Ok(());
            };
            self.client.send_meter_values(request).await?;
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use chrono::{DateTime, Utc};

        struct FixedClock(DateTime<Utc>);

        impl Clock for FixedClock {
            fn now(&self) -> DateTime<Utc> {
                self.0
            }
        }

        fn fixed() -> DateTime<Utc> {
            DateTime::parse_from_rfc3339("2026-03-04T10:15:00Z")
                .unwrap()
                .with_timezone(&Utc)
        }

        #[test]
        fn a_connector_address_is_flattened_the_way_every_other_1_6j_adapter_flattens_it() {
            // Two EVSEs, two connectors each: (1, 0) is 1.6J connector 3.
            let request = build_meter_values_request(
                &FixedClock(fixed()),
                &[2, 2],
                1,
                0,
                MeterSample::default(),
            )
            .expect("an addressable connector");

            assert_eq!(request.connector_id, 3);
            assert_eq!(request.transaction_id, None);
            assert_eq!(request.meter_value[0].timestamp, fixed().to_rfc3339());
        }

        #[test]
        fn an_unaddressable_connector_produces_no_request_rather_than_connector_zero() {
            // Connector 0 means "the charge point itself" in 1.6J, so falling back to it would
            // silently attribute an EVSE's energy to the station.
            assert!(
                build_meter_values_request(
                    &FixedClock(fixed()),
                    &[2],
                    3,
                    0,
                    MeterSample::default()
                )
                .is_none()
            );
        }
    }
}
