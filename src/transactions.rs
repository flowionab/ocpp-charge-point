//! Transactions functional block: reporting charging-session lifecycle to the CSMS via
//! TransactionEvent. See `docs/ROADMAP.md` §5.

use crate::state::{TransactionEventKind, TransactionEventOccurred};
use crate::sync::BroadcastReceiver;
use alloc::boxed::Box;

#[cfg(feature = "ocpp_1_6")]
pub use self::ocpp_1_6::Ocpp1_6TransactionNotifier;
#[cfg(feature = "ocpp_2_0_1")]
pub use self::ocpp_2_0_1::Ocpp2_0_1TransactionNotifier;
#[cfg(feature = "ocpp_2_1")]
pub use self::ocpp_2_1::Ocpp2_1TransactionNotifier;

/// Reports a transaction lifecycle event to the CSMS via TransactionEvent. Implemented per
/// protocol version (see the `ocpp_2_1` module), mirroring
/// [`crate::availability::StatusNotifier`].
#[async_trait::async_trait]
pub trait TransactionNotifier {
    /// The error type returned if the TransactionEvent request itself fails.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Reports a transaction lifecycle event of `kind` for `transaction`, on the given
    /// connector.
    async fn notify_transaction_event(
        &self,
        evse_id: usize,
        connector_id: usize,
        kind: TransactionEventKind,
        transaction: crate::state::Transaction,
    ) -> Result<(), Self::Error>;
}

/// Forwards to the wrapped notifier - see [`crate::availability::StatusNotifier`]'s `Arc` impl for
/// why this exists.
#[async_trait::async_trait]
impl<N: TransactionNotifier + Send + Sync> TransactionNotifier for alloc::sync::Arc<N> {
    type Error = N::Error;

    async fn notify_transaction_event(
        &self,
        evse_id: usize,
        connector_id: usize,
        kind: crate::state::TransactionEventKind,
        transaction: crate::state::Transaction,
    ) -> Result<(), Self::Error> {
        (**self)
            .notify_transaction_event(evse_id, connector_id, kind, transaction)
            .await
    }
}

