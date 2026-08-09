//! Trust store management (`docs/PRODUCTION-ROADMAP.md` F2.2): the CSMS roots this charge point
//! has installed (`CertificateUse::CsmsRoot`, via `InstallCertificate`/B4.2) become the
//! `rustls::RootCertStore` a TLS connection verifies the CSMS's server certificate against,
//! instead of whatever `rustls`/`ocpp-client` would trust by default (public CAs only - see
//! `ocpp_client::ConnectOptions::tls_config`'s own docs).
//!
//! # Building the store
//!
//! [`build_root_cert_store`] turns every installed `CsmsRoot` chain into rustls trust anchors. It
//! needs *every* PEM the store holds for that use, not just one - unlike
//! [`CertificateStore::certificate_chain_pem`] (right for the charge point's own single-slotted
//! certificate, wrong for a trust store, which typically holds several roots side by side), so it
//! goes through [`CertificateStore::all_certificate_chain_pems`], the multi-valued counterpart
//! this task adds to that trait. PEM framing is decoded with
//! [`crate::mutual_tls::decode_pem_certificates`] - the same routine [`crate::mutual_tls`]
//! already uses for the charge point's own chain - rather than a second copy of it.
//!
//! # Never "empty means trust everything"
//!
//! No installed `CsmsRoot` at all is reported as [`None`]: "nothing installed" is not the same
//! claim as "an empty trust store", and a caller who gets `None` back is expected to leave
//! whatever TLS trust configuration it already had alone (public CAs, or a config it built some
//! other way) rather than treat the absence of a root as "trust nothing" or, worse, feel invited
//! to build a verifier that skips validation. When at least one `CsmsRoot` PEM is installed but
//! none of it decodes into a usable certificate, [`build_root_cert_store`] returns
//! `Some(RootCertStore::empty())` rather than `None` - that *is* the fail-safe rustls already
//! gives an empty root store (a `ServerCertVerifier` that accepts nothing), and it is the honest
//! answer to "the charge point was told to trust some roots and none of them were usable", not a
//! case to be papered over. This module never builds a "trust anything" verifier under any
//! circumstances - there is no code path in it that could.
//!
//! # What happens when the CSMS deletes or replaces the root the station is connected through
//!
//! A `CsmsRoot` write (`InstallCertificate`) or removal (`DeleteCertificate`) can arrive while a
//! connection built against the *previous* trust configuration is live and working. Rebuilding
//! and applying a new `RootCertStore` immediately, unconditionally, has exactly the failure mode
//! [`crate::network_switch`] already documents for a network-profile write (A9): a CSMS mistake -
//! deleting the one root that validates its own certificate, installing a new one before its own
//! TLS endpoint is actually serving a certificate that chains to it - locks the station out of
//! the only CSMS that could push a correction, because the correction itself needs a connection
//! the broken trust configuration cannot make.
//!
//! This module therefore does not apply anything on its own; it only *builds* a `RootCertStore`
//! from the store's current contents. **The rollback behaviour lives in
//! [`crate::network_switch::ConnectionTarget`]**, extended by this task with the same shape A9
//! already gives network-profile addresses:
//! [`ConnectionTarget::stage_tls_config`](crate::network_switch::ConnectionTarget::stage_tls_config)
//! stages a freshly-built `ClientConfig` (built from this module's `RootCertStore`, plus whatever
//! client-cert config profile 3 needs - see [`crate::mutual_tls::client_config`]) without
//! replacing the configuration the *active* connection is using. The staged configuration only
//! becomes the connection's confirmed one once a redial actually succeeds under it; a redial that
//! fails `OCPPCommCtrlr`/`NetworkProfileConnectionAttempts` times in a row while it is staged
//! discards it and reverts to the configuration proven before it - the same threshold, and the
//! same "revert to what last worked, not merely what came before" shape, `ConnectionTarget`
//! already applies to a switched address. A currently-live connection is never torn down just to
//! test a new trust configuration; it is only exercised the next time the connection would have
//! redialled anyway.
//!
//! Wiring a certificate-store change through to `stage_tls_config` (typically: rebuild on every
//! `InstallCertificate`/`DeleteCertificate` targeting `CsmsRoot`, or on a slower poll) is the
//! caller's job, the same way `crate::network_switch::run_network_profile_switching` is a
//! function the integrator's own connection-setup code calls rather than something wired
//! automatically - this crate does not assume every charge point wants a live TLS trust update
//! pushed through immediately, only that the primitive to do so exists and rolls back safely.

