//! Replay protection: detecting delivery of a CSMS-initiated remote-control command whose
//! effect has already happened, and reporting it via
//! [`SecurityEventType::AttemptedReplayAttacks`](crate::state::SecurityEventType::AttemptedReplayAttacks).
//!
//! # Threat model: what OCPP-J actually exposes to replay
//!
//! OCPP-J correlates every CALL and its CALLRESULT/CALLERROR by a message id, over a WebSocket
//! that is normally TLS-protected (Security Profile 2/3). Working through what an attacker can
//! genuinely replay in that setting - and what's already handled elsewhere - shapes what this
//! module does and does not cover:
//!
//! 1. **Transport-level replay (an on-path attacker resending a captured frame byte-for-byte).**
//!    Under TLS, the record layer's own sequence numbers make this a non-issue: a replayed
//!    ciphertext record fails integrity/ordering checks and is dropped by the TLS stack before
//!    OCPP ever sees it. This module builds no defence against it, because the transport
//!    (`ocpp-client`'s TLS configuration) already owns it - Security Profile 1 (plain `ws://`)
//!    has no such protection, but that is a deployment/`ocpp-client` transport concern, not
//!    something an application-layer message inspector can retrofit.
//! 2. **A replayed CALLRESULT/CALLERROR for one of *our own* outgoing CALLs.** Read from
//!    `ocpp-client` 0.5.0's `src/client.rs` (`handle_frame`, `MESSAGE_TYPE_RESULT`/
//!    `MESSAGE_TYPE_ERROR` arms): the first matching response removes the message id from
//!    `pending_responses`; a second response frame with the same id finds nothing to remove and
//!    is silently dropped. A replayed response can therefore never be delivered twice to this
//!    crate. Nothing to add here either.
//! 3. **A duplicate/replayed incoming CALL (CSMS -> charge point) reusing a message id.**
//!    Also read from `ocpp-client` 0.5.0's `handle_frame`: an incoming `CALL` is dispatched to
//!    whatever handler is registered for its action *unconditionally* - there is no tracking of
//!    message ids already seen for incoming calls at all. Worse: `Client::on`'s callback
//!    signature (`FnMut(A::Request, Client) -> Future<Output = Result<A::Response, E>>`) never
//!    hands the registered handler the message id in the first place. That means this crate has
//!    no way to see, and therefore no way to deduplicate, incoming CALL message ids through
//!    `ocpp-client`'s current public API - full stop. **This is a gap that belongs in
//!    `ocpp-client`** (it owns CALL/CALLRESULT correlation per `CLAUDE.md`), not something this
//!    module can fake from above it. It is called out here rather than silently left uncovered.
//! 4. **Replayed authorization/transaction *content*, independent of message id** - the case
//!    this module actually addresses. A CSMS-initiated command whose effect is state-gated (it
//!    only does something the *first* time, because the precondition state it needs has since
//!    moved on) can still be *resubmitted* - by an attacker replaying an old CALL body under a
//!    fresh message id, by a compromised/buggy CSMS, or by a stuck retry loop - after it has
//!    already taken effect. The state machine already refuses to double-apply it (see each
//!    handler's own docs), but nothing previously *reported* that a refusal of this specific
//!    shape had happened. [`ReplayGuard`] closes that reporting gap.
//!
//! # What this module deliberately does *not* flag
//!
//! [`crate::offline_queue::OfflineQueue`] resending its own contents after a reconnect is
//! legitimate, expected replay at the transport-retry level, not an attack - and it is
//! structurally impossible for it to reach this module's detection, because
//! [`ReplayGuard`] is wired only into *inbound*, CSMS-initiated remote-control handlers
//! (`RequestStopTransaction` today); the offline queue exists solely on the *outbound* reporting
//! path (`TransactionEvent`, `SecurityEventNotification`, ...) and never touches it.
//!
//! # Why `RequestStopTransaction`, specifically
//!
//! The same technique - "did we already record success for this signature?" - was considered
//! for `UnlockConnector` (keyed on `(evse_id, connector_id)`) and `RequestStartTransaction`
//! (keyed on `(evse_id, idToken)`), but both keys can legitimately recur: a connector gets
//! plugged, unlocked, and plugged in again; a fleet idToken starts a session on the same EVSE
//! on a later, unrelated day. Reusing either key would report a normal repeat command as
//! `AttemptedReplayAttacks`. [`crate::state::TransactionId`] has no such recurrence: it is a
//! strictly monotonic counter that is never reused, even across a restart (see
//! `recovery_restores_the_transaction_id_counter_so_ids_are_never_reused` in
//! `src/state/charge_point_state.rs`), so "the CSMS asked to stop a transaction id that already
//! stopped" is a much cleaner replay signal than either of the other two. This module still
//! provides the generic [`ReplayGuard`] primitive so a future case with an equally
//! non-recurring key can reuse it.
//!
//! # False positives are accepted; false rejections are not
//!
//! A signature collision (the same key legitimately recurring) produces at worst an extra,
//! spurious `AttemptedReplayAttacks` report - never a refused command. [`ReplayGuard`] never
//! changes what a handler decides to do; it only observes an outcome that was already going to
//! happen and, when that outcome looks like a replay, additionally reports it. Rejecting a
//! genuine CSMS command because it merely *resembles* a replay would be worse than missing an
//! exotic one, per `CLAUDE.md`.

