//! Checking a certificate's OCSP revocation status (`docs/PRODUCTION-ROADMAP.md` B4.4):
//! `GetCertificateStatus` (2.0.1/2.1) and `GetCertificateChainStatus` (2.1) both ask this charge
//! point to establish whether a certificate is still valid. See [`crate::certificate_status`] for
//! the OCPP-facing side this trait feeds.
//!
//! # Why this is a hardware trait and not something this crate implements
//!
//! Checking OCSP status means two things this crate deliberately does not do:
//!
//! - **Talking to a third party over HTTP.** The OCSP responder named by
//!   [`OcspCertificateId::responder_url`] is neither the CSMS connection this crate owns nor
//!   anything [`crate::hardware::FileTransfer`] covers - it is an unrelated network peer this
//!   crate has never had a reason to reach before. [`crate::hardware::FileTransfer`]'s module
//!   docs make the same call for firmware/log transfer; this is the identical argument applied to
//!   OCSP.
//! - **Parsing and verifying the response.** An OCSP response is a DER-encoded ASN.1 structure
//!   (RFC 6960) that must be signature-verified against the responder's own certificate before
//!   its verdict can be trusted at all. This crate carries no crypto dependency and does no X.509
//!   parsing anywhere - [`crate::hardware::CertificateStore`]'s module docs give the same reason
//!   [`crate::hardware::StoredCertificates`] cannot compute a certificate's own hashes. Trusting
//!   an unverified response would be worse than not checking at all.
//!
//! So, like [`crate::hardware::FileTransfer`], this is a small trait: the integrator performs the
//! network round trip and the cryptographic verification, and this crate turns the resulting
//! verdict into the OCPP-shaped answer.
//!
//! # The honest default
//!
//! [`NoOcspChecker`] never claims a certificate is good because it could not check - every
//! request answers [`OcspVerdict::Unknown`] with no raw response, the same "cannot check"
//! reporting [`crate::hardware::NoCertificateStore`] gives for certificate installation.

use alloc::boxed::Box;
use alloc::string::String;
use chrono::{DateTime, Utc};

use crate::hardware::CertificateHashData;

/// One certificate to check, and where to check it - OCPP's `OCSPRequestData`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcspCertificateId {
    /// Identifies the certificate, the same way [`crate::hardware::CertificateStore`] does.
    pub hash_data: CertificateHashData,
    /// The OCSP responder to ask. Supplied by the CSMS on every request (`GetCertificateStatus`'s
    /// `OCSPRequestData.responderURL`, `GetCertificateChainStatus`'s
    /// `CertificateStatusRequestInfo.urls`) - this crate needs no OCSP configuration of its own to
    /// reach one.
    pub responder_url: String,
}

/// What an OCSP responder said about a certificate, once verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcspVerdict {
    /// Not revoked, as far as the responder is concerned.
    Good,
    /// Revoked.
    Revoked,
    /// The responder answered indeterminately (RFC 6960's `unknown`), or answered but the
    /// response could not be verified. Deliberately not distinguished further: both are "this
    /// charge point cannot vouch for the certificate", which is the only fact anything downstream
    /// acts on. Distinct from an `Err` from [`OcspChecker::check`] itself, which means the attempt
    /// to reach the responder failed rather than that it answered inconclusively - see
    /// [`crate::certificate_status`] for why the two are still reported the same way on the wire.
    Unknown,
}

/// The result of a completed OCSP check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcspCheckResult {
    /// What the responder said.
    pub verdict: OcspVerdict,
    /// The raw DER-encoded `OCSPResponse` (RFC 6960), base64-encoded. `GetCertificateStatus`'s
    /// `ocspResult` is a forwarded copy of exactly this, so the CSMS can verify it independently.
    /// `None` when the integrator cannot or does not retain the raw bytes (e.g. a checker backed
    /// by a library that only surfaces a parsed verdict) - `GetCertificateStatus` reports
    /// `Failed` in that case rather than fabricating a body it does not have, per
    /// `GetCertificateStatusResponse.ocspResult`'s "MAY only be omitted when status is not
    /// Accepted".
    pub raw_response: Option<String>,
    /// When this verdict should be considered stale, if the OCSP response carried a
    /// `nextUpdate`. Consumed only by `GetCertificateChainStatus`'s mandatory per-certificate
    /// `nextUpdate` field; `GetCertificateStatus` has no equivalent field and ignores this.
    pub next_update: Option<DateTime<Utc>>,
}

/// Checks certificates against an OCSP responder - the integration point B4.4 asks for.
///
/// Implemented by the integrator: making the HTTP request and verifying the response signature
/// both need capabilities this crate does not carry (see the module docs). [`NoOcspChecker`] is
/// the honest default for a charge point with no OCSP capability.
///
/// # Error handling
///
/// Every check is fallible, per `CLAUDE.md`: the network can be down, the responder can time out.
/// An `Err` means the attempt itself failed; a responder that was reached and answered, but
/// inconclusively, is [`OcspVerdict::Unknown`] inside an `Ok`, not an error.
#[async_trait::async_trait]
pub trait OcspChecker {
    /// The error type returned by a failed check attempt.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Checks `certificate` against its named responder.
    async fn check(&self, certificate: &OcspCertificateId) -> Result<OcspCheckResult, Self::Error>;
}

#[async_trait::async_trait]
impl<T: OcspChecker + Send + Sync + ?Sized> OcspChecker for alloc::sync::Arc<T> {
    type Error = T::Error;

    async fn check(&self, certificate: &OcspCertificateId) -> Result<OcspCheckResult, Self::Error> {
        (**self).check(certificate).await
    }
}

/// An [`OcspChecker`] for charge points with no OCSP checking capability at all.
///
/// Answers every check [`OcspVerdict::Unknown`] with no raw response - never `Good`, which would
/// have the CSMS believe a certificate was actually verified when nothing was.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOcspChecker;

/// The error type of [`NoOcspChecker`], which never actually fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoOcspCheckerError;

impl core::fmt::Display for NoOcspCheckerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("this charge point has no OCSP checking capability")
    }
}

impl core::error::Error for NoOcspCheckerError {}

#[async_trait::async_trait]
impl OcspChecker for NoOcspChecker {
    type Error = NoOcspCheckerError;

    async fn check(
        &self,
        _certificate: &OcspCertificateId,
    ) -> Result<OcspCheckResult, Self::Error> {
        Ok(OcspCheckResult {
            verdict: OcspVerdict::Unknown,
            raw_response: None,
            next_update: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn certificate() -> OcspCertificateId {
        OcspCertificateId {
            hash_data: CertificateHashData {
                hash_algorithm: crate::hardware::HashAlgorithm::Sha256,
                issuer_name_hash: "aa".into(),
                issuer_key_hash: "bb".into(),
                serial_number: "01".into(),
            },
            responder_url: "http://ocsp.example.com".into(),
        }
    }

    #[tokio::test]
    async fn a_charge_point_with_no_checker_reports_unknown_rather_than_claiming_good() {
        let result = NoOcspChecker.check(&certificate()).await.unwrap();

        assert_eq!(result.verdict, OcspVerdict::Unknown);
        assert!(result.raw_response.is_none());
        assert!(result.next_update.is_none());
    }
}
