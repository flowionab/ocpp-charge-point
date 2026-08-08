//! OCPP 2.1 wire adapter for `GetTransactionStatus` (B5.4).

use alloc::boxed::Box;
use alloc::sync::Arc;

use crate::wire::v21::{GetTransactionStatusRequest, GetTransactionStatusResponse};
use ocpp_client::ocpp_2_1::OCPP2_1Client;

use crate::actor::ChargePointActor;
use crate::offline_queue::OfflineQueue;
use crate::state::{TransactionEventOccurred, TransactionId};
use crate::transaction_status::{
    GetTransactionStatusHandler, TransactionStatus, TransactionStatusQuery,
    handle_get_transaction_status,
};

/// Parses the wire's decimal transaction id string back onto this crate's [`TransactionId`].
///
/// `None` for anything that doesn't parse as a plain `u64` - this crate only ever mints decimal
/// ids for the transactions it creates (see `crate::transactions`' `ocpp_2_1` adapter, which
/// formats [`TransactionId`] the same way), so a CSMS naming anything else is asking about a
/// transaction this charge point could never have had. That collapses to exactly the same
/// "unknown transaction" answer (E14.FR.01) a well-formed-but-never-seen id gets, so no separate
/// error path is needed - [`handle_get_transaction_status`] treats "never seen" the same way
/// whether the id was unparseable or simply absent from every connector's transaction slot.
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

/// Registers 2.1's `GetTransactionStatus`.
pub struct Ocpp2_1GetTransactionStatusHandler {
    client: OCPP2_1Client,
}

impl Ocpp2_1GetTransactionStatusHandler {
    /// Wraps `client`.
    pub fn new(client: OCPP2_1Client) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl GetTransactionStatusHandler for Ocpp2_1GetTransactionStatusHandler {
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

/// The `std` convenience: a bare [`OCPP2_1Client`] handles this block directly.
#[cfg(feature = "std")]
#[async_trait::async_trait]
impl GetTransactionStatusHandler for OCPP2_1Client {
    async fn register_get_transaction_status_handler(
        &self,
        actor: ChargePointActor,
        queue: Option<Arc<OfflineQueue<TransactionEventOccurred>>>,
    ) {
        Ocpp2_1GetTransactionStatusHandler::new(self.clone())
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
        // Not decimal, or negative, or otherwise not a `u64` this crate would ever have assigned.
        assert_eq!(map_transaction_id("not-a-number"), None);
        assert_eq!(map_transaction_id("-1"), None);
        assert_eq!(map_transaction_id(""), None);
    }

    #[test]
    fn the_response_carries_both_fields_through_unchanged() {
        let wire = response(TransactionStatus {
            ongoing_indicator: Some(true),
            messages_in_queue: true,
        });
        assert_eq!(wire.ongoing_indicator, Some(true));
        assert!(wire.messages_in_queue);

        let wire = response(TransactionStatus {
            ongoing_indicator: None,
            messages_in_queue: false,
        });
        assert_eq!(wire.ongoing_indicator, None);
        assert!(!wire.messages_in_queue);
    }
}
