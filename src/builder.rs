//! A per-functional-block alternative to [`crate::setup`].
//!
//! [`crate::setup`] takes a single CSMS client type bounded by *every* functional block's trait
//! at once, which makes it impossible to drive a client that only implements a subset of blocks
//! (e.g. `OCPP2_0_1Client`, which has no `SecurityEventNotifier`), and increasingly unworkable as
//! more blocks are added. [`ChargePointBuilder`] instead exposes one registration method per
//! functional block, each bounded only by that block's own traits, so a caller can register
//! exactly the blocks their CSMS client (and their hardware) supports and skip the rest.
//!
//! Each registration call - [`ChargePointBuilder::provisioning`],
//! [`ChargePointBuilder::status_notifications`], and so on - is independently skippable: calling
//! a subset of them, in any combination, is supported and never panics. That's what will let a
//! future Cargo-feature (see `docs/PRODUCTION-ROADMAP.md` §5.1) or runtime-capability (§5.2) gate
//! compile out or skip a block without touching this module - the gate just omits the call.
//!
//! [`crate::setup`] itself is now a thin wrapper that calls every registration method in order;
//! see its source for the canonical "everything on" recipe this builder supports.

use crate::ChargePointRuntime;
use crate::authorization::{Authorizer, run_authorization_requests};
use crate::availability::{ChangeAvailabilityHandler, DedupedStatusNotifier, StatusNotifier};
use crate::connection::{ReconnectHandler, reregister_on_reconnect};
#[cfg(feature = "tariff-cost")]
use crate::cost::CostUpdatedHandler;
use crate::device_model::{GetVariablesHandler, SetVariablesHandler};
use crate::executor::Executor;
use crate::hardware::ChargePoint;
use crate::hardware::Connector;
use crate::hardware::Evse;
use crate::hardware::{Capabilities, warn_on_feature_mismatches};
#[cfg(feature = "local-auth-list")]
use crate::local_authorization_list::{GetLocalListVersionHandler, SendLocalListHandler};
use crate::offline_queue::{OfflineQueue, run_with_offline_queue};
use crate::provisioning::{Backoff, BootNotifier, HeartbeatSender, run_heartbeat};
use crate::remote_control::{
    RequestStartTransactionHandler, RequestStopTransactionHandler, UnlockConnectorHandler,
};
use crate::reporting::{GetBaseReportHandler, GetReportHandler};
#[cfg(feature = "reservation")]
use crate::reservation::{CancelReservationHandler, ReserveNowHandler};
use crate::reset::ResetHandler;
use crate::security::SecurityEventNotifier;
use crate::state::{
    AuthorizationRequested, ChargePointEvent, Component, ConnectorStatusChanged, DeviceModelEvent,
    SecurityEvent, TransactionEventOccurred, Variable, VariableAttributeType,
};
use crate::sync::BroadcastReceiver;
use crate::transactions::TransactionNotifier;
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

/// Builds a [`ChargePointRuntime`] by starting the hardware and then registering functional
/// blocks against a CSMS client one at a time, rather than requiring a single client type that
/// implements every block at once. See the module docs for why this exists.
///
/// Each registration method (`provisioning`, `status_notifications`, ...) consumes and returns
/// `Self`, so calls chain: `Builder::start(hw, ex).await?.provisioning(&csms,
/// backoff).await.status_notifications(&csms).await....build()`. Blocks may be registered in any
/// combination and any order relative to each other (registering the same block twice, or
/// skipping it, never panics) with one exception worth calling out: the four event subscriptions
/// (status/transaction/authorization/security) are taken once, up front, in [`Self::start`] -
/// the same ordering `setup()` used - so that events fired during hardware start-up aren't lost.
/// Because each of those subscriptions exists exactly once, registering one of those four blocks
/// a second time is a **no-op**: the call is ignored (with a warning logged) rather than spawning
/// a second forwarder, which would duplicate every StatusNotification/TransactionEvent/
/// SecurityEventNotification on the wire for the rest of the process's life. The remaining blocks
/// register inbound handlers against the CSMS client instead of consuming a subscription, so
/// re-registering one of those just replaces the previous registration, exactly as calling the
/// client's own `register_*` method twice would.
pub struct ChargePointBuilder<T, X> {
    runtime: ChargePointRuntime<T>,
    executor: X,
    // Captured in `start()`, while `T`'s `ChargePoint<E, C>` bound (and therefore
    // `vendor_name()`/`model_name()`) is still in scope - later registration methods aren't
    // generic over `E`/`C`, so they can't call those hardware methods themselves.
    vendor_name: String,
    model_name: String,
    // Captured in `start()`, same reasoning as `vendor_name`/`model_name` above - later
    // registration methods aren't generic over `E`/`C` and can't call `charge_point.capabilities()`
    // themselves. See `Self::capabilities` and `docs/PRODUCTION-ROADMAP.md` §5.3 (C3) for how this
    // is consulted to keep every advertisement surface in sync with what the hardware declares.
    capabilities: Capabilities,
    status_changes: Option<BroadcastReceiver<ConnectorStatusChanged>>,
    transaction_events: Option<BroadcastReceiver<TransactionEventOccurred>>,
    authorization_requests: Option<BroadcastReceiver<AuthorizationRequested>>,
    security_events: Option<BroadcastReceiver<SecurityEvent>>,
}

