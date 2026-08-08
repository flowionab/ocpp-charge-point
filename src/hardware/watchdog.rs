//! Proving to hardware that the charge point is still alive (`docs/PRODUCTION-ROADMAP.md` G4.3).
//!
//! # What a watchdog is actually for
//!
//! An MCU watchdog resets the board unless something feeds it. That only protects anything if the
//! thing feeding it is the thing you care about staying alive. A timer task that pets the dog on
//! its own schedule proves the *timer* is running - which it will be, right up until the actor
//! deadlocks and the charge point sits there with a contactor closed, feeding a watchdog that
//! believes everything is fine.
//!
//! So this is fed from exactly one place: [`crate::actor::ChargePointActor`]'s run loop, once per
//! event it finishes applying. What it proves is the precise property worth proving - **the actor
//! is still draining its mailbox and applying events** - and nothing weaker.
//!
//! # Why it is fed after the event, not before
//!
//! An event that wedges mid-apply must *not* pet the dog on its way in. Feeding after the effects
//! have been dispatched means a handler that blocks forever stops the feeding, which is exactly
//! when a reset is the right outcome.
//!
//! # It is optional, and its absence is silent
//!
//! Most integrations have no watchdog, and a charge point without one must not behave differently.
//! [`NoWatchdog`] is the default, and it does nothing at all.

use alloc::boxed::Box;

/// A hardware watchdog the charge point's actor feeds to prove it is still running.
///
/// Implemented by the integrator, because a watchdog is a peripheral: feeding one is a register
/// write on an MCU, a `/dev/watchdog` write under Linux, and a no-op in a test.
///
/// # Contract
///
/// [`Self::pet`] is called from the actor's run loop after each event is fully applied. It must be
/// **cheap and non-blocking** - it sits between every pair of events, so a slow implementation
/// slows the whole charge point - and it must not panic, per this crate's error-handling stance
/// (G4.1/G4.2). There is deliberately no `Result`: there is nothing sensible for the actor to do
/// about a failed watchdog write except carry on, and the hardware's own timeout is the backstop.
#[async_trait::async_trait]
pub trait Watchdog: Send + Sync {
    /// Records that the charge point's actor is alive and making progress.
    async fn pet(&self);
}

#[async_trait::async_trait]
impl<T: Watchdog + ?Sized> Watchdog for alloc::sync::Arc<T> {
    async fn pet(&self) {
        (**self).pet().await;
    }
}

/// The default [`Watchdog`]: no hardware watchdog, so nothing to feed.
///
/// Not a degraded mode - most charge points have no watchdog peripheral, and one without it should
/// behave exactly as it did before this hook existed.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoWatchdog;

#[async_trait::async_trait]
impl Watchdog for NoWatchdog {
    async fn pet(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;
    use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
    use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

    struct CountingWatchdog {
        pets: BlockingMutex<CriticalSectionRawMutex, RefCell<u32>>,
    }

    #[async_trait::async_trait]
    impl Watchdog for CountingWatchdog {
        async fn pet(&self) {
            self.pets.lock(|pets| *pets.borrow_mut() += 1);
        }
    }

    #[tokio::test]
    async fn a_watchdog_can_be_shared_through_an_arc_like_every_other_hardware_binding() {
        let watchdog = alloc::sync::Arc::new(CountingWatchdog {
            pets: BlockingMutex::new(RefCell::new(0)),
        });

        let shared: alloc::sync::Arc<CountingWatchdog> = watchdog.clone();
        shared.pet().await;
        shared.pet().await;

        watchdog.pets.lock(|pets| assert_eq!(*pets.borrow(), 2));
    }

    #[tokio::test]
    async fn the_default_watchdog_does_nothing_and_that_is_not_a_failure() {
        // A charge point with no watchdog peripheral must behave exactly as it did before this
        // hook existed - petting is simply a no-op, not a degraded mode to report.
        NoWatchdog.pet().await;
    }
}
