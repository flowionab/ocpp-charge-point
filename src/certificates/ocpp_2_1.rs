//! OCPP 2.1 wire adapter for certificate management (B4.2) and the `SignCertificate`/
//! `CertificateSigned` CSR round trip (B4.3).
//!
//! Not yet registered through `ChargePointBuilder` (nothing in this crate's public surface
//! constructs [`Ocpp2_1CertificateHandler`] outside the `std`-convenience impl below and this
//! module's own tests) - see `certificates` capability feature's Cargo.toml comment. Until it is,
//! every item here is unreachable in a `--no-default-features` (no `std`, no `test`) build, which
//! `deny(warnings)` would otherwise flag as dead code.
#![cfg_attr(not(any(feature = "std", test)), allow(dead_code))]

use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::vec::Vec;

use crate::wire::v21::common::{
    CertificateHashData as WireHashData, CertificateHashDataChain, CertificateSignedStatusEnum,
    CertificateSigningUseEnum, DeleteCertificateStatusEnum, GenericStatusEnum,
    GetCertificateIdUseEnum, GetInstalledCertificateStatusEnum, HashAlgorithmEnum,
    InstallCertificateStatusEnum, InstallCertificateUseEnum,
};
use crate::wire::v21::{
    CertificateSignedRequest, CertificateSignedResponse, DeleteCertificateRequest,
    DeleteCertificateResponse, GetInstalledCertificateIdsRequest,
    GetInstalledCertificateIdsResponse, InstallCertificateRequest, InstallCertificateResponse,
    SignCertificateRequest,
};
use ocpp_client::ClientError;
use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};

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

/// 2.1's install-use enum onto this crate's. Every value has a counterpart.
fn map_install_use(use_for: &InstallCertificateUseEnum) -> CertificateUse {
    match use_for {
        InstallCertificateUseEnum::V2GRootCertificate => CertificateUse::V2gRoot,
        InstallCertificateUseEnum::MORootCertificate => CertificateUse::MobilityOperatorRoot,
        InstallCertificateUseEnum::ManufacturerRootCertificate => CertificateUse::ManufacturerRoot,
        InstallCertificateUseEnum::OEMRootCertificate => CertificateUse::OemRoot,
        _ => CertificateUse::CsmsRoot,
    }
}

/// 2.1's *get*-use enum onto this crate's. Wider than the install one: it adds the charge point's
/// own V2G chain, which can be listed but never installed.
fn map_get_use(use_for: &GetCertificateIdUseEnum) -> CertificateUse {
    match use_for {
        GetCertificateIdUseEnum::V2GRootCertificate => CertificateUse::V2gRoot,
        GetCertificateIdUseEnum::MORootCertificate => CertificateUse::MobilityOperatorRoot,
        GetCertificateIdUseEnum::ManufacturerRootCertificate => CertificateUse::ManufacturerRoot,
        GetCertificateIdUseEnum::OEMRootCertificate => CertificateUse::OemRoot,
        GetCertificateIdUseEnum::V2GCertificateChain => CertificateUse::V2gCertificateChain,
        _ => CertificateUse::CsmsRoot,
    }
}

/// This crate's use back onto 2.1's get-use enum, for reporting what is installed.
///
/// [`CertificateUse::ChargingStation`] has no `GetCertificateIdUseEnum` value - 2.1 lists the V2G
/// chain but not the charge point's own client certificate - so it is reported as the V2G chain,
/// which is the only "our own certificate" value the enum has.
fn wire_get_use(use_for: CertificateUse) -> GetCertificateIdUseEnum {
    match use_for {
        CertificateUse::CsmsRoot => GetCertificateIdUseEnum::CSMSRootCertificate,
        CertificateUse::V2gRoot => GetCertificateIdUseEnum::V2GRootCertificate,
        CertificateUse::MobilityOperatorRoot => GetCertificateIdUseEnum::MORootCertificate,
        CertificateUse::ManufacturerRoot => GetCertificateIdUseEnum::ManufacturerRootCertificate,
        CertificateUse::OemRoot => GetCertificateIdUseEnum::OEMRootCertificate,
        CertificateUse::V2gCertificateChain | CertificateUse::ChargingStation => {
            GetCertificateIdUseEnum::V2GCertificateChain
        }
    }
}

