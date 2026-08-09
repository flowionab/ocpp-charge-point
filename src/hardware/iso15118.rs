//! ISO 15118 Plug & Charge: the boundary between OCPP's `Get15118EVCertificate` (2.0.1/2.1,
//! `docs/PRODUCTION-ROADMAP.md` B4.5) and the actual ISO 15118 high-level communication (HLC)
//! stack that talks PLC/SLAC to the vehicle. See [`crate::iso15118`] for the OCPP-facing side
//! this trait feeds.
//!
//! # Why this is a hardware trait and not something this crate implements
//!
//! `Get15118EVCertificate` carries an EXI-encoded (ISO 15118's binary wire format) request from
//! the vehicle and, on success, an EXI-encoded response the vehicle needs back to complete its own
//! side of the exchange. Both getting the request in the first place and delivering the response
//! back mean speaking ISO 15118 over the vehicle's physical link (PLC via HomePlug GreenPHY, or
//! SLAC for signal-level attenuation characterization) - a completely different protocol stack and
//! physical layer than the OCPP-over-WebSocket connection this crate owns. This crate carries no
//! EXI codec and no 15118 state machine, the same "carried opaquely, never parsed" stance
//! [`crate::hardware::CertificateHashData`] documents for certificate hashes, applied here to a
//! whole message body instead of one field.
//!
//! So, like [`crate::hardware::OcspChecker`] for OCSP's HTTP round trip, this is a small trait:
//! the integrator's HLC controller owns the vehicle-facing half of the exchange, and this crate
//! turns the CSMS's answer into the OCPP-shaped result before handing it back.
//!
//! # The honest default
//!
//! [`NoIso15118Controller`] never claims to have delivered a response it had nowhere to send -
//! every delivery attempt answers `Err`, the same "cannot do this" reporting
//! [`crate::hardware::NoOcspChecker`] gives for a charge point with no OCSP capability.

use alloc::boxed::Box;
use core::fmt;

/// Which ISO 15118 operation an EV's certificate request is for - OCPP's `CertificateActionEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Iso15118CertificateAction {
    /// The vehicle is requesting its first contract certificate (or a full replacement).
    Install,
    /// The vehicle already holds a contract certificate and is requesting a renewal.
    Update,
}

/// One `Get15118EVCertificate` request: the EXI-encoded `CertificateInstallationReq` the vehicle
/// handed the charge point's HLC stack, opaque to this crate exactly as
/// [`crate::hardware::CertificateHashData`]'s fields are - it is Base64 text carried and
/// forwarded, never decoded.
///
/// The 2.1-only fields are `None` on a connection negotiated as 2.0.1 (which has no wire
/// representation for them) and, per OCPP 2.1's own field description, absent during an ISO
/// 15118-2 session even on a 2.1 connection - they only apply to ISO 15118-20.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Iso15118CertificateRequest {
    /// Which operation this is - `Install` or `Update`.
    pub action: Iso15118CertificateAction,
    /// The raw `CertificateInstallationReq`, Base64-encoded EXI, exactly as received from the
    /// vehicle. Never parsed by this crate.
    pub exi_request: alloc::string::String,
    /// The ISO 15118 schema version in use for this HLC session (e.g. `"urn:iso:15118:2:2013:MsgDef"`),
    /// needed by the CSMS to parse the EXI stream. Bounded to 50 characters on the wire in both
    /// 2.0.1 and 2.1.
    pub iso15118_schema_version: alloc::string::String,
    /// *(ISO 15118-20 only)* Maximum number of contract certificate chains the vehicle wants
    /// installed in one exchange.
    pub maximum_contract_certificate_chains: Option<i64>,
    /// *(ISO 15118-20 only)* EMAIDs the vehicle wants prioritized if more contracts exist than
    /// `maximum_contract_certificate_chains` allows. At most 8 entries of at most 255 characters
    /// each fit the 2.1 wire schema; a longer list is not this crate's to truncate silently, so
    /// [`crate::iso15118`]'s wire adapters drop the field with a warning rather than guess which
    /// entries the vehicle cared most about - see that module's docs.
    pub prioritized_emaids: Option<alloc::vec::Vec<alloc::string::String>>,
}