impl<T, X: Executor> ChargePointBuilder<T, X> {
    /// Starts the hardware and takes the four functional-block subscriptions (status,
    /// transaction, authorization, security) before doing so, so that events fired during
    /// start-up (e.g. a connector that's already occupied at boot) are buffered rather than
    /// lost. This is the same ordering [`crate::setup`] uses.
    pub async fn start<E, C>(charge_point: T, executor: X) -> Result<Self, T::StartError>
    where
        T: ChargePoint<E, C>,
        E: Evse<C>,
        C: Connector,
    {
        let vendor_name = charge_point.vendor_name().await.to_string();
        let model_name = charge_point.model_name().await.to_string();
        let capabilities = charge_point.capabilities().await;
        // C2.4 (docs/PRODUCTION-ROADMAP.md §5.2): catch a hardware binding that claims a
        // capability whose Cargo feature is compiled out, or that leaves a compiled-in feature's
        // capability unclaimed, as early as possible - logged, never fatal (see
        // `warn_on_feature_mismatches`'s docs for why this doesn't panic/fail startup).
        warn_on_feature_mismatches(&capabilities);
        tracing::info!(
            vendor = vendor_name.as_str(),
            model = model_name.as_str(),
            "Initializing charger"
        );

        let mut connector_counts = Vec::new();
        for evse in charge_point.evses().await {
            connector_counts.push(evse.connectors().await.len());
        }

        let runtime = ChargePointRuntime::new(charge_point, connector_counts, &executor);
        // Subscribe before starting the hardware so status/transaction/authorization events fired
        // during `start()` (e.g. a connector that's already occupied at boot) are buffered rather
        // than lost.
        let status_changes = runtime.subscribe_status_notifications();
        let transaction_events = runtime.subscribe_transaction_events();
        let authorization_requests = runtime.subscribe_authorization_requests();
        let security_events = runtime.subscribe_security_events();

        // C3 (docs/PRODUCTION-ROADMAP.md §5.3): land the hardware-declared capabilities into state
        // itself first - the single source of truth `crate::hardware::supported_feature_profiles_1_6`
        // and every other capability-propagation surface ultimately reads - then register every
        // `*Ctrlr.Available` device model variable [`CAPABILITY_GATES`] knows about, all before the
        // hardware binding gets a chance to register its own components.
        let _ = runtime
            .hardware_events()
            .send(crate::state::ChargePointEvent::CapabilitiesDeclared(
                capabilities,
            ))
            .await;
        for event in crate::device_model::capability_gate_events(&capabilities) {
            let _ = runtime.hardware_events().send(event).await;
        }

        runtime
            .hardware()
            .start(runtime.hardware_events(), runtime.hardware_commands())
            .await?;

        Ok(Self {
            runtime,
            executor,
            vendor_name,
            model_name,
            capabilities,
            status_changes: Some(status_changes),
            transaction_events: Some(transaction_events),
            authorization_requests: Some(authorization_requests),
            security_events: Some(security_events),
        })
    }

