//! Key storage: where a charge point's private keys live (`docs/PRODUCTION-ROADMAP.md` F2.4).
//!
//! # The invariant this whole module exists to protect
//!
//! **A private key must never be required to leave the element that holds it.** A design where
//! this trait handed back key bytes and this crate signed with them would defeat the entire
//! point: an integrator with a real secure element (an ATECC608, a TPM, an SE050) *physically
//! cannot* export the key, and an API demanding it would exclude exactly the hardware this module
//! exists to serve. So the operations are the ones that make sense on the *other* side of that
//! boundary: generate a keypair and get back only the public half plus an opaque handle
//! ([`KeyStore::generate_key_pair`]); ask the element to sign a digest with the key behind a
//! handle ([`KeyStore::sign`]); ask what it can do at all ([`KeyStore::supported_algorithms`]).
//! Nothing in this trait has a way to return a private key, and **no future addition to it
//! should either** - a method that returns key material would be the one change that undoes
//! everything this module is for. B4.3 (CSR generation), F1.3 (mutual TLS) and F3.2 (certificate
//! renewal) all build directly on this trait; none of them need the key itself, only what it can
//! produce.
//!
//! # Keys and certificates are addressed separately, on purpose
//!
//! Mirroring [`CertificateStore`](crate::hardware::CertificateStore)'s split between certificates
//! (public, freely listed) and the one private-key question it asks
//! ([`CertificateStore::has_client_private_key`](crate::hardware::CertificateStore::has_client_private_key)):
//! this module deals only in keys, identified by [`KeyHandle`]. A [`KeyHandle`] never contains key
//! material - only whatever the implementor uses to locate the key again (a secure-element slot
//! index, a label, a UUID) - so it is safe to persist in ordinary
//! [`Storage`](crate::hardware::Storage) alongside a certificate's hash data. That is exactly how
//! B4.3 is expected to remember which key a stored certificate pairs with when it comes time to
//! renew it: it stores the [`KeyHandle`] next to the certificate's [`CertificateHashData`](
//! crate::hardware::CertificateHashData), and hands the handle back to [`KeyStore::sign`] when it
//! needs a new CSR signed with the same key.
//!
//! # Reboot durability, and what happens when the handle outlives the key (E2.9)
//!
//! A [`KeyHandle`] is only useful if it keeps resolving to the same key after the charge point
//! restarts - a handle that only works until the next power cycle would force a fresh CSR (B4.3)
//! on every boot, defeating the point of persisting one next to a certificate at all (see the
//! section above). [`SoftKeyStore`] earns this the straightforward way: the handle and the key
//! material it names are written to [`Storage`](crate::hardware::Storage) together, so both
//! survive a restart or they both don't - see `keys_survive_a_reboot` in this module's tests. A
//! real secure element earns the same property in hardware: the handle is typically a slot index
//! or label the element itself keeps populated across power cycles, so nothing in this crate needs
//! to do anything extra for the *durable* case - it falls out of the handle being "whatever the
//! implementor uses to locate the key again" plus that locator being persisted (in `Storage`, next
//! to the certificate) rather than held only in memory.
//!
//! The case that needs an explicit answer is the other one: **a handle that still resolves to
//! *something*, but not to the key it used to** - a secure element physically replaced during
//! service, a slot index reused after the element was reset, or a `Storage` backup restored onto
//! different hardware than it was taken from. Silently succeeding here - signing with whatever key
//! now occupies that slot - would be a security defect: the resulting signature would be valid,
//! attributable to a certificate the *old* key earned, and produced by a key the certificate's
//! issuer never saw. **Every [`KeyStore`] implementation, present and future, must fail closed in
//! this situation**: [`KeyStore::sign`] returns `Err` rather than a signature it cannot vouch for.
//! [`SoftKeyStore`] already does this - `KeyNotFound` is returned whenever the handle it is asked
//! to sign with does not resolve inside its own `Storage`, whether that handle was never valid or
//! is stale after the key store's backing `Storage` was reset or replaced (see
//! `a_handle_from_a_replaced_key_store_fails_closed_rather_than_signing_with_a_different_key`
//! below). This crate cannot check this for a hardware implementor - verifying that a returned
//! signature actually came from the expected key would need the asymmetric verification math this
//! crate deliberately does not carry (see the "no crypto dependency" stance throughout
//! `crate::hardware`) - so upholding it is squarely the implementor's responsibility, the same way
//! the no-export invariant above is.
//!
//! The recovery from a failed-closed `sign` is not this trait's to prescribe (it is a protocol
//! decision, not a storage one), but it is the one that keeps the "fail-safe over fail-open" rule
//! in `CLAUDE.md`: treat the handle as dead, generate a fresh keypair with
//! [`KeyStore::generate_key_pair`], and drive a new `SignCertificate` round trip (B4.3) for a
//! replacement certificate over the new key, rather than continuing to address the old
//! certificate's handle and hoping it comes back.
//!
//! # Hardware-backed vs. software fallback - and never confusing the two
//!
//! Most charge points have no secure element. [`SoftKeyStore`] is the software fallback: it keeps
//! generated keys in [`Storage`](crate::hardware::Storage), which is honest about being weaker (a
//! key in flash is a key an attacker with the flash has) rather than pretending otherwise.
//! [`KeyStore::backing`] is how a caller (ultimately, whether security profile 3's Advanced
//! Security expectations are actually met) tells the two apart - [`SoftKeyStore::backing`]
//! unconditionally reports [`KeyStoreBacking::Software`], regardless of what its
//! [`SoftwareCrypto`] backend can do, so a software fallback can never be mistaken for a secure
//! element that happens to be implemented over `Storage`.
//!
//! # Credentials are not key material (CV10)
//!
//! [`KeyStore::store_credential`]/[`KeyStore::load_credential`]/[`KeyStore::delete_credential`]
//! are a second, deliberately narrower kind of secret this trait holds: an opaque, *retrievable*
//! value such as OCPP's `NetworkConfiguration.BasicAuthPassword` (A01.FR.02). This looks like it
//! should conflict with the no-export invariant above - it does not, because it is a different
//! promise about a different kind of secret. A [`KeyHandle`]'s private key exists to *sign*
//! things without ever leaving the element that holds it; a stored credential exists to be sent
//! to a peer verbatim (HTTP Basic authentication has no other way to work), so
//! [`Self::load_credential`] returning it in cleartext is the method doing its job, not a breach
//! of it. What the two share is the reason this lives here rather than in
//! [`Storage`](crate::hardware::Storage) directly: hardware with a secure element usually offers
//! protected storage for both a key slot *and* a plain data/OTP zone (an ATECC608's data slots are
//! the common example), so a credential gets the same "as good as this station's hardware allows"
//! treatment a signing key does, rather than landing in the same plain-flash blob
//! [`crate::persistence`] writes (which is deliberately readable with `cat` - see that module's
//! docs - and exactly wrong for a password). [`SoftKeyStore`] falls back to
//! [`Storage`](crate::hardware::Storage) for credentials exactly as it does for keys, and still
//! honestly reports [`KeyStoreBacking::Software`].

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// An asymmetric key algorithm a [`KeyStore`] can be asked to generate a keypair for, or that a
/// generated key already uses.
///
/// Deliberately small: just the algorithms a CSR (B4.3) plausibly needs to advertise, not a
/// general-purpose crypto-agility enum. Extend rather than reinterpret an existing variant if a
/// new one is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SignatureAlgorithm {
    /// ECDSA over the NIST P-256 curve, with SHA-256 digests.
    EcdsaP256Sha256,
    /// ECDSA over the NIST P-384 curve, with SHA-384 digests.
    EcdsaP384Sha384,
    /// RSA, 2048-bit modulus, with SHA-256 digests.
    Rsa2048Sha256,
    /// RSA, 3072-bit modulus, with SHA-256 digests.
    Rsa3072Sha256,
}