use alloc::collections::VecDeque;
use core::cell::RefCell;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

/// Default [`ReplayGuard`] capacity used by [`ReplayGuard::new`].
///
/// 16 recently-completed commands is far more than a charge point with a handful of EVSEs would
/// ever have genuinely in flight at once, while keeping the guard's footprint tiny (a handful of
/// signatures, each at most a few dozen bytes) - see `docs/PRODUCTION-ROADMAP.md` §9.2 (G2.2) for
/// the bounded-memory stance this follows. Integrators expecting a much larger number of
/// concurrently-relevant transactions should pick their own via [`ReplayGuard::with_capacity`].
pub const DEFAULT_REPLAY_GUARD_CAPACITY: usize = 16;

/// A bounded, recently-seen set of "this command signature already completed" markers, used to
/// notice when a CSMS-initiated command is asked to repeat an effect it already produced. See
/// the module docs for the full threat model and why this exists alongside (not instead of) the
/// state machine's own refusal to double-apply an effect.
///
/// Bounded at construction (see [`DEFAULT_REPLAY_GUARD_CAPACITY`] / [`Self::with_capacity`]), so
/// a long-running charge point's memory use here never grows past a fixed size (G2.2). Overflow
/// evicts the **oldest** recorded signature - the newest completions are the ones a subsequent
/// replay is most likely to reference.
///
/// Cheap to share: wrap in an [`alloc::sync::Arc`] and clone it into every closure that needs
/// to consult it; internally synchronized by the same `embassy-sync`-backed blocking mutex the
/// rest of this crate's no_std-safe shared state uses.
pub struct ReplayGuard<T> {
    seen: BlockingMutex<CriticalSectionRawMutex, RefCell<VecDeque<T>>>,
    capacity: usize,
}

impl<T: PartialEq> ReplayGuard<T> {
    /// An empty guard holding at most [`DEFAULT_REPLAY_GUARD_CAPACITY`] signatures.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_REPLAY_GUARD_CAPACITY)
    }

    /// An empty guard holding at most `capacity` signatures (clamped to at least 1 - a guard that
    /// can hold nothing would never detect anything while still costing every write).
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            seen: BlockingMutex::new(RefCell::new(VecDeque::new())),
            capacity: capacity.max(1),
        }
    }

    /// Whether `signature` was already [`record`](Self::record)ed and hasn't since been evicted.
    pub fn contains(&self, signature: &T) -> bool {
        self.seen
            .lock(|seen| seen.borrow().iter().any(|seen| seen == signature))
    }

    /// Records `signature` as having completed, evicting the oldest recorded signature if the
    /// guard is already at capacity.
    pub fn record(&self, signature: T) {
        self.seen.lock(|seen| {
            let mut seen = seen.borrow_mut();
            if seen.len() >= self.capacity {
                seen.pop_front();
            }
            seen.push_back(signature);
        });
    }
}

impl<T: PartialEq> Default for ReplayGuard<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::ReplayGuard;

    #[test]
    fn a_fresh_guard_contains_nothing() {
        let guard: ReplayGuard<u32> = ReplayGuard::new();
        assert!(!guard.contains(&1));
    }

    #[test]
    fn a_recorded_signature_is_then_contained() {
        let guard = ReplayGuard::new();
        guard.record(1);
        assert!(guard.contains(&1));
        assert!(!guard.contains(&2));
    }

    #[test]
    fn exceeding_capacity_evicts_the_oldest_signature() {
        let guard = ReplayGuard::with_capacity(2);
        guard.record(1);
        guard.record(2);
        guard.record(3);

        assert!(!guard.contains(&1));
        assert!(guard.contains(&2));
        assert!(guard.contains(&3));
    }

    #[test]
    fn capacity_is_clamped_to_at_least_one() {
        let guard = ReplayGuard::with_capacity(0);
        guard.record(1);
        assert!(guard.contains(&1));
        guard.record(2);
        assert!(!guard.contains(&1));
        assert!(guard.contains(&2));
    }
}
