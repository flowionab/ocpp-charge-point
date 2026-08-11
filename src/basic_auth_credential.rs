//! Persistence and rollback for a rotated `NetworkConfiguration[slot].BasicAuthPassword`
//! (A01.FR.02, A01.FR.04, `docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV10).
//!
//! # Why this exists apart from `device_model`/`network_switch`
//!
//! Two very different pieces of code need the same record: [`crate::device_model`]'s
//! `SetVariables` handler *writes* it (validate, persist, log - never touching
//! [`crate::state::ChargePointState`], see that module's docs on why), and
//! [`crate::network_switch::ConnectionTarget`] *reads and rolls back* it every time it redials -
//! "apply on next connect" falls out for free if every dial reads the current password fresh
//! rather than caching it, and rollback is then just "write the previous value back" rather than
//! new machinery. Sharing one small module keeps the record's shape (and the label a slot's
//! password is stored under) defined exactly once.
//!
//! # Rollback, and why it needs no extra "is this confirmed yet" flag
//!
//! [`StoredBasicAuthPassword`] holds `current` and `previous` together, and that pairing *is* the
//! rollback state - there is nothing else to track. [`rotate`] moves whatever was `current` into
//! `previous` and installs the new password as `current`. [`confirm`] (called once a dial using
//! `current` succeeds) clears `previous`, declaring the rotation proven. [`rollback`] (called once
//! [`crate::network_switch::ConnectionTarget`]'s own failure counter reaches
//! `OCPPCommCtrlr`/`NetworkProfileConnectionAttempts`, the same threshold an address or staged TLS
//! config rollback uses) swaps `previous` back into `current` and clears `previous` again. A
//! reboot between rotation and confirmation loses no safety: the record on disk still carries
//! both values, so the fresh `ConnectionTarget` a restart creates simply resumes trying `current`
//! for another `attempts_before_rollback` dials before falling back, rather than being stuck
//! either way.

use crate::hardware::KeyStore;
use crate::security_profile::BasicAuthPassword;
use alloc::format;
use alloc::string::{String, ToString};

/// The `hardware::KeyStore` label a slot's Basic-Auth credential record is stored under.
///
/// Namespaced (`basic-auth-password/`) rather than the bare slot number, so a `KeyStore`
/// implementation that shares one flat label space with other credentials (or, for
/// [`crate::hardware::SoftKeyStore`], a future one) can't collide with them.
fn label(slot: i32) -> String {
    format!("basic-auth-password/{slot}")
}

/// The record [`crate::hardware::KeyStore::store_credential`] holds for one
/// `NetworkConfiguration[slot].BasicAuthPassword` - see the module docs for why `current` and
/// `previous` together *are* the rollback state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct StoredBasicAuthPassword {
    current: String,
    previous: Option<String>,
}

/// Rotates the password stored for `slot`: whatever was `current` (if anything) becomes
/// `previous`, and `new_password` becomes `current`. Called from
/// [`crate::device_model::handle_set_variables`] once [`BasicAuthPassword::new`] has already
/// validated the incoming value - this function does no validation of its own.
pub(crate) async fn rotate<K: KeyStore>(
    key_store: &K,
    slot: i32,
    new_password: &BasicAuthPassword,
) -> Result<(), K::Error> {
    let previous = load(key_store, slot).await.map(|record| record.current);
    let record = StoredBasicAuthPassword {
        current: new_password.expose().to_string(),
        previous,
    };
    store(key_store, slot, &record).await
}

/// The password currently in force for `slot` - the value a dial should present, whether or not
/// its rotation has been confirmed yet. `None` if nothing has ever been rotated for this slot, or
/// if the record could not be read (logged, not propagated - see [`load`]).
///
/// `websocket`-gated with [`confirm`]/[`rollback`] below: [`crate::network_switch`] is this
/// function's only consumer, and it carries the same gate (a redial target is meaningless without
/// a websocket transport to redial). [`rotate`] stays ungated - [`crate::device_model`], which
/// persists a rotation regardless of which transport (if any) is compiled in, needs it
/// unconditionally.
#[cfg(feature = "websocket")]
pub(crate) async fn current<K: KeyStore>(key_store: &K, slot: i32) -> Option<String> {
    load(key_store, slot).await.map(|record| record.current)
}

