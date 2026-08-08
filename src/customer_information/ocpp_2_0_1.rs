//! OCPP 2.0.1 wire adapter for `CustomerInformation`/`NotifyCustomerInformation` (B5.5).

use alloc::string::{String, ToString};

use crate::wire::v201::common::{CustomerInformationStatusEnum, IdTokenEnum};
use crate::wire::v201::{
    CustomerInformationRequest, CustomerInformationResponse, NotifyCustomerInformationRequest,
};
use ocpp_client::ocpp_2_0_1::OCPP2_0_1Client;

use crate::actor::ChargePointActor;
use crate::customer_information::{
    CustomerInformationHandler, CustomerInformationNotifier, CustomerInformationOutcome,
    CustomerInformationQuery, CustomerInformationQueue, handle_customer_information,
};
use crate::state::{IdToken, IdTokenKind};

/// 2.0.1's `IdTokenEnumType` has no `DirectPayment`/`EVCCID`/`Vin` variant, so this is a total,
/// lossless mapping (every wire variant has a matching internal kind) - the same reach
/// `crate::remote_control`'s own `ocpp_2_0_1` adapter has for the reverse direction.
fn map_id_token_kind(kind: IdTokenEnum) -> IdTokenKind {
    match kind {
        IdTokenEnum::Central => IdTokenKind::Central,
        IdTokenEnum::EMAID => IdTokenKind::EMAID,
        IdTokenEnum::ISO14443 => IdTokenKind::ISO14443,
        IdTokenEnum::ISO15693 => IdTokenKind::ISO15693,
        IdTokenEnum::KeyCode => IdTokenKind::KeyCode,
        IdTokenEnum::Local => IdTokenKind::Local,
        IdTokenEnum::MacAddress => IdTokenKind::MacAddress,
        IdTokenEnum::NoAuthorization => IdTokenKind::NoAuthorization,
    }
}

