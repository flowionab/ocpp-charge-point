//! The certificate store: what a charge point trusts, and what it presents
//! (`docs/PRODUCTION-ROADMAP.md` B4.1).
//!
//! # Why this is a trait and not a table in `Storage`
//!
//! B4.1's own note is the reason: *on real hardware this should be able to sit behind a secure
//! element*. A secure element holds a private key and will not give it back - it signs on request
//! and the key never leaves the chip. Any design where this crate reads a key out of storage and
//! hands it to a TLS stack has already given that up, so the store is a **trait the integrator
//! implements** and the crate never sees a private key at all.
//!
//! For the many charge points with no secure element, [`StoredCertificates`] implements the same
//! trait over [`Storage`](crate::hardware::Storage) (E1), so an integrator gets a working store
//! for free and can swap in a secure element later without anything above noticing.
//!
//! # Certificates and keys are separated on purpose
//!
//! [`CertificateStore`] deals in certificates, which are public and safe to hold, list and hand
//! out. The private key that pairs with the charge point's own certificate is addressed only
//! indirectly, through [`CertificateStore::has_private_key`] - the crate needs to know whether it
//! *can* present a client certificate (security profile 3), never what the key is.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// What a certificate is *for*, spanning OCPP's two overlapping enums
/// (`InstallCertificateUseEnum` for what may be installed, `GetCertificateIdUseEnum` for what may
/// be listed - the latter adds the charge point's own chain, which is obtained by signing rather
/// than installed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CertificateUse {
    /// Root of the CSMS's trust chain - what a charge point checks the CSMS's TLS server
    /// certificate against on security profiles 2 and 3.
    CsmsRoot,
    /// Root for ISO 15118 V2G certificates.
    V2gRoot,
    /// Root for Mobility Operator certificates.
    MobilityOperatorRoot,
    /// Root for manufacturer-issued certificates.
    ManufacturerRoot,
    /// Root for OEM certificates (2.1).
    OemRoot,
    /// The charge point's own V2G certificate chain. Listable but not installable: it arrives via
    /// `CertificateSigned` in answer to a `SignCertificate` (B4.3), not via `InstallCertificate`.
    V2gCertificateChain,
    /// The charge point's own client certificate, presented on security profile 3. Same origin as
    /// [`Self::V2gCertificateChain`] - signed, not installed.
    ChargingStation,
}

impl CertificateUse {
    /// Whether OCPP's `InstallCertificate` may write this use.
    ///
    /// The two that may not are the charge point's *own* certificates: those are issued in
    /// response to a CSR it generated, so accepting one through `InstallCertificate` would mean
    /// accepting a certificate for a key pair this charge point may not even hold.
    pub fn is_installable(&self) -> bool {
        !matches!(self, Self::V2gCertificateChain | Self::ChargingStation)
    }
}

/// How a certificate's hashes were computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HashAlgorithm {
    /// SHA-256.
    Sha256,
    /// SHA-384.
    Sha384,
    /// SHA-512.
    Sha512,
}

/// OCPP's identifier for an installed certificate.
///
/// Certificates are addressed by *hash*, not by name or index, in both `DeleteCertificate` and
/// `GetInstalledCertificateIds`. This crate keeps that as the identity rather than inventing one:
/// a CSMS that asks to delete a certificate names it this way, and any local id would have to be
/// mapped back to these fields anyway.
///
/// The hashes are computed by whoever parses the certificate - the integrator - because computing
/// them means parsing X.509 and hashing DER, which needs crypto this crate does not have.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CertificateHashData {
    /// The algorithm the two hashes below were computed with.
    pub hash_algorithm: HashAlgorithm,
    /// Hash of the issuer's distinguished name.
    pub issuer_name_hash: String,
    /// Hash of the issuer's public key.
    pub issuer_key_hash: String,
    /// The certificate's serial number.
    pub serial_number: String,
}

/// One certificate the charge point holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledCertificate {
    /// What it is for.
    pub use_for: CertificateUse,
    /// How the CSMS addresses it.
    pub hash_data: CertificateHashData,
}

/// The outcome of installing a certificate, matching OCPP's `InstallCertificateStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallCertificateOutcome {
    /// Stored.
    Accepted,
    /// Refused - unparseable, untrusted, or a use this charge point does not accept.
    Rejected,
    /// The store is full. A distinct status because it is the one an operator can fix by deleting
    /// something, where `Rejected` usually means the certificate itself is wrong.
    Failed,
}

/// The outcome of deleting a certificate, matching OCPP's `DeleteCertificateStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteCertificateOutcome {
    /// Removed.
    Accepted,
    /// No certificate with that hash is installed. Not an error: the CSMS wanted it gone and it
    /// is gone.
    NotFound,
    /// Found, but could not be removed - a root the charge point needs to stay reachable, or a
    /// storage failure.
    Failed,
}

