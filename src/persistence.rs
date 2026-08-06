//! Durable transaction state: writing in-flight transactions through
//! [`crate::hardware::Storage`] as they progress, and recovering them on the next boot.
//!
//! This is the crash-consistency half of `docs/PRODUCTION-ROADMAP.md` workstream E - E2.1 (the
//! in-flight transaction is the first row of E2's "what must survive" table) and E4.1/E4.2's
//! recovery. The guarantee being bought is narrow and worth stating precisely:
//!
//! > A power cut at any point during a transaction must not lose the energy already delivered.
//!
//! Not "the session continues as if nothing happened" - see
//! [`crate::state::ChargePointEvent::PersistedTransactionsRestored`] for why a recovered
//! transaction is closed out rather than resumed.
//!
//! # What is written, and how often
//!
//! Flash has finite erase cycles, so a record is *not* written on every meter sample (E3.2). The
//! write policy ([`persistence_decision`]) is:
//!
//! - transaction started: write (this is the record that makes the session recoverable at all);
//! - charging state changed: write (a cheap, rare, semantically important transition);
//! - periodic meter reading: write only once the reading has moved at least
//!   [`TransactionStore::meter_write_threshold_wh`] from the last one that reached storage;
//! - transaction ended: clear the record - the session has been reported to the CSMS (or handed
//!   to the offline queue) and no longer needs recovering.
//!
//! The threshold is the knob that trades flash wear against how much energy a power cut can lose:
//! a cut can lose at most `meter_write_threshold_wh` of billable energy, and at a default of
//! 100 Wh that is a few pence at any realistic tariff, against roughly one write per two minutes
//! on a 7 kW charger.
//!
//! # Storage failure
//!
//! Every operation here degrades rather than propagating: a failing [`crate::hardware::Storage`]
//! is logged and the charge point keeps charging without durability, per `CLAUDE.md`'s
//! error-handling stance and E1.1. A charge point given [`crate::hardware::NoStorage`] therefore
//! behaves exactly as it did before this module existed.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use chrono::{DateTime, Utc};
use core::fmt;
use core::future::Future;

use crate::actor::ChargePointActor;
use crate::clock::Clock;
use crate::hardware::{AtomicStorage, Storage};
use crate::offline_queue::{OfflineQueue, flush_offline_queue};
use crate::state::{
    ChargePointEvent, ChargePointState, Component, LocalListEntry, MeterSample,
    RecoveredDeviceModelAttribute, RecoveredReservation, RecoveredTransaction, Reservation,
    Transaction, TransactionEventKind, TransactionEventOccurred, TransactionUpdateReason, Variable,
    VariableAttributeType,
};
use crate::sync::{BroadcastReceiver, WatchReceiver};

/// The version stamped into every record this module writes, and the only version it reads back.
///
/// A record carrying any other version is discarded on load (logged, not fatal) rather than
/// guessed at: the charge point then boots as if that transaction had never been persisted, which
/// is strictly better than closing out a session from a record whose meaning we're unsure of.
/// Bump this whenever [`PersistedTransaction`]'s shape changes incompatibly - see
/// `docs/PRODUCTION-ROADMAP.md` §7.3 (E3.3).
pub const SCHEMA_VERSION: u32 = 1;

/// How far the energy register must move before a periodic meter reading is worth a flash write.
/// See the module docs for the wear/loss trade-off this sets.
pub const DEFAULT_METER_WRITE_THRESHOLD_WH: i64 = 100;

/// The prefix every key this module owns starts with, so a [`Storage`] implementation shared with
/// other state (an integrator's own settings, a future offline-queue store) can't collide with it.
const KEY_PREFIX: &str = "ocpp-cp/txn";

/// One in-flight transaction as written to durable storage.
///
/// `started_at` and `meter_start` are captured here rather than on [`Transaction`] itself: both
/// are recovery/billing concerns rather than state-machine ones, and stamping a start time
/// requires a [`Clock`], which the state machine deliberately doesn't have (it stays pure and
/// clock-free - see `crate::clock`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PersistedTransaction {
    /// The [`SCHEMA_VERSION`] this record was written with.
    pub schema_version: u32,
    /// The transaction's connector's EVSE index.
    pub evse_id: usize,
    /// The transaction's connector's index within its EVSE.
    pub connector_id: usize,
    /// The transaction as of the last write that reached storage.
    pub transaction: Transaction,
    /// When the transaction started, per the [`Clock`] supplied to
    /// [`run_transaction_persistence`]. `None` if the charge point has no usable time source -
    /// either because no record reached storage before this one (see [`next_record`]'s docs), or
    /// because `clock.now()` itself didn't look synchronized (see
    /// [`crate::clock::is_synchronized`]) at the moment the transaction started - e.g. hardware
    /// with no RTC that hasn't yet received a CSMS `currentTime` (see `crate::provisioning`'s
    /// time-sync helpers). This crate never substitutes a fabricated timestamp for either case -
    /// see `docs/PRODUCTION-ROADMAP.md` §9.3 (G3.1). A transaction recovered with `started_at:
    /// None` is still fully recoverable and billable on its energy; only its start time is
    /// unknown until corrected out-of-band (e.g. by an operator reconciling against the CSMS's
    /// own `TransactionEvent(Started)` receipt time, if that adapter's clock was synchronized).
    pub started_at: Option<DateTime<Utc>>,
    /// The first meter reading seen during this transaction - the baseline the delivered energy
    /// is measured against. `None` until the first reading arrives (a transaction that is cut
    /// short before any sample has no billable energy to lose).
    pub meter_start: Option<MeterSample>,
}

/// What [`persistence_decision`] concluded should happen to durable storage in response to one
/// transaction event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistenceDecision {
    /// Write the record. Kept as a decision rather than a side effect so the write policy is
    /// testable without a [`Storage`] at all.
    Write,
    /// Leave storage untouched - this event doesn't move the recovery picture enough to be worth
    /// a flash write.
    Skip,
    /// Remove the connector's record: the transaction is over and no longer needs recovering.
    Clear,
}

/// The write policy described in the module docs, as a pure function of the event and the record
/// currently in storage. `previous` is the last record written for this connector, if any.
pub fn persistence_decision(
    previous: Option<&PersistedTransaction>,
    occurred: &TransactionEventOccurred,
    meter_write_threshold_wh: i64,
) -> PersistenceDecision {
    match occurred.kind {
        TransactionEventKind::Ended => PersistenceDecision::Clear,
        TransactionEventKind::Started
        | TransactionEventKind::Updated(TransactionUpdateReason::ChargingStateChanged) => {
            PersistenceDecision::Write
        }
        TransactionEventKind::Updated(TransactionUpdateReason::MeterValuePeriodic) => {
            let Some(sample) = occurred.transaction.last_meter_sample else {
                return PersistenceDecision::Skip;
            };
            // Nothing recoverable is on record for this connector yet (a start whose write
            // failed, or a threshold set so high that no sample has ever qualified) - write
            // unconditionally rather than letting the threshold delay recoverability itself.
            let Some(previous) = previous else {
                return PersistenceDecision::Write;
            };
            let Some(persisted) = previous.transaction.last_meter_sample else {
                return PersistenceDecision::Write;
            };
            // A meter that has gone backwards is a hardware glitch, not a reason to stop
            // persisting - compare the magnitude of the move either way.
            if (sample.energy_wh - persisted.energy_wh).abs() >= meter_write_threshold_wh {
                PersistenceDecision::Write
            } else {
                PersistenceDecision::Skip
            }
        }
    }
}

/// Reads and writes [`PersistedTransaction`] records through a [`Storage`].
///
/// Every method degrades rather than failing: a storage error is logged and reported as "nothing
/// persisted" / "nothing recovered", never returned to the caller to handle or panic on. See the
/// module docs.
#[derive(Debug, Clone)]
pub struct TransactionStore<S> {
    storage: S,
    meter_write_threshold_wh: i64,
}

impl<S: Storage> TransactionStore<S> {
    /// Creates a store over `storage` with [`DEFAULT_METER_WRITE_THRESHOLD_WH`].
    pub fn new(storage: S) -> Self {
        Self {
            storage,
            meter_write_threshold_wh: DEFAULT_METER_WRITE_THRESHOLD_WH,
        }
    }

    /// Overrides how far the energy register must move before a periodic meter reading earns a
    /// write. A value of `0` writes on every sample (maximum billing fidelity, maximum flash
    /// wear); a large value writes only on lifecycle transitions.
    pub fn with_meter_write_threshold_wh(mut self, threshold_wh: i64) -> Self {
        self.meter_write_threshold_wh = threshold_wh;
        self
    }

    /// The configured threshold - see [`Self::with_meter_write_threshold_wh`].
    pub fn meter_write_threshold_wh(&self) -> i64 {
        self.meter_write_threshold_wh
    }

    /// Writes `record`, replacing any previous record for the same connector. Returns whether the
    /// write actually reached storage - `false` means the charge point is now running without
    /// durability for that connector, already logged.
    pub async fn save(&self, record: &PersistedTransaction) -> bool {
        let Ok(encoded) = serde_json::to_vec(record) else {
            // Only reachable if a field's `Serialize` impl fails, which none of ours can.
            tracing::error!("failed to encode a transaction record for storage");
            return false;
        };
        let key = transaction_key(record.evse_id, record.connector_id);
        match self.storage.set(&key, &encoded).await {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    key = key.as_str(),
                    "failed to persist a transaction record; continuing without durability"
                );
                false
            }
        }
    }

    /// Removes the connector's record, if any. A missing record is not an error.
    pub async fn clear(&self, evse_id: usize, connector_id: usize) {
        let key = transaction_key(evse_id, connector_id);
        if let Err(err) = self.storage.remove(&key).await {
            tracing::warn!(
                error = %err,
                key = key.as_str(),
                "failed to clear a persisted transaction record"
            );
        }
    }

    /// Reads back the connector's record, or `None` if there isn't one, it can't be read, or it
    /// was written by an incompatible [`SCHEMA_VERSION`].
    pub async fn load(&self, evse_id: usize, connector_id: usize) -> Option<PersistedTransaction> {
        let key = transaction_key(evse_id, connector_id);
        let encoded = match self.storage.get(&key).await {
            Ok(encoded) => encoded?,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    key = key.as_str(),
                    "failed to read a persisted transaction record; treating it as absent"
                );
                return None;
            }
        };
        let record: PersistedTransaction = match serde_json::from_slice(&encoded) {
            Ok(record) => record,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    key = key.as_str(),
                    "a persisted transaction record could not be decoded; discarding it"
                );
                return None;
            }
        };
        if record.schema_version != SCHEMA_VERSION {
            tracing::warn!(
                found = record.schema_version,
                expected = SCHEMA_VERSION,
                key = key.as_str(),
                "discarding a persisted transaction record written by an incompatible schema version"
            );
            return None;
        }
        Some(record)
    }

    /// Every persisted record across the given topology (`connector_counts[evse_id]` is that
    /// EVSE's connector count), in `(evse_id, connector_id)` order.
    pub async fn load_all(&self, connector_counts: &[usize]) -> Vec<PersistedTransaction> {
        let mut records = Vec::new();
        for (evse_id, connector_count) in connector_counts.iter().copied().enumerate() {
            for connector_id in 0..connector_count {
                if let Some(record) = self.load(evse_id, connector_id).await {
                    records.push(record);
                }
            }
        }
        records
    }

    /// Records the transaction-id counter, so a recovered charge point never reissues an id the
    /// CSMS has already seen. Written alongside the record of every transaction that starts.
    pub async fn save_next_transaction_id(&self, next_transaction_id: u64) {
        let encoded = alloc::string::ToString::to_string(&next_transaction_id);
        if let Err(err) = self.storage.set(&counter_key(), encoded.as_bytes()).await {
            tracing::warn!(
                error = %err,
                "failed to persist the transaction id counter; ids may be reused after a reboot"
            );
        }
    }

    /// The persisted transaction-id counter, or `0` if none was ever written or it can't be read.
    pub async fn load_next_transaction_id(&self) -> u64 {
        let encoded = match self.storage.get(&counter_key()).await {
            Ok(Some(encoded)) => encoded,
            Ok(None) => return 0,
            Err(err) => {
                tracing::warn!(error = %err, "failed to read the persisted transaction id counter");
                return 0;
            }
        };
        core::str::from_utf8(&encoded)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| {
                tracing::warn!(
                    "the persisted transaction id counter is unreadable; starting from 0"
                );
                0
            })
    }
}