fn map_id_token(id_token: &crate::wire::v201::common::IdToken) -> IdToken {
    IdToken {
        value: id_token.id_token.to_string(),
        kind: map_id_token_kind(id_token.r#type.clone()),
    }
}

/// Resolves a `CustomerInformationRequest` to this crate's [`CustomerInformationQuery`]. Only
/// `idToken` is resolvable - see the module docs on why `customerIdentifier`/
/// `customerCertificate` cannot be searched against this crate's state.
fn map_request(request: &CustomerInformationRequest) -> CustomerInformationQuery {
    if request.id_token.is_none()
        && (request.customer_identifier.is_some() || request.customer_certificate.is_some())
    {
        tracing::warn!(
            request_id = request.request_id,
            "CustomerInformation named a customerIdentifier/customerCertificate this crate \
             cannot resolve, and no idToken - answering Invalid"
        );
    }
    CustomerInformationQuery {
        request_id: request.request_id,
        report: request.report,
        clear: request.clear,
        id_token: request.id_token.as_ref().map(map_id_token),
    }
}

fn map_outcome(outcome: CustomerInformationOutcome) -> CustomerInformationStatusEnum {
    match outcome {
        CustomerInformationOutcome::Accepted => CustomerInformationStatusEnum::Accepted,
        CustomerInformationOutcome::Rejected => CustomerInformationStatusEnum::Rejected,
        CustomerInformationOutcome::Invalid => CustomerInformationStatusEnum::Invalid,
    }
}

/// Wraps an [`OCPP2_0_1Client`] to answer `CustomerInformation` and send
/// `NotifyCustomerInformation`. `NotifyCustomerInformation`'s `generatedAt` is supplied per call
/// by [`crate::customer_information::run_customer_information_requests`] rather than sourced from
/// a `Clock` here, so this adapter needs nothing beyond the client itself.
pub struct Ocpp2_0_1CustomerInformationHandler {
    client: OCPP2_0_1Client,
}

impl Ocpp2_0_1CustomerInformationHandler {
    /// Wraps `client`.
    pub fn new(client: OCPP2_0_1Client) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl CustomerInformationHandler for Ocpp2_0_1CustomerInformationHandler {
    async fn register_customer_information_handler(
        &self,
        _actor: ChargePointActor,
        queue: CustomerInformationQueue,
    ) {
        self.client
            .on_customer_information(move |request: CustomerInformationRequest, _client| {
                let queue = queue.clone();
                async move {
                    let outcome = handle_customer_information(&queue, map_request(&request));
                    Ok(CustomerInformationResponse {
                        custom_data: None,
                        status: map_outcome(outcome),
                        status_info: None,
                    })
                }
            })
            .await;
    }
}

#[async_trait::async_trait]
impl CustomerInformationNotifier for Ocpp2_0_1CustomerInformationHandler {
    type Error = ocpp_client::ClientError<ocpp_client::ocpp_2_0_1::OCPP2_0_1Error>;

    async fn notify_customer_information(
        &self,
        request_id: i64,
        seq_no: i64,
        tbc: bool,
        generated_at: chrono::DateTime<chrono::Utc>,
        data: String,
    ) -> Result<(), Self::Error> {
        self.client
            .send_notify_customer_information(NotifyCustomerInformationRequest {
                custom_data: None,
                data: heapless::String::try_from(data.as_str()).unwrap_or_default(),
                generated_at: generated_at.into(),
                request_id,
                seq_no,
                tbc: Some(tbc),
            })
            .await
            .map(|_| ())
    }
}

/// The `std` convenience: a bare [`OCPP2_0_1Client`] handles this block directly.
#[cfg(feature = "std")]
mod std_impls {
    use super::*;

    #[async_trait::async_trait]
    impl CustomerInformationHandler for OCPP2_0_1Client {
        async fn register_customer_information_handler(
            &self,
            actor: ChargePointActor,
            queue: CustomerInformationQueue,
        ) {
            Ocpp2_0_1CustomerInformationHandler::new(self.clone())
                .register_customer_information_handler(actor, queue)
                .await;
        }
    }

    #[async_trait::async_trait]
    impl CustomerInformationNotifier for OCPP2_0_1Client {
        type Error = ocpp_client::ClientError<ocpp_client::ocpp_2_0_1::OCPP2_0_1Error>;

        async fn notify_customer_information(
            &self,
            request_id: i64,
            seq_no: i64,
            tbc: bool,
            generated_at: chrono::DateTime<chrono::Utc>,
            data: String,
        ) -> Result<(), Self::Error> {
            Ocpp2_0_1CustomerInformationHandler::new(self.clone())
                .notify_customer_information(request_id, seq_no, tbc, generated_at, data)
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire_id_token(value: &str, kind: IdTokenEnum) -> crate::wire::v201::common::IdToken {
        crate::wire::v201::common::IdToken {
            additional_info: None,
            custom_data: None,
            id_token: heapless::String::try_from(value).unwrap(),
            r#type: kind,
        }
    }

    #[test]
    fn every_wire_kind_maps_onto_this_crates_kinds() {
        assert_eq!(
            map_id_token_kind(IdTokenEnum::Central),
            IdTokenKind::Central
        );
        assert_eq!(map_id_token_kind(IdTokenEnum::EMAID), IdTokenKind::EMAID);
        assert_eq!(
            map_id_token_kind(IdTokenEnum::ISO14443),
            IdTokenKind::ISO14443
        );
        assert_eq!(
            map_id_token_kind(IdTokenEnum::ISO15693),
            IdTokenKind::ISO15693
        );
        assert_eq!(
            map_id_token_kind(IdTokenEnum::KeyCode),
            IdTokenKind::KeyCode
        );
        assert_eq!(map_id_token_kind(IdTokenEnum::Local), IdTokenKind::Local);
        assert_eq!(
            map_id_token_kind(IdTokenEnum::MacAddress),
            IdTokenKind::MacAddress
        );
        assert_eq!(
            map_id_token_kind(IdTokenEnum::NoAuthorization),
            IdTokenKind::NoAuthorization
        );
    }

    #[test]
    fn a_request_naming_an_id_token_resolves_it() {
        let request = CustomerInformationRequest {
            clear: true,
            custom_data: None,
            customer_certificate: None,
            customer_identifier: None,
            id_token: Some(wire_id_token("ABC123", IdTokenEnum::ISO14443)),
            report: true,
            request_id: 9,
        };

        let query = map_request(&request);

        assert_eq!(query.request_id, 9);
        assert!(query.report);
        assert!(query.clear);
        assert_eq!(
            query.id_token,
            Some(IdToken {
                value: "ABC123".into(),
                kind: IdTokenKind::ISO14443,
            })
        );
    }

    #[test]
    fn a_request_naming_only_a_customer_certificate_resolves_to_nothing() {
        let request = CustomerInformationRequest {
            clear: false,
            custom_data: None,
            customer_certificate: Some(crate::wire::v201::common::CertificateHashData {
                custom_data: None,
                hash_algorithm: crate::wire::v201::common::HashAlgorithmEnum::SHA256,
                issuer_key_hash: heapless::String::try_from("issuer-key-hash").unwrap(),
                issuer_name_hash: heapless::String::try_from("issuer-name-hash").unwrap(),
                serial_number: heapless::String::try_from("1").unwrap(),
            }),
            customer_identifier: None,
            id_token: None,
            report: true,
            request_id: 1,
        };

        assert_eq!(map_request(&request).id_token, None);
    }

    #[test]
    fn every_outcome_maps_onto_the_wire_status() {
        assert_eq!(
            map_outcome(CustomerInformationOutcome::Accepted),
            CustomerInformationStatusEnum::Accepted
        );
        assert_eq!(
            map_outcome(CustomerInformationOutcome::Rejected),
            CustomerInformationStatusEnum::Rejected
        );
        assert_eq!(
            map_outcome(CustomerInformationOutcome::Invalid),
            CustomerInformationStatusEnum::Invalid
        );
    }
}
