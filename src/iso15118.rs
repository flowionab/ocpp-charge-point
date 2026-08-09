//! `Get15118EVCertificate` (`docs/PRODUCTION-ROADMAP.md` B4.5): ISO 15118 Plug & Charge's
//! certificate installation/renewal round trip.
//!
//! **2.x only** - 1.6J predates ISO 15118 Plug & Charge entirely. **Charge-point-initiated**,
//! like `SignCertificate` (B4.3, see [`crate::certificates`]): the vehicle presents a
//! `CertificateInstallationReq` over its ISO 15118 high-level communication (HLC) link, this
//! crate's integrator-supplied HLC stack hands the raw request to
//! [`Iso15118CertificateRequester`]'s `request_ev_certificate`, and this crate forwards it to the
//! CSMS and returns what came back so it can be relayed to the vehicle. There is no inbound
//! `Get15118EVCertificate` handler to register - the CSMS never sends this message to a charge
//! point - so, like `SignCertificate`, this block has no row in `crate::refusal`'s decision table
//! and no `ChargePointBuilder` registration: see that module's note next to its table, and this
//! module's own note below.
//!
//! # What this crate does and does not do
//!
//! This crate carries the EXI-encoded request/response bodies opaquely - Base64 text in, Base64
//! text out, never decoded - the same stance [`crate::hardware::CertificateHashData`] takes for
//! certificate hashes. It does not implement an EXI codec, does not speak PLC/SLAC to a vehicle,
//! and does not run an ISO 15118 session state machine (deciding when to ask for a renewal, how
//! many contracts to request, or what to do with a `remainingContracts` count is the HLC stack's
//! job, not this crate's). **An integrator with a 15118-capable HLC controller supplies one**
//! ([`crate::hardware::Iso15118Controller`]) and calls `request_ev_certificate` whenever their
//! stack has a request to relay; a charge point with no such controller compiles
//! [`crate::hardware::NoIso15118Controller`] in instead and never calls it at all - see that
//! trait's module docs for the full boundary.
//!
//! # Relationship to `V2GCertificate` (B4.3)
//!
//! [`crate::certificates::CertificateSigningPurpose::V2GCertificate`] is a *different*
//! certificate: it is this charge point's own certificate (obtained via `SignCertificate`,
//! stored in [`crate::hardware::CertificateUse::V2gCertificateChain`]), which the charge point
//! presents to authenticate *itself* to the vehicle during the ISO 15118 TLS handshake that
//! precedes `CertificateInstallationReq` ever being sent. `Get15118EVCertificate` is the
//! opposite direction and a different party's certificate entirely: it is how the *vehicle*
//! obtains or renews its own contract certificate, brokered through the CSMS rather than issued
//! by this charge point. The two features share nothing at the OCPP layer beyond both existing
//! under the "ISO 15118 Plug & Charge" umbrella; a charge point can have one without the other
//! (e.g. a station with a V2G certificate for TLS but whose CSMS does not broker contract
//! certificates, or vice versa).
//!
//! # Capability gating
//!
//! Gated by [`crate::hardware::Iso15118SupportLevel`]
//! ([`crate::hardware::Capabilities::iso15118_support`]), not a separate bool: a charge point
//! that reports [`Iso15118SupportLevel::None`](crate::hardware::Iso15118SupportLevel::None) never
//! sends `Get15118EVCertificate` at all - `request_ev_certificate` answers
//! [`Iso15118CertificateStatus::Failed`](crate::hardware::Iso15118CertificateStatus::Failed)
//! locally, the same "simply does not attempt" refusal `crate::refusal`'s module docs describe
//! for `SignCertificate`, and for the identical reason: there is no inbound CALLRESULT to shape a
//! refusal into, because this charge point is the one placing the call.
//!
//! The specific level (`Iso15118_2` vs `Iso15118_20`) does not change whether this crate sends the
//! message - both support levels use `Get15118EVCertificate`, 2.1 having extended the same action
//! to also cover ISO 15118-20's contract-chain fields. It does change what the *caller* should
//! populate in [`Iso15118CertificateRequest`]: `maximum_contract_certificate_chains` and
//! `prioritized_emaids` are meaningful only for an `Iso15118_20` session (OCPP 2.1's field
//! description: "Absent during ISO 15118-2 session"), so a caller on an `Iso15118_2` session
//! should leave them `None` - this crate does not second-guess that choice, it only has no 2.0.1
//! wire representation to put them in even if they were supplied.

use alloc::boxed::Box;

use crate::hardware::{
    Iso15118CertificateRequest, Iso15118CertificateResult, Iso15118CertificateStatus,
    Iso15118Controller, Iso15118SupportLevel,
};

/// Whether this charge point should attempt `Get15118EVCertificate` at all, per the module docs'
/// capability gating.
fn iso15118_supported(capabilities: &crate::hardware::Capabilities) -> bool {
    capabilities.iso15118_support != Iso15118SupportLevel::None
}

/// The result of refusing a `Get15118EVCertificate` locally: `Failed`, with no EXI payload and no
/// contract count, because no CSMS was ever asked.
fn refused_locally() -> Iso15118CertificateResult {
    Iso15118CertificateResult {
        status: Iso15118CertificateStatus::Failed,
        exi_response: alloc::string::String::new(),
        remaining_contracts: None,
    }
}

/// Delivers `result` to `controller`, logging (rather than propagating) a failure: the CSMS
/// round trip already completed by the time this runs, so a local delivery problem is reported
/// but does not turn a successful OCPP exchange into an error.
async fn deliver<C: Iso15118Controller>(controller: &C, result: &Iso15118CertificateResult) {
    if let Err(err) = controller.deliver_certificate_response(result).await {
        tracing::warn!(
            error = %err,
            "could not deliver the Get15118EVCertificate result to the ISO 15118 controller"
        );
    }
}

/// Sends a `Get15118EVCertificate` request to the CSMS on behalf of an ISO 15118 HLC session, and
/// relays the answer to `controller`.
///
/// Implemented per protocol version directly on `ocpp-client`'s bare client types (there is no
/// cross-message state to correlate, unlike `SignCertificate`/`CertificateSigned` - this message
/// is a plain request/response, so no wrapper handler type is needed). Deliberately **not**
/// registered through [`crate::builder::ChargePointBuilder`]: like
/// [`crate::certificates::CertificateSigningRequester`], this is charge-point-initiated with
/// nothing for the builder to register against `setup()`'s CSMS - the integrator's HLC stack
/// calls this directly, supplying its own [`Iso15118Controller`] per call exactly as
/// `sign_certificate` takes its `KeyStore` per call.
#[async_trait::async_trait]
pub trait Iso15118CertificateRequester<ClientErr> {
    /// Forwards `request` to the CSMS if [`crate::hardware::Iso15118SupportLevel`] allows it,
    /// then hands the result to `controller` for delivery back to the vehicle - see the module
    /// docs for why a locally-refused request still resolves to a value rather than an error.
    async fn request_ev_certificate<C>(
        &self,
        actor: &crate::actor::ChargePointActor,
        controller: &C,
        request: Iso15118CertificateRequest,
    ) -> Result<Iso15118CertificateResult, ClientErr>
    where
        C: Iso15118Controller + Send + Sync;
}

#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1;
#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1;

#[cfg(test)]
mod tests;
