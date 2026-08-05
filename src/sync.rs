//! no_std+alloc-friendly replacements for the `tokio::sync::{oneshot, mpsc, watch, broadcast}`
//! primitives the actor used to depend on directly. Built on `embassy-sync`'s blocking
//! `Mutex`/`Signal`, fixed to `CriticalSectionRawMutex` (works under both std, via
//! `critical-section`'s `std` backend, and embedded, via whatever backend the embedded app
//! registers with `critical_section::set_impl!`). Mirrors `ocpp-client`'s own `src/sync.rs`
//! (see `docs/ROADMAP.md` §0), extended with `Watch` and payload-carrying `Broadcast` types
//! that crate didn't need.
//!
//! Every queue here is unbounded (`VecDeque`, not a fixed-capacity ring buffer): simpler to
//! implement correctly without an async mutex, and no_std-friendly (no large fixed slot count
//! reserved up front) - the tradeoff is that `Broadcast` has no `tokio::sync::broadcast`-style
//! "Lagged" case. A stalled subscriber grows its queue instead of dropping messages;
//! acceptable for OCPP-rate events, but worth remembering if this is ever reused somewhere
//! high-throughput.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;

type Lock<T> = BlockingMutex<CriticalSectionRawMutex, RefCell<T>>;

/// Single-value handoff between one sender and one receiver, replacing
/// `tokio::sync::oneshot::{Sender,Receiver}`. `send`/`wait` can be called from independent
/// clones (both hold the same underlying `Signal`).
pub(crate) struct OneShot<T> {
    signal: Arc<Signal<CriticalSectionRawMutex, T>>,
}

impl<T> OneShot<T> {
    pub(crate) fn new() -> Self {
        Self {
            signal: Arc::new(Signal::new()),
        }
    }

    pub(crate) fn send(&self, value: T) {
        self.signal.signal(value);
    }

    pub(crate) async fn wait(&self) -> T {
        self.signal.wait().await
    }
}

impl<T> Clone for OneShot<T> {
    fn clone(&self) -> Self {
        Self {
            signal: self.signal.clone(),
        }
    }
}

struct ChanInner<T> {
    queue: Lock<VecDeque<T>>,
    signal: Signal<CriticalSectionRawMutex, ()>,
}

/// Unbounded multi-producer, single-consumer mailbox, replacing the bounded
/// `tokio::sync::mpsc::channel` used for the actor's event queue.
pub(crate) struct Chan<T> {
    inner: Arc<ChanInner<T>>,
}

impl<T> Chan<T> {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(ChanInner {
                queue: Lock::new(RefCell::new(VecDeque::new())),
                signal: Signal::new(),
            }),
        }
    }

    pub(crate) fn send(&self, value: T) {
        self.inner
            .queue
            .lock(|queue| queue.borrow_mut().push_back(value));
        self.inner.signal.signal(());
    }

    /// Waits for the next queued value. `Signal`'s "latch" semantics (a `signal()` call before
    /// `wait()` is still observed) make the check-queue-then-wait-then-retry loop race-free.
    pub(crate) async fn recv(&self) -> T {
        loop {
            if let Some(value) = self
                .inner
                .queue
                .lock(|queue| queue.borrow_mut().pop_front())
            {
                return value;
            }
            self.inner.signal.wait().await;
        }
    }
}

impl<T> Clone for Chan<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

struct WatchInner<T> {
    state: Lock<(u64, T)>,
    subscribers: Lock<Vec<Arc<Signal<CriticalSectionRawMutex, ()>>>>,
}

/// Replaces `tokio::sync::watch::Sender`: holds the latest value, and wakes every subscribed
/// [`WatchReceiver`] on every `send_replace`.
pub(crate) struct WatchSender<T> {
    inner: Arc<WatchInner<T>>,
}

/// Replaces `tokio::sync::watch::Receiver`: `borrow()` reads the latest value synchronously;
/// `changed()` waits until a value newer than the one this receiver last observed arrives.
/// Each clone (via [`WatchReceiver::clone`]) tracks its own "last observed version"
/// independently, exactly like tokio's watch.
pub struct WatchReceiver<T> {
    inner: Arc<WatchInner<T>>,
    signal: Arc<Signal<CriticalSectionRawMutex, ()>>,
    seen_version: u64,
}

pub(crate) fn watch_channel<T: Clone>(initial: T) -> (WatchSender<T>, WatchReceiver<T>) {
    let inner = Arc::new(WatchInner {
        state: Lock::new(RefCell::new((0, initial))),
        subscribers: Lock::new(RefCell::new(Vec::new())),
    });
    let signal = Arc::new(Signal::new());
    inner
        .subscribers
        .lock(|subscribers| subscribers.borrow_mut().push(signal.clone()));
    let receiver = WatchReceiver {
        inner: inner.clone(),
        signal,
        seen_version: 0,
    };
    (WatchSender { inner }, receiver)
}

