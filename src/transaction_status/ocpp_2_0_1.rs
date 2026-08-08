//! OCPP 2.0.1 wire adapter for `GetTransactionStatus` (B5.4).
//!
//! Wire-identical to 2.1's `GetTransactionStatusRequest`/`Response` (both carry the same optional
//! `transactionId`, `messagesInQueue`, and optional `ongoingIndicator`), so this adapter is the
//! same shape as the 2.1 one with 2.0.1's types substituted in.

use alloc::boxed::Box;
use alloc::sync::Arc;

use crate::wire::v201::{GetTransactionStatusRequest, GetTransactionStatusResponse};
use ocpp_client::ocpp_2_0_1::OCPP2_0_1Client;

use crate::actor::ChargePointActor;
use crate::offline_queue::OfflineQueue;
use crate::state::{TransactionEventOccurred, TransactionId};
use crate::transaction_status::{
    GetTransactionStatusHandler, TransactionStatus, TransactionStatusQuery,
    handle_get_transaction_status,
};

/// Same reasoning as 2.1's identically-named function: this crate's transaction ids are formatted
/// the same decimal way regardless of the negotiated protocol version.
fn map_transaction_id(wire: &str) -> Option<TransactionId> {
    wire.parse().ok().map(TransactionId)
}

fn response(status: TransactionStatus) -> GetTransactionStatusResponse {
    GetTransactionStatusResponse {
        custom_data: None,
        messages_in_queue: status.messages_in_queue,
        ongoing_indicator: status.ongoing_indicator,
    }
}

/// Registers 2.0.1's `GetTransactionStatus`.
pub struct Ocpp2_0_1GetTransactionStatusHandler {
    client: OCPP2_0_1Client,
}

impl Ocpp2_0_1GetTransactionStatusHandler {
    /// Wraps `client`.
    pub fn new(client: OCPP2_0_1Client) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl GetTransactionStatusHandler for Ocpp2_0_1GetTransactionStatusHandler {
    async fn register_get_transaction_status_handler(
        &self,
        actor: ChargePointActor,
        queue: Option<Arc<OfflineQueue<TransactionEventOccurred>>>,
    ) {
        self.client
            .on_get_transaction_status(move |request: GetTransactionStatusRequest, _client| {
                let actor = actor.clone();
                let queue = queue.clone();
                async move {
                    let transaction_id = request
                        .transaction_id
                        .as_deref()
                        .and_then(map_transaction_id);
                    let status = handle_get_transaction_status(
                        &actor,
                        queue.as_deref(),
                        TransactionStatusQuery { transaction_id },
                    );
                    Ok(response(status))
                }
            })
            .await;
    }
}

/// The `std` convenience: a bare [`OCPP2_0_1Client`] handles this block directly.
#[cfg(feature = "std")]
#[async_trait::async_trait]
impl GetTransactionStatusHandler for OCPP2_0_1Client {
    async fn register_get_transaction_status_handler(
        &self,
        actor: ChargePointActor,
        queue: Option<Arc<OfflineQueue<TransactionEventOccurred>>>,
    ) {
        Ocpp2_0_1GetTransactionStatusHandler::new(self.clone())
            .register_get_transaction_status_handler(actor, queue)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_decimal_id_round_trips() {
        assert_eq!(map_transaction_id("42"), Some(TransactionId(42)));
    }

    #[test]
    fn an_id_this_crate_could_never_have_minted_maps_to_unknown() {
        assert_eq!(map_transaction_id("not-a-number"), None);
        assert_eq!(map_transaction_id(""), None);
    }
}