fn map_hash_algorithm(algorithm: &HashAlgorithmEnum) -> HashAlgorithm {
    match algorithm {
        HashAlgorithmEnum::SHA384 => HashAlgorithm::Sha384,
        HashAlgorithmEnum::SHA512 => HashAlgorithm::Sha512,
        _ => HashAlgorithm::Sha256,
    }
}

fn wire_hash_algorithm(algorithm: HashAlgorithm) -> HashAlgorithmEnum {
    match algorithm {
        HashAlgorithm::Sha256 => HashAlgorithmEnum::SHA256,
        HashAlgorithm::Sha384 => HashAlgorithmEnum::SHA384,
        HashAlgorithm::Sha512 => HashAlgorithmEnum::SHA512,
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

/// This crate's hash data back onto the wire's, or `None` when a field exceeds its bound.
///
/// A truncated hash identifies a *different* certificate, so a value that does not fit is dropped
/// with its whole entry rather than shortened - see [`wire_chain`].
fn wire_hash_data(hash_data: &CertificateHashData) -> Option<WireHashData> {
    Some(WireHashData {
        custom_data: None,
        hash_algorithm: wire_hash_algorithm(hash_data.hash_algorithm),
        issuer_key_hash: heapless::String::try_from(hash_data.issuer_key_hash.as_str()).ok()?,
        issuer_name_hash: heapless::String::try_from(hash_data.issuer_name_hash.as_str()).ok()?,
        serial_number: heapless::String::try_from(hash_data.serial_number.as_str()).ok()?,
    })
}

/// One installed certificate as a wire chain entry. `None` when its hashes do not fit the wire's
/// bounds, which drops that entry rather than reporting a hash that names something else.
fn wire_chain(installed: &InstalledCertificate) -> Option<CertificateHashDataChain> {
    Some(CertificateHashDataChain {
        certificate_hash_data: wire_hash_data(&installed.hash_data)?,
        certificate_type: wire_get_use(installed.use_for),
        // This crate's store holds leaf certificates flat rather than as chains - it does no X.509
        // parsing, so it cannot know which certificate issued which. A CSMS reading this sees each
        // installed certificate on its own, which is accurate rather than merely incomplete.
        child_certificate_hash_data: None,
        custom_data: None,
    })
}

fn install_response(outcome: InstallCertificateOutcome) -> InstallCertificateResponse {
    InstallCertificateResponse {
        custom_data: None,
        status: match outcome {
            InstallCertificateOutcome::Accepted => InstallCertificateStatusEnum::Accepted,
            InstallCertificateOutcome::Rejected => InstallCertificateStatusEnum::Rejected,
            InstallCertificateOutcome::Failed => InstallCertificateStatusEnum::Failed,
        },
        status_info: None,
    }
}

fn delete_response(outcome: DeleteCertificateOutcome) -> DeleteCertificateResponse {
    DeleteCertificateResponse {
        custom_data: None,
        status: match outcome {
            DeleteCertificateOutcome::Accepted => DeleteCertificateStatusEnum::Accepted,
            DeleteCertificateOutcome::NotFound => DeleteCertificateStatusEnum::NotFound,
            DeleteCertificateOutcome::Failed => DeleteCertificateStatusEnum::Failed,
        },
        status_info: None,
    }
}

/// The wire `certificateType` onto this crate's purpose. Absent `certificateType` on
/// `CertificateSigned` defaults to `ChargingStationCertificate`, per OCPP's spec text for that
/// field (unchanged from 2.0.1).
fn map_signing_purpose(use_for: Option<&CertificateSigningUseEnum>) -> CertificateSigningPurpose {
    match use_for {
        Some(CertificateSigningUseEnum::V2GCertificate) => {
            CertificateSigningPurpose::V2GCertificate
        }
        _ => CertificateSigningPurpose::ChargingStationCertificate,
    }
}

fn wire_signing_purpose(purpose: CertificateSigningPurpose) -> CertificateSigningUseEnum {
    match purpose {
        CertificateSigningPurpose::ChargingStationCertificate => {
            CertificateSigningUseEnum::ChargingStationCertificate
        }
        CertificateSigningPurpose::V2GCertificate => CertificateSigningUseEnum::V2GCertificate,
    }
}

fn certificate_signed_response(outcome: CertificateSignedOutcome) -> CertificateSignedResponse {
    CertificateSignedResponse {
        custom_data: None,
        status: match outcome {
            CertificateSignedOutcome::Accepted => CertificateSignedStatusEnum::Accepted,
            CertificateSignedOutcome::Rejected => CertificateSignedStatusEnum::Rejected,
        },
        status_info: None,
    }
}

fn ids_response(installed: &[InstalledCertificate]) -> GetInstalledCertificateIdsResponse {
    let chain: Vec<CertificateHashDataChain> = installed.iter().filter_map(wire_chain).collect();
    GetInstalledCertificateIdsResponse {
        // `NotFound` is OCPP's "none installed", not an error - a charge point holding no
        // certificates is a normal state, and the only other status is `Accepted`.
        status: if chain.is_empty() {
            GetInstalledCertificateStatusEnum::NotFound
        } else {
            GetInstalledCertificateStatusEnum::Accepted
        },
        certificate_hash_data_chain: (!chain.is_empty()).then_some(chain),
        custom_data: None,
        status_info: None,
    }
}

/// Registers 2.1's three certificate messages, plus the B4.3 `SignCertificate`/
/// `CertificateSigned` round trip.
pub struct Ocpp2_1CertificateHandler {
    client: OCPP2_1Client,
    pending: alloc::sync::Arc<PendingSignRequests>,
}

impl Ocpp2_1CertificateHandler {
    /// Wraps `client`.
    pub fn new(client: OCPP2_1Client) -> Self {
        Self {
            client,
            pending: alloc::sync::Arc::new(PendingSignRequests::new()),
        }
    }
}

#[async_trait::async_trait]
impl CertificateSigningRequester<ClientError<OCPP2_1Error>> for Ocpp2_1CertificateHandler {
    /// Builds a CSR for `purpose` over `key` (through `key_store`, without ever seeing the
    /// private key - see [`build_signed_csr`]) and sends it as a `SignCertificate` request,
    /// tagged with a fresh `requestId` (2.1's addition over 2.0.1) so the `CertificateSigned`
    /// this provokes can be correlated even if another CSR is outstanding at the same time.
    ///
    /// On [`SignCertificateOutcome::Accepted`], records the request as pending so a later
    /// `CertificateSigned` for the same `purpose`/`requestId` is recognised as solicited (see
    /// [`handle_certificate_signed`]).
    async fn sign_certificate<K>(
        &self,
        key_store: &K,
        key: &crate::hardware::GeneratedKeyPair,
        subject: &CsrSubject,
        purpose: CertificateSigningPurpose,
    ) -> Result<SignCertificateOutcome, SignCertificateError<K::Error, ClientError<OCPP2_1Error>>>
    where
        K: KeyStore + Send + Sync,
    {
        let csr = build_signed_csr(key_store, key, subject)
            .await
            .map_err(SignCertificateError::Csr)?;
        let request_id = self.pending.next_request_id();

        let response = self
            .client
            .send_sign_certificate(SignCertificateRequest {
                certificate_type: Some(wire_signing_purpose(purpose)),
                csr,
                custom_data: None,
                hash_root_certificate: None,
                request_id: Some(request_id),
            })
            .await
            .map_err(SignCertificateError::Client)?;

        let outcome = match response.status {
            GenericStatusEnum::Accepted => SignCertificateOutcome::Accepted,
            GenericStatusEnum::Rejected => SignCertificateOutcome::Rejected,
        };
        record_sign_certificate_sent(&self.pending, purpose, Some(request_id), outcome);
        Ok(outcome)
    }
}

#[async_trait::async_trait]
impl CertificateHandler for Ocpp2_1CertificateHandler {
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
                        map_install_use(&request.certificate_type),
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
                        let uses: Vec<CertificateUse> = request
                            .certificate_type
                            .as_deref()
                            .unwrap_or_default()
                            .iter()
                            .map(map_get_use)
                            .collect();
                        let installed =
                            handle_get_installed_certificate_ids(&actor, &store, &uses).await;
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
                    let purpose = map_signing_purpose(request.certificate_type.as_ref());
                    let outcome = handle_certificate_signed(
                        &actor,
                        &store,
                        &pending,
                        purpose,
                        request.request_id,
                        &request.certificate_chain,
                    )
                    .await;
                    Ok(certificate_signed_response(outcome))
                }
            })
            .await;
    }
}