/// Whether a [`KeyStore`] holds its keys behind real hardware that cannot export them, or as a
/// software fallback.
///
/// This is the difference between meeting OCPP's Advanced Security expectations (security
/// profile 3 backed by a secure element) and merely appearing to (the same profile, backed by a
/// key sitting in flash) - see the module docs. A station reports this so a caller can tell which
/// promise is actually being kept, not just whether *a* [`KeyStore`] is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStoreBacking {
    /// Keys live behind real secure hardware (a secure element, TPM, or equivalent) that does not
    /// give up private key material under any operation this trait exposes.
    HardwareSecureElement,
    /// Keys are held in software - ultimately in [`Storage`](crate::hardware::Storage) or
    /// equivalent - and are only as safe as whatever protects that storage medium.
    Software,
}

/// An opaque reference to a key held by a [`KeyStore`], returned by
/// [`KeyStore::generate_key_pair`] and passed back to address that key later.
///
/// **Never contains key material.** It is whatever the implementor uses internally to find the
/// key again - a secure-element slot index, a label, a UUID - so it is exactly as safe to log,
/// persist in ordinary [`Storage`](crate::hardware::Storage), or send across an actor boundary as
/// any other identifier. See the module docs for why a stored certificate is expected to keep one
/// of these next to it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyHandle(Vec<u8>);

impl KeyHandle {
    /// Wraps `bytes` (the implementor's own identifier for the key) as a handle.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    /// The identifier this handle carries, in whatever encoding the implementor chose.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// The public half of a keypair, in whatever encoding the implementor produces.
///
/// This crate does not parse or validate the encoding - like
/// [`CertificateHashData`](crate::hardware::CertificateHashData), it is carried opaquely from the
/// implementor to whatever eventually needs it (a CSR builder, in B4.3). Common choices are
/// SubjectPublicKeyInfo DER or a raw EC point, and the implementor is expected to document which
/// one it uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey {
    /// The algorithm this key was generated for.
    pub algorithm: SignatureAlgorithm,
    /// The encoded public key material.
    pub bytes: Vec<u8>,
}

/// The result of [`KeyStore::generate_key_pair`]: the public half, plus the handle that
/// [`KeyStore::sign`] later addresses the private half with. The private half itself never
/// appears here - see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedKeyPair {
    /// Addresses the private key this crate never sees.
    pub handle: KeyHandle,
    /// The public key, safe to hand to a CSR or an `InstallCertificate`-adjacent flow.
    pub public_key: PublicKey,
}

/// Where a charge point's private keys live and are signed with.
///
/// Implemented by the integrator, so it can sit behind a secure element - see the module docs for
/// the no-export invariant every implementation (present and future) must uphold.
/// [`SoftKeyStore`] is the ready-made fallback over [`Storage`](crate::hardware::Storage) for
/// hardware without one; [`NoKeyStore`] is the honest "there is no key storage at all" default.
///
/// # Error handling
///
/// Every operation is fallible, per `CLAUDE.md`: flash fails, a secure element can be busy,
/// locked, or physically absent. A failure is surfaced as an `Err` rather than a panic - exactly
/// [`CertificateStore`](crate::hardware::CertificateStore)'s stance, for the same reason.
#[async_trait::async_trait]
pub trait KeyStore {
    /// The error type returned by a failed key-store operation.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Generates a new keypair for `algorithm` and returns its public half and a handle to the
    /// private half. The private key itself never appears in the return value, is never copied
    /// out of the element that generated it, and this trait provides no way to ask for it later.
    async fn generate_key_pair(
        &self,
        algorithm: SignatureAlgorithm,
    ) -> Result<GeneratedKeyPair, Self::Error>;

