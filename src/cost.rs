//! Tariff and cost functional block: recording the CSMS's running cost updates for an active
//! transaction, via `CostUpdated`. See `docs/ROADMAP.md` §9.
//!
//! `CostUpdated`'s response carries no status at all - the CSMS is simply informing the charge
//! point, not asking permission, so there's no way to reject one on the wire. [`CostUpdateOutcome`]
//! exists purely for this module's own testability; it isn't reflected in the OCPP response.

use crate::actor::ChargePointActor;
use crate::state::{ChargePointEvent, ChargePointState, ConnectorEvent, EvseEvent, TransactionId};
use alloc::boxed::Box;

/// The outcome of handling a `CostUpdated` request - internal-only, see this module's docs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CostUpdateOutcome {
    /// The cost was recorded against the connector currently running `transaction_id`.
    Applied,
    /// `transaction_id` doesn't match any transaction currently active on this charge point.
    UnknownTransaction,
}

/// Handles a CSMS-initiated `CostUpdated` request against `actor`: finds the connector whose
/// active transaction is `transaction_id` and records `total_cost` against it.
#[tracing::instrument(skip_all)]
pub async fn handle_cost_updated(
    actor: &ChargePointActor,
    transaction_id: TransactionId,
    total_cost: f64,
) -> CostUpdateOutcome {
    let Some((evse_id, connector_id)) = find_transaction(&actor.state(), transaction_id) else {
        return CostUpdateOutcome::UnknownTransaction;
    };

    let _ = actor
        .send(ChargePointEvent::Evse {
            evse_id,
            event: EvseEvent::Connector {
                connector_id,
                event: ConnectorEvent::CostUpdated(total_cost),
            },
        })
        .await;

    CostUpdateOutcome::Applied
}

/// The connector (if any) whose active transaction is `transaction_id`.
fn find_transaction(
    state: &ChargePointState,
    transaction_id: TransactionId,
) -> Option<(usize, usize)> {
    state.evses.iter().enumerate().find_map(|(evse_id, evse)| {
        evse.transactions
            .iter()
            .position(|transaction| transaction.as_ref().is_some_and(|t| t.id == transaction_id))
            .map(|connector_id| (evse_id, connector_id))
    })
}

/// Registers this charge point's inbound `CostUpdated` handling with the CSMS connection.
/// Implemented per protocol version (see the `ocpp_2_1` module), mirroring
/// [`crate::reservation::ReserveNowHandler`].
#[async_trait::async_trait]
pub trait CostUpdatedHandler {
    /// Registers a `CostUpdated` handler with the CSMS connection that dispatches incoming
    /// requests to [`handle_cost_updated`] against `actor`.
    async fn register_cost_updated_handler(&self, actor: ChargePointActor);
}

#[cfg(test)]
mod tests {
    use super::{CostUpdateOutcome, handle_cost_updated};
    use crate::actor::ChargePointActor;
    use crate::executor::TokioExecutor;
    use crate::state::{
        ChargePointEvent, ConnectorEvent, EvseEvent, IdToken, IdTokenKind, TransactionId,
    };

    fn test_id_token() -> IdToken {
        IdToken {
            value: "04A224B2".into(),
            kind: IdTokenKind::ISO14443,
        }
    }

    async fn actor_with_active_transaction() -> ChargePointActor {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        for event in [
            ConnectorEvent::CableConnected,
            ConnectorEvent::LockConfirmed,
            ConnectorEvent::IdTokenPresented(test_id_token()),
            ConnectorEvent::ChargingAuthorized(test_id_token()),
        ] {
            actor
                .send(ChargePointEvent::Evse {
                    evse_id: 0,
                    event: EvseEvent::Connector {
                        connector_id: 0,
                        event,
                    },
                })
                .await
                .unwrap();
        }
        actor
    }