/// Forwards every transaction event received on `events` to the CSMS via `notifier`, forever.
/// Errors are logged and do not stop the loop - the actor already applied the event to state;
/// only the CSMS-facing report failed and is not retried. [`setup`](crate::setup) uses
/// [`crate::offline_queue::run_with_offline_queue`] instead, which queues and retries a failed
/// report rather than just logging it - prefer that for `TransactionEvent`, where a report the
/// CSMS never receives can mean lost billing/session history; this simpler fire-and-forget
/// version remains for callers who don't need retry.
pub async fn run_transaction_events<N: TransactionNotifier>(
    mut events: BroadcastReceiver<TransactionEventOccurred>,
    notifier: &N,
) {
    while let Ok(occurred) = events.recv().await {
        if let Err(err) = notifier
            .notify_transaction_event(
                occurred.evse_id,
                occurred.connector_id,
                occurred.kind,
                occurred.transaction,
            )
            .await
        {
            tracing::warn!(error = %err, "transaction event notification failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TransactionNotifier, run_transaction_events};
    use crate::state::{
        Transaction, TransactionChargingState, TransactionEventKind, TransactionEventOccurred,
        TransactionId,
    };
    use crate::sync::broadcast_channel;
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use tokio::sync::watch;

    struct RecordingTransactionNotifier {
        seen: watch::Sender<Vec<(usize, usize, TransactionEventKind, Transaction)>>,
    }

    #[async_trait::async_trait]
    impl TransactionNotifier for RecordingTransactionNotifier {
        type Error = core::convert::Infallible;

        async fn notify_transaction_event(
            &self,
            evse_id: usize,
            connector_id: usize,
            kind: TransactionEventKind,
            transaction: Transaction,
        ) -> Result<(), Self::Error> {
            self.seen
                .send_modify(|seen| seen.push((evse_id, connector_id, kind, transaction)));
            Ok(())
        }
    }

    #[tokio::test]
    async fn forwards_every_transaction_event_to_the_notifier_in_order() {
        let sender = broadcast_channel();
        let receiver = sender.subscribe();
        let (seen_tx, mut seen_rx) = watch::channel(Vec::new());
        let notifier = RecordingTransactionNotifier { seen: seen_tx };

        let forwarder = tokio::spawn(async move {
            run_transaction_events(receiver, &notifier).await;
        });

        let transaction = Transaction {
            id: TransactionId(0),
            id_token: None,
            charging_state: TransactionChargingState::EvConnected,
            stop_reason: None,
            seq_no: 0,
            last_meter_sample: None,
            priority_charging: false,
        };
        sender.send(TransactionEventOccurred {
            evse_id: 0,
            connector_id: 0,
            kind: TransactionEventKind::Started,
            transaction: transaction.clone(),
        });

        seen_rx
            .wait_for(|seen| seen.len() == 1)
            .await
            .expect("notifier task is still running");

        drop(sender);
        forwarder.await.unwrap();

        assert_eq!(
            *seen_rx.borrow(),
            alloc::vec![(0, 0, TransactionEventKind::Started, transaction)]
        );
    }
}

#[cfg(feature = "ocpp_2_1")]
pub(crate) mod ocpp_2_1 {
    pub use with_clock::Ocpp2_1TransactionNotifier;

    use crate::state::{
        MeterSample, StopReason, Transaction, TransactionChargingState, TransactionEventKind,
        TransactionUpdateReason,
    };
    use alloc::vec;
    use alloc::vec::Vec;
    use chrono::{DateTime, Utc};
    use ocpp_client::ocpp_types::v21::common::{
        ChargingStateEnum, MeasurandEnum, MeterValue, ReadingContextEnum, ReasonEnum, SampledValue,
        TransactionEventEnum, TriggerReasonEnum,
    };

    pub(super) fn map_event_type(kind: TransactionEventKind) -> TransactionEventEnum {
        match kind {
            TransactionEventKind::Started => TransactionEventEnum::Started,
            TransactionEventKind::Updated(_) => TransactionEventEnum::Updated,
            TransactionEventKind::Ended => TransactionEventEnum::Ended,
        }
    }

    pub(super) fn map_charging_state(state: TransactionChargingState) -> ChargingStateEnum {
        match state {
            TransactionChargingState::EvConnected => ChargingStateEnum::EVConnected,
            TransactionChargingState::Charging => ChargingStateEnum::Charging,
            TransactionChargingState::SuspendedEV => ChargingStateEnum::SuspendedEV,
            TransactionChargingState::SuspendedEVSE => ChargingStateEnum::SuspendedEVSE,
            TransactionChargingState::Idle => ChargingStateEnum::Idle,
        }
    }

    pub(super) fn map_stop_reason(reason: StopReason) -> ReasonEnum {
        match reason {
            StopReason::Local => ReasonEnum::Local,
            StopReason::Remote => ReasonEnum::Remote,
            StopReason::EVDisconnected => ReasonEnum::EVDisconnected,
            StopReason::EmergencyStop => ReasonEnum::EmergencyStop,
            StopReason::Reset => ReasonEnum::ImmediateReset,
            StopReason::PowerLoss => ReasonEnum::PowerLoss,
        }
    }

    /// The OCPP `triggerReason` a TransactionEvent carries isn't part of our internal
    /// `Transaction`/`TransactionEventKind` model (see CLAUDE.md's version-adapter principle) -
    /// it's derived here from the event kind and, for `Ended`, the transaction's stop reason.
    pub(super) fn trigger_reason_for(
        kind: TransactionEventKind,
        transaction: &Transaction,
    ) -> TriggerReasonEnum {
        match kind {
            TransactionEventKind::Started => TriggerReasonEnum::Authorized,
            TransactionEventKind::Updated(TransactionUpdateReason::ChargingStateChanged) => {
                TriggerReasonEnum::ChargingStateChanged
            }
            TransactionEventKind::Updated(TransactionUpdateReason::MeterValuePeriodic) => {
                TriggerReasonEnum::MeterValuePeriodic
            }
            TransactionEventKind::Ended => match transaction.stop_reason {
                Some(StopReason::EmergencyStop) => TriggerReasonEnum::AbnormalCondition,
                Some(StopReason::Remote) => TriggerReasonEnum::RemoteStop,
                Some(StopReason::EVDisconnected) => TriggerReasonEnum::EVDeparted,
                Some(StopReason::Reset) => TriggerReasonEnum::ResetCommand,
                // No `PowerLoss` trigger reason exists in 2.x - a transaction closed out from
                // its persisted record on the next boot is exactly the "something went wrong
                // outside the normal flow" case `AbnormalCondition` covers. The precise cause
                // still reaches the CSMS in `stoppedReason` (`ReasonEnum::PowerLoss`).
                Some(StopReason::PowerLoss) => TriggerReasonEnum::AbnormalCondition,
                Some(StopReason::Local) | None => TriggerReasonEnum::StopAuthorized,
            },
        }
    }

    /// Builds one `sampledValue` per measurand present in `sample` - always the energy register,
    /// plus power/current/voltage/SoC when the hardware reported them.
    ///
    /// Shared with [`crate::meter_values`]' standalone `MeterValues` adapter through
    /// `build_meter_values`: a reading is a reading, and two mappings of the same measurands into
    /// the same wire type would be two places for a unit bug to hide.
    fn sampled_values(sample: MeterSample) -> Vec<SampledValue> {
        let mut values = vec![sampled_value(
            sample.energy_wh as f64,
            MeasurandEnum::EnergyActiveImportRegister,
        )];
        if let Some(power_w) = sample.power_w {
            values.push(sampled_value(
                power_w as f64,
                MeasurandEnum::PowerActiveImport,
            ));
        }
        if let Some(current_ma) = sample.current_ma {
            values.push(sampled_value(
                current_ma as f64 / 1_000.0,
                MeasurandEnum::CurrentImport,
            ));
        }
        if let Some(voltage_v) = sample.voltage_v {
            values.push(sampled_value(voltage_v as f64, MeasurandEnum::Voltage));
        }
        if let Some(soc_percent) = sample.soc_percent {
            values.push(sampled_value(soc_percent as f64, MeasurandEnum::SoC));
        }
        values
    }

    fn sampled_value(value: f64, measurand: MeasurandEnum) -> SampledValue {
        SampledValue {
            value,
            measurand: Some(measurand),
            context: Some(ReadingContextEnum::SamplePeriodic),
            phase: None,
            location: None,
            signed_meter_value: None,
            unit_of_measure: None,
            custom_data: None,
        }
    }

    /// Builds the TransactionEvent `meterValue` list from a transaction's most recent sample -
    /// empty if it never got one (e.g. `Started`, or a transaction that ended before charging
    /// began). See `docs/ROADMAP.md` §10.
    pub(crate) fn build_meter_values(
        sample: Option<MeterSample>,
        timestamp: DateTime<Utc>,
    ) -> Vec<MeterValue> {
        let Some(sample) = sample else {
            return Vec::new();
        };
        vec![MeterValue {
            timestamp: timestamp.to_rfc3339(),
            sampled_value: sampled_values(sample),
            custom_data: None,
        }]
    }

    // `TransactionEventRequest` needs a timestamp, produced from a caller-supplied `Clock` - see
    // `crate::clock::is_synchronized`'s docs for the policy on an unsynchronized reading (send it
    // as-is, warn, never fabricate or drop it).
    mod with_clock {
        use super::{
            build_meter_values, map_charging_state, map_event_type, map_stop_reason,
            trigger_reason_for,
        };
        use crate::clock::{Clock, is_synchronized};
        use crate::state::{Transaction, TransactionEventKind};
        use crate::transactions::TransactionNotifier;
        use alloc::boxed::Box;
        use ocpp_client::ClientError;
        use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};
        use ocpp_client::ocpp_types::v21::TransactionEventRequest;
        use ocpp_client::ocpp_types::v21::common::{EVSE, Transaction as WireTransaction};

        /// Builds the `TransactionEventRequest` for `kind`/`transaction`, timestamped from
        /// `clock.now()`. Pure (no network I/O), so a fixed [`Clock`] fake can assert the exact
        /// timestamp that reaches the wire request without needing a live `OCPP2_1Client` - see
        /// this module's tests. Warns (see `crate::clock::is_synchronized`'s docs) but still
        /// builds and returns the request when `clock` reads as unsynchronized - never
        /// substitutes, clamps, or omits the reading.
        fn build_transaction_event_request<C: Clock>(
            clock: &C,
            evse_id: usize,
            connector_id: usize,
            kind: TransactionEventKind,
            transaction: Transaction,
        ) -> TransactionEventRequest {
            let now = clock.now();
            if !is_synchronized(&now) {
                tracing::warn!(
                    timestamp = %now,
                    "TransactionEvent timestamp sourced from an unsynchronized clock"
                );
            }
            let meter_value = build_meter_values(transaction.last_meter_sample, now);
            TransactionEventRequest {
                custom_data: None,
                cost_details: None,
                event_type: map_event_type(kind),
                evse_sleep: None,
                meter_value: if meter_value.is_empty() {
                    None
                } else {
                    Some(meter_value)
                },
                timestamp: now.to_rfc3339(),
                trigger_reason: trigger_reason_for(kind, &transaction),
                seq_no: transaction.seq_no as i64,
                preconditioning_status: None,
                transaction_info: WireTransaction {
                    // The transaction id is an internal `u64` formatted as decimal, always
                    // well within the wire field's 36-byte bound.
                    transaction_id: heapless::String::try_from(transaction.id.0)
                        .expect("u64 transaction id always fits in a 36-byte wire field"),
                    charging_state: Some(map_charging_state(transaction.charging_state)),
                    time_spent_charging: None,
                    stopped_reason: transaction.stop_reason.map(map_stop_reason),
                    remote_start_id: None,
                    operation_mode: None,
                    tariff_id: None,
                    transaction_limit: None,
                    custom_data: None,
                },
                offline: None,
                number_of_phases_used: None,
                cable_max_current: None,
                reservation_id: None,
                evse: Some(EVSE {
                    id: evse_id as i64,
                    connector_id: Some(connector_id as i64),
                    custom_data: None,
                }),
                id_token: None,
            }
        }

        async fn send_transaction_event<C: Clock>(
            client: &OCPP2_1Client,
            clock: &C,
            evse_id: usize,
            connector_id: usize,
            kind: TransactionEventKind,
            transaction: Transaction,
        ) -> Result<(), ClientError<OCPP2_1Error>> {
            let request =
                build_transaction_event_request(clock, evse_id, connector_id, kind, transaction);
            client.send_transaction_event(request).await?;
            Ok(())
        }

        /// Wraps an `OCPP2_1Client` with a caller-supplied [`Clock`] for the TransactionEvent
        /// timestamp - the no_std-reachable counterpart to this module's `std`-only `OCPP2_1Client`
        /// impl below, which exists alongside this (not instead of it) so no existing `std` caller
        /// needs a source change.
        pub struct Ocpp2_1TransactionNotifier<C> {
            client: OCPP2_1Client,
            clock: C,
        }

        impl<C: Clock> Ocpp2_1TransactionNotifier<C> {
            /// Wraps `client`, sourcing every TransactionEvent timestamp from `clock`.
            pub fn with_clock(client: OCPP2_1Client, clock: C) -> Self {
                Self { client, clock }
            }
        }

        #[cfg(feature = "std")]
        impl Ocpp2_1TransactionNotifier<crate::clock::SystemClock> {
            /// Wraps `client`, sourcing every TransactionEvent timestamp from
            /// [`crate::clock::SystemClock`].
            pub fn new(client: OCPP2_1Client) -> Self {
                Self::with_clock(client, crate::clock::SystemClock)
            }
        }

        #[async_trait::async_trait]
        impl<C: Clock + Send + Sync> TransactionNotifier for Ocpp2_1TransactionNotifier<C> {
            type Error = ClientError<OCPP2_1Error>;

            async fn notify_transaction_event(
                &self,
                evse_id: usize,
                connector_id: usize,
                kind: TransactionEventKind,
                transaction: Transaction,
            ) -> Result<(), Self::Error> {
                send_transaction_event(
                    &self.client,
                    &self.clock,
                    evse_id,
                    connector_id,
                    kind,
                    transaction,
                )
                .await
            }
        }

        /// The `std` convenience: `OCPP2_1Client` itself implements `TransactionNotifier` directly
        /// (sourcing its timestamp from [`crate::clock::SystemClock`]) so existing callers that
        /// pass a bare client - e.g. [`crate::connect::connect_and_setup`] - need no source change.
        #[cfg(feature = "std")]
        #[async_trait::async_trait]
        impl TransactionNotifier for OCPP2_1Client {
            type Error = ClientError<OCPP2_1Error>;

            async fn notify_transaction_event(
                &self,
                evse_id: usize,
                connector_id: usize,
                kind: TransactionEventKind,
                transaction: Transaction,
            ) -> Result<(), Self::Error> {
                send_transaction_event(
                    self,
                    &crate::clock::SystemClock,
                    evse_id,
                    connector_id,
                    kind,
                    transaction,
                )
                .await
            }
        }

        #[cfg(test)]
        mod tests {
            use super::*;
            use crate::state::{TransactionChargingState, TransactionId};
            use chrono::{DateTime, Utc};

            /// A [`Clock`] that always reads a fixed, caller-chosen instant - mirrors
            /// `crate::persistence::tests::FixedClock`.
            struct FixedClock(DateTime<Utc>);

            impl Clock for FixedClock {
                fn now(&self) -> DateTime<Utc> {
                    self.0
                }
            }

            fn transaction() -> Transaction {
                Transaction {
                    id: TransactionId(7),
                    id_token: None,
                    charging_state: TransactionChargingState::Charging,
                    stop_reason: None,
                    seq_no: 0,
                    last_meter_sample: None,
                    priority_charging: false,
                }
            }

            #[test]
            fn a_fixed_clocks_reading_reaches_the_wire_request_timestamp() {
                let fixed = DateTime::parse_from_rfc3339("2026-03-04T05:06:07Z")
                    .unwrap()
                    .with_timezone(&Utc);
                let clock = FixedClock(fixed);

                let request = build_transaction_event_request(
                    &clock,
                    0,
                    0,
                    TransactionEventKind::Started,
                    transaction(),
                );

                assert_eq!(request.timestamp, fixed.to_rfc3339());
            }

            #[test]
            fn an_unsynchronized_clocks_reading_is_still_sent_not_substituted_or_dropped() {
                let unset_rtc = FixedClock(DateTime::<Utc>::from_timestamp(0, 0).unwrap());
                assert!(!is_synchronized(&unset_rtc.now()));

                // Guards the policy documented on `crate::clock::is_synchronized`: an
                // unsynchronized reading must reach the built request unchanged, never
                // substituted, clamped, or omitted.
                let request = build_transaction_event_request(
                    &unset_rtc,
                    0,
                    0,
                    TransactionEventKind::Started,
                    transaction(),
                );

                assert_eq!(request.timestamp, unset_rtc.0.to_rfc3339());
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::state::MeterSample;

        #[test]
        fn a_sample_with_only_energy_reports_a_single_sampled_value() {
            let sample = MeterSample {
                energy_wh: 1_500,
                ..Default::default()
            };
            let timestamp = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc);

            let values = build_meter_values(Some(sample), timestamp);

            assert_eq!(values.len(), 1);
            assert_eq!(values[0].sampled_value.len(), 1);
            assert_eq!(values[0].sampled_value[0].value, 1_500.0);
            assert_eq!(
                values[0].sampled_value[0].measurand,
                Some(MeasurandEnum::EnergyActiveImportRegister)
            );
        }

        #[test]
        fn a_sample_with_every_measurand_reports_one_sampled_value_per_measurand() {
            let sample = MeterSample {
                energy_wh: 1_500,
                power_w: Some(7_400),
                current_ma: Some(32_000),
                voltage_v: Some(230),
                soc_percent: Some(42),
            };
            let timestamp = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc);

            let values = build_meter_values(Some(sample), timestamp);

            assert_eq!(values.len(), 1);
            let sampled = &values[0].sampled_value;
            assert_eq!(sampled.len(), 5);
            assert!(sampled.iter().any(|value| value.measurand
                == Some(MeasurandEnum::EnergyActiveImportRegister)
                && value.value == 1_500.0));
            assert!(sampled.iter().any(|value| value.measurand
                == Some(MeasurandEnum::PowerActiveImport)
                && value.value == 7_400.0));
            assert!(sampled.iter().any(|value| value.measurand
                == Some(MeasurandEnum::CurrentImport)
                && value.value == 32.0));
            assert!(sampled.iter().any(
                |value| value.measurand == Some(MeasurandEnum::Voltage) && value.value == 230.0
            ));
            assert!(sampled
                .iter()
                .any(|value| value.measurand == Some(MeasurandEnum::SoC) && value.value == 42.0));
        }

        #[test]
        fn no_sample_reports_no_meter_values() {
            let timestamp = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc);

            assert_eq!(build_meter_values(None, timestamp), Vec::new());
        }

        #[test]
        fn every_kind_maps_to_the_matching_wire_event_type() {
            assert_eq!(
                map_event_type(TransactionEventKind::Started),
                TransactionEventEnum::Started
            );
            assert_eq!(
                map_event_type(TransactionEventKind::Updated(
                    TransactionUpdateReason::ChargingStateChanged
                )),
                TransactionEventEnum::Updated
            );
            assert_eq!(
                map_event_type(TransactionEventKind::Ended),
                TransactionEventEnum::Ended
            );
        }

        #[test]
        fn every_charging_state_maps_to_the_matching_wire_state() {
            assert_eq!(
                map_charging_state(TransactionChargingState::EvConnected),
                ChargingStateEnum::EVConnected
            );
            assert_eq!(
                map_charging_state(TransactionChargingState::Charging),
                ChargingStateEnum::Charging
            );
            assert_eq!(
                map_charging_state(TransactionChargingState::SuspendedEV),
                ChargingStateEnum::SuspendedEV
            );
            assert_eq!(
                map_charging_state(TransactionChargingState::SuspendedEVSE),
                ChargingStateEnum::SuspendedEVSE
            );
            assert_eq!(
                map_charging_state(TransactionChargingState::Idle),
                ChargingStateEnum::Idle
            );
        }

        #[test]
        fn started_and_updated_use_fixed_trigger_reasons() {
            let transaction = Transaction {
                id: crate::state::TransactionId(0),
                id_token: None,
                charging_state: TransactionChargingState::Charging,
                stop_reason: None,
                seq_no: 0,
                last_meter_sample: None,
                priority_charging: false,
            };

            assert_eq!(
                trigger_reason_for(TransactionEventKind::Started, &transaction),
                TriggerReasonEnum::Authorized
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Updated(TransactionUpdateReason::ChargingStateChanged),
                    &transaction
                ),
                TriggerReasonEnum::ChargingStateChanged
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Updated(TransactionUpdateReason::MeterValuePeriodic),
                    &transaction
                ),
                TriggerReasonEnum::MeterValuePeriodic
            );
        }

        #[test]
        fn ended_derives_its_trigger_reason_from_the_stop_reason() {
            let base = Transaction {
                id: crate::state::TransactionId(0),
                id_token: None,
                charging_state: TransactionChargingState::EvConnected,
                stop_reason: None,
                seq_no: 2,
                last_meter_sample: None,
                priority_charging: false,
            };

            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Ended,
                    &Transaction {
                        stop_reason: Some(StopReason::EmergencyStop),
                        ..base.clone()
                    }
                ),
                TriggerReasonEnum::AbnormalCondition
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Ended,
                    &Transaction {
                        stop_reason: Some(StopReason::Remote),
                        ..base.clone()
                    }
                ),
                TriggerReasonEnum::RemoteStop
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Ended,
                    &Transaction {
                        stop_reason: Some(StopReason::EVDisconnected),
                        ..base.clone()
                    }
                ),
                TriggerReasonEnum::EVDeparted
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Ended,
                    &Transaction {
                        stop_reason: Some(StopReason::Local),
                        ..base.clone()
                    }
                ),
                TriggerReasonEnum::StopAuthorized
            );
            assert_eq!(
                trigger_reason_for(TransactionEventKind::Ended, &base),
                TriggerReasonEnum::StopAuthorized
            );
        }
    }
}

