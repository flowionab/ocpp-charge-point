//! Customer information / GDPR erasure (`docs/ROADMAP.md` §14, `docs/PRODUCTION-ROADMAP.md`
//! B5.5): OCPP's `CustomerInformation` request and the `NotifyCustomerInformation` report(s) it
//! triggers.
//!
//! **2.x only.** 1.6J has no `CustomerInformation` message and no equivalent way to ask.
//!
//! # Two independent jobs, one request
//!
//! `CustomerInformationRequest` carries two independent flags, and either or both may be set:
//! `report` ("tell me what you hold about this customer") and `clear` ("erase it"). Both are
//! honoured against this crate's *real* state rather than a fabricated data store - the places a
//! customer's identity actually appears here are the authorization cache
//! ([`crate::state::AuthorizationCache`]), the local authorization list
//! ([`crate::state::LocalAuthorizationList`]), and any transaction currently in progress under
//! that identity.
//!
//! # What can and cannot be resolved
//!
//! OCPP identifies the customer one of three ways: a vendor-specific `customerIdentifier`, an
//! `idToken`, or a `customerCertificate`. This crate's state has no notion of a
//! `customerIdentifier` distinct from an `IdToken` - nothing here is ever keyed on one - so a
//! request naming only that can't be resolved to anything real. Matching a `customerCertificate`
//! needs certificate-chain cryptography this crate does not have (see `docs/ROADMAP.md` §12/§14
//! on the certificate store gap). Both are honestly reported `Invalid` rather than pretended at.
//! Only a request that names an `idToken` - this crate's one real customer-identifying key - is
//! resolvable, and [`handle_customer_information`] answers `Accepted` for it regardless of
//! whether anything is actually found (finding nothing is itself an honest answer to "report" and
//! a no-op for "clear").
//!
//! # Why erasure never touches a live transaction
//!
//! A transaction currently in progress under the erased `idToken` is *reported* (so the CSMS
//! knows charging is ongoing under that identity) but never mutated by `clear`. Removing the
//! token from an active [`crate::state::Transaction`] would leave a session running with no
//! record of who authorized it - the CSMS still needs to bill and reconcile it, and this crate
//! keeps no separate transaction history to erase from once the session ends (see
//! `crate::transaction_status`'s docs on why a finished transaction is already indistinguishable
//! from one this charge point never had). An operator who needs a running session's identity
//! gone has to stop it first.
//!
//! # Why the response precedes the report/erasure
//!
//! Gathering the report and applying the erasure both need nothing but this charge point's own
//! in-memory state, so in principle a handler could do both inline. This crate still splits the
//! decision (synchronous, in the handler) from the work (a queued job, run by
//! [`run_customer_information_requests`]) - the same "answer immediately, then act" discipline
//! [`crate::diagnostics`] uses for `GetLog`: the response the CSMS is waiting on should never be
//! held behind whatever the follow-up work takes, even if that work is currently cheap. A future
//! [`crate::hardware`] binding that makes gathering or erasure genuinely slow (e.g. a real
//! customer-identifier store) costs nothing extra to slot in here.

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::{format, vec};

use crate::actor::ChargePointActor;
use crate::clock::{Clock, is_synchronized};
use crate::state::{AuthorizationStatus, ChargePointEvent, IdToken, TransactionId};
use crate::sync::Chan;

/// The most bytes of report text carried in a single `NotifyCustomerInformation` chunk (see
/// [`chunk_customer_information`]). Matches `ocpp-types`' own `NotifyCustomerInformationRequest`
/// wire type's `data` field, a `heapless::String<512>` - the same reasoning
/// [`crate::reporting::REPORT_CHUNK_SIZE`] gives for matching its own wire cap: every chunk this
/// crate produces is guaranteed to fit the field it's going into, in every build configuration.
pub const CUSTOMER_INFORMATION_CHUNK_SIZE: usize = 512;

