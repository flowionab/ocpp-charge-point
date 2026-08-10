//! Certificate renewal ahead of expiry (`docs/PRODUCTION-ROADMAP.md` F3.2), built on B4.3's
//! `SignCertificate`/`CertificateSigned` round trip ([`crate::certificates`]).
//!
//! # The crux B4.3 left open
//!
//! [`crate::certificates::build_signed_csr`] takes a fresh
//! [`crate::hardware::GeneratedKeyPair`] per call and remembers nothing about it once the CSR is
//! sent - fine for an initial certificate, wrong for a renewal, which must ask the CSMS to sign a
//! *replacement* certificate for the **same key** as the one it replaces (or deliberately rotate
//! to a new one - this module always reuses, see [`crate::certificate_renewal::RenewalStore::key_pair_for`]'s docs for why
//! that is the safer default). [`crate::certificate_renewal::RenewalStore`] is the piece that remembers: it persists, per
//! [`crate::certificates::CertificateSigningPurpose`], the [`crate::hardware::GeneratedKeyPair`] this charge point last used to request
//! a certificate for that purpose, so [`crate::certificate_renewal::renew_certificate`] can hand the *same* key back to
//! [`crate::certificates::build_signed_csr`] instead of generating a new one.
//!
//! **Keyed by purpose, not by [`crate::hardware::CertificateHashData`].** The obvious design
//! keys the association off the certificate's own hash, but this crate's certificate slots
//! ([`crate::hardware::CertificateUse::ChargingStation`]/[`crate::hardware::CertificateUse::V2gCertificateChain`]) are
//! single-slotted by construction - see [`crate::hardware::CertificateStore::certificate_chain_pem`]'s
//! docs - so "the key behind the certificate currently installed for this purpose" and "the key
//! behind the certificate this purpose's hash names" are the same fact. Keying by purpose avoids
//! needing the hash at all, which matters because [`crate::certificates::handle_certificate_signed`]
//! never hands one back to its caller.
//!
//! # When to renew
//!
//! OCPP mandates no threshold, so [`crate::certificate_renewal::RenewalPolicy`] makes it a configurable duration
//! ([`crate::certificate_renewal::RenewalPolicy::new`]), defaulting to [`crate::certificate_renewal::DEFAULT_RENEW_BEFORE_DAYS`] days
//! ([`crate::certificate_renewal::RenewalPolicy`]'s `Default` impl).
//!
//! # Where the expiry comes from
//!
//! This crate does no X.509 parsing (`CLAUDE.md`'s "no crypto dependency"), so it cannot compute
//! a certificate's expiry from its PEM bytes. [`crate::hardware::CertificateStore::expires_at`]
//! is the honest hook for it: `None` is not "never expires" or "far away", it is "this store
//! cannot say", and [`crate::certificate_renewal::run_certificate_renewal`] skips a slot it gets `None` for rather than
//! guessing - the same stance [`crate::clock::is_synchronized`]'s callers take for an unset RTC.
//! An integrator who wants ahead-of-expiry renewal must override that method (directly, or by
//! feeding a value learned some other way, e.g. out of band alongside a `CertificateSigned`).
//!
//! # The clock
//!
//! [`crate::certificate_renewal::run_certificate_renewal`] skips its sweep entirely while
//! [`crate::clock::is_synchronized`] says the clock is not - mirroring
//! [`crate::reservation::run_reservation_expiry`]'s precedent exactly. A station that does not
//! know the date must not conclude a certificate has expired: an unsynchronized reading is
//! typically near the epoch, which would make every real `expires_at` look overdue.
//!
//! # Failure is fail-safe: the old certificate never leaves until the new one works
//!
//! Firing a `SignCertificate` changes nothing in [`crate::hardware::CertificateStore`] - only a
//! later, correlated `CertificateSigned` does (via
//! [`crate::certificates::handle_certificate_signed`]), and only by *replacing* the same
//! single slot. A CSMS that rejects the request, or that never answers at all, therefore leaves
//! the existing, still-valid certificate exactly where it was: there is no window where this
//! charge point holds neither. [`crate::certificate_renewal::discard_certificate_renewal`] closes the one remaining gap - a
//! `CertificateSigned` that *did* install a new certificate, but that certificate then fails to
//! establish a connection (a mismatched CA, a CSMS that never actually trusted it) - by restoring
//! the [`crate::certificate_renewal::RenewalStore`]'s stashed backup and reporting
//! [`crate::state::SecurityEventType::DiscardedRenewedClientCertificate`], the security event
//! OCPP models for exactly this.

use alloc::string::String;
use alloc::vec::Vec;
use chrono::{DateTime, Duration, Utc};

use crate::actor::ChargePointActor;
use crate::certificates::{
    CertificateSigningPurpose, CertificateSigningRequester, CsrSubject, SignCertificateError,
    SignCertificateOutcome,
};
use crate::clock::Clock;
use crate::hardware::{
    CertificateStore, GeneratedKeyPair, InstallCertificateOutcome, KeyHandle, KeyStore, PublicKey,
    SignatureAlgorithm, Storage,
};
use crate::state::{SecurityEvent, SecurityEventType};

/// How many days before a certificate's known expiry [`RenewalPolicy::default`] attempts a
/// renewal.
///
/// 30: comfortably more than one failed CSMS round trip's worth of retry headroom (this crate has
/// no control over how long a CSMS takes to answer `SignCertificate`, and B4.3's round trip can
/// go unanswered indefinitely), while still short enough that a monthly renewal sweep interval
/// would not miss a certificate issued with a deliberately short lifetime.
pub const DEFAULT_RENEW_BEFORE_DAYS: i64 = 30;

/// How long before a certificate's expiry this charge point should attempt to renew it.
///
/// OCPP names no threshold for this, so it is a configurable value rather than a hardcoded one -
/// see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenewalPolicy {
    renew_before: Duration,
}

