//! `GetTransactionStatus` (`docs/PRODUCTION-ROADMAP.md` B5.4): OCPP 2.x's E14 "Check transaction
//! status" use case. Lets a CSMS ask whether a transaction is still ongoing and whether this
//! charge point still has transaction-related messages queued for it - what a CSMS uses to
//! reconcile after an offline period (E14 §4: "There are scenarios where a CSMS needs to know
//! whether there are still messages for a transaction that need to be delivered ... It may also
//! need to know whether a transaction is still ongoing.").
//!
//! **2.x only.** 1.6J has no such message and no equivalent way to ask - it has no
//! `TransactionEvent` stream to reconcile in the first place (its `StartTransaction`/
//! `StopTransaction`/`MeterValues` split isn't tracked as a single in-flight sequence the way
//! 2.x's `seqNo`-numbered `TransactionEvent`s are).
//!
//! # Two independent questions, one request
//!
//! A `GetTransactionStatusRequest` optionally names a transaction. E14's sequence diagram and its
//! requirements (E14.FR.01-08) describe two different questions depending on whether it does:
//!
//! - **With a `transactionId`:** "is this specific transaction still ongoing, and are there still
//!   messages queued about it?" - both `ongoingIndicator` and `messagesInQueue` are meaningful and
//!   answered (E14.FR.01-05).
//! - **Without one:** "are there transaction-related messages queued *at all*, for any
//!   transaction?" - `ongoingIndicator` has nothing to answer (there is no transaction it could be
//!   about) and per E14.FR.06 the field is omitted entirely, not set to `false`. Only
//!   `messagesInQueue` is answered, now scoped to the whole backlog rather than one transaction
//!   (E14.FR.07/08).
//!
//! [`handle_get_transaction_status`] takes an `Option<TransactionId>` to keep that distinction
//! explicit through to the response, rather than collapsing "no transaction named" and "named a
//! transaction that turned out not to matter" into the same shape.
//!
//! # Why a finished transaction and an unknown one answer identically
//!
//! E14.FR.01 ("no transaction with that id") and E14.FR.03 ("the transaction has stopped") both
//! require `ongoingIndicator = false` - nothing in the requirements table lets a CSMS tell those
//! two cases apart, and E14's own remarks section says so explicitly: "When the CSMS receives a
//! GetTransactionStatusResponse with both fields ... set to false, this might mean that the
//! transaction is finished ... or the Charging Station doesn't know anything about this
//! transaction (anymore)."
//!
//! That is exactly how this crate's state already behaves, with no extra bookkeeping needed to
//! match it: [`crate::state::charge_point_state`]'s `advance_transaction` clears a connector's
//! transaction slot (`Option<Transaction>::take()`) the moment its `Ended` TransactionEvent is
//! raised. A transaction id this charge point finished five minutes ago and one it never had at
//! all are therefore genuinely indistinguishable from here - both look like "not present in any
//! connector's slot" - which is the honest answer rather than a gap: keeping a graveyard of
//! finished transaction ids just to answer this one message would be state this crate does not
//! otherwise need, for a distinction the spec says the CSMS isn't owed anyway.
//!
//! A transaction that is present but mid-`Stopping` (its `stop_reason` is set but its `Ended`
//! event hasn't fired yet - see [`crate::state::Transaction::stop_reason`]) also answers `false`:
//! it has stopped in the sense E14.FR.03 means, even though this crate hasn't cleared its slot
//! yet.

use alloc::boxed::Box;
use alloc::sync::Arc;

use crate::actor::ChargePointActor;
use crate::offline_queue::OfflineQueue;
use crate::state::{TransactionEventOccurred, TransactionId};

/// What a `GetTransactionStatusRequest` asks about, protocol-independent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransactionStatusQuery {
    /// The transaction the CSMS wants the status of, or `None` to ask "are there any
    /// transaction-related messages queued at all" without naming one - see this module's docs.
    pub transaction_id: Option<TransactionId>,
}

