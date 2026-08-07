//! Cuts the power at **every** point across a charging session's lifecycle and asserts what
//! survives at each one - E4.4 (`docs/PRODUCTION-ROADMAP.md` §7.4), and the exit criterion M2 is
//! measured against:
//!
//! > A power cut at any point during a transaction must not lose the energy already delivered.
//!
//! # Why a sweep rather than one more end-to-end test
//!
//! `src/persistence.rs`'s own tests already cut once, mid-charge, and prove the energy comes back.
//! The risk that test can't cover is *which* point was chosen: a durability bug that only bites
//! between `Started` being raised and its record reaching storage, or between the last meter write
//! and the transaction ending, is invisible to a cut taken anywhere else. This file therefore
//! drives the same lifecycle once per cut point - cutting after 0 steps, after 1, after 2, ... -
//! and asserts the same invariants at each, so a regression anywhere along the sequence fails the
//! build rather than only the lucky ones.
//!
//! # What "a power cut" means here
//!
//! The actor and every task holding RAM state are dropped; only the `Storage` handle crosses into
//! the next boot. Nothing is flushed, closed, or drained first - that is the point. A fresh actor
//! is then spawned and `restore_transactions` is the only thing that runs before the assertions,
//! exactly as `ChargePointBuilder::transaction_persistence` does at boot.
//!
//! # What this does not cover
//!
//! A cut *mid-write* (a torn record) is `AtomicStorage`'s concern and is tested directly in
//! `src/hardware/storage.rs`; `InMemoryStorage` here is atomic by construction, so this sweep
//! exercises cut-between-writes, which is the case the write *policy* governs.
//!
//! The other three E2 rows that survive a reboot - the local authorization list, reservations and
//! the device model - are not swept here: none of them is written *during* a transaction, so a
//! per-cut-point sweep would re-run one storage round-trip at every step and prove nothing their
//! own end-to-end tests in `src/persistence.rs` don't already. What is swept here is what the
//! transaction lifecycle actually mutates: the in-flight record, the id counter, and the offline
//! transaction-event queue.

use std::collections::BTreeSet;
use std::sync::Arc;

use ocpp_charge_point::actor::ChargePointActor;
use ocpp_charge_point::clock::SystemClock;
use ocpp_charge_point::executor::TokioExecutor;
use ocpp_charge_point::hardware::InMemoryStorage;
use ocpp_charge_point::persistence::{
    DEFAULT_METER_WRITE_THRESHOLD_WH, TransactionStore, restore_transactions,
    run_transaction_persistence,
};
use ocpp_charge_point::state::{
    ChargePointEvent, ConnectorEvent, EvseEvent, IdToken, IdTokenKind, MeterSample, StopReason,
    TransactionEventKind,
};

/// One step of the session lifecycle, named so a failing cut point reports *where* it cut rather
/// than just an index.
struct Step {
    name: &'static str,
    event: ConnectorEvent,
    /// The energy register reading this step delivers, if it is a meter sample.
    sample_wh: Option<i64>,
    /// Whether a transaction is in flight *after* this step - i.e. whether a cut taken here should
    /// recover something. `Started` is raised on authorization and the record is cleared when the
    /// contactor confirms open, so this is true from `ChargingAuthorized` through `ChargingStopped`
    /// and false either side.
    transaction_in_flight: bool,
}

fn id_token() -> IdToken {
    IdToken {
        value: "04A224B2".into(),
        kind: IdTokenKind::ISO14443,
    }
}

fn sample(energy_wh: i64) -> ConnectorEvent {
    ConnectorEvent::MeterValueSampled(MeterSample {
        energy_wh,
        ..Default::default()
    })
}