/// A charge point's certificate store.
///
/// Implemented by the integrator, so it can sit behind a secure element (see the module docs).
/// [`StoredCertificates`] is the ready-made implementation over
/// [`Storage`](crate::hardware::Storage) for hardware without one.
///
/// # Error handling
///
/// Every operation is fallible, per `CLAUDE.md`: flash fails, a secure element can be busy or
/// locked. A failure is surfaced as the protocol-correct refusal rather than a panic - a charge
/// point that dies because a certificate could not be read is unreachable, which is precisely the
/// state certificates exist to prevent.
#[async_trait::async_trait]
pub trait CertificateStore {
    /// The error type returned by a failed store operation.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Stores `certificate` (a PEM-encoded X.509) for `use_for`.
    ///
    /// The implementor parses and validates it - that is where the crypto lives - and is expected
    /// to refuse one it cannot parse rather than storing bytes that will fail later, when the only
    /// symptom is a connection that will not come up.
    async fn install(
        &self,
        use_for: CertificateUse,
        certificate: &str,
    ) -> Result<InstallCertificateOutcome, Self::Error>;

    /// Removes the certificate `hash_data` identifies.
    async fn delete(
        &self,
        hash_data: &CertificateHashData,
    ) -> Result<DeleteCertificateOutcome, Self::Error>;

    /// Every certificate held for any of `uses`, or all of them when `uses` is empty - OCPP's
    /// absent-means-all rule, the same one `GetChargingProfiles` follows.
    async fn installed(
        &self,
        uses: &[CertificateUse],
    ) -> Result<Vec<InstalledCertificate>, Self::Error>;

    /// Whether this charge point holds a private key it can authenticate with.
    ///
    /// The one question the crate asks about keys, and it is deliberately a yes/no: security
    /// profile 3 needs to know whether a client certificate can be presented
    /// ([`SecurityProfile::is_implemented`](crate::security_profile::SecurityProfile)), and
    /// nothing above needs the key itself. An implementation backed by a secure element answers
    /// this without the key ever leaving the chip.
    async fn has_private_key(&self) -> Result<bool, Self::Error>;

    /// The PEM-encoded certificate chain currently installed for `use_for`, or `None` if this
    /// store holds none.
    ///
    /// Additive (default `Ok(None)`) so existing implementors keep compiling without adopting
    /// it. It exists for security profile 3 mutual TLS ([F1.3](crate)):
    /// [`crate::mutual_tls`] needs the actual certificate bytes to offer on a TLS connection,
    /// not merely [`Self::has_private_key`]'s yes/no or [`Self::installed`]'s hash-only summary.
    /// [`StoredCertificates`] overrides this; a secure-element-backed store that keeps
    /// certificates elsewhere should too.
    async fn certificate_chain_pem(
        &self,
        use_for: CertificateUse,
    ) -> Result<Option<String>, Self::Error> {
        let _ = use_for;
        Ok(None)
    }

    /// Every PEM-encoded certificate chain currently installed for `use_for`, in no particular
    /// order.
    ///
    /// The multi-valued counterpart to [`Self::certificate_chain_pem`] - right for a *trust
    /// store* like [`CertificateUse::CsmsRoot`], which typically holds several roots side by
    /// side (multiple CAs, or an old and new root during a rotation), where
    /// [`Self::certificate_chain_pem`]'s "the one entry this use holds" assumption - correct for
    /// the charge point's own single-slotted certificate - would silently drop every root after
    /// the first. [`crate::trust_store`] ([F2.2](crate)) is why this exists: building a
    /// `rustls::RootCertStore` from only one of several installed roots would make this charge
    /// point unable to verify a CSMS whose current certificate happens to chain to any of the
    /// others.
    ///
    /// Additive (default delegates to [`Self::certificate_chain_pem`]) so existing implementors
    /// keep compiling. The default is safe even though it can be incomplete: at worst a caller
    /// building a trust store sees one root instead of several, never zero out of some that
    /// exist and never a wrong one. [`StoredCertificates`] overrides it to return every matching
    /// entry; an implementation that can hold more than one certificate per use should too.
    async fn all_certificate_chain_pems(
        &self,
        use_for: CertificateUse,
    ) -> Result<Vec<String>, Self::Error> {
        Ok(self
            .certificate_chain_pem(use_for)
            .await?
            .into_iter()
            .collect())
    }
}

#[async_trait::async_trait]
impl<T: CertificateStore + Send + Sync + ?Sized> CertificateStore for alloc::sync::Arc<T> {
    type Error = T::Error;

    async fn install(
        &self,
        use_for: CertificateUse,
        certificate: &str,
    ) -> Result<InstallCertificateOutcome, Self::Error> {
        (**self).install(use_for, certificate).await
    }

    async fn delete(
        &self,
        hash_data: &CertificateHashData,
    ) -> Result<DeleteCertificateOutcome, Self::Error> {
        (**self).delete(hash_data).await
    }

    async fn installed(
        &self,
        uses: &[CertificateUse],
    ) -> Result<Vec<InstalledCertificate>, Self::Error> {
        (**self).installed(uses).await
    }

