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

/// What [`OfflineQueue::push`] does when the queue is already at capacity.
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
/// removes (the incoming one, or the evicted oldest one) is handed back from [`Self::push`] so a
/// caller such as [`run_with_offline_queue`] can react - typically by raising a `MemoryExhaustion`
/// security event, since a saturated offline queue signals the same operational condition as
/// approaching the wider firmware's memory limits, without needing this crate's own module to
/// depend on `crate::security` (embedded/no_std callers that don't need that reporting can just
/// ignore it).
pub struct OfflineQueue<M> {
    pending: BlockingMutex<CriticalSectionRawMutex, RefCell<VecDeque<M>>>,
    capacity: usize,
    policy: OverflowPolicy,
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
    fn push(&self, message: M) -> Option<M> {
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

impl<M: Clone> OfflineQueue<M> {
    fn peek_front(&self) -> Option<M> {
        self.pending.lock(|queue| queue.borrow().front().cloned())
    }

    fn pop_front(&self) {
        self.pending.lock(|queue| {
            queue.borrow_mut().pop_front();
        });
    }
}

/// Attempts to deliver every currently queued message, in order, via `send`. Stops at the first
/// failure - re-queuing it rather than dropping it or skipping ahead - so a later message can
/// never be delivered before an earlier one that's still stuck; that would misorder e.g.
/// `TransactionEvent`s the CSMS relies on arriving in sequence.
pub async fn flush_offline_queue<M, F, Fut, E>(queue: &OfflineQueue<M>, mut send: F)
where
    M: Clone,
    F: FnMut(M) -> Fut,
    Fut: Future<Output = Result<(), E>>,
    E: fmt::Display,
{
    while let Some(message) = queue.peek_front() {
        match send(message).await {
            Ok(()) => queue.pop_front(),
            Err(err) => {
                tracing::warn!(error = %err, "offline queue flush failed, will retry");
                break;
            }
        }
    }
}

/// Forwards every message from `events` via `send`, queuing (and retrying, in order - see
/// [`flush_offline_queue`]) any that fail rather than dropping them. Runs forever; ends only when
/// `events` closes.
///
/// `on_overflow` is called with whatever message the queue's [`OverflowPolicy`] dropped, whenever
/// pushing a newly-arrived message causes an overflow (see [`OfflineQueue::push`]) - typically
/// wired to raise a `MemoryExhaustion` security event. Pass `|_dropped| async {}` to ignore
/// overflow entirely.
pub async fn run_with_offline_queue<M, F, Fut, E, H, HFut>(
    mut events: BroadcastReceiver<M>,
    queue: &OfflineQueue<M>,
    mut send: F,
    mut on_overflow: H,
) where
    M: Clone,
    F: FnMut(M) -> Fut,
    Fut: Future<Output = Result<(), E>>,
    E: fmt::Display,
    H: FnMut(M) -> HFut,
    HFut: Future<Output = ()>,
{
    while let Ok(message) = events.recv().await {
        if let Some(dropped) = queue.push(message) {
            on_overflow(dropped).await;
        }
        flush_offline_queue(queue, &mut send).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{OfflineQueue, flush_offline_queue, run_with_offline_queue};
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
