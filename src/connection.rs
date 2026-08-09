//! Connection lifecycle: what happens once a dropped CSMS connection comes back. Redialing
//! itself - with exponential backoff - is `ocpp-client`'s job (`ConnectOptions::reconnect`,
//! enabled by default; see [`crate::connect_and_setup`]/`ocpp_client::connect_2_1`). What
//! `ocpp-client` explicitly does *not* do on its own, per its own `Client::on_reconnect` docs, is
//! resynchronize application-level state afterwards. This module is that missing piece:
//! re-registering with the CSMS (a fresh BootNotification, retried until accepted) every time
//! the connection is restored after being dropped - the conventional way for a charge point to
//! resynchronize its registration status after a communication interruption. See
//! `docs/ROADMAP.md` §0.
//!
//! True multi-version negotiation (accepting whichever of 1.6J/2.0.1/2.1 the CSMS offers, via
//! `ocpp_client::connect`) isn't wired up here yet - this crate's outbound/inbound wire adapters
//! only target `OCPP2_1Client` so far (see `docs/ROADMAP.md`'s per-functional-block status), so
//! there's nothing to negotiate down to yet. Offline message queueing is also still open: a
//! report sent while the connection is down fails immediately rather than being queued for
//! delivery once reconnected.

use crate::actor::ChargePointActor;
use crate::clock::MonotonicClock;
use crate::provisioning::{Backoff, BootNotifier, register_until_accepted};
use crate::state::BootReasonCause;
use alloc::boxed::Box;
use alloc::string::String;
use core::future::Future;

/// Registers `callback` to run every time the CSMS connection is restored after having dropped -
/// never for the initial connection. Implemented per protocol version (see the `ocpp_2_1`
/// module) as a thin wrapper around `ocpp_client::Client::on_reconnect`, kept behind this crate's
/// own trait so callers (and tests) don't need to depend on `ocpp-client` directly.
#[async_trait::async_trait]
pub trait ReconnectHandler {
    /// Registers `callback` to run on every future reconnect.
    async fn register_reconnect_handler<F, FF>(&self, callback: F)
    where
        F: FnMut() -> FF + Send + Sync + 'static,
        FF: Future<Output = ()> + Send + 'static;
}

/// Forwards to the wrapped handler - see [`crate::availability::StatusNotifier`]'s `Arc` impl.
#[async_trait::async_trait]
impl<H: ReconnectHandler + Send + Sync> ReconnectHandler for alloc::sync::Arc<H> {
    async fn register_reconnect_handler<F, FF>(&self, callback: F)
    where
        F: FnMut() -> FF + Send + Sync + 'static,
        FF: core::future::Future<Output = ()> + Send + 'static,
    {
        (**self).register_reconnect_handler(callback).await;
    }
}

/// Re-registers with the CSMS - a fresh BootNotification, retried until accepted (see
/// [`register_until_accepted`]) - every time the connection to it is restored after being
/// dropped. OCPP doesn't mandate this, but it's the conventional way for a charge point to
/// resynchronize its registration status after a communication interruption, and `ocpp-client`
/// explicitly leaves it to the caller - see [`ReconnectHandler`]'s docs.
///
/// `reason` is sent with every reconnect's BootNotification, unchanged, for the lifetime of this
/// task - it is *not* re-read from durable storage on each reconnect. A WS reconnect is a
/// connectivity event, not a new boot: the reason this process's current boot is happening
/// doesn't change just because the connection dropped and came back, so every resend during the
/// same boot reports the same [`BootReasonCause`] the very first BootNotification did (`None` if
/// that one reported an uncommanded restart). See
/// [`crate::builder::ChargePointBuilder::boot_reason_persistence`] for where `reason` is computed
/// once, at process start.
pub async fn reregister_on_reconnect<N, B, M>(
    actor: ChargePointActor,
    notifier: N,
    backoff: B,
    monotonic: M,
    vendor_name: String,
    model_name: String,
    reason: Option<BootReasonCause>,
) where
    N: ReconnectHandler + BootNotifier + Clone + Send + Sync + 'static,
    B: Backoff + Clone + Send + Sync + 'static,
    M: MonotonicClock + Clone + Send + Sync + 'static,
{
    let boot_notifier = notifier.clone();
    notifier
        .register_reconnect_handler(move || {
            let actor = actor.clone();
            let boot_notifier = boot_notifier.clone();
            let backoff = backoff.clone();
            let monotonic = monotonic.clone();
            let vendor_name = vendor_name.clone();
            let model_name = model_name.clone();
            async move {
                // A reconnect is a station-wide connectivity event, and the count of them over a
                // shift is the first thing anyone looks at when a site is flapping - so INFO,
                // even though `register_until_accepted` logs the outcome itself.
                tracing::info!("the CSMS connection came back; re-registering");
                register_until_accepted(
                    &actor,
                    &boot_notifier,
                    &backoff,
                    &monotonic,
                    &vendor_name,
                    &model_name,
                    reason,
                )
                .await;
            }
        })
        .await;
}

