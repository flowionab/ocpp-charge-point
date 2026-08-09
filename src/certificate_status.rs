//! `GetCertificateStatus` (2.0.1/2.1) and `GetCertificateChainStatus` (2.1): the CSMS asks this
//! charge point to check a certificate's OCSP status (`docs/PRODUCTION-ROADMAP.md` B4.4).
//!
//! **CSMS-initiated**, both of them - despite the name, this crate does not decide to check a
//! certificate on its own. The CSMS supplies everything a check needs inline (the certificate's
//! hash data and an OCSP responder URL), so unlike [`crate::certificates`] this block never
//! touches [`crate::hardware::CertificateStore`]. See [`crate::hardware::OcspChecker`] for the
//! hardware boundary this leans on: talking to the responder and verifying its answer both need
//! capabilities this crate does not carry.
//!
//! # Why `ocsp_checking` is its own capability, not `certificate_management`
//!
//! [`crate::hardware::Capabilities::certificate_management`] answers "can this charge point hold
//! certificates locally". Neither message here needs that: every certificate named in a request
//! is identified by hash data and a responder URL the CSMS supplies right there, so this crate's
//! own certificate store, if it has one, is never consulted. What these messages actually need is
//! a live path to a third-party OCSP responder and independent verification of what it says - a
//! [`crate::hardware::CertificateStore`], even a secure-element-backed one, gives a charge point
//! neither. Reusing `certificate_management` would tie two genuinely independent hardware facts
//! together: a charge point with a secure element but no outbound path to arbitrary third parties
//! (a common security posture - a CSMS-only network allowlist) would incorrectly advertise OCSP
//! support, and a charge point that *can* reach a responder but keeps no certificates of its own
//! would be denied it. Hence [`crate::hardware::Capabilities::ocsp_checking`], its own flag behind
//! its own `ocsp-checking` Cargo feature, checked at runtime like every other capability-gated
//! message (C5) - see [`crate::refusal`]'s `GetCertificateStatus`/`GetCertificateChainStatus` rows.
//!
//! # `Failed` doubles as "cannot check at all"
//!
//! Neither response schema has a dedicated "not supported" value. `GetCertificateStatusEnum` is
//! only `{Accepted, Failed}`, and `CertificateStatusEnum` is only `{Good, Revoked, Unknown,
//! Failed}` - so an absent capability, an unreachable responder, and a failed check attempt all
//! report through the same value a real hardware failure would: [`CertificateStatusOutcome::Failed`]
//! / [`ChainVerdict::Failed`]. [`ChainVerdict::Unknown`] is reserved for the narrower case where
//! the responder *was* reached and its answer verified, but was itself indeterminate
//! ([`crate::hardware::OcspVerdict::Unknown`]) - a CSMS distinguishing "checked, inconclusive"
//! from "could not check" only has this one bit to read it from, so this crate keeps the two
//! apart rather than collapsing both into `Unknown`.
//!
//! # CRL is out of scope
//!
//! `GetCertificateChainStatus`'s per-entry `source` can name a CRL as well as OCSP
//! (`CertificateStatusSourceEnum`). B4.4 asks for OCSP checking specifically, and this crate has
//! no CRL-fetching or parsing of its own; a CRL-sourced entry answers [`ChainVerdict::Failed`]
//! rather than being silently dropped or guessed at - claiming an unimplemented check happened
//! would be exactly the failure mode this crate's "no crypto dependency" boundary exists to
//! prevent.
//!
//! # The revoked-certificate security event
//!
//! OCPP's security-events appendix has no dedicated "certificate revoked" entry - the closest is
//! [`crate::state::SecurityEventType::InvalidChargingStationCertificate`], the same one
//! [`crate::certificates::handle_certificate_signed`] raises for "our own certificate, and it's
//! unusable". A verdict of [`crate::hardware::OcspVerdict::Revoked`] raises it here too, for the
//! same reason: whichever certificate the CSMS just confirmed is revoked, this charge point
//! should not be relying on it, and OCPP has no more specific standardized event to say so.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use chrono::{DateTime, Utc};

use crate::actor::ChargePointActor;
use crate::clock::Clock;
use crate::hardware::{CertificateHashData, OcspCertificateId, OcspChecker, OcspVerdict};
use crate::state::{SecurityEvent, SecurityEventType};