    /// The capabilities the hardware declared via
    /// [`ChargePoint::capabilities`](crate::hardware::ChargePoint::capabilities), captured once in
    /// [`Self::start`]. The single source of truth callers (e.g. [`crate::setup::setup`]) consult
    /// to decide which registration methods below to actually call - see
    /// `docs/PRODUCTION-ROADMAP.md` §5.3 (C3).
    pub fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    /// Registers the Provisioning functional block: retries BootNotification (via
    /// [`ChargePointRuntime::register_until_accepted`]) until the CSMS accepts registration,
    /// lands the accepted Heartbeat interval into the `OCPPCommCtrlr`/`HeartbeatInterval` device
    /// model variable, re-registers on every future reconnect, and spawns a background task (via
    /// the executor supplied to [`Self::start`]) that sends a Heartbeat at that interval for as
    /// long as the process runs.
    ///
    /// `backoff` is caller-supplied (rather than defaulting to tokio) so this doesn't hard-depend
    /// on an async runtime - std/tokio users can pass [`crate::provisioning::TokioBackoff`];
    /// embedded targets supply their own.
    pub async fn provisioning<N, B>(self, csms: &N, backoff: B) -> Self
    where
        N: BootNotifier + HeartbeatSender + ReconnectHandler + Clone + Send + Sync + 'static,
        B: Backoff + Clone + Send + Sync + 'static,
    {
        let vendor_name = self.vendor_name.as_str();
        let model_name = self.model_name.as_str();
        let outcome = self
            .runtime
            .register_until_accepted(csms, &backoff, vendor_name, model_name)
            .await;

        // The accepted BootNotification interval *is* the `OCPPCommCtrlr`/`HeartbeatInterval`
        // device model variable (see `crate::state::DeviceModel::register_defaults`) - land it
        // there so a CSMS reading it back via `GetVariables`/`GetConfiguration` sees the value
        // actually in effect, and so `run_heartbeat` (which reads the device model on every
        // cycle) starts from it rather than a value only known to this function.
        let _ = self
            .runtime
            .actor()
            .send(ChargePointEvent::DeviceModel(
                DeviceModelEvent::AttributeValueSet {
                    component: Component {
                        name: "OCPPCommCtrlr".into(),
                        instance: None,
                        evse: None,
                    },
                    variable: Variable {
                        name: "HeartbeatInterval".into(),
                        instance: None,
                    },
                    attribute_type: VariableAttributeType::Actual,
                    value: outcome.interval_secs.to_string(),
                },
            ))
            .await;

        reregister_on_reconnect(
            self.runtime.actor(),
            csms.clone(),
            backoff.clone(),
            vendor_name.into(),
            model_name.into(),
        )
        .await;

        let heartbeat_sender = csms.clone();
        let heartbeat_backoff = backoff.clone();
        let heartbeat_actor = self.runtime.actor();
        self.executor.spawn(Box::pin(async move {
            run_heartbeat(
                &heartbeat_sender,
                &heartbeat_backoff,
                &heartbeat_actor,
                outcome.interval_secs,
            )
            .await;
        }));

        self
    }

    /// Registers the Availability functional block's status-forwarding half: every connector
    /// status change is forwarded to `csms` via StatusNotification, deduped so the CSMS only
    /// sees wire-visible status changes (not every internal `ConnectorState` transition), queued
    /// and retried in order if the connection is currently down, and flushed on reconnect.
    pub async fn status_notifications<N>(mut self, csms: &N) -> Self
    where
        N: StatusNotifier + ReconnectHandler + Clone + Send + Sync + 'static,
    {
        let Some(status_changes) = self.take_status_changes() else {
            return self;
        };

        // Wrapped in `DedupedStatusNotifier` so `csms` only sees a wire-visible status change,
        // not every internal `ConnectorState` transition `ChargePointState` now reports (see
        // `docs/ROADMAP.md` §0) - restoring the cadence `setup()`'s csms types (2.1, 2.0.1) had
        // before that change, since neither has a status richer than `ConnectorStatus` to justify
        // seeing the extra calls. Wrapped again in `Arc` so the same dedup cache (and the same
        // `OfflineQueue`, below) is shared between the live forwarder task and the reconnect-flush
        // closure - cloning the `Arc` per call is cheap and keeps both paths consistent, unlike
        // constructing a second `DedupedStatusNotifier` with its own empty cache would.
        //
        // Each of Status/Transaction/Security also goes through an `OfflineQueue`: a report that
        // fails to send (e.g. the CSMS connection is currently down) is queued and retried, in
        // order, rather than dropped - both whenever the next report comes in and, via
        // `register_reconnect_handler` below, as soon as the connection itself comes back, so a
        // queued report doesn't wait indefinitely for an unrelated event to trigger a retry.
        let status_queue = Arc::new(OfflineQueue::new());
        let status_notifier = Arc::new(DedupedStatusNotifier::new(csms.clone()));
        let forwarder_queue = status_queue.clone();
        let forwarder_notifier = status_notifier.clone();
        self.executor.spawn(Box::pin(async move {
            run_with_offline_queue(status_changes, &forwarder_queue, move |changed| {
                let notifier = forwarder_notifier.clone();
                async move {
                    notifier
                        .notify_status(
                            changed.evse_id,
                            changed.connector_id,
                            changed.status,
                            changed.connector_state,
                        )
                        .await
                }
            })
            .await;
        }));
        csms.register_reconnect_handler(move || {
            let queue = status_queue.clone();
            let notifier = status_notifier.clone();
            async move {
                crate::offline_queue::flush_offline_queue(&queue, move |changed| {
                    let notifier = notifier.clone();
                    async move {
                        notifier
                            .notify_status(
                                changed.evse_id,
                                changed.connector_id,
                                changed.status,
                                changed.connector_state,
                            )
                            .await
                    }
                })
                .await;
            }
        })
        .await;

        self
    }

