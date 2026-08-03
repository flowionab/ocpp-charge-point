//! An abstraction over "how to run a background task", so `setup()` doesn't hard-depend on
//! tokio for spawning. Mirrors `ocpp-client`'s own `Executor` trait. See `docs/ROADMAP.md` §0.

use alloc::boxed::Box;
use core::future::Future;
use core::pin::Pin;

/// Runs a future to completion in the background, detached from the caller.
pub trait Executor {
    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>);
}

/// An [`Executor`] that spawns onto the ambient tokio runtime via `tokio::spawn`. Not gated
/// behind the `std` feature: tokio is still a mandatory dependency of this crate regardless
/// (see `docs/ROADMAP.md` §0) - that will change once the channel abstraction lands, at which
/// point this impl will move behind a real `tokio-runtime` feature.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioExecutor;

impl Executor for TokioExecutor {
    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>) {
        tokio::spawn(future);
    }
}

#[cfg(test)]
mod tests {
    use super::{Executor, TokioExecutor};
    use alloc::boxed::Box;

    #[tokio::test]
    async fn tokio_executor_runs_the_spawned_future() {
        let (sender, receiver) = tokio::sync::oneshot::channel();

        TokioExecutor.spawn(Box::pin(async move {
            let _ = sender.send(());
        }));

        // Only resolves once the spawned future above actually ran and sent.
        receiver.await.unwrap();
    }
}
