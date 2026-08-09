//! OCPP 2.1 wire adapter for `Get15118EVCertificate` (B4.5).
//!
//! 2.1 extends the same action 2.0.1 defines with ISO 15118-20's contract-chain fields
//! (`maximumContractCertificateChains`, `prioritizedEMAIDs` on the request;
//! `remainingContracts` on the response) - see `crate::iso15118`'s module docs for why a caller
//! on an ISO 15118-2 session should leave the request-side pair `None` rather than this crate
//! refusing to forward them.

use alloc::boxed::Box;
use alloc::string::ToString;

use ocpp_client::ClientError;
use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};

use crate::actor::ChargePointActor;
use crate::hardware::{
    Iso15118CertificateAction, Iso15118CertificateRequest, Iso15118CertificateResult,
    Iso15118CertificateStatus, Iso15118Controller,
};
use crate::iso15118::{Iso15118CertificateRequester, deliver, iso15118_supported, refused_locally};
use crate::wire::v21::common::{CertificateActionEnum, Iso15118EVCertificateStatusEnum};
use crate::wire::v21::{Get15118EVCertificateRequest, Get15118EVCertificateResponse};

fn wire_action(action: Iso15118CertificateAction) -> CertificateActionEnum {
    match action {
        Iso15118CertificateAction::Install => CertificateActionEnum::Install,
        Iso15118CertificateAction::Update => CertificateActionEnum::Update,
    }
}

/// The vehicle's prioritized EMAID list onto the wire's bounded form: at most 8 entries of at
/// most 255 characters each. An entry that does not fit is dropped (with a warning) rather than
/// truncated - a shortened EMAID identifies a *different* contract, the same reasoning
/// `crate::certificates`' `wire_hash_data` applies to certificate hashes. An empty result becomes
/// `None`, since the wire schema requires at least one entry when the field is present at all
/// (`check_min_items(1)`).
fn wire_prioritized_emaids(
    emaids: &[alloc::string::String],
) -> Option<heapless::Vec<heapless::String<255>, 8>> {
    let mut wire: heapless::Vec<heapless::String<255>, 8> = heapless::Vec::new();
    for emaid in emaids.iter().take(8) {
        match heapless::String::try_from(emaid.as_str()) {
            Ok(value) => {
                // `wire.len() < 8` by construction (`take(8)`), so this never fails.
                let _ = wire.push(value);
            }
            Err(()) => {
                tracing::warn!(
                    emaid,
                    "dropping a prioritized EMAID that exceeds the wire's 255-character bound"
                );
            }
        }
    }
    if emaids.len() > 8 {
        tracing::warn!(
            requested = emaids.len(),
            "prioritized EMAID list exceeds the wire's 8-entry bound; only the first 8 were sent"
        );
    }
    (!wire.is_empty()).then_some(wire)
}

fn map_response(response: Get15118EVCertificateResponse) -> Iso15118CertificateResult {
    Iso15118CertificateResult {
        status: match response.status {
            Iso15118EVCertificateStatusEnum::Accepted => Iso15118CertificateStatus::Accepted,
            Iso15118EVCertificateStatusEnum::Failed => Iso15118CertificateStatus::Failed,
        },
        exi_response: response.exi_response.to_string(),
        remaining_contracts: response.remaining_contracts,
    }
}