    /// Signs `digest` (a value already hashed by the caller, not raw message bytes) with the
    /// private key behind `handle`, returning the encoded signature.
    ///
    /// The implementor chooses the signature encoding (e.g. DER for ECDSA) and is expected to
    /// document it, the same way [`PublicKey::bytes`] documents its own encoding - this crate
    /// does not parse either.
    async fn sign(&self, handle: &KeyHandle, digest: &[u8]) -> Result<Vec<u8>, Self::Error>;

    /// Permanently removes the key behind `handle`. Removing a handle that no longer resolves to
    /// a key is not an error - the caller wanted it gone and it is gone.
    async fn delete_key_pair(&self, handle: &KeyHandle) -> Result<(), Self::Error>;

    /// Every algorithm [`Self::generate_key_pair`] can be asked for right now.
    ///
    /// Async and fallible like everything else here, not a plain slice: querying a real secure
    /// element for its supported algorithm set can itself be a round trip that fails.
    async fn supported_algorithms(&self) -> Result<Vec<SignatureAlgorithm>, Self::Error>;

    /// Whether this store's keys are held behind real secure hardware or a software fallback -
    /// see [`KeyStoreBacking`] and the module docs. An implementation must answer this honestly
    /// even when it would prefer to look more secure than it is.
    async fn backing(&self) -> Result<KeyStoreBacking, Self::Error>;

    /// Stores `value` under `label`, overwriting whatever credential was stored under that label
    /// before. See the module docs ("Credentials are not key material") for why this is not the
    /// asymmetric key material [`Self::generate_key_pair`]/[`Self::sign`] guard, and is expected
    /// to come back out of [`Self::load_credential`] unchanged.
    async fn store_credential(&self, label: &str, value: &str) -> Result<(), Self::Error>;

    /// Reads back the credential [`Self::store_credential`] stored under `label`, or `None` if
    /// nothing is stored there.
    async fn load_credential(&self, label: &str) -> Result<Option<String>, Self::Error>;

    /// Deletes the credential stored under `label`. Removing a label with nothing stored under it
    /// is not an error - the caller wanted it gone and it is gone.
    async fn delete_credential(&self, label: &str) -> Result<(), Self::Error>;
}

#[async_trait::async_trait]
impl<T: KeyStore + Send + Sync + ?Sized> KeyStore for alloc::sync::Arc<T> {
    type Error = T::Error;

    async fn generate_key_pair(
        &self,
        algorithm: SignatureAlgorithm,
    ) -> Result<GeneratedKeyPair, Self::Error> {
        (**self).generate_key_pair(algorithm).await
    }

    async fn sign(&self, handle: &KeyHandle, digest: &[u8]) -> Result<Vec<u8>, Self::Error> {
        (**self).sign(handle, digest).await
    }

    async fn delete_key_pair(&self, handle: &KeyHandle) -> Result<(), Self::Error> {
        (**self).delete_key_pair(handle).await
    }

    async fn supported_algorithms(&self) -> Result<Vec<SignatureAlgorithm>, Self::Error> {
        (**self).supported_algorithms().await
    }

    async fn backing(&self) -> Result<KeyStoreBacking, Self::Error> {
        (**self).backing().await
    }

    async fn store_credential(&self, label: &str, value: &str) -> Result<(), Self::Error> {
        (**self).store_credential(label, value).await
    }

    async fn load_credential(&self, label: &str) -> Result<Option<String>, Self::Error> {
        (**self).load_credential(label).await
    }

    async fn delete_credential(&self, label: &str) -> Result<(), Self::Error> {
        (**self).delete_credential(label).await
    }
}

/// A [`KeyStore`] for charge points with no key storage at all.
///
/// Every operation that would need a key fails rather than pretending to succeed:
/// [`Self::supported_algorithms`] answers empty, and [`Self::generate_key_pair`]/[`Self::sign`]/
/// [`Self::delete_key_pair`] all return [`NoKeyStoreError`]. Reporting
/// [`KeyStoreBacking::Software`] from [`Self::backing`] rather than refusing that call too keeps
/// the trait's contract simple (every implementor answers `backing`), and is not a false claim of
/// security: with no algorithms ever generatable, there is nothing for the answer to be dishonest
/// about, and `Software` is the answer that never overstates what's here to `HardwareSecureElement`.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoKeyStore;

/// The error type of [`NoKeyStore`]'s fallible operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoKeyStoreError;

impl core::fmt::Display for NoKeyStoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("this charge point has no key storage")
    }
}

impl core::error::Error for NoKeyStoreError {}

#[async_trait::async_trait]
impl KeyStore for NoKeyStore {
    type Error = NoKeyStoreError;

    async fn generate_key_pair(
        &self,
        _algorithm: SignatureAlgorithm,
    ) -> Result<GeneratedKeyPair, Self::Error> {
        Err(NoKeyStoreError)
    }

    async fn sign(&self, _handle: &KeyHandle, _digest: &[u8]) -> Result<Vec<u8>, Self::Error> {
        Err(NoKeyStoreError)
    }

