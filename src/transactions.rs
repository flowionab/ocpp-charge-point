//! Transactions functional block: reporting charging-session lifecycle to the CSMS via
//! TransactionEvent. See `docs/ROADMAP.md` §5.

use crate::state::{TransactionEventKind, TransactionEventOccurred};
use crate::sync::{BroadcastReceiver, RecvError};
use alloc::boxed::Box;

/// Reports a transaction lifecycle event to the CSMS via TransactionEvent. Implemented per
/// protocol version (see the `ocpp_2_1` module), mirroring
/// [`crate::availability::StatusNotifier`].
#[async_trait::async_trait]
pub trait TransactionNotifier {
    type Error: core::error::Error + Send + Sync + 'static;

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
/// only the CSMS-facing report failed.
pub async fn run_transaction_events<N: TransactionNotifier>(
    mut events: BroadcastReceiver<TransactionEventOccurred>,
    notifier: &N,
) {
    loop {
        match events.recv().await {
            Ok(occurred) => {
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
            Err(RecvError::Closed) => break,
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
            charging_state: TransactionChargingState::EvConnected,
            stop_reason: None,
            seq_no: 0,
            last_meter_sample: None,
        };
        sender.send(TransactionEventOccurred {
            evse_id: 0,
            connector_id: 0,
            kind: TransactionEventKind::Started,
            transaction,
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
        ChargingStateEnum, MeasurandEnum, MeterValue, ReadingContextEnum, ReasonEnum,
        SampledValue, TransactionEventEnum, TriggerReasonEnum,
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
                Some(StopReason::Local) | None => TriggerReasonEnum::StopAuthorized,
            },
        }
    }

    /// Builds the TransactionEvent `meterValue` list from a transaction's most recent sample -
    /// empty if it never got one (e.g. `Started`, or a transaction that ended before charging
    /// began). Only the energy register is modeled today; see `docs/ROADMAP.md` §10.
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
            sampled_value: vec![SampledValue {
                value: sample.energy_wh as f64,
                measurand: Some(MeasurandEnum::EnergyActiveImportRegister),
                context: Some(ReadingContextEnum::SamplePeriodic),
                phase: None,
                location: None,
                signed_meter_value: None,
                unit_of_measure: None,
                custom_data: None,
            }],
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
        use ocpp_client::ocpp_types::v21::common::{Transaction as WireTransaction, EVSE};

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
                        ..base
                    }
                ),
                TriggerReasonEnum::AbnormalCondition
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Ended,
                    &Transaction {
                        stop_reason: Some(StopReason::Remote),
                        ..base
                    }
                ),
                TriggerReasonEnum::RemoteStop
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Ended,
                    &Transaction {
                        stop_reason: Some(StopReason::EVDisconnected),
                        ..base
                    }
                ),
                TriggerReasonEnum::EVDeparted
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Ended,
                    &Transaction {
                        stop_reason: Some(StopReason::Local),
                        ..base
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