/// The OCPP 2.0.1 projection - a strict subset of 2.1's `TransactionEventRequest`/`Transaction`
/// wire shapes (2.1 added `costDetails`/`evseSleep`/`preconditioningStatus` and a richer
/// `Transaction` with `operationMode`/`tariffId`/`transactionLimit` - this crate always sends
/// `None` for all of them, so nothing is actually lost), and every wire enum value this module
/// actually uses (`TransactionEventEnum`, `ChargingStateEnum`, the four `ReasonEnum`/seven
/// `TriggerReasonEnum` values referenced below, `MeasurandEnum`, `ReadingContextEnum::
/// SamplePeriodic`) exists identically in both versions. Close to a copy of the 2.1 module, just
/// targeting `OCPP2_0_1Client`.
#[cfg(feature = "ocpp_2_0_1")]
pub(crate) mod ocpp_2_0_1 {
    pub use with_clock::Ocpp2_0_1TransactionNotifier;

    use crate::state::{
        MeterSample, StopReason, Transaction, TransactionChargingState, TransactionEventKind,
        TransactionUpdateReason,
    };
    use alloc::vec;
    use alloc::vec::Vec;
    use chrono::{DateTime, Utc};
    use ocpp_client::ocpp_types::v201::common::{
        ChargingStateEnum, MeasurandEnum, MeterValue, ReadingContextEnum, ReasonEnum, SampledValue,
        TransactionEventEnum, TriggerReasonEnum,
    };