/// The outcome of a `GetCertificateStatus` request (`GetCertificateStatusEnum`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CertificateStatusOutcome {
    /// The OCSP responder was reached and verified; `ocsp_result` is the raw response to relay
    /// to the CSMS.
    Accepted {
        /// The base64-encoded, DER-encoded `OCSPResponse` to relay verbatim.
        ocsp_result: String,
    },
    /// The capability is absent, the check attempt failed, or the checker returned a verdict
    /// with no raw response to relay - see the module docs.
    Failed,
}

/// Handles a CSMS-initiated `GetCertificateStatus`: asks `checker` for `certificate`'s OCSP
/// status and reports whether a relayable response was obtained.
///
/// Refuses (without touching `checker`) when
/// [`crate::hardware::Capabilities::ocsp_checking`] is absent (C5). Raises
/// [`SecurityEventType::InvalidChargingStationCertificate`] when the verdict is
/// [`OcspVerdict::Revoked`] - see the module docs.
pub async fn handle_get_certificate_status<O: OcspChecker>(
    actor: &ChargePointActor,
    checker: &O,
    certificate: &OcspCertificateId,
) -> CertificateStatusOutcome {
    if !crate::refusal::capability_present(&actor.state().capabilities, "GetCertificateStatus") {
        return CertificateStatusOutcome::Failed;
    }

    match checker.check(certificate).await {
        Ok(result) => {
            if matches!(result.verdict, OcspVerdict::Revoked) {
                report_revoked(actor, &certificate.hash_data).await;
            }
            match result.raw_response {
                Some(ocsp_result) => CertificateStatusOutcome::Accepted { ocsp_result },
                None => {
                    tracing::warn!("OCSP checker returned a verdict with no raw response to relay");
                    CertificateStatusOutcome::Failed
                }
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "OCSP check for GetCertificateStatus failed");
            CertificateStatusOutcome::Failed
        }
    }
}

/// Which revocation-checking method a [`ChainStatusRequest`] names - OCPP's
/// `CertificateStatusSourceEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainStatusSource {
    /// A Certificate Revocation List - not implemented, see the module docs.
    Crl,
    /// OCSP - the only source this crate can actually check.
    Ocsp,
}

/// One certificate named in a `GetCertificateChainStatus` request - OCPP's
/// `CertificateStatusRequestInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainStatusRequest {
    /// Identifies the certificate.
    pub hash_data: CertificateHashData,
    /// Which method the CSMS asked for. Only [`ChainStatusSource::Ocsp`] is implemented.
    pub source: ChainStatusSource,
    /// URL(s) to check against. Only the first is used - see
    /// [`handle_get_certificate_chain_status`].
    pub urls: Vec<String>,
}

/// The four values `CertificateStatusEnum` allows - one more than
/// [`crate::hardware::OcspVerdict`], because the wire status also has to say "this charge point
/// could not check at all" (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainVerdict {
    /// Not revoked, as far as an OCSP responder verified.
    Good,
    /// Revoked.
    Revoked,
    /// Checked, but indeterminate.
    Unknown,
    /// Could not be checked at all: the capability is absent, the request asked for a CRL (out
    /// of scope), no URL was supplied, or the check attempt itself failed.
    Failed,
}

impl From<OcspVerdict> for ChainVerdict {
    fn from(verdict: OcspVerdict) -> Self {
        match verdict {
            OcspVerdict::Good => Self::Good,
            OcspVerdict::Revoked => Self::Revoked,
            OcspVerdict::Unknown => Self::Unknown,
        }
    }
}

/// The reported status of one certificate from `GetCertificateChainStatus` - OCPP's
/// `CertificateStatus`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainCertificateStatus {
    /// Which certificate this is about.
    pub hash_data: CertificateHashData,
    /// The verdict.
    pub status: ChainVerdict,
    /// When this verdict should be considered stale. Falls back to the current time (immediately
    /// due for recheck) when neither the OCSP response nor this crate has anything better - see
    /// [`handle_get_certificate_chain_status`].
    pub next_update: DateTime<Utc>,
}