    /// Registers the Transactions functional block: every transaction lifecycle event is
    /// forwarded to `csms` via TransactionEvent, queued and retried in order if the connection is
    /// currently down, and flushed on reconnect.
    pub async fn transaction_events<N>(mut self, csms: &N) -> Self
    where
        N: TransactionNotifier + ReconnectHandler + Clone + Send + Sync + 'static,
    {
        let Some(transaction_events) = self.take_transaction_events() else {
            return self;
        };

        let transaction_queue = Arc::new(OfflineQueue::new());
        let forwarder_queue = transaction_queue.clone();
        let forwarder_csms = csms.clone();
        self.executor.spawn(Box::pin(async move {
            run_with_offline_queue(transaction_events, &forwarder_queue, move |occurred| {
                let notifier = forwarder_csms.clone();
                async move {
                    notifier
                        .notify_transaction_event(
                            occurred.evse_id,
                            occurred.connector_id,
                            occurred.kind,
                            occurred.transaction,
                        )
                        .await
                }
            })
            .await;
        }));
        let reconnect_csms = csms.clone();
        csms.register_reconnect_handler(move || {
            let queue = transaction_queue.clone();
            let csms = reconnect_csms.clone();
            async move {
                crate::offline_queue::flush_offline_queue(&queue, move |occurred| {
                    let notifier = csms.clone();
                    async move {
                        notifier
                            .notify_transaction_event(
                                occurred.evse_id,
                                occurred.connector_id,
                                occurred.kind,
                                occurred.transaction,
                            )
                            .await
                    }
                })
                .await;
            }
        })
        .await;

        self
    }

    /// Registers the Authorization functional block: every presented-id-token authorization
    /// request is answered via Authorize.
    pub async fn authorization<N>(mut self, csms: &N) -> Self
    where
        N: Authorizer + Clone + Send + Sync + 'static,
    {
        let Some(authorization_requests) = self.take_authorization_requests() else {
            return self;
        };

        let authorizer = csms.clone();
        let actor = self.runtime.actor();
        self.executor.spawn(Box::pin(async move {
            run_authorization_requests(authorization_requests, &authorizer, actor).await;
        }));

        self
    }

    /// Registers the Security functional block: every reported security event is forwarded to
    /// `csms` via SecurityEventNotification, queued and retried in order if the connection is
    /// currently down, and flushed on reconnect.
    pub async fn security_events<N>(mut self, csms: &N) -> Self
    where
        N: SecurityEventNotifier + ReconnectHandler + Clone + Send + Sync + 'static,
    {
        let Some(security_events) = self.take_security_events() else {
            return self;
        };

        let security_queue = Arc::new(OfflineQueue::new());
        let forwarder_queue = security_queue.clone();
        let forwarder_csms = csms.clone();
        self.executor.spawn(Box::pin(async move {
            run_with_offline_queue(security_events, &forwarder_queue, move |event| {
                let notifier = forwarder_csms.clone();
                async move {
                    notifier
                        .notify_security_event(&event.event_type, event.tech_info.as_deref())
                        .await
                }
            })
            .await;
        }));
        let reconnect_csms = csms.clone();
        csms.register_reconnect_handler(move || {
            let queue = security_queue.clone();
            let csms = reconnect_csms.clone();
            async move {
                crate::offline_queue::flush_offline_queue(&queue, move |event| {
                    let notifier = csms.clone();
                    async move {
                        notifier
                            .notify_security_event(&event.event_type, event.tech_info.as_deref())
                            .await
                    }
                })
                .await;
            }
        })
        .await;

        self
    }