#[async_trait::async_trait]
impl Iso15118CertificateRequester<ClientError<OCPP2_1Error>> for OCPP2_1Client {
    async fn request_ev_certificate<C>(
        &self,
        actor: &ChargePointActor,
        controller: &C,
        request: Iso15118CertificateRequest,
    ) -> Result<Iso15118CertificateResult, ClientError<OCPP2_1Error>>
    where
        C: Iso15118Controller + Send + Sync,
    {
        if !iso15118_supported(&actor.state().capabilities) {
            tracing::warn!(
                "refusing to send Get15118EVCertificate: this charge point has no ISO 15118 support"
            );
            let result = refused_locally();
            deliver(controller, &result).await;
            return Ok(result);
        }

        let iso15118_schema_version =
            match heapless::String::try_from(request.iso15118_schema_version.as_str()) {
                Ok(version) => version,
                Err(()) => {
                    tracing::warn!(
                        schema_version = %request.iso15118_schema_version,
                        "refusing to send Get15118EVCertificate: iso15118_schema_version exceeds \
                         the wire's 50-character bound"
                    );
                    let result = refused_locally();
                    deliver(controller, &result).await;
                    return Ok(result);
                }
            };

        let prioritized_e_m_a_i_ds = request
            .prioritized_emaids
            .as_deref()
            .and_then(wire_prioritized_emaids);

        let response = self
            .send_get_15118_ev_certificate(Get15118EVCertificateRequest {
                action: wire_action(request.action),
                custom_data: None,
                exi_request: request.exi_request,
                iso15118_schema_version,
                maximum_contract_certificate_chains: request.maximum_contract_certificate_chains,
                prioritized_e_m_a_i_ds,
            })
            .await?;

        let result = map_response(response);
        deliver(controller, &result).await;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::{Capabilities, Iso15118SupportLevel, NoIso15118Controller};
    use alloc::vec::Vec;

    fn request() -> Iso15118CertificateRequest {
        Iso15118CertificateRequest {
            action: Iso15118CertificateAction::Update,
            exi_request: "deadbeef".into(),
            iso15118_schema_version: "urn:iso:std:iso:15118:-20:CommonMessages".into(),
            maximum_contract_certificate_chains: Some(3),
            prioritized_emaids: Some(alloc::vec!["FR000000000A".into(), "DE000000000B".into()]),
        }
    }

    #[test]
    fn wire_action_maps_both_variants() {
        assert_eq!(
            wire_action(Iso15118CertificateAction::Install),
            CertificateActionEnum::Install
        );
        assert_eq!(
            wire_action(Iso15118CertificateAction::Update),
            CertificateActionEnum::Update
        );
    }

    #[test]
    fn map_response_carries_remaining_contracts() {
        let result = map_response(Get15118EVCertificateResponse {
            custom_data: None,
            exi_response: "cafebabe".into(),
            remaining_contracts: Some(2),
            status: Iso15118EVCertificateStatusEnum::Accepted,
            status_info: None,
        });

        assert_eq!(result.status, Iso15118CertificateStatus::Accepted);
        assert_eq!(result.remaining_contracts, Some(2));
    }

    #[test]
    fn prioritized_emaids_within_bounds_all_survive() {
        let req = request();
        let emaids = req.prioritized_emaids.unwrap();
        let wire = wire_prioritized_emaids(&emaids).unwrap();
        assert_eq!(wire.len(), 2);
    }

    #[test]
    fn a_prioritized_emaid_that_does_not_fit_is_dropped_not_truncated() {
        let emaids: Vec<alloc::string::String> = alloc::vec!["x".repeat(256)];
        let wire = wire_prioritized_emaids(&emaids);
        assert!(wire.is_none());
    }

    #[test]
    fn more_than_eight_emaids_keeps_only_the_first_eight() {
        let emaids: Vec<alloc::string::String> =
            (0..10).map(|i| alloc::format!("EMAID{i}")).collect();
        let wire = wire_prioritized_emaids(&emaids).unwrap();
        assert_eq!(wire.len(), 8);
    }

    #[test]
    fn iso15118_supported_reflects_iso15118_20_too() {
        let level_20 = Capabilities {
            iso15118_support: Iso15118SupportLevel::Iso15118_20,
            ..Capabilities::default()
        };
        assert!(iso15118_supported(&level_20));
    }

    #[test]
    fn no_iso15118_controller_exists_for_use_in_tests() {
        let _ = NoIso15118Controller;
    }
}