/// Handles a CSMS-initiated `GetCertificateChainStatus`, checking each of `requests` and
/// returning one [`ChainCertificateStatus`] per entry, in the same order -
/// `GetCertificateChainStatus`'s contract is one response entry per request entry.
///
/// Refuses every entry (without touching `checker`) when
/// [`crate::hardware::Capabilities::ocsp_checking`] is absent (C5). A CRL-sourced entry always
/// answers [`ChainVerdict::Failed`] - see the module docs. `next_update` falls back to `clock`'s
/// current reading when the checker did not supply one. Raises
/// [`SecurityEventType::InvalidChargingStationCertificate`] once per entry whose verdict is
/// [`OcspVerdict::Revoked`].
pub async fn handle_get_certificate_chain_status<O: OcspChecker, C: Clock>(
    actor: &ChargePointActor,
    checker: &O,
    clock: &C,
    requests: &[ChainStatusRequest],
) -> Vec<ChainCertificateStatus> {
    let capable = crate::refusal::capability_present(
        &actor.state().capabilities,
        "GetCertificateChainStatus",
    );

    let mut results = Vec::with_capacity(requests.len());
    for request in requests {
        results.push(check_one_chain_entry(actor, checker, clock, request, capable).await);
    }
    results
}

async fn check_one_chain_entry<O: OcspChecker, C: Clock>(
    actor: &ChargePointActor,
    checker: &O,
    clock: &C,
    request: &ChainStatusRequest,
    capable: bool,
) -> ChainCertificateStatus {
    let failed = |hash_data: &CertificateHashData| ChainCertificateStatus {
        hash_data: hash_data.clone(),
        status: ChainVerdict::Failed,
        next_update: clock.now(),
    };

    if !capable || !matches!(request.source, ChainStatusSource::Ocsp) {
        return failed(&request.hash_data);
    }

    let Some(responder_url) = request.urls.first() else {
        return failed(&request.hash_data);
    };

    let certificate = OcspCertificateId {
        hash_data: request.hash_data.clone(),
        responder_url: responder_url.clone(),
    };

    match checker.check(&certificate).await {
        Ok(result) => {
            if matches!(result.verdict, OcspVerdict::Revoked) {
                report_revoked(actor, &request.hash_data).await;
            }
            ChainCertificateStatus {
                hash_data: request.hash_data.clone(),
                status: result.verdict.into(),
                next_update: result.next_update.unwrap_or_else(|| clock.now()),
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "OCSP check for GetCertificateChainStatus failed");
            failed(&request.hash_data)
        }
    }
}

async fn report_revoked(actor: &ChargePointActor, hash_data: &CertificateHashData) {
    crate::security::report_security_event(
        actor,
        SecurityEvent {
            event_type: SecurityEventType::InvalidChargingStationCertificate,
            tech_info: Some(alloc::format!(
                "OCSP reports certificate (serial {}) as revoked",
                hash_data.serial_number
            )),
        },
    )
    .await;
}

/// Registers this charge point's inbound `GetCertificateStatus` handling with the CSMS
/// connection. Implemented for both 2.0.1 and 2.1 clients.
#[async_trait::async_trait]
pub trait GetCertificateStatusHandler {
    /// Registers a `GetCertificateStatus` handler dispatching against `actor` and `checker`.
    async fn register_get_certificate_status_handler<O>(&self, actor: ChargePointActor, checker: O)
    where
        O: OcspChecker + Send + Sync + 'static;
}

/// Registers this charge point's inbound `GetCertificateChainStatus` handling. **2.1 only** - see
/// the module docs.
#[async_trait::async_trait]
pub trait GetCertificateChainStatusHandler {
    /// Registers a `GetCertificateChainStatus` handler dispatching against `actor` and `checker`,
    /// falling `next_update` back to `clock` where the checker did not supply one.
    async fn register_get_certificate_chain_status_handler<O, C>(
        &self,
        actor: ChargePointActor,
        checker: O,
        clock: C,
    ) where
        O: OcspChecker + Send + Sync + 'static,
        C: Clock + Send + Sync + 'static;
}

/// OCPP 2.0.1 wire adapter: `GetCertificateStatus` only - 2.0.1 has no `GetCertificateChainStatus`.
#[cfg(feature = "ocpp_2_0_1")]
pub mod ocpp_2_0_1 {
    use super::{
        CertificateStatusOutcome, GetCertificateStatusHandler, handle_get_certificate_status,
    };
    use crate::actor::ChargePointActor;
    use crate::hardware::{CertificateHashData, HashAlgorithm, OcspCertificateId, OcspChecker};
    use crate::wire::v201::common::{GetCertificateStatusEnum, HashAlgorithmEnum};
    use crate::wire::v201::{GetCertificateStatusRequest, GetCertificateStatusResponse};

    use alloc::boxed::Box;
    use alloc::string::ToString;

    fn map_hash_algorithm(algorithm: &HashAlgorithmEnum) -> HashAlgorithm {
        match algorithm {
            HashAlgorithmEnum::SHA384 => HashAlgorithm::Sha384,
            HashAlgorithmEnum::SHA512 => HashAlgorithm::Sha512,
            _ => HashAlgorithm::Sha256,
        }
    }