/// A complete session, plug to unplug, with a meter sample per reading in `samples`. Every cut
/// point in this sequence is exercised, including the two boundaries that matter most: the
/// authorization that creates the transaction, and the contactor-open that ends it.
fn lifecycle(samples: &[i64]) -> Vec<Step> {
    let mut steps = vec![
        Step {
            name: "cable connected",
            event: ConnectorEvent::CableConnected,
            sample_wh: None,
            transaction_in_flight: false,
        },
        Step {
            name: "lock confirmed",
            event: ConnectorEvent::LockConfirmed,
            sample_wh: None,
            transaction_in_flight: false,
        },
        Step {
            name: "id token presented",
            event: ConnectorEvent::IdTokenPresented(id_token()),
            sample_wh: None,
            transaction_in_flight: false,
        },
        Step {
            name: "charging authorized",
            event: ConnectorEvent::ChargingAuthorized(id_token()),
            sample_wh: None,
            transaction_in_flight: true,
        },
        Step {
            name: "contactor closed",
            event: ConnectorEvent::ContactorClosed,
            sample_wh: None,
            transaction_in_flight: true,
        },
    ];
    for energy_wh in samples {
        steps.push(Step {
            name: "meter sample",
            event: sample(*energy_wh),
            sample_wh: Some(*energy_wh),
            transaction_in_flight: true,
        });
    }
    steps.extend([
        Step {
            name: "charging stopped",
            event: ConnectorEvent::ChargingStopped(StopReason::Local),
            sample_wh: None,
            transaction_in_flight: true,
        },
        Step {
            name: "contactor opened",
            event: ConnectorEvent::ContactorOpened,
            sample_wh: None,
            transaction_in_flight: false,
        },
        Step {
            name: "unlock confirmed",
            event: ConnectorEvent::UnlockConfirmed,
            sample_wh: None,
            transaction_in_flight: false,
        },
    ]);
    steps
}

/// Lets every spawned task drain whatever the actor just published. Generous on purpose: an
/// under-settled harness would "prove" durability by cutting before the writes it is measuring.
async fn settle() {
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
}

/// What a reboot recovered.
#[derive(Debug, Default)]
struct Recovery {
    /// How many transactions `restore_transactions` reported.
    count: usize,
    /// The energy register reading on the recovered (closed-out) transaction.
    billable_energy_wh: Option<i64>,
    /// The recovered transaction's id, so a sweep can prove ids are never reused.
    transaction_id: Option<u64>,
}

