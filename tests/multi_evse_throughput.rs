//! Sustained throughput on a multi-EVSE configuration (`docs/PRODUCTION-ROADMAP.md` H4.3).
//!
//! # Reading the roadmap row
//!
//! "Sustained-throughput test on a multi-EVSE configuration" is the third leg alongside
//! `tests/network_flapping_soak.rs` (H4.1, connectivity churn) and `tests/memory_growth.rs` (H4.2,
//! per-transaction retained memory): this one stresses *concurrency*. A real DC site is several
//! EVSEs transacting at once, each producing its own StatusNotification/TransactionEvent/meter
//! traffic, all funnelled through **one actor mailbox and one CSMS connection** - the same
//! constraint `tests/common/mod.rs`'s [`MockCsms`] models (it accepts exactly one connection).
//!
//! Same shape as the other two: a fast deterministic default that gates every push, and an
//! opt-in longer mode for whoever wants to lean on it before a release.
//!
//! # What is actually stressed, and how each invariant is checked
//!
//! - **Many EVSEs transacting concurrently.** [`run_concurrent_sessions`] spawns one task per
//!   EVSE, each driving several complete charging sessions back to back, all against the same
//!   [`ocpp_charge_point::ChargePointRuntime`] and the same [`MockCsms`] connection.
//! - **Per-EVSE ordering under concurrency.** The actor mailbox serializes every event into one
//!   total order, but each EVSE's *own* task only ever sends its next event after awaiting the
//!   previous one's acknowledgement - so interleaving with other EVSEs must never reorder or drop
//!   *within* one EVSE's sequence. [`assert_transactions_complete_and_ordered`] checks every
//!   transaction's `seqNo` run is gap-free, duplicate-free, and starts `Started`/ends `Ended`,
//!   which a cross-EVSE reorder or a dropped event would break.
//! - **Per-EVSE isolation.** One EVSE's connector is deliberately driven into a hardware fault
//!   mid-session ([`run_fault_isolation`]) while every other EVSE keeps transacting. The
//!   assertion is that the healthy EVSEs' sessions all still complete (not merely "eventually" -
//!   within the same bounded wait everything else uses) and that the faulted EVSE's transaction
//!   is force-ended rather than left dangling - a stalled mailbox or a fault that wedges the
//!   whole actor would show up as the healthy EVSEs' sessions timing out.
//! - **Mailbox backpressure.** `G4.5` made the actor's mailbox bounded
//!   (`crate::sync::DEFAULT_MAILBOX_CAPACITY`, 256) so a glitching producer cannot allocate the
//!   charge point into oblivion. [`mailbox_backpressure_refuses_and_preserves_fifo_order`] floods
//!   it directly (bypassing the network entirely - this is a property of
//!   [`ocpp_charge_point::actor::ChargePointActor`], not of the transport) with more concurrent
//!   producers than it can hold *without ever yielding to the runtime in between*, so the flood
//!   is enqueued deterministically rather than racing the actor's own drain speed. The intended
//!   behaviour under overrun is refusal, not silent drop or reordering (see `crate::sync`'s
//!   mailbox docs) - so this asserts that some sends are refused with `ActorError::MailboxFull`,
//!   that every send that *was* accepted is later applied in exactly the order it was sent (no
//!   reordering, no duplication), and that backpressure is transient: once the actor catches up,
//!   the mailbox accepts new work again rather than staying wedged.
//!
//! Every assertion here is on an invariant (ordering, completeness, refusal-not-loss, eventual
//! convergence within a bounded wait), never on wall-clock timing - the same discipline H4.1's
//! brief states and this file's siblings follow, because a throughput test that flakes on a
//! shared CI runner is a throughput test that gets deleted.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

mod common;