#[cfg(feature = "ocpp_2_1")]
mod ocpp_2_1 {
    use super::ReconnectHandler;
    use alloc::boxed::Box;
    use core::future::Future;
    use ocpp_client::ocpp_2_1::OCPP2_1Client;

    #[async_trait::async_trait]
    impl ReconnectHandler for OCPP2_1Client {
        async fn register_reconnect_handler<F, FF>(&self, mut callback: F)
        where
            F: FnMut() -> FF + Send + Sync + 'static,
            FF: Future<Output = ()> + Send + 'static,
        {
            // `Client::on_reconnect` (the inherent method this type alias exposes) hands the
            // callback a fresh clone of the client itself, which our own callback shape doesn't
            // need - `reregister_on_reconnect` already carries its own client clone.
            ocpp_client::Client::on_reconnect(self, move |_client| callback()).await;
        }
    }
}

/// `Client::on_reconnect` (see the `ocpp_2_1` module's docs) is entirely protocol-agnostic -
/// `Client<E>` is generic over the protocol error type only, not over any version-specific wire
/// shape - so this impl is identical to 2.1's but for `OCPP2_0_1Client`.
#[cfg(feature = "ocpp_2_0_1")]
mod ocpp_2_0_1 {
    use super::ReconnectHandler;
    use alloc::boxed::Box;
    use core::future::Future;
    use ocpp_client::ocpp_2_0_1::OCPP2_0_1Client;

    #[async_trait::async_trait]
    impl ReconnectHandler for OCPP2_0_1Client {
        async fn register_reconnect_handler<F, FF>(&self, mut callback: F)
        where
            F: FnMut() -> FF + Send + Sync + 'static,
            FF: Future<Output = ()> + Send + 'static,
        {
            ocpp_client::Client::on_reconnect(self, move |_client| callback()).await;
        }
    }
}

/// Same as above - mirrors `ocpp_2_1`'s impl, but for `OCPP1_6Client`.
#[cfg(feature = "ocpp_1_6")]
mod ocpp_1_6 {
    use super::ReconnectHandler;
    use alloc::boxed::Box;
    use core::future::Future;
    use ocpp_client::ocpp_1_6::OCPP1_6Client;

