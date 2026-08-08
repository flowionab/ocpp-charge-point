//! Does retained memory *grow* as thousands of transactions run through the state machine
//! (`docs/PRODUCTION-ROADMAP.md` H4.2), as opposed to `tests/memory_budget.rs`, which only
//! measures the heap a charge point retains once every bounded collection is already full - a
//! snapshot, not a trend.
//!
//! Every collection this crate exposes for transaction traffic is documented as bounded: the
//! offline queues evict at a fixed `capacity`, the security log evicts at its own, the
//! authorization cache evicts least-recently-used past `max_authorization_cache_entries`, and a
//! connector's transaction slot is `slot.take()`n when the transaction ends
//! (`src/state/charge_point_state.rs`'s `advance_transaction`). If any of that is true, retained
//! memory should reach a steady state once those bounds fill during a warm-up run and then stay
//! flat, however many further transactions run - a per-transaction leak is exactly the thing that
//! would instead show up as the reading climbing with `FOLLOW_UP`.
//!
//! # What is driven, and what it sweeps
//!
//! [`Harness`] wires up a real [`ChargePointState`] (two EVSEs, two connectors each) alongside
//! the offline queues and security log a real deployment would route effects through -
//! [`run_transaction`] drives one connector through cable-connect, lock, present an id token,
//! authorize (caching the decision, as `crate::authorization` would), close the contactor, take
//! several meter samples, stop, open the contactor, unlock, and disconnect - exactly
//! `tests/lifecycle.rs`'s `drive_a_session`, but through [`ChargePointState`] directly rather than
//! over a socket, so many thousands of them run fast enough for a growth trend to be meaningful.
//! Every `StatusNotification`/`TransactionEvent`/`SecurityEventOccurred` effect the state machine
//! raises is pushed onto its offline queue and immediately flushed (simulating an online charge
//! point - the case every one of these collections must stay bounded under), and a security event
//! is raised periodically to sweep the security log and queue's growth alongside the transaction
//! traffic. Each transaction's id token is a fresh maximum-length (36-character) `String`, so a
//! `Transaction`/cache entry/queued message that outlived its owner would show up as `String`
//! bytes retained per transaction, not just struct padding.
//!
//! # Why an integration test rather than a unit test, and why only one `#[tokio::test]`
//!
//! Same reasoning as `tests/memory_budget.rs`: the counting [`GlobalAlloc`] below sees every
//! allocation in its binary, so sharing it with the lib test binary's 500-odd concurrently
//! running tests would make any one reading meaningless. This file gets its own binary and
//! contains a single test for the same reason.

use std::alloc::{GlobalAlloc, Layout, System};
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};

use ocpp_charge_point::offline_queue::{OfflineQueue, flush_offline_queue};
use ocpp_charge_point::security::{SecurityEventLog, SecurityLogEntry};
use ocpp_charge_point::state::{
    AuthorizationStatus, ChargePointEffect, ChargePointEvent, ChargePointState, ConnectorEvent,
    ConnectorStatusChanged, EvseEvent, IdToken, IdTokenKind, MeterSample, SecurityEvent,
    SecurityEventType, StateLimits, StopReason, TransactionEventOccurred,
};

/// Live (allocated minus freed) bytes requested through [`Counting`].
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// A [`GlobalAlloc`] that forwards to the system allocator while tracking live requested bytes -
/// identical in shape to `tests/memory_budget.rs`'s `Counting`, duplicated rather than shared
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

/// Connector topology driven through the harness: two EVSEs, two connectors each. Kept small and
/// fixed - what this test measures is per-transaction growth, not per-connector cost (that is
/// `tests/memory_budget.rs`'s `fill_connectors`), so the same four connectors carry every
/// transaction in rotation.
const CONNECTOR_COUNTS: [usize; 2] = [2, 2];

/// id token length this test fills with: OCPP 2.x's `IdTokenType.idToken` is a 36-character
/// `identifierString`, the worst case for the `String` each transaction, cache entry, and queued
/// message owns - see `tests/memory_budget.rs`'s identical `ID_TOKEN_LEN` for the same reasoning.
const ID_TOKEN_LEN: usize = 36;

/// Offline queue / security log capacity used throughout - small enough that warm-up fills every
/// one of them well before `WARM_UP` transactions have run, and identical across all four so
/// nothing here is bounded more generously than the others by accident.
const CAPACITY: usize = 50;

/// Transactions run before the first reading, to fill every bounded collection - the
/// authorization cache, and the offline queues/security log, which top out at [`CAPACITY`] -
/// so the first reading already reflects steady state rather than the ramp-up to it.
const WARM_UP: usize = 2_000;