/// Runs `steps[..cut_after]` against a fresh charge point persisting through `storage`, then cuts
/// the power and boots a fresh one that recovers from storage alone.
///
/// Returns what the reboot recovered. The pre-cut charge point is dropped *before* the new one is
/// spawned, so nothing in RAM can leak across the boundary - the only channel between them is
/// `storage`.
async fn cut_after(
    storage: &Arc<InMemoryStorage>,
    steps: &[Step],
    cut_after: usize,
    write_threshold_wh: i64,
) -> Recovery {
    {
        let before = ChargePointActor::spawn([1], &TokioExecutor);
        let events = before.subscribe_transaction_events();
        let store = TransactionStore::new(storage.clone())
            .with_meter_write_threshold_wh(write_threshold_wh);
        // A real boot recovers *before* it starts persisting, and before anything can start a new
        // transaction - `ChargePointBuilder::transaction_persistence` does exactly this pair in
        // this order. Skipping it here would leave this charge point issuing transaction ids from
        // 0 again, which is precisely the id reuse
        // `a_transaction_id_is_never_reused_across_repeated_cuts_on_the_same_storage` exists to
        // catch, so the harness has to model the ordering rather than assume it.
        restore_transactions(&before, &TransactionStore::new(storage.clone())).await;
        tokio::spawn(async move {
            run_transaction_persistence(events, &store, &SystemClock).await;
        });

        for step in &steps[..cut_after] {
            let _ = before
                .send(ChargePointEvent::Evse {
                    evse_id: 0,
                    event: EvseEvent::Connector {
                        connector_id: 0,
                        event: step.event.clone(),
                    },
                })
                .await;
        }
        settle().await;

        // --- the cut: `before` and its persistence task lose everything they held in RAM.
    }

    let after = ChargePointActor::spawn([1], &TokioExecutor);
    let mut recovered_events = after.subscribe_transaction_events();
    let store = TransactionStore::new(storage.clone());
    let count = restore_transactions(&after, &store).await;

    let mut recovery = Recovery {
        count,
        ..Default::default()
    };
    if count > 0 {
        let reported = recovered_events
            .recv()
            .await
            .expect("a recovered transaction is closed out with an Ended event");
        assert_eq!(
            reported.kind,
            TransactionEventKind::Ended,
            "a recovered transaction must be closed out, not resumed"
        );
        assert_eq!(
            reported.transaction.stop_reason,
            Some(StopReason::PowerLoss),
            "a recovered transaction must report why it ended"
        );
        assert!(
            after.state().next_transaction_id > reported.transaction.id.0,
            "the id counter must come back too, or the next session reuses an id the CSMS has seen"
        );
        recovery.billable_energy_wh = reported
            .transaction
            .last_meter_sample
            .map(|sample| sample.energy_wh);
        recovery.transaction_id = Some(reported.transaction.id.0);
    }

    // Recovery is not repeatable: the record is cleared once the state machine has taken ownership
    // of it, so a second reboot must not report the same session again (which the CSMS would see
    // as a duplicate transaction).
    let second_boot = ChargePointActor::spawn([1], &TokioExecutor);
    assert_eq!(
        restore_transactions(&second_boot, &store).await,
        0,
        "recovering the same interrupted transaction twice would double-report it"
    );

    recovery
}

/// The highest meter reading delivered in `steps[..run]`, i.e. the energy the charge point had
/// actually measured when the power went.
fn delivered_energy_wh(steps: &[Step], run: usize) -> Option<i64> {
    steps[..run]
        .iter()
        .filter_map(|step| step.sample_wh)
        .next_back()
}

#[tokio::test]
async fn every_cut_point_in_a_session_recovers_exactly_what_was_delivered() {
    // Threshold 0: every sample is written, so "no energy lost" is an exact equality rather than a
    // bound. The bounded-loss case the default threshold trades for is the next test.
    let steps = lifecycle(&[1_000, 2_500, 4_200]);

    for run in 0..=steps.len() {
        let storage = Arc::new(InMemoryStorage::new());
        let recovery = cut_after(&storage, &steps, run, 0).await;

        let expected_in_flight = run > 0 && steps[run - 1].transaction_in_flight;
        let where_it_cut = if run == 0 {
            "before the session started"
        } else {
            steps[run - 1].name
        };

        assert_eq!(
            recovery.count,
            usize::from(expected_in_flight),
            "cutting after {run} steps ({where_it_cut}) should recover \
             {} transaction(s)",
            usize::from(expected_in_flight)
        );

        if expected_in_flight {
            assert_eq!(
                recovery.billable_energy_wh,
                delivered_energy_wh(&steps, run),
                "cutting after {run} steps ({where_it_cut}) lost billable energy"
            );
        }
    }
}

