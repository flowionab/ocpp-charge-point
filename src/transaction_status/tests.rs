//! Tests for `GetTransactionStatus`'s protocol-agnostic half (B5.4), against the requirements
//! table in OCPP 2.1's E14 "Check transaction status" (E14.FR.01-08).

use super::*;
use crate::executor::TokioExecutor;
use crate::offline_queue::OverflowPolicy;
use crate::state::{
    ChargePointEvent, ConnectorEvent, EvseEvent, IdToken, IdTokenKind, StopReason, Transaction,
    TransactionChargingState, TransactionEventKind,
};

fn test_id_token() -> IdToken {
    IdToken {
        value: "04A224B2".into(),
        kind: IdTokenKind::ISO14443,
    }
}

async fn send(actor: &ChargePointActor, event: ConnectorEvent) {
    actor
        .send(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::Connector {
                connector_id: 0,
                event,
            },
        })
        .await
        .unwrap();
}

/// Starts transaction id 0 on connector 0 and leaves it running (`Charging`, no `stop_reason`).
async fn actor_with_an_ongoing_transaction() -> ChargePointActor {
    let actor = ChargePointActor::spawn([1], &TokioExecutor);
    for event in [
        ConnectorEvent::CableConnected,
        ConnectorEvent::LockConfirmed,
        ConnectorEvent::IdTokenPresented(test_id_token()),
        ConnectorEvent::ChargingAuthorized(test_id_token()),
        ConnectorEvent::ContactorClosed,
    ] {
        send(&actor, event).await;
    }
    actor
}

fn queued_event(id: TransactionId) -> TransactionEventOccurred {
    TransactionEventOccurred {
        evse_id: 0,
        connector_id: 0,
        kind: TransactionEventKind::Started,
        transaction: Transaction {
            id,
            id_token: None,
            charging_state: TransactionChargingState::Charging,
            stop_reason: None,
            seq_no: 0,
            last_meter_sample: None,
            priority_charging: false,
            remote_start_id: None,
            reservation_id: None,
            stop_at_energy_wh: None,
            limit: None,
            csms_limit: None,
            limit_reached: None,
            energy_start_wh: None,
        },
        offline: false,
    }
}

// E14.FR.01: unknown transaction id -> ongoingIndicator = false, messagesInQueue = false.
#[tokio::test]
async fn an_unknown_transaction_answers_false_and_false() {
    let actor = ChargePointActor::spawn([1], &TokioExecutor);

    let status = handle_get_transaction_status(
        &actor,
        None,
        TransactionStatusQuery {
            transaction_id: Some(TransactionId(999)),
        },
    );

    assert_eq!(status.ongoing_indicator, Some(false));
    assert!(!status.messages_in_queue);
}

// E14.FR.02: a transaction that hasn't stopped -> ongoingIndicator = true.
#[tokio::test]
async fn a_running_transaction_answers_ongoing() {
    let actor = actor_with_an_ongoing_transaction().await;

    let status = handle_get_transaction_status(
        &actor,
        None,
        TransactionStatusQuery {
            transaction_id: Some(TransactionId(0)),
        },
    );

    assert_eq!(status.ongoing_indicator, Some(true));
}

// E14.FR.03: a transaction that has stopped -> ongoingIndicator = false. Caught mid-`Stopping`,
// after `stop_reason` is recorded but before the `Ended` TransactionEvent clears its slot - see
// this module's docs on why a fully-cleared slot answers identically.
#[tokio::test]
async fn a_transaction_mid_stop_answers_not_ongoing() {
    let actor = actor_with_an_ongoing_transaction().await;
    send(&actor, ConnectorEvent::ChargingStopped(StopReason::Local)).await;

    let status = handle_get_transaction_status(
        &actor,
        None,
        TransactionStatusQuery {
            transaction_id: Some(TransactionId(0)),
        },
    );

    assert_eq!(status.ongoing_indicator, Some(false));
}