    pub(super) fn map_event_type(kind: TransactionEventKind) -> TransactionEventEnum {
        match kind {
            TransactionEventKind::Started => TransactionEventEnum::Started,
            TransactionEventKind::Updated(_) => TransactionEventEnum::Updated,
            TransactionEventKind::Ended => TransactionEventEnum::Ended,
        }
    }

    pub(super) fn map_charging_state(state: TransactionChargingState) -> ChargingStateEnum {
        match state {
            TransactionChargingState::EvConnected => ChargingStateEnum::EVConnected,
            TransactionChargingState::Charging => ChargingStateEnum::Charging,
            TransactionChargingState::SuspendedEV => ChargingStateEnum::SuspendedEV,
            TransactionChargingState::SuspendedEVSE => ChargingStateEnum::SuspendedEVSE,
            TransactionChargingState::Idle => ChargingStateEnum::Idle,
        }
    }

    pub(super) fn map_stop_reason(reason: StopReason) -> ReasonEnum {
        match reason {
            StopReason::Local => ReasonEnum::Local,
            StopReason::Remote => ReasonEnum::Remote,
            StopReason::EVDisconnected => ReasonEnum::EVDisconnected,
            StopReason::EmergencyStop => ReasonEnum::EmergencyStop,
            StopReason::Reset => ReasonEnum::ImmediateReset,
            StopReason::PowerLoss => ReasonEnum::PowerLoss,
        }
    }

    /// Mirrors [`super::ocpp_2_1::trigger_reason_for`].
    pub(super) fn trigger_reason_for(
        kind: TransactionEventKind,
        transaction: &Transaction,
    ) -> TriggerReasonEnum {
        match kind {
            TransactionEventKind::Started => TriggerReasonEnum::Authorized,
            TransactionEventKind::Updated(TransactionUpdateReason::ChargingStateChanged) => {
                TriggerReasonEnum::ChargingStateChanged
            }
            TransactionEventKind::Updated(TransactionUpdateReason::MeterValuePeriodic) => {
                TriggerReasonEnum::MeterValuePeriodic
            }
            TransactionEventKind::Ended => match transaction.stop_reason {
                Some(StopReason::EmergencyStop) => TriggerReasonEnum::AbnormalCondition,
                Some(StopReason::Remote) => TriggerReasonEnum::RemoteStop,
                Some(StopReason::EVDisconnected) => TriggerReasonEnum::EVDeparted,
                Some(StopReason::Reset) => TriggerReasonEnum::ResetCommand,
                // No `PowerLoss` trigger reason exists in 2.x - a transaction closed out from
                // its persisted record on the next boot is exactly the "something went wrong
                // outside the normal flow" case `AbnormalCondition` covers. The precise cause
                // still reaches the CSMS in `stoppedReason` (`ReasonEnum::PowerLoss`).
                Some(StopReason::PowerLoss) => TriggerReasonEnum::AbnormalCondition,
                Some(StopReason::Local) | None => TriggerReasonEnum::StopAuthorized,
            },
        }
    }

