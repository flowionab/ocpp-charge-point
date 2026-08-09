//! OCPP 1.6J wire adapter for certificate management and the `SignCertificate`/
//! `CertificateSigned` CSR round trip (D2.2).
//!
//! Not yet registered through `ChargePointBuilder`, matching the 2.0.1/2.1 adapters this mirrors
//! (see their module docs) - nothing in this crate's public surface constructs
//! [`Ocpp1_6CertificateHandler`] outside the `std`-convenience impl below and this module's own
//! tests.
//!
//! # Three real losses, all one-directional
//!
//! - **`CertificateType` names only two roots.** 1.6J's enum is
//!   `{CentralSystemRootCertificate, ManufacturerRootCertificate}` - no V2G root, no Mobility
//!   Operator root, no OEM root (2.1's addition), and no way to name the charge point's own V2G
//!   chain at all. A 1.6J CSMS simply cannot install or query anything else; there is no
//!   projection to make for the other [`CertificateUse`] values because the wire has no slot for
//!   them.
//! - **`GetInstalledCertificateIds` takes exactly one type, not an optional list.** 2.x's absent-
//!   means-all rule (`certificate_type: None` asks for everything) has no 1.6J counterpart - the
//!   field is required and singular. So a 1.6J query always resolves to exactly one
//!   [`CertificateUse`], and the response - which itself carries no per-entry type, just a flat
//!   hash list - reports only that use's matches.
//! - **`SignCertificate`/`CertificateSigned` carry no `certificateType` at all.** 2.x's
//!   `CertificateSigningUseEnum` (`ChargingStationCertificate`/`V2GCertificate`) has nothing to
//!   project onto in 1.6J's plain `{csr}`/`{certificateChain}` shapes. This adapter treats every
//!   inbound `CertificateSigned` as [`CertificateSigningPurpose::ChargingStationCertificate`] -
//!   the only purpose 1.6J's Security Whitepaper actually defines this round trip for - and
//!   refuses, before sending anything, to originate a `SignCertificate` for
//!   [`CertificateSigningPurpose::V2GCertificate`] over this connection (see
//!   [`SignCertificateError::UnsupportedPurpose`]): sending an unqualified CSR and letting the
//!   CSMS guess its purpose would be a silent misrepresentation, not a graceful downgrade.
#![cfg_attr(not(any(feature = "std", test)), allow(dead_code))]

use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::vec::Vec;

use crate::wire::v16::common::{
    CertificateHashData as WireHashData, CertificateSignedResponseStatus, CertificateType,
    DeleteCertificateResponseStatus, GetInstalledCertificateIdsResponseStatus,
    HashAlgorithm as WireHashAlgorithm, InstallCertificateResponseStatus,
    SignCertificateResponseStatus,
};
use crate::wire::v16::{
    CertificateSignedRequest, CertificateSignedResponse, DeleteCertificateRequest,
    DeleteCertificateResponse, GetInstalledCertificateIdsRequest,
    GetInstalledCertificateIdsResponse, InstallCertificateRequest, InstallCertificateResponse,
    SignCertificateRequest,
};
use ocpp_client::ClientError;
use ocpp_client::ocpp_1_6::{OCPP1_6Client, OCPP1_6Error};

use crate::actor::ChargePointActor;
use crate::certificates::{
    CertificateHandler, CertificateSignedOutcome, CertificateSigningPurpose,
    CertificateSigningRequester, CsrSubject, PendingSignRequests, SignCertificateError,
    SignCertificateOutcome, build_signed_csr, handle_certificate_signed, handle_delete_certificate,
    handle_get_installed_certificate_ids, handle_install_certificate, record_sign_certificate_sent,
};
use crate::hardware::{
    CertificateHashData, CertificateStore, CertificateUse, DeleteCertificateOutcome, HashAlgorithm,
    InstallCertificateOutcome, InstalledCertificate, KeyStore,
};

/// Whether `purpose` has a representation on 1.6J's `SignCertificate`/`CertificateSigned`, which
/// carry no `certificateType` field at all - see the module docs. Only
/// [`CertificateSigningPurpose::ChargingStationCertificate`] does; a V2G request would have to be
/// sent unqualified and misread by the CSMS as the wrong purpose, so [`Ocpp1_6CertificateHandler`]
/// refuses it before building a CSR or touching the network.
fn purpose_is_representable(purpose: CertificateSigningPurpose) -> bool {
    matches!(
        purpose,
        CertificateSigningPurpose::ChargingStationCertificate
    )
}