    /// Registers the Remote Control functional block: UnlockConnector, RequestStartTransaction,
    /// and RequestStopTransaction handlers all feed into the runtime's actor.
    pub async fn remote_control<N>(self, csms: &N) -> Self
    where
        N: UnlockConnectorHandler
            + RequestStartTransactionHandler
            + RequestStopTransactionHandler
            + Send
            + Sync
            + 'static,
    {
        csms.register_unlock_connector_handler(self.runtime.actor())
            .await;
        csms.register_request_start_transaction_handler(self.runtime.actor())
            .await;
        csms.register_request_stop_transaction_handler(self.runtime.actor())
            .await;

        self
    }

    /// Registers the Availability functional block's control half: the ChangeAvailability
    /// handler feeds into the runtime's actor.
    pub async fn availability_control<N>(self, csms: &N) -> Self
    where
        N: ChangeAvailabilityHandler + Send + Sync + 'static,
    {
        csms.register_change_availability_handler(self.runtime.actor())
            .await;

        self
    }

    /// Registers the Reservation functional block: ReserveNow and CancelReservation handlers
    /// both feed into the runtime's actor.
    ///
    /// Only present when the `reservation` Cargo feature is enabled - with it off,
    /// `crate::reservation` (and therefore this method) doesn't exist, so a build that omits the
    /// feature never links the reservation handling code at all.
    #[cfg(feature = "reservation")]
    pub async fn reservation<N>(self, csms: &N) -> Self
    where
        N: ReserveNowHandler + CancelReservationHandler + Send + Sync + 'static,
    {
        csms.register_reserve_now_handler(self.runtime.actor())
            .await;
        csms.register_cancel_reservation_handler(self.runtime.actor())
            .await;

        self
    }

    /// Registers the Reset functional block: the Reset handler feeds into the runtime's actor.
    pub async fn reset<N>(self, csms: &N) -> Self
    where
        N: ResetHandler + Send + Sync + 'static,
    {
        csms.register_reset_handler(self.runtime.actor()).await;

        self
    }

    /// Registers the Local Authorization List functional block: SendLocalList and
    /// GetLocalListVersion handlers both feed into the runtime's actor.
    ///
    /// Only present when the `local-auth-list` Cargo feature is enabled - see
    /// [`Self::reservation`]'s doc comment for why the method itself disappears rather than
    /// becoming a no-op.
    #[cfg(feature = "local-auth-list")]
    pub async fn local_authorization_list<N>(self, csms: &N) -> Self
    where
        N: SendLocalListHandler + GetLocalListVersionHandler + Send + Sync + 'static,
    {
        csms.register_send_local_list_handler(self.runtime.actor())
            .await;
        csms.register_get_local_list_version_handler(self.runtime.actor())
            .await;

        self
    }

    /// Registers the Device Model functional block's reporting/configuration surface:
    /// GetVariables, SetVariables, GetBaseReport, and GetReport handlers all feed into the
    /// runtime's actor.
    pub async fn device_model<N>(self, csms: &N) -> Self
    where
        N: GetVariablesHandler
            + SetVariablesHandler
            + GetBaseReportHandler
            + GetReportHandler
            + Send
            + Sync
            + 'static,
    {
        csms.register_get_variables_handler(self.runtime.actor())
            .await;
        csms.register_set_variables_handler(self.runtime.actor())
            .await;
        csms.register_get_base_report_handler(self.runtime.actor())
            .await;
        csms.register_get_report_handler(self.runtime.actor()).await;

        self
    }

    /// Registers the Tariff and Cost functional block: the CostUpdated handler feeds into the
    /// runtime's actor.
    ///
    /// Only present when the `tariff-cost` Cargo feature is enabled - see [`Self::reservation`]'s
    /// doc comment for why the method itself disappears rather than becoming a no-op.
    #[cfg(feature = "tariff-cost")]
    pub async fn cost<N>(self, csms: &N) -> Self
    where
        N: CostUpdatedHandler + Send + Sync + 'static,
    {
        csms.register_cost_updated_handler(self.runtime.actor())
            .await;

        self
    }

    /// Finishes building, handing back the [`ChargePointRuntime`] every registered block is now
    /// wired to.
    pub fn build(self) -> ChargePointRuntime<T> {
        self.runtime
    }

    /// Takes the status-change subscription captured in [`Self::start`], or `None` if an earlier
    /// call already consumed it (see the struct docs - a repeat registration is ignored rather
    /// than duplicating every report on the wire).
    fn take_status_changes(&mut self) -> Option<BroadcastReceiver<ConnectorStatusChanged>> {
        Self::warn_if_taken(self.status_changes.take(), "status_notifications")
    }