use alloc::sync::Arc;

use ocpp_client::rustls::{ClientConfig, RootCertStore};

use crate::hardware::{CertificateStore, CertificateUse};
use crate::mutual_tls::decode_pem_certificates;

/// Builds a [`RootCertStore`] from every [`CertificateUse::CsmsRoot`] chain `certificate_store`
/// holds.
///
/// `None` when nothing at all is installed for that use - see the module docs for why that is
/// deliberately distinct from `Some(RootCertStore::empty())`, which this returns instead when at
/// least one chain is installed but none of it decoded into a usable certificate.
pub async fn build_root_cert_store<C: CertificateStore + Sync>(
    certificate_store: &C,
) -> Result<Option<RootCertStore>, C::Error> {
    let pems = certificate_store
        .all_certificate_chain_pems(CertificateUse::CsmsRoot)
        .await?;
    if pems.is_empty() {
        return Ok(None);
    }

    let mut root_store = RootCertStore::empty();
    for pem in &pems {
        let Some(certs) = decode_pem_certificates(pem) else {
            tracing::warn!("an installed CsmsRoot chain is not valid PEM; skipping it");
            continue;
        };
        for cert in certs {
            if let Err(error) = root_store.add(cert) {
                tracing::warn!(%error, "an installed CsmsRoot certificate could not be added to the trust store");
            }
        }
    }
    Ok(Some(root_store))
}

