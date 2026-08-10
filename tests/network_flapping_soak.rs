//! Soak test with induced network flapping (`docs/PRODUCTION-ROADMAP.md` H4.1).
//!
//! # Reading the roadmap row
//!
//! "Multi-day soak with induced network flapping" cannot literally run in CI - a test that takes
//! days is a test nobody runs, which is worth nothing. What is actually being asked for is that a
//! charge point survive *many* disconnect/reconnect cycles, mid-transaction, without losing or
//! reordering anything and without leaking memory - and that claim doesn't need wall-clock days to
//! check, only enough cycles. So this file has two modes:
//!
//! - [`many_reconnects_preserve_ordering_and_bounded_delivery`] runs by default, drives a few dozen
//!   forced reconnects in well under a second of wall-clock reconnect delay, and asserts the
//!   invariants that must hold regardless of cycle count - correctness properties, not timings, so
//!   it can gate CI without becoming the flaky test that gets deleted.
//! - [`long_soak_survives_hundreds_of_reconnects_without_leaking`] is `#[ignore]`d and runs the same
//!   harness for many more rounds (default 100, override with the `OCPP_SOAK_ROUNDS` env var), for
//!   whoever wants to lean on it before a release: `cargo test --test network_flapping_soak --
//!   --ignored`, or `OCPP_SOAK_ROUNDS=2000 cargo test --test network_flapping_soak -- --ignored`
//!   for a genuinely long run. It additionally checks that retained heap memory does not trend
//!   upward across rounds, the same shape of claim `tests/memory_growth.rs` (H4.2) makes for
//!   transaction count, applied here to connectivity churn instead.
//!
//! # What is actually stressed
//!
//! [`FlappingCsms`] is a real WebSocket server (not `tests/common`'s `MockCsms`, which accepts
//! exactly one connection - flapping needs many) that force-closes the connection after a small,
//! fixed number of CALLs, then accepts the next one. That is deterministic and independent of wall
//! clock, so drops land at different points in the message stream from run to run without ever
//! depending on timing, and land squarely mid-transaction because a transaction's own event
//! sequence produces more than one CALL. `connect_and_setup` is driven with real `ocpp-client`
//! reconnect (A5's exponential backoff runs for real, just with tiny delays so the test stays
//! fast) and this crate's own [`ocpp_charge_point::network_switch`]/[`ocpp_charge_point::connection`]
//! machinery re-registers (a fresh BootNotification) on every reconnect exactly as it would against
//! a real CSMS that forgot everything about this session.
//!
//! # Why rounds, not one long batch
//!
//! `setup()` (which `connect_and_setup` uses for OCPP 2.1) wires each report queue with
//! `crate::offline_queue`'s default bound (100 messages) - deliberately: an unbounded backlog on a
//! charge point that stays offline for good is a slow-motion out-of-memory crash, and G2.2 fixed
//! exactly that. Driving thousands of transactions in one uninterrupted burst against a link this
//! aggressively flaky would legitimately overrun that bound and evict old reports, which is the
//! bound working as designed, not a bug this test should chase. So instead of one giant batch, the
//! harness drives fixed-size **rounds** and waits for each round's backlog to fully drain (queue
//! back to empty) before starting the next. That keeps the outstanding backlog inside the default
//! bound at all times - so nothing is ever evicted and every invariant below can be asserted as an
//! exact equality - while still letting the *number of rounds* run arbitrarily high to accumulate
//! reconnects and, in the long form, watch for a leak.
//!
//! # What this catches that nothing else does
//!
//! `tests/network_profile_switch.rs` proves a *single* switch works. `tests/power_cut_recovery.rs`
//! proves durability across a *power* cut, not a live connection dropping while the process keeps
//! running. Neither runs enough reconnect cycles to show a leak or a reordering bug that only
//! appears on round 200. This is the one place that does.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use ocpp_charge_point::connect_and_setup;
use ocpp_charge_point::executor::TokioExecutor;
use ocpp_charge_point::hardware::{
    Capabilities, ChargePoint, Connector, Evse, HardwareCommandReceiver, HardwareEventSender,
};
use ocpp_charge_point::provisioning::TokioBackoff;
use ocpp_charge_point::state::{
    AuthorizationStatus, ChargePointEvent, ConnectorEvent, ConnectorState, EvseEvent, IdToken,
    IdTokenKind, MeterSample, StopReason,
};
use ocpp_client::{ConnectOptions, KeepaliveBehavior, ReconnectBehavior, ReconnectPolicy};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

