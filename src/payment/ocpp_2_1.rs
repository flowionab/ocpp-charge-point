//! The OCPP 2.1 projection: `NotifySettlement`, `NotifyWebPaymentStarted` and
//! `VatNumberValidation`. Payment does not exist before 2.1, so this is the only version adapter
//! this block has.

use super::{
    MerchantAddress, PaymentNotifier, SettlementEvent, SettlementReceipt, SettlementStatus,
    VatValidationOutcome, VatValidationStatus,
};
use crate::wire::v21::common::{
    Address as WireAddress, GenericStatusEnum, PaymentStatusEnum, StatusInfo as WireStatusInfo,
};
use crate::wire::v21::{
    NotifySettlementRequest, NotifySettlementResponse, NotifyWebPaymentStartedRequest,
    VatNumberValidationRequest, VatNumberValidationResponse,
};
use alloc::boxed::Box;
use alloc::string::ToString;
use ocpp_client::ocpp_2_1::OCPP2_1Client;

/// Truncates/bounds `value` to fit a `heapless::String<N>`, mirroring
/// `crate::battery_swap::ocpp_2_1`'s helper of the same name - duplicated per this crate's
/// small-helper convention rather than shared across modules.
fn bounded_string<const N: usize>(value: &str) -> heapless::String<N> {
    let mut end = value.len().min(N);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    heapless::String::try_from(&value[..end]).expect("truncated to fit the wire bound")
}

fn map_settlement_status(status: SettlementStatus) -> PaymentStatusEnum {
    match status {
        SettlementStatus::Settled => PaymentStatusEnum::Settled,
        SettlementStatus::Canceled => PaymentStatusEnum::Canceled,
        SettlementStatus::Rejected => PaymentStatusEnum::Rejected,
        SettlementStatus::Failed => PaymentStatusEnum::Failed,
    }
}

fn wire_address(address: &MerchantAddress) -> WireAddress {
    WireAddress {
        address1: bounded_string(&address.address1),
        address2: address.address2.as_deref().map(bounded_string),
        city: bounded_string(&address.city),
        country: bounded_string(&address.country),
        custom_data: None,
        name: bounded_string(&address.name),
        postal_code: address.postal_code.as_deref().map(bounded_string),
    }
}

fn map_address(address: &WireAddress) -> MerchantAddress {
    MerchantAddress {
        name: address.name.to_string(),
        address1: address.address1.to_string(),
        address2: address.address2.as_ref().map(ToString::to_string),
        city: address.city.to_string(),
        postal_code: address.postal_code.as_ref().map(ToString::to_string),
        country: address.country.to_string(),
    }
}

fn build_notify_settlement_request(event: &SettlementEvent) -> NotifySettlementRequest {
    NotifySettlementRequest {
        custom_data: None,
        psp_ref: bounded_string(&event.psp_ref),
        receipt_id: event.receipt_id.as_deref().map(bounded_string),
        receipt_url: event.receipt_url.clone(),
        settlement_amount: event.settlement_amount.parse().unwrap_or(0.0),
        settlement_time: event.settlement_time.into(),
        status: map_settlement_status(event.status),
        status_info: event.status_info.as_deref().map(bounded_string),
        transaction_id: event.transaction_id.as_deref().map(bounded_string),
        vat_company: event.vat_company.as_ref().map(wire_address),
        vat_number: event.vat_number.as_deref().map(bounded_string),
    }
}

fn map_notify_settlement_response(response: NotifySettlementResponse) -> SettlementReceipt {
    SettlementReceipt {
        receipt_id: response.receipt_id.as_ref().map(ToString::to_string),
        receipt_url: response.receipt_url,
    }
}

fn build_vat_number_validation_request(
    vat_number: &str,
    evse_id: Option<usize>,
) -> VatNumberValidationRequest {
    VatNumberValidationRequest {
        custom_data: None,
        // Negative EVSE ids don't exist on this crate's side (`evse_id` is a `usize`); a charge
        // point with more EVSEs than `i64::MAX` is not a real embedded target - mirrors
        // `crate::battery_swap::ocpp_2_1::wire_battery_data`'s `evse_id` cast.
        evse_id: evse_id.map(|id| id as i64),
        vat_number: bounded_string(vat_number),
    }
}

fn map_vat_number_validation_response(
    response: VatNumberValidationResponse,
) -> VatValidationOutcome {
    VatValidationOutcome {
        status: match response.status {
            GenericStatusEnum::Accepted => VatValidationStatus::Accepted,
            GenericStatusEnum::Rejected => VatValidationStatus::Rejected,
        },
        company: response.company.as_ref().map(map_address),
        status_info: response
            .status_info
            .as_ref()
            .map(|info: &WireStatusInfo| info.reason_code.to_string()),
    }
}