impl RenewalPolicy {
    /// A policy that renews `renew_before` ahead of expiry. A negative duration is clamped to
    /// zero (renew only once already expired, never before): a duration meant to mean "after
    /// expiry" would silently invert this policy's whole purpose, so a caller is not offered that
    /// footgun.
    pub fn new(renew_before: Duration) -> Self {
        Self {
            renew_before: renew_before.max(Duration::zero()),
        }
    }

    /// How long before expiry this policy renews.
    pub fn renew_before(&self) -> Duration {
        self.renew_before
    }
}

impl Default for RenewalPolicy {
    /// [`DEFAULT_RENEW_BEFORE_DAYS`] days ahead of expiry.
    fn default() -> Self {
        Self::new(Duration::days(DEFAULT_RENEW_BEFORE_DAYS))
    }
}

/// Whether a certificate expiring at `expires_at` is due for renewal at `now`, per `policy`.
///
/// `now >= expires_at - policy.renew_before()`, i.e. inclusive of the exact renewal instant -
/// mirroring [`crate::reservation`]'s "`expiryDateTime` is the instant it lapses" rule rather than
/// leaving an off-by-one gap where the sweep interval happens to land exactly on the threshold.
pub fn is_due_for_renewal(
    now: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    policy: &RenewalPolicy,
) -> bool {
    now >= expires_at - policy.renew_before()
}

// --- Persisted key/backup associations --------------------------------------------------------

const RENEWAL_KEY: &str = "certificate_renewal";

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
enum PersistedAlgorithm {
    EcdsaP256Sha256,
    EcdsaP384Sha384,
    Rsa2048Sha256,
    Rsa3072Sha256,
}

impl From<SignatureAlgorithm> for PersistedAlgorithm {
    fn from(algorithm: SignatureAlgorithm) -> Self {
        match algorithm {
            SignatureAlgorithm::EcdsaP256Sha256 => Self::EcdsaP256Sha256,
            SignatureAlgorithm::EcdsaP384Sha384 => Self::EcdsaP384Sha384,
            SignatureAlgorithm::Rsa2048Sha256 => Self::Rsa2048Sha256,
            SignatureAlgorithm::Rsa3072Sha256 => Self::Rsa3072Sha256,
        }
    }
}