#[tokio::test]
async fn energy_lost_to_a_cut_never_exceeds_the_configured_write_threshold() {
    // Samples stepping by less than the write threshold, so most of them are deliberately *not*
    // written (E3.2's flash-wear trade-off). What must hold is the bound the threshold promises:
    // a cut can lose at most that much energy, never more, and never reports energy that was
    // never delivered.
    let threshold = DEFAULT_METER_WRITE_THRESHOLD_WH;
    let samples: Vec<i64> = (1..=12).map(|step| step * 40).collect();
    let steps = lifecycle(&samples);

    for run in 0..=steps.len() {
        let storage = Arc::new(InMemoryStorage::new());
        let recovery = cut_after(&storage, &steps, run, threshold).await;

        if !(run > 0 && steps[run - 1].transaction_in_flight) {
            continue;
        }
        let where_it_cut = steps[run - 1].name;
        let Some(delivered) = delivered_energy_wh(&steps, run) else {
            // Cut before the first sample: nothing was measured, so nothing can be billed.
            assert_eq!(recovery.billable_energy_wh, None);
            continue;
        };
        let recovered = recovery
            .billable_energy_wh
            .expect("a transaction with a delivered reading must recover one");

        assert!(
            recovered <= delivered,
            "cutting after {run} steps ({where_it_cut}) billed {recovered} Wh against \
             {delivered} Wh actually delivered"
        );
        assert!(
            delivered - recovered <= threshold,
            "cutting after {run} steps ({where_it_cut}) lost {} Wh, more than the {threshold} Wh \
             the write threshold promises",
            delivered - recovered
        );
    }
}

