//! An optional persistent key-value store an integrator can supply so state that should survive
//! a power cycle (offline transaction queue, cached configuration, etc.) actually does. Wiring
//! that state through [`Storage`] is future work (`docs/PRODUCTION-ROADMAP.md` §7.2 E2 onward) -
//! this module only defines the trait surface and a couple of implementations, per E1.
//!
//! A charge point is not required to have persistent storage at all - [`NoStorage`] is a no-op
//! implementation an integrator without durable hardware can pass through so everything still
//! runs, just without anything surviving a restart. See [`NoStorage`]'s docs for exactly what
//! that gives up.

use alloc::boxed::Box;
#[cfg(feature = "std")]
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
}