impl<T: Clone> WatchSender<T> {
    pub(crate) fn send_replace(&self, value: T) {
        self.inner.state.lock(|state| {
            let mut state = state.borrow_mut();
            state.0 += 1;
            state.1 = value;
        });
        self.inner.subscribers.lock(|subscribers| {
            for signal in subscribers.borrow().iter() {
                signal.signal(());
            }
        });
    }
}

impl<T: Clone> WatchReceiver<T> {
    /// The current value.
    pub fn borrow(&self) -> T {
        self.inner.state.lock(|state| state.borrow().1.clone())
    }

    /// Waits until the value has changed since this receiver last observed it (via `borrow()`
    /// after a prior `changed()`, or since this receiver was created/cloned).
    pub async fn changed(&mut self) {
        loop {
            let current_version = self.inner.state.lock(|state| state.borrow().0);
            if current_version != self.seen_version {
                self.seen_version = current_version;
                return;
            }
            self.signal.wait().await;
        }
    }
}

impl<T> Clone for WatchReceiver<T> {
    fn clone(&self) -> Self {
        let signal = Arc::new(Signal::new());
        self.inner
            .subscribers
            .lock(|subscribers| subscribers.borrow_mut().push(signal.clone()));
        Self {
            inner: self.inner.clone(),
            signal,
            seen_version: self.seen_version,
        }
    }
}

/// Everything that can go wrong receiving from a [`BroadcastReceiver`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    /// Every [`BroadcastSender`] clone has been dropped and this receiver has drained
    /// everything sent before that happened.
    Closed,
}

struct SubscriberQueue<T> {
    queue: Lock<VecDeque<T>>,
    signal: Signal<CriticalSectionRawMutex, ()>,
}

struct BroadcastInner<T> {
    subscribers: Lock<Vec<Arc<SubscriberQueue<T>>>>,
    sender_count: AtomicUsize,
    closed: AtomicBool,
}

/// Replaces `tokio::sync::broadcast::Sender`: every subscribed [`BroadcastReceiver`] gets its
/// own queue and receives every value sent, in order (see the module docs for why there's no
/// "Lagged" case).
pub struct BroadcastSender<T> {
    inner: Arc<BroadcastInner<T>>,
}

/// Replaces `tokio::sync::broadcast::Receiver`.
pub struct BroadcastReceiver<T> {
    inner: Arc<BroadcastInner<T>>,
    queue: Arc<SubscriberQueue<T>>,
}

pub(crate) fn broadcast_channel<T>() -> BroadcastSender<T> {
    BroadcastSender {
        inner: Arc::new(BroadcastInner {
            subscribers: Lock::new(RefCell::new(Vec::new())),
            sender_count: AtomicUsize::new(1),
            closed: AtomicBool::new(false),
        }),
    }
}

impl<T: Clone> BroadcastSender<T> {
    pub(crate) fn subscribe(&self) -> BroadcastReceiver<T> {
        let queue = Arc::new(SubscriberQueue {
            queue: Lock::new(RefCell::new(VecDeque::new())),
            signal: Signal::new(),
        });
        self.inner
            .subscribers
            .lock(|subscribers| subscribers.borrow_mut().push(queue.clone()));
        BroadcastReceiver {
            inner: self.inner.clone(),
            queue,
        }
    }

    /// Fans `value` out to every currently subscribed receiver. Unlike `tokio::sync::broadcast`,
    /// this never fails - there's no bounded capacity to overflow, and "no receivers" is simply
    /// a no-op fan-out to zero subscribers.
    pub(crate) fn send(&self, value: T) {
        self.inner.subscribers.lock(|subscribers| {
            for subscriber in subscribers.borrow().iter() {
                subscriber
                    .queue
                    .lock(|queue| queue.borrow_mut().push_back(value.clone()));
                subscriber.signal.signal(());
            }
        });
    }
}

impl<T> Clone for BroadcastSender<T> {
    fn clone(&self) -> Self {
        self.inner.sender_count.fetch_add(1, Ordering::SeqCst);
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Drop for BroadcastSender<T> {
    fn drop(&mut self) {
        if self.inner.sender_count.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.inner.closed.store(true, Ordering::SeqCst);
            self.inner.subscribers.lock(|subscribers| {
                for subscriber in subscribers.borrow().iter() {
                    subscriber.signal.signal(());
                }
            });
        }
    }
}

impl<T> BroadcastReceiver<T> {
    /// Waits for and returns the next broadcast value, or `Err(RecvError::Closed)` once every
    /// [`BroadcastSender`] has been dropped and this receiver's own queue is drained.
    pub async fn recv(&mut self) -> Result<T, RecvError> {
        loop {
            if let Some(value) = self
                .queue
                .queue
                .lock(|queue| queue.borrow_mut().pop_front())
            {
                return Ok(value);
            }
            if self.inner.closed.load(Ordering::SeqCst) {
                return Err(RecvError::Closed);
            }
            self.queue.signal.wait().await;
        }
    }
}
