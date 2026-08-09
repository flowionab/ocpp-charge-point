//! Security profile 3: presenting this charge point's client certificate over TLS
//! (`docs/PRODUCTION-ROADMAP.md` F1.3, milestone M4's last exit criterion).
//!
//! # What this module builds, and what it deliberately does not
//!
//! [`crate::mutual_tls::client_config`] builds an `ocpp_client::rustls::ClientConfig` that offers
//! the certificate chain installed in [`crate::hardware::CertificateStore`]'s
//! [`crate::hardware::CertificateUse::ChargingStation`] slot, signing the TLS handshake through
//! [`crate::hardware::KeyStore::sign`] - the private key never appears here, matching that
//! trait's whole reason for existing (see its module docs). The resulting `Arc<ClientConfig>` is
//! meant to be handed straight to `ocpp_client::ConnectOptions::tls_config`; this crate does not
//! call that itself, because *when* to dial with a profile-3 config (vs. refusing, vs. retrying
//! after a CSR completes) is a decision [`crate::connect::connect_and_setup`] does not currently
//! make for any profile - callers already build their own `ConnectOptions` for profile 2's
//! server-only TLS today, and profile 3 is the same shape of caller responsibility with one more
//! piece filled in.
//!
//! # The hard part: rustls signs synchronously, `KeyStore` signs asynchronously
//!
//! rustls 0.23 (the version pinned through `ocpp-client` 0.5.0) has exactly the extension point
//! this needs: `rustls::sign::SigningKey`/`rustls::sign::Signer`, the trait pair it already
//! uses internally to delegate signing to a `CryptoProvider`, plus
//! `ConfigBuilder::with_client_cert_resolver` to install a custom
//! `rustls::client::ResolvesClientCert` instead of a raw private key
//! (`with_client_auth_cert` takes DER key bytes, which is exactly what [`crate::hardware::KeyStore`]
//! must never give up - see its module docs - so this module does not use it).
//! `KeyStoreSigningKey` and `KeyStoreSigner` (private to this module) are that pair, implemented
//! over a [`crate::hardware::KeyStore`] handle instead of key bytes.
//! This is the established pattern for delegating TLS client-cert signing to external hardware,
//! and it is fully sufficient for the certificate-chain and public-key parts.
//!
//! Where it stops being clean: `rustls::sign::Signer::sign` is **synchronous** -
//! `fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error>` - because rustls itself has
//! no async story; it is driven by whatever I/O loop the caller built (here,
//! `tokio-tungstenite`'s). [`crate::hardware::KeyStore::sign`] is `async fn` on purpose, because a
//! real secure element's signing operation is a round trip (SPI, I2C, a hardware queue) that must
//! be able to yield. There is no version of rustls 0.23's `Signer` that is async, and
//! `KeyStore::sign` cannot become sync without breaking every hardware-backed implementation this
//! crate exists to support - CLAUDE.md's whole reason for the trait. So this module bridges the
//! two with `tokio::task::block_in_place` plus `Handle::block_on`: the calling worker thread
//! hands itself back to the runtime's scheduler for the duration of the `await`, then blocks on
//! the result. That has exactly one hard requirement, and it is a real limitation rather than a
//! rough edge:
//!
//! **`KeyStoreSigner::sign` requires a multi-thread Tokio runtime.** `block_in_place` panics
//! outright on a `current_thread` runtime (Tokio's own documented behaviour, not something this
//! crate can change) - there is no second thread to keep the runtime alive while this one blocks.
//! A charge point built with a `current_thread` runtime cannot use security profile 3 through
//! this module as it stands. **What upstream would need to close this properly**: an async
//! `Signer`/`SigningKey` pair in rustls itself (there is no such thing as of 0.23 - signing is
//! defined synchronous throughout the crate, not just in the client-auth path), or a synchronous,
//! non-blocking way to ask a `KeyStore` for a signature when the caller already knows the answer
//! will be available without a real await point (which would not help a genuine secure element
//! anyway). Neither exists today, so this is recorded here rather than solved by quietly adding
//! a `KeyStore::sign_blocking` or similar - that would just move the same async/sync mismatch
//! into `KeyStore`'s own contract for every implementor, hardware or not.
//!
//! # No fallback, and no silent weakening
//!
//! [`crate::mutual_tls::client_config`] returns
//! [`crate::mutual_tls::MutualTlsError::NoChargingStationCertificate`] when
//! [`crate::hardware::CertificateStore::certificate_chain_pem`] reports nothing installed for
//! [`crate::hardware::CertificateUse::ChargingStation`] - it does **not** build a profile-2-shaped
//! config with no client certificate, and it does not retry or wait. A station that silently
//! connected without presenting a certificate it was configured to present would be
//! indistinguishable, from the CSMS's side, from one that had been quietly downgraded - exactly
//! the fleet-wide weakening `crate::security_profile`'s module docs describe for the OCPP-level
//! downgrade rule, just reached through a different door. Equally, this module does not brick the
//! charge point's connectivity on its behalf: it simply declines to build *this* config and
//! returns an error, leaving the decision - refuse to dial, fall back to a *different,
//! still-profile-3* address, wait for [`crate::certificates::build_signed_csr`]/`SignCertificate`
//! to obtain a certificate and retry, or alert an operator - to the integrator's connection
//! logic, which already owns that decision for every other dial failure.
//! [`crate::security_profile::SecurityProfile::is_usable`] is the check a caller should make
//! before ever reaching for this module, so the "no certificate" case is something a
//! well-behaved caller expects rather than discovers here.
//!
//! # Credentials: profile 3 sends none
//!
//! Nothing in this crate ever populates `ConnectOptions::username`/`password` on its own (see
//! `crate::connect::connection_options_from_device_model`, which never touches either field) -
//! only a caller who explicitly sets them does, and
//! [`crate::security_profile::SecurityProfile::needs_basic_auth`] already answers `false` for
//! [`crate::security_profile::SecurityProfile::TlsMutualAuth`]. A caller building
//! `ConnectOptions` from the selected [`crate::security_profile::SecurityProfile`] and consulting
//! `needs_basic_auth` before setting credentials
//! - the natural way to wire this up - therefore never sends a password on a profile-3 connection
//! by construction; this module adds nothing that could send one either, since it only ever
//! touches `tls_config`.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