/// Declares `slot`'s current password proven: a dial using it has succeeded, so there is nothing
/// left to roll back to. A no-op (not an error) if nothing is stored, or if `previous` was
/// already clear.
#[cfg(feature = "websocket")]
pub(crate) async fn confirm<K: KeyStore>(key_store: &K, slot: i32) {
    let Some(mut record) = load(key_store, slot).await else {
        return;
    };
    if record.previous.is_none() {
        return;
    }
    record.previous = None;
    if let Err(error) = store(key_store, slot, &record).await {
        tracing::warn!(%error, slot, "could not confirm a rotated Basic Auth password");
    }
}

/// Reverts `slot` to its previous password, per A01.FR.04: dialling with `current` has failed
/// often enough that [`crate::network_switch::ConnectionTarget`] has given up on it. Returns
/// whether a rollback actually happened - `false` when there was no `previous` to fall back to
/// (nothing was ever rotated, or a previous rollback/confirm already cleared it), which the
/// caller uses to decide whether it has anything left to retry.
#[cfg(feature = "websocket")]
pub(crate) async fn rollback<K: KeyStore>(key_store: &K, slot: i32) -> bool {
    let Some(mut record) = load(key_store, slot).await else {
        return false;
    };
    let Some(previous) = record.previous.take() else {
        return false;
    };
    record.current = previous;
    match store(key_store, slot, &record).await {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(%error, slot, "could not roll back a Basic Auth password rotation");
            false
        }
    }
}

async fn load<K: KeyStore>(key_store: &K, slot: i32) -> Option<StoredBasicAuthPassword> {
    match key_store.load_credential(&label(slot)).await {
        Ok(Some(encoded)) => match serde_json::from_str(&encoded) {
            Ok(record) => Some(record),
            Err(error) => {
                tracing::warn!(%error, slot, "discarding a corrupt stored Basic Auth password");
                None
            }
        },
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(%error, slot, "could not read a stored Basic Auth password");
            None
        }
    }
}

