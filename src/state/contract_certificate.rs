//! The ISO 15118 contract certificate a vehicle presents to authorize itself (OCPP use case
//! **C07 - Authorization using Contract Certificates**, `docs/PRODUCTION-ROADMAP.md` B4.6).
//!
//! Plug & Charge authorization is an ordinary `Authorize` carrying two extra things: the eMAID as
//! the [`crate::state::IdToken`], and enough certificate material for *someone* to check that the
//! contract behind it is still valid. This crate is never that someone - it does no X.509 parsing
//! (see [`crate::hardware::CertificateHashData`]) and speaks no ISO 15118 (see
//! [`crate::iso15118`]) - so both fields here are produced by the integrator's high-level
//! communication stack and carried to the CSMS untouched.

use alloc::string::String;
use alloc::vec::Vec;

use crate::hardware::OcspCertificateId;

/// The certificate material accompanying a Plug & Charge authorization.
///
/// Built by the integrator's ISO 15118 stack from the `CertificateInstallationRes`/contract
/// certificate the vehicle presented, and passed in with
/// [`crate::state::ConnectorEvent::ContractCertificatePresented`].
///
/// # Which field the CSMS actually needs
///
/// [`Self::ocsp_data`] is the one OCPP always wants (C07.FR.02): it is what lets the CSMS run the
/// revocation check. [`Self::chain_pem`] is only needed when the charge point could not validate
/// the contract certificate itself for lack of the root - which is *always* true of this crate -
/// and is sent only when `ISO15118Ctrlr.CentralContractValidationAllowed` permits it (C07.FR.06).
/// Supplying neither is allowed and means the CSMS decides on the eMAID alone.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContractCertificate {
    /// The contract certificate chain in PEM format, leaf first, excluding the root - OCPP's
    /// `AuthorizeRequest.certificate`.
    ///
    /// Carried opaquely: never parsed, never inspected, never truncated. Its length bound (10000
    /// characters on 2.1, 5500 on 2.0.1) belongs to `ocpp-types`, which enforces it when the
    /// request is validated on the way out - this crate does not re-implement the check, per
    /// `docs/UPSTREAM-POLICY.md`.
    pub chain_pem: Option<String>,
    /// OCSP request data for the contract certificate and each CA in its chain - OCPP's
    /// `AuthorizeRequest.iso15118CertificateHashData`.
    ///
    /// The same [`OcspCertificateId`] a `GetCertificateStatus` carries, reused rather than
    /// redefined: it is the identical OCPP `OCSPRequestData` type, and the hashes come from
    /// whoever parsed the certificate either way. OCPP bounds this at 4 entries.
    pub ocsp_data: Vec<OcspCertificateId>,
}

/// What the CSMS said about the contract certificate itself, distinct from what it said about the
/// eMAID - OCPP's `AuthorizeCertificateStatusEnum` (`AuthorizeResponse.certificateStatus`).
///
/// Per C07.FR.13-17 the two answers travel together and agree: a revoked certificate comes back
/// with `CertificateRevoked` *and* an `Invalid` id token status, an expired one with
/// `CertificateExpired` *and* `Expired`. This crate still checks both rather than trusting that
/// pairing - see [`crate::authorization::ContractAuthorization::accepted`] for why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContractCertificateStatus {
    /// The certificate and its chain check out. Charging may still be refused on the eMAID -
    /// C07.FR.14's note names `ConcurrentTx` and `NotAtThisLocation` as the obvious cases.
    Accepted,
    /// A signature in the chain did not verify.
    SignatureError,
    /// The certificate (or one in its chain) is past its validity period.
    CertificateExpired,
    /// The certificate has been revoked.
    CertificateRevoked,
    /// The CSMS had no certificate to check - neither the chain nor the hash data reached it.
    NoCertificateAvailable,
    /// The chain could not be verified: missing intermediates, an untrusted root, or otherwise
    /// unusable.
    CertChainError,
    /// The certificate is valid, but the contract behind it has been cancelled (C07.FR.13).
    ContractCancelled,
}

impl ContractCertificateStatus {
    /// This status's OCPP `AuthorizeCertificateStatusEnum` name, for the low-cardinality log
    /// field `CLAUDE.md` asks for rather than a `{:?}` of the enum.
    ///
    /// Exhaustive with no wildcard arm on purpose: a new status must be a compile error here.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Accepted => "Accepted",
            Self::SignatureError => "SignatureError",
            Self::CertificateExpired => "CertificateExpired",
            Self::CertificateRevoked => "CertificateRevoked",
            Self::NoCertificateAvailable => "NoCertificateAvailable",
            Self::CertChainError => "CertChainError",
            Self::ContractCancelled => "ContractCancelled",
        }
    }
}
