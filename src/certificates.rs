//! Certificate management messages: `InstallCertificate`, `DeleteCertificate`,
//! `GetInstalledCertificateIds` (`docs/ROADMAP.md` §1/§13, `docs/PRODUCTION-ROADMAP.md` B4.2), and
//! the `SignCertificate`/`CertificateSigned` CSR round trip (B4.3).
//!
//! **1.6J too, since D2.2.** 1.6J has no certificate messages in its core specification - they
//! arrive with the Security Whitepaper - but the pinned `ocpp-client` 0.5.0 generates all five
//! (verified against `ocpp_1_6::actions`), so `ocpp_1_6` below wires them. 1.6J's shapes are
//! narrower than 2.x's throughout: `CertificateType` names only two roots
//! (`CentralSystemRootCertificate`/`ManufacturerRootCertificate`, no V2G/MO/OEM roots or the
//! charge point's own chain), `GetInstalledCertificateIds` takes exactly one type rather than an
//! optional list, and `SignCertificate`/`CertificateSigned` carry no `certificateType` at all - see
//! `ocpp_1_6`'s module docs for what each of those narrowings means for the projection.
//!
//! Every handler is a thin decision over [`crate::hardware::CertificateStore`], which is where the
//! actual work lives: parsing X.509, computing hashes and holding keys all belong to the
//! integrator, and possibly to a secure element (see that trait's docs). What this module adds is
//! the OCPP-shaped part - the status a refusal takes, the absent-means-all filter, and the
//! capability gate.
//!
//! # A store that refuses everything is a valid store
//!
//! [`crate::hardware::StoredCertificates`] - the `Storage`-backed default - refuses
//! `InstallCertificate`, because it cannot compute the hash a CSMS would later address the
//! certificate by, and one it cannot address is one it cannot delete. That is a real limitation of
//! *that implementation*, not of this block: an integrator who can parse X.509 implements the
//! trait (or calls `install_with_hash`) and installation works. The refusal reaches the CSMS as
//! `Rejected`, which is what OCPP has for "this charge point will not take that certificate".
//!
//! # B4.3: the CSR round trip
//!
//! `SignCertificate` is **charge-point-initiated**: this crate builds a PKCS#10 CSR (see
//! [`csr`]) over a keypair from [`crate::hardware::KeyStore`] and sends it. `CertificateSigned`
//! is the CSMS's answer, arriving later as its own message (not a direct reply) - possibly much
//! later, and possibly never if the CSMS rejects the request outright. [`PendingSignRequests`]
//! is the (deliberately tiny - two slots, one per [`CertificateSigningPurpose`]) bit of state
//! that lets [`handle_certificate_signed`] tell a solicited `CertificateSigned` from an
//! unsolicited one, and route it to the right [`CertificateUse`] slot in the store.
//!
//! Renewal ([F3.2](#)) and mutual TLS ([F1.3](#)) both build on this round trip rather than
//! reimplementing it - see [`build_signed_csr`]'s docs for the one thing they still have to add
//! themselves (remembering which [`crate::hardware::KeyHandle`] a stored certificate pairs with,
//! so a renewal CSR can be signed with the same key).

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::Cell;
use core::cell::RefCell;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

use crate::actor::ChargePointActor;
use crate::hardware::{
    CertificateHashData, CertificateStore, CertificateUse, DeleteCertificateOutcome,
    InstallCertificateOutcome, InstalledCertificate,
};
use crate::state::{SecurityEvent, SecurityEventType};

pub mod csr;
pub use csr::{CsrBuildError, CsrSubject, build_signed_csr};

/// What a `SignCertificate`/`CertificateSigned` round trip is for - this crate's
/// protocol-version-independent view of OCPP's `CertificateSigningUseEnum`/`certificateType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificateSigningPurpose {
    /// The charge point's own client certificate, used e.g. for security profile 3 mutual TLS.
    ChargingStationCertificate,
    /// The charge point's ISO 15118 V2G certificate.
    V2GCertificate,
}