    /// Takes the transaction-event subscription captured in [`Self::start`], or `None` if an
    /// earlier call already consumed it (see [`Self::take_status_changes`]).
    fn take_transaction_events(&mut self) -> Option<BroadcastReceiver<TransactionEventOccurred>> {
        Self::warn_if_taken(self.transaction_events.take(), "transaction_events")
    }

    /// Takes the authorization-request subscription captured in [`Self::start`], or `None` if an
    /// earlier call already consumed it (see [`Self::take_status_changes`]).
    fn take_authorization_requests(&mut self) -> Option<BroadcastReceiver<AuthorizationRequested>> {
        Self::warn_if_taken(self.authorization_requests.take(), "authorization")
    }

    /// Takes the security-event subscription captured in [`Self::start`], or `None` if an earlier
    /// call already consumed it (see [`Self::take_status_changes`]).
    fn take_security_events(&mut self) -> Option<BroadcastReceiver<SecurityEvent>> {
        Self::warn_if_taken(self.security_events.take(), "security_events")
    }

    /// Logs the repeat-registration warning shared by the four subscription-consuming blocks,
    /// passing the subscription (or its absence) straight through.
    fn warn_if_taken<M>(
        taken: Option<BroadcastReceiver<M>>,
        block: &str,
    ) -> Option<BroadcastReceiver<M>> {
        if taken.is_none() {
            tracing::warn!(
                block,
                "functional block registered more than once - ignoring the repeat registration, \
                 since its event subscription was already consumed by the first"
            );
        }
        taken
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use crate::hardware::{
        Capabilities, ChargePoint, Connector, Evse, HardwareCommandReceiver, HardwareEventSender,
        execute_hardware_command,
    };
    use crate::state::{ChargePointEvent, ConnectorEvent, EvseEvent};
    use alloc::sync::Arc;
    use core::convert::Infallible;
    use core::sync::atomic::AtomicBool;

    /// Wraps any [`ChargePoint`] fixture to override just [`ChargePoint::capabilities`], so tests
    /// can exercise capability-driven behaviour (`docs/PRODUCTION-ROADMAP.md` §5.3, C3) without a
    /// dedicated fixture per capability combination.
    pub(crate) struct WithCapabilities<T> {
        pub(crate) inner: T,
        pub(crate) capabilities: Capabilities,
    }

    #[async_trait::async_trait]
    impl<T, E, C> ChargePoint<E, C> for WithCapabilities<T>
    where
        T: ChargePoint<E, C> + Sync,
        E: Evse<C>,
        C: Connector,
    {
        type StartError = T::StartError;

        async fn vendor_name(&self) -> &str {
            self.inner.vendor_name().await
        }

        async fn model_name(&self) -> &str {
            self.inner.model_name().await
        }

        async fn evses(&self) -> &[E] {
            self.inner.evses().await
        }

        async fn capabilities(&self) -> Capabilities {
            self.capabilities
        }

        async fn start(
            &self,
            events: HardwareEventSender,
            commands: HardwareCommandReceiver,
        ) -> Result<(), Self::StartError> {
            self.inner.start(events, commands).await
        }
    }

    /// A minimal [`ChargePoint`] fixture: one EVSE, one connector, that reports a cable-connect
    /// event on start and then drives whichever single hardware command the runtime issues.
    /// Shared between `setup()`'s tests and the builder's, so both exercise the same fixture
    /// rather than near-duplicates drifting apart.
    pub(crate) struct TestChargePoint {
        pub(crate) evses: [TestEvse; 1],
    }

    pub(crate) struct TestEvse {
        pub(crate) connectors: [TestConnector; 1],
    }

    pub(crate) struct TestConnector {
        pub(crate) locked: Arc<AtomicBool>,
        pub(crate) lock_succeeds: bool,
    }

    #[derive(Debug)]
    pub(crate) struct TestConnectorError;

    impl core::fmt::Display for TestConnectorError {
        fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            formatter.write_str("test connector operation failed")
        }
    }

    impl core::error::Error for TestConnectorError {}

    #[async_trait::async_trait]
    impl ChargePoint<TestEvse, TestConnector> for TestChargePoint {
        type StartError = Infallible;

        async fn vendor_name(&self) -> &str {
            "Test vendor"
        }

        async fn model_name(&self) -> &str {
            "Test model"
        }

        async fn evses(&self) -> &[TestEvse] {
            &self.evses
        }

        async fn capabilities(&self) -> crate::hardware::Capabilities {
            crate::hardware::Capabilities::default()
        }