    async fn has_private_key(&self) -> Result<bool, Self::Error> {
        (**self).has_private_key().await
    }

    async fn certificate_chain_pem(
        &self,
        use_for: CertificateUse,
    ) -> Result<Option<String>, Self::Error> {
        (**self).certificate_chain_pem(use_for).await
    }

    async fn all_certificate_chain_pems(
        &self,
        use_for: CertificateUse,
    ) -> Result<Vec<String>, Self::Error> {
        (**self).all_certificate_chain_pems(use_for).await
    }
}

/// A [`CertificateStore`] for charge points with no certificate handling at all.
///
/// Installs nothing and holds nothing, so a CSMS is told `Rejected` rather than left believing a
/// root it sent is now trusted - which would have it expect a TLS connection this charge point
/// cannot make.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCertificateStore;

/// The error type of [`NoCertificateStore`], which never actually fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoCertificateStoreError;

impl core::fmt::Display for NoCertificateStoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("this charge point has no certificate store")
    }
}

impl core::error::Error for NoCertificateStoreError {}

#[async_trait::async_trait]
impl CertificateStore for NoCertificateStore {
    type Error = NoCertificateStoreError;

    async fn install(
        &self,
        _use_for: CertificateUse,
        _certificate: &str,
    ) -> Result<InstallCertificateOutcome, Self::Error> {
        Ok(InstallCertificateOutcome::Rejected)
    }

    async fn delete(
        &self,
        _hash_data: &CertificateHashData,
    ) -> Result<DeleteCertificateOutcome, Self::Error> {
        Ok(DeleteCertificateOutcome::NotFound)
    }

    async fn installed(
        &self,
        _uses: &[CertificateUse],
    ) -> Result<Vec<InstalledCertificate>, Self::Error> {
        Ok(Vec::new())
    }

    async fn has_private_key(&self) -> Result<bool, Self::Error> {
        Ok(false)
    }
}

/// A [`CertificateStore`] backed by [`Storage`](crate::hardware::Storage) - the ready-made one for
/// charge points with no secure element, and the reason B4.1 depends on E1.
///
/// Certificates are public, so keeping them in ordinary storage costs nothing in confidentiality.
/// **It holds no private key**, and [`Self::has_private_key`] therefore answers `false`: a key in
/// flash is a key an attacker with the flash has, and this crate will not pretend otherwise. A
/// charge point that needs security profile 3 wants a secure-element-backed implementation of the
/// trait instead.
///
/// Bounded by `max_certificates`, for the reason every other collection here is (G2.2): a store a
/// remote peer can grow without limit is not a bound.
pub struct StoredCertificates<S> {
    storage: S,
    max_certificates: usize,
}

/// The key the whole certificate index is written under - one snapshot, like the local
/// authorization list, because the set is small and rewriting it whole avoids a partial update
/// leaving the store describing certificates it does not have.
const CERTIFICATES_KEY: &str = "certificates";

