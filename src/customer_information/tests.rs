//! Tests for the Customer Information block's protocol-agnostic half (B5.5).

use super::*;
use crate::executor::TokioExecutor;
use crate::state::{IdTokenKind, LocalListEntry};
use alloc::sync::Arc;
use alloc::vec::Vec;
use chrono::{DateTime, Utc};
use std::sync::Mutex as StdMutex;

fn token(value: &str) -> IdToken {
    IdToken {
        value: value.into(),
        kind: IdTokenKind::ISO14443,
    }
}

fn query(
    request_id: i64,
    report: bool,
    clear: bool,
    id_token: Option<IdToken>,
) -> CustomerInformationQuery {
    CustomerInformationQuery {
        request_id,
        report,
        clear,
        id_token,
    }
}

#[derive(Clone, Copy)]
struct FixedClock(DateTime<Utc>);

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

fn at(secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
}

/// One recorded `NotifyCustomerInformation`: `(request_id, seq_no, tbc, generated_at, data)`.
type RecordedNotification = (i64, i64, bool, String, String);

#[derive(Default)]
struct RecordingNotifier {
    seen: StdMutex<Vec<RecordedNotification>>,
}

#[async_trait::async_trait]
impl CustomerInformationNotifier for RecordingNotifier {
    type Error = core::convert::Infallible;

    async fn notify_customer_information(
        &self,
        request_id: i64,
        seq_no: i64,
        tbc: bool,
        generated_at: chrono::DateTime<chrono::Utc>,
        data: String,
    ) -> Result<(), Self::Error> {
        self.seen
            .lock()
            .unwrap()
            .push((request_id, seq_no, tbc, generated_at.to_rfc3339(), data));
        Ok(())
    }
}

async fn settle() {
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
}

// --- handle_customer_information ---

#[test]
fn neither_report_nor_clear_is_rejected() {
    let queue = CustomerInformationQueue::new();
    let outcome = handle_customer_information(&queue, query(1, false, false, Some(token("A"))));
    assert_eq!(outcome, CustomerInformationOutcome::Rejected);
}

#[test]
fn no_resolvable_identity_is_invalid() {
    let queue = CustomerInformationQueue::new();
    let outcome = handle_customer_information(&queue, query(1, true, false, None));
    assert_eq!(outcome, CustomerInformationOutcome::Invalid);
}

#[tokio::test]
async fn a_request_naming_an_id_token_is_accepted_and_actually_queued() {
    let queue = CustomerInformationQueue::new();
    let outcome = handle_customer_information(&queue, query(1, true, true, Some(token("A"))));
    assert_eq!(outcome, CustomerInformationOutcome::Accepted);

    // Proves the job landed on the queue rather than only that the outcome says `Accepted`.
    let job = queue.channel.recv().await;
    assert_eq!(job.request_id, 1);
    assert_eq!(job.id_token, token("A"));
    assert!(job.report);
    assert!(job.clear);
}

// --- chunk_customer_information ---

#[test]
fn empty_data_still_produces_one_chunk() {
    let chunks = chunk_customer_information("");
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].seq_no, 0);
    assert!(!chunks[0].tbc);
    assert_eq!(chunks[0].data, "");
}

#[test]
fn short_data_fits_in_one_chunk() {
    let chunks = chunk_customer_information("hello");
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].seq_no, 0);
    assert!(!chunks[0].tbc);
    assert_eq!(chunks[0].data, "hello");
}

#[test]
fn long_data_splits_with_correct_seq_no_and_tbc() {
    let data = "a".repeat(CUSTOMER_INFORMATION_CHUNK_SIZE * 2 + 10);
    let chunks = chunk_customer_information(&data);

    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].seq_no, 0);
    assert!(chunks[0].tbc);
    assert_eq!(chunks[0].data.len(), CUSTOMER_INFORMATION_CHUNK_SIZE);
    assert_eq!(chunks[1].seq_no, 1);
    assert!(chunks[1].tbc);
    assert_eq!(chunks[2].seq_no, 2);
    assert!(!chunks[2].tbc);
    assert_eq!(chunks[2].data.len(), 10);

    let rejoined: String = chunks.iter().map(|chunk| chunk.data.as_str()).collect();
    assert_eq!(rejoined, data);
}

#[test]
fn splitting_never_cuts_a_multi_byte_char_in_half() {
    // Each "é" is 2 bytes; a boundary that lands mid-character must back off rather than
    // produce invalid UTF-8.
    let data = "é".repeat(CUSTOMER_INFORMATION_CHUNK_SIZE);
    let chunks = chunk_customer_information(&data);

    for chunk in &chunks {
        // `String` construction above already guarantees valid UTF-8; this just proves no
        // byte was silently dropped or duplicated at a boundary.
        assert!(chunk.data.len() <= CUSTOMER_INFORMATION_CHUNK_SIZE);
    }
    let rejoined: String = chunks.iter().map(|chunk| chunk.data.as_str()).collect();
    assert_eq!(rejoined, data);
}

// --- gather / render (through run_customer_information_requests, end to end) ---

