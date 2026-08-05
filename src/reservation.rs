//! Reservation functional block: CSMS-initiated `ReserveNow`/`CancelReservation`. See
//! `docs/ROADMAP.md` §8.

use crate::actor::ChargePointActor;
use crate::state::{
    ChargePointEvent, ChargePointState, ConnectorEvent, ConnectorState, EvseEvent, IdToken,
    Reservation, ReservationId,
};
use alloc::boxed::Box;

/// The outcome of a CSMS-initiated `ReserveNow` request, matching OCPP's
/// `ReserveNowStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReserveNowOutcome {
    /// The connector was reserved.
    Accepted,
    /// The only connector(s) that could have been reserved are currently faulted.
    Faulted,
    /// The only connector(s) that could have been reserved are currently occupied (a cable is
    /// connected, or a transaction is in progress).
    Occupied,
    /// `evse_id` doesn't address an EVSE on this charge point.
    Rejected,
    /// The only connector(s) that could have been reserved are currently unavailable.
    Unavailable,
}

/// Handles a CSMS-initiated `ReserveNow` request against `actor`: finds an `Available` connector
/// on `evse_id` - or, if `evse_id` is `None`, the first `Available` connector on any EVSE - and
/// reserves it. Rejects if `evse_id` is out of range; otherwise, if no connector is currently
/// `Available`, reports why (in priority order: `Faulted`, `Unavailable`, else `Occupied`) based
/// on the states of the connectors that were considered.
///
/// OCPP's `ReserveNow` addresses at most an EVSE (`evseId` is optional and has no `connectorId`
/// counterpart) - unlike `RequestStartTransaction`/`ChangeAvailability`, there is no way for the
/// CSMS to target one specific connector directly.
pub async fn handle_reserve_now(
    actor: &ChargePointActor,
    evse_id: Option<usize>,
    reservation_id: ReservationId,
    id_token: IdToken,
) -> ReserveNowOutcome {
    let state = actor.state();
    let Some(considered) = considered_connectors(&state, evse_id) else {
        return ReserveNowOutcome::Rejected;
    };
    if considered.is_empty() {
        return ReserveNowOutcome::Rejected;
    }

    let Some((evse_id, connector_id)) = considered
        .iter()
        .find(|(_, _, connector)| *connector == ConnectorState::Available)
        .map(|(evse_id, connector_id, _)| (*evse_id, *connector_id))
    else {
        return unavailable_outcome(&considered);
    };

    let _ = actor
        .send(ChargePointEvent::Evse {
            evse_id,
            event: EvseEvent::Connector {
                connector_id,
                event: ConnectorEvent::Reserved(Reservation {
                    id: reservation_id,
                    id_token,
                }),
            },
        })
        .await;

    ReserveNowOutcome::Accepted
}

/// Every `(evse_id, connector_id, ConnectorState)` that a `ReserveNow` targeting `evse_id` (or,
/// if `None`, the whole charge point) could reserve. `None` if `evse_id` is out of range.
fn considered_connectors(
    state: &ChargePointState,
    evse_id: Option<usize>,
) -> Option<alloc::vec::Vec<(usize, usize, ConnectorState)>> {
    match evse_id {
        Some(evse_id) => {
            let evse = state.evses.get(evse_id)?;
            Some(
                evse.connectors
                    .iter()
                    .enumerate()
                    .map(|(connector_id, connector)| (evse_id, connector_id, *connector))
                    .collect(),
            )
        }
        None => Some(
            state
                .evses
                .iter()
                .enumerate()
                .flat_map(|(evse_id, evse)| {
                    evse.connectors
                        .iter()
                        .enumerate()
                        .map(move |(connector_id, connector)| (evse_id, connector_id, *connector))
                })
                .collect(),
        ),
    }
}

/// Picks the most informative rejection reason when no considered connector is `Available`.
fn unavailable_outcome(considered: &[(usize, usize, ConnectorState)]) -> ReserveNowOutcome {
    if considered.iter().any(|(_, _, connector)| {
        matches!(
            connector,
            ConnectorState::Faulted | ConnectorState::FaultedSafe
        )
    }) {
        ReserveNowOutcome::Faulted
    } else if considered
        .iter()
        .any(|(_, _, connector)| *connector == ConnectorState::Unavailable)
    {
        ReserveNowOutcome::Unavailable
    } else {
        ReserveNowOutcome::Occupied
    }
}

/// Registers this charge point's inbound `ReserveNow` handling with the CSMS connection.
/// Implemented per protocol version (see the `ocpp_2_1` module), mirroring
/// [`crate::remote_control::UnlockConnectorHandler`].
#[async_trait::async_trait]
pub trait ReserveNowHandler {
    async fn register_reserve_now_handler(&self, actor: ChargePointActor);
}

/// The outcome of a CSMS-initiated `CancelReservation` request, matching OCPP's
/// `CancelReservationStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReservationOutcome {
    Accepted,
    Rejected,
}