#[async_trait::async_trait]
impl PaymentNotifier for OCPP2_1Client {
    type Error = ocpp_client::ClientError<ocpp_client::ocpp_2_1::OCPP2_1Error>;

    async fn notify_settlement(
        &self,
        event: &SettlementEvent,
    ) -> Result<SettlementReceipt, Self::Error> {
        self.send_notify_settlement(build_notify_settlement_request(event))
            .await
            .map(map_notify_settlement_response)
    }

    async fn notify_web_payment_started(
        &self,
        evse_id: usize,
        timeout_secs: u32,
    ) -> Result<(), Self::Error> {
        self.send_notify_web_payment_started(NotifyWebPaymentStartedRequest {
            custom_data: None,
            // See `build_vat_number_validation_request`'s docs on the same cast.
            evse_id: evse_id as i64,
            timeout: timeout_secs as i64,
        })
        .await
        .map(|_| ())
    }

    async fn validate_vat_number(
        &self,
        vat_number: &str,
        evse_id: Option<usize>,
    ) -> Result<VatValidationOutcome, Self::Error> {
        self.send_vat_number_validation(build_vat_number_validation_request(vat_number, evse_id))
            .await
            .map(map_vat_number_validation_response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payment::SettlementEvent;

    fn settlement_event() -> SettlementEvent {
        SettlementEvent::new(
            "psp-ref-1",
            Some("receipt-1".into()),
            None,
            42.50,
            chrono::Utc::now(),
            SettlementStatus::Settled,
            None,
            Some("txn-1".into()),
            None,
            Some("SE556677889901".into()),
        )
    }

    #[test]
    fn the_outbound_request_carries_the_settlement_amount_and_status() {
        let event = settlement_event();

        let request = build_notify_settlement_request(&event);

        assert!((request.settlement_amount - 42.50).abs() < f64::EPSILON);
        assert_eq!(request.status, PaymentStatusEnum::Settled);
        assert_eq!(request.psp_ref.as_str(), "psp-ref-1");
        assert_eq!(request.vat_number.as_deref(), Some("SE556677889901"));
    }

    #[test]
    fn every_settlement_status_maps_to_its_wire_enum() {
        assert_eq!(
            map_settlement_status(SettlementStatus::Settled),
            PaymentStatusEnum::Settled
        );
        assert_eq!(
            map_settlement_status(SettlementStatus::Canceled),
            PaymentStatusEnum::Canceled
        );
        assert_eq!(
            map_settlement_status(SettlementStatus::Rejected),
            PaymentStatusEnum::Rejected
        );
        assert_eq!(
            map_settlement_status(SettlementStatus::Failed),
            PaymentStatusEnum::Failed
        );
    }

    #[test]
    fn a_merchant_address_round_trips_through_the_wire_shape() {
        let address = MerchantAddress {
            name: "Acme".into(),
            address1: "1 Main St".into(),
            address2: None,
            city: "Springfield".into(),
            postal_code: Some("12345".into()),
            country: "USA".into(),
        };

        let wire = wire_address(&address);
        let back = map_address(&wire);

        assert_eq!(back, address);
    }

    #[test]
    fn the_vat_validation_request_carries_the_scoping_evse_id() {
        let request = build_vat_number_validation_request("SE556677889901", Some(3));

        assert_eq!(request.evse_id, Some(3));
        assert_eq!(request.vat_number.as_str(), "SE556677889901");
    }

    #[test]
    fn a_vat_validation_request_with_no_evse_scope_carries_none() {
        let request = build_vat_number_validation_request("SE556677889901", None);

        assert_eq!(request.evse_id, None);
    }

    #[test]
    fn every_generic_status_maps_to_its_validation_outcome() {
        let accepted = VatNumberValidationResponse {
            company: None,
            custom_data: None,
            evse_id: None,
            status: GenericStatusEnum::Accepted,
            status_info: None,
            vat_number: bounded_string("SE556677889901"),
        };
        let rejected = VatNumberValidationResponse {
            status: GenericStatusEnum::Rejected,
            ..accepted.clone()
        };

        assert_eq!(
            map_vat_number_validation_response(accepted).status,
            VatValidationStatus::Accepted
        );
        assert_eq!(
            map_vat_number_validation_response(rejected).status,
            VatValidationStatus::Rejected
        );
    }
}
