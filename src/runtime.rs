use crate::actor::{ActorError, ChargePointActor};
use crate::executor::Executor;
use crate::hardware::{HardwareCommandReceiver, HardwareEventSender};
use crate::provisioning::{Backoff, BootNotificationOutcome, BootNotifier};
use crate::state::{
    AuthorizationRequested, ChargePointEvent, ChargePointState, ConnectorStatusChanged,
    RegistrationStatus, SecurityEvent, TransactionEventOccurred,
};
use crate::sync::{BroadcastReceiver, WatchReceiver};

/// Owns the charge point's [`ChargePointActor`] together with the integrator's hardware binding
/// `T`, and provides the entry points [`crate::setup`] wires together: registering with the
/// CSMS, feeding hardware events in, and subscribing every functional block to the state changes
/// and effects it cares about.
pub struct ChargePointRuntime<T = ()> {
    hardware: T,
    actor: ChargePointActor,
}

impl<T> ChargePointRuntime<T> {
    /// Creates a runtime wrapping `hardware`, spawning a fresh [`ChargePointActor`] with one
    /// EVSE per entry in `connector_counts` (each value is that EVSE's connector count).
    pub fn new(
        hardware: T,
        connector_counts: impl IntoIterator<Item = usize>,
        executor: &dyn Executor,
    ) -> Self {
        Self {
            hardware,
            actor: ChargePointActor::spawn(connector_counts, executor),
        }
    }

    /// Feeds `event` into the charge point's state machine. See
    /// [`ChargePointActor::send`].
    pub async fn send(&self, event: ChargePointEvent) -> Result<(), ActorError> {
        self.actor.send(event).await
    }

    /// Runs the Provisioning functional block's BootNotification exchange. See
    /// [`crate::provisioning::register`].
    pub async fn register<N: BootNotifier>(
        &self,
        notifier: &N,
        vendor_name: &str,
        model_name: &str,
    ) -> Result<RegistrationStatus, N::Error> {
        crate::provisioning::register(&self.actor, notifier, vendor_name, model_name).await
    }

    /// Runs BootNotification repeatedly until the CSMS accepts registration. See
    /// [`crate::provisioning::register_until_accepted`].
    pub async fn register_until_accepted<N: BootNotifier, B: Backoff>(
        &self,
        notifier: &N,
        backoff: &B,
        vendor_name: &str,
        model_name: &str,
    ) -> BootNotificationOutcome {
        crate::provisioning::register_until_accepted(
            &self.actor,
            notifier,
            backoff,
            vendor_name,
            model_name,
        )
        .await
    }

    /// A handle a hardware binding uses to push events into the state machine. See
    /// [`crate::hardware::ChargePoint::start`].
    pub fn hardware_events(&self) -> HardwareEventSender {
        HardwareEventSender::new(self.actor.clone())
    }

    /// The hardware commands (lock/unlock, open/close contactor) a hardware binding must drain
    /// and carry out. See [`crate::hardware::ChargePoint::start`].
    pub fn hardware_commands(&self) -> HardwareCommandReceiver {
        HardwareCommandReceiver::new(self.actor.subscribe_commands())
    }

    /// Subscribes to connector status changes for the Availability functional block. Subscribe
    /// before starting the hardware so early transitions (e.g. a connector already occupied at
    /// startup) aren't missed - the channel buffers them until a consumer reads them.
    pub fn subscribe_status_notifications(&self) -> BroadcastReceiver<ConnectorStatusChanged> {
        self.actor.subscribe_status_notifications()
    }

    /// Subscribes to transaction lifecycle events for the Transactions functional block.
    /// Subscribe before starting the hardware, for the same reason as
    /// [`Self::subscribe_status_notifications`].
    pub fn subscribe_transaction_events(&self) -> BroadcastReceiver<TransactionEventOccurred> {
        self.actor.subscribe_transaction_events()
    }

    /// Subscribes to authorization requests for the Authorization functional block. Subscribe
    /// before starting the hardware, for the same reason as
    /// [`Self::subscribe_status_notifications`].
    pub fn subscribe_authorization_requests(&self) -> BroadcastReceiver<AuthorizationRequested> {
        self.actor.subscribe_authorization_requests()
    }

