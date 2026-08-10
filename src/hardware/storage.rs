//! An optional persistent key-value store an integrator can supply so state that should survive
//! a power cycle (offline transaction queue, cached configuration, etc.) actually does.
//!
//! The first state actually wired through this is the in-flight transaction - see
//! [`crate::persistence`], which persists and recovers it (E2.1/E4.1). The remaining rows of
//! `docs/PRODUCTION-ROADMAP.md` §7.2's table (the offline queue, the local authorization list,
//! reservations, certificates, ...) are still RAM-only.
//!
//! A charge point is not required to have persistent storage at all - [`NoStorage`] is a no-op
//! implementation an integrator without durable hardware can pass through so everything still
//! runs, just without anything surviving a restart. See [`NoStorage`]'s docs for exactly what
//! that gives up.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// A durable (or, for [`NoStorage`], deliberately non-durable) key-value store, used to persist
/// state that should survive a power cycle.
///
/// `no_std`-friendly: keys and values are borrowed/owned byte-oriented data (`&str` keys,
/// `Vec<u8>` values) rather than anything that assumes a filesystem or a particular serialization
/// format - implementors can back this with flash, a filesystem, a database, or nothing at all.
///
/// Every operation is explicitly fallible. Per `CLAUDE.md`'s error-handling stance, a storage
/// failure must never be allowed to panic or take down the charge point: callers are expected to
/// treat an `Err` here as "persistence unavailable right now" and degrade (continue running
/// without whatever durability this write/read would have provided, and raise a
/// diagnostic/security event) rather than propagate the failure into a crash.
///
/// # What this crate expects of an implementation, exactly
///
/// Two questions every implementor asks, answered here so nobody has to infer them from the
/// persistence layer:
///
/// - **Must `set` be durable by the time it returns?** Yes, as far as this trait is concerned: a
///   `set` that returned `Ok` and then lost the value on a power cut is a broken implementation,
///   not a slow one. If the backing medium buffers (a filesystem page cache, a flash driver with
///   a write queue), flush before returning `Ok`.
/// - **Must a write be crash-atomic - never leaving a half-written value behind?** **No.** That
///   is this crate's problem, not yours. Everything durable enough to care goes through
///   [`AtomicStorage`], which stores each logical key in two alternating physical slots with a
///   checksum, so a power cut mid-`set` leaves the previous value intact and readable. Implement
///   the simple thing; do not build a journal underneath one.
///
/// Nothing here requires a filesystem, a transaction log, or ordering guarantees between
/// different keys.
#[async_trait::async_trait]
pub trait Storage {
    /// The error type returned by a failed storage operation.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Reads the value previously stored under `key`, or `Ok(None)` if nothing is stored there
    /// (this is not an error - a missing key is an expected, normal outcome).
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Stores `value` under `key`, overwriting any previous value.
    async fn set(&self, key: &str, value: &[u8]) -> Result<(), Self::Error>;

    /// Removes any value stored under `key`. Removing a key that isn't present is not an error.
    async fn remove(&self, key: &str) -> Result<(), Self::Error>;
}

/// Forwards to the wrapped store, so one [`Storage`] can be shared between the several
/// independent tasks that persist state (see [`crate::persistence`]) without each needing its
/// own handle onto the hardware.
#[async_trait::async_trait]
impl<S: Storage + Send + Sync> Storage for alloc::sync::Arc<S> {
    type Error = S::Error;

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Self::Error> {
        (**self).get(key).await
    }

    async fn set(&self, key: &str, value: &[u8]) -> Result<(), Self::Error> {
        (**self).set(key, value).await
    }

    async fn remove(&self, key: &str) -> Result<(), Self::Error> {
        (**self).remove(key).await
    }
}

/// A no-op [`Storage`] implementation for charge points with no durable storage hardware at
/// all. `get` always returns `Ok(None)`, and `set`/`remove` always succeed without persisting
/// anything.
///
/// **Durability guarantee: none.** Anything written through this is gone the moment the process
/// ends - an offline transaction queue, cached configuration, or any other state a caller layers
/// on top of [`Storage`] does not survive a restart when backed by `NoStorage`. This exists so a
/// charge point without persistent storage hardware can still run end-to-end (see
/// [`crate::hardware::Capabilities::has_persistent_storage`], which such an integrator should
/// leave `false`) rather than being forced to implement a trait it has no hardware to back.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoStorage;

