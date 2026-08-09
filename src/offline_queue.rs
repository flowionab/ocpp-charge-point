//! Generic offline-queueing: a report that fails to send (e.g. because the CSMS connection is
//! currently down) is queued instead of dropped, and retried - in order - once delivery becomes
//! possible again, either because a later report triggers a fresh attempt or because the
//! connection reconnects (see [`crate::connection::ReconnectHandler`]). Used by the Availability,
//! Transactions, and Security functional blocks' forwarding loops; not every outbound report
//! needs this - Heartbeat is self-superseding (a missed one is moot once the next one succeeds)
//! and Authorize needs a decision *now*, not eventually, so neither goes through this. See
//! `docs/ROADMAP.md` §0.

use crate::sync::BroadcastReceiver;
use alloc::collections::VecDeque;
use core::cell::RefCell;
use core::fmt;
use core::future::Future;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

/// Default [`OfflineQueue`] capacity used by [`OfflineQueue::new`].
///
/// 100 messages bounds worst-case memory to a few tens of KB (a `TransactionEvent` with a handful
/// of meter-value samples runs well under 1 KB once encoded) while still absorbing a
/// multi-minute-to-hour CSMS outage at realistic reporting rates (status changes, transaction
/// updates, security events) before the [`OverflowPolicy`] kicks in. Callers with tighter RAM
/// budgets or longer expected outages should pick their own via [`OfflineQueue::with_capacity`]
/// rather than relying on this default.
pub const DEFAULT_CAPACITY: usize = 100;

/// What `OfflineQueue::push` (crate-private) does when the queue is already at capacity.
///
/// There is no policy that is safe for every message kind: OCPP's `TransactionEvent`s carry
/// billable energy readings and must arrive in order, so losing one is a billing-data loss,
/// while a queued `StatusNotification` is superseded by whatever the connector's status is by the
/// time the connection recovers, so an old one is close to worthless once a newer one exists.
/// [`OfflineQueue`] is generic over the message type and can't tell these apart on its own, so the
/// policy is caller-configurable per queue instance via [`OfflineQueue::with_overflow_policy`] -
/// callers should pick per message kind rather than relying on one policy everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverflowPolicy {
    /// Evict the oldest queued message to make room for the new one. Loses whatever information
    /// only the evicted message carried (e.g. a stale connector status, or - if used for
    /// transaction events - the oldest unsent billing record), but keeps the queue current with
    /// the newest activity. The default: most callers care more about "what's happening now" than
    /// "what happened first during the outage".
    #[default]
    DropOldest,
    /// Reject the new message, keeping every already-queued message intact and in place. Loses
    /// the newest information instead - a fresh status change, or a fresh transaction event -
    /// rather than disturbing what's already queued. Appropriate for `TransactionEvent`s: it never
    /// evicts an already-queued billing record, at the cost of dropping new transaction activity
    /// once the queue is saturated during a very long outage.
    DropNewest,
}

/// A FIFO queue of reports whose delivery failed, kept until [`flush_offline_queue`] (called
/// from [`run_with_offline_queue`] after every new message, and typically also from a
/// [`crate::connection::ReconnectHandler`] callback) successfully delivers them. Generic over the
/// message type `M` so each functional block's forwarding loop can use its own - see this
/// module's docs for which ones do.
///
/// Bounded at construction time (see [`DEFAULT_CAPACITY`] / [`Self::with_capacity`]) so a long
/// CSMS outage grows the queue only up to that bound rather than until allocation fails - see
/// `docs/PRODUCTION-ROADMAP.md` §9.2 (G2.1). What happens once the bound is hit is controlled by
/// [`OverflowPolicy`] (see [`Self::with_overflow_policy`]); either way, the message that overflow
/// removes (the incoming one, or the evicted oldest one) is handed back from `Self::push`
/// (crate-private) so a caller such as [`crate::offline_queue::run_with_offline_queue`] can
/// react, typically by raising a `MemoryExhaustion` security event, since a saturated offline
/// queue signals the same operational condition as approaching the wider firmware's memory
/// limits, without needing this crate's own module to depend on `crate::security`
/// (embedded/no_std callers that don't need that reporting can just ignore it).
pub struct OfflineQueue<M> {
    pending: BlockingMutex<CriticalSectionRawMutex, RefCell<VecDeque<M>>>,
    capacity: usize,
    policy: OverflowPolicy,
    /// Whether a flush is in progress. [`flush_offline_queue`] peeks the front message, sends it,
    /// and pops only on success - so two flushes running at once would both peek the *same*
    /// message and send it twice. There are now three things that can start one (a new message
    /// arriving, a reconnect, and the retry timer), so this makes a concurrent flush skip rather
    /// than duplicate. An `AtomicBool` rather than a mutex because the guard has to survive an
    /// `.await`, which the `embassy-sync` blocking mutex this crate's no_std-safe state uses
    /// cannot.
    flushing: AtomicBool,
    /// How many times the *current front message* has been attempted and failed. Reset whenever
    /// the front changes, so it counts attempts at one message rather than at the queue.
    ///
    /// Kept beside the queue rather than inside each entry so the snapshot/restore path
    /// ([`Self::snapshot`], used by `crate::persistence`) keeps carrying plain messages: attempt
    /// counts are a property of *this* connection's attempts, and a count restored from before a
    /// reboot would be counting against a CSMS link that no longer exists.
    front_attempts: AtomicU32,
}