    #[tokio::test]
    async fn a_known_transaction_records_the_cost() {
        let actor = actor_with_active_transaction().await;

        let outcome = handle_cost_updated(&actor, TransactionId(0), 4.5).await;

        assert_eq!(outcome, CostUpdateOutcome::Applied);
        assert_eq!(actor.state().evses[0].running_costs[0], Some(4.5));
    }

    #[tokio::test]
    async fn an_unknown_transaction_is_rejected() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);

        let outcome = handle_cost_updated(&actor, TransactionId(0), 4.5).await;

        assert_eq!(outcome, CostUpdateOutcome::UnknownTransaction);
    }
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use super::{CostUpdatedHandler, handle_cost_updated};
    use crate::actor::ChargePointActor;
    use crate::state::TransactionId;
    use crate::wire::v21::{CostUpdatedRequest, CostUpdatedResponse};
    use alloc::boxed::Box;
    use ocpp_client::ocpp_2_1::{OCPP2_1Client, OCPP2_1Error};

    /// A `transactionId` that doesn't parse as a `u64` can't address a transaction - treated the
    /// same as an unknown one, without needing to consult the actor. Mirrors
    /// [`crate::remote_control::ocpp_2_1::parse_transaction_id`].
    fn parse_transaction_id(request: &CostUpdatedRequest) -> Option<TransactionId> {
        request
            .transaction_id
            .parse::<u64>()
            .ok()
            .map(TransactionId)
    }

    /// The logic behind [`OCPP2_1Client`]'s registered `CostUpdated` handler, factored out so
    /// C5's capability-off CALLERROR can be asserted directly in a unit test rather than only
    /// through a full client/transport round trip. See this module's top-level docs and
    /// `crate::refusal`.
    async fn handle(
        actor: &ChargePointActor,
        request: &CostUpdatedRequest,
    ) -> Result<CostUpdatedResponse, OCPP2_1Error> {
        if !crate::refusal::capability_present(&actor.state().capabilities, "CostUpdated") {
            return Err(crate::refusal::ocpp_2_1_not_supported("CostUpdated"));
        }
        if let Some(transaction_id) = parse_transaction_id(request) {
            handle_cost_updated(actor, transaction_id, request.total_cost).await;
        }
        Ok(CostUpdatedResponse { custom_data: None })
    }

    #[async_trait::async_trait]
    impl CostUpdatedHandler for OCPP2_1Client {
        async fn register_cost_updated_handler(&self, actor: ChargePointActor) {
            self.on_cost_updated(move |request, _client| {
                let actor = actor.clone();
                async move { handle(&actor, &request).await }
            })
            .await;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::executor::TokioExecutor;
        use crate::hardware::Capabilities;
        use crate::state::ChargePointEvent;
        use crate::wire::v21::RpcErrorCode;

        fn request(transaction_id: &str) -> CostUpdatedRequest {
            CostUpdatedRequest {
                custom_data: None,
                total_cost: 4.5,
                transaction_id: heapless::String::try_from(transaction_id).unwrap(),
            }
        }

        #[test]
        fn a_valid_transaction_id_parses() {
            assert_eq!(parse_transaction_id(&request("7")), Some(TransactionId(7)));
        }

        #[test]
        fn a_non_numeric_transaction_id_fails_to_parse() {
            assert_eq!(parse_transaction_id(&request("not-a-number")), None);
        }

        // C5 (docs/PRODUCTION-ROADMAP.md §5.5): `CostUpdatedResponse` has no status field, so a
        // runtime-absent `tariff_and_cost` capability must refuse with a `NotSupported`
        // CALLERROR, never a bare `Ok` and never a generic error.
        #[tokio::test]
        async fn cost_updated_is_a_not_supported_call_error_when_the_capability_is_absent() {
            let actor = ChargePointActor::spawn([1], &TokioExecutor);

            let result = handle(&actor, &request("7")).await;

            assert_eq!(result.unwrap_err().code, RpcErrorCode::NotSupported);
        }

        #[tokio::test]
        async fn cost_updated_succeeds_when_the_capability_is_present() {
            let actor = ChargePointActor::spawn([1], &TokioExecutor);
            actor
                .send(ChargePointEvent::CapabilitiesDeclared(Capabilities {
                    tariff_and_cost: true,
                    ..Default::default()
                }))
                .await
                .unwrap();

            let result = handle(&actor, &request("not-a-number")).await;

            assert!(result.is_ok());
        }
    }
}