    async fn delete_key_pair(&self, _handle: &KeyHandle) -> Result<(), Self::Error> {
        Err(NoKeyStoreError)
    }

    async fn supported_algorithms(&self) -> Result<Vec<SignatureAlgorithm>, Self::Error> {
        Ok(Vec::new())
    }

    async fn backing(&self) -> Result<KeyStoreBacking, Self::Error> {
        Ok(KeyStoreBacking::Software)
    }

    async fn store_credential(&self, _label: &str, _value: &str) -> Result<(), Self::Error> {
        Err(NoKeyStoreError)
    }

    async fn load_credential(&self, _label: &str) -> Result<Option<String>, Self::Error> {
        Err(NoKeyStoreError)
    }

    async fn delete_credential(&self, _label: &str) -> Result<(), Self::Error> {
        Err(NoKeyStoreError)
    }
}

/// The actual asymmetric-key math a [`SoftKeyStore`] needs and this crate does not implement
/// itself.
///
/// This crate carries no crypto dependency of its own - see
/// [`CertificateStore`](crate::hardware::CertificateStore)'s module docs for why
/// [`StoredCertificates`](crate::hardware::StoredCertificates) does no X.509 parsing either. A
/// software key-storage fallback still needs *something* to actually generate a keypair and
/// produce a signature, so [`SoftKeyStore`] takes that something as a type parameter rather than
/// reaching for a crypto crate this crate would then force on every consumer (including ones with
/// a real secure element and no use for software crypto at all).
pub trait SoftwareCrypto {
    /// The error type returned by a failed crypto operation.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Generates a new keypair for `algorithm`, returning the encoded private key (in whatever
    /// format [`Self::sign`] expects back) and the public key.
    fn generate_key_pair(
        &self,
        algorithm: SignatureAlgorithm,
    ) -> Result<(Vec<u8>, PublicKey), Self::Error>;

    /// Signs `digest` with `private_key` (as produced by [`Self::generate_key_pair`]), returning
    /// the encoded signature.
    fn sign(
        &self,
        algorithm: SignatureAlgorithm,
        private_key: &[u8],
        digest: &[u8],
    ) -> Result<Vec<u8>, Self::Error>;

    /// Every algorithm this backend can generate a keypair for.
    fn supported_algorithms(&self) -> &[SignatureAlgorithm];
}

/// How many keys [`SoftKeyStore::new`] holds.
///
/// Four: a station's own client key plus room for a rotation that overlaps, mirroring
/// [`DEFAULT_MAX_CERTIFICATES`](crate::hardware::DEFAULT_MAX_CERTIFICATES)'s reasoning at a
/// smaller number, since a station only ever needs a handful of its own keys, not a whole trust
/// chain's worth.
pub const DEFAULT_MAX_KEYS: usize = 4;

/// How many labelled credentials [`SoftKeyStore`] holds via
/// [`KeyStore::store_credential`]/[`KeyStore::load_credential`]/[`KeyStore::delete_credential`].
///
/// Matches [`DEFAULT_MAX_NETWORK_PROFILE_SLOTS`](crate::state::DEFAULT_MAX_NETWORK_PROFILE_SLOTS):
/// today's one consumer (CV10's `NetworkConfiguration.BasicAuthPassword` rotation) stores at most
/// one credential per configuration slot, and a store a remote CSMS could grow without limit is
/// not a bound (G2.2).
const DEFAULT_MAX_CREDENTIALS: usize = 4;

const KEYS_KEY: &str = "keys";
const CREDENTIALS_KEY: &str = "credentials";

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct PersistedKeys {
    entries: Vec<PersistedKey>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct PersistedCredentials {
    /// `(label, value)` pairs rather than a map: this crate's `no_std` JSON path
    /// ([`crate::hardware::key_storage`]) prefers the types every target already carries, and a
    /// handful of labelled credentials never needs map-shaped lookup performance to justify
    /// pulling one in.
    entries: Vec<(String, String)>,
}

/// Derives a [`SoftKeyStore`] handle from the key it names, rather than from a per-store counter.
///
/// A counter (`"key-0"`, `"key-1"`, ...) would hand out the same handle bytes from any two
/// [`SoftKeyStore`]s that generated their first key independently - exactly the E2.9 "element
/// replaced" scenario (see the module docs): a stale handle from a wiped-and-regenerated store
/// would then collide with, and silently resolve to, whatever unrelated key now occupies that same
/// sequence position. Hashing the algorithm and the public key instead ties the handle to the
/// specific key it was generated for, so a handle from a different key-generation event practically
/// never matches - collision would require finding two distinct keys with the same SHA-256 digest.
/// Reuses [`crate::certificates::csr::sha256`], the one keyless, dependency-free hash this crate
/// already carries and pins against test vectors, rather than adding a second implementation.
fn derive_handle(algorithm: SignatureAlgorithm, public_key: &[u8]) -> Vec<u8> {
    let mut preimage = alloc::vec![match algorithm {
        SignatureAlgorithm::EcdsaP256Sha256 => 0u8,
        SignatureAlgorithm::EcdsaP384Sha384 => 1u8,
        SignatureAlgorithm::Rsa2048Sha256 => 2u8,
        SignatureAlgorithm::Rsa3072Sha256 => 3u8,
    }];
    preimage.extend_from_slice(public_key);
    crate::certificates::csr::sha256(&preimage)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PersistedKey {
    handle: Vec<u8>,
    algorithm: PersistedAlgorithm,
    private_key: Vec<u8>,
    public_key: Vec<u8>,
}

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

/// The error type of [`SoftKeyStore`].
#[derive(Debug, PartialEq, Eq)]
pub enum SoftKeyStoreError<E> {
    /// The wrapped [`SoftwareCrypto`] backend failed to generate a keypair or produce a
    /// signature.
    Crypto(E),
    /// [`SoftKeyStore::generate_key_pair`] would exceed the store's key limit.
    StoreFull,
    /// [`SoftKeyStore::sign`] or [`SoftKeyStore::delete_key_pair`] was given a handle this store
    /// has no key for.
    KeyNotFound,
    /// The underlying [`Storage`](crate::hardware::Storage) failed to persist a newly generated
    /// key. Deliberately a hard error here (unlike [`crate::hardware::StoredCertificates`], which
    /// degrades a read failure to "empty" because certificates can simply be reinstalled): a key
    /// that was generated but not durably recorded would be unrecoverable, so the caller must be
    /// told rather than left believing a key exists that a restart will lose.
    PersistenceFailed,
    /// [`KeyStore::store_credential`] would exceed [`DEFAULT_MAX_CREDENTIALS`] on a label this
    /// store does not already hold.
    CredentialStoreFull,
}

impl<E: core::fmt::Display> core::fmt::Display for SoftKeyStoreError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Crypto(error) => write!(f, "software crypto backend failed: {error}"),
            Self::StoreFull => f.write_str("the software key store is full"),
            Self::KeyNotFound => f.write_str("no key exists for that handle"),
            Self::PersistenceFailed => f.write_str("could not persist the software key store"),
            Self::CredentialStoreFull => f.write_str("the software credential store is full"),
        }
    }
}