    fn sampled_values(sample: MeterSample) -> Vec<SampledValue> {
        let mut values = vec![sampled_value(
            sample.energy_wh as f64,
            MeasurandEnum::EnergyActiveImportRegister,
        )];
        if let Some(power_w) = sample.power_w {
            values.push(sampled_value(
                power_w as f64,
                MeasurandEnum::PowerActiveImport,
            ));
        }
        if let Some(current_ma) = sample.current_ma {
            values.push(sampled_value(
                current_ma as f64 / 1_000.0,
                MeasurandEnum::CurrentImport,
            ));
        }
        if let Some(voltage_v) = sample.voltage_v {
            values.push(sampled_value(voltage_v as f64, MeasurandEnum::Voltage));
        }
        if let Some(soc_percent) = sample.soc_percent {
            values.push(sampled_value(soc_percent as f64, MeasurandEnum::SoC));
        }
        values
    }

    fn sampled_value(value: f64, measurand: MeasurandEnum) -> SampledValue {
        SampledValue {
            value,
            measurand: Some(measurand),
            context: Some(ReadingContextEnum::SamplePeriodic),
            phase: None,
            location: None,
            signed_meter_value: None,
            unit_of_measure: None,
            custom_data: None,
        }
    }

    pub(crate) fn build_meter_values(
        sample: Option<MeterSample>,
        timestamp: DateTime<Utc>,
    ) -> Vec<MeterValue> {
        let Some(sample) = sample else {
            return Vec::new();
        };
        vec![MeterValue {
            timestamp: timestamp.to_rfc3339(),
            sampled_value: sampled_values(sample),
            custom_data: None,
        }]
    }

    // `TransactionEventRequest` needs a timestamp, produced from a caller-supplied `Clock` - see
    // `crate::clock::is_synchronized`'s docs for the policy on an unsynchronized reading (send it
    // as-is, warn, never fabricate or drop it). Mirrors `super::ocpp_2_1::with_clock`.
    mod with_clock {
        use super::{
            build_meter_values, map_charging_state, map_event_type, map_stop_reason,
            trigger_reason_for,
        };
        use crate::clock::{Clock, is_synchronized};
        use crate::state::{Transaction, TransactionEventKind};
        use crate::transactions::TransactionNotifier;
        use alloc::boxed::Box;
        use ocpp_client::ClientError;
        use ocpp_client::ocpp_2_0_1::{OCPP2_0_1Client, OCPP2_0_1Error};
        use ocpp_client::ocpp_types::v201::TransactionEventRequest;
        use ocpp_client::ocpp_types::v201::common::{EVSE, Transaction as WireTransaction};

        /// Mirrors `super::ocpp_2_1::with_clock::build_transaction_event_request` - pure, so a
        /// fixed [`Clock`] fake can assert the exact timestamp reaching the wire request.
        fn build_transaction_event_request<C: Clock>(
            clock: &C,
            evse_id: usize,
            connector_id: usize,
            kind: TransactionEventKind,
            transaction: Transaction,
        ) -> TransactionEventRequest {
            let now = clock.now();
            if !is_synchronized(&now) {
                tracing::warn!(
                    timestamp = %now,
                    "TransactionEvent timestamp sourced from an unsynchronized clock"
                );
            }
            let meter_value = build_meter_values(transaction.last_meter_sample, now);
            TransactionEventRequest {
                custom_data: None,
                event_type: map_event_type(kind),
                meter_value: if meter_value.is_empty() {
                    None
                } else {
                    Some(meter_value)
                },
                timestamp: now.to_rfc3339(),
                trigger_reason: trigger_reason_for(kind, &transaction),
                seq_no: transaction.seq_no as i64,
                transaction_info: WireTransaction {
                    // The transaction id is an internal `u64` formatted as decimal, always
                    // well within the wire field's 36-byte bound.
                    transaction_id: heapless::String::try_from(transaction.id.0)
                        .expect("u64 transaction id always fits in a 36-byte wire field"),
                    charging_state: Some(map_charging_state(transaction.charging_state)),
                    time_spent_charging: None,
                    stopped_reason: transaction.stop_reason.map(map_stop_reason),
                    remote_start_id: None,
                    custom_data: None,
                },
                offline: None,
                number_of_phases_used: None,
                cable_max_current: None,
                reservation_id: None,
                evse: Some(EVSE {
                    id: evse_id as i64,
                    connector_id: Some(connector_id as i64),
                    custom_data: None,
                }),
                id_token: None,
            }
        }

        async fn send_transaction_event<C: Clock>(
            client: &OCPP2_0_1Client,
            clock: &C,
            evse_id: usize,
            connector_id: usize,
            kind: TransactionEventKind,
            transaction: Transaction,
        ) -> Result<(), ClientError<OCPP2_0_1Error>> {
            let request =
                build_transaction_event_request(clock, evse_id, connector_id, kind, transaction);
            client.send_transaction_event(request).await?;
            Ok(())
        }

        /// Wraps an `OCPP2_0_1Client` with a caller-supplied [`Clock`] for the TransactionEvent
        /// timestamp - mirrors `super::ocpp_2_1::with_clock::Ocpp2_1TransactionNotifier`.
        pub struct Ocpp2_0_1TransactionNotifier<C> {
            client: OCPP2_0_1Client,
            clock: C,
        }

        impl<C: Clock> Ocpp2_0_1TransactionNotifier<C> {
            /// Wraps `client`, sourcing every TransactionEvent timestamp from `clock`.
            pub fn with_clock(client: OCPP2_0_1Client, clock: C) -> Self {
                Self { client, clock }
            }
        }

        #[cfg(feature = "std")]
        impl Ocpp2_0_1TransactionNotifier<crate::clock::SystemClock> {
            /// Wraps `client`, sourcing every TransactionEvent timestamp from
            /// [`crate::clock::SystemClock`].
            pub fn new(client: OCPP2_0_1Client) -> Self {
                Self::with_clock(client, crate::clock::SystemClock)
            }
        }

        #[async_trait::async_trait]
        impl<C: Clock + Send + Sync> TransactionNotifier for Ocpp2_0_1TransactionNotifier<C> {
            type Error = ClientError<OCPP2_0_1Error>;

            async fn notify_transaction_event(
                &self,
                evse_id: usize,
                connector_id: usize,
                kind: TransactionEventKind,
                transaction: Transaction,
            ) -> Result<(), Self::Error> {
                send_transaction_event(
                    &self.client,
                    &self.clock,
                    evse_id,
                    connector_id,
                    kind,
                    transaction,
                )
                .await
            }
        }

        /// The `std` convenience: `OCPP2_0_1Client` itself implements `TransactionNotifier`
        /// directly (sourcing its timestamp from [`crate::clock::SystemClock`]) so existing
        /// callers that pass a bare client need no source change.
        #[cfg(feature = "std")]
        #[async_trait::async_trait]
        impl TransactionNotifier for OCPP2_0_1Client {
            type Error = ClientError<OCPP2_0_1Error>;