impl From<PersistedAlgorithm> for SignatureAlgorithm {
    fn from(algorithm: PersistedAlgorithm) -> Self {
        match algorithm {
            PersistedAlgorithm::EcdsaP256Sha256 => Self::EcdsaP256Sha256,
            PersistedAlgorithm::EcdsaP384Sha384 => Self::EcdsaP384Sha384,
            PersistedAlgorithm::Rsa2048Sha256 => Self::Rsa2048Sha256,
            PersistedAlgorithm::Rsa3072Sha256 => Self::Rsa3072Sha256,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PersistedSlot {
    key_handle: Vec<u8>,
    algorithm: PersistedAlgorithm,
    public_key: Vec<u8>,
    /// The certificate this slot held immediately before the in-flight renewal, if any - what
    /// [`discard_certificate_renewal`] restores.
    backup_pem: Option<String>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct PersistedRenewalState {
    // Two named fields rather than an array indexed by
    // `CertificateSigningPurpose` - that enum's slot indexing is a private implementation detail
    // of `crate::certificates::PendingSignRequests`, not something this module reaches for.
    charging_station: Option<PersistedSlot>,
    v2g: Option<PersistedSlot>,
}

impl PersistedRenewalState {
    fn slot(&self, purpose: CertificateSigningPurpose) -> &Option<PersistedSlot> {
        match purpose {
            CertificateSigningPurpose::ChargingStationCertificate => &self.charging_station,
            CertificateSigningPurpose::V2GCertificate => &self.v2g,
        }
    }

    fn slot_mut(&mut self, purpose: CertificateSigningPurpose) -> &mut Option<PersistedSlot> {
        match purpose {
            CertificateSigningPurpose::ChargingStationCertificate => &mut self.charging_station,
            CertificateSigningPurpose::V2GCertificate => &mut self.v2g,
        }
    }
}

/// Remembers, per [`CertificateSigningPurpose`], the key pair this charge point last used to
/// request a certificate for that purpose, plus (while a renewal is in flight) the certificate it
/// is renewing away from.
///
/// Bounded by construction - exactly two slots, one per [`CertificateSigningPurpose`] - the same
/// "no runtime limit needed" reasoning
/// [`crate::certificates::PendingSignRequests`]'s docs give: a charge point only ever has one
/// current key per certificate purpose, so there is nothing here a remote peer could grow (G2.2).
///
/// Persisted over [`Storage`] (E2.9): a charge point that forgot this association on every
/// reboot would generate a fresh key pair for a purpose it already had one for the moment it
/// restarted mid-renewal, silently rotating far more often than [`RenewalPolicy`] intended.
///
/// `Clone` when `S` is (e.g. an `Arc<InMemoryStorage>`), the same way [`Storage`] implementors
/// throughout this crate are cheaply shared - the store itself carries no state beyond the
/// storage handle.
#[derive(Clone)]
pub struct RenewalStore<S> {
    storage: S,
}

impl<S: Storage> RenewalStore<S> {
    /// A store over `storage`.
    pub fn new(storage: S) -> Self {
        Self { storage }
    }

    async fn load(&self) -> PersistedRenewalState {
        match self.storage.get(RENEWAL_KEY).await {
            Ok(Some(encoded)) => serde_json::from_slice(&encoded).unwrap_or_else(|error| {
                tracing::warn!(%error, "discarding a corrupt certificate renewal store");
                PersistedRenewalState::default()
            }),
            Ok(None) => PersistedRenewalState::default(),
            Err(error) => {
                tracing::warn!(%error, "could not read the certificate renewal store");
                PersistedRenewalState::default()
            }
        }
    }

    async fn save(&self, state: &PersistedRenewalState) -> bool {
        let Ok(encoded) = serde_json::to_vec(state) else {
            return false;
        };
        match self.storage.set(RENEWAL_KEY, &encoded).await {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(%error, "could not write the certificate renewal store");
                false
            }
        }
    }

    /// The key pair this store last recorded for `purpose`, if any.
    ///
    /// [`renew_certificate`] treats `None` as "no certificate has ever been requested for this
    /// purpose through this store" and generates a fresh key pair; `Some` means an existing
    /// certificate's key should be reused rather than rotated - the safer default, since rotating
    /// on every renewal would mean a CSMS that revokes the *old* certificate mid-renewal (a
    /// legitimate thing to do) also invalidates the very key the replacement is being requested
    /// for.
    pub async fn key_pair_for(
        &self,
        purpose: CertificateSigningPurpose,
    ) -> Option<GeneratedKeyPair> {
        self.load()
            .await
            .slot(purpose)
            .as_ref()
            .map(|slot| GeneratedKeyPair {
                handle: KeyHandle::new(slot.key_handle.clone()),
                public_key: PublicKey {
                    algorithm: slot.algorithm.into(),
                    bytes: slot.public_key.clone(),
                },
            })
    }

    /// Records `key` as the key pair to use for `purpose` from now on, overwriting whatever was
    /// recorded before it. Preserves any stashed backup already recorded for `purpose` - recording
    /// the key pair and stashing a backup are independent steps in
    /// [`run_certificate_renewal`]'s sequence, and neither should erase the other.
    pub async fn record_key_pair(
        &self,
        purpose: CertificateSigningPurpose,
        key: &GeneratedKeyPair,
    ) {
        let mut state = self.load().await;
        let backup_pem = state
            .slot(purpose)
            .as_ref()
            .and_then(|slot| slot.backup_pem.clone());
        *state.slot_mut(purpose) = Some(PersistedSlot {
            key_handle: key.handle.as_bytes().to_vec(),
            algorithm: key.public_key.algorithm.into(),
            public_key: key.public_key.bytes.clone(),
            backup_pem,
        });
        self.save(&state).await;
    }

    /// Stashes `backup_pem` as the certificate to restore for `purpose` if the renewal about to
    /// be requested must later be [discarded](discard_certificate_renewal). A no-op if this store
    /// has no key pair recorded for `purpose` yet - there is nothing to renew *from*, so nothing
    /// to fail back to.
    pub async fn stash_backup(&self, purpose: CertificateSigningPurpose, backup_pem: String) {
        let mut state = self.load().await;
        if let Some(slot) = state.slot_mut(purpose) {
            slot.backup_pem = Some(backup_pem);
            self.save(&state).await;
        }
    }

    /// Removes and returns the backup stashed for `purpose`, if any - the certificate
    /// [`discard_certificate_renewal`] restores.
    pub async fn take_backup(&self, purpose: CertificateSigningPurpose) -> Option<String> {
        let mut state = self.load().await;
        let backup = state
            .slot_mut(purpose)
            .as_mut()
            .and_then(|slot| slot.backup_pem.take());
        if backup.is_some() {
            self.save(&state).await;
        }
        backup
    }

    /// Clears the backup stashed for `purpose` without restoring it - call once a renewal is
    /// confirmed working (e.g. the charge point has successfully reconnected using the new
    /// certificate), so a later renewal cannot reach back further than the certificate now
    /// actually in place.
    pub async fn confirm(&self, purpose: CertificateSigningPurpose) {
        let mut state = self.load().await;
        if let Some(slot) = state.slot_mut(purpose)
            && slot.backup_pem.take().is_some()
        {
            self.save(&state).await;
        }
    }
}

// --- Requesting a renewal, reusing the existing key --------------------------------------------

/// Failure [`renew_certificate`] can report: either this store's [`KeyStore`] could not generate
/// a fresh key pair (only reached when [`RenewalStore`] has none recorded yet for the purpose), or
/// the CSR/`SignCertificate` round trip itself failed - see [`SignCertificateError`].
#[derive(Debug)]
pub enum RenewalError<K, C> {
    /// [`KeyStore::generate_key_pair`] failed while establishing this purpose's first key pair.
    KeyGeneration(K),
    /// Building the CSR or sending `SignCertificate` failed - see [`SignCertificateError`].
    Sign(SignCertificateError<K, C>),
}

impl<K: core::fmt::Display, C: core::fmt::Display> core::fmt::Display for RenewalError<K, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::KeyGeneration(error) => {
                write!(f, "could not generate a key pair for renewal: {error}")
            }
            Self::Sign(error) => write!(f, "{error}"),
        }
    }
}

impl<K: core::fmt::Debug + core::fmt::Display, C: core::fmt::Debug + core::fmt::Display>
    core::error::Error for RenewalError<K, C>
{
}

/// Requests a certificate for `purpose`, signing with the key `renewals` already has on file for
/// it - or, if none is recorded yet, a freshly generated one (recorded on success so the *next*
/// call reuses it). This is the one piece [`crate::certificates::csr`]'s module docs left for
/// F3.2 to add: the same key as the certificate being replaced, not a new one.
///
/// On [`SignCertificateOutcome::Accepted`], records the key pair used (a no-op if it was already
/// the recorded one). On [`SignCertificateOutcome::Rejected`] or an error, `renewals` is left
/// untouched - a rejected renewal must not disturb the association a *future* renewal attempt
/// needs, and per the module docs, nothing in the certificate store has changed either.
pub async fn renew_certificate<K, St, R, ClientErr>(
    key_store: &K,
    renewals: &RenewalStore<St>,
    requester: &R,
    subject: &CsrSubject,
    purpose: CertificateSigningPurpose,
    default_algorithm: SignatureAlgorithm,
) -> Result<SignCertificateOutcome, RenewalError<K::Error, ClientErr>>
where
    K: KeyStore + Send + Sync,
    St: Storage,
    R: CertificateSigningRequester<ClientErr>,
{
    let key = match renewals.key_pair_for(purpose).await {
        Some(existing) => existing,
        None => key_store
            .generate_key_pair(default_algorithm)
            .await
            .map_err(RenewalError::KeyGeneration)?,
    };

    let outcome = requester
        .sign_certificate(key_store, &key, subject, purpose)
        .await
        .map_err(RenewalError::Sign)?;

    if outcome == SignCertificateOutcome::Accepted {
        renewals.record_key_pair(purpose, &key).await;
    }

    Ok(outcome)
}