impl<S: Storage + Send + Sync> TransactionStore<AtomicStorage<S>> {
    /// Creates a store over `storage`, wrapped in [`AtomicStorage`] so a power cut mid-write can
    /// never leave a *torn* (partially written) record in place of the previous complete one -
    /// `docs/PRODUCTION-ROADMAP.md` §7.3's E3.1. [`load`](TransactionStore::load) already
    /// discards a record that fails to decode; this closes the remaining gap, where the raw
    /// [`Storage::set`] call itself isn't guaranteed atomic by the underlying hardware.
    ///
    /// Prefer this over [`TransactionStore::new`] whenever `storage` is real hardware rather than
    /// [`crate::hardware::NoStorage`] or a test double already known to be atomic (e.g.
    /// [`crate::hardware::InMemoryStorage`]) - see [`AtomicStorage`]'s docs for exactly what
    /// guarantee it does and doesn't add, including what it still can't protect against.
    pub fn new_atomic(storage: S) -> Self {
        TransactionStore::new(AtomicStorage::new(storage))
    }
}

fn transaction_key(evse_id: usize, connector_id: usize) -> String {
    format!("{KEY_PREFIX}/{evse_id}/{connector_id}")
}

fn counter_key() -> String {
    format!("{KEY_PREFIX}/next-id")
}

/// Persists every transaction event received on `events` according to the module's write policy,
/// forever. Storage failures are logged and never stop the loop - losing durability must not also
/// lose the charge point.
///
/// `clock` stamps [`PersistedTransaction::started_at`]; pass [`crate::clock::SystemClock`] on
/// std, or an RTC-backed [`Clock`] on embedded.
pub async fn run_transaction_persistence<S: Storage, C: Clock>(
    mut events: BroadcastReceiver<TransactionEventOccurred>,
    store: &TransactionStore<S>,
    clock: &C,
) {
    // The last record written per connector, shadowed in memory so the write policy can consult
    // it without a read-back on every event (a read per meter sample would defeat the point of
    // bounding writes). Rebuilt from storage at start-up so a restart mid-session doesn't reset
    // the threshold's baseline.
    let mut shadow: Vec<(usize, usize, PersistedTransaction)> = Vec::new();
    while let Ok(occurred) = events.recv().await {
        let address = (occurred.evse_id, occurred.connector_id);
        let previous = shadow
            .iter()
            .find(|(evse_id, connector_id, _)| (*evse_id, *connector_id) == address)
            .map(|(_, _, record)| record);
        match persistence_decision(previous, &occurred, store.meter_write_threshold_wh()) {
            PersistenceDecision::Skip => {}
            PersistenceDecision::Clear => {
                store.clear(address.0, address.1).await;
                shadow.retain(|(evse_id, connector_id, _)| (*evse_id, *connector_id) != address);
            }
            PersistenceDecision::Write => {
                let record = next_record(previous, &occurred, clock);
                if occurred.kind == TransactionEventKind::Started {
                    // The counter must be durable before the transaction that consumed it is, so
                    // a cut between the two can only ever skip an id, never reuse one.
                    store
                        .save_next_transaction_id(record.transaction.id.0 + 1)
                        .await;
                }
                if store.save(&record).await {
                    shadow
                        .retain(|(evse_id, connector_id, _)| (*evse_id, *connector_id) != address);
                    shadow.push((address.0, address.1, record));
                }
            }
        }
    }
}

/// Builds the record to write for `occurred`, carrying forward the start time and meter baseline
/// already established by `previous` (a transaction's start time must not drift with every
/// subsequent write).
///
/// G3.1 (`docs/PRODUCTION-ROADMAP.md` §9.3): `clock` is exactly the caller-injectable [`Clock`]
/// that can be backed by hardware with no RTC (see [`run_transaction_persistence`]'s docs and
/// `crate::clock`'s), so a `Started` event's start time is only recorded when `clock.now()`
/// actually looks synchronized (see [`crate::clock::is_synchronized`]) - an unset RTC's reading
/// (conventionally the Unix epoch or similar) becomes `None`, never a fabricated "started at
/// 1970" record. `started_at` being `None` is the honest, already-`Option` representation this
/// module chose for "unknown" rather than inventing a plausible-looking timestamp - see
/// [`PersistedTransaction::started_at`]'s docs. This never blocks recording the transaction
/// itself (G3.1's explicit requirement): every other field is still written, the transaction is
/// still recoverable after a power cut, and only the start time is left blank pending a real
/// sync - the CSMS-facing `TransactionEvent(Started)` timestamp is a separate, `std`-only
/// concern (see `crate::transactions`'s `with_system_clock` adapters, which are not reachable
/// from unsynchronized embedded hardware since they're locked to `SystemClock`, always
/// OS-backed and real).
fn next_record<C: Clock>(
    previous: Option<&PersistedTransaction>,
    occurred: &TransactionEventOccurred,
    clock: &C,
) -> PersistedTransaction {
    let started_at = match (occurred.kind, previous) {
        (TransactionEventKind::Started, _) => {
            let now = clock.now();
            crate::clock::is_synchronized(&now).then_some(now)
        }
        (_, Some(previous)) => previous.started_at,
        // Reached only if the record written at start never made it to storage; the transaction
        // is still worth persisting, just without a trustworthy start time.
        (_, None) => None,
    };
    let meter_start = previous
        .and_then(|previous| previous.meter_start)
        .or(occurred.transaction.last_meter_sample);
    PersistedTransaction {
        schema_version: SCHEMA_VERSION,
        evse_id: occurred.evse_id,
        connector_id: occurred.connector_id,
        transaction: occurred.transaction.clone(),
        started_at,
        meter_start,
    }
}

/// Recovers whatever was in flight when the charge point last lost power, and hands it to the
/// state machine as a single [`ChargePointEvent::PersistedTransactionsRestored`] - which closes
/// each recovered transaction out with [`crate::state::StopReason::PowerLoss`], reporting the
/// energy that reached storage so it can still be billed.
///
/// Call this once at boot. It does not need the Transactions functional block to be registered
/// first - the actor fans the resulting `TransactionEvent(Ended)` out to whichever subscribers
/// exist, and a subscription taken before this runs (as
/// [`crate::builder::ChargePointBuilder::start`] does) still receives it. It *must*, however, run
/// before any new transaction can start, or the restored id counter could arrive too late to stop
/// an id being reused.
///
/// Returns the number of transactions recovered (`0` on a clean boot, or when storage is empty,
/// unreadable, or [`crate::hardware::NoStorage`]).
pub async fn restore_transactions<S: Storage>(
    actor: &ChargePointActor,
    store: &TransactionStore<S>,
) -> usize {
    let connector_counts: Vec<usize> = actor
        .state()
        .evses
        .iter()
        .map(|evse| evse.connectors.len())
        .collect();
    let records = store.load_all(&connector_counts).await;
    let next_transaction_id = store.load_next_transaction_id().await;
    let recovered = records.len();
    if recovered > 0 {
        tracing::warn!(
            count = recovered,
            "recovering transactions that were in flight when the charge point last lost power"
        );
    }
    let transactions = records
        .iter()
        .map(|record| RecoveredTransaction {
            evse_id: record.evse_id,
            connector_id: record.connector_id,
            transaction: record.transaction.clone(),
        })
        .collect();
    let _ = actor
        .send(ChargePointEvent::PersistedTransactionsRestored {
            next_transaction_id,
            transactions,
        })
        .await;
    // Only after the state machine has taken ownership of them: a cut between the load above and
    // here re-recovers the same transactions on the next boot, which is a duplicate report the
    // CSMS can reconcile - clearing first would instead lose them outright.
    for record in &records {
        store.clear(record.evse_id, record.connector_id).await;
    }
    recovered
}

/// The version stamped into every [`PersistedQueue`] record. Independent of [`SCHEMA_VERSION`]
/// (the transaction-record schema) since the two record shapes evolve on their own schedules; a
/// record carrying any other version is discarded on load, exactly as [`SCHEMA_VERSION`] is - see
/// that constant's docs for the reasoning.
pub const QUEUE_SCHEMA_VERSION: u32 = 1;

/// The prefix every key [`QueueStore`] owns starts with - see [`TransactionStore`]'s `KEY_PREFIX`
/// for why this exists.
const QUEUE_KEY_PREFIX: &str = "ocpp-cp/queue";

/// The default number of queue mutations (pushes or deliveries) between whole-queue snapshot
/// writes - see [`QueueStore::with_write_threshold`] for the wear/loss trade-off this sets.
///
/// Unlike [`DEFAULT_METER_WRITE_THRESHOLD_WH`], which throttles by how far a value moved, an
/// [`crate::offline_queue::OfflineQueue`] has no natural distance metric between states - only a
/// count of how many messages have come and gone since the backlog was last durable. `1` writes on
/// every mutation, favouring "recover everything queued" over flash wear; an integrator with a
/// battery-backed RTC-grade outage budget and flash wear to spare should keep it, one that expects
/// long, message-heavy outages (in particular `TransactionEvent`s carrying periodic meter
/// readings, which flow through this same queue) should raise it via
/// [`QueueStore::with_write_threshold`] and accept losing up to that many of the most recently
/// queued/delivered messages to a power cut mid-outage.
pub const DEFAULT_QUEUE_WRITE_THRESHOLD: usize = 1;

/// One [`crate::offline_queue::OfflineQueue`]'s entire backlog as written to durable storage.
///
/// Written and read as a single whole-queue snapshot rather than per-message records: an
/// `OfflineQueue` only ever has one logical owner (the forwarding task for one functional block),
/// so there's no benefit to per-entry keys the way [`TransactionStore`] needs per-connector ones,
/// and a snapshot is what lets [`restore_offline_queue`] replay the backlog **in order** with a
/// single read - order is exactly what matters for `TransactionEvent`s (`docs/PRODUCTION-ROADMAP.md`
/// §7.4, E4.3).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PersistedQueue<M> {
    /// The [`QUEUE_SCHEMA_VERSION`] this record was written with.
    pub schema_version: u32,
    /// The queue's backlog, oldest message first - the exact order
    /// [`crate::offline_queue::flush_offline_queue`] would deliver them in.
    pub messages: Vec<M>,
}

/// What [`QueueStore`]'s write policy concluded should happen to durable storage in response to a
/// queue mutation. Mirrors [`PersistenceDecision`]; kept as a separate, smaller type (no `Skip`
/// distinction needed beyond count-based debouncing, and an explicit `Clear` for "the queue
/// drained back to empty") because a queue snapshot's write policy is measured in message count,
/// not an application-specific magnitude like energy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuePersistenceDecision {
    /// Write a fresh whole-queue snapshot.
    Write,
    /// Leave storage untouched - not enough has changed since the last write to be worth a flash
    /// write yet.
    Skip,
    /// Remove the stored snapshot: the queue is empty, so there is nothing left to recover.
    Clear,
}

/// The write policy used by [`run_persisted_offline_queue`], as a pure function of the queue's
/// length before and after the mutation. `mutations_since_write` is how many mutations have been
/// skipped since the last write reached storage; `write_threshold` is
/// [`QueueStore::write_threshold`].
///
/// Two cases always write/clear regardless of the threshold: the queue going from empty to
/// non-empty (the first message queued during an outage must be durable immediately - waiting for
/// the threshold could lose the *only* queued message to a cut moments later) and the queue
/// draining back to empty (nothing left to lose, and leaving a stale snapshot in storage would
/// make the next boot "recover" messages that were actually delivered).
pub fn queue_persistence_decision(
    previous_len: usize,
    new_len: usize,
    mutations_since_write: usize,
    write_threshold: usize,
) -> QueuePersistenceDecision {
    if new_len == previous_len {
        return QueuePersistenceDecision::Skip;
    }
    if new_len == 0 {
        return QueuePersistenceDecision::Clear;
    }
    if previous_len == 0 {
        return QueuePersistenceDecision::Write;
    }
    if mutations_since_write + 1 >= write_threshold.max(1) {
        QueuePersistenceDecision::Write
    } else {
        QueuePersistenceDecision::Skip
    }
}

/// Reads and writes a single [`crate::offline_queue::OfflineQueue`]'s backlog, as a whole-queue
/// [`PersistedQueue`] snapshot, through a [`Storage`].
///
/// Every method degrades rather than failing, exactly like [`TransactionStore`] - see the module
/// docs and `CLAUDE.md`'s error-handling stance.
#[derive(Debug, Clone)]
pub struct QueueStore<S> {
    storage: S,
    key: String,
    write_threshold: usize,
}