impl CertificateSigningPurpose {
    /// The [`CertificateUse`] slot a signed certificate of this purpose is installed into.
    pub fn certificate_use(self) -> CertificateUse {
        match self {
            Self::ChargingStationCertificate => CertificateUse::ChargingStation,
            Self::V2GCertificate => CertificateUse::V2gCertificateChain,
        }
    }

    fn slot_index(self) -> usize {
        match self {
            Self::ChargingStationCertificate => 0,
            Self::V2GCertificate => 1,
        }
    }
}

/// The outcome of asking the CSMS to sign a CSR (`SignCertificateResponse.status` /
/// `GenericStatusEnum`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignCertificateOutcome {
    /// The CSMS accepted the request and (eventually) sends a `CertificateSigned`.
    Accepted,
    /// The CSMS refused the request outright; no `CertificateSigned` should be expected for it.
    Rejected,
}

/// The outcome this charge point reports back for a CSMS-initiated `CertificateSigned`
/// (`CertificateSignedResponse.status` / `CertificateSignedStatusEnum`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificateSignedOutcome {
    /// The chain was installed.
    Accepted,
    /// The chain was refused: unsolicited, mismatched, or the store could not use it.
    Rejected,
}

/// One outstanding `SignCertificate` request this charge point is waiting on a
/// `CertificateSigned` for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingSignRequest {
    /// 2.1's `requestId`, when the connection negotiated 2.1. `None` on 2.0.1 (which has no such
    /// field) or when a 2.1 CSMS omits it.
    request_id: Option<i64>,
}

/// Tracks outstanding `SignCertificate` requests, so [`handle_certificate_signed`] can tell a
/// solicited `CertificateSigned` from an unsolicited one and correlate it to the right
/// [`CertificateSigningPurpose`].
///
/// Fixed at exactly two slots - one per [`CertificateSigningPurpose`] - rather than a general
/// collection: a charge point only ever has one outstanding CSR per certificate purpose at a
/// time (a second `SignCertificate` for the same purpose simply supersedes the first, per
/// [`record_sent`](Self::record_sent)), so this is bounded by construction (CLAUDE.md's "bounded
/// memory" rule) rather than needing a runtime limit.
pub struct PendingSignRequests {
    slots: BlockingMutex<CriticalSectionRawMutex, RefCell<[Option<PendingSignRequest>; 2]>>,
    // A `BlockingMutex<Cell<i64>>` rather than an `AtomicI64`, deliberately: `AtomicI64` does not
    // exist on 32-bit targets without native 64-bit atomics, and `thumbv7em-none-eabihf` - the MCU
    // target CI builds for - is one of them. The wire field is `Option<i64>` (`ocpp-types` 0.3.0),
    // so narrowing this to `AtomicI32` would silently change the range this crate can represent.
    // `CriticalSectionRawMutex` is the same primitive the rest of the crate uses for shared
    // interior mutability under `no_std` (see `ChargePointActor::boot_reason_recorder`).
    next_request_id: BlockingMutex<CriticalSectionRawMutex, Cell<i64>>,
}

impl PendingSignRequests {
    /// No outstanding requests.
    pub fn new() -> Self {
        Self {
            slots: BlockingMutex::new(RefCell::new([None, None])),
            next_request_id: BlockingMutex::new(Cell::new(1)),
        }
    }

    /// The next 2.1 `requestId` to use for an outgoing `SignCertificate` - monotonically
    /// increasing, so two CSRs sent close together (e.g. one per [`CertificateSigningPurpose`])
    /// never collide. 2.0.1 has no such field and never calls this.
    pub fn next_request_id(&self) -> i64 {
        self.next_request_id.lock(|id| {
            let current = id.get();
            id.set(current.wrapping_add(1));
            current
        })
    }