/// The error type of [`NoStorage`]. `NoStorage`'s operations never actually fail (there is
/// nothing to fail - they don't touch any hardware), so this can never be constructed; it exists
/// only to give [`Storage::Error`] a concrete, documented type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoStorageError {}

impl core::fmt::Display for NoStorageError {
    fn fmt(&self, _formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {}
    }
}

impl core::error::Error for NoStorageError {}

#[async_trait::async_trait]
impl Storage for NoStorage {
    type Error = NoStorageError;

    async fn get(&self, _key: &str) -> Result<Option<Vec<u8>>, Self::Error> {
        Ok(None)
    }

    async fn set(&self, _key: &str, _value: &[u8]) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn remove(&self, _key: &str) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// A `std`-only in-memory [`Storage`] implementation, backed by a mutex-guarded `HashMap`.
/// Intended for tests and desktop integrators that want real (if process-lifetime-only)
/// persistence semantics without wiring up real hardware - values written are readable again for
/// the life of the process, but (like [`NoStorage`]) do **not** survive a restart, since nothing
/// is written to disk.
#[cfg(feature = "std")]
#[derive(Debug, Default)]
pub struct InMemoryStorage {
    entries: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
}

#[cfg(feature = "std")]
impl InMemoryStorage {
    /// Creates an empty in-memory store.
    pub fn new() -> Self {
        Self::default()
    }
}

/// The error type of [`InMemoryStorage`]. Its operations never actually fail (a poisoned mutex
/// is recovered from rather than propagated), so this can never be constructed; it exists only
/// to give [`Storage::Error`] a concrete, documented type.
#[cfg(feature = "std")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InMemoryStorageError {}

#[cfg(feature = "std")]
impl core::fmt::Display for InMemoryStorageError {
    fn fmt(&self, _formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {}
    }
}

#[cfg(feature = "std")]
impl core::error::Error for InMemoryStorageError {}

#[cfg(feature = "std")]
#[async_trait::async_trait]
impl Storage for InMemoryStorage {
    type Error = InMemoryStorageError;

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Self::Error> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(entries.get(key).cloned())
    }

    async fn set(&self, key: &str, value: &[u8]) -> Result<(), Self::Error> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.insert(String::from(key), Vec::from(value));
        Ok(())
    }

    async fn remove(&self, key: &str) -> Result<(), Self::Error> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.remove(key);
        Ok(())
    }
}