impl<S: Storage> QueueStore<S> {
    /// Creates a store over `storage` for the queue named `name` (e.g. `"transaction"`,
    /// `"status"`, `"security"`), with [`DEFAULT_QUEUE_WRITE_THRESHOLD`]. `name` becomes part of
    /// the storage key, so it must be unique among the queues sharing this `storage`.
    pub fn new(storage: S, name: &str) -> Self {
        Self {
            storage,
            key: format!("{QUEUE_KEY_PREFIX}/{name}"),
            write_threshold: DEFAULT_QUEUE_WRITE_THRESHOLD,
        }
    }

    /// Overrides how many mutations must accumulate between whole-queue snapshot writes - see
    /// [`DEFAULT_QUEUE_WRITE_THRESHOLD`] for the trade-off this sets. Clamped to at least `1` by
    /// every read site ([`queue_persistence_decision`]), so `0` behaves the same as `1`.
    pub fn with_write_threshold(mut self, write_threshold: usize) -> Self {
        self.write_threshold = write_threshold;
        self
    }

    /// The configured write threshold - see [`Self::with_write_threshold`].
    pub fn write_threshold(&self) -> usize {
        self.write_threshold
    }

    /// Writes `messages` as the queue's whole backlog, replacing whatever snapshot was there
    /// before. Returns whether the write actually reached storage.
    pub async fn save<M: serde::Serialize + Send + Sync>(&self, messages: &[M]) -> bool {
        let Ok(encoded) = serde_json::to_vec(&SerializablePersistedQueue {
            schema_version: QUEUE_SCHEMA_VERSION,
            messages,
        }) else {
            tracing::error!("failed to encode an offline-queue snapshot for storage");
            return false;
        };
        match self.storage.set(&self.key, &encoded).await {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    key = self.key.as_str(),
                    "failed to persist an offline-queue snapshot; continuing without durability \
                     for this queue"
                );
                false
            }
        }
    }

    /// Removes the stored snapshot, if any. A missing snapshot is not an error.
    pub async fn clear(&self) {
        if let Err(err) = self.storage.remove(&self.key).await {
            tracing::warn!(
                error = %err,
                key = self.key.as_str(),
                "failed to clear a persisted offline-queue snapshot"
            );
        }
    }

    /// Reads back the queue's backlog, oldest message first, or an empty `Vec` if there isn't one,
    /// it can't be read, or it was written by an incompatible [`QUEUE_SCHEMA_VERSION`] - discarded
    /// rather than guessed at, exactly like [`TransactionStore::load`].
    pub async fn load<M: serde::de::DeserializeOwned>(&self) -> Vec<M> {
        let encoded = match self.storage.get(&self.key).await {
            Ok(Some(encoded)) => encoded,
            Ok(None) => return Vec::new(),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    key = self.key.as_str(),
                    "failed to read a persisted offline-queue snapshot; treating it as absent"
                );
                return Vec::new();
            }
        };
        let record: PersistedQueue<M> = match serde_json::from_slice(&encoded) {
            Ok(record) => record,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    key = self.key.as_str(),
                    "a persisted offline-queue snapshot could not be decoded; discarding it"
                );
                return Vec::new();
            }
        };
        if record.schema_version != QUEUE_SCHEMA_VERSION {
            tracing::warn!(
                found = record.schema_version,
                expected = QUEUE_SCHEMA_VERSION,
                key = self.key.as_str(),
                "discarding a persisted offline-queue snapshot written by an incompatible schema \
                 version"
            );
            return Vec::new();
        }
        record.messages
    }
}

/// A borrowing twin of [`PersistedQueue`] used only to encode a snapshot without requiring
/// [`QueueStore::save`]'s caller to hand over ownership of `messages`.
#[derive(serde::Serialize)]
struct SerializablePersistedQueue<'a, M> {
    schema_version: u32,
    messages: &'a [M],
}

impl<S: Storage + Send + Sync> QueueStore<AtomicStorage<S>> {
    /// Creates a store over `storage`, wrapped in [`AtomicStorage`] for the same reason
    /// [`TransactionStore::new_atomic`] does - see that method's docs.
    pub fn new_atomic(storage: S, name: &str) -> Self {
        QueueStore::new(AtomicStorage::new(storage), name)
    }
}

/// Restores a queue's persisted backlog into `queue` at boot, in order, **before** any live
/// traffic starts flowing through it - call this before spawning
/// [`run_persisted_offline_queue`]/[`crate::offline_queue::run_with_offline_queue`] for the same
/// queue, or a message that arrives first could be delivered ahead of older ones the backlog
/// restores, breaking ordering (E4.3's whole point for `TransactionEvent`s).
///
/// Generic over both the queue's message type `M` and its on-disk representation `P` so a message
/// type that can't (or shouldn't) derive `serde` traits directly - `TransactionEventOccurred`, for
/// instance, carries enums owned by `crate::state` that this module has no license to add
/// `#[derive(Serialize)]` to - can still be persisted through a small mirror type instead; see
/// [`restore_transaction_event_queue`] for that case. A message type that already derives both
/// `Serialize`/`Deserialize` can just set `P = M` and lean on the reflexive `From<T> for T` impl.
///
/// The restored backlog goes through [`crate::offline_queue::OfflineQueue::restore_backlog`], so
/// it still respects the queue's capacity and [`crate::offline_queue::OverflowPolicy`] exactly as
/// if the messages had arrived one at a time - see that method's docs. If the persisted backlog
/// is larger than the queue's capacity (e.g. the capacity was lowered since the snapshot was
/// written), the overflow policy decides what's dropped, exactly as it would for live traffic;
/// this is logged, not silently swallowed.
///
/// Returns the number of messages read back from storage (not the number actually kept, if any
/// were dropped by the capacity/overflow check above).
pub async fn restore_offline_queue<M, P, S>(queue: &OfflineQueue<M>, store: &QueueStore<S>) -> usize
where
    M: Clone + From<P>,
    P: serde::de::DeserializeOwned,
    S: Storage,
{
    let messages: Vec<M> = store.load::<P>().await.into_iter().map(M::from).collect();
    let recovered = messages.len();
    if recovered > 0 {
        tracing::info!(
            count = recovered,
            "replaying a persisted offline-message-queue backlog after reboot"
        );
    }
    let dropped = queue.restore_backlog(messages);
    if !dropped.is_empty() {
        tracing::warn!(
            dropped = dropped.len(),
            "a restored offline-queue backlog exceeded the queue's capacity; the overflow policy \
             dropped the excess exactly as it would for live traffic"
        );
    }
    recovered
}

/// Snapshots `queue` into `store` (converting each message to `P` first) if its length actually
/// changed since `*previous_len`, applying [`queue_persistence_decision`]. Updates `*previous_len`
/// and `*mutations_since_write` in place so the caller can thread them through repeated calls (one
/// per mutation) without re-deriving them from storage.
async fn persist_queue_change<M, P, S>(
    store: &QueueStore<S>,
    queue: &OfflineQueue<M>,
    previous_len: &mut usize,
    mutations_since_write: &mut usize,
) where
    M: Clone,
    P: From<M> + serde::Serialize + Send + Sync,
    S: Storage,
{
    let new_len = queue.len();
    match queue_persistence_decision(
        *previous_len,
        new_len,
        *mutations_since_write,
        store.write_threshold(),
    ) {
        QueuePersistenceDecision::Write => {
            let snapshot: Vec<P> = queue.snapshot().into_iter().map(P::from).collect();
            store.save(&snapshot).await;
            *mutations_since_write = 0;
        }
        QueuePersistenceDecision::Clear => {
            store.clear().await;
            *mutations_since_write = 0;
        }
        QueuePersistenceDecision::Skip => {
            if new_len != *previous_len {
                *mutations_since_write += 1;
            }
        }
    }
    *previous_len = new_len;
}

/// [`crate::offline_queue::run_with_offline_queue`], plus durability: every push and every
/// successful delivery is reflected into `store` (via the `M` -> `P` conversion - see
/// [`restore_offline_queue`]) per [`queue_persistence_decision`]'s write policy, so a reboot
/// mid-outage can replay whatever didn't make it out.
///
/// Call [`restore_offline_queue`] on `queue` before spawning this, so a persisted backlog is
/// already in place (and already counted in the length this function starts tracking from) rather
/// than being overwritten by an empty starting state.
pub async fn run_persisted_offline_queue<M, P, S, F, Fut, E, H, HFut>(
    mut events: BroadcastReceiver<M>,
    queue: &OfflineQueue<M>,
    store: &QueueStore<S>,
    mut send: F,
    mut on_overflow: H,
) where
    M: Clone,
    P: From<M> + serde::Serialize + Send + Sync,
    S: Storage,
    F: FnMut(M) -> Fut,
    Fut: Future<Output = Result<(), E>>,
    E: fmt::Display,
    H: FnMut(M) -> HFut,
    HFut: Future<Output = ()>,
{
    let mut previous_len = queue.len();
    let mut mutations_since_write: usize = 0;
    while let Ok(message) = events.recv().await {
        if let Some(dropped) = queue.push(message) {
            on_overflow(dropped).await;
        }
        persist_queue_change::<M, P, S>(
            store,
            queue,
            &mut previous_len,
            &mut mutations_since_write,
        )
        .await;

        flush_offline_queue(queue, &mut send).await;
        persist_queue_change::<M, P, S>(
            store,
            queue,
            &mut previous_len,
            &mut mutations_since_write,
        )
        .await;
    }
}

/// Flushes `queue` (exactly like [`crate::offline_queue::flush_offline_queue`]) and then
/// unconditionally reconciles `store` with whatever's left - used from a
/// [`crate::connection::ReconnectHandler`] callback, where a flush is a rare event (a reconnect),
/// not the steady per-message drumbeat [`run_persisted_offline_queue`]'s write-threshold exists to
/// throttle, so there's no reason to debounce this write.
pub async fn flush_and_persist_offline_queue<M, P, S, F, Fut, E>(
    queue: &OfflineQueue<M>,
    store: &QueueStore<S>,
    send: F,
) where
    M: Clone,
    P: From<M> + serde::Serialize + Send + Sync,
    S: Storage,
    F: FnMut(M) -> Fut,
    Fut: Future<Output = Result<(), E>>,
    E: fmt::Display,
{
    flush_offline_queue(queue, send).await;
    let snapshot: Vec<P> = queue.snapshot().into_iter().map(P::from).collect();
    if snapshot.is_empty() {
        store.clear().await;
    } else {
        store.save(&snapshot).await;
    }
}

/// A `serde`-able mirror of [`TransactionEventKind`], since the original enum lives in
/// `crate::state` and isn't (and, per its module's ownership, shouldn't need to be) `serde`-
/// derived just for this module's benefit. Exhaustively matched both ways below, so the
/// conversion can never fail or silently reinterpret a variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum PersistedTransactionEventKind {
    Started,
    UpdatedChargingStateChanged,
    UpdatedMeterValuePeriodic,
    Ended,
}

impl From<TransactionEventKind> for PersistedTransactionEventKind {
    fn from(kind: TransactionEventKind) -> Self {
        match kind {
            TransactionEventKind::Started => Self::Started,
            TransactionEventKind::Updated(TransactionUpdateReason::ChargingStateChanged) => {
                Self::UpdatedChargingStateChanged
            }
            TransactionEventKind::Updated(TransactionUpdateReason::MeterValuePeriodic) => {
                Self::UpdatedMeterValuePeriodic
            }
            TransactionEventKind::Ended => Self::Ended,
        }
    }
}

impl From<PersistedTransactionEventKind> for TransactionEventKind {
    fn from(kind: PersistedTransactionEventKind) -> Self {
        match kind {
            PersistedTransactionEventKind::Started => Self::Started,
            PersistedTransactionEventKind::UpdatedChargingStateChanged => {
                Self::Updated(TransactionUpdateReason::ChargingStateChanged)
            }
            PersistedTransactionEventKind::UpdatedMeterValuePeriodic => {
                Self::Updated(TransactionUpdateReason::MeterValuePeriodic)
            }
            PersistedTransactionEventKind::Ended => Self::Ended,
        }
    }
}