/// What a `CustomerInformationRequest` asks for and about, protocol-independent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomerInformationQuery {
    /// Correlates the response and every `NotifyCustomerInformation` with the request that
    /// started them.
    pub request_id: i64,
    /// Whether the CSMS wants a report of what this charge point holds.
    pub report: bool,
    /// Whether the CSMS wants that data erased.
    pub clear: bool,
    /// The customer's identifier, resolved to the one key this crate can actually search its
    /// state by. `None` when the request named no `idToken`, or named only a `customerIdentifier`
    /// / `customerCertificate` - see this module's docs on why those can't be resolved here.
    pub id_token: Option<IdToken>,
}

/// What this charge point answers a `CustomerInformationRequest` with, matching OCPP's
/// `CustomerInformationStatusEnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomerInformationOutcome {
    /// The request was accepted; the report and/or erasure it asked for is queued.
    Accepted,
    /// Neither `report` nor `clear` was set, so there is nothing to do.
    Rejected,
    /// The customer could not be resolved to anything this crate can search by - see this
    /// module's docs.
    Invalid,
}

/// One job queued by an accepted `CustomerInformationRequest`, carried from the handler to
/// [`run_customer_information_requests`] through [`CustomerInformationQueue`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct CustomerInformationJob {
    request_id: i64,
    id_token: IdToken,
    report: bool,
    clear: bool,
}

/// The queue a handler hands accepted requests to [`run_customer_information_requests`] through.
#[derive(Clone)]
pub struct CustomerInformationQueue {
    channel: Chan<CustomerInformationJob>,
}

impl CustomerInformationQueue {
    /// A new, empty queue.
    pub fn new() -> Self {
        Self {
            channel: Chan::new(),
        }
    }
}

impl Default for CustomerInformationQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Decides what to answer a `CustomerInformationRequest` with, and queues the accepted work (if
/// any) for [`run_customer_information_requests`].
///
/// Synchronous and immediate by design - see this module's docs on why the response precedes the
/// work. `Rejected` when neither `report` nor `clear` was asked for (there is nothing to queue);
/// `Invalid` when the customer names nothing this crate can search by.
pub fn handle_customer_information(
    queue: &CustomerInformationQueue,
    query: CustomerInformationQuery,
) -> CustomerInformationOutcome {
    if !query.report && !query.clear {
        return CustomerInformationOutcome::Rejected;
    }
    let Some(id_token) = query.id_token else {
        return CustomerInformationOutcome::Invalid;
    };
    queue.channel.send(CustomerInformationJob {
        request_id: query.request_id,
        id_token,
        report: query.report,
        clear: query.clear,
    });
    CustomerInformationOutcome::Accepted
}

/// What this charge point currently holds about a customer, gathered by [`gather`] - the "report"
/// half of `CustomerInformation`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct CustomerInformationRecord {
    /// The authorization cache's decision for this token, if any is held - see
    /// [`crate::state::AuthorizationCache`].
    cached_authorization: Option<AuthorizationStatus>,
    /// The local authorization list's decision for this token, if any is held - see
    /// [`crate::state::LocalAuthorizationList`].
    local_list_status: Option<AuthorizationStatus>,
    /// Every transaction currently in progress under this token. Reported, but never erased by
    /// `clear` - see this module's docs.
    live_transactions: Vec<TransactionId>,
}

/// Reads (without mutating) everything this charge point currently holds about `id_token`.
/// Synchronous: every source is already in memory.
fn gather(actor: &ChargePointActor, id_token: &IdToken) -> CustomerInformationRecord {
    let state = actor.state();
    let cached_authorization = state
        .authorization_cache
        .entries()
        .iter()
        .find(|entry| entry.id_token.value == id_token.value)
        .map(|entry| entry.status);
    let local_list_status = state
        .local_authorization_list
        .entries
        .iter()
        .find(|entry| entry.id_token.value == id_token.value)
        .map(|entry| entry.status);
    let live_transactions = state
        .evses
        .iter()
        .flat_map(|evse| evse.transactions.iter())
        .filter_map(|slot| slot.as_ref())
        .filter(|transaction| {
            transaction
                .id_token
                .as_ref()
                .is_some_and(|token| token.value == id_token.value)
        })
        .map(|transaction| transaction.id)
        .collect();
    CustomerInformationRecord {
        cached_authorization,
        local_list_status,
        live_transactions,
    }
}

