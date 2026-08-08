//! The OCPP 2.1 projection: `RequestBatterySwap` (inbound) and `BatterySwap` (outbound). Battery
//! swap does not exist before 2.1, so this is the only version adapter this block has.

use super::{BatterySwapNotifier, RequestBatterySwapHandler, handle_request_battery_swap};
use crate::hardware::BatterySwapStation;
use crate::state::{
    BatteryData, BatterySwapEvent, BatterySwapEventKind, BatterySwapRequestId, IdToken, IdTokenKind,
};
use crate::wire::v21::common::{
    BatteryData as WireBatteryData, BatterySwapEventEnum, GenericStatusEnum, IdToken as WireIdToken,
};
use crate::wire::v21::{BatterySwapRequest, RequestBatterySwapResponse};
use alloc::boxed::Box;
use alloc::string::ToString;
use ocpp_client::ocpp_2_1::OCPP2_1Client;

/// Truncates/bounds `value` to fit a `heapless::String<N>`, mirroring
/// `crate::reservation::ocpp_2_1`'s and `crate::display_message::ocpp_2_1::bounded_string` -
/// duplicated per this crate's small-helper convention rather than shared across modules.
fn bounded_string<const N: usize>(value: &str) -> heapless::String<N> {
    let mut end = value.len().min(N);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    heapless::String::try_from(&value[..end]).expect("truncated to fit the wire bound")
}

fn map_id_token_kind(kind: &str) -> IdTokenKind {
    match kind {
        "Central" => IdTokenKind::Central,
        "DirectPayment" => IdTokenKind::DirectPayment,
        "eMAID" => IdTokenKind::EMAID,
        "EVCCID" => IdTokenKind::EVCCID,
        "ISO14443" => IdTokenKind::ISO14443,
        "ISO15693" => IdTokenKind::ISO15693,
        "KeyCode" => IdTokenKind::KeyCode,
        "Local" => IdTokenKind::Local,
        "MacAddress" => IdTokenKind::MacAddress,
        "NoAuthorization" => IdTokenKind::NoAuthorization,
        _ => IdTokenKind::Vin,
    }
}

fn map_id_token(id_token: &WireIdToken) -> IdToken {
    IdToken {
        value: id_token.id_token.to_string(),
        kind: map_id_token_kind(id_token.r#type.as_str()),
    }
}

/// The wire `type` string for an [`IdTokenKind`], mirroring
/// `crate::authorization::ocpp_2_1::wire_type`.
fn wire_type(kind: IdTokenKind) -> &'static str {
    match kind {
        IdTokenKind::Central => "Central",
        IdTokenKind::DirectPayment => "DirectPayment",
        IdTokenKind::EMAID => "eMAID",
        IdTokenKind::EVCCID => "EVCCID",
        IdTokenKind::ISO14443 => "ISO14443",
        IdTokenKind::ISO15693 => "ISO15693",
        IdTokenKind::KeyCode => "KeyCode",
        IdTokenKind::Local => "Local",
        IdTokenKind::MacAddress => "MacAddress",
        IdTokenKind::NoAuthorization => "NoAuthorization",
        IdTokenKind::Vin => "VIN",
    }
}

fn wire_id_token(id_token: &IdToken) -> WireIdToken {
    WireIdToken {
        additional_info: None,
        id_token: bounded_string(&id_token.value),
        r#type: bounded_string(wire_type(id_token.kind)),
        custom_data: None,
    }
}

fn map_event_kind(kind: BatterySwapEventKind) -> BatterySwapEventEnum {
    match kind {
        BatterySwapEventKind::BatteryIn => BatterySwapEventEnum::BatteryIn,
        BatterySwapEventKind::BatteryOut => BatterySwapEventEnum::BatteryOut,
        BatterySwapEventKind::BatteryOutTimeout => BatterySwapEventEnum::BatteryOutTimeout,
    }
}

fn wire_battery_data(data: &BatteryData) -> WireBatteryData {
    WireBatteryData {
        custom_data: None,
        // Negative slot indices don't exist on this crate's side (`evse_id` is a `usize`); a
        // charge point with more EVSEs than `i64::MAX` is not a real embedded target.
        evse_id: data.evse_id as i64,
        production_date: data.production_date.map(Into::into),
        serial_number: bounded_string(&data.serial_number),
        so_c: data.state_of_charge.parse().unwrap_or(0.0),
        so_h: data.state_of_health.parse().unwrap_or(0.0),
        vendor_info: data.vendor_info.as_deref().map(bounded_string),
    }
}