/// The `std` convenience: a bare [`OCPP2_1Client`] handles this block directly.
#[cfg(feature = "std")]
#[async_trait::async_trait]
impl CertificateHandler for OCPP2_1Client {
    async fn register_certificate_handlers<S>(&self, actor: ChargePointActor, store: S)
    where
        S: CertificateStore + Send + Sync + 'static,
    {
        Ocpp2_1CertificateHandler::new(self.clone())
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
    fn every_install_use_maps_onto_this_crates_kinds() {
        for (wire, expected) in [
            (
                InstallCertificateUseEnum::CSMSRootCertificate,
                CertificateUse::CsmsRoot,
            ),
            (
                InstallCertificateUseEnum::V2GRootCertificate,
                CertificateUse::V2gRoot,
            ),
            (
                InstallCertificateUseEnum::MORootCertificate,
                CertificateUse::MobilityOperatorRoot,
            ),
            (
                InstallCertificateUseEnum::ManufacturerRootCertificate,
                CertificateUse::ManufacturerRoot,
            ),
            (
                InstallCertificateUseEnum::OEMRootCertificate,
                CertificateUse::OemRoot,
            ),
        ] {
            assert_eq!(map_install_use(&wire), expected);
        }
    }

    #[test]
    fn the_get_use_enum_adds_the_chain_the_install_one_cannot_express() {
        // `GetCertificateIdUseEnum` is the wider of the two: it can name the charge point's own
        // V2G chain, which `InstallCertificateUseEnum` deliberately cannot.
        assert_eq!(
            map_get_use(&GetCertificateIdUseEnum::V2GCertificateChain),
            CertificateUse::V2gCertificateChain
        );
        assert_eq!(
            wire_get_use(CertificateUse::V2gCertificateChain),
            GetCertificateIdUseEnum::V2GCertificateChain
        );
    }

    #[test]
    fn a_hash_too_long_for_the_wire_drops_its_entry_rather_than_naming_another_certificate() {
        // A truncated issuer hash identifies a *different* certificate, and a CSMS would then ask
        // to delete the wrong one.
        let oversized = InstalledCertificate {
            use_for: CertificateUse::CsmsRoot,
            hash_data: CertificateHashData {
                issuer_name_hash: String::from("a").repeat(200),
                ..hash_data("01")
            },
        };

        assert!(wire_chain(&oversized).is_none());
        assert_eq!(
            ids_response(core::slice::from_ref(&oversized)).status,
            GetInstalledCertificateStatusEnum::NotFound
        );
    }

    #[test]
    fn holding_no_certificates_is_reported_as_not_found_rather_than_an_error() {
        let response = ids_response(&[]);

        assert_eq!(response.status, GetInstalledCertificateStatusEnum::NotFound);
        // And no empty list alongside it, which would be a second way of saying the same thing.
        assert!(response.certificate_hash_data_chain.is_none());
    }

    #[test]
    fn an_installed_certificate_is_reported_with_its_hash_and_use() {
        let response = ids_response(&[InstalledCertificate {
            use_for: CertificateUse::CsmsRoot,
            hash_data: hash_data("01"),
        }]);

        assert_eq!(response.status, GetInstalledCertificateStatusEnum::Accepted);
        let chain = response.certificate_hash_data_chain.unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(
            chain[0].certificate_type,
            GetCertificateIdUseEnum::CSMSRootCertificate
        );
        assert_eq!(chain[0].certificate_hash_data.serial_number, "01");
        // No children: this crate's store holds certificates flat, since it does no X.509 parsing
        // and cannot know which issued which.
        assert!(chain[0].child_certificate_hash_data.is_none());
    }

    #[test]
    fn every_outcome_has_its_own_wire_status() {
        assert_eq!(
            install_response(InstallCertificateOutcome::Failed).status,
            InstallCertificateStatusEnum::Failed
        );
        assert_eq!(
            install_response(InstallCertificateOutcome::Rejected).status,
            InstallCertificateStatusEnum::Rejected
        );
        assert_eq!(
            delete_response(DeleteCertificateOutcome::NotFound).status,
            DeleteCertificateStatusEnum::NotFound
        );
        assert_eq!(
            delete_response(DeleteCertificateOutcome::Failed).status,
            DeleteCertificateStatusEnum::Failed
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
}