    fn map_certificate(request: &GetCertificateStatusRequest) -> OcspCertificateId {
        let data = &request.ocsp_request_data;
        OcspCertificateId {
            hash_data: CertificateHashData {
                hash_algorithm: map_hash_algorithm(&data.hash_algorithm),
                issuer_name_hash: data.issuer_name_hash.to_string(),
                issuer_key_hash: data.issuer_key_hash.to_string(),
                serial_number: data.serial_number.to_string(),
            },
            responder_url: data.responder_u_r_l.to_string(),
        }
    }

    fn wire_response(outcome: CertificateStatusOutcome) -> GetCertificateStatusResponse {
        match outcome {
            CertificateStatusOutcome::Accepted { ocsp_result } => GetCertificateStatusResponse {
                custom_data: None,
                ocsp_result: Some(ocsp_result),
                status: GetCertificateStatusEnum::Accepted,
                status_info: None,
            },
            CertificateStatusOutcome::Failed => GetCertificateStatusResponse {
                custom_data: None,
                ocsp_result: None,
                status: GetCertificateStatusEnum::Failed,
                status_info: None,
            },
        }
    }

    #[async_trait::async_trait]
    impl GetCertificateStatusHandler for ocpp_client::ocpp_2_0_1::OCPP2_0_1Client {
        async fn register_get_certificate_status_handler<O>(
            &self,
            actor: ChargePointActor,
            checker: O,
        ) where
            O: OcspChecker + Send + Sync + 'static,
        {
            let checker = alloc::sync::Arc::new(checker);
            self.on_get_certificate_status(move |request: GetCertificateStatusRequest, _client| {
                let actor = actor.clone();
                let checker = checker.clone();
                async move {
                    let certificate = map_certificate(&request);
                    let outcome =
                        handle_get_certificate_status(&actor, &*checker, &certificate).await;
                    Ok(wire_response(outcome))
                }
            })
            .await;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_hash_algorithm_maps_onto_this_crates_kind() {
            assert_eq!(
                map_hash_algorithm(&HashAlgorithmEnum::SHA256),
                HashAlgorithm::Sha256
            );
            assert_eq!(
                map_hash_algorithm(&HashAlgorithmEnum::SHA384),
                HashAlgorithm::Sha384
            );
            assert_eq!(
                map_hash_algorithm(&HashAlgorithmEnum::SHA512),
                HashAlgorithm::Sha512
            );
        }

        #[test]
        fn an_accepted_outcome_carries_the_ocsp_result() {
            let response = wire_response(CertificateStatusOutcome::Accepted {
                ocsp_result: "base64==".into(),
            });
            assert_eq!(response.status, GetCertificateStatusEnum::Accepted);
            assert_eq!(response.ocsp_result.as_deref(), Some("base64=="));
        }

        #[test]
        fn a_failed_outcome_carries_no_result() {
            let response = wire_response(CertificateStatusOutcome::Failed);
            assert_eq!(response.status, GetCertificateStatusEnum::Failed);
            assert!(response.ocsp_result.is_none());
        }
    }
}

/// OCPP 2.1 wire adapter: both `GetCertificateStatus` and `GetCertificateChainStatus` - the only
/// version with the latter (see the module's parent docs).
#[cfg(feature = "ocpp_2_1")]
pub mod ocpp_2_1 {
    use super::{
        CertificateStatusOutcome, ChainCertificateStatus, ChainStatusRequest, ChainStatusSource,
        ChainVerdict, GetCertificateChainStatusHandler, GetCertificateStatusHandler,
        handle_get_certificate_chain_status, handle_get_certificate_status,
    };
    use crate::actor::ChargePointActor;
    use crate::clock::Clock;
    use crate::hardware::{CertificateHashData, HashAlgorithm, OcspCertificateId, OcspChecker};
    use crate::wire::v21::common::{
        CertificateStatus, CertificateStatusEnum, CertificateStatusRequestInfo,
        CertificateStatusSourceEnum, GetCertificateStatusEnum, HashAlgorithmEnum,
    };
    use crate::wire::v21::{
        GetCertificateChainStatusRequest, GetCertificateChainStatusResponse,
        GetCertificateStatusRequest, GetCertificateStatusResponse,
    };

    use alloc::boxed::Box;
    use alloc::string::ToString;
    use alloc::vec::Vec;

