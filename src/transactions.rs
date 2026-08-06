//! Transactions functional block: reporting charging-session lifecycle to the CSMS via
//! TransactionEvent. See `docs/ROADMAP.md` §5.

use crate::state::{TransactionEventKind, TransactionEventOccurred};
use crate::sync::BroadcastReceiver;
use alloc::boxed::Box;

#[cfg(feature = "ocpp_1_6")]
pub use self::ocpp_1_6::Ocpp1_6TransactionNotifier;

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
mod ocpp_2_1 {
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

    // The four functions below are only consumed by `with_system_clock` (`std`-gated) and by
    // this module's own tests; without either, they're legitimately unused.

    #[cfg_attr(not(feature = "std"), allow(dead_code))]
    pub(super) fn map_event_type(kind: TransactionEventKind) -> TransactionEventEnum {
        match kind {
            TransactionEventKind::Started => TransactionEventEnum::Started,
            TransactionEventKind::Updated(_) => TransactionEventEnum::Updated,
            TransactionEventKind::Ended => TransactionEventEnum::Ended,
        }
    }

    #[cfg_attr(not(feature = "std"), allow(dead_code))]
    pub(super) fn map_charging_state(state: TransactionChargingState) -> ChargingStateEnum {
        match state {
            TransactionChargingState::EvConnected => ChargingStateEnum::EVConnected,
            TransactionChargingState::Charging => ChargingStateEnum::Charging,
            TransactionChargingState::SuspendedEV => ChargingStateEnum::SuspendedEV,
            TransactionChargingState::SuspendedEVSE => ChargingStateEnum::SuspendedEVSE,
            TransactionChargingState::Idle => ChargingStateEnum::Idle,
        }
    }

    #[cfg_attr(not(feature = "std"), allow(dead_code))]
    pub(super) fn map_stop_reason(reason: StopReason) -> ReasonEnum {
        match reason {
            StopReason::Local => ReasonEnum::Local,
            StopReason::Remote => ReasonEnum::Remote,
            StopReason::EVDisconnected => ReasonEnum::EVDisconnected,
            StopReason::EmergencyStop => ReasonEnum::EmergencyStop,
            StopReason::Reset => ReasonEnum::ImmediateReset,
        }
    }