    /// Subscribes to security events for the Security functional block. Subscribe before
    /// starting the hardware, for the same reason as [`Self::subscribe_status_notifications`].
    pub fn subscribe_security_events(&self) -> BroadcastReceiver<SecurityEvent> {
        self.actor.subscribe_security_events()
    }

    /// A snapshot of the charge point's current state. See [`ChargePointActor::state`].
    pub fn state(&self) -> ChargePointState {
        self.actor.state()
    }

    /// Subscribes to every future state change. See [`ChargePointActor::subscribe`].
    pub fn subscribe(&self) -> WatchReceiver<ChargePointState> {
        self.actor.subscribe()
    }

    pub(crate) fn hardware(&self) -> &T {
        &self.hardware
    }

    /// A cheap-to-clone handle to the underlying actor, independent of `T` - lets background
    /// tasks (like [`crate::authorization::run_authorization_requests`]) feed events back in
    /// without needing to be generic over the hardware type.
    pub(crate) fn actor(&self) -> ChargePointActor {
        self.actor.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::ChargePointRuntime;
    use crate::executor::TokioExecutor;
    use crate::provisioning::{BootNotificationOutcome, BootNotifier};
    use crate::state::{
        ChargePointEvent, ConnectorEvent, ConnectorState, EvseEvent, LifecycleState,
        RegistrationStatus,
    };
    use alloc::boxed::Box;

    struct FakeBootNotifier {
        outcome: BootNotificationOutcome,
    }

    #[async_trait::async_trait]
    impl BootNotifier for FakeBootNotifier {
        type Error = core::convert::Infallible;

        async fn notify_boot(
            &self,
            vendor_name: &str,
            model_name: &str,
        ) -> Result<BootNotificationOutcome, Self::Error> {
            assert_eq!(vendor_name, "Acme");
            assert_eq!(model_name, "Charger 9000");
            Ok(self.outcome)
        }
    }

    #[tokio::test]
    async fn accepted_registration_makes_the_charge_point_available() {
        let runtime = ChargePointRuntime::new((), [1], &TokioExecutor);
        let notifier = FakeBootNotifier {
            outcome: BootNotificationOutcome {
                status: RegistrationStatus::Accepted,
                interval_secs: 60,
            },
        };
        let mut states = runtime.subscribe();

        let status = runtime
            .register(&notifier, "Acme", "Charger 9000")
            .await
            .unwrap();
        states.changed().await;

        assert_eq!(status, RegistrationStatus::Accepted);
        assert_eq!(
            runtime.state().registration,
            Some(RegistrationStatus::Accepted)
        );
        assert_eq!(runtime.state().lifecycle, LifecycleState::Available);
    }

    #[tokio::test]
    async fn pending_registration_records_status_without_becoming_available() {
        let runtime = ChargePointRuntime::new((), [1], &TokioExecutor);
        let notifier = FakeBootNotifier {
            outcome: BootNotificationOutcome {
                status: RegistrationStatus::Pending,
                interval_secs: 10,
            },
        };
        let mut states = runtime.subscribe();

        let status = runtime
            .register(&notifier, "Acme", "Charger 9000")
            .await
            .unwrap();
        states.changed().await;

        assert_eq!(status, RegistrationStatus::Pending);
        assert_eq!(
            runtime.state().registration,
            Some(RegistrationStatus::Pending)
        );
        assert_eq!(runtime.state().lifecycle, LifecycleState::Booting);
    }

    struct ScriptedBootNotifier {
        responses: alloc::vec::Vec<BootNotificationOutcome>,
        calls: core::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl BootNotifier for ScriptedBootNotifier {
        type Error = core::convert::Infallible;

        async fn notify_boot(
            &self,
            _vendor_name: &str,
            _model_name: &str,
        ) -> Result<BootNotificationOutcome, Self::Error> {
            let index = self
                .calls
                .fetch_add(1, core::sync::atomic::Ordering::SeqCst);
            Ok(self.responses[index])
        }
    }

    struct FlakyThenAcceptedNotifier {
        remaining_failures: core::sync::atomic::AtomicUsize,
    }

    #[derive(Debug)]
    struct FakeTransportError;

    impl core::fmt::Display for FakeTransportError {
        fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            formatter.write_str("simulated transport failure")
        }
    }

    impl core::error::Error for FakeTransportError {}

    #[async_trait::async_trait]
    impl BootNotifier for FlakyThenAcceptedNotifier {
        type Error = FakeTransportError;

        async fn notify_boot(
            &self,
            _vendor_name: &str,
            _model_name: &str,
        ) -> Result<BootNotificationOutcome, Self::Error> {
            if self
                .remaining_failures
                .fetch_update(
                    core::sync::atomic::Ordering::SeqCst,
                    core::sync::atomic::Ordering::SeqCst,
                    |n| if n > 0 { Some(n - 1) } else { None },
                )
                .is_ok()
            {
                Err(FakeTransportError)
            } else {
                Ok(BootNotificationOutcome {
                    status: RegistrationStatus::Accepted,
                    interval_secs: 60,
                })
            }
        }
    }

    struct NoopBackoff {
        waits: core::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::provisioning::Backoff for NoopBackoff {
        async fn wait(&self, _seconds: u32) {
            self.waits
                .fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn register_until_accepted_retries_through_pending_and_rejected() {
        let runtime = ChargePointRuntime::new((), [1], &TokioExecutor);
        let notifier = ScriptedBootNotifier {
            responses: alloc::vec![
                BootNotificationOutcome {
                    status: RegistrationStatus::Pending,
                    interval_secs: 5,
                },
                BootNotificationOutcome {
                    status: RegistrationStatus::Rejected,
                    interval_secs: 5,
                },
                BootNotificationOutcome {
                    status: RegistrationStatus::Accepted,
                    interval_secs: 60,
                },
            ],
            calls: core::sync::atomic::AtomicUsize::new(0),
        };
        let backoff = NoopBackoff {
            waits: core::sync::atomic::AtomicUsize::new(0),
        };

        let outcome = runtime
            .register_until_accepted(&notifier, &backoff, "Acme", "Charger 9000")
            .await;

        assert_eq!(outcome.status, RegistrationStatus::Accepted);
        assert_eq!(outcome.interval_secs, 60);
        assert_eq!(
            runtime.state().registration,
            Some(RegistrationStatus::Accepted)
        );
        assert_eq!(runtime.state().lifecycle, LifecycleState::Available);
        assert_eq!(notifier.calls.load(core::sync::atomic::Ordering::SeqCst), 3);
        assert_eq!(backoff.waits.load(core::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn register_until_accepted_retries_past_transport_failures() {
        let runtime = ChargePointRuntime::new((), [1], &TokioExecutor);
        let notifier = FlakyThenAcceptedNotifier {
            remaining_failures: core::sync::atomic::AtomicUsize::new(2),
        };
        let backoff = NoopBackoff {
            waits: core::sync::atomic::AtomicUsize::new(0),
        };

        let outcome = runtime
            .register_until_accepted(&notifier, &backoff, "Acme", "Charger 9000")
            .await;

        assert_eq!(outcome.status, RegistrationStatus::Accepted);
        assert_eq!(runtime.state().lifecycle, LifecycleState::Available);
        assert_eq!(backoff.waits.load(core::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn runtime_routes_hardware_events_to_the_supervisor_actor() {
        let runtime = ChargePointRuntime::new((), [1], &TokioExecutor);
        let mut states = runtime.subscribe();

        runtime.send(ChargePointEvent::BootCompleted).await.unwrap();
        states.changed().await;

        runtime
            .send(ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::CableConnected,
                },
            })
            .await
            .unwrap();
        states.changed().await;

        assert_eq!(runtime.state().lifecycle, LifecycleState::Available);
        assert_eq!(
            runtime.state().evses[0].connectors[0],
            ConnectorState::Connected
        );
    }
}
