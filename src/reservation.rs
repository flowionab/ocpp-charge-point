//! Reservation functional block: CSMS-initiated `ReserveNow`/`CancelReservation`. See
//! `docs/ROADMAP.md` §8.

use crate::actor::ChargePointActor;
use crate::state::{
    ChargePointEvent, ChargePointState, ConnectorEvent, ConnectorState, EvseEvent, IdToken,
    Reservation, ReservationId,
};
use alloc::boxed::Box;

#[cfg(feature = "ocpp_1_6")]
pub use self::ocpp_1_6::Ocpp1_6ReserveNowHandler;

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
    // C5 (docs/PRODUCTION-ROADMAP.md §5.5): the handler is registered whenever the
    // `reservation` Cargo feature is on, but the hardware may still declare the capability
    // runtime-absent - refuse via the same `Rejected` status a wire-level rejection already
    // uses, per `crate::refusal`'s decision table.
    if !crate::refusal::capability_present(&state.capabilities, "ReserveNow") {
        return ReserveNowOutcome::Rejected;
    }
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
                    // Wiring the wire `expiryDateTime`/`expiry_date` through to here is future
                    // work (see `crate::state::Reservation`'s docs) - `expires_at` today is only
                    // consulted by `persistence::restore_reservations`, so a reservation created
                    // through this handler never expires on its own within a single boot, exactly
                    // as before this field existed.
                    expires_at: None,
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
    /// Registers a `ReserveNow` handler with the CSMS connection that dispatches incoming
    /// requests against `actor`.
    async fn register_reserve_now_handler(&self, actor: ChargePointActor);
}

/// The outcome of a CSMS-initiated `CancelReservation` request, matching OCPP's
/// `CancelReservationStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReservationOutcome {
    /// The reservation was cancelled.
    Accepted,
    /// `reservation_id` doesn't address an active reservation.
    Rejected,
}