// --- The periodic sweep --------------------------------------------------------------------

/// Runs [`renew_certificate`] for every [`CertificateSigningPurpose`] whose certificate
/// [`CertificateStore::expires_at`] reports as due under `policy`, every `interval_secs`.
///
/// Before requesting, stashes the currently-installed certificate on `renewals` as the backup
/// [`discard_certificate_renewal`] would restore - see the module docs for why that makes the
/// whole renewal fail-safe.
///
/// **Skipped entirely while the clock is unsynchronized**, mirroring
/// [`crate::reservation::run_reservation_expiry`] exactly - see the module docs.
///
/// A store that answers [`CertificateStore::expires_at`] with `Ok(None)` (no certificate
/// installed for that purpose, or an integrator that has not wired expiry) or `Err` is skipped
/// for that sweep pass without treating either as due: an unknown expiry must never be treated as
/// an overdue one.
// Ten parameters: every collaborator this sweep needs (clock, both stores, key store, requester,
// subject builder, algorithm, policy, backoff, interval) - splitting them into a config struct
// would only move the same count into field initializers at every call site, per the same
// trade-off `crate::firmware`/`crate::persistence` already accept for their own many-argument
// entry points (see their own `too_many_arguments` allows).
#[allow(clippy::too_many_arguments)]
pub async fn run_certificate_renewal<Cl, S, K, St, R, ClientErr, B>(
    clock: &Cl,
    certificate_store: &S,
    key_store: &K,
    renewals: &RenewalStore<St>,
    requester: &R,
    subject_for: impl Fn(CertificateSigningPurpose) -> CsrSubject,
    default_algorithm: SignatureAlgorithm,
    policy: &RenewalPolicy,
    backoff: &B,
    interval_secs: u32,
) where
    Cl: Clock,
    S: CertificateStore + Send + Sync,
    K: KeyStore + Send + Sync,
    St: Storage,
    R: CertificateSigningRequester<ClientErr>,
    ClientErr: core::fmt::Display,
    B: crate::provisioning::Backoff,
{
    loop {
        backoff.wait(interval_secs.max(1)).await;
        let now = clock.now();
        if !crate::clock::is_synchronized(&now) {
            tracing::debug!(
                "skipping the certificate renewal sweep: the clock is not synchronized"
            );
            continue;
        }

        for purpose in [
            CertificateSigningPurpose::ChargingStationCertificate,
            CertificateSigningPurpose::V2GCertificate,
        ] {
            let use_for = purpose.certificate_use();
            let expires_at = match certificate_store.expires_at(use_for).await {
                Ok(Some(expiry)) => expiry,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(?purpose, %error, "could not read this certificate's expiry");
                    continue;
                }
            };
            if !is_due_for_renewal(now, expires_at, policy) {
                continue;
            }

            if let Ok(Some(current_pem)) = certificate_store.certificate_chain_pem(use_for).await {
                renewals.stash_backup(purpose, current_pem).await;
            }

            match renew_certificate(
                key_store,
                renewals,
                requester,
                &subject_for(purpose),
                purpose,
                default_algorithm,
            )
            .await
            {
                Ok(SignCertificateOutcome::Accepted) => {
                    tracing::info!(?purpose, "requested a renewed certificate ahead of expiry");
                }
                Ok(SignCertificateOutcome::Rejected) => {
                    tracing::warn!(?purpose, "the CSMS rejected a certificate renewal request");
                }
                Err(error) => {
                    tracing::warn!(?purpose, %error, "failed to request a certificate renewal");
                }
            }
        }
    }
}

// --- Confirming or discarding an installed renewal --------------------------------------------

/// Confirms that the certificate just renewed for `purpose` is working (e.g. this charge point
/// has reconnected using it), clearing the stashed backup so a future
/// [`discard_certificate_renewal`] cannot reach back further than the certificate now in place.
pub async fn confirm_certificate_renewal<S: Storage>(
    renewals: &RenewalStore<S>,
    purpose: CertificateSigningPurpose,
) {
    renewals.confirm(purpose).await;
}