// E14.FR.03's other half: once `Ended` has actually fired and the slot is cleared, a finished
// transaction looks exactly like one that never existed (E14.FR.01) - see this module's docs.
#[tokio::test]
async fn a_fully_ended_transaction_answers_the_same_as_unknown() {
    let actor = actor_with_an_ongoing_transaction().await;
    send(&actor, ConnectorEvent::ChargingStopped(StopReason::Local)).await;
    send(&actor, ConnectorEvent::ContactorOpened).await;

    let status = handle_get_transaction_status(
        &actor,
        None,
        TransactionStatusQuery {
            transaction_id: Some(TransactionId(0)),
        },
    );

    assert_eq!(status.ongoing_indicator, Some(false));
    assert!(!status.messages_in_queue);
}

// E14.FR.04/05: `messagesInQueue` scoped to the named transaction.
#[tokio::test]
async fn messages_in_queue_is_scoped_to_the_named_transaction() {
    let actor = ChargePointActor::spawn([1], &TokioExecutor);
    let queue = OfflineQueue::with_capacity(10).with_overflow_policy(OverflowPolicy::DropNewest);
    queue.push(queued_event(TransactionId(7)));

    // FR.04: a message is queued for the transaction asked about.
    let status = handle_get_transaction_status(
        &actor,
        Some(&queue),
        TransactionStatusQuery {
            transaction_id: Some(TransactionId(7)),
        },
    );
    assert!(status.messages_in_queue);

    // FR.05: nothing is queued for a *different* transaction, even though the queue isn't empty.
    let status = handle_get_transaction_status(
        &actor,
        Some(&queue),
        TransactionStatusQuery {
            transaction_id: Some(TransactionId(8)),
        },
    );
    assert!(!status.messages_in_queue);
}

// E14.FR.06: no transactionId in the request -> the response must not set ongoingIndicator at
// all, not even to `false` - there's no transaction it could describe.
#[tokio::test]
async fn no_transaction_id_leaves_ongoing_indicator_unset() {
    let actor = ChargePointActor::spawn([1], &TokioExecutor);

    let status = handle_get_transaction_status(
        &actor,
        None,
        TransactionStatusQuery {
            transaction_id: None,
        },
    );

    assert_eq!(status.ongoing_indicator, None);
}

// E14.FR.07/08: with no transactionId, `messagesInQueue` answers for the whole backlog rather
// than any one transaction.
#[tokio::test]
async fn no_transaction_id_reports_whether_anything_at_all_is_queued() {
    let actor = ChargePointActor::spawn([1], &TokioExecutor);
    let queue = OfflineQueue::with_capacity(10).with_overflow_policy(OverflowPolicy::DropNewest);

    // FR.08: nothing queued at all.
    let status = handle_get_transaction_status(
        &actor,
        Some(&queue),
        TransactionStatusQuery {
            transaction_id: None,
        },
    );
    assert!(!status.messages_in_queue);

    // FR.07: something is queued, for *some* transaction - which one doesn't matter here.
    queue.push(queued_event(TransactionId(3)));
    let status = handle_get_transaction_status(
        &actor,
        Some(&queue),
        TransactionStatusQuery {
            transaction_id: None,
        },
    );
    assert!(status.messages_in_queue);
}

/// If the Transactions block's CSMS-forwarding queue was never wired up (no
/// `transaction_events`/`transaction_events_persisted` call), nothing this crate produces is ever
/// queued through it - so `false` here is the honest answer, not a degraded fallback.
#[tokio::test]
async fn with_no_queue_wired_messages_in_queue_is_always_false() {
    let actor = ChargePointActor::spawn([1], &TokioExecutor);

    let status = handle_get_transaction_status(
        &actor,
        None,
        TransactionStatusQuery {
            transaction_id: Some(TransactionId(0)),
        },
    );

    assert!(!status.messages_in_queue);
}