/// Renders `record` as the human-readable text OCPP's `data` field asks for ("No format specified
/// ... Should be human readable"). One line per fact held, so an operator (or the CSMS's own
/// operator-facing tooling) can read it directly without a parser.
fn render(id_token: &IdToken, record: &CustomerInformationRecord) -> String {
    let mut rendered = format!("idToken: {} ({:?})\n", id_token.value, id_token.kind);
    match record.cached_authorization {
        Some(status) => rendered.push_str(&format!("authorization-cache: {status:?}\n")),
        None => rendered.push_str("authorization-cache: no entry held\n"),
    }
    match record.local_list_status {
        Some(status) => rendered.push_str(&format!("local-authorization-list: {status:?}\n")),
        None => rendered.push_str("local-authorization-list: no entry held\n"),
    }
    if record.live_transactions.is_empty() {
        rendered.push_str("transactions: none in progress\n");
    } else {
        for id in &record.live_transactions {
            rendered.push_str(&format!(
                "transaction: {} (in progress - not erasable while active)\n",
                id.0
            ));
        }
    }
    rendered
}

/// One `NotifyCustomerInformation` message's worth of a chunked report: its `seqNo`/`tbc`, plus
/// this chunk's slice of the report text. Produced by [`chunk_customer_information`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomerInformationChunk {
    /// This chunk's sequence number - `0` for the first chunk, incrementing by one per chunk.
    pub seq_no: i64,
    /// Whether another chunk follows this one.
    pub tbc: bool,
    /// This chunk's slice of the report text (at most [`CUSTOMER_INFORMATION_CHUNK_SIZE`] bytes,
    /// split on a `char` boundary).
    pub data: String,
}

/// Splits `data` into an ordered sequence of [`CustomerInformationChunk`]s of at most
/// [`CUSTOMER_INFORMATION_CHUNK_SIZE`] bytes each, with correctly incrementing `seq_no`/`tbc` -
/// the same shape [`crate::reporting::chunk_report`] uses for `NotifyReport`, just splitting one
/// text blob by byte length rather than a list of entries by count, since a single
/// `NotifyCustomerInformation` carries one bounded string rather than a list.
///
/// An empty `data` still produces exactly one (empty, `tbc: false`) chunk rather than zero -
/// `seqNo` must start at `0` per OCPP, which only makes sense if at least one
/// `NotifyCustomerInformation` is actually sent.
pub fn chunk_customer_information(data: &str) -> Vec<CustomerInformationChunk> {
    let parts = split_at_char_boundaries(data, CUSTOMER_INFORMATION_CHUNK_SIZE);
    let total = parts.len();
    parts
        .into_iter()
        .enumerate()
        .map(|(index, part)| CustomerInformationChunk {
            seq_no: index as i64,
            tbc: index + 1 < total,
            data: part.to_string(),
        })
        .collect()
}

/// Splits `data` into slices of at most `max_bytes` bytes each, never cutting a multi-byte `char`
/// in half. Returns `[""]` for empty input, matching [`chunk_customer_information`]'s "always at
/// least one chunk" contract.
fn split_at_char_boundaries(data: &str, max_bytes: usize) -> Vec<&str> {
    if data.is_empty() {
        return vec![""];
    }
    let mut parts = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let mut end = rest.len().min(max_bytes);
        while end > 0 && !rest.is_char_boundary(end) {
            end -= 1;
        }
        if end == 0 {
            // `max_bytes` landed inside the first `char`'s own encoding - take that whole `char`
            // rather than produce an empty chunk that would never make progress.
            end = rest.chars().next().map_or(rest.len(), char::len_utf8);
        }
        let (part, remainder) = rest.split_at(end);
        parts.push(part);
        rest = remainder;
    }
    parts
}

/// The `NotifyCustomerInformation.generatedAt` timestamp, sourced from `clock.now()` - the same
/// unsynchronized-clock stance [`crate::reporting`]'s equivalent helper takes: send it as-is and
/// warn, never fabricate or drop it.
fn generated_at<C: Clock>(clock: &C) -> String {
    let now = clock.now();
    if !is_synchronized(&now) {
        tracing::warn!(
            timestamp = %now,
            "NotifyCustomerInformation generatedAt sourced from an unsynchronized clock"
        );
    }
    now.to_rfc3339()
}