        async fn start(
            &self,
            events: HardwareEventSender,
            mut commands: HardwareCommandReceiver,
        ) -> Result<(), Self::StartError> {
            events
                .send(ChargePointEvent::Evse {
                    evse_id: 0,
                    event: EvseEvent::Connector {
                        connector_id: 0,
                        event: ConnectorEvent::CableConnected,
                    },
                })
                .await
                .unwrap();
            execute_hardware_command(&self.evses, commands.recv().await.unwrap(), &events).await;
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl Evse<TestConnector> for TestEvse {
        type Error = TestConnectorError;

        async fn connectors(&self) -> &[TestConnector] {
            &self.connectors
        }

        async fn reboot(&self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl Connector for TestConnector {
        type Error = TestConnectorError;

        async fn lock(&self) -> Result<(), Self::Error> {
            if self.lock_succeeds {
                self.locked
                    .store(true, core::sync::atomic::Ordering::SeqCst);
                Ok(())
            } else {
                Err(TestConnectorError)
            }
        }

        async fn unlock(&self) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn close_contactor(&self) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn open_contactor(&self) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn set_current_limit(&self, _limit_ma: u32) -> Result<(), Self::Error> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ChargePointBuilder;
    use super::test_support::{TestChargePoint, TestConnector, TestEvse};
    use crate::executor::TokioExecutor;
    use crate::provisioning::BootNotificationOutcome;
    use crate::provisioning::TokioBackoff;
    use crate::provisioning::test_support::FixedBootNotifier;
    use crate::state::{ConnectorState, RegistrationStatus};
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, Ordering};

    fn accepted_boot_notifier() -> FixedBootNotifier {
        FixedBootNotifier(BootNotificationOutcome {
            status: RegistrationStatus::Accepted,
            interval_secs: 60,
        })
    }

    fn test_charge_point(lock_succeeds: bool) -> (TestChargePoint, Arc<AtomicBool>) {
        let locked = Arc::new(AtomicBool::new(false));
        (
            TestChargePoint {
                evses: [TestEvse {
                    connectors: [TestConnector {
                        locked: locked.clone(),
                        lock_succeeds,
                    }],
                }],
            },
            locked,
        )
    }

    /// A CSMS type implementing only Provisioning's traits - notably *not* `SecurityEventNotifier`
    /// or any of the other 18 traits `setup()` requires all at once. This is the type that
    /// couldn't be driven through `setup()` at all: the whole point of the builder refactor is
    /// that registering only the blocks a client supports compiles.
    #[derive(Clone)]
    struct ProvisioningOnlyCsms(FixedBootNotifier);

    #[async_trait::async_trait]
    impl crate::provisioning::BootNotifier for ProvisioningOnlyCsms {
        type Error = core::convert::Infallible;

        async fn notify_boot(
            &self,
            vendor_name: &str,
            model_name: &str,
        ) -> Result<BootNotificationOutcome, Self::Error> {
            self.0.notify_boot(vendor_name, model_name).await
        }
    }

    #[async_trait::async_trait]
    impl crate::provisioning::HeartbeatSender for ProvisioningOnlyCsms {
        type Error = core::convert::Infallible;

        async fn send_heartbeat(&self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::connection::ReconnectHandler for ProvisioningOnlyCsms {
        async fn register_reconnect_handler<F, FF>(&self, _callback: F)
        where
            F: FnMut() -> FF + Send + Sync + 'static,
            FF: core::future::Future<Output = ()> + Send + 'static,
        {
            // No transport to reconnect in this fixture - nothing to register against.
        }
    }

    #[tokio::test]
    async fn a_csms_implementing_only_provisioning_compiles_and_boots() {
        let (charge_point, locked) = test_charge_point(true);
        let csms = ProvisioningOnlyCsms(accepted_boot_notifier());

        let runtime = ChargePointBuilder::start(charge_point, TokioExecutor)
            .await
            .unwrap()
            .provisioning(&csms, TokioBackoff)
            .await
            .build();

        assert_eq!(
            runtime.state().evses[0].connectors[0],
            ConnectorState::Locked
        );
        assert!(locked.load(Ordering::SeqCst));
        assert_eq!(
            runtime.state().registration,
            Some(RegistrationStatus::Accepted)
        );
    }

    #[tokio::test]
    async fn registering_a_block_twice_or_skipping_it_does_not_panic() {
        let (charge_point, _locked) = test_charge_point(true);
        let csms = ProvisioningOnlyCsms(accepted_boot_notifier());

        // `provisioning` is registered twice, and every other block is skipped entirely; neither
        // should panic.
        let runtime = ChargePointBuilder::start(charge_point, TokioExecutor)
            .await
            .unwrap()
            .provisioning(&csms, TokioBackoff)
            .await
            .provisioning(&csms, TokioBackoff)
            .await
            .build();

        assert_eq!(
            runtime.state().registration,
            Some(RegistrationStatus::Accepted)
        );
    }

    /// A CSMS type implementing only `StatusNotifier` + `ReconnectHandler`, counting every
    /// `notify_status` call it receives - used to prove that registering `status_notifications`
    /// twice results in exactly one StatusNotification per connector status change, not two (see
    /// module docs: the repeat registration is a no-op, not a second forwarder).
    #[derive(Clone)]
    struct CountingStatusCsms {
        calls: Arc<core::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::availability::StatusNotifier for CountingStatusCsms {
        type Error = core::convert::Infallible;

        async fn notify_status(
            &self,
            _evse_id: usize,
            _connector_id: usize,
            _status: crate::state::ConnectorStatus,
            _connector_state: ConnectorState,
        ) -> Result<(), Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::connection::ReconnectHandler for CountingStatusCsms {
        async fn register_reconnect_handler<F, FF>(&self, _callback: F)
        where
            F: FnMut() -> FF + Send + Sync + 'static,
            FF: core::future::Future<Output = ()> + Send + 'static,
        {
            // No transport to reconnect in this fixture - nothing to register against.
        }
    }

    #[tokio::test]
    async fn registering_status_notifications_twice_forwards_each_status_change_once() {
        // `test_charge_point`'s `TestChargePoint::start` fires a single `CableConnected` event,
        // which is the one wire-visible connector status change (Available -> Occupied) this test
        // needs: registering `status_notifications` a second time must not cause that single
        // change to be reported twice.
        let (charge_point, _locked) = test_charge_point(true);
        let calls = Arc::new(core::sync::atomic::AtomicUsize::new(0));
        let csms = CountingStatusCsms {
            calls: calls.clone(),
        };

        let runtime = ChargePointBuilder::start(charge_point, TokioExecutor)
            .await
            .unwrap()
            .status_notifications(&csms)
            .await
            .status_notifications(&csms)
            .await
            .build();

        // Give the spawned forwarder task(s) a chance to process the `CableConnected` event fired
        // during `start()` before establishing the baseline below.
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        let before = calls.load(Ordering::SeqCst);

        // Drive one more, genuinely wire-visible connector status change (Occupied -> Faulted)
        // through the actor, so any forwarder registered by *either* `status_notifications` call
        // - not just the one wired up in `start()` - would see it. `FaultDetected` doesn't need a
        // hardware round-trip to take effect, unlike e.g. `CableDisconnected` while locked.
        runtime
            .actor()
            .send(crate::state::ChargePointEvent::Evse {
                evse_id: 0,
                event: crate::state::EvseEvent::Connector {
                    connector_id: 0,
                    event: crate::state::ConnectorEvent::FaultDetected,
                },
            })
            .await
            .unwrap();

        for _ in 0..100 {
            tokio::task::yield_now().await;
        }

        // Exactly one StatusNotification per status change: the repeat `status_notifications`
        // registration must not have spawned a second forwarder.
        assert_eq!(calls.load(Ordering::SeqCst) - before, 1);
    }

    #[cfg(all(
        feature = "reservation",
        feature = "local-auth-list",
        feature = "tariff-cost"
    ))]
    #[tokio::test]
    async fn a_fully_configured_builder_behaves_like_setup() {
        let (charge_point, locked) = test_charge_point(false);
        let csms = accepted_boot_notifier();

        // `FixedBootNotifier` implements every functional block's trait (see
        // `crate::provisioning::test_support`), so this exercises the same "everything on" shape
        // `setup()` wraps, but built up one call per block instead of one giant bound.
        let runtime = ChargePointBuilder::start(charge_point, TokioExecutor)
            .await
            .unwrap()
            .provisioning(&csms, TokioBackoff)
            .await
            .status_notifications(&csms)
            .await
            .transaction_events(&csms)
            .await
            .authorization(&csms)
            .await
            .security_events(&csms)
            .await
            .remote_control(&csms)
            .await
            .availability_control(&csms)
            .await
            .reservation(&csms)
            .await
            .reset(&csms)
            .await
            .local_authorization_list(&csms)
            .await
            .device_model(&csms)
            .await
            .cost(&csms)
            .await
            .build();

        assert_eq!(
            runtime.state().evses[0].connectors[0],
            ConnectorState::Faulted
        );
        assert!(!locked.load(Ordering::SeqCst));
    }
}