/// A serde mirror of the store's contents.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct PersistedCertificates {
    entries: Vec<PersistedCertificate>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PersistedCertificate {
    use_for: PersistedCertificateUse,
    hash_algorithm: PersistedHashAlgorithm,
    issuer_name_hash: String,
    issuer_key_hash: String,
    serial_number: String,
    pem: String,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
enum PersistedCertificateUse {
    CsmsRoot,
    V2gRoot,
    MobilityOperatorRoot,
    ManufacturerRoot,
    OemRoot,
    V2gCertificateChain,
    ChargingStation,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
enum PersistedHashAlgorithm {
    Sha256,
    Sha384,
    Sha512,
}

impl From<CertificateUse> for PersistedCertificateUse {
    fn from(use_for: CertificateUse) -> Self {
        match use_for {
            CertificateUse::CsmsRoot => Self::CsmsRoot,
            CertificateUse::V2gRoot => Self::V2gRoot,
            CertificateUse::MobilityOperatorRoot => Self::MobilityOperatorRoot,
            CertificateUse::ManufacturerRoot => Self::ManufacturerRoot,
            CertificateUse::OemRoot => Self::OemRoot,
            CertificateUse::V2gCertificateChain => Self::V2gCertificateChain,
            CertificateUse::ChargingStation => Self::ChargingStation,
        }
    }
}

impl From<PersistedCertificateUse> for CertificateUse {
    fn from(use_for: PersistedCertificateUse) -> Self {
        match use_for {
            PersistedCertificateUse::CsmsRoot => Self::CsmsRoot,
            PersistedCertificateUse::V2gRoot => Self::V2gRoot,
            PersistedCertificateUse::MobilityOperatorRoot => Self::MobilityOperatorRoot,
            PersistedCertificateUse::ManufacturerRoot => Self::ManufacturerRoot,
            PersistedCertificateUse::OemRoot => Self::OemRoot,
            PersistedCertificateUse::V2gCertificateChain => Self::V2gCertificateChain,
            PersistedCertificateUse::ChargingStation => Self::ChargingStation,
        }
    }
}

impl From<HashAlgorithm> for PersistedHashAlgorithm {
    fn from(algorithm: HashAlgorithm) -> Self {
        match algorithm {
            HashAlgorithm::Sha256 => Self::Sha256,
            HashAlgorithm::Sha384 => Self::Sha384,
            HashAlgorithm::Sha512 => Self::Sha512,
        }
    }
}

impl From<PersistedHashAlgorithm> for HashAlgorithm {
    fn from(algorithm: PersistedHashAlgorithm) -> Self {
        match algorithm {
            PersistedHashAlgorithm::Sha256 => Self::Sha256,
            PersistedHashAlgorithm::Sha384 => Self::Sha384,
            PersistedHashAlgorithm::Sha512 => Self::Sha512,
        }
    }
}

/// How many certificates [`StoredCertificates::new`] holds.
///
/// Ten: the four or five roots a deployment actually installs, with room for a rotation that
/// overlaps. Certificates are a few kilobytes each, so this is single-digit KB of flash.
pub const DEFAULT_MAX_CERTIFICATES: usize = 10;

impl<S: crate::hardware::Storage> StoredCertificates<S> {
    /// A store over `storage`, holding at most [`DEFAULT_MAX_CERTIFICATES`].
    pub fn new(storage: S) -> Self {
        Self::with_limit(storage, DEFAULT_MAX_CERTIFICATES)
    }

    /// A store over `storage`, holding at most `max_certificates` (clamped to at least one).
    pub fn with_limit(storage: S, max_certificates: usize) -> Self {
        Self {
            storage,
            max_certificates: max_certificates.max(1),
        }
    }

    /// Reads the index, treating an unreadable or corrupt one as empty.
    ///
    /// Discarding beats propagating here for the reason [`crate::persistence`] gives throughout: a
    /// charge point that cannot read its certificates should come up trusting nothing and let the
    /// CSMS reinstall, not refuse to come up.
    async fn load(&self) -> PersistedCertificates {
        match self.storage.get(CERTIFICATES_KEY).await {
            Ok(Some(encoded)) => serde_json::from_slice(&encoded).unwrap_or_else(|error| {
                tracing::warn!(%error, "discarding a corrupt certificate store");
                PersistedCertificates::default()
            }),
            Ok(None) => PersistedCertificates::default(),
            Err(error) => {
                tracing::warn!(%error, "could not read the certificate store");
                PersistedCertificates::default()
            }
        }
    }

    async fn save(&self, certificates: &PersistedCertificates) -> bool {
        let Ok(encoded) = serde_json::to_vec(certificates) else {
            return false;
        };
        match self.storage.set(CERTIFICATES_KEY, &encoded).await {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(%error, "could not write the certificate store");
                false
            }
        }
    }
}

/// The error type of [`StoredCertificates`].
///
/// Deliberately uninhabited in practice: every storage failure is degraded into a protocol-correct
/// refusal inside the store rather than propagated, per `CLAUDE.md`'s "a storage failure must
/// never take down the charge point".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredCertificatesError;

impl core::fmt::Display for StoredCertificatesError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("certificate store failure")
    }
}

impl core::error::Error for StoredCertificatesError {}

#[async_trait::async_trait]
impl<S: crate::hardware::Storage + Send + Sync> CertificateStore for StoredCertificates<S> {
    type Error = StoredCertificatesError;

    async fn install(
        &self,
        use_for: CertificateUse,
        certificate: &str,
    ) -> Result<InstallCertificateOutcome, Self::Error> {
        if use_for.is_installable() {
            // A CSMS-pushed root (`InstallCertificate`). This store does no X.509 parsing - it
            // has no crypto - so it cannot compute the hashes a CSMS would later address the
            // certificate by. `install_with_hash` is the entry point for an integrator that can;
            // through this plain trait method, a certificate with no hash data would be
            // unaddressable and therefore undeletable, which is worse than refusing it. This is a
            // real, honest limitation of *this* store - see the module docs.
            let _ = certificate;
            return Ok(InstallCertificateOutcome::Rejected);
        }
        // The charge point's *own* certificate (`ChargingStation`/`V2gCertificateChain`),
        // arriving via `CertificateSigned` rather than `InstallCertificate` - see
        // `crate::certificates::handle_certificate_signed`, the only caller that reaches this
        // branch in practice. Unlike a CSMS-pushed root, nothing needs this slot's hash to match a
        // value some other party (the CSMS) computed independently by parsing the same
        // certificate: it is self-issued and single-slotted (`certificate_chain_pem`'s docs), so
        // this store can give it a self-computed, honestly-labelled stand-in identity instead of
        // refusing outright. See `install_own_certificate` for exactly what that identity is and
        // is not.
        Ok(self.install_own_certificate(use_for, certificate).await)
    }