impl<E: core::fmt::Debug + core::fmt::Display> core::error::Error for SoftKeyStoreError<E> {}

/// A [`KeyStore`] backed by [`Storage`](crate::hardware::Storage) plus a [`SoftwareCrypto`]
/// backend - the software fallback for charge points with no secure element.
///
/// **Always reports [`KeyStoreBacking::Software`]** from [`KeyStore::backing`], never
/// [`KeyStoreBacking::HardwareSecureElement`], regardless of what the wrapped [`SoftwareCrypto`]
/// can do - see the module docs. Private keys are stored in `Storage` in whatever encoding the
/// crypto backend produces; that storage medium's own confidentiality (encrypted flash, a locked
/// enclosure, ...) is the only protection they get, which is exactly the gap a real secure
/// element closes and this type does not pretend to.
///
/// Bounded by `max_keys`, for the reason every other collection here is (G2.2): a store a remote
/// peer can grow without limit is not a bound.
pub struct SoftKeyStore<S, C> {
    storage: S,
    crypto: C,
    max_keys: usize,
}

impl<S: crate::hardware::Storage, C: SoftwareCrypto> SoftKeyStore<S, C> {
    /// A store over `storage` and `crypto`, holding at most [`DEFAULT_MAX_KEYS`].
    pub fn new(storage: S, crypto: C) -> Self {
        Self::with_limit(storage, crypto, DEFAULT_MAX_KEYS)
    }

    /// A store over `storage` and `crypto`, holding at most `max_keys` (clamped to at least one).
    pub fn with_limit(storage: S, crypto: C, max_keys: usize) -> Self {
        Self {
            storage,
            crypto,
            max_keys: max_keys.max(1),
        }
    }

    /// Reads the index, treating an unreadable or corrupt one as empty - the same "come up empty
    /// rather than refuse to come up" rule [`crate::hardware::StoredCertificates`] follows for
    /// certificates. Unlike a lost certificate, a key lost this way cannot be reinstalled by the
    /// CSMS - but a charge point that cannot read its own key store is no better off refusing to
    /// boot, and the caller finds out anyway the moment [`KeyStore::sign`] reports
    /// [`SoftKeyStoreError::KeyNotFound`].
    async fn load(&self) -> PersistedKeys {
        match self.storage.get(KEYS_KEY).await {
            Ok(Some(encoded)) => serde_json::from_slice(&encoded).unwrap_or_else(|error| {
                tracing::warn!(%error, "discarding a corrupt software key store");
                PersistedKeys::default()
            }),
            Ok(None) => PersistedKeys::default(),
            Err(error) => {
                tracing::warn!(%error, "could not read the software key store");
                PersistedKeys::default()
            }
        }
    }

    async fn save(&self, keys: &PersistedKeys) -> bool {
        let Ok(encoded) = serde_json::to_vec(keys) else {
            return false;
        };
        match self.storage.set(KEYS_KEY, &encoded).await {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(%error, "could not write the software key store");
                false
            }
        }
    }

    /// Reads the credential index, treating an unreadable or corrupt one as empty - the same
    /// "come up empty rather than refuse to come up" rule [`Self::load`] follows for keys. Unlike
    /// a lost key, a lost credential is exactly what CV10's rollback exists for: a station that
    /// cannot read its own rotated password falls back to behaving as though none was ever
    /// stored, which is safe (it simply keeps whatever credential the transport was already
    /// configured with) rather than a boot failure.
    async fn load_credentials(&self) -> PersistedCredentials {
        match self.storage.get(CREDENTIALS_KEY).await {
            Ok(Some(encoded)) => serde_json::from_slice(&encoded).unwrap_or_else(|error| {
                tracing::warn!(%error, "discarding a corrupt software credential store");
                PersistedCredentials::default()
            }),
            Ok(None) => PersistedCredentials::default(),
            Err(error) => {
                tracing::warn!(%error, "could not read the software credential store");
                PersistedCredentials::default()
            }
        }
    }

    async fn save_credentials(&self, credentials: &PersistedCredentials) -> bool {
        let Ok(encoded) = serde_json::to_vec(credentials) else {
            return false;
        };
        match self.storage.set(CREDENTIALS_KEY, &encoded).await {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(%error, "could not write the software credential store");
                false
            }
        }
    }
}