/// Reports that the certificate just renewed for `purpose` could not be used to connect at all,
/// restoring the previous, still-valid certificate [`RenewalStore`] stashed before the renewal was
/// requested, and reporting
/// [`SecurityEventType::DiscardedRenewedClientCertificate`] - OCPP's security event for exactly
/// this: a renewal that did not take, with the charge point still running on the one before it.
///
/// Returns `false` (without touching the store or reporting anything) if `renewals` has no
/// stashed backup for `purpose` - either nothing was ever renewed, or a previous discard/confirm
/// already resolved it.
pub async fn discard_certificate_renewal<S, C>(
    actor: &ChargePointActor,
    renewals: &RenewalStore<S>,
    certificate_store: &C,
    purpose: CertificateSigningPurpose,
) -> bool
where
    S: Storage,
    C: CertificateStore,
{
    let Some(backup_pem) = renewals.take_backup(purpose).await else {
        return false;
    };

    let restored = certificate_store
        .install(purpose.certificate_use(), &backup_pem)
        .await;
    let restored_ok = matches!(restored, Ok(InstallCertificateOutcome::Accepted));
    if !restored_ok {
        tracing::warn!(
            ?purpose,
            "could not restore the previous certificate after discarding a renewal"
        );
    }

    crate::security::report_security_event(
        actor,
        SecurityEvent {
            event_type: SecurityEventType::DiscardedRenewedClientCertificate,
            tech_info: Some("a renewed certificate could not be used to connect".into()),
        },
    )
    .await;

    restored_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::{InMemoryStorage, KeyStoreBacking, NoCertificateStore};
    use alloc::sync::Arc;

    fn at(rfc3339: &str) -> DateTime<Utc> {
        rfc3339.parse().unwrap()
    }

    // --- RenewalPolicy / is_due_for_renewal -----------------------------------------------

    #[test]
    fn the_default_policy_renews_thirty_days_ahead_of_expiry() {
        assert_eq!(
            RenewalPolicy::default().renew_before(),
            Duration::days(DEFAULT_RENEW_BEFORE_DAYS)
        );
    }

    #[test]
    fn a_negative_renew_before_is_clamped_to_zero() {
        let policy = RenewalPolicy::new(Duration::days(-5));
        assert_eq!(policy.renew_before(), Duration::zero());
    }

    #[test]
    fn a_certificate_well_before_its_threshold_is_not_due() {
        let policy = RenewalPolicy::new(Duration::days(30));
        assert!(!is_due_for_renewal(
            at("2030-01-01T00:00:00Z"),
            at("2030-06-01T00:00:00Z"),
            &policy
        ));
    }

    #[test]
    fn a_certificate_exactly_at_its_threshold_is_due() {
        let policy = RenewalPolicy::new(Duration::days(30));
        assert!(is_due_for_renewal(
            at("2030-05-02T00:00:00Z"),
            at("2030-06-01T00:00:00Z"),
            &policy
        ));
    }

    #[test]
    fn an_already_expired_certificate_is_due() {
        let policy = RenewalPolicy::new(Duration::days(30));
        assert!(is_due_for_renewal(
            at("2030-07-01T00:00:00Z"),
            at("2030-06-01T00:00:00Z"),
            &policy
        ));
    }

    // --- RenewalStore ----------------------------------------------------------------------

    fn store() -> RenewalStore<Arc<InMemoryStorage>> {
        RenewalStore::new(Arc::new(InMemoryStorage::new()))
    }

    fn key(bytes: &[u8]) -> GeneratedKeyPair {
        GeneratedKeyPair {
            handle: KeyHandle::new(bytes.to_vec()),
            public_key: PublicKey {
                algorithm: SignatureAlgorithm::EcdsaP256Sha256,
                bytes: alloc::vec![0xAA, 0xBB],
            },
        }
    }

    #[tokio::test]
    async fn a_store_with_nothing_recorded_has_no_key_pair_for_either_purpose() {
        let store = store();
        assert_eq!(
            store
                .key_pair_for(CertificateSigningPurpose::ChargingStationCertificate)
                .await,
            None
        );
        assert_eq!(
            store
                .key_pair_for(CertificateSigningPurpose::V2GCertificate)
                .await,
            None
        );
    }

    #[tokio::test]
    async fn a_recorded_key_pair_round_trips() {
        let store = store();
        let generated = key(b"key-1");
        store
            .record_key_pair(
                CertificateSigningPurpose::ChargingStationCertificate,
                &generated,
            )
            .await;

        assert_eq!(
            store
                .key_pair_for(CertificateSigningPurpose::ChargingStationCertificate)
                .await,
            Some(generated)
        );
        // The other purpose's slot is untouched.
        assert_eq!(
            store
                .key_pair_for(CertificateSigningPurpose::V2GCertificate)
                .await,
            None
        );
    }

    #[tokio::test]
    async fn recording_a_key_pair_survives_a_reboot() {
        let storage = Arc::new(InMemoryStorage::new());
        let before = RenewalStore::new(storage.clone());
        let generated = key(b"key-1");
        before
            .record_key_pair(
                CertificateSigningPurpose::ChargingStationCertificate,
                &generated,
            )
            .await;

        // --- the cut: only `storage` survives.
        let after = RenewalStore::new(storage);
        assert_eq!(
            after
                .key_pair_for(CertificateSigningPurpose::ChargingStationCertificate)
                .await,
            Some(generated)
        );
    }

    #[tokio::test]
    async fn recording_a_new_key_pair_replaces_the_old_one_for_the_same_purpose() {
        let store = store();
        store
            .record_key_pair(
                CertificateSigningPurpose::ChargingStationCertificate,
                &key(b"key-1"),
            )
            .await;
        store
            .record_key_pair(
                CertificateSigningPurpose::ChargingStationCertificate,
                &key(b"key-2"),
            )
            .await;

        assert_eq!(
            store
                .key_pair_for(CertificateSigningPurpose::ChargingStationCertificate)
                .await,
            Some(key(b"key-2"))
        );
    }

    #[tokio::test]
    async fn stashing_a_backup_with_no_key_pair_recorded_yet_is_a_no_op() {
        // Nothing to renew *from* - there is nothing to fail back to either.
        let store = store();
        store
            .stash_backup(
                CertificateSigningPurpose::ChargingStationCertificate,
                "pem".into(),
            )
            .await;

        assert_eq!(
            store
                .take_backup(CertificateSigningPurpose::ChargingStationCertificate)
                .await,
            None
        );
    }

    #[tokio::test]
    async fn a_stashed_backup_round_trips_and_is_removed_once_taken() {
        let store = store();
        store
            .record_key_pair(
                CertificateSigningPurpose::ChargingStationCertificate,
                &key(b"key-1"),
            )
            .await;
        store
            .stash_backup(
                CertificateSigningPurpose::ChargingStationCertificate,
                "old-pem".into(),
            )
            .await;

        assert_eq!(
            store
                .take_backup(CertificateSigningPurpose::ChargingStationCertificate)
                .await,
            Some("old-pem".into())
        );
        // Taken, not merely peeked - a second discard must find nothing to restore.
        assert_eq!(
            store
                .take_backup(CertificateSigningPurpose::ChargingStationCertificate)
                .await,
            None
        );
    }

    #[tokio::test]
    async fn confirming_a_renewal_clears_the_backup_without_returning_it() {
        let store = store();
        store
            .record_key_pair(
                CertificateSigningPurpose::ChargingStationCertificate,
                &key(b"key-1"),
            )
            .await;
        store
            .stash_backup(
                CertificateSigningPurpose::ChargingStationCertificate,
                "old-pem".into(),
            )
            .await;

        store
            .confirm(CertificateSigningPurpose::ChargingStationCertificate)
            .await;

        assert_eq!(
            store
                .take_backup(CertificateSigningPurpose::ChargingStationCertificate)
                .await,
            None
        );
        // The key pair itself is unaffected by confirming.
        assert_eq!(
            store
                .key_pair_for(CertificateSigningPurpose::ChargingStationCertificate)
                .await,
            Some(key(b"key-1"))
        );
    }

    #[tokio::test]
    async fn recording_a_new_key_pair_preserves_an_already_stashed_backup() {
        let store = store();
        store
            .record_key_pair(
                CertificateSigningPurpose::ChargingStationCertificate,
                &key(b"key-1"),
            )
            .await;
        store
            .stash_backup(
                CertificateSigningPurpose::ChargingStationCertificate,
                "old-pem".into(),
            )
            .await;

        // Reusing the same key for the renewal itself re-records it - this must not wipe the
        // backup that was just stashed for the same purpose.
        store
            .record_key_pair(
                CertificateSigningPurpose::ChargingStationCertificate,
                &key(b"key-1"),
            )
            .await;

        assert_eq!(
            store
                .take_backup(CertificateSigningPurpose::ChargingStationCertificate)
                .await,
            Some("old-pem".into())
        );
    }

    // --- renew_certificate -------------------------------------------------------------------

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct FakeKeyStoreError;
    impl core::fmt::Display for FakeKeyStoreError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("fake key store error")
        }
    }
    impl core::error::Error for FakeKeyStoreError {}

    struct FakeKeyStore {
        next_id: core::sync::atomic::AtomicU8,
    }

    impl Default for FakeKeyStore {
        fn default() -> Self {
            Self {
                next_id: core::sync::atomic::AtomicU8::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl KeyStore for FakeKeyStore {
        type Error = FakeKeyStoreError;

        async fn generate_key_pair(
            &self,
            algorithm: SignatureAlgorithm,
        ) -> Result<GeneratedKeyPair, Self::Error> {
            let id = self
                .next_id
                .fetch_add(1, core::sync::atomic::Ordering::SeqCst);
            Ok(GeneratedKeyPair {
                handle: KeyHandle::new(alloc::vec![id]),
                public_key: PublicKey {
                    algorithm,
                    bytes: alloc::vec![id, id],
                },
            })
        }

        async fn sign(&self, _handle: &KeyHandle, _digest: &[u8]) -> Result<Vec<u8>, Self::Error> {
            Ok(alloc::vec![0xAA])
        }

        async fn delete_key_pair(&self, _handle: &KeyHandle) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn supported_algorithms(&self) -> Result<Vec<SignatureAlgorithm>, Self::Error> {
            Ok(alloc::vec![SignatureAlgorithm::EcdsaP256Sha256])
        }

        async fn backing(&self) -> Result<KeyStoreBacking, Self::Error> {
            Ok(KeyStoreBacking::Software)
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct FakeClientError;
    impl core::fmt::Display for FakeClientError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("fake client error")
        }
    }

    /// A [`CertificateSigningRequester`] that records the [`KeyHandle`] it was asked to sign with
    /// on every call, and answers with a fixed, caller-chosen outcome - enough to prove
    /// [`renew_certificate`]'s key-reuse behaviour without a real protocol adapter.
    struct FakeRequester {
        outcome: SignCertificateOutcome,
        seen_handles: std::sync::Mutex<Vec<KeyHandle>>,
    }

    impl FakeRequester {
        fn accepting() -> Self {
            Self {
                outcome: SignCertificateOutcome::Accepted,
                seen_handles: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn rejecting() -> Self {
            Self {
                outcome: SignCertificateOutcome::Rejected,
                seen_handles: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl CertificateSigningRequester<FakeClientError> for FakeRequester {
        async fn sign_certificate<K>(
            &self,
            _key_store: &K,
            key: &GeneratedKeyPair,
            _subject: &CsrSubject,
            _purpose: CertificateSigningPurpose,
        ) -> Result<SignCertificateOutcome, SignCertificateError<K::Error, FakeClientError>>
        where
            K: KeyStore + Send + Sync,
        {
            self.seen_handles.lock().unwrap().push(key.handle.clone());
            Ok(self.outcome)
        }
    }

    #[tokio::test]
    async fn the_first_renewal_generates_a_key_pair_and_records_it_on_acceptance() {
        let key_store = FakeKeyStore::default();
        let renewals = store();
        let requester = FakeRequester::accepting();

        let outcome = renew_certificate(
            &key_store,
            &renewals,
            &requester,
            &CsrSubject::new("CP-0001"),
            CertificateSigningPurpose::ChargingStationCertificate,
            SignatureAlgorithm::EcdsaP256Sha256,
        )
        .await
        .unwrap();

        assert_eq!(outcome, SignCertificateOutcome::Accepted);
        assert!(
            renewals
                .key_pair_for(CertificateSigningPurpose::ChargingStationCertificate)
                .await
                .is_some(),
            "an accepted request must record the key pair used"
        );
    }

    #[tokio::test]
    async fn a_second_renewal_reuses_the_previously_recorded_key_rather_than_generating_a_new_one()
    {
        let key_store = FakeKeyStore::default();
        let renewals = store();
        let requester = FakeRequester::accepting();

        renew_certificate(
            &key_store,
            &renewals,
            &requester,
            &CsrSubject::new("CP-0001"),
            CertificateSigningPurpose::ChargingStationCertificate,
            SignatureAlgorithm::EcdsaP256Sha256,
        )
        .await
        .unwrap();
        let first_key = renewals
            .key_pair_for(CertificateSigningPurpose::ChargingStationCertificate)
            .await
            .unwrap();

        renew_certificate(
            &key_store,
            &renewals,
            &requester,
            &CsrSubject::new("CP-0001"),
            CertificateSigningPurpose::ChargingStationCertificate,
            SignatureAlgorithm::EcdsaP256Sha256,
        )
        .await
        .unwrap();

        let seen = requester.seen_handles.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0], first_key.handle);
        assert_eq!(
            seen[1], first_key.handle,
            "the renewal must sign with the same key as the certificate it replaces"
        );
    }

    #[tokio::test]
    async fn a_rejected_renewal_does_not_disturb_the_recorded_key_pair() {
        let key_store = FakeKeyStore::default();
        let renewals = store();
        renewals
            .record_key_pair(
                CertificateSigningPurpose::ChargingStationCertificate,
                &key(b"existing"),
            )
            .await;
        let requester = FakeRequester::rejecting();

        let outcome = renew_certificate(
            &key_store,
            &renewals,
            &requester,
            &CsrSubject::new("CP-0001"),
            CertificateSigningPurpose::ChargingStationCertificate,
            SignatureAlgorithm::EcdsaP256Sha256,
        )
        .await
        .unwrap();

        assert_eq!(outcome, SignCertificateOutcome::Rejected);
        assert_eq!(
            renewals
                .key_pair_for(CertificateSigningPurpose::ChargingStationCertificate)
                .await,
            Some(key(b"existing")),
            "a rejected renewal must not disturb the key a future attempt needs"
        );
    }

    // --- run_certificate_renewal -------------------------------------------------------------

    #[derive(Clone)]
    struct FixedClock(DateTime<Utc>);
    impl Clock for FixedClock {
        fn now(&self) -> DateTime<Utc> {
            self.0
        }
    }

    /// A [`CertificateStore`] reporting a fixed, caller-chosen expiry for
    /// [`CertificateUse::ChargingStation`] and nothing for every other use - enough to drive
    /// [`run_certificate_renewal`]'s due/not-due branch without a real store.
    struct FixedExpiryStore {
        expires_at: DateTime<Utc>,
        pem: String,
    }

    #[async_trait::async_trait]
    impl CertificateStore for FixedExpiryStore {
        type Error = crate::hardware::NoCertificateStoreError;

        async fn install(
            &self,
            _use_for: crate::hardware::CertificateUse,
            _certificate: &str,
        ) -> Result<InstallCertificateOutcome, Self::Error> {
            Ok(InstallCertificateOutcome::Accepted)
        }

        async fn delete(
            &self,
            _hash_data: &crate::hardware::CertificateHashData,
        ) -> Result<crate::hardware::DeleteCertificateOutcome, Self::Error> {
            Ok(crate::hardware::DeleteCertificateOutcome::NotFound)
        }

        async fn installed(
            &self,
            _uses: &[crate::hardware::CertificateUse],
        ) -> Result<Vec<crate::hardware::InstalledCertificate>, Self::Error> {
            Ok(Vec::new())
        }

        async fn has_client_private_key(&self) -> Result<bool, Self::Error> {
            Ok(true)
        }

        async fn certificate_chain_pem(
            &self,
            use_for: crate::hardware::CertificateUse,
        ) -> Result<Option<String>, Self::Error> {
            Ok(
                (use_for == crate::hardware::CertificateUse::ChargingStation)
                    .then(|| self.pem.clone()),
            )
        }

        async fn expires_at(
            &self,
            use_for: crate::hardware::CertificateUse,
        ) -> Result<Option<DateTime<Utc>>, Self::Error> {
            Ok(
                (use_for == crate::hardware::CertificateUse::ChargingStation)
                    .then_some(self.expires_at),
            )
        }
    }

    /// A [`crate::provisioning::Backoff`] that waits almost nothing, so tests don't spend real
    /// wall time on the sweep's own delay - mirroring the pattern the reservation-expiry tests
    /// would use were they not exercising `TokioBackoff` directly; here the sweep interval is
    /// irrelevant to what's under test. **A real (if tiny) sleep, not a no-op**: the sweep loop
    /// otherwise never awaits anything that actually suspends (every store/key-store/requester
    /// call below resolves immediately), which starves a current-thread Tokio runtime's other
    /// tasks - including the polling `tokio::time::sleep` these tests use to wait for a result -
    /// in a tight busy loop.
    struct NoBackoff;
    #[async_trait::async_trait]
    impl crate::provisioning::Backoff for NoBackoff {
        async fn wait(&self, _seconds: u32) {
            tokio::time::sleep(core::time::Duration::from_millis(1)).await;
        }
    }

    #[tokio::test]
    async fn a_certificate_due_for_renewal_is_renewed() {
        let key_store = FakeKeyStore::default();
        let renewals = store();
        let requester = Arc::new(FakeRequester::accepting());
        let certificate_store = FixedExpiryStore {
            expires_at: at("2030-01-15T00:00:00Z"),
            pem: "old-pem".into(),
        };
        let clock = FixedClock(at("2030-01-01T00:00:00Z")); // 14 days out, inside a 30-day policy
        let policy = RenewalPolicy::default();

        let sweeping = {
            let requester = requester.clone();
            tokio::spawn(async move {
                run_certificate_renewal(
                    &clock,
                    &certificate_store,
                    &key_store,
                    &renewals,
                    &*requester,
                    |_purpose| CsrSubject::new("CP-0001"),
                    SignatureAlgorithm::EcdsaP256Sha256,
                    &policy,
                    &NoBackoff,
                    1,
                )
                .await;
            })
        };

        for _ in 0..50 {
            if !requester.seen_handles.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(core::time::Duration::from_millis(10)).await;
        }
        assert!(
            !requester.seen_handles.lock().unwrap().is_empty(),
            "a certificate 14 days from expiry under a 30-day policy must be renewed"
        );
        sweeping.abort();
    }

    #[tokio::test]
    async fn a_certificate_nowhere_near_expiry_is_left_alone() {
        let key_store = FakeKeyStore::default();
        let renewals = store();
        let requester = Arc::new(FakeRequester::accepting());
        let certificate_store = FixedExpiryStore {
            expires_at: at("2031-01-01T00:00:00Z"), // a year out
            pem: "old-pem".into(),
        };
        let clock = FixedClock(at("2030-01-01T00:00:00Z"));
        let policy = RenewalPolicy::default();

        let sweeping = {
            let requester = requester.clone();
            tokio::spawn(async move {
                run_certificate_renewal(
                    &clock,
                    &certificate_store,
                    &key_store,
                    &renewals,
                    &*requester,
                    |_purpose| CsrSubject::new("CP-0001"),
                    SignatureAlgorithm::EcdsaP256Sha256,
                    &policy,
                    &NoBackoff,
                    1,
                )
                .await;
            })
        };
        tokio::time::sleep(core::time::Duration::from_millis(200)).await;

        assert!(requester.seen_handles.lock().unwrap().is_empty());
        sweeping.abort();
    }

    #[tokio::test]
    async fn an_unsynchronized_clock_renews_nothing() {
        let key_store = FakeKeyStore::default();
        let renewals = store();
        let requester = Arc::new(FakeRequester::accepting());
        let certificate_store = FixedExpiryStore {
            // Already expired - the sweep would renew it if the clock guard were not there.
            expires_at: at("2010-01-01T00:00:00Z"),
            pem: "old-pem".into(),
        };
        // Before `clock::unsynchronized_before` - not to be trusted.
        let clock = FixedClock(at("2010-06-01T00:00:00Z"));
        let policy = RenewalPolicy::default();

        let sweeping = {
            let requester = requester.clone();
            tokio::spawn(async move {
                run_certificate_renewal(
                    &clock,
                    &certificate_store,
                    &key_store,
                    &renewals,
                    &*requester,
                    |_purpose| CsrSubject::new("CP-0001"),
                    SignatureAlgorithm::EcdsaP256Sha256,
                    &policy,
                    &NoBackoff,
                    1,
                )
                .await;
            })
        };
        tokio::time::sleep(core::time::Duration::from_millis(200)).await;

        assert!(requester.seen_handles.lock().unwrap().is_empty());
        sweeping.abort();
    }

    #[tokio::test]
    async fn the_sweep_stashes_a_backup_before_requesting_a_renewal() {
        let key_store = FakeKeyStore::default();
        let renewals = store();
        let requester = Arc::new(FakeRequester::accepting());
        // The store already has a key pair (a "certificate already installed" scenario), so a
        // backup can be meaningfully stashed for it.
        renewals
            .record_key_pair(
                CertificateSigningPurpose::ChargingStationCertificate,
                &key(b"existing"),
            )
            .await;
        let certificate_store = FixedExpiryStore {
            expires_at: at("2030-01-15T00:00:00Z"),
            pem: "current-pem".into(),
        };
        let clock = FixedClock(at("2030-01-01T00:00:00Z"));
        let policy = RenewalPolicy::default();

        let sweeping = {
            let requester = requester.clone();
            let renewals = renewals.clone();
            tokio::spawn(async move {
                run_certificate_renewal(
                    &clock,
                    &certificate_store,
                    &key_store,
                    &renewals,
                    &*requester,
                    |_purpose| CsrSubject::new("CP-0001"),
                    SignatureAlgorithm::EcdsaP256Sha256,
                    &policy,
                    &NoBackoff,
                    1,
                )
                .await;
            })
        };

        for _ in 0..50 {
            if !requester.seen_handles.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(core::time::Duration::from_millis(10)).await;
        }
        sweeping.abort();

        assert_eq!(
            renewals
                .take_backup(CertificateSigningPurpose::ChargingStationCertificate)
                .await,
            Some("current-pem".into())
        );
    }

    // --- confirm/discard ---------------------------------------------------------------------

    async fn actor_for_tests() -> ChargePointActor {
        ChargePointActor::spawn([1], &crate::executor::TokioExecutor)
    }

    #[tokio::test]
    async fn discarding_with_nothing_stashed_reports_nothing_and_returns_false() {
        let actor = actor_for_tests().await;
        let renewals = store();
        let certificate_store = NoCertificateStore;

        let restored = discard_certificate_renewal(
            &actor,
            &renewals,
            &certificate_store,
            CertificateSigningPurpose::ChargingStationCertificate,
        )
        .await;

        assert!(!restored);
    }

    #[tokio::test]
    async fn discarding_a_renewal_restores_the_backup_and_reports_the_security_event() {
        let actor = actor_for_tests().await;
        let renewals = store();
        renewals
            .record_key_pair(
                CertificateSigningPurpose::ChargingStationCertificate,
                &key(b"existing"),
            )
            .await;
        renewals
            .stash_backup(
                CertificateSigningPurpose::ChargingStationCertificate,
                "old-working-pem".into(),
            )
            .await;
        let certificate_store =
            crate::hardware::StoredCertificates::new(Arc::new(InMemoryStorage::new()));

        let restored = discard_certificate_renewal(
            &actor,
            &renewals,
            &certificate_store,
            CertificateSigningPurpose::ChargingStationCertificate,
        )
        .await;

        assert!(restored);
        assert_eq!(
            certificate_store
                .certificate_chain_pem(crate::hardware::CertificateUse::ChargingStation)
                .await
                .unwrap(),
            Some("old-working-pem".into())
        );
        // The backup is consumed - a second discard finds nothing left to restore.
        assert!(
            !discard_certificate_renewal(
                &actor,
                &renewals,
                &certificate_store,
                CertificateSigningPurpose::ChargingStationCertificate,
            )
            .await
        );
    }

    #[tokio::test]
    async fn confirming_a_renewal_leaves_nothing_for_a_later_discard_to_restore() {
        let actor = actor_for_tests().await;
        let renewals = store();
        renewals
            .record_key_pair(
                CertificateSigningPurpose::ChargingStationCertificate,
                &key(b"existing"),
            )
            .await;
        renewals
            .stash_backup(
                CertificateSigningPurpose::ChargingStationCertificate,
                "old-pem".into(),
            )
            .await;

        confirm_certificate_renewal(
            &renewals,
            CertificateSigningPurpose::ChargingStationCertificate,
        )
        .await;

        let certificate_store = NoCertificateStore;
        assert!(
            !discard_certificate_renewal(
                &actor,
                &renewals,
                &certificate_store,
                CertificateSigningPurpose::ChargingStationCertificate,
            )
            .await
        );
    }
}