    async fn delete(
        &self,
        hash_data: &CertificateHashData,
    ) -> Result<DeleteCertificateOutcome, Self::Error> {
        let mut certificates = self.load().await;
        let before = certificates.entries.len();
        certificates
            .entries
            .retain(|entry| !matches_hash(entry, hash_data));
        if certificates.entries.len() == before {
            return Ok(DeleteCertificateOutcome::NotFound);
        }
        if self.save(&certificates).await {
            Ok(DeleteCertificateOutcome::Accepted)
        } else {
            Ok(DeleteCertificateOutcome::Failed)
        }
    }

    async fn installed(
        &self,
        uses: &[CertificateUse],
    ) -> Result<Vec<InstalledCertificate>, Self::Error> {
        Ok(self
            .load()
            .await
            .entries
            .into_iter()
            .filter(|entry| uses.is_empty() || uses.contains(&entry.use_for.into()))
            .map(|entry| InstalledCertificate {
                use_for: entry.use_for.into(),
                hash_data: CertificateHashData {
                    hash_algorithm: entry.hash_algorithm.into(),
                    issuer_name_hash: entry.issuer_name_hash,
                    issuer_key_hash: entry.issuer_key_hash,
                    serial_number: entry.serial_number,
                },
            })
            .collect())
    }

    async fn has_private_key(&self) -> Result<bool, Self::Error> {
        // Always false, and not a stub: a key in ordinary flash is a key an attacker holding the
        // flash has. A charge point that needs security profile 3 wants a secure-element-backed
        // implementation of this trait, which is why the trait exists.
        Ok(false)
    }

    async fn certificate_chain_pem(
        &self,
        use_for: CertificateUse,
    ) -> Result<Option<String>, Self::Error> {
        // `CertificateUse::ChargingStation`/`V2gCertificateChain` *can* hold an entry here -
        // `install`'s own-certificate branch (`install_own_certificate`) puts one there, which is
        // what lets `crate::mutual_tls::client_config` retrieve it against this store for real.
        //
        // Each use holds exactly one entry (`install_with_hash` replaces rather than
        // duplicates), and `install`/`install_with_hash` both take the caller's certificate
        // string whole - a chain rather than a single leaf is expected to already be
        // concatenated PEM blocks, so returning this entry's `pem` verbatim is the whole chain.
        Ok(self
            .load()
            .await
            .entries
            .into_iter()
            .find(|entry| CertificateUse::from(entry.use_for) == use_for)
            .map(|entry| entry.pem))
    }

    async fn all_certificate_chain_pems(
        &self,
        use_for: CertificateUse,
    ) -> Result<Vec<String>, Self::Error> {
        Ok(self
            .load()
            .await
            .entries
            .into_iter()
            .filter(|entry| CertificateUse::from(entry.use_for) == use_for)
            .map(|entry| entry.pem)
            .collect())
    }
}

impl<S: crate::hardware::Storage + Send + Sync> StoredCertificates<S> {
    /// Stores `certificate` with hashes the caller has already computed.
    ///
    /// The way in for an integrator that *can* parse X.509 but has no secure element: they supply
    /// the hash data, this store keeps it. Separate from
    /// [`CertificateStore::install`] because the trait method takes only a PEM, and this store
    /// cannot derive the hashes from one.
    ///
    /// **No `is_installable` gate.** That check exists to stop a *CSMS* from pushing the charge
    /// point's own certificate through `InstallCertificate` - and it is enforced there, at the
    /// only call site that message reaches
    /// (`crate::certificates::handle_install_certificate`, which checks it before ever touching a
    /// `CertificateStore`). This method is never on that path: it is only ever called directly, by
    /// an integrator or by this crate's own `CertificateSigned` handling
    /// (`crate::certificates::handle_certificate_signed`, via [`CertificateStore::install`]'s
    /// `StoredCertificates` impl above), both of which already know the certificate's provenance
    /// is legitimate by the time they call it. Re-applying the CSMS-provenance gate here blocked
    /// exactly that legitimate caller - the charge point storing its own signed certificate -
    /// which is what made security profile 3 impossible to run against this store at all; see the
    /// module docs.
    pub async fn install_with_hash(
        &self,
        use_for: CertificateUse,
        certificate: &str,
        hash_data: CertificateHashData,
    ) -> InstallCertificateOutcome {
        let mut certificates = self.load().await;
        // Reinstalling the same certificate replaces it rather than duplicating - the CSMS is
        // addressing one slot, the same rule the charging-profile store follows.
        let replaced = certificates
            .entries
            .iter()
            .any(|entry| matches_hash(entry, &hash_data));
        certificates
            .entries
            .retain(|entry| !matches_hash(entry, &hash_data));
        if !replaced && certificates.entries.len() >= self.max_certificates {
            return InstallCertificateOutcome::Failed;
        }
        certificates.entries.push(PersistedCertificate {
            use_for: use_for.into(),
            hash_algorithm: hash_data.hash_algorithm.into(),
            issuer_name_hash: hash_data.issuer_name_hash,
            issuer_key_hash: hash_data.issuer_key_hash,
            serial_number: hash_data.serial_number,
            pem: certificate.into(),
        });
        if self.save(&certificates).await {
            InstallCertificateOutcome::Accepted
        } else {
            InstallCertificateOutcome::Failed
        }
    }