    fn map_hash_algorithm(algorithm: &HashAlgorithmEnum) -> HashAlgorithm {
        match algorithm {
            HashAlgorithmEnum::SHA384 => HashAlgorithm::Sha384,
            HashAlgorithmEnum::SHA512 => HashAlgorithm::Sha512,
            _ => HashAlgorithm::Sha256,
        }
    }

    fn map_certificate(request: &GetCertificateStatusRequest) -> OcspCertificateId {
        let data = &request.ocsp_request_data;
        OcspCertificateId {
            hash_data: CertificateHashData {
                hash_algorithm: map_hash_algorithm(&data.hash_algorithm),
                issuer_name_hash: data.issuer_name_hash.to_string(),
                issuer_key_hash: data.issuer_key_hash.to_string(),
                serial_number: data.serial_number.to_string(),
            },
            responder_url: data.responder_u_r_l.to_string(),
        }
    }

    fn wire_response(outcome: CertificateStatusOutcome) -> GetCertificateStatusResponse {
        match outcome {
            CertificateStatusOutcome::Accepted { ocsp_result } => GetCertificateStatusResponse {
                custom_data: None,
                ocsp_result: Some(ocsp_result),
                status: GetCertificateStatusEnum::Accepted,
                status_info: None,
            },
            CertificateStatusOutcome::Failed => GetCertificateStatusResponse {
                custom_data: None,
                ocsp_result: None,
                status: GetCertificateStatusEnum::Failed,
                status_info: None,
            },
        }
    }

    fn map_hash_data(
        hash_data: &crate::wire::v21::common::CertificateHashData,
    ) -> CertificateHashData {
        CertificateHashData {
            hash_algorithm: map_hash_algorithm(&hash_data.hash_algorithm),
            issuer_name_hash: hash_data.issuer_name_hash.to_string(),
            issuer_key_hash: hash_data.issuer_key_hash.to_string(),
            serial_number: hash_data.serial_number.to_string(),
        }
    }

    fn wire_hash_algorithm(algorithm: HashAlgorithm) -> HashAlgorithmEnum {
        match algorithm {
            HashAlgorithm::Sha256 => HashAlgorithmEnum::SHA256,
            HashAlgorithm::Sha384 => HashAlgorithmEnum::SHA384,
            HashAlgorithm::Sha512 => HashAlgorithmEnum::SHA512,
        }
    }

    /// This crate's hash data back onto the wire's, or `None` when a field exceeds its bound -
    /// dropped for the same reason `certificates::ocpp_2_1::wire_hash_data` drops one: a
    /// truncated hash would identify a *different* certificate.
    fn wire_hash_data(
        hash_data: &CertificateHashData,
    ) -> Option<crate::wire::v21::common::CertificateHashData> {
        Some(crate::wire::v21::common::CertificateHashData {
            custom_data: None,
            hash_algorithm: wire_hash_algorithm(hash_data.hash_algorithm),
            issuer_key_hash: heapless::String::try_from(hash_data.issuer_key_hash.as_str()).ok()?,
            issuer_name_hash: heapless::String::try_from(hash_data.issuer_name_hash.as_str())
                .ok()?,
            serial_number: heapless::String::try_from(hash_data.serial_number.as_str()).ok()?,
        })
    }

    fn map_source(source: &CertificateStatusSourceEnum) -> ChainStatusSource {
        match source {
            CertificateStatusSourceEnum::OCSP => ChainStatusSource::Ocsp,
            CertificateStatusSourceEnum::CRL => ChainStatusSource::Crl,
        }
    }

    fn map_chain_request(request: &CertificateStatusRequestInfo) -> ChainStatusRequest {
        ChainStatusRequest {
            hash_data: map_hash_data(&request.certificate_hash_data),
            source: map_source(&request.source),
            urls: request.urls.iter().map(|url| url.to_string()).collect(),
        }
    }

    fn wire_verdict(status: ChainVerdict) -> CertificateStatusEnum {
        match status {
            ChainVerdict::Good => CertificateStatusEnum::Good,
            ChainVerdict::Revoked => CertificateStatusEnum::Revoked,
            ChainVerdict::Unknown => CertificateStatusEnum::Unknown,
            ChainVerdict::Failed => CertificateStatusEnum::Failed,
        }
    }