/// Reports one `CustomerInformation` chunk to the CSMS, logging (and swallowing) a failure - a
/// send that fails must not stop erasure, which OCPP does not ask the report to succeed first.
async fn send_chunk<N: CustomerInformationNotifier>(
    notifier: &N,
    request_id: i64,
    generated_at: &str,
    chunk: CustomerInformationChunk,
) {
    if let Err(err) = notifier
        .notify_customer_information(
            request_id,
            chunk.seq_no,
            chunk.tbc,
            generated_at,
            chunk.data,
        )
        .await
    {
        tracing::warn!(
            error = %err,
            seq_no = chunk.seq_no,
            "failed to send a NotifyCustomerInformation chunk"
        );
    }
}

/// Erases everything this charge point holds about `id_token`, through the real state machine -
/// see [`ChargePointEvent::CustomerInformationErased`]. Send failure means the actor has shut
/// down, matching every other fire-and-forget `send` in this crate.
async fn erase(actor: &ChargePointActor, id_token: &IdToken) {
    let _ = actor
        .send(ChargePointEvent::CustomerInformationErased {
            id_token: id_token.clone(),
        })
        .await;
}

/// Performs every accepted `CustomerInformation` job - gathering and sending the report (if
/// asked), then applying the erasure (if asked) - until the process ends.
///
/// The report is gathered and sent **before** erasure is applied: a request with both `report`
/// and `clear` set should describe what was held right before it was deleted, not report on data
/// that `clear` has already removed.
pub async fn run_customer_information_requests<N, C>(
    actor: &ChargePointActor,
    queue: CustomerInformationQueue,
    notifier: &N,
    clock: &C,
) where
    N: CustomerInformationNotifier,
    C: Clock,
{
    loop {
        let job = queue.channel.recv().await;
        if job.report {
            let record = gather(actor, &job.id_token);
            let data = render(&job.id_token, &record);
            let generated_at = generated_at(clock);
            for chunk in chunk_customer_information(&data) {
                send_chunk(notifier, job.request_id, &generated_at, chunk).await;
            }
        }
        if job.clear {
            erase(actor, &job.id_token).await;
        }
    }
}

/// Reports one `NotifyCustomerInformation` chunk to the CSMS - OCPP 2.x's counterpart to
/// [`crate::reporting`]'s `NotifyReport`.
#[async_trait::async_trait]
pub trait CustomerInformationNotifier {
    /// What went wrong sending the notification.
    type Error: core::fmt::Display;

    /// Sends one chunk of `data` for the report started by `request_id`.
    async fn notify_customer_information(
        &self,
        request_id: i64,
        seq_no: i64,
        tbc: bool,
        generated_at: &str,
        data: String,
    ) -> Result<(), Self::Error>;
}

#[async_trait::async_trait]
impl<T: CustomerInformationNotifier + Send + Sync + ?Sized> CustomerInformationNotifier
    for alloc::sync::Arc<T>
{
    type Error = T::Error;

    async fn notify_customer_information(
        &self,
        request_id: i64,
        seq_no: i64,
        tbc: bool,
        generated_at: &str,
        data: String,
    ) -> Result<(), Self::Error> {
        (**self)
            .notify_customer_information(request_id, seq_no, tbc, generated_at, data)
            .await
    }
}

/// Registers this charge point's inbound `CustomerInformation` handling. 2.x only - see this
/// module's docs.
#[async_trait::async_trait]
pub trait CustomerInformationHandler {
    /// Registers a handler answering with [`handle_customer_information`]'s outcome and queueing
    /// any accepted work onto `queue`.
    async fn register_customer_information_handler(
        &self,
        actor: ChargePointActor,
        queue: CustomerInformationQueue,
    );
}

#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1;
#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1;

#[cfg(feature = "ocpp_2_0_1")]
pub use self::ocpp_2_0_1::Ocpp2_0_1CustomerInformationHandler;
#[cfg(feature = "ocpp_2_1")]
pub use self::ocpp_2_1::Ocpp2_1CustomerInformationHandler;

#[cfg(test)]
mod tests;