async fn store<K: KeyStore>(
    key_store: &K,
    slot: i32,
    record: &StoredBasicAuthPassword,
) -> Result<(), K::Error> {
    // Infallible in practice (the record has no type `serde_json` can't encode), and there is no
    // sane fallback if it ever weren't - the caller's `Result` has no variant for "the value was
    // fine but this crate couldn't serialize it".
    let encoded = serde_json::to_string(record).unwrap_or_default();
    key_store.store_credential(&label(slot), &encoded).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::{InMemoryStorage, SignatureAlgorithm, SoftKeyStore, SoftwareCrypto};
    use alloc::sync::Arc;
    use alloc::vec::Vec;

    /// A [`SoftwareCrypto`] that panics if ever asked to touch key material - this module's
    /// functions never do, and a test reaching one of these would mean a credential accidentally
    /// went through the key-material path instead.
    #[derive(Debug, Default)]
    struct UnusedCrypto;
    impl SoftwareCrypto for UnusedCrypto {
        type Error = core::convert::Infallible;
        fn generate_key_pair(
            &self,
            _algorithm: SignatureAlgorithm,
        ) -> Result<(Vec<u8>, crate::hardware::PublicKey), Self::Error> {
            unreachable!("basic_auth_credential never generates a key pair")
        }
        fn sign(
            &self,
            _algorithm: SignatureAlgorithm,
            _private_key: &[u8],
            _digest: &[u8],
        ) -> Result<Vec<u8>, Self::Error> {
            unreachable!("basic_auth_credential never signs")
        }
        fn supported_algorithms(&self) -> &[SignatureAlgorithm] {
            &[]
        }
    }

    fn key_store() -> SoftKeyStore<Arc<InMemoryStorage>, UnusedCrypto> {
        SoftKeyStore::new(Arc::new(InMemoryStorage::new()), UnusedCrypto)
    }

    fn password(value: &str) -> BasicAuthPassword {
        BasicAuthPassword::new(value).unwrap()
    }

    #[tokio::test]
    async fn a_slot_nobody_has_rotated_has_no_current_password() {
        let store = key_store();
        assert_eq!(current(&store, 1).await, None);
    }

    #[tokio::test]
    async fn a_rotation_becomes_the_current_password() {
        let store = key_store();

        rotate(&store, 1, &password("first-password-16"))
            .await
            .unwrap();

        assert_eq!(
            current(&store, 1).await.as_deref(),
            Some("first-password-16")
        );
    }

    #[tokio::test]
    async fn a_second_rotation_keeps_the_first_as_previous_for_rollback() {
        let store = key_store();
        rotate(&store, 1, &password("first-password-16"))
            .await
            .unwrap();

        rotate(&store, 1, &password("second-password-16"))
            .await
            .unwrap();
        assert_eq!(
            current(&store, 1).await.as_deref(),
            Some("second-password-16")
        );

        // Rolling back after the *second* rotation returns to the *first* password, not to
        // nothing - `rotate` must have carried the still-unconfirmed first password forward as
        // `previous` rather than discarding it.
        assert!(rollback(&store, 1).await);
        assert_eq!(
            current(&store, 1).await.as_deref(),
            Some("first-password-16")
        );
    }

    #[tokio::test]
    async fn confirming_clears_previous_so_a_later_rollback_has_nothing_to_do() {
        let store = key_store();
        rotate(&store, 1, &password("rotated-password-16"))
            .await
            .unwrap();

        confirm(&store, 1).await;

        assert!(!rollback(&store, 1).await);
        // The confirmed password is still in force - `rollback` finding nothing to revert must
        // not have cleared `current` too.
        assert_eq!(
            current(&store, 1).await.as_deref(),
            Some("rotated-password-16")
        );
    }

    #[tokio::test]
    async fn rolling_back_with_nothing_stored_is_a_safe_no_op() {
        let store = key_store();
        assert!(!rollback(&store, 1).await);
        assert_eq!(current(&store, 1).await, None);
    }

    #[tokio::test]
    async fn confirming_a_slot_nobody_rotated_is_a_safe_no_op() {
        let store = key_store();
        confirm(&store, 1).await;
        assert_eq!(current(&store, 1).await, None);
    }

    #[tokio::test]
    async fn different_slots_do_not_share_a_password() {
        let store = key_store();
        rotate(&store, 1, &password("slot-one-password"))
            .await
            .unwrap();
        rotate(&store, 2, &password("slot-two-password"))
            .await
            .unwrap();

        assert_eq!(
            current(&store, 1).await.as_deref(),
            Some("slot-one-password")
        );
        assert_eq!(
            current(&store, 2).await.as_deref(),
            Some("slot-two-password")
        );

        // Neither slot has been rotated twice, so neither has a `previous` to fall back to - but
        // the point of this test is that rolling back one slot cannot possibly touch the other.
        assert!(!rollback(&store, 1).await);
        assert_eq!(
            current(&store, 2).await.as_deref(),
            Some("slot-two-password")
        );
    }

    /// CV10's whole point (A01.FR.02 without A01.FR.03's lockout risk): the record survives a
    /// reboot, so a station that rotated a password and then restarted before confirming it still
    /// has both the new password to try and the old one to fall back to.
    #[tokio::test]
    async fn a_rotation_survives_a_reboot() {
        let storage = Arc::new(InMemoryStorage::new());
        let before = SoftKeyStore::new(storage.clone(), UnusedCrypto);
        rotate(&before, 1, &password("rotated-password-16"))
            .await
            .unwrap();

        let after = SoftKeyStore::new(storage, UnusedCrypto);

        assert_eq!(
            current(&after, 1).await.as_deref(),
            Some("rotated-password-16")
        );
    }
}