/// Turns a [`Storage`] whose `set`/`remove` are *not* guaranteed atomic into one whose `get`
/// never observes a torn write - see `docs/PRODUCTION-ROADMAP.md` §7.3 (E3.1).
///
/// # The problem
///
/// [`Storage::set`] is documented as "stores `value` under `key`, overwriting any previous
/// value", but says nothing about what a reader sees if power is lost mid-write. No flash driver
/// actually promises atomicity for an arbitrary-length write: a page write can be interrupted
/// partway, leaving a mix of old and new bytes (or a range that reads back as `0xFF`/garbage)
/// under `key`. [`crate::persistence`] already discards a record that fails to decode as a
/// complete, current-schema value (so a torn write is never *silently* half-applied) - but that
/// only protects against corruption being misread as valid data. It cannot recover the write that
/// was in progress, and if the *previous* value happened to be left in a state that still decodes
/// (partially overwritten but coincidentally well-formed), the corruption wouldn't even be
/// detected. `AtomicStorage` closes that gap at the storage boundary itself, rather than relying
/// on every caller to defend against it.
///
/// # The protocol
///
/// Each logical `key` passed through `AtomicStorage` is backed by two physical keys in the
/// wrapped store (an "A/B slot" pair, `"{key}.a"` and `"{key}.b"`), never one. A write always
/// targets whichever slot does **not** currently hold the latest valid record, leaving the other
/// slot - and therefore the previous value - completely untouched:
///
/// - every record is wrapped with a small header: a monotonically increasing sequence number, a
///   one-byte kind (value or tombstone, so `remove` is a write like any other rather than a
///   destructive in-place delete), a length, and a CRC32 of the payload;
/// - `set`/`remove` read both slots, pick whichever slot is *not* the current winner (by highest
///   sequence number among the slots that still decode and checksum correctly), and write the new
///   record - with the next sequence number - only to that slot;
/// - `get` reads both slots, decodes and checksums each independently, and returns the payload of
///   whichever valid record has the higher sequence number (a tombstone's "payload" is `None`).
///
/// A power cut during the write to the target slot leaves that slot's bytes corrupt (they will
/// fail the length check or the CRC), so it is simply skipped: the other slot, never touched by
/// this write, is still there with the highest surviving valid sequence number and is returned as
/// before. The write is therefore atomic *from a reader's point of view* - `get` after a crash
/// returns either the value from before the interrupted write, or the value from after it,
/// **never** a mix of the two - without requiring the underlying [`Storage::set`] to be atomic at
/// all.
///
/// # What this does **not** guarantee
///
/// - **A storage backend that corrupts bytes it wasn't asked to write.** The whole protocol
///   assumes writing slot `.a` cannot also corrupt slot `.b` (or any unrelated key). That is true
///   for a key-value store with per-key write isolation, but is *not* automatically true of a raw
///   flash region: if both slots happen to share an erase block and a power cut occurs mid-erase,
///   both can be lost together. An integrator putting `AtomicStorage` directly over raw flash
///   still needs the underlying [`Storage`] impl to place `.a`/`.b` pairs (and distinct keys
///   generally) in a way that a torn write to one cannot clobber another - e.g. separate erase
///   sectors, or a wear-levelling flash filesystem that already provides that isolation.
/// - **Detecting corruption that leaves a record's bytes complete and checksum-valid but wrong.**
///   The CRC32 catches torn/partial writes and bit-level flash glitches; it cannot catch, say, the
///   wrong (but complete and self-consistent) value being written due to a logic bug upstream.
/// - **Concurrent writers.** Like [`Storage`] itself, this assumes a single writer per key at a
///   time (true of every caller in this crate - see the module docs on `TransactionStore`).
///   Two concurrent `set`s racing to read-then-write could both pick the same target slot.
/// - **Wear beyond what two slots buys you.** Alternating between two physical slots roughly
///   halves the erase-cycle pressure on any one flash region compared to always overwriting a
///   single key in place, but this is not a wear-levelling scheme across the whole device.
///
/// # Key ownership
///
/// `AtomicStorage` claims `"{key}.a"` and `"{key}.b"` in the wrapped [`Storage`] for every `key`
/// passed through it. Do not write to those exact suffixed keys through a different handle onto
/// the same store, and do not wrap a [`Storage`] that some other code already addresses with a
/// `.a`/`.b`-suffixed naming convention of its own.
#[derive(Debug, Clone)]
pub struct AtomicStorage<S> {
    inner: S,
}

/// One physical slot of an [`AtomicStorage`] logical key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    A,
    B,
}

impl Slot {
    fn suffix(self) -> char {
        match self {
            Slot::A => 'a',
            Slot::B => 'b',
        }
    }

    fn other(self) -> Slot {
        match self {
            Slot::A => Slot::B,
            Slot::B => Slot::A,
        }
    }
}

/// Record kind stamped into an [`AtomicStorage`] slot header - see [`AtomicStorage`]'s docs.
const RECORD_KIND_VALUE: u8 = 1;
const RECORD_KIND_TOMBSTONE: u8 = 0;

/// `magic(4) + seq(8) + kind(1) + len(4) + crc32(4)`.
const RECORD_HEADER_LEN: usize = 21;
const RECORD_MAGIC: [u8; 4] = *b"OCPA";

/// A successfully decoded, checksummed [`AtomicStorage`] slot record.
struct DecodedRecord<'a> {
    seq: u64,
    kind: u8,
    payload: &'a [u8],
}

fn encode_record(seq: u64, kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(RECORD_HEADER_LEN + payload.len());
    buf.extend_from_slice(&RECORD_MAGIC);
    buf.extend_from_slice(&seq.to_le_bytes());
    buf.push(kind);
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&crc32(payload).to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Decodes and checksums a slot's raw bytes. `None` covers every way a torn or corrupt write can
/// present - too short, bad magic, a length that doesn't match what's actually there, or a CRC
/// mismatch - and is treated identically by the caller: this slot doesn't have a usable record.
fn decode_record(bytes: &[u8]) -> Option<DecodedRecord<'_>> {
    if bytes.len() < RECORD_HEADER_LEN {
        return None;
    }
    if bytes[0..4] != RECORD_MAGIC {
        return None;
    }
    let seq = u64::from_le_bytes(bytes[4..12].try_into().ok()?);
    let kind = bytes[12];
    if kind != RECORD_KIND_VALUE && kind != RECORD_KIND_TOMBSTONE {
        return None;
    }
    let len = u32::from_le_bytes(bytes[13..17].try_into().ok()?) as usize;
    let crc = u32::from_le_bytes(bytes[17..21].try_into().ok()?);
    if bytes.len() != RECORD_HEADER_LEN + len {
        return None;
    }
    let payload = &bytes[RECORD_HEADER_LEN..];
    if crc32(payload) != crc {
        return None;
    }
    Some(DecodedRecord { seq, kind, payload })
}