use common::MockCsms;
use ocpp_charge_point::actor::{ActorError, ChargePointActor};
use ocpp_charge_point::executor::TokioExecutor;
use ocpp_charge_point::hardware::{
    Capabilities, ChargePoint, Connector, Evse, HardwareCommandReceiver, HardwareEventSender,
};
use ocpp_charge_point::state::{
    ChargePointEvent, ConnectorEvent, ConnectorState, EvseEvent, IdToken, IdTokenKind, MeterSample,
    StopReason,
};
use ocpp_charge_point::{OcppVersion, connect_and_setup};
use serde_json::Value;

// --- minimal hardware binding, one connector per EVSE, EVSE count configurable at construction --

struct TestChargePoint {
    evses: Vec<TestEvse>,
}
struct TestEvse {
    connectors: [TestConnector; 1],
}
struct TestConnector;

#[async_trait::async_trait]
impl ChargePoint<TestEvse, TestConnector> for TestChargePoint {
    type StartError = core::convert::Infallible;
    async fn vendor_name(&self) -> &str {
        "Acme"
    }
    async fn model_name(&self) -> &str {
        "Charger 9000"
    }
    async fn evses(&self) -> &[TestEvse] {
        &self.evses
    }
    async fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    async fn start(
        &self,
        _events: HardwareEventSender,
        _commands: HardwareCommandReceiver,
    ) -> Result<(), Self::StartError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl Evse<TestConnector> for TestEvse {
    type Error = core::convert::Infallible;
    async fn connectors(&self) -> &[TestConnector] {
        &self.connectors
    }
    async fn reboot(&self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl Connector for TestConnector {
    type Error = core::convert::Infallible;
    async fn lock(&self) -> Result<(), Self::Error> {
        Ok(())
    }
    async fn unlock(&self) -> Result<(), Self::Error> {
        Ok(())
    }
    async fn close_contactor(&self) -> Result<(), Self::Error> {
        Ok(())
    }
    async fn open_contactor(&self) -> Result<(), Self::Error> {
        Ok(())
    }
    async fn set_current_limit(&self, _limit_ma: Option<u32>) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn charge_point(evse_count: usize) -> TestChargePoint {
    TestChargePoint {
        evses: (0..evse_count)
            .map(|_| TestEvse {
                connectors: [TestConnector],
            })
            .collect(),
    }
}

/// A unique id token for `(evse_id, round)`, so no two concurrent sessions ever share one and a
/// mix-up would show up as a transaction crediting the wrong token rather than being masked by
/// reuse.
fn id_token(evse_id: usize, round: usize) -> IdToken {
    IdToken {
        value: format!("EVSE{evse_id:02}-ROUND{round:04}"),
        kind: IdTokenKind::ISO14443,
    }
}

/// Waits (bounded, not forever) until `condition` holds, polling every 5 ms - the same shape
/// every other longevity test here uses: the failure this guards against is *silence*, and an
/// unbounded wait would hang the suite instead of failing it with a usable message.
async fn wait_until(max_wait: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let attempts = (max_wait.as_millis() / 5).max(1) as usize;
    for _ in 0..attempts {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    condition()
}

async fn send_connector_event(
    runtime: &ocpp_charge_point::ChargePointRuntime<TestChargePoint>,
    evse_id: usize,
    event: ConnectorEvent,
) {
    let _ = runtime
        .send(ChargePointEvent::Evse {
            evse_id,
            event: EvseEvent::Connector {
                connector_id: 0,
                event,
            },
        })
        .await;
}

/// Drives one connector through a whole charging session - plug, authorize (a real Authorize
/// round trip; `MockCsms`'s default reply accepts every one), charge, meter, stop, unplug -
/// exactly `tests/lifecycle.rs`'s `drive_a_session`, parameterized over which EVSE.
async fn drive_a_session(
    runtime: &ocpp_charge_point::ChargePointRuntime<TestChargePoint>,
    evse_id: usize,
    round: usize,
) {
    let token = id_token(evse_id, round);
    send_connector_event(runtime, evse_id, ConnectorEvent::CableConnected).await;
    send_connector_event(runtime, evse_id, ConnectorEvent::LockConfirmed).await;
    send_connector_event(
        runtime,
        evse_id,
        ConnectorEvent::IdTokenPresented(token.clone()),
    )
    .await;

    let authorized = wait_until(Duration::from_secs(10), || {
        runtime.state().evses[evse_id].connectors[0] != ConnectorState::Authorizing
    })
    .await
        && runtime.state().evses[evse_id].connectors[0] == ConnectorState::Starting;
    assert!(
        authorized,
        "EVSE {evse_id} round {round}: authorization never reached Accepted - connector state: \
         {:?}",
        runtime.state().evses[evse_id].connectors[0]
    );

    for event in [
        ConnectorEvent::ContactorClosed,
        ConnectorEvent::MeterValueSampled(MeterSample {
            energy_wh: 1_500,
            ..Default::default()
        }),
        ConnectorEvent::MeterValueSampled(MeterSample {
            energy_wh: 3_000,
            ..Default::default()
        }),
        ConnectorEvent::ChargingStopped(StopReason::Local),
        ConnectorEvent::ContactorOpened,
        ConnectorEvent::UnlockConfirmed,
        ConnectorEvent::CableDisconnected,
    ] {
        send_connector_event(runtime, evse_id, event).await;
    }
}

/// Drives `rounds` complete sessions back to back on one EVSE.
async fn drive_rounds(
    runtime: &ocpp_charge_point::ChargePointRuntime<TestChargePoint>,
    evse_id: usize,
    rounds: usize,
) {
    for round in 0..rounds {
        drive_a_session(runtime, evse_id, round).await;
    }
}

/// Drives one EVSE partway into a session (through the first meter sample, so it has an active
/// transaction with real energy already reported) and then forces a hardware fault on it - the
/// per-connector `FaultDetected`, not the EVSE- or charge-point-wide variants, so only this one
/// connector is affected. Returns once the connector has actually reached a faulted state, which
/// is also when the interrupted transaction's forced `Ended` event has been raised.
async fn run_into_a_fault(
    runtime: &ocpp_charge_point::ChargePointRuntime<TestChargePoint>,
    evse_id: usize,
) {
    let token = id_token(evse_id, 0);
    send_connector_event(runtime, evse_id, ConnectorEvent::CableConnected).await;
    send_connector_event(runtime, evse_id, ConnectorEvent::LockConfirmed).await;
    send_connector_event(runtime, evse_id, ConnectorEvent::IdTokenPresented(token)).await;
    wait_until(Duration::from_secs(10), || {
        runtime.state().evses[evse_id].connectors[0] != ConnectorState::Authorizing
    })
    .await;
    send_connector_event(runtime, evse_id, ConnectorEvent::ContactorClosed).await;
    send_connector_event(
        runtime,
        evse_id,
        ConnectorEvent::MeterValueSampled(MeterSample {
            energy_wh: 500,
            ..Default::default()
        }),
    )
    .await;

    send_connector_event(runtime, evse_id, ConnectorEvent::FaultDetected).await;

    let faulted = wait_until(Duration::from_secs(10), || {
        matches!(
            runtime.state().evses[evse_id].connectors[0],
            ConnectorState::Faulted | ConnectorState::FaultedSafe
        )
    })
    .await;
    assert!(
        faulted,
        "EVSE {evse_id}: connector never reached a faulted state after FaultDetected - state: \
         {:?}",
        runtime.state().evses[evse_id].connectors[0]
    );
    // The other connectors on this charge point must not have been touched by a fault addressed
    // at one connector specifically - proof this isn't accidentally the EVSE-wide fault variant.
    assert_eq!(
        runtime.state().evses[evse_id].status,
        ocpp_charge_point::state::EvseStatus::Available,
        "a single connector's FaultDetected should not have faulted the whole EVSE"
    );
}

/// Checks that `events` contain exactly `expected_transactions` distinct transactions, each a
/// complete, gap-free, duplicate-free `seqNo` run from 0, starting with `Started` and ending with
/// `Ended` - the same shape `tests/network_flapping_soak.rs` checks for its own ordering claim,
/// reused here for the concurrent-EVSE case. The fault-isolation call site's one transaction
/// satisfies this too: a hardware fault force-ends the active transaction (`ConnectorEvent`'s
/// docs, "`(_, ConnectorState::Faulted)`" in `charge_point_state.rs`) rather than leaving it
/// dangling, so it is `Started`-then-`Ended` just like a normal session, only shorter.
fn assert_transactions_complete_and_ordered(events: &[Value], expected_transactions: usize) {
    let mut by_transaction: BTreeMap<String, Vec<(i64, String)>> = BTreeMap::new();
    for event in events {
        let transaction_id = event["transactionInfo"]["transactionId"]
            .as_str()
            .expect("every TransactionEvent carries transactionInfo.transactionId")
            .to_string();
        let seq_no = event["seqNo"]
            .as_i64()
            .expect("every TransactionEvent carries seqNo");
        let event_type = event["eventType"]
            .as_str()
            .expect("every TransactionEvent carries eventType")
            .to_string();
        by_transaction
            .entry(transaction_id)
            .or_default()
            .push((seq_no, event_type));
    }

    assert_eq!(
        by_transaction.len(),
        expected_transactions,
        "expected {expected_transactions} distinct transactions to reach the CSMS, saw {} - some \
         transaction's events never arrived, or two EVSEs' events were merged under one id",
        by_transaction.len()
    );

    for (transaction_id, mut events) in by_transaction {
        events.sort_by_key(|(seq_no, _)| *seq_no);
        let seq_nos: Vec<i64> = events.iter().map(|(seq_no, _)| *seq_no).collect();
        let expected: Vec<i64> = (0..seq_nos.len() as i64).collect();
        assert_eq!(
            seq_nos, expected,
            "transaction {transaction_id} delivered seqNo {seq_nos:?}, which is not a gap-free, \
             duplicate-free run from 0 - concurrent EVSEs' traffic was reordered or duplicated \
             somewhere between the actor mailbox and the CSMS"
        );
        assert_eq!(
            events.first().map(|(_, event_type)| event_type.as_str()),
            Some("Started"),
            "transaction {transaction_id} did not start with a Started event"
        );
        assert_eq!(
            events.last().map(|(_, event_type)| event_type.as_str()),
            Some("Ended"),
            "transaction {transaction_id} did not end with an Ended event"
        );
    }
}

/// Every `TransactionEvent` payload whose `evse.id` is `evse_id`.
fn transaction_events_for_evse(calls: &[common::ReceivedCall], evse_id: usize) -> Vec<Value> {
    calls
        .iter()
        .filter(|call| call.action == "TransactionEvent")
        .map(|call| call.payload.clone())
        .filter(|payload| payload["evse"]["id"].as_i64() == Some(evse_id as i64))
        .collect()
}

/// Waits (boundedly) for at least `target` `Ended` events across every EVSE to have reached the
/// CSMS.
async fn wait_for_ended_count(csms: &MockCsms, target: usize) {
    let is_converged = || {
        csms.received()
            .iter()
            .filter(|call| {
                call.action == "TransactionEvent" && call.payload["eventType"] == "Ended"
            })
            .count()
            >= target
    };
    let ended = wait_until(Duration::from_secs(20), is_converged).await;
    assert!(
        ended,
        "not every expected transaction's Ended event reached the CSMS ({target} expected, {} \
         TransactionEvent calls received total) - a stalled EVSE, or the whole actor wedged",
        csms.received()
            .iter()
            .filter(|call| call.action == "TransactionEvent")
            .count()
    );
}

/// How many EVSEs the fast default drives concurrently.
const EVSE_COUNT: usize = 6;
/// Complete sessions driven per healthy EVSE.
const ROUNDS_PER_EVSE: usize = 4;
/// The EVSE deliberately driven into a hardware fault while the others keep transacting.
const FAULTED_EVSE: usize = 0;

/// The fast, deterministic default: several EVSEs transacting concurrently over one actor and one
/// CSMS connection, with one of them deliberately faulted mid-session. Runs on every `cargo test`.
#[tokio::test]
async fn many_evses_transact_concurrently_with_isolation_and_ordering() {
    let csms = MockCsms::start("ocpp2.1", Vec::new()).await;
    let runtime = connect_and_setup(
        charge_point(EVSE_COUNT),
        csms.address(),
        Some(&[OcppVersion::V2_1]),
        None,
        None,
        TokioExecutor,
        ocpp_charge_point::provisioning::TokioBackoff,
    )
    .await
    .expect("the connection should come up");
    csms.wait_for("BootNotification").await;

    // Every healthy EVSE drives its own sequence of complete sessions concurrently with every
    // other EVSE and with the faulted one - all funnelled through the one runtime/mailbox and
    // the one CSMS connection this file's docs describe.
    let healthy_evses: Vec<usize> = (0..EVSE_COUNT).filter(|&id| id != FAULTED_EVSE).collect();
    let healthy_futures = healthy_evses
        .iter()
        .map(|&evse_id| drive_rounds(&runtime, evse_id, ROUNDS_PER_EVSE));
    let fault_future = run_into_a_fault(&runtime, FAULTED_EVSE);

    // Bounded by `wait_for_ended_count` below, not by this join itself - a hang here is caught
    // as a failed wait, not by racing wall-clock time against the test harness.
    futures::future::join(futures::future::join_all(healthy_futures), fault_future).await;

    let expected_ended = healthy_evses.len() * ROUNDS_PER_EVSE + 1;
    wait_for_ended_count(&csms, expected_ended).await;

    let received = csms.received();
    for &evse_id in &healthy_evses {
        assert_transactions_complete_and_ordered(
            &transaction_events_for_evse(&received, evse_id),
            ROUNDS_PER_EVSE,
        );
    }
    // The faulted EVSE's one interrupted transaction still reached the CSMS as a complete,
    // ordered (if short) lifecycle - forced-ended, not dropped.
    assert_transactions_complete_and_ordered(
        &transaction_events_for_evse(&received, FAULTED_EVSE),
        1,
    );
}

/// The opt-in long form: the same harness, more EVSEs and more rounds each, for whoever wants to
/// lean on it before a release:
///
/// ```text
/// cargo test --test multi_evse_throughput -- --ignored
/// OCPP_THROUGHPUT_EVSES=20 OCPP_THROUGHPUT_ROUNDS=200 cargo test --test multi_evse_throughput -- --ignored
/// ```
#[tokio::test]
#[ignore = "opt-in long throughput soak; run explicitly with `cargo test -- --ignored`, see module docs"]
async fn long_sustained_multi_evse_throughput() {
    let evse_count: usize = std::env::var("OCPP_THROUGHPUT_EVSES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(20);
    let rounds: usize = std::env::var("OCPP_THROUGHPUT_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(50);
    assert!(
        evse_count >= 2,
        "need at least one healthy EVSE and one to fault"
    );
    let faulted_evse = 0;

    let csms = MockCsms::start("ocpp2.1", Vec::new()).await;
    let runtime = connect_and_setup(
        charge_point(evse_count),
        csms.address(),
        Some(&[OcppVersion::V2_1]),
        None,
        None,
        TokioExecutor,
        ocpp_charge_point::provisioning::TokioBackoff,
    )
    .await
    .expect("the connection should come up");
    csms.wait_for("BootNotification").await;

    let healthy_evses: Vec<usize> = (0..evse_count).filter(|&id| id != faulted_evse).collect();
    let healthy_futures = healthy_evses
        .iter()
        .map(|&evse_id| drive_rounds(&runtime, evse_id, rounds));
    let fault_future = run_into_a_fault(&runtime, faulted_evse);

    futures::future::join(futures::future::join_all(healthy_futures), fault_future).await;

    let expected_ended = healthy_evses.len() * rounds + 1;
    wait_for_ended_count(&csms, expected_ended).await;

    let received = csms.received();
    for &evse_id in &healthy_evses {
        assert_transactions_complete_and_ordered(
            &transaction_events_for_evse(&received, evse_id),
            rounds,
        );
    }
    assert_transactions_complete_and_ordered(
        &transaction_events_for_evse(&received, faulted_evse),
        1,
    );

    println!(
        "long multi-EVSE throughput: {evse_count} EVSEs ({} healthy x {rounds} rounds + 1 \
         faulted), {expected_ended} Ended events, all ordered and complete",
        healthy_evses.len()
    );
}

// --- mailbox backpressure: a property of `ChargePointActor` itself, not of the transport --------

/// How many un-drained events the mailbox holds before refusing more (`crate::sync`'s
/// `DEFAULT_MAILBOX_CAPACITY`, G4.5). Not importable (it's `pub(crate)`), so duplicated here as a
/// documented constant this test's own assertions are built around - if the crate ever changes
/// the real value without updating this one, the flood below (`MAILBOX_CAPACITY + 64`) still
/// proves the qualitative invariants (some refusals, FIFO order, recovery); only the exact
/// enqueued/refused split would then no longer match `MAILBOX_CAPACITY` precisely, which is why
/// that split is asserted as "some of each", not as an exact count against this constant.
const MAILBOX_CAPACITY_HINT: usize = 256;

/// Floods a freshly spawned actor's mailbox with more concurrent producers than it can hold,
/// **without ever yielding to the Tokio runtime while doing so** - each future is polled exactly
/// once by hand, with a no-op waker, which runs it synchronously up to its first real suspension
/// point and no further. Since the actor's own run loop is a separate Tokio task that only gets a
/// chance to execute when this task yields control back to the runtime, none of it runs while the
/// flood is being enqueued - so the flood lands deterministically against a mailbox whose
/// occupancy this test fully controls, rather than racing the actor's own drain speed (which
/// would make "did it overflow" depend on scheduler timing, exactly what this file's docs say to
/// avoid).
#[tokio::test]
async fn mailbox_backpressure_refuses_and_preserves_fifo_order() {
    let actor = ChargePointActor::spawn([1], &TokioExecutor);

    // Get the connector into `Charging` first (each step awaited normally - the actor drains
    // between these, which is fine, they aren't part of the flood) so the flood below can use
    // `MeterValueSampled`, whose effect (`TransactionEvent::Updated`, `seqNo` incremented by
    // exactly one each time - see `apply_meter_sample`) gives a cheap, exact way to observe
    // delivery order later: the sequence of `energy_wh` values recorded is a direct readout of
    // the order the mailbox actually applied things in.
    let mut transaction_events = actor.subscribe_transaction_events();
    let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let collector = tokio::spawn({
        let observed = observed.clone();
        async move {
            while let Ok(occurred) = transaction_events.recv().await {
                observed.lock().unwrap().push(occurred);
            }
        }
    });

    for event in [
        ConnectorEvent::CableConnected,
        ConnectorEvent::LockConfirmed,
        ConnectorEvent::IdTokenPresented(IdToken {
            value: "FLOODTOKEN".into(),
            kind: IdTokenKind::ISO14443,
        }),
        ConnectorEvent::ChargingAuthorized(IdToken {
            value: "FLOODTOKEN".into(),
            kind: IdTokenKind::ISO14443,
        }),
        ConnectorEvent::ContactorClosed,
    ] {
        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event,
                },
            })
            .await
            .expect("setup events should not overflow an idle mailbox");
    }

    // Named alias for a boxed, pinned `actor.send(..)` future - the type itself is unremarkable,
    // but spelled out inline it trips clippy's complexity lint at every use site below.
    type SendFuture = Pin<Box<dyn Future<Output = Result<(), ActorError>>>>;

    const FLOOD: usize = MAILBOX_CAPACITY_HINT + 64;
    let mut futures: Vec<SendFuture> = (0..FLOOD)
        .map(|index| {
            let actor = actor.clone();
            let event = ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::MeterValueSampled(MeterSample {
                        // Distinct and monotonic, so the order they're later observed in is
                        // unambiguous - a reorder or duplicate would show up directly as a
                        // non-monotonic or repeated value.
                        energy_wh: 10_000 + index as i64,
                        ..Default::default()
                    }),
                },
            };
            Box::pin(async move { actor.send(event).await }) as SendFuture
        })
        .collect();

    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut enqueued: Vec<(usize, SendFuture)> = Vec::new();
    let mut refused = 0usize;
    for (index, mut future) in futures.drain(..).enumerate() {
        match future.as_mut().poll(&mut context) {
            Poll::Pending => enqueued.push((index, future)),
            Poll::Ready(Ok(())) => panic!(
                "index {index} completed synchronously - the actor's run loop must not have run \
                 while this test was still enqueueing the flood, or this test's premise (fully \
                 controlled mailbox occupancy) doesn't hold"
            ),
            Poll::Ready(Err(ActorError::MailboxFull)) => refused += 1,
            Poll::Ready(Err(ActorError::Stopped)) => panic!("actor stopped unexpectedly"),
        }
    }

    // The whole point: overrun refuses some sends outright rather than silently dropping or
    // reordering them (G4.5) - so with a flood larger than the mailbox, at least one send must
    // have been enqueued and at least one refused.
    assert!(
        !enqueued.is_empty(),
        "expected at least one send to be accepted into the mailbox"
    );
    assert!(
        refused > 0,
        "expected the flood ({FLOOD} sends) to exceed mailbox capacity and refuse at least one - \
         got 0 refusals, meaning either the mailbox is no longer bounded or this flood is too \
         small for the real capacity"
    );
    assert_eq!(
        enqueued.len() + refused,
        FLOOD,
        "every one of the flood's sends should resolve to exactly one outcome"
    );

    // Now let the actor actually drain - awaiting each previously-enqueued future normally lets
    // the Tokio runtime run the actor's task in between.
    for (_, future) in enqueued.iter_mut() {
        future
            .await
            .expect("a send the mailbox already accepted must eventually be applied");
    }

    // Backpressure is transient, not a latch (`crate::sync`'s own unit test makes the same claim
    // at the primitive level): now that the flood has drained, the mailbox accepts new work
    // again rather than staying wedged.
    actor
        .send(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::Connector {
                connector_id: 0,
                event: ConnectorEvent::MeterValueSampled(MeterSample {
                    energy_wh: 999_999,
                    ..Default::default()
                }),
            },
        })
        .await
        .expect("the mailbox must recover once its backlog has drained, not stay wedged");

    // Let the collector task catch up with everything the drain above just produced before
    // reading its output.
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    collector.abort();

    // FIFO order, no reordering and no duplication: every `Updated` event this drove, filtered to
    // the flood's own energy range, must appear in exactly the order the accepted sends were
    // issued in - a reorder or a drop would show up directly as a mismatch here.
    let expected_energy: Vec<i64> = enqueued
        .iter()
        .map(|(index, _)| 10_000 + *index as i64)
        .collect();
    let observed_flood_energy: Vec<i64> = observed
        .lock()
        .unwrap()
        .iter()
        .filter_map(|occurred| occurred.transaction.last_meter_sample)
        .map(|sample| sample.energy_wh)
        .filter(|energy| (10_000..10_000 + FLOOD as i64).contains(energy))
        .collect();
    assert_eq!(
        observed_flood_energy, expected_energy,
        "the mailbox applied the accepted sends out of order, dropped one, or duplicated one - \
         G4.5's ordering guarantee does not hold under this flood"
    );
}