/// The OCPP 2.0.1 projection - identical `CostUpdatedRequest`/`CostUpdatedResponse` wire shape to
/// 2.1's, so this is a copy of the 2.1 module, just targeting `OCPP2_0_1Client`.
#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1 {
    use super::{CostUpdatedHandler, handle_cost_updated};
    use crate::actor::ChargePointActor;
    use crate::state::TransactionId;
    use crate::wire::v201::{CostUpdatedRequest, CostUpdatedResponse};
    use alloc::boxed::Box;
    use ocpp_client::ocpp_2_0_1::{OCPP2_0_1Client, OCPP2_0_1Error};

    fn parse_transaction_id(request: &CostUpdatedRequest) -> Option<TransactionId> {
        request
            .transaction_id
            .parse::<u64>()
            .ok()
            .map(TransactionId)
    }

    /// Mirrors [`super::ocpp_2_1::handle`].
    async fn handle(
        actor: &ChargePointActor,
        request: &CostUpdatedRequest,
    ) -> Result<CostUpdatedResponse, OCPP2_0_1Error> {
        if !crate::refusal::capability_present(&actor.state().capabilities, "CostUpdated") {
            return Err(crate::refusal::ocpp_2_0_1_not_supported("CostUpdated"));
        }
        if let Some(transaction_id) = parse_transaction_id(request) {
            handle_cost_updated(actor, transaction_id, request.total_cost).await;
        }
        Ok(CostUpdatedResponse { custom_data: None })
    }

    #[async_trait::async_trait]
    impl CostUpdatedHandler for OCPP2_0_1Client {
        async fn register_cost_updated_handler(&self, actor: ChargePointActor) {
            self.on_cost_updated(move |request, _client| {
                let actor = actor.clone();
                async move { handle(&actor, &request).await }
            })
            .await;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::executor::TokioExecutor;
        use crate::hardware::Capabilities;
        use crate::state::ChargePointEvent;
        use crate::wire::v201::RpcErrorCode;

        fn request(transaction_id: &str) -> CostUpdatedRequest {
            CostUpdatedRequest {
                custom_data: None,
                total_cost: 4.5,
                transaction_id: heapless::String::try_from(transaction_id).unwrap(),
            }
        }

        #[test]
        fn a_valid_transaction_id_parses() {
            assert_eq!(parse_transaction_id(&request("7")), Some(TransactionId(7)));
        }

        #[test]
        fn a_non_numeric_transaction_id_fails_to_parse() {
            assert_eq!(parse_transaction_id(&request("not-a-number")), None);
        }

        #[tokio::test]
        async fn cost_updated_is_a_not_supported_call_error_when_the_capability_is_absent() {
            let actor = ChargePointActor::spawn([1], &TokioExecutor);

            let result = handle(&actor, &request("7")).await;

            assert_eq!(result.unwrap_err().code, RpcErrorCode::NotSupported);
        }

        #[tokio::test]
        async fn cost_updated_succeeds_when_the_capability_is_present() {
            let actor = ChargePointActor::spawn([1], &TokioExecutor);
            actor
                .send(ChargePointEvent::CapabilitiesDeclared(Capabilities {
                    tariff_and_cost: true,
                    ..Default::default()
                }))
                .await
                .unwrap();

            let result = handle(&actor, &request("not-a-number")).await;

            assert!(result.is_ok());
        }
    }
}