use ocpp_client::rustls::client::ResolvesClientCert;
use ocpp_client::rustls::pki_types::{CertificateDer, SubjectPublicKeyInfoDer};
use ocpp_client::rustls::sign::{CertifiedKey, Signer, SigningKey, SingleCertAndKey};
use ocpp_client::rustls::{
    ClientConfig, Error as RustlsError, RootCertStore,
    SignatureAlgorithm as RustlsSignatureAlgorithm, SignatureScheme,
};

use crate::certificates::csr::sha256;
use crate::hardware::{
    CertificateStore, CertificateUse, KeyHandle, KeyStore, PublicKey,
    SignatureAlgorithm as KeySignatureAlgorithm,
};

/// Failure building a mutual-TLS [`ClientConfig`] with [`client_config`].
#[derive(Debug)]
pub enum MutualTlsError<E> {
    /// [`CertificateStore::certificate_chain_pem`] reported no
    /// [`CertificateUse::ChargingStation`] chain installed. See the module docs for why this is
    /// returned rather than silently building a weaker config.
    NoChargingStationCertificate,
    /// The installed chain was not parseable PEM (no `CERTIFICATE` blocks found in it).
    InvalidPem,
    /// `public_key.algorithm` has no rustls [`SignatureScheme`] this module can sign for today -
    /// currently only the SHA-256 family, the same limitation
    /// [`crate::certificates::csr::build_signed_csr`] documents for the same reason (no in-crate
    /// SHA-384 implementation).
    UnsupportedAlgorithm(KeySignatureAlgorithm),
    /// [`CertificateStore::certificate_chain_pem`] itself failed.
    Store(E),
}