/// What this charge point answers a `GetTransactionStatusRequest` with, protocol-independent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactionStatus {
    /// Whether the named transaction is still ongoing.
    ///
    /// `None` when the request named no transaction (E14.FR.06 requires the field be absent
    /// entirely, not `Some(false)`, since there is no transaction it could describe).
    /// `Some(true)` while the named transaction is running (E14.FR.02). `Some(false)` once it has
    /// stopped, or if this charge point never had a transaction with that id at all - see this
    /// module's docs on why those two cases (E14.FR.01/03) are indistinguishable by design.
    pub ongoing_indicator: Option<bool>,
    /// Whether there are still transaction-related messages queued for delivery: scoped to the
    /// named transaction if one was named (E14.FR.04/05), or to the whole backlog if none was
    /// (E14.FR.07/08).
    pub messages_in_queue: bool,
}

/// Decides the answer to a `GetTransactionStatusRequest` against real state.
///
/// `queue` is the same `OfflineQueue<TransactionEventOccurred>` that
/// [`crate::builder::ChargePointBuilder::transaction_events`] (or `_persisted`) created for the
/// Transactions block's CSMS forwarding - `None` if neither of those has been registered, in
/// which case nothing this crate produces is ever queued and `messages_in_queue` is always
/// `false`, correctly.
///
/// Synchronous: everything this needs - the live transaction slots, the queue snapshot - is
/// already in memory, so there is nothing to await, unlike e.g. [`crate::certificates`]'s
/// handlers which reach into an integrator-supplied store.
pub fn handle_get_transaction_status(
    actor: &ChargePointActor,
    queue: Option<&OfflineQueue<TransactionEventOccurred>>,
    query: TransactionStatusQuery,
) -> TransactionStatus {
    let ongoing_indicator = query.transaction_id.map(|id| is_ongoing(actor, id));
    let messages_in_queue = match (queue, query.transaction_id) {
        (None, _) => false,
        (Some(queue), None) => !queue.is_empty(),
        // Scoped to the named transaction: a snapshot rather than `peek_front`/`pop_front`, since
        // this only needs to *look* at the backlog, never touch delivery order the way
        // `flush_offline_queue` does.
        (Some(queue), Some(id)) => queue
            .snapshot()
            .iter()
            .any(|occurred| occurred.transaction.id == id),
    };
    TransactionStatus {
        ongoing_indicator,
        messages_in_queue,
    }
}

/// Whether `id` names a transaction this charge point currently has live: present in some
/// connector's slot with no `stop_reason` recorded yet. See this module's docs for why a
/// transaction whose slot has already been cleared (finished, or never existed) both answer
/// `false` here without needing to be told apart.
fn is_ongoing(actor: &ChargePointActor, id: TransactionId) -> bool {
    actor
        .state()
        .evses
        .iter()
        .flat_map(|evse| evse.transactions.iter())
        .filter_map(|slot| slot.as_ref())
        .find(|transaction| transaction.id == id)
        .is_some_and(|transaction| transaction.stop_reason.is_none())
}

/// Registers this charge point's inbound `GetTransactionStatus` handling. 2.x only - see this
/// module's docs.
#[async_trait::async_trait]
pub trait GetTransactionStatusHandler {
    /// Registers a handler dispatching against `actor`, answering from `queue` (see
    /// [`handle_get_transaction_status`]).
    async fn register_get_transaction_status_handler(
        &self,
        actor: ChargePointActor,
        queue: Option<Arc<OfflineQueue<TransactionEventOccurred>>>,
    );
}

#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1;
#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1;

#[cfg(feature = "ocpp_2_0_1")]
pub use self::ocpp_2_0_1::Ocpp2_0_1GetTransactionStatusHandler;
#[cfg(feature = "ocpp_2_1")]
pub use self::ocpp_2_1::Ocpp2_1GetTransactionStatusHandler;

#[cfg(test)]
mod tests;