            async fn notify_transaction_event(
                &self,
                evse_id: usize,
                connector_id: usize,
                kind: TransactionEventKind,
                transaction: Transaction,
            ) -> Result<(), Self::Error> {
                send_transaction_event(
                    self,
                    &crate::clock::SystemClock,
                    evse_id,
                    connector_id,
                    kind,
                    transaction,
                )
                .await
            }
        }

        #[cfg(test)]
        mod tests {
            use super::*;
            use crate::state::{TransactionChargingState, TransactionId};
            use chrono::{DateTime, Utc};

            /// A [`Clock`] that always reads a fixed, caller-chosen instant.
            struct FixedClock(DateTime<Utc>);

            impl Clock for FixedClock {
                fn now(&self) -> DateTime<Utc> {
                    self.0
                }
            }

            fn transaction() -> Transaction {
                Transaction {
                    id: TransactionId(7),
                    id_token: None,
                    charging_state: TransactionChargingState::Charging,
                    stop_reason: None,
                    seq_no: 0,
                    last_meter_sample: None,
                    priority_charging: false,
                }
            }

            #[test]
            fn a_fixed_clocks_reading_reaches_the_wire_request_timestamp() {
                let fixed = DateTime::parse_from_rfc3339("2026-03-04T05:06:07Z")
                    .unwrap()
                    .with_timezone(&Utc);
                let clock = FixedClock(fixed);

                let request = build_transaction_event_request(
                    &clock,
                    0,
                    0,
                    TransactionEventKind::Started,
                    transaction(),
                );

                assert_eq!(request.timestamp, fixed.to_rfc3339());
            }

            #[test]
            fn an_unsynchronized_clocks_reading_is_still_sent_not_substituted_or_dropped() {
                let unset_rtc = FixedClock(DateTime::<Utc>::from_timestamp(0, 0).unwrap());
                assert!(!is_synchronized(&unset_rtc.now()));

                let request = build_transaction_event_request(
                    &unset_rtc,
                    0,
                    0,
                    TransactionEventKind::Started,
                    transaction(),
                );

                assert_eq!(request.timestamp, unset_rtc.0.to_rfc3339());
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::state::MeterSample;

        #[test]
        fn a_sample_with_only_energy_reports_a_single_sampled_value() {
            let sample = MeterSample {
                energy_wh: 1_500,
                ..Default::default()
            };
            let timestamp = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc);

            let values = build_meter_values(Some(sample), timestamp);

            assert_eq!(values.len(), 1);
            assert_eq!(values[0].sampled_value.len(), 1);
            assert_eq!(values[0].sampled_value[0].value, 1_500.0);
            assert_eq!(
                values[0].sampled_value[0].measurand,
                Some(MeasurandEnum::EnergyActiveImportRegister)
            );
        }

        #[test]
        fn no_sample_reports_no_meter_values() {
            let timestamp = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc);

            assert_eq!(build_meter_values(None, timestamp), Vec::new());
        }

        #[test]
        fn every_kind_maps_to_the_matching_wire_event_type() {
            assert_eq!(
                map_event_type(TransactionEventKind::Started),
                TransactionEventEnum::Started
            );
            assert_eq!(
                map_event_type(TransactionEventKind::Updated(
                    TransactionUpdateReason::ChargingStateChanged
                )),
                TransactionEventEnum::Updated
            );
            assert_eq!(
                map_event_type(TransactionEventKind::Ended),
                TransactionEventEnum::Ended
            );
        }

        #[test]
        fn every_charging_state_maps_to_the_matching_wire_state() {
            assert_eq!(
                map_charging_state(TransactionChargingState::EvConnected),
                ChargingStateEnum::EVConnected
            );
            assert_eq!(
                map_charging_state(TransactionChargingState::Charging),
                ChargingStateEnum::Charging
            );
            assert_eq!(
                map_charging_state(TransactionChargingState::SuspendedEV),
                ChargingStateEnum::SuspendedEV
            );
            assert_eq!(
                map_charging_state(TransactionChargingState::SuspendedEVSE),
                ChargingStateEnum::SuspendedEVSE
            );
            assert_eq!(
                map_charging_state(TransactionChargingState::Idle),
                ChargingStateEnum::Idle
            );
        }

        #[test]
        fn started_and_updated_use_fixed_trigger_reasons() {
            let transaction = Transaction {
                id: crate::state::TransactionId(0),
                id_token: None,
                charging_state: TransactionChargingState::Charging,
                stop_reason: None,
                seq_no: 0,
                last_meter_sample: None,
                priority_charging: false,
            };

            assert_eq!(
                trigger_reason_for(TransactionEventKind::Started, &transaction),
                TriggerReasonEnum::Authorized
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Updated(TransactionUpdateReason::ChargingStateChanged),
                    &transaction
                ),
                TriggerReasonEnum::ChargingStateChanged
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Updated(TransactionUpdateReason::MeterValuePeriodic),
                    &transaction
                ),
                TriggerReasonEnum::MeterValuePeriodic
            );
        }

        #[test]
        fn ended_derives_its_trigger_reason_from_the_stop_reason() {
            let base = Transaction {
                id: crate::state::TransactionId(0),
                id_token: None,
                charging_state: TransactionChargingState::EvConnected,
                stop_reason: None,
                seq_no: 2,
                last_meter_sample: None,
                priority_charging: false,
            };

            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Ended,
                    &Transaction {
                        stop_reason: Some(StopReason::EmergencyStop),
                        ..base.clone()
                    }
                ),
                TriggerReasonEnum::AbnormalCondition
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Ended,
                    &Transaction {
                        stop_reason: Some(StopReason::Remote),
                        ..base.clone()
                    }
                ),
                TriggerReasonEnum::RemoteStop
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Ended,
                    &Transaction {
                        stop_reason: Some(StopReason::EVDisconnected),
                        ..base.clone()
                    }
                ),
                TriggerReasonEnum::EVDeparted
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Ended,
                    &Transaction {
                        stop_reason: Some(StopReason::Local),
                        ..base.clone()
                    }
                ),
                TriggerReasonEnum::StopAuthorized
            );
            assert_eq!(
                trigger_reason_for(TransactionEventKind::Ended, &base),
                TriggerReasonEnum::StopAuthorized
            );
        }
    }
}