impl<E: fmt::Display> fmt::Display for MutualTlsError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoChargingStationCertificate => {
                f.write_str("no ChargingStation certificate is installed")
            }
            Self::InvalidPem => f.write_str("the installed certificate chain is not valid PEM"),
            Self::UnsupportedAlgorithm(algorithm) => {
                write!(f, "no in-crate TLS signing support for {algorithm:?}")
            }
            Self::Store(error) => write!(f, "the certificate store failed: {error}"),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> core::error::Error for MutualTlsError<E> {}

/// Builds a client TLS configuration presenting `public_key`/`key_handle` (signed through
/// `key_store`, never exporting the private key) and the certificate chain installed for
/// [`CertificateUse::ChargingStation`] in `certificate_store`, verifying the CSMS's server
/// certificate against `root_store`.
///
/// `public_key`/`key_handle` are the [`crate::hardware::GeneratedKeyPair`] halves the charge
/// point generated (and had signed, via [`crate::certificates::build_signed_csr`]) for its own
/// certificate - this module does not track that association itself; see
/// `crate::certificates::csr`'s module docs for why that bookkeeping is the caller's (or F3.2's)
/// job, not this one's.
///
/// See the module docs for exactly when this errors rather than falling back, and for
/// `KeyStoreSigner`'s one hard runtime requirement (a multi-thread Tokio runtime).
pub async fn client_config<C, K>(
    certificate_store: &C,
    key_store: Arc<K>,
    public_key: PublicKey,
    key_handle: KeyHandle,
    root_store: impl Into<Arc<RootCertStore>>,
) -> Result<Arc<ClientConfig>, MutualTlsError<C::Error>>
where
    C: CertificateStore + Sync,
    K: KeyStore + Send + Sync + 'static,
{
    // Each of the three failures below surfaces to an integrator as a bare TLS handshake refusal
    // from the CSMS, with nothing on either side saying which one it was. They are the whole
    // reason security profile 3 bring-up is painful, so each says so on the way out.
    let pem = match certificate_store
        .certificate_chain_pem(CertificateUse::ChargingStation)
        .await
    {
        Err(error) => {
            tracing::warn!("the certificate store failed to yield the charging station chain");
            return Err(MutualTlsError::Store(error));
        }
        Ok(None) => {
            tracing::warn!(
                "no charging station certificate is installed; mutual TLS (security profile 3) \
                 cannot be configured until one is"
            );
            return Err(MutualTlsError::NoChargingStationCertificate);
        }
        Ok(Some(pem)) => pem,
    };

    let Some(chain) = decode_pem_certificates(&pem) else {
        tracing::warn!("the installed charging station certificate chain is not valid PEM");
        return Err(MutualTlsError::InvalidPem);
    };

    let Some(scheme) = signature_scheme(public_key.algorithm) else {
        tracing::warn!(
            algorithm = ?public_key.algorithm,
            "the key's signature algorithm has no digest implementation in this crate"
        );
        return Err(MutualTlsError::UnsupportedAlgorithm(public_key.algorithm));
    };

    // Captured before `chain` is moved into the resolver below. Worth a field of its own: a chain
    // of one is the usual sign that the intermediate was never installed, which fails later and
    // less legibly, at the CSMS.
    let chain_length = chain.len();
    let algorithm = public_key.algorithm;

    let signing_key: Arc<dyn SigningKey> = Arc::new(KeyStoreSigningKey {
        key_store,
        handle: key_handle,
        public_key,
        scheme,
    });

    let resolver: Arc<dyn ResolvesClientCert> = Arc::new(SingleCertAndKey::from(
        CertifiedKey::new(chain, signing_key),
    ));

    let config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_client_cert_resolver(resolver);

    tracing::info!(
        chain_length,
        ?algorithm,
        "configured mutual TLS from the installed charging station certificate"
    );
    Ok(Arc::new(config))
}