/// 1.6J's two-value `CertificateType` onto this crate's [`CertificateUse`]. Total: both wire
/// values have a home.
fn map_certificate_type(certificate_type: &CertificateType) -> CertificateUse {
    match certificate_type {
        CertificateType::CentralSystemRootCertificate => CertificateUse::CsmsRoot,
        CertificateType::ManufacturerRootCertificate => CertificateUse::ManufacturerRoot,
    }
}

fn map_hash_algorithm(algorithm: &WireHashAlgorithm) -> HashAlgorithm {
    match algorithm {
        WireHashAlgorithm::SHA384 => HashAlgorithm::Sha384,
        WireHashAlgorithm::SHA512 => HashAlgorithm::Sha512,
        WireHashAlgorithm::SHA256 => HashAlgorithm::Sha256,
    }
}

fn wire_hash_algorithm(algorithm: HashAlgorithm) -> WireHashAlgorithm {
    match algorithm {
        HashAlgorithm::Sha256 => WireHashAlgorithm::SHA256,
        HashAlgorithm::Sha384 => WireHashAlgorithm::SHA384,
        HashAlgorithm::Sha512 => WireHashAlgorithm::SHA512,
    }
}

fn map_hash_data(hash_data: &WireHashData) -> CertificateHashData {
    CertificateHashData {
        hash_algorithm: map_hash_algorithm(&hash_data.hash_algorithm),
        issuer_name_hash: hash_data.issuer_name_hash.to_string(),
        issuer_key_hash: hash_data.issuer_key_hash.to_string(),
        serial_number: hash_data.serial_number.to_string(),
    }
}

/// This crate's hash data back onto 1.6J's narrower bounds (128/128/40 bytes), or `None` when a
/// field does not fit. As in the 2.x adapters, a truncated hash would identify a *different*
/// certificate, so a value that overflows drops its whole entry rather than being shortened.
fn wire_hash_data(hash_data: &CertificateHashData) -> Option<WireHashData> {
    Some(WireHashData {
        hash_algorithm: wire_hash_algorithm(hash_data.hash_algorithm),
        issuer_key_hash: heapless::String::try_from(hash_data.issuer_key_hash.as_str()).ok()?,
        issuer_name_hash: heapless::String::try_from(hash_data.issuer_name_hash.as_str()).ok()?,
        serial_number: heapless::String::try_from(hash_data.serial_number.as_str()).ok()?,
    })
}

fn install_response(outcome: InstallCertificateOutcome) -> InstallCertificateResponse {
    InstallCertificateResponse {
        status: match outcome {
            InstallCertificateOutcome::Accepted => InstallCertificateResponseStatus::Accepted,
            InstallCertificateOutcome::Rejected => InstallCertificateResponseStatus::Rejected,
            InstallCertificateOutcome::Failed => InstallCertificateResponseStatus::Failed,
        },
    }
}

fn delete_response(outcome: DeleteCertificateOutcome) -> DeleteCertificateResponse {
    DeleteCertificateResponse {
        status: match outcome {
            DeleteCertificateOutcome::Accepted => DeleteCertificateResponseStatus::Accepted,
            DeleteCertificateOutcome::NotFound => DeleteCertificateResponseStatus::NotFound,
            DeleteCertificateOutcome::Failed => DeleteCertificateResponseStatus::Failed,
        },
    }
}

/// 1.6J's response carries no per-entry type - the request already named exactly one - so this
/// just reports the matching hashes, dropping (with a warning already logged by [`wire_hash_data`]
/// via `filter_map`) any that overflow 1.6J's narrower field bounds.
fn ids_response(installed: &[InstalledCertificate]) -> GetInstalledCertificateIdsResponse {
    let hashes: Vec<WireHashData> = installed
        .iter()
        .filter_map(|entry| wire_hash_data(&entry.hash_data))
        .collect();
    GetInstalledCertificateIdsResponse {
        status: if hashes.is_empty() {
            GetInstalledCertificateIdsResponseStatus::NotFound
        } else {
            GetInstalledCertificateIdsResponseStatus::Accepted
        },
        certificate_hash_data: (!hashes.is_empty()).then_some(hashes),
    }
}