/// A server-only `ClientConfig` (no client certificate) verifying against `root_store` - security
/// profile 2's shape. Security profile 3 callers build their own via
/// [`crate::mutual_tls::client_config`], which already takes a `root_store` parameter for exactly
/// this reason (see that function's docs); this crate does not duplicate that assembly for a
/// second, client-cert-bearing config.
pub fn server_only_client_config(root_store: Arc<RootCertStore>) -> Arc<ClientConfig> {
    Arc::new(
        ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::{
        CertificateHashData, HashAlgorithm, InMemoryStorage, StoredCertificates,
    };
    use alloc::string::ToString;

    fn hash(serial: &str) -> CertificateHashData {
        CertificateHashData {
            hash_algorithm: HashAlgorithm::Sha256,
            issuer_name_hash: "aa".to_string(),
            issuer_key_hash: "bb".to_string(),
            serial_number: serial.to_string(),
        }
    }

    // Real, self-signed test-only certificates (`openssl req -x509 -newkey ec ...`) - `add`
    // parses DER through webpki, so the fake "PEM around arbitrary bytes" fixture
    // `mutual_tls`'s own tests get away with (no X.509 parsing there) will not do here.
    const ROOT_ONE_PEM: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBhDCCASugAwIBAgIUZYS6oRuFoKYt8xp+6W/RzAnJD9kwCgYIKoZIzj0EAwIw\n\
GDEWMBQGA1UEAwwNVGVzdCBSb290IE9uZTAeFw0yNjA4MDkxMjQ4NTFaFw0zNjA4\n\
MDYxMjQ4NTFaMBgxFjAUBgNVBAMMDVRlc3QgUm9vdCBPbmUwWTATBgcqhkjOPQIB\n\
BggqhkjOPQMBBwNCAASrKxTNaAv3O2HQ2XOO4VHT4OtU398Uxw0nDQkD656QI9S0\n\
B0EOeN2iI0FkIrK+J4U6EjefLtRRF/rxyBYrVO8Eo1MwUTAdBgNVHQ4EFgQUYvS0\n\
4uynDj7XfnSMrEeihPdoRAEwHwYDVR0jBBgwFoAUYvS04uynDj7XfnSMrEeihPdo\n\
RAEwDwYDVR0TAQH/BAUwAwEB/zAKBggqhkjOPQQDAgNHADBEAiBNT7tsBP8XLfgF\n\
El+xMPCNj6Q2TI7X+RxvjZLXmiq6oAIgS7UOOjKBwZ+6/NAoX18emZnd+UllSigL\n\
b9gVeVEZFbg=\n\
-----END CERTIFICATE-----\n";

    const ROOT_TWO_PEM: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBhTCCASugAwIBAgIURlp4P6u9ib379wZXqkWt62b8txswCgYIKoZIzj0EAwIw\n\
GDEWMBQGA1UEAwwNVGVzdCBSb290IFR3bzAeFw0yNjA4MDkxMjQ4NTFaFw0zNjA4\n\
MDYxMjQ4NTFaMBgxFjAUBgNVBAMMDVRlc3QgUm9vdCBUd28wWTATBgcqhkjOPQIB\n\
BggqhkjOPQMBBwNCAAR9bMy3HktCCxzO4SFUp83Ip2lmcREMVdr0VgJNWUU8+B/J\n\
i99scqXstEtrrs2fbgk7pcAyu5R00t6SD0r5z75ho1MwUTAdBgNVHQ4EFgQU8w+g\n\
l/NBxZDm6xz+0ys2uAJXCJMwHwYDVR0jBBgwFoAU8w+gl/NBxZDm6xz+0ys2uAJX\n\
CJMwDwYDVR0TAQH/BAUwAwEB/zAKBggqhkjOPQQDAgNIADBFAiEA7d96BQyC/VqE\n\
6z79IzG2tXuEN1ssSYhqC1rEgwPWmjUCICguepyAUM7Nl016wVnb471QlhDFYBew\n\
Nw9pJl5k7A/A\n\
-----END CERTIFICATE-----\n";

    #[tokio::test]
    async fn no_installed_root_at_all_is_none_not_an_empty_store() {
        let store = StoredCertificates::new(Arc::new(InMemoryStorage::new()));

        assert!(build_root_cert_store(&store).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn every_installed_root_becomes_a_trust_anchor() {
        let store = StoredCertificates::new(Arc::new(InMemoryStorage::new()));
        store
            .install_with_hash(CertificateUse::CsmsRoot, ROOT_ONE_PEM, hash("01"))
            .await;
        store
            .install_with_hash(CertificateUse::CsmsRoot, ROOT_TWO_PEM, hash("02"))
            .await;
        // A different use must not leak into the trust store.
        store
            .install_with_hash(CertificateUse::V2gRoot, ROOT_ONE_PEM, hash("03"))
            .await;

        let root_store = build_root_cert_store(&store)
            .await
            .unwrap()
            .expect("two roots were installed");

        assert_eq!(root_store.len(), 2);
    }

    #[tokio::test]
    async fn a_chain_that_is_not_valid_pem_is_skipped_rather_than_failing_the_whole_build() {
        let store = StoredCertificates::new(Arc::new(InMemoryStorage::new()));
        store
            .install_with_hash(CertificateUse::CsmsRoot, "not a certificate", hash("01"))
            .await;
        store
            .install_with_hash(CertificateUse::CsmsRoot, ROOT_ONE_PEM, hash("02"))
            .await;

        let root_store = build_root_cert_store(&store).await.unwrap().unwrap();

        assert_eq!(root_store.len(), 1);
    }

    #[tokio::test]
    async fn installed_but_entirely_unusable_roots_report_an_empty_store_not_none() {
        // The module docs' distinction: "nothing installed" (`None`, leave the caller's own
        // config alone) is not the same claim as "installed but unusable" (`Some(empty)`, the
        // fail-safe rustls already gives - reject everything - surfaced honestly).
        let store = StoredCertificates::new(Arc::new(InMemoryStorage::new()));
        store
            .install_with_hash(CertificateUse::CsmsRoot, "not a certificate", hash("01"))
            .await;

        let root_store = build_root_cert_store(&store)
            .await
            .unwrap()
            .expect("something was installed, even though it was unusable");

        assert_eq!(root_store.len(), 0);
    }

    #[test]
    fn server_only_config_carries_no_client_certificate() {
        let config = server_only_client_config(Arc::new(RootCertStore::empty()));
        assert!(!config.client_auth_cert_resolver.has_certs());
    }
}