    /// Records that a `SignCertificate` for `purpose` was accepted by the CSMS, so a later
    /// `CertificateSigned` for the same purpose is recognised as solicited.
    ///
    /// Superseding: a second call for the same `purpose` (e.g. a renewal fired before the first
    /// CSR was answered) replaces the first pending entry rather than stacking - only the most
    /// recent CSR for a given purpose can plausibly still be answered by the CSMS.
    pub fn record_sent(&self, purpose: CertificateSigningPurpose, request_id: Option<i64>) {
        self.slots.lock(|slots| {
            slots.borrow_mut()[purpose.slot_index()] = Some(PendingSignRequest { request_id });
        });
    }

    /// Whether a pending request for `purpose` matches `request_id`, per
    /// [`handle_certificate_signed`]'s correlation rule.
    fn matches(&self, purpose: CertificateSigningPurpose, request_id: Option<i64>) -> bool {
        self.slots.lock(|slots| {
            match &slots.borrow()[purpose.slot_index()] {
                None => false,
                // Correlate on `requestId` only when both sides supplied one (2.1); a
                // 2.0.1 connection - or a 2.1 CSMS that omits it - has nothing more specific
                // than `purpose` itself to go on, and `purpose` is already required to match by
                // construction (each slot only ever holds one purpose's pending request).
                Some(pending) => match (pending.request_id, request_id) {
                    (Some(a), Some(b)) => a == b,
                    _ => true,
                },
            }
        })
    }

    /// Clears any pending request for `purpose`, regardless of whether it matched.
    fn clear(&self, purpose: CertificateSigningPurpose) {
        self.slots
            .lock(|slots| slots.borrow_mut()[purpose.slot_index()] = None);
    }
}

impl Default for PendingSignRequests {
    fn default() -> Self {
        Self::new()
    }
}

/// Records the outcome of a `SignCertificate` this charge point just sent, updating `pending`
/// accordingly. Call after the CSMS's `SignCertificateResponse` comes back.
pub fn record_sign_certificate_sent(
    pending: &PendingSignRequests,
    purpose: CertificateSigningPurpose,
    request_id: Option<i64>,
    outcome: SignCertificateOutcome,
) {
    match outcome {
        SignCertificateOutcome::Accepted => pending.record_sent(purpose, request_id),
        SignCertificateOutcome::Rejected => {
            tracing::warn!(?purpose, "CSMS rejected our certificate signing request");
        }
    }
}

/// Handles a CSMS-initiated `CertificateSigned`.
///
/// Refuses (without touching the store) when the capability is absent, or when nothing in
/// `pending` matches `purpose`/`request_id` - an unsolicited `CertificateSigned` is exactly the
/// scenario B4.3 asks this crate to guard against, since installing one blind would let a CSMS
/// push a certificate for a key this charge point never asked to have signed. A rejection that
/// *did* correlate to a pending request (a chain the store could not use) raises
/// [`SecurityEventType::InvalidChargingStationCertificate`] - the closest standardized event to
/// "our own certificate, and it's unusable" (OCPP's security-events list has no
/// V2G-certificate-specific variant, so both purposes report through this one; see
/// `state::security_event`).
pub async fn handle_certificate_signed<S: CertificateStore>(
    actor: &ChargePointActor,
    store: &S,
    pending: &PendingSignRequests,
    purpose: CertificateSigningPurpose,
    request_id: Option<i64>,
    certificate_chain: &str,
) -> CertificateSignedOutcome {
    if !crate::refusal::capability_present(&actor.state().capabilities, "CertificateSigned") {
        return CertificateSignedOutcome::Rejected;
    }

    if !pending.matches(purpose, request_id) {
        tracing::warn!(
            ?purpose,
            ?request_id,
            "received a CertificateSigned that does not match any outstanding SignCertificate"
        );
        crate::security::report_security_event(
            actor,
            SecurityEvent {
                event_type: SecurityEventType::InvalidChargingStationCertificate,
                tech_info: Some("unsolicited CertificateSigned".into()),
            },
        )
        .await;
        return CertificateSignedOutcome::Rejected;
    }
    pending.clear(purpose);

    match store
        .install(purpose.certificate_use(), certificate_chain)
        .await
    {
        Ok(InstallCertificateOutcome::Accepted) => CertificateSignedOutcome::Accepted,
        Ok(_) => {
            tracing::warn!(?purpose, "the certificate store refused the signed chain");
            crate::security::report_security_event(
                actor,
                SecurityEvent {
                    event_type: SecurityEventType::InvalidChargingStationCertificate,
                    tech_info: Some("the certificate store refused the signed chain".into()),
                },
            )
            .await;
            CertificateSignedOutcome::Rejected
        }
        Err(err) => {
            tracing::warn!(error = %err, "could not install the signed certificate chain");
            crate::security::report_security_event(
                actor,
                SecurityEvent {
                    event_type: SecurityEventType::InvalidChargingStationCertificate,
                    tech_info: Some("the certificate store failed to install the chain".into()),
                },
            )
            .await;
            CertificateSignedOutcome::Rejected
        }
    }
}

