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
    use crate::state::{StopReason, Transaction, TransactionChargingState, TransactionEventKind};
    use ocpp_client::rust_ocpp::v2_1::enumerations::{
        ChargingStateEnumType, ReasonEnumType, TransactionEventEnumType, TriggerReasonEnumType,
    };

    // The four functions below are only consumed by `with_system_clock` (`std`-gated) and by
    // this module's own tests; without either, they're legitimately unused.

    #[cfg_attr(not(feature = "std"), allow(dead_code))]
    pub(super) fn map_event_type(kind: TransactionEventKind) -> TransactionEventEnumType {
        match kind {
            TransactionEventKind::Started => TransactionEventEnumType::Started,
            TransactionEventKind::Updated => TransactionEventEnumType::Updated,
            TransactionEventKind::Ended => TransactionEventEnumType::Ended,
        }
    }

    #[cfg_attr(not(feature = "std"), allow(dead_code))]
    pub(super) fn map_charging_state(state: TransactionChargingState) -> ChargingStateEnumType {
        match state {
            TransactionChargingState::EvConnected => ChargingStateEnumType::EVConnected,
            TransactionChargingState::Charging => ChargingStateEnumType::Charging,
            TransactionChargingState::SuspendedEV => ChargingStateEnumType::SuspendedEV,
            TransactionChargingState::SuspendedEVSE => ChargingStateEnumType::SuspendedEVSE,
            TransactionChargingState::Idle => ChargingStateEnumType::Idle,
        }
    }

    #[cfg_attr(not(feature = "std"), allow(dead_code))]
    pub(super) fn map_stop_reason(reason: StopReason) -> ReasonEnumType {
        match reason {
            StopReason::Local => ReasonEnumType::Local,
            StopReason::Remote => ReasonEnumType::Remote,
            StopReason::EVDisconnected => ReasonEnumType::EVDisconnected,
            StopReason::EmergencyStop => ReasonEnumType::EmergencyStop,
        }
    }

    /// The OCPP `triggerReason` a TransactionEvent carries isn't part of our internal
    /// `Transaction`/`TransactionEventKind` model (see CLAUDE.md's version-adapter principle) -
    /// it's derived here from the event kind and, for `Ended`, the transaction's stop reason.
    #[cfg_attr(not(feature = "std"), allow(dead_code))]
    pub(super) fn trigger_reason_for(
        kind: TransactionEventKind,
        transaction: &Transaction,
    ) -> TriggerReasonEnumType {
        match kind {
            TransactionEventKind::Started => TriggerReasonEnumType::Authorized,
            TransactionEventKind::Updated => TriggerReasonEnumType::ChargingStateChanged,
            TransactionEventKind::Ended => match transaction.stop_reason {
                Some(StopReason::EmergencyStop) => TriggerReasonEnumType::AbnormalCondition,
                Some(StopReason::Remote) => TriggerReasonEnumType::RemoteStop,
                Some(StopReason::EVDisconnected) => TriggerReasonEnumType::EVDeparted,
                Some(StopReason::Local) | None => TriggerReasonEnumType::StopAuthorized,
            },
        }
    }

    // `TransactionEventRequest` needs a timestamp; producing one without a caller-supplied
    // `Clock` requires the `std`-only `SystemClock` (see `crate::clock`), so this impl - unlike
    // the rest of this file - needs both `ocpp_2_1` and `std`.
    #[cfg(feature = "std")]
    mod with_system_clock {
        use super::{map_charging_state, map_event_type, map_stop_reason, trigger_reason_for};
        use crate::clock::{Clock, SystemClock};
        use crate::state::{Transaction, TransactionEventKind};
        use crate::transactions::TransactionNotifier;
        use alloc::boxed::Box;
        use alloc::string::ToString;
        use alloc::vec::Vec;
        use ocpp_client::ClientError;
        use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};
        use ocpp_client::rust_ocpp::v2_1::datatypes::{EVSEType, TransactionType};
        use ocpp_client::rust_ocpp::v2_1::messages::transaction_event::TransactionEventRequest;

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
                self.send_transaction_event(TransactionEventRequest {
                    custom_data: None,
                    event_type: map_event_type(kind),
                    meter_value: Vec::new(),
                    timestamp: SystemClock.now(),
                    trigger_reason: trigger_reason_for(kind, &transaction),
                    seq_no: transaction.seq_no as i32,
                    transaction_info: TransactionType {
                        transaction_id: transaction.id.0.to_string(),
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
                    evse: Some(EVSEType {
                        id: evse_id as i32,
                        connector_id: Some(connector_id as i32),
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
                TransactionEventEnumType::Started
            );
            assert_eq!(
                map_event_type(TransactionEventKind::Updated),
                TransactionEventEnumType::Updated
            );
            assert_eq!(
                map_event_type(TransactionEventKind::Ended),
                TransactionEventEnumType::Ended
            );
        }

        #[test]
        fn every_charging_state_maps_to_the_matching_wire_state() {
            assert_eq!(
                map_charging_state(TransactionChargingState::EvConnected),
                ChargingStateEnumType::EVConnected
            );
            assert_eq!(
                map_charging_state(TransactionChargingState::Charging),
                ChargingStateEnumType::Charging
            );
            assert_eq!(
                map_charging_state(TransactionChargingState::SuspendedEV),
                ChargingStateEnumType::SuspendedEV
            );
            assert_eq!(
                map_charging_state(TransactionChargingState::SuspendedEVSE),
                ChargingStateEnumType::SuspendedEVSE
            );
            assert_eq!(
                map_charging_state(TransactionChargingState::Idle),
                ChargingStateEnumType::Idle
            );
        }

        #[test]
        fn started_and_updated_use_fixed_trigger_reasons() {
            let transaction = Transaction {
                id: crate::state::TransactionId(0),
                charging_state: TransactionChargingState::Charging,
                stop_reason: None,
                seq_no: 0,
            };

            assert_eq!(
                trigger_reason_for(TransactionEventKind::Started, &transaction),
                TriggerReasonEnumType::Authorized
            );
            assert_eq!(
                trigger_reason_for(TransactionEventKind::Updated, &transaction),
                TriggerReasonEnumType::ChargingStateChanged
            );
        }

        #[test]
        fn ended_derives_its_trigger_reason_from_the_stop_reason() {
            let base = Transaction {
                id: crate::state::TransactionId(0),
                charging_state: TransactionChargingState::EvConnected,
                stop_reason: None,
                seq_no: 2,
            };

            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Ended,
                    &Transaction {
                        stop_reason: Some(StopReason::EmergencyStop),
                        ..base
                    }
                ),
                TriggerReasonEnumType::AbnormalCondition
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Ended,
                    &Transaction {
                        stop_reason: Some(StopReason::Remote),
                        ..base
                    }
                ),
                TriggerReasonEnumType::RemoteStop
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Ended,
                    &Transaction {
                        stop_reason: Some(StopReason::EVDisconnected),
                        ..base
                    }
                ),
                TriggerReasonEnumType::EVDeparted
            );
            assert_eq!(
                trigger_reason_for(
                    TransactionEventKind::Ended,
                    &Transaction {
                        stop_reason: Some(StopReason::Local),
                        ..base
                    }
                ),
                TriggerReasonEnumType::StopAuthorized
            );
            assert_eq!(
                trigger_reason_for(TransactionEventKind::Ended, &base),
                TriggerReasonEnumType::StopAuthorized
            );
        }
    }
}