impl<M> OfflineQueue<M> {
    /// An empty queue with [`DEFAULT_CAPACITY`] and [`OverflowPolicy::DropOldest`].
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// An empty queue holding at most `capacity` messages (clamped to at least 1 - a queue that
    /// can hold nothing would drop every message immediately), using [`OverflowPolicy::DropOldest`]
    /// unless overridden via [`Self::with_overflow_policy`].
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            pending: BlockingMutex::new(RefCell::new(VecDeque::new())),
            capacity: capacity.max(1),
            policy: OverflowPolicy::default(),
            flushing: AtomicBool::new(false),
            front_attempts: AtomicU32::new(0),
        }
    }

    /// Overrides the [`OverflowPolicy`] used once the queue reaches capacity. See that type's
    /// docs for how to choose one per message kind.
    pub fn with_overflow_policy(mut self, policy: OverflowPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Queues `message`, applying [`OverflowPolicy`] if the queue is already at capacity. Returns
    /// the message that overflow caused to be dropped - `message` itself under
    /// [`OverflowPolicy::DropNewest`], or the previously-oldest queued message under
    /// [`OverflowPolicy::DropOldest`] - or `None` if `message` was queued without dropping
    /// anything.
    pub(crate) fn push(&self, message: M) -> Option<M> {
        self.pending.lock(|queue| {
            let mut queue = queue.borrow_mut();
            if queue.len() >= self.capacity {
                match self.policy {
                    OverflowPolicy::DropOldest => {
                        let dropped = queue.pop_front();
                        queue.push_back(message);
                        dropped
                    }
                    OverflowPolicy::DropNewest => Some(message),
                }
            } else {
                queue.push_back(message);
                None
            }
        })
    }
}

impl<M> Default for OfflineQueue<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M> OfflineQueue<M> {
    /// The queue's configured capacity - see [`Self::with_capacity`].
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Claims the right to flush, or reports that someone else already holds it. Released by
    /// [`Self::end_flush`]; see [`Self::flushing`]'s docs for why this exists.
    fn try_begin_flush(&self) -> bool {
        !self.flushing.swap(true, Ordering::SeqCst)
    }

    fn end_flush(&self) {
        self.flushing.store(false, Ordering::SeqCst);
    }

    /// Records a failed attempt at the front message and reports how many it has now had.
    fn record_failed_attempt(&self) -> u32 {
        self.front_attempts.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Forgets the attempt count, because the front message changed (delivered or dropped).
    fn reset_attempts(&self) {
        self.front_attempts.store(0, Ordering::SeqCst);
    }
}

impl<M: Clone> OfflineQueue<M> {
    fn peek_front(&self) -> Option<M> {
        self.pending.lock(|queue| queue.borrow().front().cloned())
    }

    fn pop_front(&self) {
        self.pending.lock(|queue| {
            queue.borrow_mut().pop_front();
        });
        self.reset_attempts();
    }

    /// How many messages are currently queued. Used by [`crate::persistence`]'s queue-persistence
    /// write policy to decide whether a change is worth a flash write, without needing to clone
    /// the whole backlog just to measure it.
    pub fn len(&self) -> usize {
        self.pending.lock(|queue| queue.borrow().len())
    }

