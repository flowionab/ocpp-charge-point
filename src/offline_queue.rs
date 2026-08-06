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

/// A FIFO queue of reports whose delivery failed, kept until [`flush_offline_queue`] (called
/// from [`run_with_offline_queue`] after every new message, and typically also from a
/// [`crate::connection::ReconnectHandler`] callback) successfully delivers them. Generic over the
/// message type `M` so each functional block's forwarding loop can use its own - see this
/// module's docs for which ones do.
pub struct OfflineQueue<M> {
    pending: BlockingMutex<CriticalSectionRawMutex, RefCell<VecDeque<M>>>,
}

impl<M> OfflineQueue<M> {
    /// An empty queue.
    pub fn new() -> Self {
        Self {
            pending: BlockingMutex::new(RefCell::new(VecDeque::new())),
        }
    }

    fn push(&self, message: M) {
        self.pending
            .lock(|queue| queue.borrow_mut().push_back(message));
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
pub async fn run_with_offline_queue<M, F, Fut, E>(
    mut events: BroadcastReceiver<M>,
    queue: &OfflineQueue<M>,
    mut send: F,
) where
    M: Clone,
    F: FnMut(M) -> Fut,
    Fut: Future<Output = Result<(), E>>,
    E: fmt::Display,
{
    while let Ok(message) = events.recv().await {
        queue.push(message);
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
            run_with_offline_queue(receiver, &queue, |message: i32| {
                let seen_tx = seen_tx.clone();
                async move {
                    seen_tx.send_modify(|seen| seen.push(message));
                    Ok::<(), SendError>(())
                }
            })
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
            run_with_offline_queue(receiver, &queue, move |message: i32| {
                let seen_tx = seen_tx.clone();
                let should_fail = task_should_fail.clone();
                async move {
                    if should_fail.load(Ordering::SeqCst) {
                        return Err(SendError);
                    }
                    seen_tx.send_modify(|seen| seen.push(message));
                    Ok(())
                }
            })
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
}
