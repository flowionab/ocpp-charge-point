//! OCPP 2.0.1 wire adapter for certificate management (B4.2).

use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::vec::Vec;

use crate::wire::v201::common::{
    CertificateHashData as WireHashData, CertificateHashDataChain, DeleteCertificateStatusEnum,
    GetCertificateIdUseEnum, GetInstalledCertificateStatusEnum, HashAlgorithmEnum,
    InstallCertificateStatusEnum, InstallCertificateUseEnum,
};
use crate::wire::v201::{
    DeleteCertificateRequest, DeleteCertificateResponse, GetInstalledCertificateIdsRequest,
    GetInstalledCertificateIdsResponse, InstallCertificateRequest, InstallCertificateResponse,
};
use ocpp_client::ocpp_2_0_1::OCPP2_0_1Client;

use crate::actor::ChargePointActor;
use crate::certificates::{
    CertificateHandler, handle_delete_certificate, handle_get_installed_certificate_ids,
    handle_install_certificate,
};
use crate::hardware::{
    CertificateHashData, CertificateStore, CertificateUse, DeleteCertificateOutcome, HashAlgorithm,
    InstallCertificateOutcome, InstalledCertificate,
};

/// 2.1's install-use enum onto this crate's. Every value has a counterpart.
fn map_install_use(use_for: &InstallCertificateUseEnum) -> CertificateUse {
    match use_for {
        InstallCertificateUseEnum::V2GRootCertificate => CertificateUse::V2gRoot,
        InstallCertificateUseEnum::MORootCertificate => CertificateUse::MobilityOperatorRoot,
        InstallCertificateUseEnum::ManufacturerRootCertificate => CertificateUse::ManufacturerRoot,
        // No `OEMRootCertificate` here - that is 2.1's addition, so a 2.0.1 CSMS has no way to
        // name it.
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
        // 2.0.1's enum has no `OEMRootCertificate`. An OEM root can only have been installed over
        // a 2.1 connection, and reporting it to a 2.0.1 CSMS at all is a projection: the
        // manufacturer root is the closest use 2.0.1 can name, and both are "a root this station's
        // maker put here". A documented loss, like `wire_purpose`'s `PriorityCharging`.
        CertificateUse::OemRoot => GetCertificateIdUseEnum::ManufacturerRootCertificate,
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

/// Registers 2.1's three certificate messages.
pub struct Ocpp2_0_1CertificateHandler {
    client: OCPP2_0_1Client,
}

impl Ocpp2_0_1CertificateHandler {
    /// Wraps `client`.
    pub fn new(client: OCPP2_0_1Client) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl CertificateHandler for Ocpp2_0_1CertificateHandler {
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

        self.client
            .on_get_installed_certificate_ids(
                move |request: GetInstalledCertificateIdsRequest, _client| {
                    let actor = actor.clone();
                    let store = store.clone();
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
    }
}

/// The `std` convenience: a bare [`OCPP2_0_1Client`] handles this block directly.
#[cfg(feature = "std")]
#[async_trait::async_trait]
impl CertificateHandler for OCPP2_0_1Client {
    async fn register_certificate_handlers<S>(&self, actor: ChargePointActor, store: S)
    where
        S: CertificateStore + Send + Sync + 'static,
    {
        Ocpp2_0_1CertificateHandler::new(self.clone())
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
        ] {
            assert_eq!(map_install_use(&wire), expected);
        }
    }

    #[test]
    fn an_oem_root_degrades_to_the_manufacturer_root_2_0_1_can_name() {
        // 2.0.1's enum has no `OEMRootCertificate`; both are "a root this station's maker put
        // here", so this is the closest 2.0.1 can express rather than an omission.
        assert_eq!(
            wire_get_use(CertificateUse::OemRoot),
            GetCertificateIdUseEnum::ManufacturerRootCertificate
        );
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