/// Handles a CSMS-initiated `CancelReservation` request against `actor`: finds the connector
/// whose active reservation is `reservation_id` and, if found, cancels it. Rejects an unknown
/// `reservation_id`.
pub async fn handle_cancel_reservation(
    actor: &ChargePointActor,
    reservation_id: ReservationId,
) -> CancelReservationOutcome {
    let state = actor.state();
    // C5 (docs/PRODUCTION-ROADMAP.md §5.5): mirrors the capability check in
    // `handle_reserve_now`.
    if !crate::refusal::capability_present(&state.capabilities, "CancelReservation") {
        return CancelReservationOutcome::Rejected;
    }
    let Some((evse_id, connector_id)) = find_reservation(&state, reservation_id) else {
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
    /// Registers a `CancelReservation` handler with the CSMS connection that dispatches incoming
    /// requests to [`handle_cancel_reservation`] against `actor`.
    async fn register_cancel_reservation_handler(&self, actor: ChargePointActor);
}

#[cfg(test)]
mod tests {
    use super::{
        CancelReservationOutcome, ReserveNowOutcome, handle_cancel_reservation, handle_reserve_now,
    };
    use crate::actor::ChargePointActor;
    use crate::executor::TokioExecutor;
    use crate::hardware::Capabilities;
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

    /// Spawns an actor with the `reservation` capability declared present - every test in this
    /// module except the C5 capability-off ones below wants a charge point that actually
    /// supports reservations, mirroring what `ChargePointBuilder`/`setup()` would have declared
    /// from the hardware binding's [`crate::hardware::ChargePoint::capabilities`] in a real
    /// deployment.
    async fn spawn_with_reservation<const N: usize>(evses: [usize; N]) -> ChargePointActor {
        let actor = ChargePointActor::spawn(evses, &TokioExecutor);
        actor
            .send(ChargePointEvent::CapabilitiesDeclared(Capabilities {
                reservation: true,
                ..Default::default()
            }))
            .await
            .unwrap();
        actor
    }

    #[tokio::test]
    async fn reserving_an_available_connector_on_a_given_evse_succeeds() {
        let actor = spawn_with_reservation([1]).await;

        let outcome = handle_reserve_now(&actor, Some(0), ReservationId(1), test_id_token()).await;

        assert_eq!(outcome, ReserveNowOutcome::Accepted);
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Reserved
        );
    }

    #[tokio::test]
    async fn no_evse_id_reserves_the_first_available_connector_on_any_evse() {
        let actor = spawn_with_reservation([1, 1]).await;
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
        let actor = spawn_with_reservation([1]).await;

        let outcome = handle_reserve_now(&actor, Some(5), ReservationId(1), test_id_token()).await;

        assert_eq!(outcome, ReserveNowOutcome::Rejected);
    }

    #[tokio::test]
    async fn an_occupied_connector_reports_occupied() {
        let actor = spawn_with_reservation([1]).await;
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
        let actor = spawn_with_reservation([1]).await;
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
        let actor = spawn_with_reservation([1]).await;
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
        let actor = spawn_with_reservation([1]).await;

        let outcome = handle_cancel_reservation(&actor, ReservationId(1)).await;

        assert_eq!(outcome, CancelReservationOutcome::Rejected);
    }

    // C5 (docs/PRODUCTION-ROADMAP.md §5.5): with the `reservation` capability runtime-absent
    // (the default - see `Capabilities::default`), both handlers must refuse via the same
    // `Rejected` status a normal CALLRESULT already uses, per `crate::refusal`'s decision table -
    // never let a `NotImplemented`/generic error leak out of a handler that's actually registered.

    #[tokio::test]
    async fn reserve_now_is_rejected_when_the_reservation_capability_is_absent() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome = handle_reserve_now(&actor, Some(0), ReservationId(1), test_id_token()).await;

        assert_eq!(outcome, ReserveNowOutcome::Rejected);
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Available,
            "a refused reservation must not mutate connector state"
        );
    }

    #[tokio::test]
    async fn cancel_reservation_is_rejected_when_the_reservation_capability_is_absent() {
        let actor = spawn_with_reservation([1]).await;
        handle_reserve_now(&actor, Some(0), ReservationId(1), test_id_token()).await;
        // Turn the capability back off - the reservation already made must not become
        // cancellable just because it exists.
        actor
            .send(ChargePointEvent::CapabilitiesDeclared(
                Capabilities::default(),
            ))
            .await
            .unwrap();

        let outcome = handle_cancel_reservation(&actor, ReservationId(1)).await;

        assert_eq!(outcome, CancelReservationOutcome::Rejected);
        assert_eq!(
            actor.state().evses[0].connectors[0],
            ConnectorState::Reserved,
            "a refused cancellation must not mutate connector state"
        );
    }
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use super::{
        CancelReservationHandler, CancelReservationOutcome, ReserveNowHandler, ReserveNowOutcome,
        handle_cancel_reservation, handle_reserve_now,
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

/// The OCPP 2.0.1 projection - identical `ReserveNowRequest`/`CancelReservationRequest`/
/// `ReserveNowStatusEnum`/`CancelReservationStatusEnum` wire shapes to 2.1's (2.1's
/// `connector_type` field also loosened from an enum to a free-form string, but this crate always
/// sends `None` for it either way), so this is close to a copy of the 2.1 module - **except**
/// `id_token` mapping, which for 2.0.1 goes through the same closed `IdTokenEnum`
/// [`crate::remote_control::ocpp_2_0_1::map_id_token_kind`] already had to handle for
/// `RequestStartTransaction`'s inbound `id_token` - reused directly here instead of a third copy.
#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1 {
    use super::{
        CancelReservationHandler, CancelReservationOutcome, ReserveNowHandler, ReserveNowOutcome,
        handle_cancel_reservation, handle_reserve_now,
    };
    use crate::actor::ChargePointActor;
    use crate::remote_control::ocpp_2_0_1::map_id_token_kind;
    use crate::state::{IdToken, ReservationId};
    use alloc::boxed::Box;
    use alloc::string::ToString;
    use ocpp_client::ocpp_2_0_1::OCPP2_0_1Client;
    use ocpp_client::ocpp_types::v201::common::{
        CancelReservationStatusEnum, ReserveNowStatusEnum,
    };
    use ocpp_client::ocpp_types::v201::{
        CancelReservationResponse, ReserveNowRequest, ReserveNowResponse,
    };

    fn map_id_token(id_token: &ocpp_client::ocpp_types::v201::common::IdToken) -> IdToken {
        IdToken {
            value: id_token.id_token.to_string(),
            kind: map_id_token_kind(id_token.r#type.clone()),
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

    fn parse_evse_id(request: &ReserveNowRequest) -> Result<Option<usize>, ()> {
        match request.evse_id {
            None => Ok(None),
            Some(evse_id) => usize::try_from(evse_id).map(Some).map_err(|_| ()),
        }
    }

    #[async_trait::async_trait]
    impl ReserveNowHandler for OCPP2_0_1Client {
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
    impl CancelReservationHandler for OCPP2_0_1Client {
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

        fn wire_id_token() -> ocpp_client::ocpp_types::v201::common::IdToken {
            ocpp_client::ocpp_types::v201::common::IdToken {
                additional_info: None,
                id_token: heapless::String::try_from("04A224B2").unwrap(),
                r#type: ocpp_client::ocpp_types::v201::common::IdTokenEnum::ISO14443,
                custom_data: None,
            }
        }

        #[test]
        fn a_wire_id_token_maps_to_the_internal_representation() {
            let mapped = map_id_token(&wire_id_token());
            assert_eq!(mapped.value, "04A224B2");
            assert_eq!(mapped.kind, crate::state::IdTokenKind::ISO14443);
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

/// The OCPP 1.6J projection of `ReserveNowHandler`/`CancelReservationHandler`. `CancelReservation`
/// is the simple case - `CancelReservationRequest.reservationId` is a bare `i64`, same as
/// [`crate::state::ReservationId`], so `CancelReservationHandler` is implemented directly on
/// `OCPP1_6Client` with no wrapper needed (mirroring `RequestStopTransactionHandler`'s 1.6J
/// adapter in `crate::remote_control`).
///
/// `ReserveNow` needs the topology-aware translation every inbound 1.6J handler with a flat
/// `connectorId` needs, via [`crate::topology::unflatten_ocpp_1_6_connector_id`] - so
/// [`Ocpp1_6ReserveNowHandler`] wraps `OCPP1_6Client` with that topology, the same way
/// `crate::remote_control::Ocpp1_6RemoteControlHandler` does. Unlike `UnlockConnector`'s mandatory
/// specific connector, 1.6J's `ReserveNowRequest.connectorId` is `0` to mean "the Charge Point may
/// choose any connector" (matching `evseId: None`'s "search every EVSE" in later versions) or a
/// specific flat connector otherwise; [`handle_reserve_now`] only targets at EVSE granularity
/// itself (picking the first `Available` connector on it), so - the same reduction
/// `crate::remote_control`'s `RemoteStartTransaction` adapter makes - a specific `connectorId` is
/// unflattened down to its EVSE half and the specific connector within it is dropped.
/// `ReserveNowRequest.idTag` has no type/kind metadata (see `crate::id_tag`), so
/// [`crate::id_tag::map_id_token`] fills in `IdTokenKind::Central`. 1.6J's `ReserveNowResponseStatus`
/// already matches [`ReserveNowOutcome`] exactly (`Accepted`/`Faulted`/`Occupied`/`Rejected`/
/// `Unavailable`), so no narrowing is needed there.
#[cfg(feature = "ocpp_1_6")]
mod ocpp_1_6 {
    use super::{
        CancelReservationHandler, CancelReservationOutcome, ReserveNowHandler, ReserveNowOutcome,
        handle_cancel_reservation, handle_reserve_now,
    };
    use crate::actor::ChargePointActor;
    use crate::id_tag::map_id_token;
    use crate::state::ReservationId;
    use crate::topology::unflatten_ocpp_1_6_connector_id;
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use ocpp_client::ocpp_1_6::OCPP1_6Client;
    use ocpp_client::ocpp_types::v16::common::{
        CancelReservationResponseStatus, ReserveNowResponseStatus,
    };
    use ocpp_client::ocpp_types::v16::{
        CancelReservationResponse, ReserveNowRequest, ReserveNowResponse,
    };

    pub(super) fn map_reserve_now_outcome(outcome: ReserveNowOutcome) -> ReserveNowResponseStatus {
        match outcome {
            ReserveNowOutcome::Accepted => ReserveNowResponseStatus::Accepted,
            ReserveNowOutcome::Faulted => ReserveNowResponseStatus::Faulted,
            ReserveNowOutcome::Occupied => ReserveNowResponseStatus::Occupied,
            ReserveNowOutcome::Rejected => ReserveNowResponseStatus::Rejected,
            ReserveNowOutcome::Unavailable => ReserveNowResponseStatus::Unavailable,
        }
    }

    pub(super) fn map_cancel_reservation_outcome(
        outcome: CancelReservationOutcome,
    ) -> CancelReservationResponseStatus {
        match outcome {
            CancelReservationOutcome::Accepted => CancelReservationResponseStatus::Accepted,
            CancelReservationOutcome::Rejected => CancelReservationResponseStatus::Rejected,
        }
    }

    /// `Ok(None)` means "the Charge Point may choose any connector" (`connectorId == 0`);
    /// `Ok(Some(evse_id))` means the request's `connectorId` resolved to that EVSE; `Err(())`
    /// means it didn't address a real connector under `connector_counts` and the request must be
    /// rejected outright.
    pub(super) fn parse_evse_id(
        connector_counts: &[usize],
        request: &ReserveNowRequest,
    ) -> Result<Option<usize>, ()> {
        if request.connector_id == 0 {
            return Ok(None);
        }
        unflatten_ocpp_1_6_connector_id(connector_counts, request.connector_id)
            .map(|(evse_id, _)| Some(evse_id))
            .ok_or(())
    }

    /// Wraps an `OCPP1_6Client` with the charge point's connector topology, needed to translate
    /// `ReserveNowRequest`'s flat `connectorId` into this crate's `(evse_id, connector_id)`
    /// addressing - see this module's docs.
    pub struct Ocpp1_6ReserveNowHandler {
        client: OCPP1_6Client,
        connector_counts: Vec<usize>,
    }

    impl Ocpp1_6ReserveNowHandler {
        /// Wraps `client`, capturing `connector_counts` (each EVSE's connector count, in
        /// `evse_id` order) for translating connector addresses on every request.
        pub fn new(
            client: OCPP1_6Client,
            connector_counts: impl IntoIterator<Item = usize>,
        ) -> Self {
            Self {
                client,
                connector_counts: connector_counts.into_iter().collect(),
            }
        }
    }

    #[async_trait::async_trait]
    impl ReserveNowHandler for Ocpp1_6ReserveNowHandler {
        async fn register_reserve_now_handler(&self, actor: ChargePointActor) {
            let connector_counts = self.connector_counts.clone();
            self.client
                .on_reserve_now(move |request, _client| {
                    let actor = actor.clone();
                    let connector_counts = connector_counts.clone();
                    async move {
                        let outcome = match parse_evse_id(&connector_counts, &request) {
                            Ok(evse_id) => {
                                handle_reserve_now(
                                    &actor,
                                    evse_id,
                                    ReservationId(request.reservation_id),
                                    map_id_token(&request.id_tag),
                                )
                                .await
                            }
                            Err(()) => ReserveNowOutcome::Rejected,
                        };
                        Ok(ReserveNowResponse {
                            status: map_reserve_now_outcome(outcome),
                        })
                    }
                })
                .await;
        }
    }

    #[async_trait::async_trait]
    impl CancelReservationHandler for OCPP1_6Client {
        async fn register_cancel_reservation_handler(&self, actor: ChargePointActor) {
            self.on_cancel_reservation(move |request, _client| {
                let actor = actor.clone();
                async move {
                    let outcome =
                        handle_cancel_reservation(&actor, ReservationId(request.reservation_id))
                            .await;
                    Ok(CancelReservationResponse {
                        status: map_cancel_reservation_outcome(outcome),
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
        fn every_reserve_now_outcome_maps_to_a_wire_status() {
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Accepted),
                ReserveNowResponseStatus::Accepted
            );
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Faulted),
                ReserveNowResponseStatus::Faulted
            );
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Occupied),
                ReserveNowResponseStatus::Occupied
            );
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Rejected),
                ReserveNowResponseStatus::Rejected
            );
            assert_eq!(
                map_reserve_now_outcome(ReserveNowOutcome::Unavailable),
                ReserveNowResponseStatus::Unavailable
            );
        }

        #[test]
        fn every_cancel_reservation_outcome_maps_to_a_wire_status() {
            assert_eq!(
                map_cancel_reservation_outcome(CancelReservationOutcome::Accepted),
                CancelReservationResponseStatus::Accepted
            );
            assert_eq!(
                map_cancel_reservation_outcome(CancelReservationOutcome::Rejected),
                CancelReservationResponseStatus::Rejected
            );
        }

        fn request(connector_id: i64) -> ReserveNowRequest {
            ReserveNowRequest {
                connector_id,
                expiry_date: "2030-01-01T00:00:00Z".into(),
                id_tag: ocpp_client::ocpp_types::v16::IdTag::try_from("04A224B2").unwrap(),
                parent_id_tag: None,
                reservation_id: 1,
            }
        }

        #[test]
        fn connector_id_zero_means_the_charge_point_may_choose() {
            let connector_counts = [1, 1];

            assert_eq!(parse_evse_id(&connector_counts, &request(0)), Ok(None));
        }

        #[test]
        fn a_specific_connector_id_resolves_to_its_evse() {
            let connector_counts = [2, 1];

            assert_eq!(parse_evse_id(&connector_counts, &request(3)), Ok(Some(1)));
        }

        #[test]
        fn an_out_of_range_connector_id_is_rejected() {
            let connector_counts = [1, 1];

            assert_eq!(parse_evse_id(&connector_counts, &request(5)), Err(()));
        }

        #[test]
        fn ocpp1_6_reserve_now_handler_implements_the_handler_trait() {
            fn assert_impl<T: ReserveNowHandler>() {}
            assert_impl::<Ocpp1_6ReserveNowHandler>();
            fn assert_cancel_impl<T: CancelReservationHandler>() {}
            assert_cancel_impl::<OCPP1_6Client>();
        }
    }
}