/// The on-disk representation of one queued [`TransactionEventOccurred`] - the `P` type parameter
/// [`restore_offline_queue`]/[`run_persisted_offline_queue`]/[`flush_and_persist_offline_queue`]
/// are instantiated with for the offline transaction-event queue (E2/E4.3). `transaction` reuses
/// [`Transaction`]'s own `Serialize`/`Deserialize` impl (already relied on by
/// [`PersistedTransaction`]); `kind` goes through [`PersistedTransactionEventKind`] for the reason
/// documented there.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct PersistedQueuedTransactionEvent {
    evse_id: usize,
    connector_id: usize,
    kind: PersistedTransactionEventKind,
    transaction: Transaction,
}

impl From<TransactionEventOccurred> for PersistedQueuedTransactionEvent {
    fn from(occurred: TransactionEventOccurred) -> Self {
        Self {
            evse_id: occurred.evse_id,
            connector_id: occurred.connector_id,
            kind: occurred.kind.into(),
            transaction: occurred.transaction,
        }
    }
}

impl From<PersistedQueuedTransactionEvent> for TransactionEventOccurred {
    fn from(persisted: PersistedQueuedTransactionEvent) -> Self {
        Self {
            evse_id: persisted.evse_id,
            connector_id: persisted.connector_id,
            kind: persisted.kind.into(),
            transaction: persisted.transaction,
        }
    }
}

/// [`restore_offline_queue`], specialized to the offline transaction-event queue - see
/// [`crate::builder::ChargePointBuilder::transaction_events_persisted`].
pub async fn restore_transaction_event_queue<S: Storage>(
    queue: &OfflineQueue<TransactionEventOccurred>,
    store: &QueueStore<S>,
) -> usize {
    restore_offline_queue::<TransactionEventOccurred, PersistedQueuedTransactionEvent, S>(
        queue, store,
    )
    .await
}

/// [`run_persisted_offline_queue`], specialized to the offline transaction-event queue - see
/// [`crate::builder::ChargePointBuilder::transaction_events_persisted`].
pub async fn run_persisted_transaction_event_queue<S, F, Fut, E, H, HFut>(
    events: BroadcastReceiver<TransactionEventOccurred>,
    queue: &OfflineQueue<TransactionEventOccurred>,
    store: &QueueStore<S>,
    send: F,
    on_overflow: H,
) where
    S: Storage,
    F: FnMut(TransactionEventOccurred) -> Fut,
    Fut: Future<Output = Result<(), E>>,
    E: fmt::Display,
    H: FnMut(TransactionEventOccurred) -> HFut,
    HFut: Future<Output = ()>,
{
    run_persisted_offline_queue::<
        TransactionEventOccurred,
        PersistedQueuedTransactionEvent,
        S,
        F,
        Fut,
        E,
        H,
        HFut,
    >(events, queue, store, send, on_overflow)
    .await
}

/// [`flush_and_persist_offline_queue`], specialized to the offline transaction-event queue - see
/// [`crate::builder::ChargePointBuilder::transaction_events_persisted`].
pub async fn flush_and_persist_transaction_event_queue<S, F, Fut, E>(
    queue: &OfflineQueue<TransactionEventOccurred>,
    store: &QueueStore<S>,
    send: F,
) where
    S: Storage,
    F: FnMut(TransactionEventOccurred) -> Fut,
    Fut: Future<Output = Result<(), E>>,
    E: fmt::Display,
{
    flush_and_persist_offline_queue::<
        TransactionEventOccurred,
        PersistedQueuedTransactionEvent,
        S,
        F,
        Fut,
        E,
    >(queue, store, send)
    .await
}

// --- local authorization list persistence (E2.4, docs/PRODUCTION-ROADMAP.md §7.2) ---

/// The version stamped into every [`PersistedLocalAuthorizationList`] record. Independent of
/// [`SCHEMA_VERSION`]/[`QUEUE_SCHEMA_VERSION`] - see those constants' docs for why each concern
/// versions on its own schedule.
pub const LOCAL_AUTH_LIST_SCHEMA_VERSION: u32 = 1;

/// The key the whole local authorization list is written under. A single whole-list snapshot,
/// not one record per entry: the list only ever changes via one `SendLocalList` call replacing
/// or patching the entire thing at once (never a single entry in isolation from the CSMS's own
/// perspective - even a differential update is resolved to the full resulting list before
/// [`crate::state::ChargePointEvent::LocalListUpdated`] is even raised, see
/// `crate::local_authorization_list::handle_send_local_list`), so there is no per-entry
/// addressing to gain the way [`TransactionStore`] needs per-connector keys.
const LOCAL_AUTH_LIST_KEY: &str = "ocpp-cp/local-auth-list";

/// The local authorization list as written to durable storage.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PersistedLocalAuthorizationList {
    /// The [`LOCAL_AUTH_LIST_SCHEMA_VERSION`] this record was written with.
    pub schema_version: u32,
    /// The list's version number.
    pub version: i64,
    /// The list's entries.
    pub entries: Vec<LocalListEntry>,
}

/// Reads and writes the local authorization list through a [`Storage`], mirroring
/// [`TransactionStore`]'s degrade-rather-than-fail behaviour.
///
/// Write policy: unlike a periodic meter reading, `SendLocalList` is a rare, CSMS-initiated,
/// operator-driven event (provisioning or updating an offline authorization cache), not
/// something that fires on a hot path - so this writes on every change, no threshold. Bounding
/// writes here the way [`persistence_decision`] bounds meter-reading writes would be
/// over-engineering a knob nothing exercises: a charge point does not receive `SendLocalList`
/// calls often enough for flash wear to be a realistic concern.
#[derive(Debug, Clone)]
pub struct LocalAuthorizationListStore<S> {
    storage: S,
}

impl<S: Storage> LocalAuthorizationListStore<S> {
    /// Creates a store over `storage`.
    pub fn new(storage: S) -> Self {
        Self { storage }
    }

    /// Writes the list outright, replacing whatever was there before. Returns whether the write
    /// actually reached storage.
    pub async fn save(&self, version: i64, entries: &[LocalListEntry]) -> bool {
        let record = PersistedLocalAuthorizationList {
            schema_version: LOCAL_AUTH_LIST_SCHEMA_VERSION,
            version,
            entries: entries.to_vec(),
        };
        let Ok(encoded) = serde_json::to_vec(&record) else {
            tracing::error!("failed to encode the local authorization list for storage");
            return false;
        };
        match self.storage.set(LOCAL_AUTH_LIST_KEY, &encoded).await {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "failed to persist the local authorization list; continuing without \
                     durability for it"
                );
                false
            }
        }
    }

    /// Reads back the persisted list, or `None` if there isn't one, it can't be read, or it was
    /// written by an incompatible [`LOCAL_AUTH_LIST_SCHEMA_VERSION`] - discarded rather than
    /// guessed at, exactly like [`TransactionStore::load`].
    pub async fn load(&self) -> Option<PersistedLocalAuthorizationList> {
        let encoded = match self.storage.get(LOCAL_AUTH_LIST_KEY).await {
            Ok(Some(encoded)) => encoded,
            Ok(None) => return None,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "failed to read the persisted local authorization list; treating it as absent"
                );
                return None;
            }
        };
        let record: PersistedLocalAuthorizationList = match serde_json::from_slice(&encoded) {
            Ok(record) => record,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "a persisted local authorization list could not be decoded; discarding it"
                );
                return None;
            }
        };
        if record.schema_version != LOCAL_AUTH_LIST_SCHEMA_VERSION {
            tracing::warn!(
                found = record.schema_version,
                expected = LOCAL_AUTH_LIST_SCHEMA_VERSION,
                "discarding a persisted local authorization list written by an incompatible \
                 schema version"
            );
            return None;
        }
        Some(record)
    }
}

impl<S: Storage + Send + Sync> LocalAuthorizationListStore<AtomicStorage<S>> {
    /// Creates a store over `storage`, wrapped in [`AtomicStorage`] for the same reason
    /// [`TransactionStore::new_atomic`] does.
    pub fn new_atomic(storage: S) -> Self {
        LocalAuthorizationListStore::new(AtomicStorage::new(storage))
    }
}

/// Recovers the local authorization list from durable storage, if any, and hands it to the state
/// machine as one [`ChargePointEvent::PersistedLocalAuthorizationListRestored`].
///
/// Call this once at boot, before [`crate::local_authorization_list::handle_send_local_list`]/
/// `handle_get_local_list_version` can be reached by a CSMS request - see
/// `crate::builder::ChargePointBuilder::local_authorization_list_persistence`.
///
/// Returns whether a list was actually recovered.
pub async fn restore_local_authorization_list<S: Storage>(
    actor: &ChargePointActor,
    store: &LocalAuthorizationListStore<S>,
) -> bool {
    let Some(record) = store.load().await else {
        return false;
    };
    tracing::info!(
        version = record.version,
        entries = record.entries.len(),
        "recovering the local authorization list from durable storage"
    );
    let _ = actor
        .send(ChargePointEvent::PersistedLocalAuthorizationListRestored {
            version: record.version,
            entries: record.entries,
        })
        .await;
    true
}

/// Persists the local authorization list through `store` for as long as `state_changes` keeps
/// producing new values, writing whenever the list's version changes - see
/// [`LocalAuthorizationListStore`]'s docs for why no debounce is applied.
pub async fn run_local_authorization_list_persistence<S: Storage>(
    mut state_changes: WatchReceiver<ChargePointState>,
    store: &LocalAuthorizationListStore<S>,
) {
    let mut last_version: Option<i64> = None;
    loop {
        state_changes.changed().await;
        let state = state_changes.borrow();
        if last_version != Some(state.local_authorization_list.version) {
            last_version = Some(state.local_authorization_list.version);
            store
                .save(
                    state.local_authorization_list.version,
                    &state.local_authorization_list.entries,
                )
                .await;
        }
    }
}

// --- reservation persistence (E2.6, docs/PRODUCTION-ROADMAP.md §7.2) ---

/// The version stamped into every [`PersistedReservations`] record.
pub const RESERVATION_SCHEMA_VERSION: u32 = 1;

/// The key the whole set of active reservations is written under - one whole-set snapshot, not
/// one record per reservation, mirroring [`LocalAuthorizationListStore`]'s reasoning: the bounded
/// set (at most one reservation per connector) changes in small discrete steps (create/cancel/
/// used/expire), so there's no meaningful per-entry addressing to gain, and a single key keeps
/// reads and the expiry-filtering pass in [`restore_reservations`] a single round trip.
const RESERVATION_KEY: &str = "ocpp-cp/reservations";

/// One active reservation as written to durable storage.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PersistedReservationEntry {
    /// The reservation's connector's EVSE index.
    pub evse_id: usize,
    /// The reservation's connector's index within its EVSE.
    pub connector_id: usize,
    /// The reservation itself.
    pub reservation: Reservation,
}

/// The whole set of active reservations as written to durable storage.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PersistedReservations {
    /// The [`RESERVATION_SCHEMA_VERSION`] this record was written with.
    pub schema_version: u32,
    /// Every currently active reservation.
    pub reservations: Vec<PersistedReservationEntry>,
}

/// Reads and writes the whole set of active reservations through a [`Storage`].
///
/// Write policy: like [`LocalAuthorizationListStore`], every change is written unconditionally -
/// no debounce. A tick-driven expiry sweep might look like the kind of steady drumbeat
/// [`QueueStore`]'s write-threshold exists to throttle, but this crate doesn't run one (see
/// [`Reservation`]'s docs: expiry isn't enforced live by a background timer today, only checked
/// at [`restore_reservations`] time) - every write this store actually issues today comes from a
/// discrete CSMS-initiated `ReserveNow`/`CancelReservation`, or a reservation being consumed by a
/// cable connection, all of which are already rare relative to a charging session's meter
/// cadence. If a live expiry sweep is added later, it should reuse
/// [`queue_persistence_decision`]-style debouncing rather than writing on every tick - flagged
/// here rather than silently decided against, since this store's write policy would need
/// revisiting at that point.
#[derive(Debug, Clone)]
pub struct ReservationStore<S> {
    storage: S,
}

impl<S: Storage> ReservationStore<S> {
    /// Creates a store over `storage`.
    pub fn new(storage: S) -> Self {
        Self { storage }
    }