fn build_battery_swap_request(event: &BatterySwapEvent) -> BatterySwapRequest {
    BatterySwapRequest {
        battery_data: event.battery_data.iter().map(wire_battery_data).collect(),
        custom_data: None,
        event_type: map_event_kind(event.event_type),
        id_token: wire_id_token(&event.id_token),
        request_id: event.request_id.0,
    }
}

#[async_trait::async_trait]
impl RequestBatterySwapHandler for OCPP2_1Client {
    async fn register_request_battery_swap_handler<S>(
        &self,
        actor: crate::actor::ChargePointActor,
        station: S,
    ) where
        S: BatterySwapStation + Send + Sync + 'static,
    {
        let station = alloc::sync::Arc::new(station);
        self.on_request_battery_swap(move |request, _client| {
            let actor = actor.clone();
            let station = station.clone();
            async move {
                let outcome = handle_request_battery_swap(
                    &actor,
                    &*station,
                    BatterySwapRequestId(request.request_id),
                    map_id_token(&request.id_token),
                )
                .await;
                Ok(RequestBatterySwapResponse {
                    custom_data: None,
                    status: match outcome {
                        super::RequestBatterySwapOutcome::Accepted => GenericStatusEnum::Accepted,
                        super::RequestBatterySwapOutcome::Rejected => GenericStatusEnum::Rejected,
                    },
                    status_info: None,
                })
            }
        })
        .await;
    }
}

#[async_trait::async_trait]
impl BatterySwapNotifier for OCPP2_1Client {
    type Error = ocpp_client::ClientError<ocpp_client::ocpp_2_1::OCPP2_1Error>;

    async fn notify_battery_swap(&self, event: &BatterySwapEvent) -> Result<(), Self::Error> {
        self.send_battery_swap(build_battery_swap_request(event))
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::IdTokenKind;

    #[test]
    fn id_token_round_trips_through_the_wire_shape() {
        let id_token = IdToken {
            value: "04A224B2".into(),
            kind: IdTokenKind::ISO14443,
        };

        let wire = wire_id_token(&id_token);
        let back = map_id_token(&wire);

        assert_eq!(back, id_token);
    }

    #[test]
    fn battery_data_carries_evse_id_and_percentages_onto_the_wire() {
        let data = BatteryData::new(2, "SN123", 87.5, 99.0, None, Some("vendor".into()));

        let wire = wire_battery_data(&data);

        assert_eq!(wire.evse_id, 2);
        assert_eq!(wire.serial_number.as_str(), "SN123");
        assert!((wire.so_c - 87.5).abs() < f64::EPSILON);
        assert!((wire.so_h - 99.0).abs() < f64::EPSILON);
        assert_eq!(wire.vendor_info.as_deref(), Some("vendor"));
    }

    #[test]
    fn every_event_kind_maps_to_its_wire_enum() {
        assert_eq!(
            map_event_kind(BatterySwapEventKind::BatteryIn),
            BatterySwapEventEnum::BatteryIn
        );
        assert_eq!(
            map_event_kind(BatterySwapEventKind::BatteryOut),
            BatterySwapEventEnum::BatteryOut
        );
        assert_eq!(
            map_event_kind(BatterySwapEventKind::BatteryOutTimeout),
            BatterySwapEventEnum::BatteryOutTimeout
        );
    }

    #[test]
    fn the_outbound_request_carries_the_correlating_request_id() {
        let event = BatterySwapEvent {
            request_id: BatterySwapRequestId(42),
            event_type: BatterySwapEventKind::BatteryOut,
            id_token: IdToken {
                value: "04A224B2".into(),
                kind: IdTokenKind::ISO14443,
            },
            battery_data: alloc::vec![BatteryData::new(0, "SN1", 50.0, 90.0, None, None)],
        };

        let request = build_battery_swap_request(&event);

        assert_eq!(request.request_id, 42);
        assert_eq!(request.battery_data.len(), 1);
    }
}