/// Handles a CSMS-initiated `CancelReservation` request against `actor`: finds the connector
/// whose active reservation is `reservation_id` and, if found, cancels it. Rejects an unknown
/// `reservation_id`.
pub async fn handle_cancel_reservation(
    actor: &ChargePointActor,
    reservation_id: ReservationId,
) -> CancelReservationOutcome {
    let Some((evse_id, connector_id)) = find_reservation(&actor.state(), reservation_id) else {
        return CancelReservationOutcome::Rejected;
    };

    let _ = actor
        .send(ChargePointEvent::Evse {
            evse_id,
            event: EvseEvent::Connector {
                connector_id,
                event: ConnectorEvent::ReservationCancelled,
            },
        })
        .await;

    CancelReservationOutcome::Accepted
}

/// The connector (if any) whose active reservation is `reservation_id`.
fn find_reservation(
    state: &ChargePointState,
    reservation_id: ReservationId,
) -> Option<(usize, usize)> {
    state.evses.iter().enumerate().find_map(|(evse_id, evse)| {
        evse.reservations
            .iter()
            .position(|reservation| reservation.as_ref().is_some_and(|r| r.id == reservation_id))
            .map(|connector_id| (evse_id, connector_id))
    })
}

/// Registers this charge point's inbound `CancelReservation` handling with the CSMS connection.
/// Implemented per protocol version (see the `ocpp_2_1` module), mirroring
/// [`ReserveNowHandler`].
#[async_trait::async_trait]
pub trait CancelReservationHandler {
    async fn register_cancel_reservation_handler(&self, actor: ChargePointActor);
}

#[cfg(test)]
mod tests {
    use super::{
        handle_cancel_reservation, handle_reserve_now, CancelReservationOutcome, ReserveNowOutcome,
    };
    use crate::actor::ChargePointActor;
    use crate::executor::TokioExecutor;
    use crate::state::{
        ChargePointEvent, ConnectorEvent, ConnectorState, EvseEvent, IdToken, IdTokenKind,
        ReservationId,
    };

    fn test_id_token() -> IdToken {
        IdToken {
            value: "04A224B2".into(),
            kind: IdTokenKind::ISO14443,
        }
    }

    #[tokio::test]
    async fn reserving_an_available_connector_on_a_given_evse_succeeds() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome = handle_reserve_now(&actor, Some(0), ReservationId(1), test_id_token()).await;