/// Handles a CSMS-initiated `InstallCertificate`.
///
/// Refuses before touching the store when the capability is absent (C5) or the use is one OCPP
/// does not allow to be installed - the charge point's own certificates, which arrive by
/// `CertificateSigned` in answer to a CSR rather than by this message.
pub async fn handle_install_certificate<S: CertificateStore>(
    actor: &ChargePointActor,
    store: &S,
    use_for: CertificateUse,
    certificate: &str,
) -> InstallCertificateOutcome {
    if !crate::refusal::capability_present(&actor.state().capabilities, "InstallCertificate") {
        return InstallCertificateOutcome::Rejected;
    }
    if !use_for.is_installable() {
        tracing::warn!(
            ?use_for,
            "refusing to install a certificate this charge point issues rather than receives"
        );
        return InstallCertificateOutcome::Rejected;
    }
    match store.install(use_for, certificate).await {
        Ok(outcome) => outcome,
        Err(err) => {
            // Degraded rather than propagated, per `CLAUDE.md`: a store that cannot be written is
            // a charge point that keeps running with the certificates it already has, not one
            // that falls over.
            tracing::warn!(error = %err, "the certificate store refused an install");
            InstallCertificateOutcome::Failed
        }
    }
}

/// Handles a CSMS-initiated `DeleteCertificate`.
///
/// An absent capability answers `NotFound` rather than `Failed`: a charge point with no
/// certificate store genuinely does not have the certificate, and `Failed` would send an operator
/// looking for a fault.
pub async fn handle_delete_certificate<S: CertificateStore>(
    actor: &ChargePointActor,
    store: &S,
    hash_data: &CertificateHashData,
) -> DeleteCertificateOutcome {
    if !crate::refusal::capability_present(&actor.state().capabilities, "DeleteCertificate") {
        return DeleteCertificateOutcome::NotFound;
    }
    match store.delete(hash_data).await {
        Ok(outcome) => outcome,
        Err(err) => {
            tracing::warn!(error = %err, "the certificate store refused a delete");
            DeleteCertificateOutcome::Failed
        }
    }
}

/// Handles a CSMS-initiated `GetInstalledCertificateIds`, returning what matches.
///
/// An empty `uses` means every use - OCPP's absent-means-all rule, the same one
/// `GetChargingProfiles` follows. An empty *result* is reported by the caller as `NotFound`, which
/// is OCPP's way of saying "none installed" rather than an error.
pub async fn handle_get_installed_certificate_ids<S: CertificateStore>(
    actor: &ChargePointActor,
    store: &S,
    uses: &[CertificateUse],
) -> Vec<InstalledCertificate> {
    if !crate::refusal::capability_present(
        &actor.state().capabilities,
        "GetInstalledCertificateIds",
    ) {
        return Vec::new();
    }
    match store.installed(uses).await {
        Ok(installed) => installed,
        Err(err) => {
            tracing::warn!(error = %err, "could not list installed certificates");
            Vec::new()
        }
    }
}