    #[async_trait::async_trait]
    impl ReconnectHandler for OCPP1_6Client {
        async fn register_reconnect_handler<F, FF>(&self, mut callback: F)
        where
            F: FnMut() -> FF + Send + Sync + 'static,
            FF: Future<Output = ()> + Send + 'static,
        {
            ocpp_client::Client::on_reconnect(self, move |_client| callback()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ReconnectHandler, reregister_on_reconnect};
    use crate::actor::ChargePointActor;
    use crate::executor::TokioExecutor;
    use crate::provisioning::{Backoff, BootNotificationOutcome, BootNotifier};
    use crate::state::{BootReasonCause, RegistrationStatus};
    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::sync::Arc;
    use core::future::Future;
    use core::pin::Pin;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;

    type BoxedReconnectCallback =
        Box<dyn FnMut() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

    /// A fake CSMS connection whose "reconnect" can be triggered manually from a test via
    /// `fire_reconnect`, standing in for `ocpp-client`'s real `Client::on_reconnect` (which only
    /// fires on an actual dropped-and-restored WebSocket).
    #[derive(Clone, Default)]
    struct FakeReconnectingClient {
        callback: Arc<Mutex<Option<BoxedReconnectCallback>>>,
        boot_calls: Arc<AtomicUsize>,
    }

    impl FakeReconnectingClient {
        async fn fire_reconnect(&self) {
            let future = {
                let mut lock = self.callback.lock().await;
                let callback = lock.as_mut().expect("no reconnect handler registered");
                callback()
            };
            future.await;
        }
    }

    #[async_trait::async_trait]
    impl ReconnectHandler for FakeReconnectingClient {
        async fn register_reconnect_handler<F, FF>(&self, mut callback: F)
        where
            F: FnMut() -> FF + Send + Sync + 'static,
            FF: Future<Output = ()> + Send + 'static,
        {
            let mut lock = self.callback.lock().await;
            *lock = Some(Box::new(move || {
                Box::pin(callback()) as Pin<Box<dyn Future<Output = ()> + Send>>
            }));
        }
    }

    #[async_trait::async_trait]
    impl BootNotifier for FakeReconnectingClient {
        type Error = core::convert::Infallible;

        async fn notify_boot(
            &self,
            _vendor_name: &str,
            _model_name: &str,
            _reason: Option<BootReasonCause>,
        ) -> Result<BootNotificationOutcome, Self::Error> {
            self.boot_calls.fetch_add(1, Ordering::SeqCst);
            Ok(BootNotificationOutcome {
                status: RegistrationStatus::Accepted,
                interval_secs: 60,
                current_time: None,
            })
        }
    }

    #[derive(Clone)]
    struct NoopBackoff;

    #[async_trait::async_trait]
    impl Backoff for NoopBackoff {
        async fn wait(&self, _seconds: u32) {}
    }

    #[cfg(feature = "ocpp_2_1")]
    #[test]
    fn ocpp2_1_client_implements_reconnect_handler() {
        fn assert_impl<T: ReconnectHandler>() {}
        assert_impl::<ocpp_client::ocpp_2_1::OCPP2_1Client>();
    }

    #[tokio::test]
    async fn a_reconnect_triggers_a_fresh_registration() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let client = FakeReconnectingClient::default();

        reregister_on_reconnect(
            actor.clone(),
            client.clone(),
            NoopBackoff,
            crate::clock::SystemMonotonicClock,
            String::from("Acme"),
            String::from("Charger 9000"),
            None,
        )
        .await;

        client.fire_reconnect().await;

        assert_eq!(client.boot_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            actor.state().registration,
            Some(RegistrationStatus::Accepted)
        );
    }

    #[tokio::test]
    async fn registering_the_handler_does_not_itself_send_a_boot_notification() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let client = FakeReconnectingClient::default();

        reregister_on_reconnect(
            actor.clone(),
            client.clone(),
            NoopBackoff,
            crate::clock::SystemMonotonicClock,
            String::from("Acme"),
            String::from("Charger 9000"),
            None,
        )
        .await;

        assert_eq!(client.boot_calls.load(Ordering::SeqCst), 0);
        assert_eq!(actor.state().registration, None);
    }

    #[tokio::test]
    async fn every_reconnect_re_registers() {
        let actor = ChargePointActor::spawn([1], &TokioExecutor);
        let client = FakeReconnectingClient::default();

        reregister_on_reconnect(
            actor.clone(),
            client.clone(),
            NoopBackoff,
            crate::clock::SystemMonotonicClock,
            String::from("Acme"),
            String::from("Charger 9000"),
            None,
        )
        .await;

        client.fire_reconnect().await;
        client.fire_reconnect().await;
        client.fire_reconnect().await;

        assert_eq!(client.boot_calls.load(Ordering::SeqCst), 3);
    }
}