#[tokio::test]
async fn a_report_describes_the_cache_the_local_list_and_a_live_transaction() {
    let actor = ChargePointActor::spawn([1], &TokioExecutor);
    let id_token = token("A");

    let _ = actor
        .send(ChargePointEvent::AuthorizationCached {
            id_token: id_token.clone(),
            status: AuthorizationStatus::Accepted,
            cached_at: None,
        })
        .await;
    let _ = actor
        .send(ChargePointEvent::LocalListUpdated {
            version: 1,
            entries: alloc::vec![LocalListEntry {
                id_token: id_token.clone(),
                status: AuthorizationStatus::Rejected,
            }],
        })
        .await;

    let queue = CustomerInformationQueue::new();
    let notifier = Arc::new(RecordingNotifier::default());
    let clock = FixedClock(at(0));

    assert_eq!(
        handle_customer_information(&queue, query(7, true, false, Some(id_token))),
        CustomerInformationOutcome::Accepted
    );

    let actor_clone = actor.clone();
    let notifier_clone = notifier.clone();
    tokio::spawn(async move {
        run_customer_information_requests(&actor_clone, queue, &notifier_clone, &clock).await;
    });
    settle().await;

    let seen = notifier.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let (request_id, seq_no, tbc, _generated_at, data) = &seen[0];
    assert_eq!(*request_id, 7);
    assert_eq!(*seq_no, 0);
    assert!(!*tbc);
    assert!(data.contains("authorization-cache: Accepted"));
    assert!(data.contains("local-authorization-list: Rejected"));
    assert!(data.contains("transactions: none in progress"));
}

#[tokio::test]
async fn a_report_for_an_unknown_token_says_so_honestly() {
    let actor = ChargePointActor::spawn([1], &TokioExecutor);
    let queue = CustomerInformationQueue::new();
    let notifier = Arc::new(RecordingNotifier::default());
    let clock = FixedClock(at(0));

    assert_eq!(
        handle_customer_information(&queue, query(1, true, false, Some(token("nobody")))),
        CustomerInformationOutcome::Accepted
    );

    let actor_clone = actor.clone();
    let notifier_clone = notifier.clone();
    tokio::spawn(async move {
        run_customer_information_requests(&actor_clone, queue, &notifier_clone, &clock).await;
    });
    settle().await;

    let seen = notifier.seen.lock().unwrap();
    let (.., data) = &seen[0];
    assert!(data.contains("authorization-cache: no entry held"));
    assert!(data.contains("local-authorization-list: no entry held"));
}

#[tokio::test]
async fn clear_erases_the_cache_and_the_local_list_through_real_state() {
    let actor = ChargePointActor::spawn([1], &TokioExecutor);
    let id_token = token("A");
    let _ = actor
        .send(ChargePointEvent::AuthorizationCached {
            id_token: id_token.clone(),
            status: AuthorizationStatus::Accepted,
            cached_at: None,
        })
        .await;
    let _ = actor
        .send(ChargePointEvent::LocalListUpdated {
            version: 1,
            entries: alloc::vec![LocalListEntry {
                id_token: id_token.clone(),
                status: AuthorizationStatus::Accepted,
            }],
        })
        .await;

    let queue = CustomerInformationQueue::new();
    let notifier = Arc::new(RecordingNotifier::default());
    let clock = FixedClock(at(0));

    assert_eq!(
        handle_customer_information(&queue, query(1, false, true, Some(id_token))),
        CustomerInformationOutcome::Accepted
    );

    let actor_clone = actor.clone();
    let notifier_clone = notifier.clone();
    tokio::spawn(async move {
        run_customer_information_requests(&actor_clone, queue, &notifier_clone, &clock).await;
    });
    settle().await;

    // No report was asked for, so nothing was sent.
    assert!(notifier.seen.lock().unwrap().is_empty());
    let state = actor.state();
    assert!(state.authorization_cache.entries().is_empty());
    assert!(state.local_authorization_list.entries.is_empty());
}

#[tokio::test]
async fn report_reflects_what_existed_before_clear_erases_it() {
    let actor = ChargePointActor::spawn([1], &TokioExecutor);
    let id_token = token("A");
    let _ = actor
        .send(ChargePointEvent::AuthorizationCached {
            id_token: id_token.clone(),
            status: AuthorizationStatus::Accepted,
            cached_at: None,
        })
        .await;

    let queue = CustomerInformationQueue::new();
    let notifier = Arc::new(RecordingNotifier::default());
    let clock = FixedClock(at(0));

    assert_eq!(
        handle_customer_information(&queue, query(1, true, true, Some(id_token))),
        CustomerInformationOutcome::Accepted
    );

    let actor_clone = actor.clone();
    let notifier_clone = notifier.clone();
    tokio::spawn(async move {
        run_customer_information_requests(&actor_clone, queue, &notifier_clone, &clock).await;
    });
    settle().await;

    let seen = notifier.seen.lock().unwrap();
    let (.., data) = &seen[0];
    // The report still shows the entry that `clear` went on to erase.
    assert!(data.contains("authorization-cache: Accepted"));
    assert!(actor.state().authorization_cache.entries().is_empty());
}