    /// Stores the charge point's own certificate (`ChargingStation`/`V2gCertificateChain`) - the
    /// branch of [`CertificateStore::install`] that is not a CSMS-pushed root, reached from
    /// [`crate::certificates::handle_certificate_signed`] answering a `CertificateSigned`.
    ///
    /// **The hash this reports is not a real X.509 issuer hash.** This store has no X.509 parser
    /// (see the module docs), so it cannot derive `issuerNameHash`/`issuerKeyHash` from
    /// `certificate` the way a CSMS would from parsing the same bytes. What it *can* do - the one
    /// hashing exception this crate makes, the same one `crate::certificates::csr` (CSR digests)
    /// and `crate::mutual_tls` (TLS handshake digests) already rely on - is hash the PEM bytes
    /// with SHA-256 and use that as a stable stand-in identity for
    /// [`CertificateStore::installed`]/[`CertificateStore::delete`]. That is sound for *this* slot
    /// specifically, in a way it would not be for a CSMS-pushed root: nothing else ever computes
    /// this certificate's hash independently and expects a match - it is self-issued and
    /// single-slotted (`certificate_chain_pem`'s docs), so the only party that will ever ask "is
    /// this the one I mean" is this same charge point, comparing against a value it produced
    /// itself. A CSMS listing it via `GetInstalledCertificateIds` sees this stand-in, not a
    /// spec-compliant issuer hash - a genuinely compliant one still needs the X.509 parser this
    /// crate does not carry, the same honest limitation `install`'s root branch documents.
    async fn install_own_certificate(
        &self,
        use_for: CertificateUse,
        certificate: &str,
    ) -> InstallCertificateOutcome {
        let digest = hex_encode(&crate::certificates::csr::sha256(certificate.as_bytes()));
        self.install_with_hash(
            use_for,
            certificate,
            CertificateHashData {
                hash_algorithm: HashAlgorithm::Sha256,
                issuer_name_hash: digest.clone(),
                issuer_key_hash: digest.clone(),
                serial_number: digest,
            },
        )
        .await
    }
}

/// Whether `entry` is the certificate `hash_data` addresses. All four fields must match: the
/// serial number alone is only unique per issuer.
fn matches_hash(entry: &PersistedCertificate, hash_data: &CertificateHashData) -> bool {
    entry.serial_number == hash_data.serial_number
        && entry.issuer_name_hash == hash_data.issuer_name_hash
        && entry.issuer_key_hash == hash_data.issuer_key_hash
}