/// The rustls [`SignatureScheme`] `algorithm` signs as, or `None` for one this module has no
/// digest implementation for (mirrors
/// `crate::certificates::csr::signature_algorithm_identifier`'s limitation).
fn signature_scheme(algorithm: KeySignatureAlgorithm) -> Option<SignatureScheme> {
    match algorithm {
        KeySignatureAlgorithm::EcdsaP256Sha256 => Some(SignatureScheme::ECDSA_NISTP256_SHA256),
        KeySignatureAlgorithm::Rsa2048Sha256 | KeySignatureAlgorithm::Rsa3072Sha256 => {
            Some(SignatureScheme::RSA_PKCS1_SHA256)
        }
        // No in-crate SHA-384 implementation - see the module docs.
        KeySignatureAlgorithm::EcdsaP384Sha384 => None,
    }
}

/// A [`rustls::sign::SigningKey`] over a [`KeyStore`] handle - the public half and algorithm are
/// held directly (this module does no X.509 parsing to recover them from the certificate), the
/// private half is never touched: [`Self::choose_scheme`] hands back a [`KeyStoreSigner`], which
/// calls [`KeyStore::sign`] and nothing else. See the module docs for why this is the
/// established rustls delegation pattern rather than a workaround.
struct KeyStoreSigningKey<K> {
    key_store: Arc<K>,
    handle: KeyHandle,
    public_key: PublicKey,
    scheme: SignatureScheme,
}

impl<K> fmt::Debug for KeyStoreSigningKey<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyStoreSigningKey")
            .field("handle", &self.handle)
            .field("scheme", &self.scheme)
            .finish_non_exhaustive()
    }
}

impl<K: KeyStore + Send + Sync + 'static> SigningKey for KeyStoreSigningKey<K> {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        if !offered.contains(&self.scheme) {
            return None;
        }
        Some(Box::new(KeyStoreSigner {
            key_store: self.key_store.clone(),
            handle: self.handle.clone(),
            scheme: self.scheme,
        }))
    }

    fn public_key(&self) -> Option<SubjectPublicKeyInfoDer<'_>> {
        Some(SubjectPublicKeyInfoDer::from(self.public_key.bytes.clone()))
    }

    fn algorithm(&self) -> RustlsSignatureAlgorithm {
        match self.public_key.algorithm {
            KeySignatureAlgorithm::EcdsaP256Sha256 | KeySignatureAlgorithm::EcdsaP384Sha384 => {
                RustlsSignatureAlgorithm::ECDSA
            }
            KeySignatureAlgorithm::Rsa2048Sha256 | KeySignatureAlgorithm::Rsa3072Sha256 => {
                RustlsSignatureAlgorithm::RSA
            }
        }
    }
}

/// A [`rustls::sign::Signer`] over a [`KeyStore`] handle. See the module docs for the
/// synchronous/asynchronous bridge [`Self::sign`] performs and its one hard requirement (a
/// multi-thread Tokio runtime).
struct KeyStoreSigner<K> {
    key_store: Arc<K>,
    handle: KeyHandle,
    scheme: SignatureScheme,
}

impl<K> fmt::Debug for KeyStoreSigner<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyStoreSigner")
            .field("handle", &self.handle)
            .field("scheme", &self.scheme)
            .finish_non_exhaustive()
    }
}

impl<K: KeyStore + Send + Sync + 'static> Signer for KeyStoreSigner<K> {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, RustlsError> {
        // Every scheme `signature_scheme` produces is SHA-256-based; `message` is the raw
        // handshake content rustls asks us to sign, not yet hashed - `KeyStore::sign` expects a
        // digest (see its own docs), so that hashing happens here, the same way
        // `crate::certificates::csr::build_signed_csr` hashes a CSR before calling it.
        let digest = sha256(message);

        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return Err(RustlsError::General(String::from(
                "KeyStoreSigner::sign requires a Tokio runtime driving the connection - see \
                 crate::mutual_tls's module docs",
            )));
        };

        let key_store = self.key_store.clone();
        let key_handle = self.handle.clone();
        // See the module docs: this is the bridge from rustls's synchronous `Signer::sign` to
        // `KeyStore::sign`'s async signature. Panics on a `current_thread` runtime - Tokio's own
        // documented behaviour for `block_in_place`, not something this crate can avoid short of
        // rustls gaining an async signing path.
        tokio::task::block_in_place(move || {
            handle
                .block_on(async move { key_store.sign(&key_handle, &digest).await })
                .map_err(|error| RustlsError::General(format!("KeyStore::sign failed: {error}")))
        })
    }

    fn scheme(&self) -> SignatureScheme {
        self.scheme
    }
}