/// `true` if `candidate` is the later of two sequence numbers, tolerant of `u64` wraparound
/// (never reached in practice, but cheap to get right): the two are treated as points on a cycle,
/// and whichever is reachable from the other by advancing less than half the space is "later".
fn seq_is_later(candidate: u64, than: u64) -> bool {
    let delta = candidate.wrapping_sub(than);
    delta != 0 && delta < u64::MAX / 2
}

/// Bare CRC32 (IEEE 802.3 polynomial), computed byte-at-a-time rather than via a lookup table:
/// [`AtomicStorage`] records are small (persisted transaction records, not bulk data) and this
/// avoids pulling in a `crc`/`crc32fast` dependency or a 1 KiB static table for a check that runs
/// once per write on an embedded target.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

impl<S: Storage> AtomicStorage<S> {
    /// Wraps `inner` so every `set`/`remove` through the result is crash-safe - see
    /// [`AtomicStorage`]'s docs for exactly what that does and doesn't guarantee.
    pub fn new(inner: S) -> Self {
        Self { inner }
    }

    /// Gives back the wrapped store.
    pub fn into_inner(self) -> S {
        self.inner
    }

    fn slot_key(key: &str, slot: Slot) -> String {
        format!("{key}.{}", slot.suffix())
    }

    /// Reads and decodes `key`'s slot `a` and slot `b`, without interpreting them as a value yet.
    /// Returns the winning slot (whichever holds the later valid sequence number) and that
    /// sequence number, or `None` if neither slot currently holds a valid record.
    ///
    /// A slot whose raw read errors is treated the same as one that fails to decode (best-effort:
    /// the other slot may still resolve the read) *unless both slots error*, in which case the
    /// caller cannot safely determine current state at all and the error is propagated rather
    /// than guessed past.
    async fn read_slots(&self, key: &str) -> Result<Option<(Slot, u64)>, S::Error> {
        let a_raw = self.inner.get(&Self::slot_key(key, Slot::A)).await;
        let b_raw = self.inner.get(&Self::slot_key(key, Slot::B)).await;
        let a_seq = match &a_raw {
            Ok(Some(bytes)) => decode_record(bytes).map(|record| record.seq),
            _ => None,
        };
        let b_seq = match &b_raw {
            Ok(Some(bytes)) => decode_record(bytes).map(|record| record.seq),
            _ => None,
        };
        match (a_seq, b_seq) {
            (Some(a_seq), Some(b_seq)) => Ok(Some(if seq_is_later(b_seq, a_seq) {
                (Slot::B, b_seq)
            } else {
                (Slot::A, a_seq)
            })),
            (Some(a_seq), None) => Ok(Some((Slot::A, a_seq))),
            (None, Some(b_seq)) => Ok(Some((Slot::B, b_seq))),
            (None, None) => match (a_raw, b_raw) {
                (Err(err), _) => Err(err),
                (_, Err(err)) => Err(err),
                _ => Ok(None),
            },
        }
    }

    /// Writes `payload` (a real value, or an empty tombstone payload for `remove`) as the next
    /// record for `key`, to whichever slot doesn't currently hold the winning record.
    async fn write_record(&self, key: &str, kind: u8, payload: &[u8]) -> Result<(), S::Error> {
        let current = self.read_slots(key).await?;
        let (target, next_seq) = match current {
            Some((winner, seq)) => (winner.other(), seq.wrapping_add(1)),
            None => (Slot::A, 0),
        };
        let encoded = encode_record(next_seq, kind, payload);
        self.inner.set(&Self::slot_key(key, target), &encoded).await
    }
}