/// What the CSMS answered to a `Get15118EVCertificate` - OCPP's `Iso15118EVCertificateStatusEnum`
/// plus its payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Iso15118CertificateResult {
    /// `Accepted` or `Failed`.
    pub status: Iso15118CertificateStatus,
    /// The raw `CertificateInstallationRes`, Base64-encoded EXI, to hand back to the vehicle.
    /// Empty when `status` is `Failed` and the CSMS (or this charge point, refusing locally - see
    /// [`crate::iso15118`]) had nothing to send.
    pub exi_response: alloc::string::String,
    /// *(2.1 only)* How many more contract certificates the vehicle can still request in
    /// follow-up `Get15118EVCertificate` calls. `None` on 2.0.1 (no such field) or when the CSMS
    /// did not report one.
    pub remaining_contracts: Option<i64>,
}

/// The status half of [`Iso15118CertificateResult`] - OCPP's `Iso15118EVCertificateStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Iso15118CertificateStatus {
    /// The CSMS obtained a certificate installation response for the vehicle.
    Accepted,
    /// The CSMS could not, or this charge point never asked because it has no ISO 15118 support -
    /// see [`crate::iso15118`]'s capability gating.
    Failed,
}

/// Delivers a `Get15118EVCertificate` result back into the vehicle-facing ISO 15118 HLC session it
/// came from - the integration point B4.5 asks for.
///
/// Implemented by the integrator: relaying the EXI response over PLC/SLAC to the vehicle needs a
/// capability this crate does not carry (see the module docs). [`NoIso15118Controller`] is the
/// honest default for a charge point with no ISO 15118 controller at all.
///
/// # Error handling
///
/// Every delivery is fallible, per `CLAUDE.md`: the HLC session may have already timed out or the
/// vehicle may have unplugged. An `Err` means delivery could not be completed; it does not change
/// the [`Iso15118CertificateResult`] already obtained from the CSMS, which the caller keeps.
#[async_trait::async_trait]
pub trait Iso15118Controller {
    /// The error type returned by a failed delivery attempt.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Hands `result` to the HLC session so it can be relayed to the vehicle.
    async fn deliver_certificate_response(
        &self,
        result: &Iso15118CertificateResult,
    ) -> Result<(), Self::Error>;
}

#[async_trait::async_trait]
impl<T: Iso15118Controller + Send + Sync + ?Sized> Iso15118Controller for alloc::sync::Arc<T> {
    type Error = T::Error;

    async fn deliver_certificate_response(
        &self,
        result: &Iso15118CertificateResult,
    ) -> Result<(), Self::Error> {
        (**self).deliver_certificate_response(result).await
    }
}

/// An [`Iso15118Controller`] for charge points with no ISO 15118 HLC stack at all.
///
/// Every delivery fails honestly - there is nothing wired up to hand the response to.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoIso15118Controller;

/// The error [`NoIso15118Controller`] always returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoIso15118ControllerError;

impl fmt::Display for NoIso15118ControllerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("this charge point has no ISO 15118 controller to deliver a response to")
    }
}

impl core::error::Error for NoIso15118ControllerError {}

#[async_trait::async_trait]
impl Iso15118Controller for NoIso15118Controller {
    type Error = NoIso15118ControllerError;

    async fn deliver_certificate_response(
        &self,
        _result: &Iso15118CertificateResult,
    ) -> Result<(), Self::Error> {
        Err(NoIso15118ControllerError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_charge_point_with_no_controller_reports_it_cannot_deliver() {
        let result = Iso15118CertificateResult {
            status: Iso15118CertificateStatus::Accepted,
            exi_response: "abc".into(),
            remaining_contracts: None,
        };

        let err = NoIso15118Controller
            .deliver_certificate_response(&result)
            .await
            .unwrap_err();

        assert_eq!(err, NoIso15118ControllerError);
    }
}