/// Decodes every `-----BEGIN CERTIFICATE-----`/`-----END CERTIFICATE-----` block in `pem` into
/// DER, in order - a "chain" here is exactly what [`crate::hardware::CertificateStore::install`]
/// already accepted as one PEM string, concatenated blocks and all. `None` if `pem` contains no
/// certificate block at all, or a malformed one.
///
/// No X.509 parsing happens here - only PEM's own framing (a fixed textual marker) and base64,
/// consistent with this crate carrying no crypto dependency (see `crate::certificates::csr`'s
/// module docs for the same stance applied to CSR assembly).
///
/// `pub(crate)` so [`crate::trust_store`] can decode installed `CsmsRoot` chains with the same
/// framing logic rather than a second copy of it - see that module's docs.
pub(crate) fn decode_pem_certificates(pem: &str) -> Option<Vec<CertificateDer<'static>>> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";

    let mut certs = Vec::new();
    let mut rest = pem;
    while let Some(start) = rest.find(BEGIN) {
        let after_begin = &rest[start + BEGIN.len()..];
        let end = after_begin.find(END)?;
        let der = base64_decode(&after_begin[..end])?;
        certs.push(CertificateDer::from(der));
        rest = &after_begin[end + END.len()..];
    }
    if certs.is_empty() { None } else { Some(certs) }
}