        assert_eq!(outcome, ReserveNowOutcome::Accepted);
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Reserved
        );
    }

    #[tokio::test]
    async fn no_evse_id_reserves_the_first_available_connector_on_any_evse() {
        let actor = ChargePointActor::spawn([1, 1], &TokioExecutor);
        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::SetUnavailable,
                },
            })
            .await
            .unwrap();

        let outcome = handle_reserve_now(&actor, None, ReservationId(1), test_id_token()).await;

        assert_eq!(outcome, ReserveNowOutcome::Accepted);
        assert_eq!(
            actor.state().evses[1].connectors[0],
            ConnectorState::Reserved
        );
    }

    #[tokio::test]
    async fn an_unknown_evse_is_rejected() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome = handle_reserve_now(&actor, Some(5), ReservationId(1), test_id_token()).await;

        assert_eq!(outcome, ReserveNowOutcome::Rejected);
    }

    #[tokio::test]
    async fn an_occupied_connector_reports_occupied() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::CableConnected,
                },
            })
            .await
            .unwrap();

        let outcome = handle_reserve_now(&actor, Some(0), ReservationId(1), test_id_token()).await;

        assert_eq!(outcome, ReserveNowOutcome::Occupied);
    }

    #[tokio::test]
    async fn an_unavailable_connector_reports_unavailable() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        actor
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::SetUnavailable,
                },
            })
            .await
            .unwrap();

        let outcome = handle_reserve_now(&actor, Some(0), ReservationId(1), test_id_token()).await;

        assert_eq!(outcome, ReserveNowOutcome::Unavailable);
    }

    #[tokio::test]
    async fn cancelling_a_known_reservation_frees_the_connector() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        handle_reserve_now(&actor, Some(0), ReservationId(1), test_id_token()).await;

        let outcome = handle_cancel_reservation(&actor, ReservationId(1)).await;

        assert_eq!(outcome, CancelReservationOutcome::Accepted);
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Available
        );
    }

    #[tokio::test]
    async fn cancelling_an_unknown_reservation_is_rejected() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome = handle_cancel_reservation(&actor, ReservationId(1)).await;

        assert_eq!(outcome, CancelReservationOutcome::Rejected);
    }
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use super::{
        handle_cancel_reservation, handle_reserve_now, CancelReservationHandler,
        CancelReservationOutcome, ReserveNowHandler, ReserveNowOutcome,
    };
    use crate::actor::ChargePointActor;
    use crate::state::{IdToken, IdTokenKind, ReservationId};
    use alloc::boxed::Box;
    use alloc::string::ToString;
    use ocpp_client::ocpp_2_1::OCPP2_1Client;
    use ocpp_client::ocpp_types::v21::common::{CancelReservationStatusEnum, ReserveNowStatusEnum};
    use ocpp_client::ocpp_types::v21::{
        CancelReservationResponse, ReserveNowRequest, ReserveNowResponse,
    };

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

    fn map_id_token(id_token: &ocpp_client::ocpp_types::v21::common::IdToken) -> IdToken {
        IdToken {
            value: id_token.id_token.to_string(),
            kind: map_id_token_kind(id_token.r#type.as_str()),
        }
    }

    pub(super) fn map_reserve_now_outcome(outcome: ReserveNowOutcome) -> ReserveNowStatusEnum {
        match outcome {
            ReserveNowOutcome::Accepted => ReserveNowStatusEnum::Accepted,
            ReserveNowOutcome::Faulted => ReserveNowStatusEnum::Faulted,
            ReserveNowOutcome::Occupied => ReserveNowStatusEnum::Occupied,
            ReserveNowOutcome::Rejected => ReserveNowStatusEnum::Rejected,
            ReserveNowOutcome::Unavailable => ReserveNowStatusEnum::Unavailable,
        }
    }

    pub(super) fn map_cancel_reservation_outcome(
        outcome: CancelReservationOutcome,
    ) -> CancelReservationStatusEnum {
        match outcome {
            CancelReservationOutcome::Accepted => CancelReservationStatusEnum::Accepted,
            CancelReservationOutcome::Rejected => CancelReservationStatusEnum::Rejected,
        }
    }

    /// A negative wire `evseId` can't address an EVSE - treated the same as an out-of-range one,
    /// without needing to consult the actor.
    fn parse_evse_id(request: &ReserveNowRequest) -> Result<Option<usize>, ()> {
        match request.evse_id {
            None => Ok(None),
            Some(evse_id) => usize::try_from(evse_id).map(Some).map_err(|_| ()),
        }
    }

    #[async_trait::async_trait]
    impl ReserveNowHandler for OCPP2_1Client {
        async fn register_reserve_now_handler(&self, actor: ChargePointActor) {
            self.on_reserve_now(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let outcome = match parse_evse_id(&request) {
                        Ok(evse_id) => {
                            handle_reserve_now(
                                &actor,
                                evse_id,
                                ReservationId(request.id),
                                map_id_token(&request.id_token),
                            )
                            .await
                        }
                        Err(()) => ReserveNowOutcome::Rejected,
                    };
                    Ok(ReserveNowResponse {
                        custom_data: None,
                        status: map_reserve_now_outcome(outcome),
                        status_info: None,
                    })
                }
            })
            .await;
        }
    }

    #[async_trait::async_trait]
    impl CancelReservationHandler for OCPP2_1Client {
        async fn register_cancel_reservation_handler(&self, actor: ChargePointActor) {
            self.on_cancel_reservation(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let outcome =
                        handle_cancel_reservation(&actor, ReservationId(request.reservation_id))
                            .await;
                    Ok(CancelReservationResponse {
                        custom_data: None,
                        status: map_cancel_reservation_outcome(outcome),
                        status_info: None,
                    })
                }
            })
            .await;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_reserve_now_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Accepted),
                ReserveNowStatusEnum::Accepted
            );
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Faulted),
                ReserveNowStatusEnum::Faulted
            );
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Occupied),
                ReserveNowStatusEnum::Occupied
            );
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Rejected),
                ReserveNowStatusEnum::Rejected
            );
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Unavailable),
                ReserveNowStatusEnum::Unavailable
            );
        }

        #[test]
        fn every_cancel_reservation_outcome_maps_to_the_matching_wire_status() {
            assert_eq!(
                map_cancel_reservation_outcome(CancelReservationOutcome::Accepted),
                CancelReservationStatusEnum::Accepted
            );
            assert_eq!(
                map_cancel_reservation_outcome(CancelReservationOutcome::Rejected),
                CancelReservationStatusEnum::Rejected
            );
        }

        fn wire_id_token() -> ocpp_client::ocpp_types::v21::common::IdToken {
            ocpp_client::ocpp_types::v21::common::IdToken {
                additional_info: None,
                id_token: heapless::String::try_from("04A224B2").unwrap(),
                r#type: heapless::String::try_from("ISO14443").unwrap(),
                custom_data: None,
            }
        }

        #[test]
        fn a_wire_id_token_maps_to_the_internal_representation() {
            let mapped = map_id_token(&wire_id_token());
            assert_eq!(mapped.value, "04A224B2");
            assert_eq!(mapped.kind, IdTokenKind::ISO14443);
        }

        fn request(evse_id: Option<i64>) -> ReserveNowRequest {
            ReserveNowRequest {
                connector_type: None,
                custom_data: None,
                evse_id,
                expiry_date_time: "2026-01-01T00:00:00Z".to_string(),
                group_id_token: None,
                id: 1,
                id_token: wire_id_token(),
            }
        }

        #[test]
        fn no_evse_id_parses_to_none() {
            assert_eq!(parse_evse_id(&request(None)), Ok(None));
        }

        #[test]
        fn a_valid_evse_id_parses() {
            assert_eq!(parse_evse_id(&request(Some(1))), Ok(Some(1)));
        }

        #[test]
        fn a_negative_evse_id_fails_to_parse() {
            assert_eq!(parse_evse_id(&request(Some(-1))), Err(()));
        }
    }
}
