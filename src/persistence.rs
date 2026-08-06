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

use crate::actor::ChargePointActor;
use crate::clock::Clock;
use crate::hardware::Storage;
use crate::state::{
    ChargePointEvent, MeterSample, RecoveredTransaction, Transaction, TransactionEventKind,
    TransactionEventOccurred, TransactionUpdateReason,
};
use crate::sync::BroadcastReceiver;

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
    /// [`run_transaction_persistence`]. `None` if the charge point has no usable time source.
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
fn next_record<C: Clock>(
    previous: Option<&PersistedTransaction>,
    occurred: &TransactionEventOccurred,
    clock: &C,
) -> PersistedTransaction {
    let started_at = match (occurred.kind, previous) {
        (TransactionEventKind::Started, _) => Some(clock.now()),
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

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    use crate::clock::SystemClock;
    use crate::hardware::{InMemoryStorage, NoStorage};
    use crate::state::{IdToken, IdTokenKind, StopReason, TransactionChargingState, TransactionId};

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
}