fn certificate_signed_response(outcome: CertificateSignedOutcome) -> CertificateSignedResponse {
    CertificateSignedResponse {
        status: match outcome {
            CertificateSignedOutcome::Accepted => CertificateSignedResponseStatus::Accepted,
            CertificateSignedOutcome::Rejected => CertificateSignedResponseStatus::Rejected,
        },
    }
}

/// Registers 1.6J's five certificate/CSR messages.
pub struct Ocpp1_6CertificateHandler {
    client: OCPP1_6Client,
    pending: alloc::sync::Arc<PendingSignRequests>,
}

impl Ocpp1_6CertificateHandler {
    /// Wraps `client`.
    pub fn new(client: OCPP1_6Client) -> Self {
        Self {
            client,
            pending: alloc::sync::Arc::new(PendingSignRequests::new()),
        }
    }
}

#[async_trait::async_trait]
impl CertificateSigningRequester<ClientError<OCPP1_6Error>> for Ocpp1_6CertificateHandler {
    async fn sign_certificate<K>(
        &self,
        key_store: &K,
        key: &crate::hardware::GeneratedKeyPair,
        subject: &CsrSubject,
        purpose: CertificateSigningPurpose,
    ) -> Result<SignCertificateOutcome, SignCertificateError<K::Error, ClientError<OCPP1_6Error>>>
    where
        K: KeyStore + Send + Sync,
    {
        if !purpose_is_representable(purpose) {
            return Err(SignCertificateError::UnsupportedPurpose);
        }

        let csr = build_signed_csr(key_store, key, subject)
            .await
            .map_err(SignCertificateError::Csr)?;

        let response = self
            .client
            .send_sign_certificate(SignCertificateRequest { csr })
            .await
            .map_err(SignCertificateError::Client)?;

        let outcome = match response.status {
            SignCertificateResponseStatus::Accepted => SignCertificateOutcome::Accepted,
            SignCertificateResponseStatus::Rejected => SignCertificateOutcome::Rejected,
        };
        record_sign_certificate_sent(&self.pending, purpose, None, outcome);
        Ok(outcome)
    }
}

#[async_trait::async_trait]
impl CertificateHandler for Ocpp1_6CertificateHandler {
    async fn register_certificate_handlers<S>(&self, actor: ChargePointActor, store: S)
    where
        S: CertificateStore + Send + Sync + 'static,
    {
        let store = alloc::sync::Arc::new(store);

        let install_actor = actor.clone();
        let install_store = store.clone();
        self.client
            .on_install_certificate(move |request: InstallCertificateRequest, _client| {
                let actor = install_actor.clone();
                let store = install_store.clone();
                async move {
                    let outcome = handle_install_certificate(
                        &actor,
                        &store,
                        map_certificate_type(&request.certificate_type),
                        &request.certificate,
                    )
                    .await;
                    Ok(install_response(outcome))
                }
            })
            .await;

        let delete_actor = actor.clone();
        let delete_store = store.clone();
        self.client
            .on_delete_certificate(move |request: DeleteCertificateRequest, _client| {
                let actor = delete_actor.clone();
                let store = delete_store.clone();
                async move {
                    let outcome = handle_delete_certificate(
                        &actor,
                        &store,
                        &map_hash_data(&request.certificate_hash_data),
                    )
                    .await;
                    Ok(delete_response(outcome))
                }
            })
            .await;

        let ids_actor = actor.clone();
        let ids_store = store.clone();
        self.client
            .on_get_installed_certificate_ids(
                move |request: GetInstalledCertificateIdsRequest, _client| {
                    let actor = ids_actor.clone();
                    let store = ids_store.clone();
                    async move {
                        let use_for = map_certificate_type(&request.certificate_type);
                        let installed =
                            handle_get_installed_certificate_ids(&actor, &store, &[use_for]).await;
                        Ok(ids_response(&installed))
                    }
                },
            )
            .await;

        let signed_pending = self.pending.clone();
        self.client
            .on_certificate_signed(move |request: CertificateSignedRequest, _client| {
                let actor = actor.clone();
                let store = store.clone();
                let pending = signed_pending.clone();
                async move {
                    // No `certificateType` on the wire (see module docs) - every 1.6J
                    // `CertificateSigned` is treated as the charging-station certificate.
                    let outcome = handle_certificate_signed(
                        &actor,
                        &store,
                        &pending,
                        CertificateSigningPurpose::ChargingStationCertificate,
                        None,
                        &request.certificate_chain,
                    )
                    .await;
                    Ok(certificate_signed_response(outcome))
                }
            })
            .await;
    }
}