#[async_trait::async_trait]
impl<S, C> KeyStore for SoftKeyStore<S, C>
where
    S: crate::hardware::Storage + Send + Sync,
    C: SoftwareCrypto + Send + Sync,
{
    type Error = SoftKeyStoreError<C::Error>;

    async fn generate_key_pair(
        &self,
        algorithm: SignatureAlgorithm,
    ) -> Result<GeneratedKeyPair, Self::Error> {
        let (private_key, public_key) = self
            .crypto
            .generate_key_pair(algorithm)
            .map_err(SoftKeyStoreError::Crypto)?;

        let mut keys = self.load().await;
        if keys.entries.len() >= self.max_keys {
            return Err(SoftKeyStoreError::StoreFull);
        }

        let handle = KeyHandle::new(derive_handle(algorithm, &public_key.bytes));
        keys.entries.push(PersistedKey {
            handle: handle.as_bytes().to_vec(),
            algorithm: algorithm.into(),
            private_key,
            public_key: public_key.bytes.clone(),
        });

        if self.save(&keys).await {
            Ok(GeneratedKeyPair { handle, public_key })
        } else {
            Err(SoftKeyStoreError::PersistenceFailed)
        }
    }

    async fn sign(&self, handle: &KeyHandle, digest: &[u8]) -> Result<Vec<u8>, Self::Error> {
        let keys = self.load().await;
        let entry = keys
            .entries
            .iter()
            .find(|entry| entry.handle == handle.as_bytes())
            .ok_or(SoftKeyStoreError::KeyNotFound)?;
        self.crypto
            .sign(entry.algorithm.into(), &entry.private_key, digest)
            .map_err(SoftKeyStoreError::Crypto)
    }

    async fn delete_key_pair(&self, handle: &KeyHandle) -> Result<(), Self::Error> {
        let mut keys = self.load().await;
        let before = keys.entries.len();
        keys.entries
            .retain(|entry| entry.handle != handle.as_bytes());
        if keys.entries.len() == before {
            return Ok(());
        }
        if self.save(&keys).await {
            Ok(())
        } else {
            Err(SoftKeyStoreError::PersistenceFailed)
        }
    }

    async fn supported_algorithms(&self) -> Result<Vec<SignatureAlgorithm>, Self::Error> {
        Ok(self.crypto.supported_algorithms().to_vec())
    }

    async fn backing(&self) -> Result<KeyStoreBacking, Self::Error> {
        // Hardcoded, not derived from `crypto` in any way: a `SoftKeyStore` is software by
        // construction (its keys live in `Storage`), no matter how good the math behind it is.
        Ok(KeyStoreBacking::Software)
    }

    async fn store_credential(&self, label: &str, value: &str) -> Result<(), Self::Error> {
        let mut credentials = self.load_credentials().await;
        let already_present = credentials
            .entries
            .iter()
            .any(|(existing, _)| existing == label);
        if !already_present && credentials.entries.len() >= DEFAULT_MAX_CREDENTIALS {
            return Err(SoftKeyStoreError::CredentialStoreFull);
        }
        credentials
            .entries
            .retain(|(existing, _)| existing != label);
        credentials.entries.push((label.into(), value.into()));
        if self.save_credentials(&credentials).await {
            Ok(())
        } else {
            Err(SoftKeyStoreError::PersistenceFailed)
        }
    }

    async fn load_credential(&self, label: &str) -> Result<Option<String>, Self::Error> {
        let credentials = self.load_credentials().await;
        Ok(credentials
            .entries
            .into_iter()
            .find(|(existing, _)| existing == label)
            .map(|(_, value)| value))
    }

    async fn delete_credential(&self, label: &str) -> Result<(), Self::Error> {
        let mut credentials = self.load_credentials().await;
        let before = credentials.entries.len();
        credentials
            .entries
            .retain(|(existing, _)| existing != label);
        if credentials.entries.len() == before {
            return Ok(());
        }
        if self.save_credentials(&credentials).await {
            Ok(())
        } else {
            Err(SoftKeyStoreError::PersistenceFailed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::InMemoryStorage;
    use alloc::sync::Arc;

    #[tokio::test]
    async fn a_charge_point_with_no_key_store_refuses_rather_than_pretending_to_hold_a_key() {
        let store = NoKeyStore;

        assert_eq!(
            store
                .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
                .await,
            Err(NoKeyStoreError)
        );
        assert_eq!(
            store.sign(&KeyHandle::new(*b"anything"), b"digest").await,
            Err(NoKeyStoreError)
        );
        assert_eq!(
            store.delete_key_pair(&KeyHandle::new(*b"anything")).await,
            Err(NoKeyStoreError)
        );
        assert_eq!(store.supported_algorithms().await, Ok(Vec::new()));
        // Reports `Software`, never overclaiming hardware it doesn't have.
        assert_eq!(store.backing().await, Ok(KeyStoreBacking::Software));
        // A credential is a secret too (CV10) - a charge point with no key storage at all cannot
        // hold one durably, so it must refuse rather than silently drop it.
        assert_eq!(
            store.store_credential("label", "value").await,
            Err(NoKeyStoreError)
        );
        assert_eq!(store.load_credential("label").await, Err(NoKeyStoreError));
        assert_eq!(store.delete_credential("label").await, Err(NoKeyStoreError));
    }

    /// A fake [`SoftwareCrypto`] backend: "signing" is deterministic and reversible so tests can
    /// check a signature was actually produced with the right key, without any real asymmetric
    /// math.
    #[derive(Debug, Default)]
    struct FakeCrypto {
        next_key_byte: core::sync::atomic::AtomicU8,
    }

    impl SoftwareCrypto for FakeCrypto {
        type Error = core::convert::Infallible;

        fn generate_key_pair(
            &self,
            algorithm: SignatureAlgorithm,
        ) -> Result<(Vec<u8>, PublicKey), Self::Error> {
            let byte = self
                .next_key_byte
                .fetch_add(1, core::sync::atomic::Ordering::SeqCst);
            let private_key = alloc::vec![byte];
            let public_key = PublicKey {
                algorithm,
                bytes: alloc::vec![byte, byte],
            };
            Ok((private_key, public_key))
        }

        fn sign(
            &self,
            _algorithm: SignatureAlgorithm,
            private_key: &[u8],
            digest: &[u8],
        ) -> Result<Vec<u8>, Self::Error> {
            let mut signature = private_key.to_vec();
            signature.extend_from_slice(digest);
            Ok(signature)
        }

        fn supported_algorithms(&self) -> &[SignatureAlgorithm] {
            &[
                SignatureAlgorithm::EcdsaP256Sha256,
                SignatureAlgorithm::EcdsaP384Sha384,
            ]
        }
    }

    fn store() -> SoftKeyStore<Arc<InMemoryStorage>, FakeCrypto> {
        SoftKeyStore::new(Arc::new(InMemoryStorage::new()), FakeCrypto::default())
    }

    #[tokio::test]
    async fn a_generated_key_never_appears_in_its_own_return_value() {
        let store = store();
        let generated = store
            .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
            .await
            .unwrap();

        // The only things returned are the handle and the public key - `GeneratedKeyPair` has no
        // field a private key could even be smuggled through.
        assert_eq!(
            generated.public_key.algorithm,
            SignatureAlgorithm::EcdsaP256Sha256
        );
        assert!(!generated.handle.as_bytes().is_empty());
    }

    #[tokio::test]
    async fn signing_with_a_generated_handle_round_trips_through_the_stored_key() {
        let store = store();
        let generated = store
            .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
            .await
            .unwrap();

        let signature = store.sign(&generated.handle, b"a digest").await.unwrap();
        // `FakeCrypto::sign` appends the digest to the private key bytes - proof the right stored
        // key was loaded and handed to the crypto backend, without this test ever seeing the key.
        assert!(signature.ends_with(b"a digest"));
    }

    #[tokio::test]
    async fn signing_with_an_unknown_handle_fails_rather_than_silently_using_another_key() {
        let store = store();
        let result = store.sign(&KeyHandle::new(*b"nope"), b"digest").await;
        assert!(matches!(result, Err(SoftKeyStoreError::KeyNotFound)));
    }

    #[tokio::test]
    async fn deleting_a_key_makes_it_unusable_for_signing() {
        let store = store();
        let generated = store
            .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
            .await
            .unwrap();

        store.delete_key_pair(&generated.handle).await.unwrap();

        let result = store.sign(&generated.handle, b"digest").await;
        assert!(matches!(result, Err(SoftKeyStoreError::KeyNotFound)));
    }

    #[tokio::test]
    async fn deleting_a_handle_that_is_not_there_is_not_an_error() {
        let store = store();
        store
            .delete_key_pair(&KeyHandle::new(*b"never-existed"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_store_is_bounded() {
        let store =
            SoftKeyStore::with_limit(Arc::new(InMemoryStorage::new()), FakeCrypto::default(), 1);
        store
            .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
            .await
            .unwrap();

        // G2.2: a store a remote peer can grow without limit is not a bound.
        let result = store
            .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
            .await;
        assert!(matches!(result, Err(SoftKeyStoreError::StoreFull)));
    }

    #[tokio::test]
    async fn a_software_store_never_claims_to_be_hardware_backed() {
        let store = store();
        assert_eq!(store.backing().await, Ok(KeyStoreBacking::Software));
    }

    #[tokio::test]
    async fn supported_algorithms_reflects_the_crypto_backend() {
        let store = store();
        let algorithms = store.supported_algorithms().await.unwrap();
        assert_eq!(
            algorithms,
            alloc::vec![
                SignatureAlgorithm::EcdsaP256Sha256,
                SignatureAlgorithm::EcdsaP384Sha384
            ]
        );
    }

    #[tokio::test]
    async fn keys_survive_a_reboot() {
        let storage = Arc::new(InMemoryStorage::new());
        let before = SoftKeyStore::new(storage.clone(), FakeCrypto::default());
        let generated = before
            .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
            .await
            .unwrap();

        // --- the cut: only `storage` survives, a fresh `FakeCrypto` stands in for reconnecting
        // to the same secure hardware.
        let after = SoftKeyStore::new(storage, FakeCrypto::default());

        let signature = after.sign(&generated.handle, b"digest").await.unwrap();
        assert!(signature.ends_with(b"digest"));
    }

    #[tokio::test]
    async fn a_handle_from_a_replaced_key_store_fails_closed_rather_than_signing_with_a_different_key()
     {
        // Models E2.9's "the handle persists but the key is gone" case: a secure element physically
        // replaced, or a `Storage` backup restored onto different hardware than it was taken from.
        let original_storage = Arc::new(InMemoryStorage::new());
        let original = SoftKeyStore::new(original_storage, FakeCrypto::default());
        let stale_handle = original
            .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
            .await
            .unwrap()
            .handle;

        // --- the cut: a fresh `Storage` (a replaced element / restored-elsewhere backup) that
        // happens to hold a *different* key, generated independently. A distinct starting byte
        // (rather than another `FakeCrypto::default()`) stands in for two independent key
        // generations actually producing different key material, the way any two real keypairs
        // would.
        let replacement_storage = Arc::new(InMemoryStorage::new());
        let replacement = SoftKeyStore::new(
            replacement_storage,
            FakeCrypto {
                next_key_byte: core::sync::atomic::AtomicU8::new(200),
            },
        );
        replacement
            .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
            .await
            .unwrap();

        // The old handle must not resolve to whatever key now lives in the replacement store - it
        // must fail closed, never silently sign with the wrong key.
        let result = replacement.sign(&stale_handle, b"digest").await;
        assert!(matches!(result, Err(SoftKeyStoreError::KeyNotFound)));
    }

    #[tokio::test]
    async fn a_corrupt_store_comes_up_empty_rather_than_refusing_to_come_up() {
        let storage = Arc::new(InMemoryStorage::new());
        crate::hardware::Storage::set(&*storage, "keys", b"{not json")
            .await
            .unwrap();

        let store = SoftKeyStore::new(storage, FakeCrypto::default());
        let result = store.sign(&KeyHandle::new(*b"anything"), b"digest").await;
        assert!(matches!(result, Err(SoftKeyStoreError::KeyNotFound)));
    }

    /// CV10: a stored credential must come back out unchanged - unlike a key behind a
    /// `KeyHandle`, this is the whole point of `store_credential`/`load_credential` (see the
    /// module docs, "Credentials are not key material").
    #[tokio::test]
    async fn a_stored_credential_round_trips() {
        let store = store();
        assert_eq!(store.load_credential("basic-auth/1").await, Ok(None));

        store
            .store_credential("basic-auth/1", "s3cret-but-long-enough")
            .await
            .unwrap();

        assert_eq!(
            store.load_credential("basic-auth/1").await,
            Ok(Some("s3cret-but-long-enough".into()))
        );
    }

    #[tokio::test]
    async fn storing_under_the_same_label_again_overwrites_rather_than_duplicating() {
        let store = store();
        store
            .store_credential("basic-auth/1", "first")
            .await
            .unwrap();
        store
            .store_credential("basic-auth/1", "second")
            .await
            .unwrap();

        assert_eq!(
            store.load_credential("basic-auth/1").await,
            Ok(Some("second".into()))
        );
    }

    #[tokio::test]
    async fn deleting_a_credential_makes_it_unreadable() {
        let store = store();
        store
            .store_credential("basic-auth/1", "value")
            .await
            .unwrap();

        store.delete_credential("basic-auth/1").await.unwrap();

        assert_eq!(store.load_credential("basic-auth/1").await, Ok(None));
    }

    #[tokio::test]
    async fn deleting_a_label_that_is_not_there_is_not_an_error() {
        let store = store();
        store.delete_credential("never-stored").await.unwrap();
    }

    #[tokio::test]
    async fn the_credential_store_is_bounded_but_overwriting_an_existing_label_is_not_blocked() {
        let store = store();
        for slot in 0..DEFAULT_MAX_CREDENTIALS {
            store
                .store_credential(&alloc::format!("slot-{slot}"), "value")
                .await
                .unwrap();
        }

        // G2.2: a store a remote CSMS can grow without limit is not a bound.
        let result = store.store_credential("one-too-many", "value").await;
        assert!(matches!(
            result,
            Err(SoftKeyStoreError::CredentialStoreFull)
        ));

        // Rotating an already-occupied label must still work at the limit - CV10's rollback
        // depends on being able to keep rewriting the one label a slot's password lives under.
        store.store_credential("slot-0", "rotated").await.unwrap();
        assert_eq!(
            store.load_credential("slot-0").await,
            Ok(Some("rotated".into()))
        );
    }

    #[tokio::test]
    async fn credentials_survive_a_reboot() {
        let storage = Arc::new(InMemoryStorage::new());
        let before = SoftKeyStore::new(storage.clone(), FakeCrypto::default());
        before
            .store_credential("basic-auth/1", "value")
            .await
            .unwrap();

        // --- the cut: only `storage` survives.
        let after = SoftKeyStore::new(storage, FakeCrypto::default());

        assert_eq!(
            after.load_credential("basic-auth/1").await,
            Ok(Some("value".into()))
        );
    }

    #[tokio::test]
    async fn a_corrupt_credential_store_comes_up_empty_rather_than_refusing_to_come_up() {
        let storage = Arc::new(InMemoryStorage::new());
        crate::hardware::Storage::set(&*storage, "credentials", b"{not json")
            .await
            .unwrap();

        let store = SoftKeyStore::new(storage, FakeCrypto::default());
        assert_eq!(store.load_credential("anything").await, Ok(None));
    }

    /// The keys index and the credentials index are stored under different `Storage` keys, so
    /// generating a key does not disturb a stored credential and vice versa.
    #[tokio::test]
    async fn keys_and_credentials_do_not_collide() {
        let store = store();
        store
            .store_credential("basic-auth/1", "value")
            .await
            .unwrap();
        let generated = store
            .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
            .await
            .unwrap();

        assert_eq!(
            store.load_credential("basic-auth/1").await,
            Ok(Some("value".into()))
        );
        assert!(store.sign(&generated.handle, b"digest").await.is_ok());
    }
}