    /// The OCPP `triggerReason` a TransactionEvent carries isn't part of our internal
    /// `Transaction`/`TransactionEventKind` model (see CLAUDE.md's version-adapter principle) -
    /// it's derived here from the event kind and, for `Ended`, the transaction's stop reason.
    #[cfg_attr(not(feature = "std"), allow(dead_code))]
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
                Some(StopReason::Local) | None => TriggerReasonEnum::StopAuthorized,
            },
        }
    }

    /// Builds one `sampledValue` per measurand present in `sample` - always the energy register,
    /// plus power/current/voltage/SoC when the hardware reported them.
    #[cfg_attr(not(feature = "std"), allow(dead_code))]
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

    #[cfg_attr(not(feature = "std"), allow(dead_code))]
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
    #[cfg_attr(not(feature = "std"), allow(dead_code))]
    pub(super) fn build_meter_values(
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

    // `TransactionEventRequest` needs a timestamp; producing one without a caller-supplied
    // `Clock` requires the `std`-only `SystemClock` (see `crate::clock`), so this impl - unlike
    // the rest of this file - needs both `ocpp_2_1` and `std`.
    #[cfg(feature = "std")]
    mod with_system_clock {
        use super::{
            build_meter_values, map_charging_state, map_event_type, map_stop_reason,
            trigger_reason_for,
        };
        use crate::clock::{Clock, SystemClock};
        use crate::state::{Transaction, TransactionEventKind};
        use crate::transactions::TransactionNotifier;
        use alloc::boxed::Box;
        use ocpp_client::ClientError;
        use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};
        use ocpp_client::ocpp_types::v21::TransactionEventRequest;
        use ocpp_client::ocpp_types::v21::common::{EVSE, Transaction as WireTransaction};

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
                let now = SystemClock.now();
                let meter_value = build_meter_values(transaction.last_meter_sample, now);
                self.send_transaction_event(TransactionEventRequest {
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
                })
                .await?;
                Ok(())
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
mod ocpp_2_0_1 {
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

    #[cfg_attr(not(feature = "std"), allow(dead_code))]
    pub(super) fn map_event_type(kind: TransactionEventKind) -> TransactionEventEnum {
        match kind {
            TransactionEventKind::Started => TransactionEventEnum::Started,
            TransactionEventKind::Updated(_) => TransactionEventEnum::Updated,
            TransactionEventKind::Ended => TransactionEventEnum::Ended,
        }
    }

    #[cfg_attr(not(feature = "std"), allow(dead_code))]
    pub(super) fn map_charging_state(state: TransactionChargingState) -> ChargingStateEnum {
        match state {
            TransactionChargingState::EvConnected => ChargingStateEnum::EVConnected,
            TransactionChargingState::Charging => ChargingStateEnum::Charging,
            TransactionChargingState::SuspendedEV => ChargingStateEnum::SuspendedEV,
            TransactionChargingState::SuspendedEVSE => ChargingStateEnum::SuspendedEVSE,
            TransactionChargingState::Idle => ChargingStateEnum::Idle,
        }
    }

    #[cfg_attr(not(feature = "std"), allow(dead_code))]
    pub(super) fn map_stop_reason(reason: StopReason) -> ReasonEnum {
        match reason {
            StopReason::Local => ReasonEnum::Local,
            StopReason::Remote => ReasonEnum::Remote,
            StopReason::EVDisconnected => ReasonEnum::EVDisconnected,
            StopReason::EmergencyStop => ReasonEnum::EmergencyStop,
            StopReason::Reset => ReasonEnum::ImmediateReset,
        }
    }

    /// Mirrors [`super::ocpp_2_1::trigger_reason_for`].
    #[cfg_attr(not(feature = "std"), allow(dead_code))]
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
                Some(StopReason::Local) | None => TriggerReasonEnum::StopAuthorized,
            },
        }
    }

    #[cfg_attr(not(feature = "std"), allow(dead_code))]
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

    #[cfg_attr(not(feature = "std"), allow(dead_code))]
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

    #[cfg_attr(not(feature = "std"), allow(dead_code))]
    pub(super) fn build_meter_values(
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

    #[cfg(feature = "std")]
    mod with_system_clock {
        use super::{
            build_meter_values, map_charging_state, map_event_type, map_stop_reason,
            trigger_reason_for,
        };
        use crate::clock::{Clock, SystemClock};
        use crate::state::{Transaction, TransactionEventKind};
        use crate::transactions::TransactionNotifier;
        use alloc::boxed::Box;
        use ocpp_client::ClientError;
        use ocpp_client::ocpp_2_0_1::{OCPP2_0_1Client, OCPP2_0_1Error};
        use ocpp_client::ocpp_types::v201::TransactionEventRequest;
        use ocpp_client::ocpp_types::v201::common::{EVSE, Transaction as WireTransaction};

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
                let now = SystemClock.now();
                let meter_value = build_meter_values(transaction.last_meter_sample, now);
                self.send_transaction_event(TransactionEventRequest {
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
                })
                .await?;
                Ok(())
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
mod ocpp_1_6 {
    use crate::state::{MeterSample, StopReason, TransactionId};
    use alloc::collections::BTreeMap;
    use alloc::string::ToString;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
    use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
    use ocpp_client::ocpp_1_6::OCPP1_6Client;
    use ocpp_client::ocpp_types::v16::common::{Measurand, Reason, SampledValueItem};

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
        }
    }

    /// Builds one `sampledValue` per measurand present in `sample`, mirroring
    /// [`super::ocpp_2_1::sampled_values`] but for 1.6J's string-valued `SampledValueItem`.
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

    /// Wraps an `OCPP1_6Client` with the charge point's connector topology and a cache of
    /// CSMS-assigned transaction ids - see this module's top-level docs.
    pub struct Ocpp1_6TransactionNotifier {
        client: OCPP1_6Client,
        connector_counts: Vec<usize>,
        csms_transaction_ids:
            BlockingMutex<CriticalSectionRawMutex, RefCell<BTreeMap<TransactionId, i64>>>,
    }

    impl Ocpp1_6TransactionNotifier {
        /// Wraps `client`, capturing `connector_counts` (each EVSE's connector count, in
        /// `evse_id` order) for translating connector addresses, starting with no cached
        /// CSMS-assigned transaction ids.
        pub fn new(
            client: OCPP1_6Client,
            connector_counts: impl IntoIterator<Item = usize>,
        ) -> Self {
            Self {
                client,
                connector_counts: connector_counts.into_iter().collect(),
                csms_transaction_ids: BlockingMutex::new(RefCell::new(BTreeMap::new())),
            }
        }
    }

    // `StartTransactionRequest`/`StopTransactionRequest` need a timestamp; producing one without
    // a caller-supplied `Clock` requires the `std`-only `SystemClock` (see `crate::clock`), so
    // this impl - unlike the rest of this file - needs both `ocpp_1_6` and `std`.
    #[cfg(feature = "std")]
    mod with_system_clock {
        use super::{Ocpp1_6TransactionNotifier, map_stop_reason, sampled_values};
        use crate::clock::{Clock, SystemClock};
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

        #[async_trait::async_trait]
        impl TransactionNotifier for Ocpp1_6TransactionNotifier {
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
                let now = SystemClock.now();
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
            // `with_system_clock`'s impl, only reachable with a live `OCPP1_6Client`.
            fn assert_ctor<F: Fn(OCPP1_6Client, [usize; 2]) -> Ocpp1_6TransactionNotifier>(_: F) {}
            assert_ctor(Ocpp1_6TransactionNotifier::new);
        }
    }
}