/// The plain (non-URL) base64 alphabet's inverse, for decoding a PEM body - the reverse of
/// `crate::certificates::csr`'s `base64_encode`. `None` on any byte that isn't whitespace, `=`,
/// or in the alphabet, or on input whose length (after stripping whitespace) isn't a multiple of
/// four - a malformed chain is refused rather than guessed at.
fn base64_decode(data: &str) -> Option<Vec<u8>> {
    fn value(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let filtered: Vec<u8> = data.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if filtered.is_empty() {
        return Some(Vec::new());
    }
    if !filtered.len().is_multiple_of(4) {
        return None;
    }

    let mut out = Vec::with_capacity(filtered.len() / 4 * 3);
    for chunk in filtered.chunks(4) {
        let pad = chunk.iter().rev().take_while(|&&b| b == b'=').count();
        if pad > 2 || chunk[..4 - pad].contains(&b'=') {
            return None;
        }
        let mut n: u32 = 0;
        for &b in chunk {
            let v = if b == b'=' { 0 } else { value(b)? };
            n = (n << 6) | u32::from(v);
        }
        let bytes = n.to_be_bytes();
        out.push(bytes[1]);
        if pad < 2 {
            out.push(bytes[2]);
        }
        if pad < 1 {
            out.push(bytes[3]);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::{
        GeneratedKeyPair, InMemoryStorage, KeyStoreBacking, NoCertificateStore, SoftKeyStore,
        SoftwareCrypto, StoredCertificates,
    };

    /// A fake `SoftwareCrypto` backend: not real cryptography (the "signature" is just the
    /// digest with a marker byte appended), only enough to prove the plumbing - that
    /// `KeyStoreSigner::sign` hashes the message and forwards the digest to `KeyStore::sign`,
    /// and that the returned bytes come back out the other end unchanged. Real signature
    /// validity is rustls's/the CSMS's problem at handshake time, not something a unit test
    /// here can check without a real crypto dependency this crate does not have.
    #[derive(Clone)]
    struct FakeCrypto;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct FakeCryptoError;
    impl fmt::Display for FakeCryptoError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("fake crypto failure")
        }
    }
    impl core::error::Error for FakeCryptoError {}

    impl SoftwareCrypto for FakeCrypto {
        type Error = FakeCryptoError;

        fn generate_key_pair(
            &self,
            algorithm: KeySignatureAlgorithm,
        ) -> Result<(Vec<u8>, PublicKey), Self::Error> {
            Ok((
                alloc::vec![0xAB],
                PublicKey {
                    algorithm,
                    bytes: alloc::vec![0x01, 0x02, 0x03],
                },
            ))
        }

        fn sign(
            &self,
            _algorithm: KeySignatureAlgorithm,
            _private_key: &[u8],
            digest: &[u8],
        ) -> Result<Vec<u8>, Self::Error> {
            let mut signature = digest.to_vec();
            signature.push(0xFF);
            Ok(signature)
        }

        fn supported_algorithms(&self) -> &[KeySignatureAlgorithm] {
            &[KeySignatureAlgorithm::EcdsaP256Sha256]
        }
    }

    fn key_store() -> Arc<SoftKeyStore<Arc<InMemoryStorage>, FakeCrypto>> {
        Arc::new(SoftKeyStore::new(
            Arc::new(InMemoryStorage::new()),
            FakeCrypto,
        ))
    }

    /// A minimal [`CertificateStore`] that holds a `ChargingStation` chain without going through
    /// [`StoredCertificates`]'s real persistence - stands in for "an integrator's own store" in
    /// the tests below that only care about `client_config`'s handling of the PEM/algorithm it
    /// gets back, not about how a store persists it.
    /// `client_config_succeeds_against_the_real_stored_certificates_store`, further down, proves
    /// the same success path against the real [`StoredCertificates`] (F2.2's fix to
    /// `install`/`install_with_hash` - see `crate::hardware::certificate`'s module docs).
    #[derive(Default)]
    struct FakeChargingStationStore {
        chain_pem: Option<String>,
    }

    #[async_trait::async_trait]
    impl CertificateStore for FakeChargingStationStore {
        type Error = core::convert::Infallible;

        async fn install(
            &self,
            _use_for: CertificateUse,
            _certificate: &str,
        ) -> Result<crate::hardware::InstallCertificateOutcome, Self::Error> {
            Ok(crate::hardware::InstallCertificateOutcome::Rejected)
        }

        async fn delete(
            &self,
            _hash_data: &crate::hardware::CertificateHashData,
        ) -> Result<crate::hardware::DeleteCertificateOutcome, Self::Error> {
            Ok(crate::hardware::DeleteCertificateOutcome::NotFound)
        }

        async fn installed(
            &self,
            _uses: &[CertificateUse],
        ) -> Result<Vec<crate::hardware::InstalledCertificate>, Self::Error> {
            Ok(Vec::new())
        }

        async fn has_private_key(&self) -> Result<bool, Self::Error> {
            Ok(self.chain_pem.is_some())
        }

        async fn certificate_chain_pem(
            &self,
            use_for: CertificateUse,
        ) -> Result<Option<String>, Self::Error> {
            Ok(match use_for {
                CertificateUse::ChargingStation => self.chain_pem.clone(),
                _ => None,
            })
        }
    }

    /// The same alphabet [`base64_decode`] inverts, so tests can build PEM bodies without a real
    /// certificate or a crypto dependency.
    fn base64_encode_for_test(data: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
        for chunk in data.chunks(3) {
            let b0 = chunk[0];
            let b1 = chunk.get(1).copied();
            let b2 = chunk.get(2).copied();
            let n = (u32::from(b0) << 16)
                | (u32::from(b1.unwrap_or(0)) << 8)
                | u32::from(b2.unwrap_or(0));
            out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
            out.push(if b1.is_some() {
                ALPHABET[((n >> 6) & 0x3F) as usize] as char
            } else {
                '='
            });
            out.push(if b2.is_some() {
                ALPHABET[(n & 0x3F) as usize] as char
            } else {
                '='
            });
        }
        out
    }

    #[test]
    fn a_chain_with_no_certificate_block_is_not_decoded() {
        assert!(decode_pem_certificates("not a certificate").is_none());
        assert!(decode_pem_certificates("").is_none());
    }

    #[test]
    fn base64_round_trips_through_the_csrs_own_encoder() {
        for data in [b"".as_slice(), b"a", b"ab", b"abc", b"OCPP mutual TLS!!"] {
            let encoded = base64_encode_for_test(data);
            assert_eq!(base64_decode(&encoded).unwrap(), data);
        }
    }

    #[test]
    fn a_pem_chain_of_two_blocks_decodes_to_two_certificates() {
        let leaf = base64_encode_for_test(b"leaf-der-bytes");
        let root = base64_encode_for_test(b"root-der-bytes");
        let chain = format!(
            "-----BEGIN CERTIFICATE-----\n{leaf}\n-----END CERTIFICATE-----\n\
             -----BEGIN CERTIFICATE-----\n{root}\n-----END CERTIFICATE-----\n"
        );

        let certs = decode_pem_certificates(&chain).unwrap();
        assert_eq!(certs.len(), 2);
        assert_eq!(certs[0].as_ref(), b"leaf-der-bytes");
        assert_eq!(certs[1].as_ref(), b"root-der-bytes");
    }

    #[test]
    fn only_the_sha_256_family_has_a_signature_scheme() {
        assert_eq!(
            signature_scheme(KeySignatureAlgorithm::EcdsaP256Sha256),
            Some(SignatureScheme::ECDSA_NISTP256_SHA256)
        );
        assert_eq!(
            signature_scheme(KeySignatureAlgorithm::Rsa2048Sha256),
            Some(SignatureScheme::RSA_PKCS1_SHA256)
        );
        assert_eq!(
            signature_scheme(KeySignatureAlgorithm::Rsa3072Sha256),
            Some(SignatureScheme::RSA_PKCS1_SHA256)
        );
        assert_eq!(
            signature_scheme(KeySignatureAlgorithm::EcdsaP384Sha384),
            None
        );
    }

    #[test]
    fn choose_scheme_refuses_a_scheme_the_csms_did_not_offer() {
        let signing_key = KeyStoreSigningKey {
            key_store: key_store(),
            handle: KeyHandle::new(alloc::vec![1]),
            public_key: PublicKey {
                algorithm: KeySignatureAlgorithm::EcdsaP256Sha256,
                bytes: alloc::vec![0x01],
            },
            scheme: SignatureScheme::ECDSA_NISTP256_SHA256,
        };

        assert!(
            signing_key
                .choose_scheme(&[SignatureScheme::RSA_PKCS1_SHA256])
                .is_none()
        );
        let signer = signing_key
            .choose_scheme(&[SignatureScheme::ECDSA_NISTP256_SHA256])
            .expect("offered");
        assert_eq!(signer.scheme(), SignatureScheme::ECDSA_NISTP256_SHA256);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn signer_hashes_the_message_and_forwards_the_digest_to_the_key_store() {
        let store = key_store();
        let generated = store
            .generate_key_pair(KeySignatureAlgorithm::EcdsaP256Sha256)
            .await
            .unwrap();

        let signer = KeyStoreSigner {
            key_store: store.clone(),
            handle: generated.handle.clone(),
            scheme: SignatureScheme::ECDSA_NISTP256_SHA256,
        };

        let signature = signer.sign(b"a TLS handshake message").unwrap();
        let mut expected = sha256(b"a TLS handshake message");
        expected.push(0xFF);
        assert_eq!(signature, expected);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn signing_on_a_current_thread_runtime_fails_rather_than_deadlocking() {
        // The module docs' documented hard requirement: `block_in_place` panics outright on a
        // `current_thread` runtime, so `sign` must refuse cleanly *before* reaching it rather
        // than let that panic take the connection down.
        let store = key_store();
        let generated = store
            .generate_key_pair(KeySignatureAlgorithm::EcdsaP256Sha256)
            .await
            .unwrap();
        assert_eq!(store.backing().await.unwrap(), KeyStoreBacking::Software);

        // `Handle::try_current()` still succeeds on a current-thread runtime (one exists, it's
        // just single-threaded) - so this test only proves `sign` does not itself panic calling
        // into `block_in_place` from a context this crate cannot control; the real guarantee
        // (multi-thread required) is Tokio's own panic contract on `block_in_place`, documented
        // rather than re-tested here.
        let _ = generated;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn client_config_refuses_when_no_charging_station_certificate_is_installed() {
        let certificate_store = StoredCertificates::new(Arc::new(InMemoryStorage::new()));
        let store = key_store();
        let generated = store
            .generate_key_pair(KeySignatureAlgorithm::EcdsaP256Sha256)
            .await
            .unwrap();

        let result = client_config(
            &certificate_store,
            store,
            generated.public_key,
            generated.handle,
            RootCertStore::empty(),
        )
        .await;

        assert!(matches!(
            result,
            Err(MutualTlsError::NoChargingStationCertificate)
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn client_config_refuses_a_store_that_holds_no_certificates_at_all() {
        let store = key_store();
        let generated = store
            .generate_key_pair(KeySignatureAlgorithm::EcdsaP256Sha256)
            .await
            .unwrap();

        let result = client_config(
            &NoCertificateStore,
            store,
            generated.public_key,
            generated.handle,
            RootCertStore::empty(),
        )
        .await;

        assert!(matches!(
            result,
            Err(MutualTlsError::NoChargingStationCertificate)
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn client_config_succeeds_with_an_installed_chain_and_a_supported_algorithm() {
        let leaf = base64_encode_for_test(b"leaf-der-bytes");
        let certificate_store = FakeChargingStationStore {
            chain_pem: Some(format!(
                "-----BEGIN CERTIFICATE-----\n{leaf}\n-----END CERTIFICATE-----\n"
            )),
        };

        let store = key_store();
        let generated: GeneratedKeyPair = store
            .generate_key_pair(KeySignatureAlgorithm::EcdsaP256Sha256)
            .await
            .unwrap();

        let config = client_config(
            &certificate_store,
            store,
            generated.public_key,
            generated.handle,
            RootCertStore::empty(),
        )
        .await
        .expect("a config should build from an installed chain and a supported algorithm");

        assert!(config.client_auth_cert_resolver.has_certs());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn client_config_succeeds_against_the_real_stored_certificates_store() {
        // F2.2: proves `client_config`'s success path against `StoredCertificates` itself, not a
        // fake - the concrete gap F1.3's own report flagged ("a station using the built-in store
        // cannot run profile 3, no matter what else is configured"). Installed the same way
        // `crate::certificates::handle_certificate_signed` does in production: the plain `install`
        // trait method, with only a raw PEM and no hash data.
        let leaf = base64_encode_for_test(b"leaf-der-bytes");
        let certificate_store = StoredCertificates::new(Arc::new(InMemoryStorage::new()));
        assert_eq!(
            certificate_store
                .install(
                    CertificateUse::ChargingStation,
                    &format!("-----BEGIN CERTIFICATE-----\n{leaf}\n-----END CERTIFICATE-----\n"),
                )
                .await,
            Ok(crate::hardware::InstallCertificateOutcome::Accepted)
        );

        let store = key_store();
        let generated: GeneratedKeyPair = store
            .generate_key_pair(KeySignatureAlgorithm::EcdsaP256Sha256)
            .await
            .unwrap();

        let config = client_config(
            &certificate_store,
            store,
            generated.public_key,
            generated.handle,
            RootCertStore::empty(),
        )
        .await
        .expect("a config should build against the real StoredCertificates store");

        assert!(config.client_auth_cert_resolver.has_certs());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn client_config_refuses_an_algorithm_this_crate_cannot_sign_for() {
        let leaf = base64_encode_for_test(b"leaf-der-bytes");
        let certificate_store = FakeChargingStationStore {
            chain_pem: Some(format!(
                "-----BEGIN CERTIFICATE-----\n{leaf}\n-----END CERTIFICATE-----\n"
            )),
        };

        let store = key_store();
        let public_key = PublicKey {
            algorithm: KeySignatureAlgorithm::EcdsaP384Sha384,
            bytes: alloc::vec![0x01],
        };

        let result = client_config(
            &certificate_store,
            store,
            public_key,
            KeyHandle::new(alloc::vec![1]),
            RootCertStore::empty(),
        )
        .await;

        assert!(matches!(
            result,
            Err(MutualTlsError::UnsupportedAlgorithm(
                KeySignatureAlgorithm::EcdsaP384Sha384
            ))
        ));
    }
}