/// The OCPP 1.6J projection of `TransactionNotifier` - a harder case than any adapter so far
/// (see `docs/ROADMAP.md` §0): 1.6J splits the unified `TransactionEvent` stream into three
/// separate calls (`StartTransaction`/`MeterValues`/`StopTransaction`), and - unlike every other
/// identifier in this crate - **the CSMS assigns the transaction id**, not the charge point:
/// `StartTransaction.conf` returns a `transactionId` that every later `MeterValues`/
/// `StopTransaction` for that session must use instead of this crate's own [`TransactionId`].
/// [`Ocpp1_6TransactionNotifier`] wraps `OCPP1_6Client` with the connector topology needed for
/// `StartTransaction`/`MeterValues`'s flat `connectorId` (see [`crate::topology`], shared with
/// [`crate::availability::Ocpp1_6StatusNotifier`]) and a small cache mapping this crate's
/// `TransactionId` to the CSMS-assigned one, populated when `StartTransaction.conf` returns and
/// consulted (then, on `Ended`, removed) for every later call on that transaction.
///
/// Two real gaps, both inherited from the internal model rather than introduced here: 1.6J's
/// `StartTransaction.req` needs `idTag` and `meterStart` - `idTag` now has a real source
/// ([`crate::state::Transaction::id_token`]), but `meterStart` doesn't; this crate's
/// [`crate::state::MeterSample`] is only ever recorded once charging is already under way (see
/// `docs/ROADMAP.md` §10), so there's no reading captured *at* `Started` to report - this falls
/// back to `0` (documented, not silently wrong). `idTag` mapping itself (including truncation
/// for identifiers exceeding 1.6J's 20-byte bound) lives in [`crate::id_tag`], shared with every
/// other 1.6J adapter that sends an identifier on the wire.
#[cfg(feature = "ocpp_1_6")]
pub(crate) mod ocpp_1_6 {
    use crate::state::{MeterSample, StopReason, TransactionId};
    use alloc::collections::BTreeMap;
    use alloc::string::ToString;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
    use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
    use ocpp_client::ocpp_1_6::OCPP1_6Client;
    use ocpp_client::ocpp_types::v16::common::{
        Measurand, MeterValueItem, Reason, SampledValueItem,
    };

    pub(super) fn map_stop_reason(reason: StopReason) -> Reason {
        match reason {
            StopReason::Local => Reason::Local,
            StopReason::Remote => Reason::Remote,
            StopReason::EVDisconnected => Reason::EVDisconnected,
            StopReason::EmergencyStop => Reason::EmergencyStop,
            // A CSMS-initiated `Reset` (`ResetKind::Immediate`) that interrupted this
            // transaction is, on the wire, indistinguishable from any other "the whole charge
            // point restarted" stop - 1.6J's `Reason` enum's closest match is `HardReset`
            // (mirroring the `Hard`/`Immediate` pairing `crate::reset::ocpp_1_6` uses).
            StopReason::Reset => Reason::HardReset,
            StopReason::PowerLoss => Reason::PowerLoss,
        }
    }

    /// Builds one `sampledValue` per measurand present in `sample`, mirroring
    /// [`super::ocpp_2_1::sampled_values`] but for 1.6J's string-valued `SampledValueItem`.
    /// Builds the 1.6J `meterValue` body for `sample`, shared with [`crate::meter_values`]'
    /// standalone `MeterValues` adapter for the same reason the 2.x builders are.
    pub(crate) fn build_meter_values(
        sample: MeterSample,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) -> Vec<MeterValueItem> {
        vec![MeterValueItem {
            timestamp: timestamp.to_rfc3339(),
            sampled_value: sampled_values(sample),
        }]
    }

    fn sampled_values(sample: MeterSample) -> Vec<SampledValueItem> {
        let mut values = vec![sampled_value(
            sample.energy_wh.to_string(),
            Measurand::EnergyActiveImportRegister,
        )];
        if let Some(power_w) = sample.power_w {
            values.push(sampled_value(
                power_w.to_string(),
                Measurand::PowerActiveImport,
            ));
        }
        if let Some(current_ma) = sample.current_ma {
            values.push(sampled_value(
                (current_ma as f64 / 1_000.0).to_string(),
                Measurand::CurrentImport,
            ));
        }
        if let Some(voltage_v) = sample.voltage_v {
            values.push(sampled_value(voltage_v.to_string(), Measurand::Voltage));
        }
        if let Some(soc_percent) = sample.soc_percent {
            values.push(sampled_value(soc_percent.to_string(), Measurand::SoC));
        }
        values
    }

    fn sampled_value(value: alloc::string::String, measurand: Measurand) -> SampledValueItem {
        SampledValueItem {
            context: None,
            format: None,
            location: None,
            measurand: Some(measurand),
            phase: None,
            unit: None,
            value,
        }
    }

    /// Wraps an `OCPP1_6Client` with the charge point's connector topology, a cache of
    /// CSMS-assigned transaction ids, and a caller-supplied [`crate::clock::Clock`] for the
    /// `StartTransaction`/`MeterValues`/`StopTransaction` timestamp - see this module's top-level
    /// docs. `C` defaults to nothing in particular; embedded/no_std callers construct with
    /// [`Self::with_clock`] and an RTC-backed `Clock`, while `std` callers keep using
    /// [`Self::new`] (sourcing the timestamp from [`crate::clock::SystemClock`]) unchanged.
    pub struct Ocpp1_6TransactionNotifier<C> {
        client: OCPP1_6Client,
        connector_counts: Vec<usize>,
        csms_transaction_ids:
            BlockingMutex<CriticalSectionRawMutex, RefCell<BTreeMap<TransactionId, i64>>>,
        clock: C,
    }

    impl<C: crate::clock::Clock> Ocpp1_6TransactionNotifier<C> {
        /// Wraps `client`, capturing `connector_counts` (each EVSE's connector count, in
        /// `evse_id` order) for translating connector addresses, starting with no cached
        /// CSMS-assigned transaction ids, and sourcing every wire timestamp from `clock`.
        pub fn with_clock(
            client: OCPP1_6Client,
            connector_counts: impl IntoIterator<Item = usize>,
            clock: C,
        ) -> Self {
            Self {
                client,
                connector_counts: connector_counts.into_iter().collect(),
                csms_transaction_ids: BlockingMutex::new(RefCell::new(BTreeMap::new())),
                clock,
            }
        }
    }

    #[cfg(feature = "std")]
    impl Ocpp1_6TransactionNotifier<crate::clock::SystemClock> {
        /// Wraps `client`, capturing `connector_counts` (each EVSE's connector count, in
        /// `evse_id` order) for translating connector addresses, starting with no cached
        /// CSMS-assigned transaction ids, and sourcing every wire timestamp from
        /// [`crate::clock::SystemClock`].
        pub fn new(
            client: OCPP1_6Client,
            connector_counts: impl IntoIterator<Item = usize>,
        ) -> Self {
            Self::with_clock(client, connector_counts, crate::clock::SystemClock)
        }
    }

    // `StartTransactionRequest`/`StopTransactionRequest`/`MeterValuesRequest` need a timestamp,
    // produced from `self.clock` - see `crate::clock::is_synchronized`'s docs for the policy on
    // an unsynchronized reading (send it as-is, warn, never fabricate or drop it).
    mod with_clock {
        use super::{Ocpp1_6TransactionNotifier, map_stop_reason, sampled_values};
        use crate::clock::{Clock, is_synchronized};
        use crate::id_tag::map_id_tag;
        use crate::state::{Transaction, TransactionEventKind};
        use crate::topology::flatten_ocpp_1_6_connector_id;
        use crate::transactions::TransactionNotifier;
        use alloc::boxed::Box;
        use alloc::vec::Vec;
        use ocpp_client::ClientError;
        use ocpp_client::ocpp_1_6::OCPP1_6Error;
        use ocpp_client::ocpp_types::v16::common::MeterValueItem;
        use ocpp_client::ocpp_types::v16::{
            MeterValuesRequest, StartTransactionRequest, StopTransactionRequest,
        };