    /// Writes `reservations` as the whole active set, replacing whatever was there before -
    /// clearing storage outright if `reservations` is empty. Returns whether the write actually
    /// reached storage.
    pub async fn save(&self, reservations: &[PersistedReservationEntry]) -> bool {
        if reservations.is_empty() {
            self.clear().await;
            return true;
        }
        let record = PersistedReservations {
            schema_version: RESERVATION_SCHEMA_VERSION,
            reservations: reservations.to_vec(),
        };
        let Ok(encoded) = serde_json::to_vec(&record) else {
            tracing::error!("failed to encode the active reservation set for storage");
            return false;
        };
        match self.storage.set(RESERVATION_KEY, &encoded).await {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "failed to persist the active reservation set; continuing without \
                     durability for it"
                );
                false
            }
        }
    }

    /// Removes the stored snapshot, if any. A missing snapshot is not an error.
    pub async fn clear(&self) {
        if let Err(err) = self.storage.remove(RESERVATION_KEY).await {
            tracing::warn!(
                error = %err,
                "failed to clear the persisted active reservation set"
            );
        }
    }

    /// Reads back every persisted reservation, or an empty `Vec` if there isn't a record, it
    /// can't be read, or it was written by an incompatible [`RESERVATION_SCHEMA_VERSION`].
    pub async fn load(&self) -> Vec<PersistedReservationEntry> {
        let encoded = match self.storage.get(RESERVATION_KEY).await {
            Ok(Some(encoded)) => encoded,
            Ok(None) => return Vec::new(),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "failed to read the persisted active reservation set; treating it as absent"
                );
                return Vec::new();
            }
        };
        let record: PersistedReservations = match serde_json::from_slice(&encoded) {
            Ok(record) => record,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "a persisted active reservation set could not be decoded; discarding it"
                );
                return Vec::new();
            }
        };
        if record.schema_version != RESERVATION_SCHEMA_VERSION {
            tracing::warn!(
                found = record.schema_version,
                expected = RESERVATION_SCHEMA_VERSION,
                "discarding a persisted active reservation set written by an incompatible \
                 schema version"
            );
            return Vec::new();
        }
        record.reservations
    }
}

impl<S: Storage + Send + Sync> ReservationStore<AtomicStorage<S>> {
    /// Creates a store over `storage`, wrapped in [`AtomicStorage`] for the same reason
    /// [`TransactionStore::new_atomic`] does.
    pub fn new_atomic(storage: S) -> Self {
        ReservationStore::new(AtomicStorage::new(storage))
    }
}

/// Recovers active reservations from durable storage, filters out any whose
/// [`Reservation::expires_at`] has already passed, and hands the rest to the state machine as one
/// [`ChargePointEvent::PersistedReservationsRestored`].
///
/// # The expired-reservation decision
///
/// A reservation whose expiry passed while the charge point was powered off is **not**
/// resurrected as active: it is dropped here, logged, and never reaches
/// [`ChargePointEvent::PersistedReservationsRestored`] at all, so the state machine never has to
/// reason about an already-invalid reservation. The alternative - restoring it anyway and relying
/// on a live expiry check to immediately cancel it - was rejected because this crate doesn't
/// have a live expiry check yet (see [`Reservation`]'s and [`ReservationStore`]'s docs); silently
/// restoring an expired reservation as `Reserved` with nothing to ever clear it would leave the
/// connector wrongly unusable indefinitely, exactly the "resurrect an expired reservation as
/// active" outcome this function must not produce.
///
/// The expiry check itself only fires when `clock` looks synchronized (see
/// [`crate::clock::is_synchronized`]): hardware with no RTC yet must not have every restored
/// reservation dropped as "expired" just because its unset clock reads before any real
/// `expires_at` - the same G3.1 stance [`next_record`] already takes for `started_at`, applied
/// here as "don't discard based on a clock reading we don't trust" rather than "don't record a
/// timestamp we don't trust". A reservation with `expires_at: None` (see that field's docs on the
/// wire value not being wired through yet) is likewise never treated as expired.
///
/// Storage is reconciled to hold only the entries actually restored, once the recovery event has
/// been handed to the state machine, so a stale expired entry doesn't keep getting re-loaded and
/// re-filtered on every subsequent boot.
pub async fn restore_reservations<S: Storage, C: Clock>(
    actor: &ChargePointActor,
    store: &ReservationStore<S>,
    clock: &C,
) -> usize {
    let entries = store.load().await;
    let now = clock.now();
    let clock_trustworthy = crate::clock::is_synchronized(&now);
    let mut kept = Vec::new();
    for entry in entries {
        let expired = clock_trustworthy
            && entry
                .reservation
                .expires_at
                .is_some_and(|expires_at| now >= expires_at);
        if expired {
            tracing::warn!(
                evse_id = entry.evse_id,
                connector_id = entry.connector_id,
                "discarding a reservation that expired while the charge point was powered off"
            );
            continue;
        }
        kept.push(entry);
    }
    let recovered = kept.len();
    if recovered > 0 {
        tracing::info!(
            count = recovered,
            "recovering reservations from durable storage"
        );
    }
    let reservations = kept
        .iter()
        .map(|entry| RecoveredReservation {
            evse_id: entry.evse_id,
            connector_id: entry.connector_id,
            reservation: entry.reservation.clone(),
        })
        .collect();
    let _ = actor
        .send(ChargePointEvent::PersistedReservationsRestored { reservations })
        .await;
    store.save(&kept).await;
    recovered
}

/// Persists the active reservation set through `store` for as long as `state_changes` keeps
/// producing new values, writing whenever the set actually changes - see [`ReservationStore`]'s
/// docs for the write policy.
pub async fn run_reservation_persistence<S: Storage>(
    mut state_changes: WatchReceiver<ChargePointState>,
    store: &ReservationStore<S>,
) {
    let mut last: Vec<PersistedReservationEntry> = Vec::new();
    loop {
        state_changes.changed().await;
        let snapshot = active_reservations(&state_changes.borrow());
        if snapshot != last {
            store.save(&snapshot).await;
            last = snapshot;
        }
    }
}

/// Every currently active reservation across `state`'s EVSEs, as [`PersistedReservationEntry`]
/// records in `(evse_id, connector_id)` order.
fn active_reservations(state: &ChargePointState) -> Vec<PersistedReservationEntry> {
    let mut entries = Vec::new();
    for (evse_id, evse) in state.evses.iter().enumerate() {
        for (connector_id, reservation) in evse.reservations.iter().enumerate() {
            if let Some(reservation) = reservation {
                entries.push(PersistedReservationEntry {
                    evse_id,
                    connector_id,
                    reservation: reservation.clone(),
                });
            }
        }
    }
    entries
}

// --- device model attribute persistence (E2.3, docs/PRODUCTION-ROADMAP.md §7.2) ---

/// The version stamped into every [`PersistedDeviceModel`] record.
pub const DEVICE_MODEL_SCHEMA_VERSION: u32 = 1;

/// The key the whole set of persistent device model attribute values is written under.
const DEVICE_MODEL_KEY: &str = "ocpp-cp/device-model";

/// The default number of skipped writes between whole-snapshot writes for persistent device
/// model attributes - see [`DeviceModelStore::with_write_threshold`] for the trade-off this sets.
///
/// `1` writes on every change, same as [`LocalAuthorizationListStore`] and [`ReservationStore`].
/// Unlike those two, `SetVariables` genuinely *can* be bursty - a single CSMS request may set
/// several variables at once, and nothing stops a script issuing several `SetVariables` calls
/// back to back. That argues for a threshold the way [`QueueStore`] has one. This crate still
/// defaults to `1` rather than a higher value, though: `SetVariables` is an operator/CSMS
/// configuration action, not a hot path driven by charging telemetry (unlike periodic meter
/// values, which is the one write this crate already debounces by *magnitude* via
/// [`TransactionStore::meter_write_threshold_wh`]) - a burst of a handful of variables in one
/// request is at most a handful of small JSON snapshot writes, not the sustained per-sample
/// cadence meter values produce. Integrators who do expect frequent bulk `SetVariables` traffic
/// (e.g. a provisioning tool that reconfigures many variables in a scripted loop) can raise this
/// via [`DeviceModelStore::with_write_threshold`] and accept losing up to that many of the most
/// recent attribute writes to a power cut - the same trade [`QueueStore::with_write_threshold`]
/// documents.
pub const DEFAULT_DEVICE_MODEL_WRITE_THRESHOLD: usize = 1;

/// One persistent device model attribute value as written to durable storage.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PersistedDeviceModelAttribute {
    /// The attribute's component.
    pub component: Component,
    /// The attribute's variable.
    pub variable: Variable,
    /// Which attribute of `variable` this is.
    pub attribute_type: VariableAttributeType,
    /// The persisted value.
    pub value: String,
}

/// The whole set of persistent device model attribute values as written to durable storage - only
/// attributes flagged [`crate::state::VariableAttribute::persistent`], never the rest of the
/// model (every other variable is re-registered by the hardware binding on every boot - see
/// `crate::hardware::ChargePoint::start` - so persisting it too would just be redundant writes of
/// data that's about to be overwritten anyway).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PersistedDeviceModel {
    /// The [`DEVICE_MODEL_SCHEMA_VERSION`] this record was written with.
    pub schema_version: u32,
    /// Every currently registered attribute flagged `persistent`.
    pub attributes: Vec<PersistedDeviceModelAttribute>,
}

/// What [`run_device_model_persistence`]'s write policy concluded, as a pure function of how many
/// writes have been skipped since the last one actually reached storage. Mirrors
/// [`QueuePersistenceDecision`] without its `Clear` case - a `PersistedDeviceModel` with no
/// persistent attributes registered is a legitimate (if unusual) snapshot to write, not a signal
/// to remove the key the way an emptied offline queue is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceModelPersistenceDecision {
    /// Write a fresh whole-snapshot.
    Write,
    /// Leave storage untouched - not enough writes have been skipped yet to be worth one.
    Skip,
}

/// The write policy used by [`run_device_model_persistence`] - see
/// [`DEFAULT_DEVICE_MODEL_WRITE_THRESHOLD`] for the reasoning. `write_threshold` is clamped to at
/// least `1`, exactly like [`queue_persistence_decision`].
pub fn device_model_persistence_decision(
    mutations_since_write: usize,
    write_threshold: usize,
) -> DeviceModelPersistenceDecision {
    if mutations_since_write + 1 >= write_threshold.max(1) {
        DeviceModelPersistenceDecision::Write
    } else {
        DeviceModelPersistenceDecision::Skip
    }
}

/// Reads and writes the whole set of persistent device model attribute values through a
/// [`Storage`].
#[derive(Debug, Clone)]
pub struct DeviceModelStore<S> {
    storage: S,
    write_threshold: usize,
}

impl<S: Storage> DeviceModelStore<S> {
    /// Creates a store over `storage` with [`DEFAULT_DEVICE_MODEL_WRITE_THRESHOLD`].
    pub fn new(storage: S) -> Self {
        Self {
            storage,
            write_threshold: DEFAULT_DEVICE_MODEL_WRITE_THRESHOLD,
        }
    }

    /// Overrides how many skipped writes must accumulate between whole-snapshot writes - see
    /// [`DEFAULT_DEVICE_MODEL_WRITE_THRESHOLD`].
    pub fn with_write_threshold(mut self, write_threshold: usize) -> Self {
        self.write_threshold = write_threshold;
        self
    }

    /// The configured write threshold.
    pub fn write_threshold(&self) -> usize {
        self.write_threshold
    }

    /// Writes `attributes` as the whole persistent-attribute snapshot, replacing whatever was
    /// there before. Returns whether the write actually reached storage.
    pub async fn save(&self, attributes: &[PersistedDeviceModelAttribute]) -> bool {
        let record = PersistedDeviceModel {
            schema_version: DEVICE_MODEL_SCHEMA_VERSION,
            attributes: attributes.to_vec(),
        };
        let Ok(encoded) = serde_json::to_vec(&record) else {
            tracing::error!("failed to encode the persistent device model attributes for storage");
            return false;
        };
        match self.storage.set(DEVICE_MODEL_KEY, &encoded).await {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "failed to persist device model attribute values; continuing without \
                     durability for them"
                );
                false
            }
        }
    }

    /// Reads back every persisted attribute, or an empty `Vec` if there isn't a record, it can't
    /// be read, or it was written by an incompatible [`DEVICE_MODEL_SCHEMA_VERSION`].
    pub async fn load(&self) -> Vec<PersistedDeviceModelAttribute> {
        let encoded = match self.storage.get(DEVICE_MODEL_KEY).await {
            Ok(Some(encoded)) => encoded,
            Ok(None) => return Vec::new(),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "failed to read the persisted device model attribute values; treating them \
                     as absent"
                );
                return Vec::new();
            }
        };
        let record: PersistedDeviceModel = match serde_json::from_slice(&encoded) {
            Ok(record) => record,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "persisted device model attribute values could not be decoded; discarding \
                     them"
                );
                return Vec::new();
            }
        };
        if record.schema_version != DEVICE_MODEL_SCHEMA_VERSION {
            tracing::warn!(
                found = record.schema_version,
                expected = DEVICE_MODEL_SCHEMA_VERSION,
                "discarding persisted device model attribute values written by an incompatible \
                 schema version"
            );
            return Vec::new();
        }
        record.attributes
    }
}