/// Further transactions run after the first reading. A per-transaction leak this test is meant
/// to catch shows up as the *second* reading growing roughly in proportion to this count; a flat
/// reading regardless of `FOLLOW_UP` is the claim under test.
const FOLLOW_UP: usize = 3_000;

/// The slack allowed between the two readings, in bytes. Every collection this harness touches is
/// documented as bounded (see this file's module docs), so in principle the two readings should
/// be identical. The tolerance exists for allocator noise the counting wrapper sees but that
/// isn't retained application state (e.g. transient `format!`/`to_rfc3339` scratch buffers whose
/// deallocation lands on one side of a reading or the other) - it is a small, fixed budget, not
/// one that scales with [`FOLLOW_UP`], which is exactly what would hide a real per-transaction
/// leak instead of catching one.
const TOLERANCE_BYTES: usize = 8_192;

fn id_token(index: usize) -> IdToken {
    let mut value = format!("{index:0ID_TOKEN_LEN$}");
    value.truncate(ID_TOKEN_LEN);
    IdToken {
        value,
        kind: IdTokenKind::ISO14443,
    }
}

/// Everything a real charge point routes transaction/connector/security traffic through,
/// alongside the [`ChargePointState`] itself - the side-tables this item is asked to sweep for
/// unbounded growth.
struct Harness {
    state: ChargePointState,
    status_queue: OfflineQueue<ConnectorStatusChanged>,
    transaction_queue: OfflineQueue<TransactionEventOccurred>,
    security_queue: OfflineQueue<SecurityEvent>,
    security_log: SecurityEventLog,
}

impl Harness {
    fn new() -> Self {
        Self {
            state: ChargePointState::with_limits(CONNECTOR_COUNTS, StateLimits::default()),
            status_queue: OfflineQueue::with_capacity(CAPACITY),
            transaction_queue: OfflineQueue::with_capacity(CAPACITY),
            security_queue: OfflineQueue::with_capacity(CAPACITY),
            security_log: SecurityEventLog::with_capacity(CAPACITY),
        }
    }

    /// Applies `event` to the underlying state and routes every effect it produces, exactly as
    /// the real forwarding loops (`crate::availability`, `crate::transactions`,
    /// `crate::security`) would.
    async fn dispatch(&mut self, event: ChargePointEvent) {
        let effects = self.state.apply(event);
        self.route(effects).await;
    }

    /// [`Self::dispatch`] for an event addressed at one connector - the shape every step of
    /// [`run_transaction`] uses.
    async fn apply(&mut self, evse_id: usize, connector_id: usize, event: ConnectorEvent) {
        self.dispatch(ChargePointEvent::Evse {
            evse_id,
            event: EvseEvent::Connector {
                connector_id,
                event,
            },
        })
        .await;
    }

    /// Queues each CSMS-facing effect for delivery and immediately flushes it - simulating an
    /// online charge point, the case every offline queue and the security log must stay bounded
    /// under regardless of how much transaction traffic has gone by.
    async fn route(&mut self, effects: Vec<ChargePointEffect>) {
        for effect in effects {
            match effect {
                ChargePointEffect::StatusNotification(status) => {
                    self.status_queue.restore_backlog(vec![status]);
                    drain(&self.status_queue).await;
                }
                ChargePointEffect::TransactionEvent(occurred) => {
                    self.transaction_queue.restore_backlog(vec![occurred]);
                    drain(&self.transaction_queue).await;
                }
                ChargePointEffect::SecurityEventOccurred(event) => {
                    self.security_log.record(SecurityLogEntry {
                        event: event.clone(),
                        recorded_at: Some(chrono::Utc::now()),
                    });
                    self.security_queue.restore_backlog(vec![event]);
                    drain(&self.security_queue).await;
                }
                _ => {}
            }
        }
    }

    /// Raises a synthetic security event through the real event path, so
    /// [`SecurityEventLog`]/the security offline queue see traffic alongside the transaction
    /// lifecycle rather than only being exercised in isolation.
    async fn raise_security_event(&mut self, index: usize) {
        self.dispatch(ChargePointEvent::SecurityEventOccurred(SecurityEvent {
            event_type: SecurityEventType::MemoryExhaustion,
            tech_info: Some(format!(
                "synthetic memory-growth sweep event #{index} on an offline-report queue"
            )),
        }))
        .await;
    }
}

/// Delivers everything currently queued, unconditionally succeeding - the "connection is up"
/// case every one of this harness's queues is routed through.
async fn drain<M: Clone>(queue: &OfflineQueue<M>) {
    flush_offline_queue(queue, |_message| async { Ok::<(), Infallible>(()) }).await;
}