/// The `std` convenience: a bare [`OCPP1_6Client`] handles this block directly.
#[cfg(feature = "std")]
#[async_trait::async_trait]
impl CertificateHandler for OCPP1_6Client {
    async fn register_certificate_handlers<S>(&self, actor: ChargePointActor, store: S)
    where
        S: CertificateStore + Send + Sync + 'static,
    {
        Ocpp1_6CertificateHandler::new(self.clone())
            .register_certificate_handlers(actor, store)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;

    fn hash_data(serial: &str) -> CertificateHashData {
        CertificateHashData {
            hash_algorithm: HashAlgorithm::Sha256,
            issuer_name_hash: "aa".into(),
            issuer_key_hash: "bb".into(),
            serial_number: serial.into(),
        }
    }

    #[test]
    fn both_certificate_types_map_onto_this_crates_uses() {
        assert_eq!(
            map_certificate_type(&CertificateType::CentralSystemRootCertificate),
            CertificateUse::CsmsRoot
        );
        assert_eq!(
            map_certificate_type(&CertificateType::ManufacturerRootCertificate),
            CertificateUse::ManufacturerRoot
        );
    }

    #[test]
    fn every_hash_algorithm_round_trips() {
        for algorithm in [
            HashAlgorithm::Sha256,
            HashAlgorithm::Sha384,
            HashAlgorithm::Sha512,
        ] {
            assert_eq!(
                map_hash_algorithm(&wire_hash_algorithm(algorithm)),
                algorithm
            );
        }
    }

    #[test]
    fn a_hash_too_long_for_1_6s_narrower_bound_drops_its_entry() {
        // 1.6J's `issuerNameHash` is bounded to 128 bytes - narrower than 2.x's - so an entry
        // that fit a 2.x wire request can still overflow here.
        let oversized = InstalledCertificate {
            use_for: CertificateUse::CsmsRoot,
            hash_data: CertificateHashData {
                issuer_name_hash: String::from("a").repeat(200),
                ..hash_data("01")
            },
        };

        assert!(wire_hash_data(&oversized.hash_data).is_none());
        assert_eq!(
            ids_response(core::slice::from_ref(&oversized)).status,
            GetInstalledCertificateIdsResponseStatus::NotFound
        );
    }

    #[test]
    fn holding_no_certificates_is_reported_as_not_found_rather_than_an_error() {
        let response = ids_response(&[]);

        assert_eq!(
            response.status,
            GetInstalledCertificateIdsResponseStatus::NotFound
        );
        assert!(response.certificate_hash_data.is_none());
    }

    #[test]
    fn an_installed_certificate_is_reported_by_hash_alone_with_no_type_field() {
        let response = ids_response(&[InstalledCertificate {
            use_for: CertificateUse::CsmsRoot,
            hash_data: hash_data("01"),
        }]);

        assert_eq!(
            response.status,
            GetInstalledCertificateIdsResponseStatus::Accepted
        );
        let hashes = response.certificate_hash_data.unwrap();
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes[0].serial_number, "01");
    }

    #[test]
    fn every_outcome_has_its_own_wire_status() {
        assert_eq!(
            install_response(InstallCertificateOutcome::Failed).status,
            InstallCertificateResponseStatus::Failed
        );
        assert_eq!(
            install_response(InstallCertificateOutcome::Rejected).status,
            InstallCertificateResponseStatus::Rejected
        );
        assert_eq!(
            delete_response(DeleteCertificateOutcome::NotFound).status,
            DeleteCertificateResponseStatus::NotFound
        );
        assert_eq!(
            delete_response(DeleteCertificateOutcome::Failed).status,
            DeleteCertificateResponseStatus::Failed
        );
    }

    #[test]
    fn certificate_signed_outcomes_map_onto_1_6s_two_statuses() {
        assert_eq!(
            certificate_signed_response(CertificateSignedOutcome::Accepted).status,
            CertificateSignedResponseStatus::Accepted
        );
        assert_eq!(
            certificate_signed_response(CertificateSignedOutcome::Rejected).status,
            CertificateSignedResponseStatus::Rejected
        );
    }

    #[test]
    fn only_the_charging_station_certificate_purpose_is_representable_on_the_wire() {
        assert!(purpose_is_representable(
            CertificateSigningPurpose::ChargingStationCertificate
        ));
        assert!(!purpose_is_representable(
            CertificateSigningPurpose::V2GCertificate
        ));
    }
}