impl<S: Storage + Send + Sync> DeviceModelStore<AtomicStorage<S>> {
    /// Creates a store over `storage`, wrapped in [`AtomicStorage`] for the same reason
    /// [`TransactionStore::new_atomic`] does.
    pub fn new_atomic(storage: S) -> Self {
        DeviceModelStore::new(AtomicStorage::new(storage))
    }
}

/// Recovers persistent device model attribute values from durable storage and hands them to the
/// state machine as one [`ChargePointEvent::PersistedDeviceModelAttributesRestored`].
///
/// # Ordering vs the hardware binding's own registration
///
/// Call this **after** the hardware binding has finished registering its own variables (i.e.
/// after `crate::hardware::ChargePoint::start` has returned, the same point
/// `crate::builder::ChargePointBuilder::start` already waits for) - never before. The device
/// model's registered set is empty until the binding declares it, and
/// [`ChargePointEvent::PersistedDeviceModelAttributesRestored`] only applies a persisted value
/// onto a component/variable/attribute-type that's already registered (see that variant's docs) -
/// restoring first against an empty model would discard every persisted value as "unregistered",
/// which is exactly wrong on every normal boot. Restoring after registration means a variable the
/// binding still declares this boot gets its persisted value back; one it no longer declares is
/// left dormant and logged rather than silently applied to a variable that, as far as this boot's
/// hardware is concerned, doesn't exist.
///
/// Returns the number of attribute records read back from storage (not the number actually
/// applied - some may be left dormant per the above).
pub async fn restore_device_model<S: Storage>(
    actor: &ChargePointActor,
    store: &DeviceModelStore<S>,
) -> usize {
    let attributes = store.load().await;
    let recovered = attributes.len();
    if recovered > 0 {
        tracing::info!(
            count = recovered,
            "recovering persistent device model attribute values from durable storage"
        );
    }
    let attributes = attributes
        .into_iter()
        .map(|attribute| RecoveredDeviceModelAttribute {
            component: attribute.component,
            variable: attribute.variable,
            attribute_type: attribute.attribute_type,
            value: attribute.value,
        })
        .collect();
    let _ = actor
        .send(ChargePointEvent::PersistedDeviceModelAttributesRestored { attributes })
        .await;
    recovered
}

/// Persists every currently registered `persistent`-flagged device model attribute through
/// `store` for as long as `state_changes` keeps producing new values, applying
/// [`device_model_persistence_decision`]'s write policy.
pub async fn run_device_model_persistence<S: Storage>(
    mut state_changes: WatchReceiver<ChargePointState>,
    store: &DeviceModelStore<S>,
) {
    let mut last_written: Vec<PersistedDeviceModelAttribute> = store.load().await;
    let mut mutations_since_write: usize = 0;
    loop {
        state_changes.changed().await;
        let snapshot = persistent_attributes(&state_changes.borrow());
        if snapshot == last_written {
            continue;
        }
        match device_model_persistence_decision(mutations_since_write, store.write_threshold()) {
            DeviceModelPersistenceDecision::Write => {
                store.save(&snapshot).await;
                last_written = snapshot;
                mutations_since_write = 0;
            }
            DeviceModelPersistenceDecision::Skip => {
                mutations_since_write += 1;
            }
        }
    }
}