/// Drives one complete, realistic transaction lifecycle on one connector: cable connected, id
/// token presented, authorized (caching the decision), charging, several meter samples, stopped,
/// unlocked, disconnected - the same sequence `tests/lifecycle.rs`'s `drive_a_session` exercises
/// over a real socket, here driven directly against [`ChargePointState`] so thousands of them run
/// fast enough to show a growth trend.
async fn run_transaction(harness: &mut Harness, evse_id: usize, connector_id: usize, index: usize) {
    let token = id_token(index);

    harness
        .apply(evse_id, connector_id, ConnectorEvent::CableConnected)
        .await;
    harness
        .apply(evse_id, connector_id, ConnectorEvent::LockConfirmed)
        .await;
    harness
        .apply(
            evse_id,
            connector_id,
            ConnectorEvent::IdTokenPresented(token.clone()),
        )
        .await;

    // The Authorization functional block remembers every decision it receives - see
    // `ChargePointEvent::AuthorizationCached`'s docs.
    harness
        .dispatch(ChargePointEvent::AuthorizationCached {
            id_token: token.clone(),
            status: AuthorizationStatus::Accepted,
            cached_at: Some(chrono::Utc::now()),
        })
        .await;

    harness
        .apply(
            evse_id,
            connector_id,
            ConnectorEvent::ChargingAuthorized(token.clone()),
        )
        .await;
    harness
        .apply(evse_id, connector_id, ConnectorEvent::ContactorClosed)
        .await;

    for sample in 0..3u8 {
        harness
            .apply(
                evse_id,
                connector_id,
                ConnectorEvent::MeterValueSampled(MeterSample {
                    energy_wh: 1_000 + i64::from(sample) * 500,
                    power_w: Some(7_400),
                    current_ma: Some(32_000),
                    voltage_v: Some(230),
                    soc_percent: Some(40 + sample),
                }),
            )
            .await;
    }

    harness
        .apply(
            evse_id,
            connector_id,
            ConnectorEvent::ChargingStopped(StopReason::Local),
        )
        .await;
    harness
        .apply(evse_id, connector_id, ConnectorEvent::ContactorOpened)
        .await;
    harness
        .apply(evse_id, connector_id, ConnectorEvent::UnlockConfirmed)
        .await;
    harness
        .apply(evse_id, connector_id, ConnectorEvent::CableDisconnected)
        .await;

    // An odd cadence, deliberately not aligned with the four-connector rotation, so the security
    // log/queue fill on their own schedule rather than in lockstep with a particular connector.
    if index.is_multiple_of(41) {
        harness.raise_security_event(index).await;
    }
}

#[tokio::test]
async fn retained_memory_does_not_grow_with_transaction_count() {
    let mut harness = Harness::new();
    let connectors: Vec<(usize, usize)> = CONNECTOR_COUNTS
        .iter()
        .enumerate()
        .flat_map(|(evse_id, &count)| (0..count).map(move |connector_id| (evse_id, connector_id)))
        .collect();

    for index in 0..WARM_UP {
        let (evse_id, connector_id) = connectors[index % connectors.len()];
        run_transaction(&mut harness, evse_id, connector_id, index).await;
    }

    let after_warm_up = live();

    for index in WARM_UP..(WARM_UP + FOLLOW_UP) {
        let (evse_id, connector_id) = connectors[index % connectors.len()];
        run_transaction(&mut harness, evse_id, connector_id, index).await;
    }

    let after_follow_up = live();

    println!(
        "retained after {WARM_UP} warm-up transactions: {after_warm_up} B; after {FOLLOW_UP} more \
         ({} total): {after_follow_up} B",
        WARM_UP + FOLLOW_UP,
    );

    assert!(
        after_follow_up <= after_warm_up + TOLERANCE_BYTES,
        "retained memory grew by {} B over {FOLLOW_UP} further transactions (from {after_warm_up} B \
         to {after_follow_up} B) - every collection this harness touches is documented as bounded, \
         so growth proportional to transaction count means one of them is leaking",
        after_follow_up.saturating_sub(after_warm_up),
    );

    // Sweep the obvious leak candidates individually, so a failure above points at *which*
    // collection grew rather than just the total.
    assert!(
        harness.status_queue.is_empty(),
        "the status queue should have drained on every flush, not backlogged"
    );
    assert!(
        harness.transaction_queue.is_empty(),
        "the transaction queue should have drained on every flush, not backlogged"
    );
    assert!(
        harness.security_queue.len() <= harness.security_queue.capacity(),
        "the security queue exceeded its configured capacity"
    );
    assert!(
        harness.security_log.len() <= harness.security_log.capacity(),
        "the security log exceeded its configured capacity"
    );
    assert!(
        harness.state.authorization_cache.len() <= harness.state.authorization_cache.max_entries(),
        "the authorization cache exceeded its configured maximum"
    );
    for evse in &harness.state.evses {
        assert!(
            evse.transactions.iter().all(Option::is_none),
            "a connector still holds a transaction after every simulated session ended"
        );
    }
}