#[async_trait::async_trait]
impl<S: Storage + Send + Sync> Storage for AtomicStorage<S> {
    type Error = S::Error;

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Self::Error> {
        let a_raw = self.inner.get(&Self::slot_key(key, Slot::A)).await;
        let b_raw = self.inner.get(&Self::slot_key(key, Slot::B)).await;
        let a_record = match &a_raw {
            Ok(Some(bytes)) => decode_record(bytes),
            _ => None,
        };
        let b_record = match &b_raw {
            Ok(Some(bytes)) => decode_record(bytes),
            _ => None,
        };
        let winner = match (a_record, b_record) {
            (Some(a), Some(b)) => Some(if seq_is_later(b.seq, a.seq) { b } else { a }),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        match winner {
            Some(record) if record.kind == RECORD_KIND_VALUE => Ok(Some(record.payload.to_vec())),
            Some(_tombstone) => Ok(None),
            None => match (a_raw, b_raw) {
                (Err(err), _) => Err(err),
                (_, Err(err)) => Err(err),
                _ => Ok(None),
            },
        }
    }

    async fn set(&self, key: &str, value: &[u8]) -> Result<(), Self::Error> {
        self.write_record(key, RECORD_KIND_VALUE, value).await
    }

    async fn remove(&self, key: &str) -> Result<(), Self::Error> {
        self.write_record(key, RECORD_KIND_TOMBSTONE, &[]).await
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_storage_get_is_always_none() {
        let storage = NoStorage;
        assert_eq!(storage.get("key").await.unwrap(), None);
    }

    #[tokio::test]
    async fn no_storage_set_and_remove_always_succeed_without_persisting() {
        let storage = NoStorage;
        storage.set("key", b"value").await.unwrap();
        assert_eq!(storage.get("key").await.unwrap(), None);
        storage.remove("key").await.unwrap();
    }

    #[tokio::test]
    async fn in_memory_storage_round_trips_a_value() {
        let storage = InMemoryStorage::new();
        assert_eq!(storage.get("key").await.unwrap(), None);

        storage.set("key", b"value").await.unwrap();
        assert_eq!(
            storage.get("key").await.unwrap(),
            Some(Vec::from(&b"value"[..]))
        );

        storage.set("key", b"updated").await.unwrap();
        assert_eq!(
            storage.get("key").await.unwrap(),
            Some(Vec::from(&b"updated"[..]))
        );

        storage.remove("key").await.unwrap();
        assert_eq!(storage.get("key").await.unwrap(), None);
    }

    #[tokio::test]
    async fn in_memory_storage_remove_of_missing_key_is_not_an_error() {
        let storage = InMemoryStorage::new();
        storage.remove("missing").await.unwrap();
    }

    /// A [`Storage`] wrapper that lets a test truncate the *next* `set` call to a chosen key,
    /// simulating a power cut partway through a flash write: the underlying store ends up holding
    /// a short, garbage tail instead of the intended bytes.
    #[derive(Debug, Default)]
    struct TornWriteStorage {
        inner: InMemoryStorage,
        tear_next_write_to: std::sync::Mutex<Option<String>>,
    }

    impl TornWriteStorage {
        fn new() -> Self {
            Self::default()
        }

        /// The next `set` to exactly this key writes only the first half of the intended bytes.
        fn tear_next_write_to(&self, key: &str) {
            *self.tear_next_write_to.lock().unwrap() = Some(key.to_string());
        }
    }

    #[async_trait::async_trait]
    impl Storage for TornWriteStorage {
        type Error = InMemoryStorageError;

        async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Self::Error> {
            self.inner.get(key).await
        }

        async fn set(&self, key: &str, value: &[u8]) -> Result<(), Self::Error> {
            let tear = {
                let mut armed = self.tear_next_write_to.lock().unwrap();
                if armed.as_deref() == Some(key) {
                    *armed = None;
                    true
                } else {
                    false
                }
            };
            if tear {
                self.inner.set(key, &value[..value.len() / 2]).await
            } else {
                self.inner.set(key, value).await
            }
        }

        async fn remove(&self, key: &str) -> Result<(), Self::Error> {
            self.inner.remove(key).await
        }
    }

    #[tokio::test]
    async fn atomic_storage_round_trips_a_value() {
        let storage = AtomicStorage::new(InMemoryStorage::new());
        assert_eq!(storage.get("key").await.unwrap(), None);

        storage.set("key", b"value").await.unwrap();
        assert_eq!(
            storage.get("key").await.unwrap(),
            Some(Vec::from(&b"value"[..]))
        );

        storage.set("key", b"updated").await.unwrap();
        assert_eq!(
            storage.get("key").await.unwrap(),
            Some(Vec::from(&b"updated"[..]))
        );

        storage.remove("key").await.unwrap();
        assert_eq!(storage.get("key").await.unwrap(), None);
    }

    #[tokio::test]
    async fn atomic_storage_alternates_between_two_physical_slots() {
        let storage = AtomicStorage::new(InMemoryStorage::new());
        storage.set("key", b"one").await.unwrap();
        storage.set("key", b"two").await.unwrap();
        storage.set("key", b"three").await.unwrap();

        // Both slots must exist and both must independently decode to a valid record - a real
        // write never touches the slot it isn't targeting.
        assert!(storage.inner.get("key.a").await.unwrap().is_some());
        assert!(storage.inner.get("key.b").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn atomic_storage_survives_a_torn_write_by_falling_back_to_the_previous_value() {
        let storage = AtomicStorage::new(TornWriteStorage::new());
        storage.set("key", b"first value").await.unwrap();
        assert_eq!(
            storage.get("key").await.unwrap(),
            Some(Vec::from(&b"first value"[..]))
        );

        // The next write lands on whichever slot didn't just get "first value" - tear exactly
        // that one, simulating a power cut mid-write.
        let a_holds_first_write = storage
            .inner
            .get("key.a")
            .await
            .unwrap()
            .and_then(|bytes| decode_record(&bytes).map(|r| r.seq))
            == Some(0);
        let target = if a_holds_first_write {
            "key.b"
        } else {
            "key.a"
        };
        storage.inner.tear_next_write_to(target);
        // A torn write is still an `Ok` as far as the underlying `Storage::set` call is concerned
        // (the driver doesn't know it was interrupted) - it's `AtomicStorage::get` that must
        // notice and recover, not this `set` call.
        storage
            .set("key", b"second value, longer than first")
            .await
            .unwrap();

        assert_eq!(
            storage.get("key").await.unwrap(),
            Some(Vec::from(&b"first value"[..])),
            "a torn write must not be observable - the previous complete value must win"
        );

        // And the store is still writable afterwards: the next successful write reaches the
        // torn slot fine now that nothing is interrupting it.
        storage.set("key", b"third value").await.unwrap();
        assert_eq!(
            storage.get("key").await.unwrap(),
            Some(Vec::from(&b"third value"[..]))
        );
    }

    #[tokio::test]
    async fn atomic_storage_detects_bit_level_corruption_via_crc() {
        let storage = AtomicStorage::new(InMemoryStorage::new());
        storage.set("key", b"value").await.unwrap();

        // Corrupt whichever slot actually holds the record, in place, without going through
        // `AtomicStorage::set` - simulating a flash bit-flip rather than a torn write.
        for slot in ["key.a", "key.b"] {
            if let Some(mut bytes) = storage.inner.get(slot).await.unwrap()
                && decode_record(&bytes).is_some()
            {
                let last = bytes.len() - 1;
                bytes[last] ^= 0xFF;
                storage.inner.set(slot, &bytes).await.unwrap();
            }
        }

        assert_eq!(
            storage.get("key").await.unwrap(),
            None,
            "a corrupted record must not be returned as if it were valid"
        );
    }

    #[tokio::test]
    async fn atomic_storage_remove_is_itself_crash_safe() {
        let storage = AtomicStorage::new(TornWriteStorage::new());
        storage.set("key", b"value").await.unwrap();

        let target = if storage
            .inner
            .get("key.a")
            .await
            .unwrap()
            .and_then(|bytes| decode_record(&bytes).map(|r| r.seq))
            == Some(0)
        {
            "key.b"
        } else {
            "key.a"
        };
        storage.inner.tear_next_write_to(target);
        storage.remove("key").await.unwrap();

        // The tombstone write was torn, so the previous value must still be readable - a torn
        // `remove` must not silently lose data any more than a torn `set` may corrupt it.
        assert_eq!(
            storage.get("key").await.unwrap(),
            Some(Vec::from(&b"value"[..]))
        );

        // A clean remove afterwards does take effect.
        storage.remove("key").await.unwrap();
        assert_eq!(storage.get("key").await.unwrap(), None);
    }

    #[test]
    fn crc32_matches_a_known_vector() {
        // The canonical "check" value for CRC-32/ISO-HDLC (the variant this implements) over the
        // ASCII string "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