/// The two halves of durability, cut at every point *together*: the in-flight record (E2.1) and
/// the offline transaction-event queue (E2.8/E4.3) are each covered on their own, but a real
/// charge point runs both, and what an operator is actually owed is the end-to-end statement -
/// **the CSMS eventually learns about the energy, whatever point the power went at.** A bug in
/// how the two compose (a recovered close-out that never reaches the queue, a backlog replayed
/// out of order behind it) is invisible to either test alone.
#[tokio::test]
async fn at_every_cut_point_the_csms_eventually_learns_the_delivered_energy() {
    use ocpp_charge_point::offline_queue::{OfflineQueue, flush_offline_queue};
    use ocpp_charge_point::persistence::{
        QueueStore, restore_transaction_event_queue, run_persisted_transaction_event_queue,
    };
    use ocpp_charge_point::state::TransactionEventOccurred;

    #[derive(Debug)]
    struct Offline;
    impl std::fmt::Display for Offline {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("the CSMS connection is down")
        }
    }

    let steps = lifecycle(&[1_000, 2_500, 4_200]);

    for run in 0..=steps.len() {
        let storage = Arc::new(InMemoryStorage::new());

        // --- before the cut: the CSMS is unreachable throughout, so every event ends up in the
        // durable queue rather than on the wire.
        {
            let before = ChargePointActor::spawn([1], &TokioExecutor);
            let queue: OfflineQueue<TransactionEventOccurred> = OfflineQueue::new();
            let queue_store = QueueStore::new(storage.clone(), "transaction");
            let queued_events = before.subscribe_transaction_events();
            let persisted_events = before.subscribe_transaction_events();
            restore_transaction_event_queue(&queue, &queue_store).await;
            restore_transactions(&before, &TransactionStore::new(storage.clone())).await;
            let store = TransactionStore::new(storage.clone()).with_meter_write_threshold_wh(0);
            tokio::spawn(async move {
                run_transaction_persistence(persisted_events, &store, &SystemClock).await;
            });
            tokio::spawn(async move {
                run_persisted_transaction_event_queue(
                    queued_events,
                    &queue,
                    &queue_store,
                    |_event| async { Err::<(), _>(Offline) },
                    |_dropped| async {},
                )
                .await;
            });

            for step in &steps[..run] {
                let _ = before
                    .send(ChargePointEvent::Evse {
                        evse_id: 0,
                        event: EvseEvent::Connector {
                            connector_id: 0,
                            event: step.event.clone(),
                        },
                    })
                    .await;
            }
            settle().await;
        }

        // --- after the reboot, with the connection back: the replayed backlog and the recovered
        // close-out both reach the CSMS.
        let after = ChargePointActor::spawn([1], &TokioExecutor);
        let queue: Arc<OfflineQueue<TransactionEventOccurred>> = Arc::new(OfflineQueue::new());
        let queue_store = QueueStore::new(storage.clone(), "transaction");
        let delivered = Arc::new(std::sync::Mutex::new(Vec::new()));
        let queued_events = after.subscribe_transaction_events();
        restore_transaction_event_queue(&queue, &queue_store).await;

        let forwarder_queue = queue.clone();
        let forwarder_delivered = delivered.clone();
        tokio::spawn(async move {
            run_persisted_transaction_event_queue(
                queued_events,
                &forwarder_queue,
                &queue_store,
                move |event: TransactionEventOccurred| {
                    let delivered = forwarder_delivered.clone();
                    async move {
                        delivered.lock().unwrap().push(event);
                        Ok::<(), Offline>(())
                    }
                },
                |_dropped| async {},
            )
            .await;
        });
        // Recovery happens after the forwarder is subscribed, so the close-out it raises is
        // reported like any other event rather than being lost between boot and registration.
        restore_transactions(&after, &TransactionStore::new(storage.clone())).await;
        settle().await;
        // The backlog restored before the forwarder started needs one flush to go out; live
        // events took the forwarder's own path already.
        let flush_delivered = delivered.clone();
        flush_offline_queue(&queue, move |event: TransactionEventOccurred| {
            let delivered = flush_delivered.clone();
            async move {
                delivered.lock().unwrap().push(event);
                Ok::<(), Offline>(())
            }
        })
        .await;

        let delivered = delivered.lock().unwrap().clone();
        let expected_in_flight = run > 0 && steps[run - 1].transaction_in_flight;
        let where_it_cut = if run == 0 {
            "before the session started"
        } else {
            steps[run - 1].name
        };

        if !expected_in_flight {
            continue;
        }

        let ended: Vec<&TransactionEventOccurred> = delivered
            .iter()
            .filter(|event| event.kind == TransactionEventKind::Ended)
            .collect();
        assert_eq!(
            ended.len(),
            1,
            "cutting after {run} steps ({where_it_cut}) should close the session out exactly once, \
             not {} times",
            ended.len()
        );
        assert_eq!(
            ended[0].transaction.stop_reason,
            Some(StopReason::PowerLoss),
            "cutting after {run} steps ({where_it_cut}) should report the power loss"
        );
        assert_eq!(
            ended[0]
                .transaction
                .last_meter_sample
                .map(|sample| sample.energy_wh),
            delivered_energy_wh(&steps, run),
            "cutting after {run} steps ({where_it_cut}): the CSMS was never told about the energy"
        );
        assert_eq!(
            delivered
                .iter()
                .filter(|event| event.kind == TransactionEventKind::Started)
                .count(),
            1,
            "cutting after {run} steps ({where_it_cut}) should still report the session's start \
             exactly once - it was queued before the cut"
        );

        let seq_nos: Vec<u32> = delivered
            .iter()
            .map(|event| event.transaction.seq_no)
            .collect();
        let mut sorted = seq_nos.clone();
        sorted.sort_unstable();
        assert_eq!(
            seq_nos, sorted,
            "cutting after {run} steps ({where_it_cut}) delivered events out of sequence: \
             {seq_nos:?}"
        );
    }
}

#[tokio::test]
async fn a_transaction_id_is_never_reused_across_repeated_cuts_on_the_same_storage() {
    // One storage across the whole sweep, so each iteration boots on top of whatever the previous
    // cut left behind - the way a field unit actually accumulates history. An id the CSMS has
    // already seen must never come back on a later session.
    let storage = Arc::new(InMemoryStorage::new());
    let steps = lifecycle(&[1_000, 2_500]);
    let mut seen = BTreeSet::new();

    for run in 0..=steps.len() {
        let recovery = cut_after(&storage, &steps, run, 0).await;
        if let Some(id) = recovery.transaction_id {
            assert!(
                seen.insert(id),
                "transaction id {id} was reused after a cut following {run} steps"
            );
        }
    }

    assert!(
        !seen.is_empty(),
        "the sweep must actually have recovered some transactions, or it proves nothing"
    );
}