    /// One checked certificate as a wire `CertificateStatus`, or `None` when its hash does not
    /// fit the wire's bounds - dropped rather than reported with a hash naming another
    /// certificate, the same rule [`wire_hash_data`] follows.
    fn wire_chain_status(status: &ChainCertificateStatus) -> Option<CertificateStatus> {
        Some(CertificateStatus {
            certificate_hash_data: wire_hash_data(&status.hash_data)?,
            custom_data: None,
            next_update: status.next_update.into(),
            // Always reported as OCSP: this crate never actually performs a CRL check (see the
            // module docs), so every entry that reaches a verdict at all did so via OCSP.
            source: CertificateStatusSourceEnum::OCSP,
            status: wire_verdict(status.status),
        })
    }

    #[async_trait::async_trait]
    impl GetCertificateStatusHandler for ocpp_client::ocpp_2_1::OCPP2_1Client {
        async fn register_get_certificate_status_handler<O>(
            &self,
            actor: ChargePointActor,
            checker: O,
        ) where
            O: OcspChecker + Send + Sync + 'static,
        {
            let checker = alloc::sync::Arc::new(checker);
            self.on_get_certificate_status(move |request: GetCertificateStatusRequest, _client| {
                let actor = actor.clone();
                let checker = checker.clone();
                async move {
                    let certificate = map_certificate(&request);
                    let outcome =
                        handle_get_certificate_status(&actor, &*checker, &certificate).await;
                    Ok(wire_response(outcome))
                }
            })
            .await;
        }
    }