/// Every currently registered attribute flagged `persistent` across `state`'s device model, as
/// [`PersistedDeviceModelAttribute`] records in the device model's own stable iteration order.
fn persistent_attributes(state: &ChargePointState) -> Vec<PersistedDeviceModelAttribute> {
    state
        .device_model
        .iter()
        .flat_map(|(component, variable, definition)| {
            definition
                .attributes
                .iter()
                .filter(|attribute| attribute.persistent)
                .map(move |attribute| PersistedDeviceModelAttribute {
                    component: component.clone(),
                    variable: variable.clone(),
                    attribute_type: attribute.attribute_type,
                    value: attribute.value.clone(),
                })
        })
        .collect()
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    use crate::clock::SystemClock;
    use crate::hardware::{InMemoryStorage, NoStorage};
    use crate::state::{IdToken, IdTokenKind, StopReason, TransactionChargingState, TransactionId};
    use chrono::Duration as ChronoDuration;

    /// A [`Clock`] that always reads a fixed, caller-chosen instant - used to simulate both a
    /// synchronized clock (any plausible date) and an unset, no-RTC clock
    /// ([`crate::clock::unsynchronized_before`] or earlier).
    struct FixedClock(DateTime<Utc>);

    impl Clock for FixedClock {
        fn now(&self) -> DateTime<Utc> {
            self.0
        }
    }

    fn test_transaction(energy_wh: Option<i64>) -> Transaction {
        Transaction {
            id: TransactionId(3),
            id_token: Some(IdToken {
                value: "04A224B2".into(),
                kind: IdTokenKind::ISO14443,
            }),
            charging_state: TransactionChargingState::Charging,
            stop_reason: None,
            seq_no: 5,
            last_meter_sample: energy_wh.map(|energy_wh| MeterSample {
                energy_wh,
                ..Default::default()
            }),
        }
    }

    fn occurred(kind: TransactionEventKind, energy_wh: Option<i64>) -> TransactionEventOccurred {
        TransactionEventOccurred {
            evse_id: 0,
            connector_id: 0,
            kind,
            transaction: test_transaction(energy_wh),
        }
    }

    fn record(energy_wh: Option<i64>) -> PersistedTransaction {
        PersistedTransaction {
            schema_version: SCHEMA_VERSION,
            evse_id: 0,
            connector_id: 0,
            transaction: test_transaction(energy_wh),
            started_at: None,
            meter_start: None,
        }
    }

    #[test]
    fn a_started_transaction_is_always_written() {
        assert_eq!(
            persistence_decision(None, &occurred(TransactionEventKind::Started, None), 100),
            PersistenceDecision::Write
        );
    }

    #[test]
    fn an_ended_transaction_clears_its_record() {
        assert_eq!(
            persistence_decision(
                Some(&record(Some(0))),
                &occurred(TransactionEventKind::Ended, Some(500)),
                100
            ),
            PersistenceDecision::Clear
        );
    }

    #[test]
    fn a_charging_state_change_is_always_written() {
        assert_eq!(
            persistence_decision(
                Some(&record(Some(500))),
                &occurred(
                    TransactionEventKind::Updated(TransactionUpdateReason::ChargingStateChanged),
                    Some(500)
                ),
                100
            ),
            PersistenceDecision::Write
        );
    }

    #[test]
    fn a_meter_reading_below_the_write_threshold_is_not_written_to_flash() {
        assert_eq!(
            persistence_decision(
                Some(&record(Some(500))),
                &occurred(
                    TransactionEventKind::Updated(TransactionUpdateReason::MeterValuePeriodic),
                    Some(599)
                ),
                100
            ),
            PersistenceDecision::Skip
        );
    }

    #[test]
    fn a_meter_reading_at_or_above_the_write_threshold_is_written() {
        assert_eq!(
            persistence_decision(
                Some(&record(Some(500))),
                &occurred(
                    TransactionEventKind::Updated(TransactionUpdateReason::MeterValuePeriodic),
                    Some(600)
                ),
                100
            ),
            PersistenceDecision::Write
        );
    }

    #[test]
    fn a_meter_reading_with_nothing_yet_on_record_is_written_regardless_of_the_threshold() {
        assert_eq!(
            persistence_decision(
                None,
                &occurred(
                    TransactionEventKind::Updated(TransactionUpdateReason::MeterValuePeriodic),
                    Some(1)
                ),
                1_000_000
            ),
            PersistenceDecision::Write
        );
    }

    #[test]
    fn a_started_transaction_records_its_start_time_from_a_synchronized_clock() {
        let clock = FixedClock(crate::clock::unsynchronized_before() + ChronoDuration::days(1));
        let record = next_record(None, &occurred(TransactionEventKind::Started, None), &clock);

        assert_eq!(record.started_at, Some(clock.0));
    }

    /// G3.1: hardware with no RTC (`Clock::now()` reads before
    /// `crate::clock::unsynchronized_before()`, e.g. an unset RTC's Unix-epoch default - see
    /// `crate::clock`'s docs) must not have a fabricated, plausible-looking start time invented
    /// for it. `started_at` stays `None` - honest "unknown" - rather than recording 1970.
    #[test]
    fn a_started_transaction_on_an_unsynchronized_clock_records_no_start_time() {
        let unset_rtc = FixedClock(DateTime::<Utc>::from_timestamp(0, 0).unwrap());
        let record = next_record(
            None,
            &occurred(TransactionEventKind::Started, None),
            &unset_rtc,
        );

        assert_eq!(record.started_at, None);
    }

    /// The transaction is still fully recordable with no RTC at all (G3.1's explicit
    /// requirement) - only `started_at` is left blank; everything else needed to recover and
    /// bill the transaction is still written.
    #[test]
    fn a_transaction_on_an_unsynchronized_clock_is_still_otherwise_recorded() {
        let unset_rtc = FixedClock(DateTime::<Utc>::from_timestamp(0, 0).unwrap());
        let event = occurred(TransactionEventKind::Started, Some(500));
        let record = next_record(None, &event, &unset_rtc);

        assert_eq!(record.started_at, None);
        assert_eq!(record.transaction, event.transaction);
        assert_eq!(
            record.meter_start,
            Some(MeterSample {
                energy_wh: 500,
                ..Default::default()
            })
        );
    }

    /// The sentinel value itself counts as synchronized (matching
    /// [`crate::clock::is_synchronized`]'s own boundary), so a clock that has just barely
    /// crossed into plausible territory is trusted, not treated as still-unset.
    #[test]
    fn a_started_transaction_at_exactly_the_sentinel_records_its_start_time() {
        let clock = FixedClock(crate::clock::unsynchronized_before());
        let record = next_record(None, &occurred(TransactionEventKind::Started, None), &clock);

        assert_eq!(record.started_at, Some(clock.0));
    }

    #[tokio::test]
    async fn a_record_round_trips_through_storage() {
        let store = TransactionStore::new(InMemoryStorage::new());
        assert_eq!(store.load(0, 0).await, None);

        assert!(store.save(&record(Some(500))).await);
        assert_eq!(store.load(0, 0).await, Some(record(Some(500))));

        store.clear(0, 0).await;
        assert_eq!(store.load(0, 0).await, None);
    }

    #[tokio::test]
    async fn records_are_keyed_per_connector() {
        let store = TransactionStore::new(InMemoryStorage::new());
        let mut other = record(Some(500));
        other.evse_id = 1;
        other.connector_id = 2;

        store.save(&record(Some(500))).await;
        store.save(&other).await;

        assert_eq!(
            store.load_all(&[1, 3]).await,
            alloc::vec![record(Some(500)), other]
        );
    }

    #[tokio::test]
    async fn a_record_from_an_incompatible_schema_version_is_discarded_rather_than_guessed_at() {
        let storage = InMemoryStorage::new();
        let mut stale = record(Some(500));
        stale.schema_version = SCHEMA_VERSION + 1;
        storage
            .set(&transaction_key(0, 0), &serde_json::to_vec(&stale).unwrap())
            .await
            .unwrap();

        let store = TransactionStore::new(storage);
        assert_eq!(store.load(0, 0).await, None);
    }

    #[tokio::test]
    async fn a_corrupt_record_is_discarded_rather_than_panicking() {
        let storage = InMemoryStorage::new();
        storage
            .set(&transaction_key(0, 0), b"{ half-written")
            .await
            .unwrap();

        let store = TransactionStore::new(storage);
        assert_eq!(store.load(0, 0).await, None);
    }

    #[tokio::test]
    async fn the_transaction_id_counter_round_trips_and_defaults_to_zero() {
        let store = TransactionStore::new(InMemoryStorage::new());
        assert_eq!(store.load_next_transaction_id().await, 0);

        store.save_next_transaction_id(42).await;
        assert_eq!(store.load_next_transaction_id().await, 42);
    }

    #[tokio::test]
    async fn a_charge_point_without_storage_persists_and_recovers_nothing() {
        let store = TransactionStore::new(NoStorage);

        assert!(store.save(&record(Some(500))).await);
        assert_eq!(store.load(0, 0).await, None);
        assert_eq!(store.load_all(&[1]).await, Vec::new());
    }

    /// The end-to-end guarantee this whole module exists for: energy delivered before a power cut
    /// is still reported to the CSMS after the reboot.
    #[tokio::test]
    async fn a_transaction_interrupted_by_a_power_cut_is_closed_out_with_its_energy_intact() {
        let storage = alloc::sync::Arc::new(InMemoryStorage::new());
        let store = TransactionStore::new(storage.clone());

        // --- before the cut: a transaction starts and delivers energy.
        let executor = crate::executor::TokioExecutor;
        let before = ChargePointActor::spawn([1], &executor);
        let events = before.subscribe_transaction_events();
        let persistence_store = TransactionStore::new(storage.clone());
        tokio::spawn(async move {
            run_transaction_persistence(events, &persistence_store, &SystemClock).await;
        });
        drive_a_charging_transaction(&before, 4_200).await;
        // Let the persistence task drain the events the actor just published.
        tokio::task::yield_now().await;
        for _ in 0..10 {
            if store.load(0, 0).await.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let persisted = store.load(0, 0).await.expect("the in-flight transaction");
        assert_eq!(
            persisted.transaction.last_meter_sample.map(|s| s.energy_wh),
            Some(4_200)
        );
        assert!(persisted.started_at.is_some());

        // --- the cut: the actor (and all its RAM state) simply vanishes.
        drop(before);

        // --- after the reboot: a fresh charge point recovers from storage alone.
        let after = ChargePointActor::spawn([1], &executor);
        let mut recovered_events = after.subscribe_transaction_events();
        assert_eq!(restore_transactions(&after, &store).await, 1);

        let reported = recovered_events.recv().await.expect("a closing event");
        assert_eq!(reported.kind, TransactionEventKind::Ended);
        assert_eq!(
            reported.transaction.stop_reason,
            Some(StopReason::PowerLoss)
        );
        assert_eq!(
            reported
                .transaction
                .last_meter_sample
                .map(|sample| sample.energy_wh),
            Some(4_200),
            "the energy delivered before the cut must still be billable"
        );
        // The id counter came back too, so the next session can't reuse the interrupted id.
        assert!(after.state().next_transaction_id > reported.transaction.id.0);
        // And the record is gone, so a second reboot doesn't report the same session again.
        assert_eq!(store.load(0, 0).await, None);
    }

    #[tokio::test]
    async fn recovering_from_an_empty_store_is_a_no_op() {
        let executor = crate::executor::TokioExecutor;
        let actor = ChargePointActor::spawn([1], &executor);
        let store = TransactionStore::new(InMemoryStorage::new());

        assert_eq!(restore_transactions(&actor, &store).await, 0);
    }

    /// Drives connector 0 of `actor` into a live charging transaction reporting `energy_wh`.
    async fn drive_a_charging_transaction(actor: &ChargePointActor, energy_wh: i64) {
        use crate::state::{ConnectorEvent, EvseEvent};
        let id_token = IdToken {
            value: "04A224B2".into(),
            kind: IdTokenKind::ISO14443,
        };
        for event in [
            ConnectorEvent::CableConnected,
            ConnectorEvent::LockConfirmed,
            ConnectorEvent::IdTokenPresented(id_token.clone()),
            ConnectorEvent::ChargingAuthorized(id_token),
            ConnectorEvent::ContactorClosed,
            ConnectorEvent::MeterValueSampled(MeterSample {
                energy_wh,
                ..Default::default()
            }),
        ] {
            let _ = actor
                .send(ChargePointEvent::Evse {
                    evse_id: 0,
                    event: EvseEvent::Connector {
                        connector_id: 0,
                        event,
                    },
                })
                .await;
        }
    }

    // --- offline-queue persistence (E2/E4.3) ---

    #[test]
    fn the_first_message_queued_during_an_outage_is_always_written_immediately() {
        assert_eq!(
            queue_persistence_decision(0, 1, 0, 100),
            QueuePersistenceDecision::Write
        );
    }

    #[test]
    fn the_queue_draining_back_to_empty_always_clears_storage() {
        assert_eq!(
            queue_persistence_decision(1, 0, 0, 100),
            QueuePersistenceDecision::Clear
        );
    }

    #[test]
    fn a_mutation_below_the_write_threshold_is_skipped() {
        assert_eq!(
            queue_persistence_decision(3, 4, 0, 5),
            QueuePersistenceDecision::Skip
        );
    }

    #[test]
    fn a_mutation_reaching_the_write_threshold_is_written() {
        assert_eq!(
            queue_persistence_decision(3, 4, 4, 5),
            QueuePersistenceDecision::Write
        );
    }

    #[test]
    fn no_length_change_is_always_skipped_regardless_of_threshold() {
        assert_eq!(
            queue_persistence_decision(5, 5, 999, 1),
            QueuePersistenceDecision::Skip
        );
    }

    #[test]
    fn a_zero_write_threshold_behaves_like_one() {
        assert_eq!(
            queue_persistence_decision(3, 4, 0, 0),
            QueuePersistenceDecision::Write
        );
    }

    #[tokio::test]
    async fn a_queue_snapshot_round_trips_through_storage() {
        let store = QueueStore::new(InMemoryStorage::new(), "test");
        assert_eq!(store.load::<i32>().await, Vec::<i32>::new());

        store.save(&[1, 2, 3]).await;
        assert_eq!(store.load::<i32>().await, alloc::vec![1, 2, 3]);

        store.clear().await;
        assert_eq!(store.load::<i32>().await, Vec::<i32>::new());
    }

    #[tokio::test]
    async fn a_queue_snapshot_from_an_incompatible_schema_version_is_discarded() {
        let storage = InMemoryStorage::new();
        storage
            .set(
                "ocpp-cp/queue/test",
                &serde_json::to_vec(&PersistedQueue {
                    schema_version: QUEUE_SCHEMA_VERSION + 1,
                    messages: alloc::vec![1, 2, 3],
                })
                .unwrap(),
            )
            .await
            .unwrap();

        let store = QueueStore::new(storage, "test");
        assert_eq!(store.load::<i32>().await, Vec::<i32>::new());
    }

    #[tokio::test]
    async fn a_corrupt_queue_snapshot_is_discarded_rather_than_panicking() {
        let storage = InMemoryStorage::new();
        storage
            .set("ocpp-cp/queue/test", b"{ half-written")
            .await
            .unwrap();

        let store = QueueStore::new(storage, "test");
        assert_eq!(store.load::<i32>().await, Vec::<i32>::new());
    }

    #[tokio::test]
    async fn restoring_an_empty_store_is_a_no_op() {
        use crate::offline_queue::OfflineQueue;

        let queue: OfflineQueue<i32> = OfflineQueue::new();
        let store = QueueStore::new(InMemoryStorage::new(), "test");
        assert_eq!(
            restore_offline_queue::<i32, i32, _>(&queue, &store).await,
            0
        );
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn restoring_a_backlog_replays_it_in_order_and_respects_capacity() {
        use crate::offline_queue::OfflineQueue;

        let store = QueueStore::new(InMemoryStorage::new(), "test");
        store.save(&[1, 2, 3]).await;

        // Capacity 2: the restored backlog of 3 doesn't fit, so DropOldest evicts 1.
        let queue: OfflineQueue<i32> = OfflineQueue::with_capacity(2);
        assert_eq!(
            restore_offline_queue::<i32, i32, _>(&queue, &store).await,
            3
        );
        assert_eq!(queue.snapshot(), alloc::vec![2, 3]);
    }

    /// The end-to-end guarantee E4.3 exists for: a message queued while offline survives a power
    /// cut and is replayed, in order, once the process restarts and the connection recovers.
    #[tokio::test]
    async fn a_queue_interrupted_by_a_power_cut_replays_its_backlog_in_order_after_reboot() {
        use crate::offline_queue::OfflineQueue;
        use crate::sync::broadcast_channel;

        let storage = alloc::sync::Arc::new(InMemoryStorage::new());
        let store = QueueStore::new(storage.clone(), "test");

        // --- before the cut: two messages queued while "offline" (every send fails).
        let queue: OfflineQueue<i32> = OfflineQueue::new();
        let sender = broadcast_channel();
        let receiver = sender.subscribe();
        let forwarder = tokio::spawn(async move {
            run_persisted_offline_queue::<i32, i32, _, _, _, _, _, _>(
                receiver,
                &queue,
                &store,
                |_message: i32| async { Err::<(), _>(TestSendError) },
                |_dropped| async {},
            )
            .await;
        });
        sender.send(1);
        sender.send(2);
        // Let the persisted forwarder task process both pushes.
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        drop(sender);
        forwarder.await.unwrap();

        // --- the cut: nothing but `storage` survives.
        let persisted_store = QueueStore::new(storage.clone(), "test");
        assert_eq!(persisted_store.load::<i32>().await, alloc::vec![1, 2]);

        // --- after the reboot: a fresh queue restores the backlog, in order, and delivers it once
        // the connection is back up.
        let restored_queue: OfflineQueue<i32> = OfflineQueue::new();
        assert_eq!(
            restore_offline_queue::<i32, i32, _>(&restored_queue, &persisted_store).await,
            2
        );
        let delivered = alloc::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let flush_delivered = delivered.clone();
        flush_offline_queue(&restored_queue, move |message: i32| {
            let delivered = flush_delivered.clone();
            async move {
                delivered.lock().unwrap().push(message);
                Ok::<(), TestSendError>(())
            }
        })
        .await;

        assert_eq!(*delivered.lock().unwrap(), alloc::vec![1, 2]);
    }

    #[derive(Debug)]
    struct TestSendError;

    impl core::fmt::Display for TestSendError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("send failed")
        }
    }

    // --- local authorization list persistence (E2.4) ---

    use crate::state::{
        AuthorizationStatus, Component, ConnectorEvent, ConnectorState, DeviceModelEvent,
        EvseEvent, ReservationId, Variable, VariableCharacteristics, VariableDataType,
        VariableMutability,
    };

    fn local_list_entry() -> LocalListEntry {
        LocalListEntry {
            id_token: IdToken {
                value: "04A224B2".into(),
                kind: IdTokenKind::ISO14443,
            },
            status: AuthorizationStatus::Accepted,
        }
    }

    #[tokio::test]
    async fn a_local_authorization_list_round_trips_through_storage() {
        let store = LocalAuthorizationListStore::new(InMemoryStorage::new());
        assert_eq!(store.load().await, None);

        assert!(store.save(3, &[local_list_entry()]).await);
        let loaded = store.load().await.unwrap();
        assert_eq!(loaded.version, 3);
        assert_eq!(loaded.entries, alloc::vec![local_list_entry()]);
    }

    #[tokio::test]
    async fn a_local_authorization_list_from_an_incompatible_schema_version_is_discarded() {
        let storage = InMemoryStorage::new();
        storage
            .set(
                LOCAL_AUTH_LIST_KEY,
                &serde_json::to_vec(&PersistedLocalAuthorizationList {
                    schema_version: LOCAL_AUTH_LIST_SCHEMA_VERSION + 1,
                    version: 1,
                    entries: alloc::vec![local_list_entry()],
                })
                .unwrap(),
            )
            .await
            .unwrap();

        let store = LocalAuthorizationListStore::new(storage);
        assert_eq!(store.load().await, None);
    }

    /// The end-to-end guarantee E2.4 exists for: a local authorization list already installed by
    /// `SendLocalList` survives a power cut, without re-downloading it from the CSMS.
    #[tokio::test]
    async fn a_local_authorization_list_interrupted_by_a_power_cut_is_recovered_after_reboot() {
        let storage = alloc::sync::Arc::new(InMemoryStorage::new());
        let store = LocalAuthorizationListStore::new(storage.clone());

        let executor = crate::executor::TokioExecutor;
        let before = ChargePointActor::spawn([1], &executor);
        let state_changes = before.subscribe();
        let persistence_store = LocalAuthorizationListStore::new(storage.clone());
        tokio::spawn(async move {
            run_local_authorization_list_persistence(state_changes, &persistence_store).await;
        });
        let _ = before
            .send(ChargePointEvent::LocalListUpdated {
                version: 7,
                entries: alloc::vec![local_list_entry()],
            })
            .await;
        for _ in 0..20 {
            if store.load().await.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(store.load().await.unwrap().version, 7);

        // --- the cut: the actor (and all its RAM state) simply vanishes.
        drop(before);

        // --- after the reboot: a fresh charge point recovers from storage alone.
        let after = ChargePointActor::spawn([1], &executor);
        assert!(restore_local_authorization_list(&after, &store).await);
        assert_eq!(after.state().local_authorization_list.version, 7);
        assert_eq!(
            after.state().local_authorization_list.entries,
            alloc::vec![local_list_entry()]
        );
    }

    // --- reservation persistence (E2.6) ---

    fn test_id_token() -> IdToken {
        IdToken {
            value: "04A224B2".into(),
            kind: IdTokenKind::ISO14443,
        }
    }

    fn reservation_entry(expires_at: Option<DateTime<Utc>>) -> PersistedReservationEntry {
        PersistedReservationEntry {
            evse_id: 0,
            connector_id: 0,
            reservation: Reservation {
                id: ReservationId(1),
                id_token: test_id_token(),
                expires_at,
            },
        }
    }

    #[tokio::test]
    async fn a_reservation_set_round_trips_through_storage() {
        let store = ReservationStore::new(InMemoryStorage::new());
        assert_eq!(store.load().await, Vec::new());

        assert!(store.save(&[reservation_entry(None)]).await);
        assert_eq!(store.load().await, alloc::vec![reservation_entry(None)]);

        assert!(store.save(&[]).await);
        assert_eq!(store.load().await, Vec::new());
    }

    #[tokio::test]
    async fn a_reservation_set_from_an_incompatible_schema_version_is_discarded() {
        let storage = InMemoryStorage::new();
        storage
            .set(
                RESERVATION_KEY,
                &serde_json::to_vec(&PersistedReservations {
                    schema_version: RESERVATION_SCHEMA_VERSION + 1,
                    reservations: alloc::vec![reservation_entry(None)],
                })
                .unwrap(),
            )
            .await
            .unwrap();

        let store = ReservationStore::new(storage);
        assert_eq!(store.load().await, Vec::new());
    }

    /// The expired-reservation decision: a reservation whose `expires_at` has already passed
    /// while the charge point was off is dropped, not restored as active.
    #[tokio::test]
    async fn restoring_an_expired_reservation_discards_it_rather_than_reactivating_it() {
        let storage = InMemoryStorage::new();
        let store = ReservationStore::new(storage);
        let synchronized_now =
            crate::clock::unsynchronized_before() + ChronoDuration::days(365 * 5);
        let already_expired = synchronized_now - ChronoDuration::hours(1);
        store
            .save(&[reservation_entry(Some(already_expired))])
            .await;

        let executor = crate::executor::TokioExecutor;
        let actor = ChargePointActor::spawn([1], &executor);
        let clock = FixedClock(synchronized_now);

        let recovered = restore_reservations(&actor, &store, &clock).await;

        assert_eq!(recovered, 0);
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Available,
            "an expired reservation must not resurrect the connector as Reserved"
        );
        // Storage is reconciled too, so a later boot doesn't keep re-discovering the same
        // expired entry.
        assert_eq!(store.load().await, Vec::new());
    }

    /// A reservation that hasn't expired yet is restored as an active `Reserved` connector.
    #[tokio::test]
    async fn restoring_an_unexpired_reservation_reactivates_the_connector() {
        let storage = InMemoryStorage::new();
        let store = ReservationStore::new(storage);
        let synchronized_now =
            crate::clock::unsynchronized_before() + ChronoDuration::days(365 * 5);
        let not_yet_expired = synchronized_now + ChronoDuration::hours(1);
        store
            .save(&[reservation_entry(Some(not_yet_expired))])
            .await;

        let executor = crate::executor::TokioExecutor;
        let actor = ChargePointActor::spawn([1], &executor);
        let clock = FixedClock(synchronized_now);

        let recovered = restore_reservations(&actor, &store, &clock).await;

        assert_eq!(recovered, 1);
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Reserved
        );
    }

    /// An unsynchronized clock (no RTC yet) must not cause every restored reservation to look
    /// expired - the same G3.1 "don't act on a clock reading we don't trust" stance
    /// `next_record` already takes for `started_at`.
    #[tokio::test]
    async fn restoring_a_reservation_with_an_unsynchronized_clock_does_not_discard_it_as_expired() {
        let storage = InMemoryStorage::new();
        let store = ReservationStore::new(storage);
        // An expiry far in the "past" relative to a synchronized clock, but the clock reading at
        // restore time is itself unsynchronized (before the sentinel) - so this must not be
        // treated as expired.
        let plausible_expiry =
            crate::clock::unsynchronized_before() + ChronoDuration::days(365 * 5);
        store
            .save(&[reservation_entry(Some(plausible_expiry))])
            .await;

        let executor = crate::executor::TokioExecutor;
        let actor = ChargePointActor::spawn([1], &executor);
        let unset_rtc = FixedClock(DateTime::<Utc>::from_timestamp(0, 0).unwrap());

        let recovered = restore_reservations(&actor, &store, &unset_rtc).await;

        assert_eq!(recovered, 1);
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Reserved
        );
    }

    /// The end-to-end guarantee E2.6 exists for: an active reservation survives a power cut and
    /// is restored, so the reserved connector doesn't come back `Available` for anyone else to
    /// grab.
    #[tokio::test]
    async fn a_reservation_interrupted_by_a_power_cut_is_recovered_after_reboot() {
        let storage = alloc::sync::Arc::new(InMemoryStorage::new());
        let store = ReservationStore::new(storage.clone());

        let executor = crate::executor::TokioExecutor;
        let before = ChargePointActor::spawn([1], &executor);
        let state_changes = before.subscribe();
        let persistence_store = ReservationStore::new(storage.clone());
        tokio::spawn(async move {
            run_reservation_persistence(state_changes, &persistence_store).await;
        });
        let _ = before
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::Reserved(Reservation {
                        id: ReservationId(9),
                        id_token: test_id_token(),
                        expires_at: None,
                    }),
                },
            })
            .await;
        for _ in 0..20 {
            if !store.load().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(store.load().await.len(), 1);

        // --- the cut.
        drop(before);

        // --- after the reboot.
        let after = ChargePointActor::spawn([1], &executor);
        let clock = SystemClock;
        let recovered = restore_reservations(&after, &store, &clock).await;

        assert_eq!(recovered, 1);
        assert_eq!(
            after.state().evses[0].connectors[0],
            ConnectorState::Reserved
        );
    }

    // --- device model attribute persistence (E2.3) ---

    fn persistent_component() -> Component {
        Component {
            name: "TestCtrlr".into(),
            instance: None,
            evse: None,
        }
    }

    fn persistent_variable() -> Variable {
        Variable {
            name: "Setting".into(),
            instance: None,
        }
    }

    fn persisted_attribute(value: &str) -> PersistedDeviceModelAttribute {
        PersistedDeviceModelAttribute {
            component: persistent_component(),
            variable: persistent_variable(),
            attribute_type: VariableAttributeType::Actual,
            value: value.into(),
        }
    }

    #[test]
    fn a_write_below_the_threshold_is_skipped() {
        assert_eq!(
            device_model_persistence_decision(2, 5),
            DeviceModelPersistenceDecision::Skip
        );
    }

    #[test]
    fn a_write_reaching_the_threshold_is_written() {
        assert_eq!(
            device_model_persistence_decision(4, 5),
            DeviceModelPersistenceDecision::Write
        );
    }

    #[test]
    fn a_zero_threshold_behaves_like_one() {
        assert_eq!(
            device_model_persistence_decision(0, 0),
            DeviceModelPersistenceDecision::Write
        );
    }

    #[tokio::test]
    async fn a_device_model_snapshot_round_trips_through_storage() {
        let store = DeviceModelStore::new(InMemoryStorage::new());
        assert_eq!(store.load().await, Vec::new());

        assert!(store.save(&[persisted_attribute("hello")]).await);
        assert_eq!(
            store.load().await,
            alloc::vec![persisted_attribute("hello")]
        );
    }

    #[tokio::test]
    async fn a_device_model_snapshot_from_an_incompatible_schema_version_is_discarded() {
        let storage = InMemoryStorage::new();
        storage
            .set(
                DEVICE_MODEL_KEY,
                &serde_json::to_vec(&PersistedDeviceModel {
                    schema_version: DEVICE_MODEL_SCHEMA_VERSION + 1,
                    attributes: alloc::vec![persisted_attribute("hello")],
                })
                .unwrap(),
            )
            .await
            .unwrap();

        let store = DeviceModelStore::new(storage);
        assert_eq!(store.load().await, Vec::new());
    }

    /// The unregistered-variable decision: a persisted value for a variable the hardware binding
    /// did not re-register this boot is left dormant, not applied.
    #[tokio::test]
    async fn a_persisted_attribute_for_an_unregistered_variable_is_left_dormant() {
        let storage = InMemoryStorage::new();
        let store = DeviceModelStore::new(storage);
        store.save(&[persisted_attribute("restored-value")]).await;

        let executor = crate::executor::TokioExecutor;
        // A fresh actor whose device model never registered `TestCtrlr`/`Setting` this boot.
        let actor = ChargePointActor::spawn([1], &executor);

        let recovered = restore_device_model(&actor, &store).await;

        assert_eq!(recovered, 1, "the record was read back from storage");
        assert_eq!(
            actor
                .state()
                .device_model
                .get(&persistent_component(), &persistent_variable()),
            None,
            "a variable the binding never registered this boot must not appear in the model"
        );
    }

    /// The end-to-end guarantee E2.3 exists for: a `persistent`-flagged attribute's value
    /// survives a power cut, applied back onto the variable once the hardware binding has
    /// re-registered it this boot.
    #[tokio::test]
    async fn a_persistent_device_model_attribute_interrupted_by_a_power_cut_is_recovered_after_reboot()
     {
        let storage = alloc::sync::Arc::new(InMemoryStorage::new());
        let store = DeviceModelStore::new(storage.clone());

        let executor = crate::executor::TokioExecutor;
        let before = ChargePointActor::spawn([1], &executor);
        register_persistent_variable(&before).await;
        let state_changes = before.subscribe();
        let persistence_store = DeviceModelStore::new(storage.clone());
        tokio::spawn(async move {
            run_device_model_persistence(state_changes, &persistence_store).await;
        });
        let _ = before
            .send(ChargePointEvent::DeviceModel(
                DeviceModelEvent::AttributeValueSet {
                    component: persistent_component(),
                    variable: persistent_variable(),
                    attribute_type: VariableAttributeType::Actual,
                    value: "42".into(),
                },
            ))
            .await;
        for _ in 0..20 {
            if !store.load().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            store.load().await.contains(&persisted_attribute("42")),
            "the newly-set persistent attribute must be in the snapshot, alongside this crate's \
             own built-in persistent defaults (OCPPCommCtrlr/HeartbeatInterval, \
             AuthCtrlr/AuthorizeRemoteStart)"
        );

        // --- the cut.
        drop(before);

        // --- after the reboot: the binding re-registers the variable (as `crate::hardware`
        // bindings do on every boot) *before* the persisted value is restored onto it.
        let after = ChargePointActor::spawn([1], &executor);
        register_persistent_variable(&after).await;
        let recovered = restore_device_model(&after, &store).await;

        assert_eq!(
            recovered, 3,
            "the test attribute plus this crate's own built-in persistent defaults"
        );
        assert_eq!(
            after
                .state()
                .device_model
                .get(&persistent_component(), &persistent_variable())
                .unwrap()
                .attribute(VariableAttributeType::Actual)
                .unwrap()
                .value,
            "42"
        );
    }

    /// Registers `persistent_component()`/`persistent_variable()` on `actor`'s device model,
    /// flagged `persistent: true` - simulating what a hardware binding does during
    /// `ChargePoint::start`.
    async fn register_persistent_variable(actor: &ChargePointActor) {
        let _ = actor
            .send(ChargePointEvent::DeviceModel(
                DeviceModelEvent::VariableRegistered {
                    component: persistent_component(),
                    variable: persistent_variable(),
                    characteristics: VariableCharacteristics {
                        data_type: VariableDataType::String,
                        unit: None,
                        min_limit: None,
                        max_limit: None,
                        values_list: None,
                        supports_monitoring: false,
                    },
                    attributes: alloc::vec![crate::state::VariableAttribute {
                        attribute_type: VariableAttributeType::Actual,
                        value: "0".into(),
                        mutability: VariableMutability::ReadWrite,
                        persistent: true,
                        constant: false,
                        requires_reboot: false,
                    }],
                },
            ))
            .await;
    }
}