    /// Whether the queue currently holds nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every currently queued message, oldest first - the exact order [`flush_offline_queue`]
    /// would deliver them in. Used to snapshot the backlog for durable storage
    /// (`docs/PRODUCTION-ROADMAP.md` §7.4, E4.3); not used on the hot delivery path, which uses
    /// `Self::peek_front`/`Self::pop_front` (both private) instead to avoid cloning the whole
    /// queue on every message.
    pub fn snapshot(&self) -> alloc::vec::Vec<M> {
        self.pending
            .lock(|queue| queue.borrow().iter().cloned().collect())
    }

    /// Pushes every message from `messages`, in order, through `Self::push` (crate-private) - so a backlog
    /// restored from durable storage after a reboot still respects this queue's capacity and
    /// [`OverflowPolicy`] exactly as if the messages had arrived one at a time while running.
    /// Returns every message the overflow policy dropped while restoring (in the order they were
    /// dropped), so a caller can log or report on a backlog that didn't fully fit.
    pub fn restore_backlog(&self, messages: alloc::vec::Vec<M>) -> alloc::vec::Vec<M> {
        let mut dropped = alloc::vec::Vec::new();
        for message in messages {
            if let Some(evicted) = self.push(message) {
                dropped.push(evicted);
            }
        }
        dropped
    }
}

/// Attempts to deliver every currently queued message, in order, via `send`. Stops at the first
/// failure - re-queuing it rather than dropping it or skipping ahead - so a later message can
/// never be delivered before an earlier one that's still stuck; that would misorder e.g.
/// `TransactionEvent`s the CSMS relies on arriving in sequence.
pub async fn flush_offline_queue<M, F, Fut, E>(queue: &OfflineQueue<M>, send: F)
where
    M: Clone,
    F: FnMut(M) -> Fut,
    Fut: Future<Output = Result<(), E>>,
    E: fmt::Display,
{
    flush_offline_queue_with_attempts(queue, None, send).await
}

/// [`flush_offline_queue`], with a cap on how many times one message may be attempted before it is
/// given up on - OCPP's `OCPPCommCtrlr`/`MessageAttempts[TransactionEvent]` (A6).
///
/// **This is head-of-line unblocking, and that is the point.** Without a cap, a message the CSMS
/// will never accept - a malformed report, a transaction the CSMS has already closed - is retried
/// forever at the front of the queue, and every message behind it waits on that retry. The queue
/// then stops being an outage buffer and becomes a permanent blockage. With a cap, the stuck
/// message is dropped after `max_attempts` failures and the rest drain.
///
/// Dropping is logged at error level, and deliberately not softened: for a transaction event it is
/// billable data leaving the charge point unreported, which an operator needs to see. `None`
/// disables the cap entirely, which is the old behaviour and remains right for a caller who would
/// rather block than lose anything.
pub async fn flush_offline_queue_with_attempts<M, F, Fut, E>(
    queue: &OfflineQueue<M>,
    max_attempts: Option<u32>,
    mut send: F,
) where
    M: Clone,
    F: FnMut(M) -> Fut,
    Fut: Future<Output = Result<(), E>>,
    E: fmt::Display,
{
    // A flush already in progress is doing this work; joining in would peek the same front
    // message and send it twice. Returning is safe rather than lossy - the flush that holds the
    // claim drains everything queued, including whatever this caller had just pushed.
    if !queue.try_begin_flush() {
        return;
    }
    while let Some(message) = queue.peek_front() {
        match send(message).await {
            Ok(()) => queue.pop_front(),
            Err(err) => {
                let attempts = queue.record_failed_attempt();
                match max_attempts {
                    Some(max) if attempts >= max => {
                        tracing::error!(
                            error = %err,
                            attempts,
                            "giving up on a queued message after the configured attempts and \
                             dropping it - the messages behind it were blocked by this one"
                        );
                        queue.pop_front();
                        continue;
                    }
                    _ => {
                        tracing::warn!(
                            error = %err,
                            attempts,
                            "offline queue flush failed, will retry"
                        );
                        break;
                    }
                }
            }
        }
    }
    queue.end_flush();
}

/// The `(Component, Variable)` OCPP defines for how many times a queued message may be attempted -
/// `OCPPCommCtrlr`/`MessageAttempts[TransactionEvent]`.
fn message_attempts_variable() -> (crate::state::Component, crate::state::Variable) {
    (
        crate::state::Component {
            name: "OCPPCommCtrlr".into(),
            instance: None,
            evse: None,
        },
        crate::state::Variable {
            name: "MessageAttempts".into(),
            instance: Some("TransactionEvent".into()),
        },
    )
}

/// How many times a queued message may be attempted before it is dropped, from
/// `OCPPCommCtrlr`/`MessageAttempts[TransactionEvent]`.
///
/// `None` - no cap, retry forever - when the variable is absent, unparseable or `0`. Zero meaning
/// "unlimited" rather than "never try" is deliberate and matches how OCPP's other `0`-valued
/// intervals read in this crate: a charge point that dropped every report on its first failure
/// would be worse than one that blocks, and nobody configuring `0` can plausibly mean that.
pub fn message_attempts(actor: &crate::actor::ChargePointActor) -> Option<u32> {
    let (component, variable) = message_attempts_variable();
    actor
        .state()
        .device_model
        .get(&component, &variable)
        .and_then(|definition| definition.attribute(crate::state::VariableAttributeType::Actual))
        .and_then(|attribute| attribute.value.parse::<u32>().ok())
        .filter(|attempts| *attempts != 0)
}

/// The `(Component, Variable)` OCPP defines for how long to wait between attempts at a queued
/// message - `OCPPCommCtrlr`/`MessageAttemptInterval[TransactionEvent]`.
fn message_attempt_interval_variable() -> (crate::state::Component, crate::state::Variable) {
    (
        crate::state::Component {
            name: "OCPPCommCtrlr".into(),
            instance: None,
            evse: None,
        },
        crate::state::Variable {
            name: "MessageAttemptInterval".into(),
            instance: Some("TransactionEvent".into()),
        },
    )
}

/// How long to wait between retry sweeps of an offline queue, from
/// `OCPPCommCtrlr`/`MessageAttemptInterval[TransactionEvent]`, or `fallback` when it is absent,
/// unparseable or `0` - a `0` would turn [`run_offline_queue_retries`] into a busy-spin, so it is
/// treated as "not set" exactly as [`crate::provisioning::run_heartbeat`] treats a `0` interval.
///
/// OCPP scopes this variable to `TransactionEvent`, and this crate applies it to every offline
/// queue rather than only that one. The alternative - inventing separate intervals for status and
/// security events, which OCPP does not define - would be a configuration surface no CSMS knows
/// how to drive.
pub fn message_attempt_interval_secs(actor: &crate::actor::ChargePointActor, fallback: u32) -> u32 {
    let (component, variable) = message_attempt_interval_variable();
    actor
        .state()
        .device_model
        .get(&component, &variable)
        .and_then(|definition| definition.attribute(crate::state::VariableAttributeType::Actual))
        .and_then(|attribute| attribute.value.parse::<u32>().ok())
        .filter(|interval| *interval != 0)
        .unwrap_or(fallback)
}

/// Retries whatever is sitting in `queue` every
/// `OCPPCommCtrlr`/`MessageAttemptInterval[TransactionEvent]` seconds, forever.
///
/// Without this, a queued report is only retried when *new* traffic arrives or the connection
/// reconnects - so the last report before an outage could sit indefinitely on a charge point that
/// went quiet, which is exactly the charge point most likely to have gone quiet *because* it is
/// offline. The interval is re-read every cycle, so a CSMS changing it takes effect on the next
/// sweep without a reboot.
///
/// Concurrent with the forwarder's own flush and the reconnect flush; [`flush_offline_queue`]'s
/// claim makes the overlap a no-op rather than a double-send.
pub async fn run_offline_queue_retries<M, B, F, Fut, E>(
    queue: &OfflineQueue<M>,
    backoff: &B,
    actor: &crate::actor::ChargePointActor,
    fallback_interval_secs: u32,
    mut send: F,
) where
    M: Clone,
    B: crate::provisioning::Backoff,
    F: FnMut(M) -> Fut,
    Fut: Future<Output = Result<(), E>>,
    E: fmt::Display,
{
    loop {
        backoff
            .wait(message_attempt_interval_secs(actor, fallback_interval_secs))
            .await;
        if queue.is_empty() {
            continue;
        }
        flush_offline_queue_with_attempts(queue, message_attempts(actor), &mut send).await;
    }
}

/// Forwards every message from `events` via `send`, queuing (and retrying, in order - see
/// [`flush_offline_queue`]) any that fail rather than dropping them. Runs forever; ends only when
/// `events` closes.
///
/// `on_overflow` is called with whatever message the queue's [`OverflowPolicy`] dropped, whenever
/// pushing a newly-arrived message causes an overflow (see `OfflineQueue::push`, crate-private) - typically
/// wired to raise a `MemoryExhaustion` security event. Pass `|_dropped| async {}` to ignore
/// overflow entirely.
pub async fn run_with_offline_queue<M, F, Fut, E, H, HFut>(
    events: BroadcastReceiver<M>,
    queue: &OfflineQueue<M>,
    send: F,
    on_overflow: H,
) where
    M: Clone,
    F: FnMut(M) -> Fut,
    Fut: Future<Output = Result<(), E>>,
    E: fmt::Display,
    H: FnMut(M) -> HFut,
    HFut: Future<Output = ()>,
{
    run_with_offline_queue_where(events, queue, |_| true, send, on_overflow).await;
}

/// [`run_with_offline_queue`], but only messages `should_send` accepts are queued and sent.
///
/// The rejected ones are dropped *before* the queue rather than filtered at the send, which is the
/// whole point: the queue is bounded and evicts its oldest entry on overflow, so anything allowed
/// into it can push something else out. A message class that must never be able to displace
/// another has to be kept out of the queue entirely, not merely skipped on the way to the wire.
///
/// The Security block is what needs this - see
/// [`SecurityEventType::is_critical`](crate::state::SecurityEventType::is_critical), where the
/// distinction stops an attacker flooding remotely-triggerable non-critical events to evict a
/// queued tamper report.
pub async fn run_with_offline_queue_where<M, P, F, Fut, E, H, HFut>(
    mut events: BroadcastReceiver<M>,
    queue: &OfflineQueue<M>,
    mut should_send: P,
    mut send: F,
    mut on_overflow: H,
) where
    M: Clone,
    P: FnMut(&M) -> bool,
    F: FnMut(M) -> Fut,
    Fut: Future<Output = Result<(), E>>,
    E: fmt::Display,
    H: FnMut(M) -> HFut,
    HFut: Future<Output = ()>,
{
    while let Ok(message) = events.recv().await {
        if !should_send(&message) {
            continue;
        }
        if let Some(dropped) = queue.push(message) {
            on_overflow(dropped).await;
        }
        flush_offline_queue(queue, &mut send).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        OfflineQueue, flush_offline_queue, flush_offline_queue_with_attempts,
        message_attempt_interval_secs, message_attempts, run_offline_queue_retries,
        run_with_offline_queue,
    };
    use crate::sync::broadcast_channel;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::watch;

    #[derive(Debug)]
    struct SendError;

    impl core::fmt::Display for SendError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("send failed")
        }
    }

    #[tokio::test]
    async fn a_successful_send_is_delivered_exactly_once() {
        let sender = broadcast_channel();
        let receiver = sender.subscribe();
        let queue = OfflineQueue::new();
        let (seen_tx, mut seen_rx) = watch::channel(Vec::new());

        let forwarder = tokio::spawn(async move {
            run_with_offline_queue(
                receiver,
                &queue,
                |message: i32| {
                    let seen_tx = seen_tx.clone();
                    async move {
                        seen_tx.send_modify(|seen| seen.push(message));
                        Ok::<(), SendError>(())
                    }
                },
                |_dropped| async {},
            )
            .await;
        });

        sender.send(1);
        seen_rx
            .wait_for(|seen| !seen.is_empty())
            .await
            .expect("forwarder is still running");
        drop(sender);
        forwarder.await.unwrap();

        assert_eq!(*seen_rx.borrow(), alloc::vec![1]);
    }

    #[tokio::test]
    async fn a_failed_send_is_queued_and_retried_when_the_next_message_arrives() {
        let sender = broadcast_channel();
        let receiver = sender.subscribe();
        let queue = OfflineQueue::new();
        let should_fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (seen_tx, mut seen_rx) = watch::channel(Vec::new());

        let task_should_fail = should_fail.clone();
        let forwarder = tokio::spawn(async move {
            run_with_offline_queue(
                receiver,
                &queue,
                move |message: i32| {
                    let seen_tx = seen_tx.clone();
                    let should_fail = task_should_fail.clone();
                    async move {
                        if should_fail.load(Ordering::SeqCst) {
                            return Err(SendError);
                        }
                        seen_tx.send_modify(|seen| seen.push(message));
                        Ok(())
                    }
                },
                |_dropped| async {},
            )
            .await;
        });

        // Sent while `should_fail` is true - queued, not delivered.
        sender.send(1);
        tokio::time::sleep(core::time::Duration::from_millis(10)).await;
        assert!(seen_rx.borrow().is_empty());

        // The connection "comes back"; the next message triggers a flush that delivers both the
        // queued first message and this new one, in order.
        should_fail.store(false, Ordering::SeqCst);
        sender.send(2);
        seen_rx
            .wait_for(|seen| seen.len() == 2)
            .await
            .expect("forwarder is still running");

        drop(sender);
        forwarder.await.unwrap();

        assert_eq!(*seen_rx.borrow(), alloc::vec![1, 2]);
    }

    #[tokio::test]
    async fn flushing_stops_at_the_first_failure_without_skipping_ahead() {
        let queue = OfflineQueue::new();
        // Reach into the queue directly (this test module is a descendant of `offline_queue`,
        // so it can) to set up a backlog without going through the channel-driven path.
        queue.push(1);
        queue.push(2);
        let attempted = Arc::new(AtomicUsize::new(0));

        let task_attempted = attempted.clone();
        flush_offline_queue(&queue, |_message: i32| {
            let attempted = task_attempted.clone();
            async move {
                attempted.fetch_add(1, Ordering::SeqCst);
                Err::<(), SendError>(SendError)
            }
        })
        .await;

        // Only the first (still-stuck) message was ever attempted - `2` was never reached.
        assert_eq!(attempted.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn drop_oldest_evicts_the_front_message_to_make_room() {
        let queue = OfflineQueue::with_capacity(2);
        assert_eq!(queue.push(1), None);
        assert_eq!(queue.push(2), None);
        // Queue is now full at [1, 2]; pushing 3 evicts 1.
        assert_eq!(queue.push(3), Some(1));
        assert_eq!(queue.peek_front(), Some(2));
    }

    #[test]
    fn drop_newest_rejects_the_incoming_message_and_keeps_the_queue_intact() {
        let queue =
            OfflineQueue::with_capacity(2).with_overflow_policy(super::OverflowPolicy::DropNewest);
        assert_eq!(queue.push(1), None);
        assert_eq!(queue.push(2), None);
        // Queue is full at [1, 2]; pushing 3 is rejected, 3 itself comes back.
        assert_eq!(queue.push(3), Some(3));
        assert_eq!(queue.peek_front(), Some(1));
    }

    #[test]
    fn capacity_is_clamped_to_at_least_one() {
        let queue: OfflineQueue<i32> = OfflineQueue::with_capacity(0);
        assert_eq!(queue.push(1), None);
        // Capacity-0 was clamped to 1, so a second push overflows immediately.
        assert_eq!(queue.push(2), Some(1));
    }

    #[test]
    fn snapshot_returns_every_queued_message_oldest_first() {
        let queue = OfflineQueue::new();
        assert_eq!(queue.snapshot(), Vec::<i32>::new());
        queue.push(1);
        queue.push(2);
        queue.push(3);
        assert_eq!(queue.snapshot(), alloc::vec![1, 2, 3]);
        assert_eq!(queue.len(), 3);
        assert!(!queue.is_empty());
    }

    #[test]
    fn restore_backlog_pushes_every_message_in_order_respecting_capacity_and_policy() {
        let queue = OfflineQueue::with_capacity(2);
        let dropped = queue.restore_backlog(alloc::vec![1, 2, 3]);
        // Capacity 2, DropOldest: restoring [1, 2, 3] evicts 1 to make room for 3.
        assert_eq!(dropped, alloc::vec![1]);
        assert_eq!(queue.snapshot(), alloc::vec![2, 3]);
    }

    #[tokio::test]
    async fn a_second_flush_running_concurrently_does_not_send_the_same_message_twice() {
        // Three things can start a flush - a new message, a reconnect, and the retry timer - so
        // the front message would otherwise be peeked by two of them and sent twice.
        let queue = alloc::sync::Arc::new(OfflineQueue::new());
        queue.push(1);
        let sent = Arc::new(AtomicUsize::new(0));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let release_rx = Arc::new(tokio::sync::Mutex::new(Some(release_rx)));

        let slow_queue = queue.clone();
        let slow_sent = sent.clone();
        let slow = tokio::spawn(async move {
            flush_offline_queue(&slow_queue, move |_message: i32| {
                let sent = slow_sent.clone();
                let release_rx = release_rx.clone();
                async move {
                    sent.fetch_add(1, Ordering::SeqCst);
                    // Hold the flush open until the second one has had its chance to interfere.
                    if let Some(rx) = release_rx.lock().await.take() {
                        let _ = rx.await;
                    }
                    Ok::<(), SendError>(())
                }
            })
            .await;
        });

        // Let the first flush reach its in-flight send.
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }

        let second_sent = sent.clone();
        flush_offline_queue(&queue, move |_message: i32| {
            let sent = second_sent.clone();
            async move {
                sent.fetch_add(1, Ordering::SeqCst);
                Ok::<(), SendError>(())
            }
        })
        .await;

        assert_eq!(
            sent.load(Ordering::SeqCst),
            1,
            "the concurrent flush should have skipped, not re-sent the in-flight message"
        );

        let _ = release_tx.send(());
        slow.await.unwrap();
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn a_flush_that_finished_leaves_the_queue_flushable_again() {
        // The claim must be released on the failure path too, or one failed send would wedge the
        // queue shut for the life of the process.
        let queue = OfflineQueue::new();
        queue.push(1);

        flush_offline_queue(&queue, |_message: i32| async { Err::<(), _>(SendError) }).await;
        assert_eq!(queue.len(), 1);

        let delivered = Arc::new(AtomicUsize::new(0));
        let counted = delivered.clone();
        flush_offline_queue(&queue, move |_message: i32| {
            let delivered = counted.clone();
            async move {
                delivered.fetch_add(1, Ordering::SeqCst);
                Ok::<(), SendError>(())
            }
        })
        .await;

        assert_eq!(delivered.load(Ordering::SeqCst), 1);
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn a_message_the_csms_will_never_accept_stops_blocking_the_ones_behind_it() {
        // The failure this exists for: without a cap, a permanently-rejected message is retried
        // forever at the front and everything behind it waits on that retry - the queue stops
        // being an outage buffer and becomes a permanent blockage.
        let queue = OfflineQueue::new();
        queue.push(1); // the CSMS will never accept this one
        queue.push(2);
        let delivered = Arc::new(std::sync::Mutex::new(Vec::new()));

        for _ in 0..3 {
            let seen = delivered.clone();
            flush_offline_queue_with_attempts(&queue, Some(3), move |message: i32| {
                let seen = seen.clone();
                async move {
                    if message == 1 {
                        return Err(SendError);
                    }
                    seen.lock().unwrap().push(message);
                    Ok(())
                }
            })
            .await;
        }

        assert_eq!(
            *delivered.lock().unwrap(),
            alloc::vec![2],
            "the message behind the stuck one should have gone out once the cap was reached"
        );
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn attempts_are_counted_per_message_not_per_queue() {
        // A queue that failed twice on an earlier message must not give up early on the next one.
        let queue = OfflineQueue::new();
        queue.push(1);
        queue.push(2);
        let attempts = Arc::new(AtomicUsize::new(0));

        // Two failures against message 1, then it succeeds - the counter must reset for 2.
        for _ in 0..2 {
            flush_offline_queue_with_attempts(&queue, Some(3), |_message: i32| async {
                Err::<(), _>(SendError)
            })
            .await;
        }
        let counted = attempts.clone();
        flush_offline_queue_with_attempts(&queue, Some(3), move |message: i32| {
            let attempts = counted.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                // Message 2 fails once; with a per-queue counter it would already be at the cap.
                if message == 2 {
                    return Err(SendError);
                }
                Ok(())
            }
        })
        .await;

        assert_eq!(
            queue.len(),
            1,
            "message 2 should still be queued, not dropped"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn no_cap_means_retry_forever_which_is_still_the_default() {
        let queue = OfflineQueue::new();
        queue.push(1);

        for _ in 0..10 {
            flush_offline_queue(&queue, |_message: i32| async { Err::<(), _>(SendError) }).await;
        }

        assert_eq!(queue.len(), 1, "an uncapped queue keeps the message");
    }

    #[tokio::test]
    async fn the_attempt_cap_comes_from_the_device_model_and_zero_means_unlimited() {
        use crate::actor::ChargePointActor;
        use crate::executor::TokioExecutor;
        use crate::state::{ChargePointEvent, DeviceModelEvent, VariableAttributeType};

        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        // B1.7 registers this with a value of 3.
        assert_eq!(message_attempts(&actor), Some(3));

        let set = |value: &str| {
            ChargePointEvent::DeviceModel(DeviceModelEvent::AttributeValueSet {
                component: crate::state::Component {
                    name: "OCPPCommCtrlr".into(),
                    instance: None,
                    evse: None,
                },
                variable: crate::state::Variable {
                    name: "MessageAttempts".into(),
                    instance: Some("TransactionEvent".into()),
                },
                attribute_type: VariableAttributeType::Actual,
                value: value.into(),
            })
        };

        let _ = actor.send(set("10")).await;
        assert_eq!(message_attempts(&actor), Some(10));
        // `0` reads as unlimited, not as "drop on first failure" - see `message_attempts`' docs.
        let _ = actor.send(set("0")).await;
        assert_eq!(message_attempts(&actor), None);
    }

    #[tokio::test]
    async fn the_retry_timer_drains_a_queue_nothing_else_is_touching() {
        use crate::actor::ChargePointActor;
        use crate::executor::TokioExecutor;

        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let queue = alloc::sync::Arc::new(OfflineQueue::new());
        queue.push(7);
        let delivered = Arc::new(std::sync::Mutex::new(Vec::new()));

        struct ImmediateBackoff;

        #[async_trait::async_trait]
        impl crate::provisioning::Backoff for ImmediateBackoff {
            async fn wait(&self, _seconds: u32) {
                tokio::task::yield_now().await;
            }
        }

        let task_queue = queue.clone();
        let task_delivered = delivered.clone();
        let task_actor = actor.clone();
        let task = tokio::spawn(async move {
            run_offline_queue_retries(
                &task_queue,
                &ImmediateBackoff,
                &task_actor,
                60,
                move |message: i32| {
                    let delivered = task_delivered.clone();
                    async move {
                        delivered.lock().unwrap().push(message);
                        Ok::<(), SendError>(())
                    }
                },
            )
            .await;
        });

        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        task.abort();

        // No new message arrived and no reconnect happened - the timer alone got it out.
        assert_eq!(*delivered.lock().unwrap(), alloc::vec![7]);
    }

    #[tokio::test]
    async fn the_retry_interval_comes_from_the_device_model_and_falls_back_sanely() {
        use crate::actor::ChargePointActor;
        use crate::executor::TokioExecutor;
        use crate::state::{ChargePointEvent, DeviceModelEvent, VariableAttributeType};

        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        // B1.7 registers this with a value of 60.
        assert_eq!(message_attempt_interval_secs(&actor, 30), 60);

        let set = |value: &str| {
            ChargePointEvent::DeviceModel(DeviceModelEvent::AttributeValueSet {
                component: crate::state::Component {
                    name: "OCPPCommCtrlr".into(),
                    instance: None,
                    evse: None,
                },
                variable: crate::state::Variable {
                    name: "MessageAttemptInterval".into(),
                    instance: Some("TransactionEvent".into()),
                },
                attribute_type: VariableAttributeType::Actual,
                value: value.into(),
            })
        };

        let _ = actor.send(set("15")).await;
        assert_eq!(message_attempt_interval_secs(&actor, 30), 15);

        // A `0` would be a busy-spin, and an unparseable value is not a number - both fall back.
        let _ = actor.send(set("0")).await;
        assert_eq!(message_attempt_interval_secs(&actor, 30), 30);
        let _ = actor.send(set("soon")).await;
        assert_eq!(message_attempt_interval_secs(&actor, 30), 30);
    }

    #[tokio::test]
    async fn run_with_offline_queue_reports_overflow_via_the_callback() {
        let sender = broadcast_channel();
        let receiver = sender.subscribe();
        let queue = OfflineQueue::with_capacity(1);
        let (overflowed_tx, mut overflowed_rx) = watch::channel(Vec::new());

        let forwarder = tokio::spawn(async move {
            run_with_offline_queue(
                receiver,
                &queue,
                // Every send fails, so nothing ever drains and the bound is what forces drops.
                |_message: i32| async { Err::<(), SendError>(SendError) },
                move |dropped: i32| {
                    let overflowed_tx = overflowed_tx.clone();
                    async move {
                        overflowed_tx.send_modify(|seen| seen.push(dropped));
                    }
                },
            )
            .await;
        });

        sender.send(1);
        sender.send(2);
        overflowed_rx
            .wait_for(|seen| !seen.is_empty())
            .await
            .expect("forwarder is still running");

        drop(sender);
        forwarder.await.unwrap();

        // Capacity 1, DropOldest by default: pushing 2 evicts 1.
        assert_eq!(*overflowed_rx.borrow(), alloc::vec![1]);
    }
}