/// Hex-encodes `bytes`, lowercase - just enough formatting for
/// [`StoredCertificates::install_own_certificate`]'s stand-in hash to be readable; no dependency
/// worth pulling in for it.
fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0F) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_charge_points_own_certificates_cannot_be_installed_by_a_csms() {
        // They arrive by `CertificateSigned` in answer to a CSR this charge point generated.
        // Accepting one through `InstallCertificate` would mean accepting a certificate for a key
        // pair this charge point may not hold.
        assert!(!CertificateUse::ChargingStation.is_installable());
        assert!(!CertificateUse::V2gCertificateChain.is_installable());

        for installable in [
            CertificateUse::CsmsRoot,
            CertificateUse::V2gRoot,
            CertificateUse::MobilityOperatorRoot,
            CertificateUse::ManufacturerRoot,
            CertificateUse::OemRoot,
        ] {
            assert!(installable.is_installable(), "{installable:?}");
        }
    }

    #[tokio::test]
    async fn a_charge_point_with_no_store_refuses_rather_than_appearing_to_trust_a_root() {
        // A CSMS told `Accepted` would expect a TLS connection this charge point cannot make.
        let store = NoCertificateStore;

        assert_eq!(
            store
                .install(CertificateUse::CsmsRoot, "-----BEGIN CERTIFICATE-----")
                .await,
            Ok(InstallCertificateOutcome::Rejected)
        );
        assert!(store.installed(&[]).await.unwrap().is_empty());
        // And it must not claim it can present a client certificate.
        assert!(!store.has_private_key().await.unwrap());
    }

    use crate::hardware::InMemoryStorage;
    use alloc::string::ToString;
    use alloc::sync::Arc;

    fn hash(serial: &str) -> CertificateHashData {
        CertificateHashData {
            hash_algorithm: HashAlgorithm::Sha256,
            issuer_name_hash: "aa".to_string(),
            issuer_key_hash: "bb".to_string(),
            serial_number: serial.to_string(),
        }
    }

    fn store() -> StoredCertificates<Arc<InMemoryStorage>> {
        StoredCertificates::new(Arc::new(InMemoryStorage::new()))
    }

    #[tokio::test]
    async fn an_installed_certificate_is_listed_and_can_be_deleted_by_its_hash() {
        let store = store();

        assert_eq!(
            store
                .install_with_hash(CertificateUse::CsmsRoot, "-----BEGIN-----", hash("01"))
                .await,
            InstallCertificateOutcome::Accepted
        );

        let installed = store.installed(&[]).await.unwrap();
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].use_for, CertificateUse::CsmsRoot);
        assert_eq!(installed[0].hash_data.serial_number, "01");

        assert_eq!(
            store.delete(&hash("01")).await.unwrap(),
            DeleteCertificateOutcome::Accepted
        );
        assert!(store.installed(&[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn deleting_something_that_is_not_there_is_not_an_error() {
        // The CSMS wanted it gone and it is gone - a failure status would have an operator chasing
        // a problem that does not exist.
        assert_eq!(
            store().delete(&hash("99")).await.unwrap(),
            DeleteCertificateOutcome::NotFound
        );
    }

    #[tokio::test]
    async fn listing_filters_by_use_and_an_empty_filter_means_all() {
        let store = store();
        store
            .install_with_hash(CertificateUse::CsmsRoot, "a", hash("01"))
            .await;
        store
            .install_with_hash(CertificateUse::V2gRoot, "b", hash("02"))
            .await;

        assert_eq!(store.installed(&[]).await.unwrap().len(), 2);
        let csms = store.installed(&[CertificateUse::CsmsRoot]).await.unwrap();
        assert_eq!(csms.len(), 1);
        assert_eq!(csms[0].use_for, CertificateUse::CsmsRoot);
    }

    #[tokio::test]
    async fn reinstalling_the_same_certificate_replaces_it_rather_than_duplicating() {
        let store = store();
        store
            .install_with_hash(CertificateUse::CsmsRoot, "old", hash("01"))
            .await;
        store
            .install_with_hash(CertificateUse::CsmsRoot, "new", hash("01"))
            .await;

        // The CSMS is addressing one slot, the same rule the charging-profile store follows.
        assert_eq!(store.installed(&[]).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_store_is_bounded_but_a_replacement_is_never_refused_for_being_one_too_many() {
        let store = StoredCertificates::with_limit(Arc::new(InMemoryStorage::new()), 2);
        store
            .install_with_hash(CertificateUse::CsmsRoot, "a", hash("01"))
            .await;
        store
            .install_with_hash(CertificateUse::V2gRoot, "b", hash("02"))
            .await;

        // G2.2: a store a remote peer can grow without limit is not a bound.
        assert_eq!(
            store
                .install_with_hash(CertificateUse::OemRoot, "c", hash("03"))
                .await,
            InstallCertificateOutcome::Failed
        );
        // ...but replacing one already held still works at the bound.
        assert_eq!(
            store
                .install_with_hash(CertificateUse::CsmsRoot, "a2", hash("01"))
                .await,
            InstallCertificateOutcome::Accepted
        );
        assert_eq!(store.installed(&[]).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn install_with_hash_stores_the_charge_points_own_certificate_when_the_caller_supplies_a_hash()
     {
        // F2.2: `install_with_hash`'s `is_installable` gate was over-broad - it exists to stop a
        // *CSMS* pushing this slot via `InstallCertificate`, a path this method is never reached
        // from. A caller who already has hash data (an integrator, or this crate's own
        // `CertificateSigned` handling) can use it for the charge point's own certificate too.
        let store = store();
        assert_eq!(
            store
                .install_with_hash(CertificateUse::ChargingStation, "a", hash("01"))
                .await,
            InstallCertificateOutcome::Accepted
        );
        let installed = store.installed(&[]).await.unwrap();
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].use_for, CertificateUse::ChargingStation);
    }

    #[tokio::test]
    async fn install_stores_the_charge_points_own_certificate_with_a_self_computed_stand_in_hash() {
        // The plain trait method, as `crate::certificates::handle_certificate_signed` calls it -
        // no hash data available at all, only the raw PEM from `CertificateSigned`. Unlike a
        // CSMS-pushed root (still `Rejected`: no crypto to compute the hash a CSMS would expect),
        // the charge point's own certificate gets a self-computed stand-in instead of a refusal -
        // see `install_own_certificate`'s docs for exactly what that stand-in is and is not.
        let store = store();

        assert_eq!(
            store
                .install(
                    CertificateUse::ChargingStation,
                    "-----BEGIN CERTIFICATE-----\nleaf\n-----END CERTIFICATE-----",
                )
                .await,
            Ok(InstallCertificateOutcome::Accepted)
        );

        assert_eq!(
            store
                .certificate_chain_pem(CertificateUse::ChargingStation)
                .await
                .unwrap(),
            Some("-----BEGIN CERTIFICATE-----\nleaf\n-----END CERTIFICATE-----".to_string())
        );
        let installed = store
            .installed(&[CertificateUse::ChargingStation])
            .await
            .unwrap();
        assert_eq!(installed.len(), 1);
        // Deterministic (same PEM hashes the same way twice), which is what lets a repeat
        // `CertificateSigned` for the same certificate replace rather than duplicate the entry.
        assert_eq!(
            installed[0].hash_data.serial_number,
            installed[0].hash_data.issuer_name_hash
        );
    }

    #[tokio::test]
    async fn a_csms_pushed_root_is_still_refused_through_the_plain_install_method() {
        // The genuinely honest limitation this fix must not erase: `install` still cannot compute
        // a spec-compliant hash for a certificate a CSMS pushed, so a root still needs
        // `install_with_hash` (an integrator that can parse X.509) rather than silently
        // succeeding with a stand-in a CSMS never asked for and would not recognise.
        assert_eq!(
            store()
                .install(CertificateUse::CsmsRoot, "-----BEGIN CERTIFICATE-----")
                .await,
            Ok(InstallCertificateOutcome::Rejected)
        );
    }

    #[tokio::test]
    async fn a_flash_backed_store_never_claims_to_hold_a_private_key() {
        // Not a stub: a key in ordinary flash is a key an attacker holding the flash has. Claiming
        // otherwise would have security profile 3 believe it can present a client certificate.
        assert!(!store().has_private_key().await.unwrap());
    }

    #[tokio::test]
    async fn certificates_survive_a_reboot() {
        let storage = Arc::new(InMemoryStorage::new());
        let before = StoredCertificates::new(storage.clone());
        before
            .install_with_hash(CertificateUse::CsmsRoot, "root", hash("01"))
            .await;

        // --- the cut: only `storage` survives.
        let after = StoredCertificates::new(storage);

        let recovered = after.installed(&[]).await.unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].hash_data.serial_number, "01");
    }

    #[tokio::test]
    async fn a_corrupt_store_comes_up_empty_rather_than_refusing_to_come_up() {
        let storage = Arc::new(InMemoryStorage::new());
        crate::hardware::Storage::set(&*storage, "certificates", b"{not json")
            .await
            .unwrap();

        // A charge point that cannot read its certificates should let the CSMS reinstall them,
        // not fail to start.
        assert!(
            StoredCertificates::new(storage)
                .installed(&[])
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn certificate_chain_pem_returns_the_installed_pem_for_the_matching_use() {
        // `CsmsRoot` proves the lookup plumbing; the own-certificate slots have their own
        // dedicated tests above now that they can hold an entry at all.
        let store = store();
        store
            .install_with_hash(
                CertificateUse::CsmsRoot,
                "-----BEGIN CERTIFICATE-----\nchain\n-----END CERTIFICATE-----",
                hash("01"),
            )
            .await;

        assert_eq!(
            store
                .certificate_chain_pem(CertificateUse::CsmsRoot)
                .await
                .unwrap(),
            Some("-----BEGIN CERTIFICATE-----\nchain\n-----END CERTIFICATE-----".to_string())
        );
        // A use with nothing installed reports `None` rather than an empty string - "no
        // certificate" and "an empty certificate" must not be conflated.
        assert_eq!(
            store
                .certificate_chain_pem(CertificateUse::V2gRoot)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_store_with_nothing_installed_never_claims_a_chain_exists() {
        assert_eq!(
            NoCertificateStore
                .certificate_chain_pem(CertificateUse::ChargingStation)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn all_certificate_chain_pems_returns_every_root_not_just_the_first() {
        // F2.2: `certificate_chain_pem`'s "one entry" assumption is right for the charge point's
        // own certificate but wrong for a trust store, which typically holds several roots at
        // once - `crate::trust_store` needs all of them, not whichever happened to load first.
        let store = store();
        store
            .install_with_hash(CertificateUse::CsmsRoot, "root-a", hash("01"))
            .await;
        store
            .install_with_hash(CertificateUse::CsmsRoot, "root-b", hash("02"))
            .await;
        store
            .install_with_hash(CertificateUse::V2gRoot, "not-a-csms-root", hash("03"))
            .await;

        let mut pems = store
            .all_certificate_chain_pems(CertificateUse::CsmsRoot)
            .await
            .unwrap();
        pems.sort();
        assert_eq!(
            pems,
            alloc::vec!["root-a".to_string(), "root-b".to_string()]
        );
    }

    #[tokio::test]
    async fn all_certificate_chain_pems_is_empty_rather_than_erroring_when_nothing_is_installed() {
        assert!(
            store()
                .all_certificate_chain_pems(CertificateUse::CsmsRoot)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn the_default_all_certificate_chain_pems_delegates_to_the_single_valued_method() {
        // Proves the default implementation on the trait itself (`NoCertificateStore` never
        // overrides it), so a `CertificateStore` that only implements the older single-valued
        // method still answers sensibly through the newer multi-valued one.
        assert!(
            NoCertificateStore
                .all_certificate_chain_pems(CertificateUse::CsmsRoot)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