        /// Delegates to the wrapped client: a reconnect is a property of the *connection*, and
        /// this wrapper adds topology and an id cache on top of one rather than owning a
        /// different one. Without this a 1.6J charge point could not use the offline queues at
        /// all, since those flush on reconnect.
        #[async_trait::async_trait]
        impl<C: Clock + Send + Sync> crate::connection::ReconnectHandler for Ocpp1_6TransactionNotifier<C> {
            async fn register_reconnect_handler<F, FF>(&self, callback: F)
            where
                F: FnMut() -> FF + Send + Sync + 'static,
                FF: core::future::Future<Output = ()> + Send + 'static,
            {
                crate::connection::ReconnectHandler::register_reconnect_handler(
                    &self.client,
                    callback,
                )
                .await;
            }
        }

        #[async_trait::async_trait]
        impl<C: Clock + Send + Sync> TransactionNotifier for Ocpp1_6TransactionNotifier<C> {
            type Error = ClientError<OCPP1_6Error>;

            async fn notify_transaction_event(
                &self,
                evse_id: usize,
                connector_id: usize,
                kind: TransactionEventKind,
                transaction: Transaction,
            ) -> Result<(), Self::Error> {
                let Some(connector_id) =
                    flatten_ocpp_1_6_connector_id(&self.connector_counts, evse_id, connector_id)
                else {
                    // Not addressable under 1.6J's flat numbering - see
                    // `Ocpp1_6StatusNotifier::notify_status`'s identical handling.
                    return Ok(());
                };
                let now = self.clock.now();
                if !is_synchronized(&now) {
                    tracing::warn!(
                        timestamp = %now,
                        ?kind,
                        "1.6J transaction timestamp sourced from an unsynchronized clock"
                    );
                }
                let meter_reading = transaction
                    .last_meter_sample
                    .map(|s| s.energy_wh)
                    .unwrap_or(0);

                match kind {
                    TransactionEventKind::Started => {
                        let response = self
                            .client
                            .send_start_transaction(StartTransactionRequest {
                                connector_id,
                                id_tag: map_id_tag(transaction.id_token.as_ref()),
                                meter_start: meter_reading,
                                reservation_id: None,
                                timestamp: now.to_rfc3339(),
                            })
                            .await?;
                        self.csms_transaction_ids.lock(|cache| {
                            cache
                                .borrow_mut()
                                .insert(transaction.id, response.transaction_id);
                        });
                    }
                    TransactionEventKind::Updated(_) => {
                        let Some(sample) = transaction.last_meter_sample else {
                            // Nothing sampled yet - a `MeterValues` with no `sampledValue` at all
                            // isn't meaningful, unlike 2.1's `TransactionEvent`, which reports
                            // regardless (see `super::ocpp_2_1`'s `Updated` handling).
                            return Ok(());
                        };
                        let csms_transaction_id = self
                            .csms_transaction_ids
                            .lock(|cache| cache.borrow().get(&transaction.id).copied());
                        self.client
                            .send_meter_values(MeterValuesRequest {
                                connector_id,
                                meter_value: Vec::from([MeterValueItem {
                                    sampled_value: sampled_values(sample),
                                    timestamp: now.to_rfc3339(),
                                }]),
                                transaction_id: csms_transaction_id,
                            })
                            .await?;
                    }
                    TransactionEventKind::Ended => {
                        let csms_transaction_id = self
                            .csms_transaction_ids
                            .lock(|cache| cache.borrow_mut().remove(&transaction.id));
                        let Some(csms_transaction_id) = csms_transaction_id else {
                            // No `StartTransaction` on record for this transaction - shouldn't
                            // happen (`Started` always precedes `Ended` for the same
                            // transaction), but there's nothing to stop without the CSMS's id.
                            return Ok(());
                        };
                        self.client
                            .send_stop_transaction(StopTransactionRequest {
                                id_tag: Some(map_id_tag(transaction.id_token.as_ref())),
                                meter_stop: meter_reading,
                                reason: transaction.stop_reason.map(map_stop_reason),
                                timestamp: now.to_rfc3339(),
                                transaction_data: None,
                                transaction_id: csms_transaction_id,
                            })
                            .await?;
                    }
                }
                Ok(())
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_stop_reason_maps_to_the_matching_wire_reason() {
            assert_eq!(map_stop_reason(StopReason::Local), Reason::Local);
            assert_eq!(map_stop_reason(StopReason::Remote), Reason::Remote);
            assert_eq!(
                map_stop_reason(StopReason::EVDisconnected),
                Reason::EVDisconnected
            );
            assert_eq!(
                map_stop_reason(StopReason::EmergencyStop),
                Reason::EmergencyStop
            );
        }

        #[test]
        fn a_sample_reports_every_present_measurand_as_a_string() {
            let sample = MeterSample {
                energy_wh: 1_500,
                power_w: Some(7_400),
                current_ma: Some(32_000),
                voltage_v: Some(230),
                soc_percent: Some(42),
            };

            let values = sampled_values(sample);

            assert_eq!(values.len(), 5);
            assert!(values.iter().any(|value| value.measurand
                == Some(Measurand::EnergyActiveImportRegister)
                && value.value == "1500"));
            assert!(
                values
                    .iter()
                    .any(|value| value.measurand == Some(Measurand::CurrentImport)
                        && value.value == "32")
            );
        }

        #[test]
        fn ocpp1_6_transaction_notifier_can_be_constructed_from_a_client_and_topology() {
            // Compile-level check that `Ocpp1_6TransactionNotifier::new` accepts the same
            // topology shape `Ocpp1_6StatusNotifier` does - actual wiring is exercised via
            // `with_clock`'s impl, only reachable with a live `OCPP1_6Client`.
            fn assert_ctor<
                F: Fn(
                    OCPP1_6Client,
                    [usize; 2],
                ) -> Ocpp1_6TransactionNotifier<crate::clock::SystemClock>,
            >(
                _: F,
            ) {
            }
            assert_ctor(Ocpp1_6TransactionNotifier::new);
        }

        /// A [`crate::clock::Clock`] that always reads a fixed, caller-chosen instant.
        struct FixedClock(chrono::DateTime<chrono::Utc>);

        impl crate::clock::Clock for FixedClock {
            fn now(&self) -> chrono::DateTime<chrono::Utc> {
                self.0
            }
        }

        #[test]
        fn ocpp1_6_transaction_notifier_can_be_constructed_with_a_caller_supplied_clock() {
            // Compile-level check that `with_clock` accepts a non-`SystemClock` fake, unlocking
            // the no_std path `crate::clock::SystemClock` (a `std`-only type) can't reach - the
            // actual timestamp behavior is exercised end-to-end via
            // `with_clock`'s tests (only reachable with a live `OCPP1_6Client`).
            fn assert_ctor<
                F: Fn(OCPP1_6Client, [usize; 2], FixedClock) -> Ocpp1_6TransactionNotifier<FixedClock>,
            >(
                _: F,
            ) {
            }
            assert_ctor(Ocpp1_6TransactionNotifier::with_clock);
        }
    }
}
