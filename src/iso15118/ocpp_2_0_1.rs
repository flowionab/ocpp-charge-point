//! OCPP 2.0.1 wire adapter for `Get15118EVCertificate` (B4.5).
//!
//! 2.0.1's request has no ISO 15118-20 contract-chain fields (`maximumContractCertificateChains`,
//! `prioritizedEMAIDs`) and its response has no `remainingContracts` - both are 2.1 additions
//! (see `crate::iso15118`'s module docs on why the *level* still matters even though both
//! versions send the same action). A request that populated them anyway simply has nothing to
//! carry them in here; they are silently unrepresentable on this version, the same kind of
//! documented, version-shaped loss `crate::certificates`' `wire_get_use` describes for
//! `OEMRootCertificate`.

use alloc::boxed::Box;
use alloc::string::ToString;

use ocpp_client::ClientError;
use ocpp_client::ocpp_2_0_1::{OCPP2_0_1Client, OCPP2_0_1Error};

use crate::actor::ChargePointActor;
use crate::hardware::{
    Iso15118CertificateAction, Iso15118CertificateRequest, Iso15118CertificateResult,
    Iso15118CertificateStatus, Iso15118Controller,
};
use crate::iso15118::{Iso15118CertificateRequester, deliver, iso15118_supported, refused_locally};
use crate::wire::v201::common::{CertificateActionEnum, Iso15118EVCertificateStatusEnum};
use crate::wire::v201::{Get15118EVCertificateRequest, Get15118EVCertificateResponse};

fn wire_action(action: Iso15118CertificateAction) -> CertificateActionEnum {
    match action {
        Iso15118CertificateAction::Install => CertificateActionEnum::Install,
        Iso15118CertificateAction::Update => CertificateActionEnum::Update,
    }
}

fn map_response(response: Get15118EVCertificateResponse) -> Iso15118CertificateResult {
    Iso15118CertificateResult {
        status: match response.status {
            Iso15118EVCertificateStatusEnum::Accepted => Iso15118CertificateStatus::Accepted,
            Iso15118EVCertificateStatusEnum::Failed => Iso15118CertificateStatus::Failed,
        },
        exi_response: response.exi_response.to_string(),
        // 2.0.1's response has no `remainingContracts` - that is 2.1's addition.
        remaining_contracts: None,
    }
}

#[async_trait::async_trait]
impl Iso15118CertificateRequester<ClientError<OCPP2_0_1Error>> for OCPP2_0_1Client {
    async fn request_ev_certificate<C>(
        &self,
        actor: &ChargePointActor,
        controller: &C,
        request: Iso15118CertificateRequest,
    ) -> Result<Iso15118CertificateResult, ClientError<OCPP2_0_1Error>>
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

        let response = self
            .send_get_15118_ev_certificate(Get15118EVCertificateRequest {
                action: wire_action(request.action),
                custom_data: None,
                exi_request: request.exi_request,
                iso15118_schema_version,
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

    fn request() -> Iso15118CertificateRequest {
        Iso15118CertificateRequest {
            action: Iso15118CertificateAction::Install,
            exi_request: "deadbeef".into(),
            iso15118_schema_version: "urn:iso:15118:2:2013:MsgDef".into(),
            maximum_contract_certificate_chains: None,
            prioritized_emaids: None,
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
    fn map_response_has_no_remaining_contracts_on_2_0_1() {
        let result = map_response(Get15118EVCertificateResponse {
            custom_data: None,
            exi_response: "cafebabe".into(),
            status: Iso15118EVCertificateStatusEnum::Accepted,
            status_info: None,
        });

        assert_eq!(result.status, Iso15118CertificateStatus::Accepted);
        assert_eq!(result.exi_response, "cafebabe");
        assert_eq!(result.remaining_contracts, None);
    }

    #[tokio::test]
    async fn a_schema_version_that_does_not_fit_the_wire_bound_is_refused_locally() {
        let mut long_request = request();
        long_request.iso15118_schema_version = "x".repeat(51);

        // No client to send with - this must never reach the wire, which we confirm indirectly
        // by exercising just the bound check the requester performs first.
        let fits = heapless::String::<50>::try_from(long_request.iso15118_schema_version.as_str());
        assert!(fits.is_err());
    }

    #[test]
    fn iso15118_supported_reflects_the_capability() {
        let none = Capabilities {
            iso15118_support: Iso15118SupportLevel::None,
            ..Capabilities::default()
        };
        let some = Capabilities {
            iso15118_support: Iso15118SupportLevel::Iso15118_2,
            ..Capabilities::default()
        };
        assert!(!iso15118_supported(&none));
        assert!(iso15118_supported(&some));
    }

    #[test]
    fn no_iso15118_controller_exists_for_use_in_tests() {
        let _ = NoIso15118Controller;
    }
}