    #[async_trait::async_trait]
    impl GetCertificateChainStatusHandler for ocpp_client::ocpp_2_1::OCPP2_1Client {
        async fn register_get_certificate_chain_status_handler<O, C>(
            &self,
            actor: ChargePointActor,
            checker: O,
            clock: C,
        ) where
            O: OcspChecker + Send + Sync + 'static,
            C: Clock + Send + Sync + 'static,
        {
            let checker = alloc::sync::Arc::new(checker);
            let clock = alloc::sync::Arc::new(clock);
            self.on_get_certificate_chain_status(
                move |request: GetCertificateChainStatusRequest, _client| {
                    let actor = actor.clone();
                    let checker = checker.clone();
                    let clock = clock.clone();
                    async move {
                        let requests: Vec<ChainStatusRequest> = request
                            .certificate_status_requests
                            .iter()
                            .map(map_chain_request)
                            .collect();
                        let results = handle_get_certificate_chain_status(
                            &actor, &*checker, &*clock, &requests,
                        )
                        .await;
                        let mut certificate_status: heapless::Vec<CertificateStatus, 4> =
                            heapless::Vec::new();
                        for status in results.iter().filter_map(wire_chain_status) {
                            if certificate_status.push(status).is_err() {
                                // Unreachable in practice: the request array itself is bounded to
                                // 4 entries, so a 1:1 response never overflows - but truncate
                                // rather than panic if that invariant is ever violated.
                                tracing::warn!(
                                    "truncating a GetCertificateChainStatus response to the 4 \
                                     entries 2.1 can carry"
                                );
                                break;
                            }
                        }
                        Ok(GetCertificateChainStatusResponse {
                            certificate_status,
                            custom_data: None,
                        })
                    }
                },
            )
            .await;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_verdict_maps_onto_its_own_wire_status() {
            for (verdict, expected) in [
                (ChainVerdict::Good, CertificateStatusEnum::Good),
                (ChainVerdict::Revoked, CertificateStatusEnum::Revoked),
                (ChainVerdict::Unknown, CertificateStatusEnum::Unknown),
                (ChainVerdict::Failed, CertificateStatusEnum::Failed),
            ] {
                assert_eq!(wire_verdict(verdict), expected);
            }
        }

        #[test]
        fn a_crl_source_request_maps_through_rather_than_being_silently_dropped() {
            assert_eq!(
                map_source(&CertificateStatusSourceEnum::CRL),
                ChainStatusSource::Crl
            );
            assert_eq!(
                map_source(&CertificateStatusSourceEnum::OCSP),
                ChainStatusSource::Ocsp
            );
        }

        #[test]
        fn a_checked_status_is_always_reported_as_ocsp_sourced() {
            let status = ChainCertificateStatus {
                hash_data: CertificateHashData {
                    hash_algorithm: HashAlgorithm::Sha256,
                    issuer_name_hash: "aa".into(),
                    issuer_key_hash: "bb".into(),
                    serial_number: "01".into(),
                },
                status: ChainVerdict::Good,
                next_update: chrono::Utc::now(),
            };
            let wire = wire_chain_status(&status).unwrap();
            assert_eq!(wire.source, CertificateStatusSourceEnum::OCSP);
            assert_eq!(wire.status, CertificateStatusEnum::Good);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::ChargePointActor;
    use crate::clock::SystemClock;
    use crate::executor::TokioExecutor;
    use crate::hardware::{HashAlgorithm, OcspCheckResult, OcspVerdict};
    use crate::state::ChargePointEvent;
    use alloc::vec;
    use core::sync::atomic::{AtomicBool, Ordering};

    fn hash_data(serial: &str) -> CertificateHashData {
        CertificateHashData {
            hash_algorithm: HashAlgorithm::Sha256,
            issuer_name_hash: "aa".into(),
            issuer_key_hash: "bb".into(),
            serial_number: serial.into(),
        }
    }

    fn certificate() -> OcspCertificateId {
        OcspCertificateId {
            hash_data: hash_data("01"),
            responder_url: "http://ocsp.example.com".into(),
        }
    }

    async fn actor_with(capabilities: crate::hardware::Capabilities) -> ChargePointActor {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        actor
            .send(ChargePointEvent::CapabilitiesDeclared(capabilities))
            .await
            .unwrap();
        actor
    }

    struct StubChecker {
        result: Result<OcspCheckResult, NoOcspCheckerError>,
        called: AtomicBool,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct NoOcspCheckerError;
    impl core::fmt::Display for NoOcspCheckerError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("stub failure")
        }
    }
    impl core::error::Error for NoOcspCheckerError {}

    #[async_trait::async_trait]
    impl OcspChecker for StubChecker {
        type Error = NoOcspCheckerError;

        async fn check(
            &self,
            _certificate: &OcspCertificateId,
        ) -> Result<OcspCheckResult, Self::Error> {
            self.called.store(true, Ordering::SeqCst);
            self.result.clone()
        }
    }

    #[tokio::test]
    async fn get_certificate_status_refuses_before_touching_the_checker_when_absent() {
        let actor = actor_with(crate::hardware::Capabilities::default()).await;
        let checker = StubChecker {
            result: Ok(OcspCheckResult {
                verdict: OcspVerdict::Good,
                raw_response: Some("base64==".into()),
                next_update: None,
            }),
            called: AtomicBool::new(false),
        };

        let outcome = handle_get_certificate_status(&actor, &checker, &certificate()).await;

        assert_eq!(outcome, CertificateStatusOutcome::Failed);
        assert!(!checker.called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn get_certificate_status_relays_the_raw_response_when_present() {
        let actor =
            actor_with(crate::hardware::Capabilities::default().with_ocsp_checking(true)).await;
        let checker = StubChecker {
            result: Ok(OcspCheckResult {
                verdict: OcspVerdict::Good,
                raw_response: Some("base64==".into()),
                next_update: None,
            }),
            called: AtomicBool::new(false),
        };

        let outcome = handle_get_certificate_status(&actor, &checker, &certificate()).await;

        assert_eq!(
            outcome,
            CertificateStatusOutcome::Accepted {
                ocsp_result: "base64==".into()
            }
        );
    }

    #[tokio::test]
    async fn get_certificate_status_fails_when_the_checker_has_no_raw_response() {
        let actor =
            actor_with(crate::hardware::Capabilities::default().with_ocsp_checking(true)).await;
        let checker = StubChecker {
            result: Ok(OcspCheckResult {
                verdict: OcspVerdict::Unknown,
                raw_response: None,
                next_update: None,
            }),
            called: AtomicBool::new(false),
        };

        let outcome = handle_get_certificate_status(&actor, &checker, &certificate()).await;

        assert_eq!(outcome, CertificateStatusOutcome::Failed);
    }

    #[tokio::test]
    async fn get_certificate_status_fails_when_the_check_attempt_errors() {
        let actor =
            actor_with(crate::hardware::Capabilities::default().with_ocsp_checking(true)).await;
        let checker = StubChecker {
            result: Err(NoOcspCheckerError),
            called: AtomicBool::new(false),
        };

        let outcome = handle_get_certificate_status(&actor, &checker, &certificate()).await;

        assert_eq!(outcome, CertificateStatusOutcome::Failed);
    }

    #[tokio::test]
    async fn a_revoked_verdict_raises_the_closest_standardized_security_event() {
        let actor =
            actor_with(crate::hardware::Capabilities::default().with_ocsp_checking(true)).await;
        let mut events = actor.subscribe_security_events();
        let checker = StubChecker {
            result: Ok(OcspCheckResult {
                verdict: OcspVerdict::Revoked,
                raw_response: Some("base64==".into()),
                next_update: None,
            }),
            called: AtomicBool::new(false),
        };

        handle_get_certificate_status(&actor, &checker, &certificate()).await;

        let event = events.recv().await.unwrap();
        assert_eq!(
            event.event_type,
            SecurityEventType::InvalidChargingStationCertificate
        );
    }

    #[tokio::test]
    async fn chain_status_reports_failed_for_every_entry_when_the_capability_is_absent() {
        let actor = actor_with(crate::hardware::Capabilities::default()).await;
        let checker = StubChecker {
            result: Ok(OcspCheckResult {
                verdict: OcspVerdict::Good,
                raw_response: None,
                next_update: None,
            }),
            called: AtomicBool::new(false),
        };
        let requests = vec![ChainStatusRequest {
            hash_data: hash_data("01"),
            source: ChainStatusSource::Ocsp,
            urls: vec!["http://ocsp.example.com".into()],
        }];

        let results =
            handle_get_certificate_chain_status(&actor, &checker, &SystemClock, &requests).await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, ChainVerdict::Failed);
        assert!(!checker.called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn chain_status_reports_failed_for_a_crl_sourced_entry_without_calling_the_checker() {
        let actor =
            actor_with(crate::hardware::Capabilities::default().with_ocsp_checking(true)).await;
        let checker = StubChecker {
            result: Ok(OcspCheckResult {
                verdict: OcspVerdict::Good,
                raw_response: None,
                next_update: None,
            }),
            called: AtomicBool::new(false),
        };
        let requests = vec![ChainStatusRequest {
            hash_data: hash_data("01"),
            source: ChainStatusSource::Crl,
            urls: vec!["http://crl.example.com".into()],
        }];

        let results =
            handle_get_certificate_chain_status(&actor, &checker, &SystemClock, &requests).await;

        assert_eq!(results[0].status, ChainVerdict::Failed);
        assert!(!checker.called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn chain_status_maps_a_verified_verdict_and_next_update_through() {
        let actor =
            actor_with(crate::hardware::Capabilities::default().with_ocsp_checking(true)).await;
        let next_update = chrono::Utc::now() + chrono::Duration::days(7);
        let checker = StubChecker {
            result: Ok(OcspCheckResult {
                verdict: OcspVerdict::Revoked,
                raw_response: None,
                next_update: Some(next_update),
            }),
            called: AtomicBool::new(false),
        };
        let requests = vec![ChainStatusRequest {
            hash_data: hash_data("01"),
            source: ChainStatusSource::Ocsp,
            urls: vec!["http://ocsp.example.com".into()],
        }];

        let results =
            handle_get_certificate_chain_status(&actor, &checker, &SystemClock, &requests).await;

        assert_eq!(results[0].status, ChainVerdict::Revoked);
        assert_eq!(results[0].next_update, next_update);
    }

    #[tokio::test]
    async fn chain_status_falls_back_to_the_clock_when_no_next_update_is_supplied() {
        let actor =
            actor_with(crate::hardware::Capabilities::default().with_ocsp_checking(true)).await;
        let checker = StubChecker {
            result: Ok(OcspCheckResult {
                verdict: OcspVerdict::Good,
                raw_response: None,
                next_update: None,
            }),
            called: AtomicBool::new(false),
        };
        let requests = vec![ChainStatusRequest {
            hash_data: hash_data("01"),
            source: ChainStatusSource::Ocsp,
            urls: vec!["http://ocsp.example.com".into()],
        }];
        let before = SystemClock.now();

        let results =
            handle_get_certificate_chain_status(&actor, &checker, &SystemClock, &requests).await;

        assert!(results[0].next_update >= before);
    }

    #[tokio::test]
    async fn chain_status_fails_an_entry_with_no_url_rather_than_panicking() {
        let actor =
            actor_with(crate::hardware::Capabilities::default().with_ocsp_checking(true)).await;
        let checker = StubChecker {
            result: Ok(OcspCheckResult {
                verdict: OcspVerdict::Good,
                raw_response: None,
                next_update: None,
            }),
            called: AtomicBool::new(false),
        };
        let requests = vec![ChainStatusRequest {
            hash_data: hash_data("01"),
            source: ChainStatusSource::Ocsp,
            urls: vec![],
        }];

        let results =
            handle_get_certificate_chain_status(&actor, &checker, &SystemClock, &requests).await;

        assert_eq!(results[0].status, ChainVerdict::Failed);
        assert!(!checker.called.load(Ordering::SeqCst));
    }
}