/// Registers this charge point's inbound certificate-management handling.
///
/// One trait for all three messages, unlike most blocks here: they share a store, and a CSMS
/// client that can answer one can answer all of them.
#[async_trait::async_trait]
pub trait CertificateHandler {
    /// Registers `InstallCertificate`, `DeleteCertificate`, `GetInstalledCertificateIds` and
    /// `CertificateSigned` handlers dispatching against `actor` and `store`.
    ///
    /// `CertificateSigned` shares this registration (rather than getting its own trait method)
    /// because it needs the same `store` the other three do, and because the
    /// [`PendingSignRequests`] it correlates against must be the same instance
    /// [`CertificateSigningRequester::sign_certificate`] populates - which only holds if both are
    /// set up together, on the version adapter's one long-lived handler instance.
    async fn register_certificate_handlers<S>(&self, actor: ChargePointActor, store: S)
    where
        S: CertificateStore + Send + Sync + 'static;
}

/// Failure asking the CSMS to sign a CSR (B4.3): either building/signing the CSR itself failed
/// (`K`, the [`crate::hardware::KeyStore`]'s error type), or sending it did (`C`, the protocol
/// adapter's `ClientError<...>`).
#[derive(Debug)]
pub enum SignCertificateError<K, C> {
    /// Building or signing the CSR itself failed - see [`CsrBuildError`].
    Csr(CsrBuildError<K>),
    /// Sending the `SignCertificate` request failed at the transport/protocol level.
    Client(C),
    /// The requested [`CertificateSigningPurpose`] has no representation on this connection's
    /// wire protocol - added for D2.2's 1.6J adapter, whose `SignCertificateRequest` carries no
    /// `certificateType` field at all (unlike 2.x's), so it can only ever mean
    /// [`CertificateSigningPurpose::ChargingStationCertificate`]. Returned before anything is
    /// sent, so a caller asking a 1.6J connection to sign a V2G certificate gets a clear local
    /// error rather than a CSR silently sent and misinterpreted by the CSMS as the wrong purpose.
    UnsupportedPurpose,
}

impl<K: core::fmt::Display, C: core::fmt::Display> core::fmt::Display
    for SignCertificateError<K, C>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Csr(error) => write!(f, "{error}"),
            Self::Client(error) => write!(f, "sending SignCertificate failed: {error}"),
            Self::UnsupportedPurpose => {
                write!(
                    f,
                    "this connection's SignCertificate cannot express that purpose"
                )
            }
        }
    }
}

impl<K: core::fmt::Debug + core::fmt::Display, C: core::fmt::Debug + core::fmt::Display>
    core::error::Error for SignCertificateError<K, C>
{
}

/// Asks the CSMS to sign a CSR: builds it (see [`build_signed_csr`]) and sends `SignCertificate`.
///
/// Generic over `ClientErr` (each protocol adapter's `ClientError<...>`), unlike
/// [`CertificateHandler`], since the trait cannot itself name a wire-protocol-specific error
/// type. A separate trait from [`CertificateHandler`] altogether, and deliberately **not** given
/// a bare-client `std` convenience impl the way [`CertificateHandler`] is: unlike the three
/// inbound messages (which need nothing beyond a store, so a convenience impl can construct a
/// throwaway handler per call), this method's [`PendingSignRequests`] must be the *same
/// instance* a `CertificateSigned` handler was registered against, or nothing would ever match -
/// construct one handler instance and use it for both.
#[async_trait::async_trait]
pub trait CertificateSigningRequester<ClientErr> {
    /// Builds a CSR for `purpose` over `key` through `key_store` and sends it, recording it as
    /// pending on [`SignCertificateOutcome::Accepted`] so a later `CertificateSigned` for the
    /// same purpose is recognised by [`handle_certificate_signed`].
    async fn sign_certificate<K>(
        &self,
        key_store: &K,
        key: &crate::hardware::GeneratedKeyPair,
        subject: &CsrSubject,
        purpose: CertificateSigningPurpose,
    ) -> Result<SignCertificateOutcome, SignCertificateError<K::Error, ClientErr>>
    where
        K: crate::hardware::KeyStore + Send + Sync;
}

#[cfg(feature = "ocpp_1_6")]
mod ocpp_1_6;
#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1;
#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1;

#[cfg(test)]
mod tests;