/// Live (allocated minus freed) bytes requested through [`Counting`] - used only by the opt-in
/// long soak (see [`long_soak_survives_hundreds_of_reconnects_without_leaking`]) to check that
/// retained memory does not trend upward across hundreds of reconnect cycles, the same shape of
/// claim `tests/memory_growth.rs` (H4.2) makes for transaction count. Harmless overhead for the
/// default fast test, which does not read it.
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// A [`GlobalAlloc`] that forwards to the system allocator while tracking live requested bytes -
/// identical in shape to `tests/memory_growth.rs`'s `Counting`, duplicated rather than shared
/// because each measuring binary needs its own `#[global_allocator]` and its own static counter.
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The current live-byte reading.
fn live() -> usize {
    LIVE.load(Ordering::Relaxed)
}

// --- minimal hardware binding, identical in shape to every other socket-backed test here -------

struct TestChargePoint {
    evses: [TestEvse; 1],
}
struct TestEvse {
    connectors: [TestConnector; 1],
}
struct TestConnector;

#[async_trait::async_trait]
impl ChargePoint<TestEvse, TestConnector> for TestChargePoint {
    type StartError = core::convert::Infallible;
    fn vendor_name(&self) -> &str {
        "Acme"
    }
    fn model_name(&self) -> &str {
        "Charger 9000"
    }
    fn evses(&self) -> &[TestEvse] {
        &self.evses
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    async fn start(
        self: Arc<Self>,
        _events: HardwareEventSender,
        _commands: HardwareCommandReceiver,
    ) -> Result<(), Self::StartError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl Evse<TestConnector> for TestEvse {
    type Error = core::convert::Infallible;
    fn connectors(&self) -> &[TestConnector] {
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

fn charge_point() -> TestChargePoint {
    TestChargePoint {
        evses: [TestEvse {
            connectors: [TestConnector],
        }],
    }
}

fn id_token() -> IdToken {
    IdToken {
        value: "04A224B2".into(),
        kind: IdTokenKind::ISO14443,
    }
}

// --- the flapping mock CSMS -----------------------------------------------------------------

/// One CALL the charge point sent, tagged with which connection it arrived on so a test can tell
/// "before a flap" traffic from "after" traffic if it needs to.
#[derive(Debug, Clone)]
struct ReceivedCall {
    // Not read directly - kept for `{:?}` debug output if a test assertion ever needs to show
    // which connection a call arrived on.
    #[allow(dead_code)]
    connection: usize,
    action: String,
    payload: Value,
}

/// A real WebSocket server that accepts a connection, answers every CALL with a valid default
/// reply, and force-closes the socket after `calls_before_drop` CALLs - then accepts the next one
/// and repeats, for as long as the charge point keeps redialling.
///
/// Closing on a *count* rather than a timer is the whole point: it reproduces "flap mid-message"
/// deterministically, with no dependence on wall-clock scheduling, so the test cannot become
/// flaky by running on a slower machine.
struct FlappingCsms {
    address: String,
    received: Arc<Mutex<Vec<ReceivedCall>>>,
    connections: Arc<AtomicUsize>,
    /// Total `BootNotification` CALLs ever received, tracked independently of `received` so
    /// [`Self::drain`] (which the long soak uses to keep its own bookkeeping from growing
    /// unboundedly with round count - see its docs) never loses this cumulative count.
    boots: Arc<AtomicUsize>,
    /// The currently-connected socket's outbound control half, if any - swapped in by whichever
    /// per-connection task is live, so [`Self::send_call`] always reaches the *current*
    /// connection without the caller needing to know when the last flap happened.
    current_sink: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<Message>>>>,
}

impl FlappingCsms {
    async fn start(calls_before_drop: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("ws://{}", listener.local_addr().unwrap());
        let received = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let boots = Arc::new(AtomicUsize::new(0));
        let current_sink = Arc::new(Mutex::new(None));

        let recorded = received.clone();
        let counter = connections.clone();
        let boot_counter = boots.clone();
        let shared_sink = current_sink.clone();
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    break;
                };
                let connection_id = counter.fetch_add(1, Ordering::SeqCst);
                let recorded = recorded.clone();
                let boot_counter = boot_counter.clone();
                let shared_sink = shared_sink.clone();
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(
                        tcp,
                        #[allow(clippy::result_large_err)]
                        |_req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                         mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                            response
                                .headers_mut()
                                .insert("Sec-WebSocket-Protocol", "ocpp2.1".parse().unwrap());
                            Ok(response)
                        },
                    )
                    .await
                    else {
                        return;
                    };

                    let (outbound_tx, mut outbound_rx) =
                        tokio::sync::mpsc::unbounded_channel::<Message>();
                    *shared_sink.lock().unwrap() = Some(outbound_tx);

                    let mut calls_this_connection = 0usize;
                    loop {
                        tokio::select! {
                            // A CSMS-initiated CALL a test wants to push at the charge point over
                            // whichever connection is current - see `Self::send_call`.
                            Some(message) = outbound_rx.recv() => {
                                if ws.send(message).await.is_err() {
                                    break;
                                }
                            }
                            frame = ws.next() => {
                                let frame = match frame {
                                    Some(Ok(Message::Text(text))) => text.to_string(),
                                    // Anything else (ping/pong/close/error/stream end) is not a
                                    // CALL; a real connection can carry those too, and none of
                                    // them count toward the drop threshold.
                                    Some(Ok(_)) => continue,
                                    _ => break,
                                };
                                let Ok(message) = serde_json::from_str::<Value>(&frame) else {
                                    continue;
                                };
                                if message[0] != 2 {
                                    // A CALLRESULT/CALLERROR answering something this mock sent -
                                    // recorded implicitly by not needing a reply.
                                    continue;
                                }
                                let action = message[2].as_str().unwrap_or_default().to_string();
                                let id = message[1].as_str().unwrap_or_default().to_string();
                                if action == "BootNotification" {
                                    boot_counter.fetch_add(1, Ordering::SeqCst);
                                }
                                recorded.lock().unwrap().push(ReceivedCall {
                                    connection: connection_id,
                                    action: action.clone(),
                                    payload: message[3].clone(),
                                });
                                let reply = default_reply(&action);
                                if ws
                                    .send(Message::text(json!([3, id, reply]).to_string()))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                                calls_this_connection += 1;
                                if calls_this_connection >= calls_before_drop {
                                    // The forced flap: drop the socket. `ocpp-client`'s reconnect
                                    // policy (installed by `connect_and_setup`) redials, and the
                                    // loop above accepts the new connection like any other.
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });

        Self {
            address,
            received,
            connections,
            boots,
            current_sink,
        }
    }

    fn actions(&self) -> Vec<ReceivedCall> {
        self.received.lock().unwrap().clone()
    }

    /// Takes and clears every call recorded so far, returning what was there. Used by the long
    /// soak to check each round's traffic and then let it go, rather than retaining full history
    /// (payloads and all) for the whole run - see [`long_soak_survives_hundreds_of_reconnects_without_leaking`]'s
    /// docs for why that retention would itself look like a leak to the very check meant to catch
    /// one. [`Self::connection_count`]/[`Self::boot_count`] are unaffected - they read independent
    /// counters, not this log.
    fn drain(&self) -> Vec<ReceivedCall> {
        core::mem::take(&mut self.received.lock().unwrap())
    }

    /// How many separate TCP connections have been accepted so far - the number of times the
    /// charge point has (re)dialled in, including the first.
    fn connection_count(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// Pushes a CSMS-initiated CALL at the charge point over whichever connection is current.
    /// Fire-and-forget - the reply is a CALLRESULT the receive loop above already ignores.
    fn send_call(&self, id: &str, action: &str, payload: Value) {
        if let Some(sink) = self.current_sink.lock().unwrap().as_ref() {
            let _ = sink.send(Message::text(json!([2, id, action, payload]).to_string()));
        }
    }

    /// Total `BootNotification` CALLs ever received, unaffected by [`Self::drain`].
    fn boot_count(&self) -> usize {
        self.boots.load(Ordering::SeqCst)
    }

    /// Every `TransactionEvent` payload received since the last [`Self::drain`], in the order the
    /// CSMS saw them.
    fn transaction_events(&self) -> Vec<Value> {
        self.actions()
            .into_iter()
            .filter(|call| call.action == "TransactionEvent")
            .map(|call| call.payload)
            .collect()
    }
}

fn transaction_events_of(calls: &[ReceivedCall]) -> Vec<Value> {
    calls
        .iter()
        .filter(|call| call.action == "TransactionEvent")
        .map(|call| call.payload.clone())
        .collect()
}

fn default_reply(action: &str) -> Value {
    match action {
        "BootNotification" => json!({
            "currentTime": "2026-01-01T00:00:00Z",
            "interval": 300,
            "status": "Accepted"
        }),
        "Heartbeat" => json!({ "currentTime": "2026-01-01T00:00:00Z" }),
        "Authorize" => json!({ "idTokenInfo": { "status": "Accepted" } }),
        _ => json!({}),
    }
}

/// A `ConnectOptions` that reconnects fast (single-digit milliseconds) and without extra jitter,
/// so a test can force many cycles without paying `ocpp-client`'s production-sized backoff
/// (seconds, by design - see `connect.rs`'s `connection_options_from_device_model`). Real
/// exponential backoff still runs - `ReconnectPolicy` is exercised for real, just scaled down -
/// which is what A5 asks a soak test to actually stress rather than merely reference.
fn fast_reconnect_options() -> ConnectOptions<'static> {
    ConnectOptions {
        // Short on purpose: a CALL sent just as a forced flap closes the socket must fail fast and
        // get requeued rather than sit out a production-sized `MessageTimeout` before the offline
        // queue can move on - see the module docs on why this test scales the whole reconnect
        // curve down rather than only the delay between attempts.
        timeout: Some(Duration::from_millis(200)),
        keepalive: KeepaliveBehavior::Disabled,
        reconnect: ReconnectBehavior::Enabled(ReconnectPolicy {
            initial_delay: Duration::from_millis(2),
            max_delay: Duration::from_millis(20),
            multiplier: 2,
            jitter: false,
        }),
        ..Default::default()
    }
}

/// Drives one connector through a whole charging session, exactly `tests/lifecycle.rs`'s
/// `drive_a_session` - plug, authorize, charge, meter, stop, unplug - the sequence whose CALLs
/// (StatusNotification/TransactionEvent) are what the flapping CSMS actually receives.
async fn send_connector_event(
    runtime: &ocpp_charge_point::ChargePointRuntime<TestChargePoint>,
    event: ConnectorEvent,
) {
    let _ = runtime
        .send(ChargePointEvent::Evse {
            evse_id: 0,
            event: EvseEvent::Connector {
                connector_id: 0,
                event,
            },
        })
        .await;
}

/// Drives one connector through a whole charging session, up to and including presenting an id
/// token - deliberately **not** also sending `ChargingAuthorized` manually the way
/// `tests/lifecycle.rs`'s otherwise-identical `drive_a_session` does. `setup()` (via
/// `connect_and_setup`) registers a real [`crate::authorization::Authorizer`], which reacts to
/// `IdTokenPresented` on its own by making an actual Authorize round trip and feeding the
/// decision back as `ChargingAuthorized`/`AuthorizationDenied` - manually sending
/// `ChargingAuthorized` too would race that background decision, which is invisible on the
/// stable, single-shot connection `lifecycle.rs` runs against but a real hazard here, where that
/// Authorize call is itself subject to the same forced flapping as everything else. Letting the
/// real path run is also more representative of what this soak test is meant to stress: an
/// Authorize call that has to survive the same disconnects a report does.
async fn present_id_token_and_wait_for_authorization(
    runtime: &ocpp_charge_point::ChargePointRuntime<TestChargePoint>,
) -> bool {
    send_connector_event(runtime, ConnectorEvent::CableConnected).await;
    send_connector_event(runtime, ConnectorEvent::LockConfirmed).await;
    send_connector_event(runtime, ConnectorEvent::IdTokenPresented(id_token())).await;

    wait_until(Duration::from_secs(10), || {
        runtime.state().evses[0].connectors[0] != ConnectorState::Authorizing
    })
    .await;
    runtime.state().evses[0].connectors[0] == ConnectorState::Starting
}

/// Finishes a session already authorized by [`present_id_token_and_wait_for_authorization`]:
/// charge, meter, stop, unplug.
async fn finish_session(runtime: &ocpp_charge_point::ChargePointRuntime<TestChargePoint>) {
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
        send_connector_event(runtime, event).await;
    }
}

/// Drives one complete, successful charging session end to end. The id token is pre-cached (see
/// [`establish`]) as `Accepted`, so - online CSMS traffic or offline cache fallback alike -
/// authorization always succeeds; a session that somehow didn't would be a real bug worth this
/// test failing loudly on, so this asserts it rather than silently skipping the rest of the
/// sequence.
async fn drive_a_session(runtime: &ocpp_charge_point::ChargePointRuntime<TestChargePoint>) {
    let authorized = present_id_token_and_wait_for_authorization(runtime).await;
    assert!(
        authorized,
        "authorization never reached Accepted for a pre-cached id token - connector state: {:?}",
        runtime.state().evses[0].connectors[0]
    );
    finish_session(runtime).await;
}

/// Waits (bounded, not forever) until `condition` holds, polling every 5 ms. Bounded for the
/// reason `tests/common/mod.rs`'s `wait_for` gives: the failure this guards against is *silence*,
/// and an unbounded wait would hang the suite instead of failing it with a usable message.
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

/// What one soak run produced, for a test to assert on. Deliberately holds only cheap running
/// counters, never the full per-call history - see [`FlappingCsms::drain`]'s docs for why keeping
/// that around for the whole run would itself look like a leak to the long soak's own memory
/// check.
struct SoakReport {
    connections: usize,
    boots: usize,
}

/// Dials the flapping CSMS and waits for the initial BootNotification, so a caller can start
/// driving cycles against an already-registered charge point.
async fn establish(
    calls_before_drop: usize,
) -> (
    FlappingCsms,
    ocpp_charge_point::ChargePointRuntime<TestChargePoint>,
) {
    let csms = FlappingCsms::start(calls_before_drop).await;

    let runtime = connect_and_setup(
        charge_point(),
        &csms.address,
        Some(&[ocpp_charge_point::OcppVersion::V2_1]),
        Some(fast_reconnect_options()),
        None,
        TokioExecutor,
        TokioBackoff,
    )
    .await
    .expect("the initial connection should come up");

    assert!(
        wait_until(Duration::from_secs(5), || csms.boot_count() >= 1).await,
        "the charge point never sent an initial BootNotification"
    );

    // `connect_and_setup` only returns once every functional block (including `SetVariables`'s
    // handler) is registered, so it is safe to send this now. Shrinks
    // `OCPPCommCtrlr`/`MessageAttemptInterval[TransactionEvent]` (see
    // `crate::offline_queue::run_offline_queue_retries`) from its production default (60 s) down
    // to something a test can wait out. This is exactly the knob a real CSMS would turn to get a
    // charge point retrying its backlog aggressively on a link it knows is flaky - not a
    // test-only shortcut - and it is what makes "eventual" convergence bounded rather than
    // dependent on every last message happening to ride out a reconnect.
    csms.send_call(
        "shrink-retry-interval",
        "SetVariables",
        json!({
            "setVariableData": [{
                "attributeType": "Actual",
                "attributeValue": "1",
                "component": { "name": "OCPPCommCtrlr" },
                "variable": {
                    "name": "MessageAttemptInterval",
                    "instance": "TransactionEvent"
                }
            }]
        }),
    );
    // Give the charge point a moment to apply it before any cycles start.
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    // Pre-cache the id token every round reuses as `Accepted`, exactly as
    // `tests/memory_growth.rs`'s harness does. Every `IdTokenPresented` in this file triggers a
    // *real* Authorize round trip (see `drive_a_session`'s docs) rather than a manually-synthesized
    // decision, and that round trip is itself subject to the same forced flapping as everything
    // else; without a cache entry, a transport failure right when the token is uncached would
    // legitimately deny it (`crate::authorization::offline_decision`'s documented policy). Pre-
    // caching means every subsequent decision - online or offline fallback alike - agrees, so
    // authorization succeeding is the reliable baseline this file's invariants assume rather than a
    // race this test would otherwise have to tolerate.
    let _ = runtime
        .send(ChargePointEvent::AuthorizationCached {
            id_token: id_token(),
            status: AuthorizationStatus::Accepted,
            cached_at: None,
        })
        .await;

    (csms, runtime)
}

/// Drives `count` full transactions back-to-back, yielding between each so the reconnect and
/// offline-queue machinery gets scheduling opportunities throughout rather than only afterward.
async fn drive_cycles(
    runtime: &ocpp_charge_point::ChargePointRuntime<TestChargePoint>,
    count: usize,
) {
    for _ in 0..count {
        drive_a_session(runtime).await;
        tokio::task::yield_now().await;
    }
}

/// Waits (boundedly) for at least `target` transactions to have reached `Ended` at the CSMS -
/// the "eventual convergence" property: however many times the link flapped in between, the
/// offline queue must drain the backlog once the connection is up again. Bounded at 20 s: on a
/// working charge point this converges in well under a second even at the tightest drop
/// threshold this file uses, so a real deadlock (the failure mode worth catching) still fails the
/// test promptly rather than hanging the suite.
async fn wait_for_ended(csms: &FlappingCsms, target: usize) {
    let is_converged = || {
        csms.transaction_events()
            .iter()
            .filter(|event| event["eventType"] == "Ended")
            .count()
            >= target
    };

    // A small tail backlog (fewer messages left than `calls_before_drop`) may need one more
    // natural reconnect (or `crate::offline_queue`'s periodic sweep, shrunk by `establish`) to
    // drain; either can take a moment longer than the steady-state rate, so this polls rather
    // than expecting instant convergence.
    let ended = wait_until(Duration::from_secs(20), is_converged).await;
    assert!(
        ended,
        "not every transaction's Ended event reached the CSMS after {target} expected \
         ({} connections, {} TransactionEvent calls received) - the offline queue never drained \
         the backlog. event types: {:?}",
        csms.connection_count(),
        csms.transaction_events().len(),
        csms.transaction_events()
            .iter()
            .map(|e| (
                e["eventType"].as_str().unwrap_or("?").to_string(),
                e["seqNo"].clone(),
                e["transactionInfo"]["transactionId"].clone()
            ))
            .collect::<Vec<_>>()
    );
}

/// Checks that `events` (typically one round's `TransactionEvent` payloads) contain exactly
/// `expected_transactions` distinct transactions, each a complete, gap-free, duplicate-free
/// `seqNo` run from 0 that starts with `Started` and ends with `Ended` - what would catch a
/// reorder or a duplicate-delivery bug introduced by the offline queue flushing across a flap.
fn assert_transactions_complete_and_ordered(events: &[Value], expected_transactions: usize) {
    use std::collections::BTreeMap;
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
         transaction's events never arrived at all",
        by_transaction.len()
    );

    for (transaction_id, mut events) in by_transaction {
        events.sort_by_key(|(seq_no, _)| *seq_no);
        let seq_nos: Vec<i64> = events.iter().map(|(seq_no, _)| *seq_no).collect();
        let expected: Vec<i64> = (0..seq_nos.len() as i64).collect();
        assert_eq!(
            seq_nos, expected,
            "transaction {transaction_id} delivered seqNo {seq_nos:?}, which is not a gap-free, \
             duplicate-free run from 0 - a flap either dropped or replayed an event"
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

/// Runs `rounds` batches of `txs_per_round` full transactions each over a connection
/// force-dropped every `calls_before_drop` CALLs, waiting for each round's backlog to fully drain
/// before starting the next (see the module docs, "Why rounds, not one long batch") - so the
/// outstanding backlog never approaches the offline queue's default bound regardless of how many
/// rounds run, and every round after the first still finds the connection already flapping.
///
/// Each round's `TransactionEvent` traffic is checked with
/// [`assert_transactions_complete_and_ordered`] and then [`FlappingCsms::drain`]ed immediately, so
/// this - like [`long_soak_survives_hundreds_of_reconnects_without_leaking`]'s own loop - never
/// retains more than one round's worth of history no matter how many rounds run.
async fn run_rounds(rounds: usize, txs_per_round: usize, calls_before_drop: usize) -> SoakReport {
    let (csms, runtime) = establish(calls_before_drop).await;

    for _ in 0..rounds {
        drive_cycles(&runtime, txs_per_round).await;
        wait_for_ended(&csms, txs_per_round).await;
        let calls = csms.drain();
        assert_transactions_complete_and_ordered(&transaction_events_of(&calls), txs_per_round);
    }

    SoakReport {
        connections: csms.connection_count(),
        boots: csms.boot_count(),
    }
}

/// Asserts the invariants that must hold regardless of how many rounds ran: enough separate
/// connections to prove real flapping happened, and essentially every reconnect resynchronizing
/// with a fresh BootNotification (per-round transaction completeness/ordering is
/// [`assert_transactions_complete_and_ordered`], already checked as each round drains).
fn assert_soak_invariants(report: &SoakReport, min_connections: usize) {
    assert!(
        report.connections >= min_connections,
        "expected at least {min_connections} separate connections (i.e. real flapping), got only \
         {} - the drop threshold is too high for this many cycles to exercise reconnection at all",
        report.connections
    );

    // G's resync-on-reconnect property: essentially every connection - the first dial and every
    // redial after a forced drop - re-registers with its own BootNotification. Not *exactly*
    // every one: reconnecting this aggressively can occasionally race a redial that was already
    // in flight against the forced close that triggered it (the same class of race
    // `tests/network_profile_switch.rs` documents for a profile switch - a connection is
    // sometimes torn down again before it gets to send anything, and the redial after *that*
    // succeeds), so a handful of boot-less connections is tolerated. What must never happen is a
    // reconnect that leaves the charge point silently unregistered for good.
    let silent_connections = report.connections.saturating_sub(report.boots);
    assert!(
        report.boots <= report.connections
            && silent_connections <= (report.connections / 10).max(3),
        "expected close to one BootNotification per connection ({}), got {} ({silent_connections} \
         silent) - too many reconnects left the charge point unregistered",
        report.connections,
        report.boots
    );
}

/// Transactions driven per round throughout this file - small enough that a round's worth of
/// traffic (this many transactions' StatusNotification/TransactionEvent CALLs) stays well inside
/// the offline queue's default 100-message bound even before anything drains, so eviction never
/// enters into what either test measures. See the module docs, "Why rounds, not one long batch".
const TXS_PER_ROUND: usize = 5;

/// How many consecutive CALLs the flapping CSMS allows before force-closing the connection. Small
/// enough that a single round's ~20-30 CALLs span several forced drops, landing squarely
/// mid-transaction rather than only between sessions.
const CALLS_BEFORE_DROP: usize = 3;

/// The fast, deterministic default: enough forced flaps across enough rounds to prove
/// reconnection, ordering and bounded delivery all hold, in well under a second of actual
/// reconnect wait time. Runs on every `cargo test`.
#[tokio::test]
async fn many_reconnects_preserve_ordering_and_bounded_delivery() {
    const ROUNDS: usize = 6;

    let report = run_rounds(ROUNDS, TXS_PER_ROUND, CALLS_BEFORE_DROP).await;
    assert_soak_invariants(&report, 10);
}

/// The opt-in long form: the same harness, many more rounds, plus a leak check across them. Not
/// run by `cargo test` - a soak whose failure mode is "occasionally too slow for CI" is exactly
/// the kind of test that gets deleted the first time it is a nuisance, so this stays manual:
///
/// ```text
/// cargo test --test network_flapping_soak -- --ignored
/// OCPP_SOAK_ROUNDS=2000 cargo test --test network_flapping_soak -- --ignored
/// ```
///
/// Each round drives [`TXS_PER_ROUND`] transactions, checks and drains its own traffic (see
/// [`run_rounds`]'s docs), and fully converges before the next starts, so `rounds` scales the
/// number of *reconnect cycles* accumulated - the axis H4.1 actually asks a soak test to stress -
/// independent of how much ever sits in the offline queue, or in this harness's own bookkeeping,
/// at once. That last part matters here specifically: keeping the full per-call history (payloads
/// included) for a run of hundreds of rounds would itself cost memory proportional to round
/// count, which is exactly the shape a real leak in the crate would also produce - undermining the
/// one thing this test exists to tell apart. Draining each round as it converges keeps this
/// harness's own footprint flat, so a growing reading below is attributable to the charge point
/// under test, not to this file.
#[tokio::test]
#[ignore = "opt-in long soak; run explicitly with `cargo test -- --ignored`, see module docs"]
async fn long_soak_survives_hundreds_of_reconnects_without_leaking() {
    let rounds: usize = std::env::var("OCPP_SOAK_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(100);
    // Retained bytes are allowed to drift by this much between the two halves without failing -
    // allocator noise from sockets/TLS-free buffers/`format!` scratch space that isn't retained
    // application state, the same reasoning `tests/memory_growth.rs`'s `TOLERANCE_BYTES` gives.
    // Fixed, not scaled by `rounds`: a leak that is proportional to reconnect count is exactly
    // what this must catch, not hide.
    const TOLERANCE_BYTES: usize = 4 * 1024 * 1024;

    let (csms, runtime) = establish(CALLS_BEFORE_DROP).await;

    async fn run_and_check_round(
        csms: &FlappingCsms,
        runtime: &ocpp_charge_point::ChargePointRuntime<TestChargePoint>,
    ) {
        drive_cycles(runtime, TXS_PER_ROUND).await;
        wait_for_ended(csms, TXS_PER_ROUND).await;
        let calls = csms.drain();
        assert_transactions_complete_and_ordered(&transaction_events_of(&calls), TXS_PER_ROUND);
    }

    // Warm up (fill any bounded collection - the authorization cache, the security log, etc. - to
    // steady state) before the first reading, exactly `tests/memory_growth.rs`'s rationale for its
    // own `WARM_UP`.
    let warm_up_rounds = rounds / 2;
    for _ in 0..warm_up_rounds {
        run_and_check_round(&csms, &runtime).await;
    }
    let after_warm_up = live();

    let follow_up_rounds = rounds - warm_up_rounds;
    for _ in 0..follow_up_rounds {
        run_and_check_round(&csms, &runtime).await;
    }
    let after_follow_up = live();

    let report = SoakReport {
        connections: csms.connection_count(),
        boots: csms.boot_count(),
    };
    assert_soak_invariants(&report, rounds);

    println!(
        "long soak: {rounds} rounds ({} transactions), {} connections, {} BootNotification calls; \
         retained memory {after_warm_up} B after {warm_up_rounds} rounds, {after_follow_up} B \
         after {rounds} total",
        rounds * TXS_PER_ROUND,
        report.connections,
        report.boots,
    );

    assert!(
        after_follow_up <= after_warm_up + TOLERANCE_BYTES,
        "retained memory grew by {} B over {follow_up_rounds} further reconnect rounds (from \
         {after_warm_up} B to {after_follow_up} B) - a leak that only shows up after many \
         reconnects is exactly what this soak test exists to catch",
        after_follow_up.saturating_sub(after_warm_up)
    );
}
