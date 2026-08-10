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
use crate::clock::MonotonicClock;
use crate::connection::{ReconnectHandler, reregister_on_reconnect};
#[cfg(feature = "tariff-cost")]
use crate::cost::CostUpdatedHandler;
#[cfg(feature = "der-control")]
use crate::der_control::{
    AfrrSignalHandler, ClearDERControlHandler, GetDERControlHandler,
    NotifyAllowedEnergyTransferHandler, SetDERControlHandler,
};
use crate::device_model::{GetVariablesHandler, SetVariablesHandler};
use crate::executor::Executor;
use crate::hardware::ChargePoint;
use crate::hardware::Connector;
use crate::hardware::Evse;
use crate::hardware::{Capabilities, warn_on_feature_mismatches};
#[cfg(feature = "local-auth-list")]
use crate::local_authorization_list::{GetLocalListVersionHandler, SendLocalListHandler};
use crate::offline_queue::{
    OfflineQueue, OverflowPolicy, run_with_offline_queue, run_with_offline_queue_where,
};
#[cfg(feature = "periodic-event-stream")]
use crate::periodic_event_stream::{
    AdjustPeriodicEventStreamHandler, ClosePeriodicEventStreamHandler,
    GetPeriodicEventStreamHandler, OpenPeriodicEventStreamHandler, PeriodicEventStreamNotifier,
    run_periodic_event_streams,
};
use crate::persistence::{
    AuthorizationCacheStore, BootReasonStore, DeviceModelStore, NetworkProfileSnapshotStore,
    QueueStore, TransactionStore, flush_and_persist_security_event_queue,
    flush_and_persist_status_notification_queue, flush_and_persist_transaction_event_queue,
    restore_authorization_cache, restore_device_model, restore_network_profiles,
    restore_security_event_queue, restore_security_log, restore_status_notification_queue,
    restore_transaction_event_queue, restore_transactions, run_authorization_cache_persistence,
    run_device_model_persistence, run_network_profile_persistence,
    run_persisted_security_event_queue, run_persisted_status_notification_queue,
    run_persisted_transaction_event_queue, run_security_log_persistence,
    run_transaction_persistence,
};
// Split out from the `crate::persistence` import above (C4.2): each of these backs exactly one
// feature-gated registration method below, so importing it unconditionally would leave an unused
// import (denied by `-D warnings`) whenever that block's Cargo feature is off.
#[cfg(feature = "smart-charging")]
use crate::persistence::run_charging_profile_persistence;
#[cfg(feature = "smart-charging")]
use crate::persistence::{ChargingProfileSnapshotStore, restore_charging_profiles};
#[cfg(feature = "local-auth-list")]
use crate::persistence::{
    LocalAuthorizationListStore, restore_local_authorization_list,
    run_local_authorization_list_persistence,
};
#[cfg(feature = "reservation")]
use crate::persistence::{ReservationStore, restore_reservations, run_reservation_persistence};
use crate::provisioning::{Backoff, BootNotifier, HeartbeatSender, run_heartbeat};
use crate::remote_control::{
    RequestStartTransactionHandler, RequestStopTransactionHandler, UnlockConnectorHandler,
};
use crate::reporting::{GetBaseReportHandler, GetReportHandler};
#[cfg(feature = "reservation")]
use crate::reservation::{CancelReservationHandler, ReserveNowHandler};
use crate::reset::ResetHandler;
use crate::security::{SecurityEventNotifier, report_security_event};
use crate::state::{
    AuthorizationRequested, BootReasonCause, ChargePointEvent, Component, ConnectorStatusChanged,
    DeviceModelEvent, SecurityEvent, SecurityEventType, TransactionEventOccurred, Variable,
    VariableAttributeType,
};
use crate::sync::BroadcastReceiver;
#[cfg(feature = "tariff-cost")]
use crate::tariff::{
    ChangeTransactionTariffHandler, ClearTariffsHandler, GetTariffsHandler, SetDefaultTariffHandler,
};
use crate::transactions::TransactionNotifier;
#[cfg(feature = "variable-monitoring")]
use crate::variable_monitoring::{
    ClearVariableMonitoringHandler, GetMonitoringReportHandler, SetMonitoringBaseHandler,
    SetMonitoringLevelHandler, SetVariableMonitoringHandler, VariableMonitorEventNotifier,
};
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

/// Builds a [`ChargePointRuntime`] by starting the hardware and then registering functional
/// blocks against a CSMS client one at a time, rather than requiring a single client type that
/// implements every block at once. See the module docs for why this exists.
///
/// Each registration method (`provisioning`, `status_notifications`, ...) consumes and returns
/// `Self`, so calls chain: `Builder::start(hw, ex).await?.provisioning(&csms, backoff,
/// monotonic).await.status_notifications(&csms).await....build()`. Blocks may be registered in any
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
    // A second transaction-event subscription, independent of the one the Transactions block
    // consumes: durability and CSMS reporting are separate concerns that must not starve each
    // other, and persistence has to see every event the CSMS forwarder does. Taken in `start()`
    // for the same reason as the other four - so a transaction the hardware reports during
    // start-up is persisted rather than missed.
    transaction_persistence_events: Option<BroadcastReceiver<TransactionEventOccurred>>,
    // A second security-event subscription, independent of the one the Security block consumes,
    // for the same reason `transaction_persistence_events` is independent of `transaction_events`:
    // the security *log* (E2.10) must record every event whether or not it ever reaches the CSMS,
    // and must not starve - or be starved by - the CSMS forwarder. Taken in `start()` so an event
    // raised during hardware start-up is logged rather than missed.
    security_log_events: Option<BroadcastReceiver<SecurityEvent>>,
    // The offline queue `Self::transaction_events`/`Self::transaction_events_persisted` created
    // for the Transactions block's CSMS forwarding, kept so `Self::get_transaction_status` can
    // answer `GetTransactionStatus`'s `messagesInQueue` from the real backlog rather than a
    // fabricated "always false". `None` until one of those two is registered - and stays `None`
    // forever if neither ever is, which is still correct: nothing this crate forwards is ever
    // queued through a queue that doesn't exist, so `messagesInQueue` genuinely is always false
    // in that case. See `crate::transaction_status`.
    transaction_queue: Option<Arc<OfflineQueue<TransactionEventOccurred>>>,
    // One entry per offline queue registered so far, each a closure that flushes that queue
    // exactly as its reconnect handler does. `Self::offline_queue_retries` drives them all on a
    // timer; collecting closures rather than the queues themselves keeps the builder free of the
    // per-queue message types, which differ.
    queue_flushes: Vec<QueueFlush>,
    // Set by `offline_queue_capacity`, read by every queue-creating registration below. Fixed
    // for the life of the builder, like `StateLimits` is for the state - a bound a later call
    // could raise is not a bound.
    offline_queue_capacity: usize,
    // Set by `boot_reason_persistence`, read by `provisioning`: the cause loaded from durable
    // storage at build time (`None` - no `boot_reason_persistence` call, or nothing was
    // persisted - reports an uncommanded restart, same as before this feature existed). Fixed for
    // the life of the process once `provisioning` reads it, including every reconnect's resend -
    // see `crate::connection::reregister_on_reconnect`'s docs for why it's never re-read.
    boot_reason: Option<BootReasonCause>,
    // Set by `boot_reason_persistence`, consumed by `provisioning` once the CSMS accepts
    // registration - see `BootReasonClearer`'s docs for why this is type-erased rather than a
    // generic `BootReasonStore<S>` field (which would force `S` onto `ChargePointBuilder` itself).
    boot_reason_clearer: Option<Arc<dyn BootReasonClearer>>,
}

/// One registered offline queue's flush, type-erased so [`ChargePointBuilder`] can hold flushes
/// for queues whose message types differ - see that struct's `queue_flushes` field.
type QueueFlush = Arc<
    dyn Fn() -> core::pin::Pin<Box<dyn core::future::Future<Output = ()> + Send>> + Send + Sync,
>;

/// Type-erases a [`BootReasonStore<S>`]'s `clear` so [`ChargePointBuilder`] can hold one without
/// becoming generic over `S` itself - the same reason `alloc::sync::Arc<dyn Trait>` gets reached
/// for elsewhere once a concrete generic type would otherwise "infect" a struct that only needs
/// one method off it. Only [`Self::clear`] is exposed; [`BootReasonStore::save`]/`load` stay
/// reachable solely through [`crate::actor::ChargePointActor::set_boot_reason_recorder`]'s closure
/// and `boot_reason_persistence`'s own local variable.
#[async_trait::async_trait]
trait BootReasonClearer: Send + Sync {
    /// See [`BootReasonStore::clear`].
    async fn clear(&self);
}

#[async_trait::async_trait]
impl<S: crate::hardware::Storage + Send + Sync> BootReasonClearer for BootReasonStore<S> {
    async fn clear(&self) {
        BootReasonStore::clear(self).await;
    }
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
        Self::start_with_limits(charge_point, executor, crate::state::StateLimits::default()).await
    }

    /// [`Self::start`] with caller-chosen bounds on the state's growable collections (the local
    /// authorization list and the device model) - see [`crate::state::StateLimits`] and
    /// `docs/PRODUCTION-ROADMAP.md` §9.2 (G2.2). Limits have to be supplied here, before the
    /// hardware binding's `start` gets to register anything, because they are fixed for the life of
    /// the state; there is deliberately no way to raise one later.
    ///
    /// A binding that registers more device model variables than
    /// [`StateLimits::max_device_model_variables`](crate::state::StateLimits::max_device_model_variables)
    /// allows gets the surplus registrations refused and logged - raise the limit rather than let
    /// the model silently miss components.
    pub async fn start_with_limits<E, C>(
        charge_point: T,
        executor: X,
        limits: crate::state::StateLimits,
    ) -> Result<Self, T::StartError>
    where
        T: ChargePoint<E, C>,
        E: Evse<C>,
        C: Connector,
    {
        let vendor_name = charge_point.vendor_name().to_string();
        let model_name = charge_point.model_name().to_string();
        let capabilities = charge_point.capabilities();
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
        for evse in charge_point.evses() {
            connector_counts.push(evse.connectors().len());
        }

        let runtime =
            ChargePointRuntime::new_with_limits(charge_point, connector_counts, &executor, limits);
        // Subscribe before starting the hardware so status/transaction/authorization events fired
        // during `start()` (e.g. a connector that's already occupied at boot) are buffered rather
        // than lost.
        let status_changes = runtime.subscribe_status_notifications();
        let transaction_events = runtime.subscribe_transaction_events();
        let authorization_requests = runtime.subscribe_authorization_requests();
        let security_events = runtime.subscribe_security_events();
        let transaction_persistence_events = runtime.subscribe_transaction_events();
        let security_log_events = runtime.subscribe_security_events();

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
            .hardware_handle()
            .start(runtime.hardware_events(), runtime.hardware_commands())
            .await?;

        // F4.2: OCPP's `StartupOfTheDevice`, a critical security event. Raised after the hardware
        // binding has started - a charge point that failed to start has not started, and reporting
        // a boot that then errored out would be reporting something that did not happen.
        //
        // It is *raised* here, not delivered: the subscriptions above are already buffering, so it
        // sits in the queue until a Security block is registered and the CSMS connection is up.
        // That ordering is what makes it useful at all - a boot event that needed a live
        // connection to be raised could never report the boot that follows a power cut.
        report_security_event(
            &runtime.actor(),
            SecurityEvent {
                event_type: SecurityEventType::StartupOfTheDevice,
                tech_info: Some(alloc::format!("{vendor_name} {model_name}")),
            },
        )
        .await;

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
            transaction_persistence_events: Some(transaction_persistence_events),
            security_log_events: Some(security_log_events),
            transaction_queue: None,
            queue_flushes: Vec::new(),
            offline_queue_capacity: crate::offline_queue::DEFAULT_CAPACITY,
            boot_reason: None,
            boot_reason_clearer: None,
        })
    }

    /// Bounds every offline report queue this builder creates (status, transaction, security) at
    /// `capacity` messages instead of [`crate::offline_queue::DEFAULT_CAPACITY`].
    ///
    /// **Call this before registering any of those blocks** - a queue is created when its block
    /// registers, so a later call cannot resize one that already exists. That is deliberate: a
    /// queue whose bound can move underneath it is not bounded in the sense G2.1 means.
    ///
    /// The trade-off is memory against how long an outage the charge point can absorb without
    /// losing reports - `docs/MEMORY.md` prices a queued message per kind, and
    /// [`crate::offline_queue::OverflowPolicy`] decides what goes when the bound is hit.
    pub fn offline_queue_capacity(mut self, capacity: usize) -> Self {
        self.offline_queue_capacity = capacity;
        self
    }

    /// Spawns a timer that retries every offline queue registered so far, every
    /// `OCPPCommCtrlr`/`MessageAttemptInterval[TransactionEvent]` seconds (A7).
    ///
    /// **Call this after the blocks whose queues you want retried** - it drives the queues that
    /// exist when it runs, and registration order is otherwise irrelevant.
    ///
    /// Without it a queued report is only retried when *new* traffic arrives or the connection
    /// reconnects. That is enough for a busy charge point and wrong for a quiet one: the last
    /// report before an outage would sit indefinitely on a charge point that has gone quiet -
    /// which is exactly the charge point most likely to have gone quiet *because* it is offline.
    ///
    /// The interval is re-read every cycle, so a CSMS changing it takes effect on the next sweep
    /// without a reboot; `fallback_interval_secs` covers it being absent, unparseable or `0`
    /// (which would be a busy-spin). Sweeps run concurrently with the forwarder's own flush and
    /// the reconnect flush, and [`crate::offline_queue::flush_offline_queue`]'s claim makes that
    /// overlap a no-op rather than a double-send.
    pub fn offline_queue_retries<B>(self, backoff: B, fallback_interval_secs: u32) -> Self
    where
        B: Backoff + Send + Sync + 'static,
    {
        let flushes = self.queue_flushes.clone();
        if flushes.is_empty() {
            tracing::warn!(
                "offline_queue_retries registered before any queue - nothing will be retried on a \
                 timer; call it after the blocks whose queues you want swept"
            );
            return self;
        }
        let actor = self.runtime.actor();
        self.executor.spawn(Box::pin(async move {
            loop {
                backoff
                    .wait(crate::offline_queue::message_attempt_interval_secs(
                        &actor,
                        fallback_interval_secs,
                    ))
                    .await;
                for flush in &flushes {
                    flush().await;
                }
            }
        }));

        self
    }

    /// This charge point's connector topology - `connector_counts[evse_id]` is that EVSE's
    /// connector count, in the shape this crate's 1.6J connector-address helpers take.
    ///
    /// Read back from the state the hardware binding established in [`Self::start`], so a caller
    /// that handed its `ChargePoint` over can still build the version adapters that need it -
    /// every 1.6J wrapper does, since 1.6J addresses connectors with a single flat id and has no
    /// EVSE concept to derive one from.
    pub fn connector_counts(&self) -> Vec<usize> {
        self.runtime
            .actor()
            .state()
            .evses
            .iter()
            .map(|evse| evse.connectors.len())
            .collect()
    }

    /// The capabilities the hardware declared via
    /// [`ChargePoint::capabilities`](crate::hardware::ChargePoint::capabilities), captured once in
    /// [`Self::start`]. The single source of truth callers (e.g. [`crate::setup::setup`]) consult
    /// to decide which registration methods below to actually call - see
    /// `docs/PRODUCTION-ROADMAP.md` §5.3 (C3).
    pub fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    /// Registers inbound `TriggerMessage` handling (`docs/ROADMAP.md` §6,
    /// `docs/PRODUCTION-ROADMAP.md` B1.3/B1.4): a CSMS asking the charge point to re-send a
    /// message it can produce - a `Heartbeat`, or a `StatusNotification` for the whole charge
    /// point, one EVSE or one connector - gets it, and anything else gets OCPP's
    /// `NotImplemented`.
    ///
    /// Registered separately from [`Self::remote_control`] rather than inside it: `TriggerMessage`
    /// needs a CSMS type that can *send* the triggered messages too (a `HeartbeatSender` and a
    /// `StatusNotifier`, not just a handler registration), and folding that bound into
    /// `remote_control` would force it on callers who only wanted `UnlockConnector`. Under 1.6J
    /// those two senders live on different types, so pass
    /// [`crate::remote_control::Ocpp1_6TriggerMessageHandler`], which is both.
    pub async fn trigger_message<N>(self, csms: &N) -> Self
    where
        N: crate::remote_control::TriggerMessageHandler + Send + Sync + 'static,
    {
        csms.register_trigger_message_handler(self.runtime.actor())
            .await;
        self
    }

    /// Registers standalone `MeterValues` (`docs/ROADMAP.md` §10,
    /// `docs/PRODUCTION-ROADMAP.md` B1.1): a background loop that reports every connector's latest
    /// meter reading on the wall-clock schedule the CSMS configures through
    /// `AlignedDataCtrlr`/`Interval`.
    ///
    /// Sends nothing until that variable is non-zero - OCPP's own "disabled" default - and picks
    /// the change up on the next cycle, so a CSMS can turn clock-aligned data on and off at
    /// runtime without a reboot. Unlike [`Self::transaction_events`], this reports readings taken
    /// with no transaction running at all, which is the whole point of the message.
    ///
    /// `backoff`/`clock` are caller-supplied for the same no_std reason [`Self::provisioning`]'s
    /// are.
    pub async fn meter_values<N, B, K>(self, csms: &N, backoff: B, clock: K) -> Self
    where
        N: crate::meter_values::MeterValuesNotifier + Clone + Send + Sync + 'static,
        B: Backoff + Send + Sync + 'static,
        K: crate::clock::Clock + Send + Sync + 'static,
    {
        let actor = self.runtime.actor();
        let notifier = csms.clone();
        self.executor.spawn(Box::pin(async move {
            crate::meter_values::run_aligned_meter_values(&notifier, &backoff, &clock, &actor)
                .await;
        }));

        self
    }

    /// Registers the variable monitoring engine's outbound reporting: every threshold/delta
    /// monitor that fires is forwarded to the CSMS via `NotifyEvent` as it happens, and every
    /// periodic monitor is swept and reported every `periodic_sweep_interval_secs`
    /// (`docs/PRODUCTION-ROADMAP.md` §B5, B5.2).
    ///
    /// Two background loops, for the same reason [`Self::reservation_status_updates`] spawns two:
    /// a threshold/delta trigger is event-driven (forwarded off
    /// [`crate::actor::ChargePointActor::subscribe_variable_monitor_events`] as it happens) while
    /// a periodic monitor has nothing to be "driven" by - it fires on its own clock regardless of
    /// whether anything changed, which is exactly what
    /// [`crate::variable_monitoring::run_periodic_variable_monitors`]'s sweep loop is for.
    ///
    /// **2.x only** - see [`Self::variable_monitoring`]'s docs. `backoff`/`clock` are
    /// caller-supplied for the same no_std reason [`Self::provisioning`]'s are.
    ///
    /// Only present when the `variable-monitoring` Cargo feature is enabled (C4.2).
    #[cfg(feature = "variable-monitoring")]
    pub fn variable_monitor_events<N, B, K>(
        self,
        csms: &N,
        backoff: B,
        clock: K,
        periodic_sweep_interval_secs: u32,
    ) -> Self
    where
        N: VariableMonitorEventNotifier + Clone + Send + Sync + 'static,
        B: Backoff + Clone + Send + Sync + 'static,
        K: crate::clock::Clock + Clone + Send + Sync + 'static,
    {
        let events = self.runtime.actor().subscribe_variable_monitor_events();
        let notifier = csms.clone();
        let events_actor = self.runtime.actor();
        self.executor.spawn(Box::pin(async move {
            crate::variable_monitoring::run_variable_monitor_events(
                events,
                &notifier,
                &events_actor,
            )
            .await;
        }));

        let actor = self.runtime.actor();
        let notifier = csms.clone();
        self.executor.spawn(Box::pin(async move {
            crate::variable_monitoring::run_periodic_variable_monitors(
                &notifier,
                &clock,
                &backoff,
                periodic_sweep_interval_secs,
                &actor,
            )
            .await;
        }));

        self
    }

    /// Registers durable authorization-cache state (`docs/PRODUCTION-ROADMAP.md` §7.2, E2.5):
    /// recovers the decisions the CSMS had already made before the charge point last lost power,
    /// then persists every subsequent change through `storage` for the life of the process.
    ///
    /// **Call this before [`Self::authorization`]**, so the cache is in place before anything can
    /// present an identifier - the case this exists for is a charge point that reboots *while its
    /// CSMS is unreachable*, where an empty cache means every card is refused until the link comes
    /// back. `storage` may be [`crate::hardware::NoStorage`], in which case this behaves exactly
    /// as if it were never called.
    ///
    /// Nothing is filtered by age at boot; entry expiry is evaluated at lookup instead - see
    /// [`crate::persistence::restore_authorization_cache`] for why that is the correct place for
    /// it.
    pub async fn authorization_cache_persistence<S>(self, storage: S) -> Self
    where
        S: crate::hardware::Storage + Send + Sync + 'static,
    {
        let store = Arc::new(AuthorizationCacheStore::new(storage));
        restore_authorization_cache(&self.runtime.actor(), &store).await;

        let state_changes = self.runtime.actor().subscribe();
        self.executor.spawn(Box::pin(async move {
            run_authorization_cache_persistence(state_changes, &store).await;
        }));

        self
    }

    /// Registers durable charging-profile state (`docs/PRODUCTION-ROADMAP.md` §7.2, E2.7):
    /// recovers whatever load limits the CSMS had installed before the charge point last lost
    /// power, then persists every subsequent change through `storage` for the life of the process.
    ///
    /// **Call this before [`Self::smart_charging`]** - the restore must land before the
    /// charging-limit projection first evaluates, or the charge point spends its first moments
    /// back believing nothing limits it. Registering the two in this order is all that takes; the
    /// restore completes before this method returns.
    ///
    /// Without it a power cut silently un-limits a load-managed charge point: the profiles are
    /// gone, the projection computes no limit, and hardware goes back to its own maximum until the
    /// CSMS notices and re-sends. `storage` may be [`crate::hardware::NoStorage`], in which case
    /// this behaves exactly as if it were never called.
    ///
    /// `clock` decides whether a persisted profile's `valid_to` is trustworthy to compare
    /// against; see [`crate::persistence::restore_charging_profiles`] for what an unsynchronized
    /// clock does (nothing, deliberately).
    ///
    /// Only present when the `smart-charging` Cargo feature is enabled (C4.2). Unlike
    /// [`Self::reservation`]'s `crate::reservation`, `crate::smart_charging`'s types stay
    /// compiled regardless of this feature - `crate::tariff`/`crate::device_model`/
    /// `crate::provisioning` depend on them unconditionally - so this gates only the
    /// registration entry point, not the module.
    #[cfg(feature = "smart-charging")]
    pub async fn charging_profile_persistence<S, K>(self, storage: S, clock: K) -> Self
    where
        S: crate::hardware::Storage + Send + Sync + 'static,
        K: crate::clock::Clock + Send + Sync + 'static,
    {
        let store = Arc::new(ChargingProfileSnapshotStore::new(storage));
        restore_charging_profiles(&self.runtime.actor(), &store, &clock).await;

        let state_changes = self.runtime.actor().subscribe();
        self.executor.spawn(Box::pin(async move {
            run_charging_profile_persistence(state_changes, &store).await;
        }));

        self
    }

    /// Registers the Smart Charging functional block (`docs/ROADMAP.md` §11,
    /// `docs/PRODUCTION-ROADMAP.md` B2): the `SetChargingProfile`, `ClearChargingProfile` and
    /// `GetCompositeSchedule` handlers, plus the two background loops that project the composite
    /// schedule onto [`crate::hardware::Connector::set_current_limit`].
    ///
    /// `projection` is shared between the loops and the `GetCompositeSchedule` handler on purpose:
    /// what the CSMS is told the charge point *will* do has to come from the same composition that
    /// decides what it actually does. Construct it with
    /// [`ChargingLimitProjection::with_supply`](crate::smart_charging::ChargingLimitProjection::with_supply)
    /// when the installation's voltage and phase count are known - without it, a profile
    /// denominated in watts is skipped rather than converted, since this crate will not guess a
    /// supply it cannot see.
    ///
    /// `clock` stamps the installation instant onto a schedule the CSMS left unanchored and drives
    /// composition's "now"; `backoff` is what the period-boundary loop sleeps on - the same
    /// caller-supplied primitives [`Self::provisioning`] takes, for the same no_std reason.
    ///
    /// Registered by [`crate::setup::setup`] only when the hardware declares
    /// [`Capabilities::smart_charging`](crate::hardware::Capabilities::smart_charging) - a charge
    /// point that cannot limit its current has nothing to project a schedule onto (C3.1).
    ///
    /// Only present when the `smart-charging` Cargo feature is enabled - see
    /// [`Self::charging_profile_persistence`]'s doc comment for why the module itself does not
    /// disappear alongside this method (C4.2).
    #[cfg(feature = "smart-charging")]
    pub async fn smart_charging<N, K, B>(
        self,
        csms: &N,
        projection: Arc<crate::smart_charging::ChargingLimitProjection>,
        clock: K,
        backoff: B,
    ) -> Self
    where
        N: crate::smart_charging::SetChargingProfileHandler
            + crate::smart_charging::ClearChargingProfileHandler
            + crate::smart_charging::GetCompositeScheduleHandler
            + Send
            + Sync
            + 'static,
        K: crate::clock::Clock + Clone + Send + Sync + 'static,
        B: Backoff + Send + Sync + 'static,
    {
        csms.register_set_charging_profile_handler(self.runtime.actor())
            .await;
        csms.register_clear_charging_profile_handler(self.runtime.actor())
            .await;
        csms.register_get_composite_schedule_handler(self.runtime.actor(), projection.clone())
            .await;

        let state_driven_actor = self.runtime.actor();
        let state_driven_projection = projection.clone();
        let state_driven_clock = clock.clone();
        self.executor.spawn(Box::pin(async move {
            crate::smart_charging::run_charging_limit_projection(
                &state_driven_actor,
                &state_driven_projection,
                &state_driven_clock,
            )
            .await;
        }));
        let timer_actor = self.runtime.actor();
        self.executor.spawn(Box::pin(async move {
            crate::smart_charging::run_charging_limit_schedule(
                &timer_actor,
                &projection,
                &clock,
                &backoff,
            )
            .await;
        }));

        self
    }

    /// Registers durable boot-reason state (`docs/PRODUCTION-ROADMAP.md` §7.2/§7.4: the
    /// boot-reason row of E2, and E4.2): loads whatever cause `crate::reset::handle_reset`
    /// recorded before this boot (if any), so [`Self::provisioning`] sends the honest
    /// `BootNotification.reason` for it, then installs a recorder on the actor so a *future*
    /// commanded reboot gets the same treatment.
    ///
    /// Call this **before** [`Self::provisioning`] - the loaded cause has to be in hand before
    /// that method sends the first BootNotification. `storage` may be
    /// [`crate::hardware::NoStorage`], in which case this behaves exactly as if it were never
    /// called: every boot reports an uncommanded restart, and no reset is ever recorded.
    ///
    /// # Timing this crate chose, and why
    ///
    /// The persisted cause is **not** cleared here, and not before the BootNotification is sent -
    /// it is cleared by [`Self::provisioning`] only once the CSMS has *accepted* registration.
    /// Clearing any earlier would mean a crash between the reboot and a successful registration
    /// makes the *next* boot wrongly report an uncommanded restart, even though it's still really
    /// recovering from the original commanded one - the same "don't lose information you don't
    /// have to" stance `crate::persistence`'s other stores take. The cause is also not re-read
    /// from storage on every reconnect - see `crate::connection::reregister_on_reconnect`'s docs
    /// for why a fixed, once-loaded value is what every resend during this boot should send.
    pub async fn boot_reason_persistence<S>(mut self, storage: S) -> Self
    where
        S: crate::hardware::Storage + Send + Sync + 'static,
    {
        let store = Arc::new(BootReasonStore::new(storage));

        self.boot_reason = store.load().await;
        self.boot_reason_clearer = Some(store.clone());

        let recorder_store = store.clone();
        self.runtime.actor().set_boot_reason_recorder(Arc::new(
            move |kind: crate::state::ResetKind| {
                let recorder_store = recorder_store.clone();
                Box::pin(async move {
                    recorder_store.save(BootReasonCause::from(kind)).await;
                })
                    as core::pin::Pin<Box<dyn core::future::Future<Output = ()> + Send>>
            },
        ));

        self
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
    /// embedded targets supply their own. `monotonic` is likewise caller-supplied - std/tokio
    /// users can pass [`crate::clock::SystemMonotonicClock`]; embedded targets supply their own
    /// free-running timer. It anchors the CSMS's BootNotification/Heartbeat `currentTime`
    /// against elapsed real time so repeated syncs don't re-report routine drift as a fresh step
    /// - see `crate::provisioning::register_until_accepted`'s and `run_heartbeat`'s docs.
    pub async fn provisioning<N, B, M>(self, csms: &N, backoff: B, monotonic: M) -> Self
    where
        N: BootNotifier + HeartbeatSender + ReconnectHandler + Clone + Send + Sync + 'static,
        B: Backoff + Clone + Send + Sync + 'static,
        M: MonotonicClock + Clone + Send + Sync + 'static,
    {
        let vendor_name = self.vendor_name.as_str();
        let model_name = self.model_name.as_str();
        let boot_reason = self.boot_reason;
        let outcome = self
            .runtime
            .register_until_accepted(
                csms,
                &backoff,
                &monotonic,
                vendor_name,
                model_name,
                boot_reason,
            )
            .await;

        // The BootNotification carrying `boot_reason` has now been accepted - clear the
        // persisted cause (if `boot_reason_persistence` registered one) so a *future* uncommanded
        // restart doesn't wrongly keep reporting it. See
        // `crate::persistence::BootReasonStore::clear`'s docs for why this only happens now,
        // after acceptance, rather than before sending.
        if let Some(clearer) = &self.boot_reason_clearer {
            clearer.clear().await;
        }

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
            monotonic.clone(),
            vendor_name.into(),
            model_name.into(),
            boot_reason,
        )
        .await;

        let heartbeat_sender = csms.clone();
        let heartbeat_backoff = backoff.clone();
        let heartbeat_monotonic = monotonic.clone();
        let heartbeat_actor = self.runtime.actor();
        self.executor.spawn(Box::pin(async move {
            run_heartbeat(
                &heartbeat_sender,
                &heartbeat_backoff,
                &heartbeat_monotonic,
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
        // `DropOldest` (the default): a queued status is superseded by whatever the connector's
        // status actually is by the time the connection recovers, so keeping the newest is more
        // useful than keeping the oldest - see `OverflowPolicy`'s docs.
        let status_queue = Arc::new(OfflineQueue::with_capacity(self.offline_queue_capacity));
        let status_notifier = Arc::new(DedupedStatusNotifier::new(csms.clone()));
        let forwarder_queue = status_queue.clone();
        let forwarder_notifier = status_notifier.clone();
        let overflow_actor = self.runtime.actor();
        self.executor.spawn(Box::pin(async move {
            run_with_offline_queue(
                status_changes,
                &forwarder_queue,
                move |changed| {
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
                },
                move |_dropped| {
                    let actor = overflow_actor.clone();
                    async move { report_memory_exhaustion(&actor).await }
                },
            )
            .await;
        }));
        let flush_queue = status_queue.clone();
        let flush_notifier = status_notifier.clone();
        let flush: QueueFlush = Arc::new(move || {
            let queue = flush_queue.clone();
            let notifier = flush_notifier.clone();
            Box::pin(async move {
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
            })
        });
        self.queue_flushes.push(flush.clone());
        csms.register_reconnect_handler(move || {
            let flush = flush.clone();
            async move { flush().await }
        })
        .await;

        self
    }

    /// Like [`Self::status_notifications`], but the offline queue is durable: whatever's still
    /// queued when the process loses power is restored, **in order**, from `store` at the next
    /// boot - `docs/PRODUCTION-ROADMAP.md` §7.2/§7.4 (E2.8, E4.3). `store` may be built over
    /// [`crate::hardware::NoStorage`], in which case this behaves exactly like
    /// [`Self::status_notifications`].
    ///
    /// The dedup cache that [`DedupedStatusNotifier`] wraps `csms` in starts **empty** every
    /// boot, including this one - it is not itself persisted. That means the first status forwarded
    /// after a restart is always sent, even if it repeats whatever was last sent before the
    /// power was lost. That is deliberate: re-reporting a status the CSMS already has is
    /// harmless, while the alternative (persisting the dedup cache too, to suppress that resend)
    /// risks silently dropping a status the CSMS never actually received - the wrong failure mode
    /// for a durability feature to introduce.
    pub async fn status_notifications_persisted<N, S>(
        mut self,
        csms: &N,
        store: QueueStore<S>,
    ) -> Self
    where
        N: StatusNotifier + ReconnectHandler + Clone + Send + Sync + 'static,
        S: crate::hardware::Storage + Send + Sync + 'static,
    {
        let Some(status_changes) = self.take_status_changes() else {
            return self;
        };

        // `DropOldest` (the default), same reasoning as `Self::status_notifications`.
        let status_queue = Arc::new(OfflineQueue::with_capacity(self.offline_queue_capacity));
        // Restore before any live traffic is wired up - so an event that arrives during start-up
        // can never be delivered ahead of an older one the backlog restores.
        restore_status_notification_queue(&status_queue, &store).await;
        let store = Arc::new(store);
        let status_notifier = Arc::new(DedupedStatusNotifier::new(csms.clone()));
        let forwarder_queue = status_queue.clone();
        let forwarder_store = store.clone();
        let forwarder_notifier = status_notifier.clone();
        let overflow_actor = self.runtime.actor();
        self.executor.spawn(Box::pin(async move {
            run_persisted_status_notification_queue(
                status_changes,
                &forwarder_queue,
                &forwarder_store,
                move |changed| {
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
                },
                move |_dropped| {
                    let actor = overflow_actor.clone();
                    async move { report_memory_exhaustion(&actor).await }
                },
            )
            .await;
        }));
        let flush: QueueFlush = Arc::new(move || {
            let queue = status_queue.clone();
            let store = store.clone();
            let notifier = status_notifier.clone();
            Box::pin(async move {
                flush_and_persist_status_notification_queue(&queue, &store, move |changed| {
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
            })
        });
        self.queue_flushes.push(flush.clone());
        csms.register_reconnect_handler(move || {
            let flush = flush.clone();
            async move { flush().await }
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

        // `DropNewest`, unlike the status/security queues above: a queued `TransactionEvent`
        // carries a billable energy reading, and evicting the oldest to make room for a new one
        // would permanently lose that billing data. Rejecting the newest instead only delays
        // fresh transaction activity - it never disturbs what's already queued - see
        // `OverflowPolicy`'s docs.
        let transaction_queue = Arc::new(
            OfflineQueue::with_capacity(self.offline_queue_capacity)
                .with_overflow_policy(OverflowPolicy::DropNewest),
        );
        // Kept for `Self::get_transaction_status` - see the `transaction_queue` field's docs.
        self.transaction_queue = Some(transaction_queue.clone());
        let forwarder_queue = transaction_queue.clone();
        let forwarder_csms = csms.clone();
        let overflow_actor = self.runtime.actor();
        self.executor.spawn(Box::pin(async move {
            run_with_offline_queue(
                transaction_events,
                &forwarder_queue,
                move |occurred| {
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
                },
                move |_dropped| {
                    let actor = overflow_actor.clone();
                    async move { report_memory_exhaustion(&actor).await }
                },
            )
            .await;
        }));
        let reconnect_csms = csms.clone();
        let flush: QueueFlush = Arc::new(move || {
            let queue = transaction_queue.clone();
            let csms = reconnect_csms.clone();
            Box::pin(async move {
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
            })
        });
        self.queue_flushes.push(flush.clone());
        csms.register_reconnect_handler(move || {
            let flush = flush.clone();
            async move { flush().await }
        })
        .await;

        self
    }

    /// Like [`Self::transaction_events`], but the offline queue is durable: whatever's still
    /// queued when the process loses power is restored, **in order**, from `store` at the next
    /// boot - `docs/PRODUCTION-ROADMAP.md` §7.2/§7.4 (E2, E4.3). This is the queue ordering
    /// matters most for: the CSMS relies on `TransactionEvent`s arriving in sequence. `store` may
    /// be built over [`crate::hardware::NoStorage`], in which case this behaves exactly like
    /// [`Self::transaction_events`].
    pub async fn transaction_events_persisted<N, S>(
        mut self,
        csms: &N,
        store: QueueStore<S>,
    ) -> Self
    where
        N: TransactionNotifier + ReconnectHandler + Clone + Send + Sync + 'static,
        S: crate::hardware::Storage + Send + Sync + 'static,
    {
        let Some(transaction_events) = self.take_transaction_events() else {
            return self;
        };

        // `DropNewest`, same reasoning as `Self::transaction_events`.
        let transaction_queue = Arc::new(
            OfflineQueue::with_capacity(self.offline_queue_capacity)
                .with_overflow_policy(OverflowPolicy::DropNewest),
        );
        // Restore before any live traffic is wired up - so an event that arrives during start-up
        // can never be delivered ahead of an older one the backlog restores.
        restore_transaction_event_queue(&transaction_queue, &store).await;
        // Kept for `Self::get_transaction_status` - see the `transaction_queue` field's docs.
        self.transaction_queue = Some(transaction_queue.clone());
        let store = Arc::new(store);
        let forwarder_queue = transaction_queue.clone();
        let forwarder_store = store.clone();
        let forwarder_csms = csms.clone();
        let overflow_actor = self.runtime.actor();
        self.executor.spawn(Box::pin(async move {
            run_persisted_transaction_event_queue(
                transaction_events,
                &forwarder_queue,
                &forwarder_store,
                move |occurred| {
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
                },
                move |_dropped| {
                    let actor = overflow_actor.clone();
                    async move { report_memory_exhaustion(&actor).await }
                },
            )
            .await;
        }));
        let reconnect_csms = csms.clone();
        let flush: QueueFlush = Arc::new(move || {
            let queue = transaction_queue.clone();
            let store = store.clone();
            let csms = reconnect_csms.clone();
            Box::pin(async move {
                flush_and_persist_transaction_event_queue(&queue, &store, move |occurred| {
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
            })
        });
        self.queue_flushes.push(flush.clone());
        csms.register_reconnect_handler(move || {
            let flush = flush.clone();
            async move { flush().await }
        })
        .await;

        self
    }

    /// Registers the Authorization functional block: every presented-id-token authorization
    /// request is answered via Authorize, every answer the CSMS gives is remembered in the
    /// authorization cache, and a request that can't reach the CSMS falls back to the local
    /// authorization list and then that cache - see [`crate::authorization`] for the order and
    /// the device-model switches that gate it.
    ///
    /// `clock` stamps cache entries so `AuthCacheCtrlr`/`LifeTime` can expire them; an
    /// unsynchronized clock records no timestamp and the entry then never expires on age, rather
    /// than this crate inventing one.
    pub async fn authorization<N, K>(mut self, csms: &N, clock: K) -> Self
    where
        N: Authorizer + Clone + Send + Sync + 'static,
        K: crate::clock::Clock + Send + Sync + 'static,
    {
        let Some(authorization_requests) = self.take_authorization_requests() else {
            return self;
        };

        let authorizer = csms.clone();
        let actor = self.runtime.actor();
        self.executor.spawn(Box::pin(async move {
            run_authorization_requests(authorization_requests, &authorizer, actor, &clock).await;
        }));

        self
    }

    /// Registers durable network-connection-profile state (`docs/PRODUCTION-ROADMAP.md` §7.2,
    /// E2.11): recovers whatever configuration slots the CSMS had written before the charge point
    /// last lost power, then persists every subsequent change through `storage` for the life of
    /// the process.
    ///
    /// **Call this before [`Self::network_profiles`] and before
    /// [`Self::network_profile_switching`]** - the restore must land before either one can touch
    /// the store: `network_profiles` could otherwise let a live `SetNetworkProfile` race the
    /// restore (an inbound write landing in the empty store just before `replace` overwrites it
    /// with the stale snapshot), and `network_profile_switching` could select a profile before the
    /// operator's own choice is back in place. Registering all three in this order is all that
    /// takes; the restore completes before this method returns, exactly like
    /// [`Self::charging_profile_persistence`].
    ///
    /// Without it, a charge point moved onto a CSMS-written profile after [A9] comes back on the
    /// address its integrator compiled in rather than the one the operator moved it to - the whole
    /// point of storing profiles is defeated by the very event (a reboot) they most need to
    /// survive. `storage` may be [`crate::hardware::NoStorage`], in which case this behaves exactly
    /// as if it were never called.
    ///
    /// No age or reachability filtering happens at boot - see
    /// [`crate::persistence::restore_network_profiles`] for why neither applies here.
    ///
    /// [A9]: crate::network_switch
    pub async fn network_profile_persistence<S>(self, storage: S) -> Self
    where
        S: crate::hardware::Storage + Send + Sync + 'static,
    {
        let store = Arc::new(NetworkProfileSnapshotStore::new(storage));
        restore_network_profiles(&self.runtime.actor(), &store).await;

        let state_changes = self.runtime.actor().subscribe();
        self.executor.spawn(Box::pin(async move {
            run_network_profile_persistence(state_changes, &store).await;
        }));

        self
    }

    /// Registers inbound `SetNetworkProfile` handling (`docs/ROADMAP.md` §2,
    /// `docs/PRODUCTION-ROADMAP.md` B1.8): the CSMS writing a network connection profile into a
    /// configuration slot.
    ///
    /// **Storing a profile does not by itself switch the connection** - that is
    /// [`Self::network_profile_switching`], registered separately because it needs a transport
    /// this crate can re-point, which only [`crate::connect_and_setup`] builds.
    ///
    /// If [`Self::network_profile_persistence`] is used, register it before this method - see its
    /// docs for why.
    ///
    /// 2.x only - 1.6J has no such message.
    pub async fn network_profiles<N>(self, csms: &N) -> Self
    where
        N: crate::network_profile::SetNetworkProfileHandler + Send + Sync + 'static,
    {
        csms.register_set_network_profile_handler(self.runtime.actor())
            .await;
        self
    }

    /// Moves the live CSMS connection onto whichever stored network profile the priority order
    /// selects, rolling back if the new address does not work (A9). See
    /// [`crate::network_switch`] for the switch and rollback rules.
    ///
    /// Separate from [`Self::network_profiles`] - which stores what the CSMS wrote - because
    /// switching needs a [`ConnectionTarget`](crate::network_switch::ConnectionTarget) installed
    /// as the transport's reconnector *before* the first connection was made. A caller who built
    /// their own client has no such target and registers only `network_profiles`; their
    /// connection stays where they put it, which is the honest outcome rather than a switch that
    /// silently does nothing.
    #[cfg(feature = "websocket")]
    pub fn network_profile_switching<D, B>(
        self,
        target: &alloc::sync::Arc<crate::network_switch::ConnectionTarget>,
        closer: D,
        backoff: B,
    ) -> Self
    where
        D: crate::network_switch::ConnectionCloser + Send + Sync + 'static,
        B: Backoff + Send + Sync + 'static,
    {
        let actor = self.runtime.actor();
        let target = target.clone();
        // F5.2: from here on, a redial's `SizeLimitedStream` can raise `MemoryExhaustion` on this
        // charge point's own actor when it refuses an oversized frame.
        target.attach_security_reporting(actor.clone());
        self.executor.spawn(Box::pin(async move {
            crate::network_switch::run_network_profile_switching(
                &actor, &target, &closer, &backoff,
            )
            .await;
        }));
        self
    }

    /// Registers inbound `GetChargingProfiles` handling: the CSMS asking which charging profiles
    /// are installed, answered with one or more `ReportChargingProfiles`
    /// (`docs/PRODUCTION-ROADMAP.md` B2).
    ///
    /// Separate from [`Self::smart_charging`] because it is **2.x only** - 1.6J has no way to ask
    /// what is installed at all - and folding it into that method's bounds would make the block
    /// unregisterable on a 1.6J connection.
    ///
    /// Only present when the `smart-charging` Cargo feature is enabled (C4.2).
    #[cfg(feature = "smart-charging")]
    pub async fn charging_profile_reports<N>(self, csms: &N) -> Self
    where
        N: crate::smart_charging::GetChargingProfilesHandler + Send + Sync + 'static,
    {
        csms.register_get_charging_profiles_handler(self.runtime.actor())
            .await;
        self
    }

    /// Registers dynamic charging profiles (`docs/PRODUCTION-ROADMAP.md` B2.6, OCPP K28): inbound
    /// `UpdateDynamicSchedule` for limits the CSMS pushes, and a sweep that pulls one with
    /// `PullDynamicScheduleUpdate` for every installed dynamic profile whose own
    /// `dynUpdateInterval` has come round.
    ///
    /// `interval_secs` is how often *due-ness is checked*, not how often a pull happens - each
    /// profile carries its own interval, and a charge point with no dynamic profiles installed
    /// makes no requests at all.
    ///
    /// **2.1 only.** Dynamic charging profiles do not exist in 1.6J or 2.0.1, so this is separate
    /// from [`Self::smart_charging`] for the same reason [`Self::priority_charging`] is.
    ///
    /// Only present when the `smart-charging` Cargo feature is enabled (C4.2).
    #[cfg(feature = "smart-charging")]
    pub async fn dynamic_charging_profiles<N, K, B>(
        self,
        csms: &N,
        clock: K,
        backoff: B,
        interval_secs: u32,
    ) -> Self
    where
        N: crate::smart_charging::UpdateDynamicScheduleHandler
            + crate::smart_charging::DynamicSchedulePuller
            + Clone
            + Send
            + Sync
            + 'static,
        <N as crate::smart_charging::DynamicSchedulePuller>::Error: Send,
        K: crate::clock::Clock + Send + Sync + 'static,
        B: Backoff + Send + Sync + 'static,
    {
        csms.register_update_dynamic_schedule_handler(self.runtime.actor())
            .await;

        let actor = self.runtime.actor();
        let puller = csms.clone();
        self.executor.spawn(Box::pin(async move {
            crate::smart_charging::run_dynamic_schedule_pulls(
                &actor,
                &puller,
                &clock,
                &backoff,
                interval_secs,
            )
            .await;
        }));
        self
    }

    /// Registers priority charging (`docs/PRODUCTION-ROADMAP.md` B2.6): inbound
    /// `UsePriorityCharging`, and the outbound `NotifyPriorityCharging` loop reporting any grant
    /// the charge point makes on its own.
    ///
    /// Separate from [`Self::smart_charging`] because it is **2.1 only** - neither 1.6J nor 2.0.1
    /// has the messages or the profile purpose behind them - and folding it into that method's
    /// bounds would make the whole block unregisterable on the older versions.
    ///
    /// Only present when the `smart-charging` Cargo feature is enabled (C4.2).
    #[cfg(feature = "smart-charging")]
    pub async fn priority_charging<N>(self, csms: &N) -> Self
    where
        N: crate::smart_charging::UsePriorityChargingHandler
            + crate::smart_charging::PriorityChargingNotifier
            + Clone
            + Send
            + Sync
            + 'static,
    {
        csms.register_use_priority_charging_handler(self.runtime.actor())
            .await;

        let changes = self.runtime.actor().subscribe_priority_charging_changes();
        let notifier = csms.clone();
        self.executor.spawn(Box::pin(async move {
            crate::smart_charging::run_priority_charging_notifications(changes, &notifier).await;
        }));
        self
    }

    /// Registers inbound `ClearCache` handling (`docs/ROADMAP.md` §3,
    /// `docs/PRODUCTION-ROADMAP.md` B1.2): a CSMS emptying the authorization cache.
    ///
    /// Separate from [`Self::authorization`] because it is an inbound handler rather than an
    /// outbound loop, and a CSMS client may implement one without the other.
    pub async fn clear_cache<N>(self, csms: &N) -> Self
    where
        N: crate::authorization::ClearCacheHandler + Send + Sync + 'static,
    {
        csms.register_clear_cache_handler(self.runtime.actor())
            .await;
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

        let security_queue = Arc::new(OfflineQueue::with_capacity(self.offline_queue_capacity));
        let forwarder_queue = security_queue.clone();
        let forwarder_csms = csms.clone();
        self.executor.spawn(Box::pin(async move {
            run_with_offline_queue_where(
                security_events,
                &forwarder_queue,
                // A04.FR.01: only *critical* events are reported to the CSMS. The non-critical
                // ones still reach the security log (A04.FR.04), which subscribes separately -
                // see `SecurityEventType::is_critical` for why sharing this bounded queue with
                // them is a security problem rather than a tidiness one.
                |event: &SecurityEvent| event.event_type.is_critical(),
                move |event| {
                    let notifier = forwarder_csms.clone();
                    async move {
                        notifier
                            .notify_security_event(&event.event_type, event.tech_info.as_deref())
                            .await
                    }
                },
                // Deliberately does NOT call `report_memory_exhaustion` here, unlike the
                // status/transaction queues above: that would raise a `SecurityEventOccurred`
                // which is itself broadcast to this same forwarding loop and pushed into this
                // same queue, and if the queue is already full that overflows again - an
                // unbounded feedback loop. A dropped security event (this queue's overflow) is
                // logged instead, once, without re-entering the reporting pipeline.
                move |dropped: SecurityEvent| async move {
                    tracing::error!(
                        event_type = ?dropped.event_type,
                        "offline security-event queue overflowed; dropping the oldest queued \
                         security event notification rather than risk an unbounded feedback loop \
                         by reporting MemoryExhaustion through this same queue"
                    );
                },
            )
            .await;
        }));
        let reconnect_csms = csms.clone();
        let flush: QueueFlush = Arc::new(move || {
            let queue = security_queue.clone();
            let csms = reconnect_csms.clone();
            Box::pin(async move {
                crate::offline_queue::flush_offline_queue(&queue, move |event| {
                    let notifier = csms.clone();
                    async move {
                        notifier
                            .notify_security_event(&event.event_type, event.tech_info.as_deref())
                            .await
                    }
                })
                .await;
            })
        });
        self.queue_flushes.push(flush.clone());
        csms.register_reconnect_handler(move || {
            let flush = flush.clone();
            async move { flush().await }
        })
        .await;

        self
    }

    /// Like [`Self::security_events`], but the offline queue is durable: whatever's still queued
    /// when the process loses power is restored, **in order**, from `store` at the next boot -
    /// `docs/PRODUCTION-ROADMAP.md` §7.2/§7.4 (E2.8, E4.3). `store` may be built over
    /// [`crate::hardware::NoStorage`], in which case this behaves exactly like
    /// [`Self::security_events`].
    pub async fn security_events_persisted<N, S>(mut self, csms: &N, store: QueueStore<S>) -> Self
    where
        N: SecurityEventNotifier + ReconnectHandler + Clone + Send + Sync + 'static,
        S: crate::hardware::Storage + Send + Sync + 'static,
    {
        let Some(security_events) = self.take_security_events() else {
            return self;
        };

        // `DropOldest` (the default), same reasoning as `Self::security_events`.
        let security_queue = Arc::new(OfflineQueue::with_capacity(self.offline_queue_capacity));
        // Restore before any live traffic is wired up - so an event that arrives during start-up
        // can never be delivered ahead of an older one the backlog restores.
        restore_security_event_queue(&security_queue, &store).await;
        let store = Arc::new(store);
        let forwarder_queue = security_queue.clone();
        let forwarder_store = store.clone();
        let forwarder_csms = csms.clone();
        self.executor.spawn(Box::pin(async move {
            run_persisted_security_event_queue(
                security_events,
                &forwarder_queue,
                &forwarder_store,
                move |event| {
                    let notifier = forwarder_csms.clone();
                    async move {
                        notifier
                            .notify_security_event(&event.event_type, event.tech_info.as_deref())
                            .await
                    }
                },
                // Deliberately does NOT call `report_memory_exhaustion` here, same reason as
                // `Self::security_events` - see the comment at that call site.
                move |dropped: SecurityEvent| async move {
                    tracing::error!(
                        event_type = ?dropped.event_type,
                        "offline security-event queue overflowed; dropping the oldest queued \
                         security event notification rather than risk an unbounded feedback loop \
                         by reporting MemoryExhaustion through this same queue"
                    );
                },
            )
            .await;
        }));
        let reconnect_csms = csms.clone();
        let flush: QueueFlush = Arc::new(move || {
            let queue = security_queue.clone();
            let store = store.clone();
            let csms = reconnect_csms.clone();
            Box::pin(async move {
                flush_and_persist_security_event_queue(&queue, &store, move |event| {
                    let notifier = csms.clone();
                    async move {
                        notifier
                            .notify_security_event(&event.event_type, event.tech_info.as_deref())
                            .await
                    }
                })
                .await;
            })
        });
        self.queue_flushes.push(flush.clone());
        csms.register_reconnect_handler(move || {
            let flush = flush.clone();
            async move { flush().await }
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

    /// Registers `ReservationStatusUpdate` reporting and the expiry sweep that produces most of
    /// it (`docs/PRODUCTION-ROADMAP.md` B8.1).
    ///
    /// Two things, because neither is useful alone: a reservation that expires with nobody
    /// telling the CSMS is a silent divergence, and a report loop with no expiry to report has
    /// almost nothing to say. `interval_secs` is how often expiry is checked - a minute is ample,
    /// since reservation windows are quarter-hours.
    ///
    /// **2.x only** - 1.6J has no `ReservationStatusUpdate`, so it is registered separately from
    /// [`Self::reservation`] rather than folded into its bounds.
    #[cfg(feature = "reservation")]
    pub fn reservation_status_updates<N, K, B>(
        self,
        csms: &N,
        clock: K,
        backoff: B,
        interval_secs: u32,
    ) -> Self
    where
        N: crate::reservation::ReservationStatusNotifier + Clone + Send + Sync + 'static,
        K: crate::clock::Clock + Send + Sync + 'static,
        B: Backoff + Send + Sync + 'static,
    {
        let updates = self.runtime.actor().subscribe_reservation_updates();
        let notifier = csms.clone();
        self.executor.spawn(Box::pin(async move {
            crate::reservation::run_reservation_status_updates(updates, &notifier).await;
        }));

        let actor = self.runtime.actor();
        self.executor.spawn(Box::pin(async move {
            crate::reservation::run_reservation_expiry(&actor, &clock, &backoff, interval_secs)
                .await;
        }));
        self
    }

    /// Registers outbound smart-charging notifications - `NotifyChargingLimit`/
    /// `ClearedChargingLimit`/`NotifyEVChargingNeeds`/`NotifyEVChargingSchedule`
    /// (`docs/PRODUCTION-ROADMAP.md` B2.8). **2.0.1/2.1 only** - 1.6J has none of these messages,
    /// so `csms` must implement [`crate::smart_charging::ChargingLimitNotifier`] and
    /// [`crate::smart_charging::EVChargingNotifier`], which no 1.6J client type does.
    ///
    /// Nothing here decides *when* to notify - see
    /// [`crate::state::ChargePointEvent::ExternalChargingLimitSet`]/
    /// [`crate::state::EvseEvent::EVChargingNeedsReported`] and friends for how an integrator (a
    /// local energy-management binding, or an ISO 15118 stack) feeds this.
    ///
    /// Only present when the `smart-charging` Cargo feature is enabled (C4.2).
    #[cfg(feature = "smart-charging")]
    pub fn smart_charging_notifications<N>(self, csms: &N) -> Self
    where
        N: crate::smart_charging::ChargingLimitNotifier
            + crate::smart_charging::EVChargingNotifier
            + Clone
            + Send
            + Sync
            + 'static,
    {
        let notifications = self
            .runtime
            .actor()
            .subscribe_smart_charging_notifications();
        let notifier = csms.clone();
        self.executor.spawn(Box::pin(async move {
            crate::smart_charging::run_smart_charging_notifications(notifications, &notifier).await;
        }));
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
        csms.register_get_base_report_handler(self.runtime.actor())
            .await;
        csms.register_get_report_handler(self.runtime.actor()).await;
        self.configuration(csms).await
    }

    /// Registers the *reading and writing* half of the device model - `GetVariables`/
    /// `SetVariables` on 2.x, or the `GetConfiguration`/`ChangeConfiguration` those project onto
    /// under 1.6J - without the 2.x-only reporting half ([`Self::device_model`] registers both).
    ///
    /// Exists because 1.6J has no `GetBaseReport`/`GetReport` at all: its flat `GetConfiguration`
    /// already returns everything a report would, so bundling the four handlers together would
    /// make a 1.6J connection unable to register any of them.
    pub async fn configuration<N>(self, csms: &N) -> Self
    where
        N: GetVariablesHandler + SetVariablesHandler + Send + Sync + 'static,
    {
        csms.register_get_variables_handler(self.runtime.actor())
            .await;
        csms.register_set_variables_handler(self.runtime.actor())
            .await;

        self
    }

    /// Registers the variable monitoring engine's inbound surface: `SetVariableMonitoring`/
    /// `ClearVariableMonitoring` handlers both feed into the runtime's actor
    /// (`docs/PRODUCTION-ROADMAP.md` §B5, B5.2).
    ///
    /// **2.x only** - 1.6J has no such messages, so this method (like [`Self::configuration`]'s
    /// 2.x-specific siblings) is meaningless to call against a 1.6J-only connection; `ocpp-client`
    /// simply never invokes a handler a 1.6J CSMS has no message to trigger. Pair with
    /// [`Self::variable_monitor_events`] to also report triggered monitors outbound - registering
    /// only this half lets a CSMS install/clear monitors that never fire anything back.
    ///
    /// Only present when the `variable-monitoring` Cargo feature is enabled (C4.2).
    #[cfg(feature = "variable-monitoring")]
    pub async fn variable_monitoring<N>(self, csms: &N) -> Self
    where
        N: SetVariableMonitoringHandler + ClearVariableMonitoringHandler + Send + Sync + 'static,
    {
        csms.register_set_variable_monitoring_handler(self.runtime.actor())
            .await;
        csms.register_clear_variable_monitoring_handler(self.runtime.actor())
            .await;

        self
    }

    /// Registers the variable monitoring engine's *reporting* surface: `SetMonitoringBase`,
    /// `SetMonitoringLevel`, and `GetMonitoringReport` (answered via one or more
    /// `NotifyMonitoringReport`s) all feed into the runtime's actor
    /// (`docs/PRODUCTION-ROADMAP.md` §B5, B5.3) - the read/bulk-control counterpart to
    /// [`Self::variable_monitoring`]'s install/clear surface, kept as a separate call so a CSMS
    /// client implementing only one half doesn't need the other (mirrors [`Self::device_model`]/
    /// [`Self::configuration`]'s same split).
    ///
    /// **2.x only** - see [`Self::variable_monitoring`]'s docs; 1.6J has no such messages.
    ///
    /// Only present when the `variable-monitoring` Cargo feature is enabled (C4.2).
    #[cfg(feature = "variable-monitoring")]
    pub async fn monitoring_reports<N>(self, csms: &N) -> Self
    where
        N: SetMonitoringBaseHandler
            + SetMonitoringLevelHandler
            + GetMonitoringReportHandler
            + Send
            + Sync
            + 'static,
    {
        csms.register_set_monitoring_base_handler(self.runtime.actor())
            .await;
        csms.register_set_monitoring_level_handler(self.runtime.actor())
            .await;
        csms.register_get_monitoring_report_handler(self.runtime.actor())
            .await;

        self
    }

    /// Registers the inbound half of the Tariff and Cost functional block's cost reporting: the
    /// `CostUpdated` handler feeds into the runtime's actor. See [`Self::tariffs`] for the tariff
    /// store/assignment messages (`docs/PRODUCTION-ROADMAP.md` B7.1) - kept as a separate call
    /// so a CSMS client implementing only one half doesn't need the other.
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

    /// Registers the Tariff and Cost functional block's tariff store and per-transaction
    /// assignment: `SetDefaultTariff`, `ChangeTransactionTariff`, `ClearTariffs` and `GetTariffs`
    /// all feed into the runtime's actor (`docs/PRODUCTION-ROADMAP.md` B7.1). **2.1 only** - see
    /// [`crate::tariff`]'s docs; `ocpp-client`'s 1.6J/2.0.1 clients don't implement these traits
    /// at all, since neither version has tariff messages.
    ///
    /// Only present when the `tariff-cost` Cargo feature is enabled - see [`Self::reservation`]'s
    /// doc comment for why the method itself disappears rather than becoming a no-op.
    #[cfg(feature = "tariff-cost")]
    pub async fn tariffs<N>(self, csms: &N) -> Self
    where
        N: SetDefaultTariffHandler
            + ChangeTransactionTariffHandler
            + ClearTariffsHandler
            + GetTariffsHandler
            + Send
            + Sync
            + 'static,
    {
        csms.register_set_default_tariff_handler(self.runtime.actor())
            .await;
        csms.register_change_transaction_tariff_handler(self.runtime.actor())
            .await;
        csms.register_clear_tariffs_handler(self.runtime.actor())
            .await;
        csms.register_get_tariffs_handler(self.runtime.actor())
            .await;

        self
    }

    /// Registers the Periodic Event Streams functional block: `OpenPeriodicEventStream`/
    /// `ClosePeriodicEventStream`/`AdjustPeriodicEventStream`/`GetPeriodicEventStream` all feed
    /// into the runtime's actor, and the sweep that drives outbound `NotifyPeriodicEventStream`
    /// (`docs/PRODUCTION-ROADMAP.md` B5.6) is spawned via `executor` - see
    /// [`crate::periodic_event_stream::run_periodic_event_streams`]. **2.1 only** - see
    /// [`crate::periodic_event_stream`]'s docs; `ocpp-client`'s 1.6J/2.0.1 clients don't implement
    /// these traits at all, since neither version has periodic event stream messages.
    ///
    /// `sweep_interval_secs` is how often open streams are checked for being due - a few seconds
    /// is ample against streams configured in the tens of seconds and up, mirroring
    /// [`Self::variable_monitor_events`]'s own sweep.
    ///
    /// Only present when the `periodic-event-stream` Cargo feature is enabled - see
    /// [`Self::reservation`]'s doc comment for why the method itself disappears rather than
    /// becoming a no-op.
    #[cfg(feature = "periodic-event-stream")]
    pub async fn periodic_event_streams<N, C, B>(
        self,
        csms: &N,
        clock: C,
        backoff: B,
        sweep_interval_secs: u32,
    ) -> Self
    where
        N: OpenPeriodicEventStreamHandler
            + ClosePeriodicEventStreamHandler
            + AdjustPeriodicEventStreamHandler
            + GetPeriodicEventStreamHandler
            + PeriodicEventStreamNotifier
            + Clone
            + Send
            + Sync
            + 'static,
        C: crate::clock::Clock + Send + Sync + 'static,
        B: Backoff + Send + Sync + 'static,
    {
        csms.register_open_periodic_event_stream_handler(self.runtime.actor())
            .await;
        csms.register_close_periodic_event_stream_handler(self.runtime.actor())
            .await;
        csms.register_adjust_periodic_event_stream_handler(self.runtime.actor())
            .await;
        csms.register_get_periodic_event_stream_handler(self.runtime.actor())
            .await;

        let actor = self.runtime.actor();
        let notifier = csms.clone();
        self.executor.spawn(Box::pin(async move {
            run_periodic_event_streams(&actor, &notifier, &clock, &backoff, sweep_interval_secs)
                .await;
        }));

        self
    }

    /// Registers the DER Control functional block: `GetDERControl`, `SetDERControl`,
    /// `ClearDERControl`, `AFRRSignal` and `NotifyAllowedEnergyTransfer` all feed into the
    /// runtime's actor (`docs/PRODUCTION-ROADMAP.md` B8.2); `ReportDERControl` is sent inline by
    /// the `GetDERControl` handler. **2.1 only** - see [`crate::der_control`]'s docs; neither
    /// 1.6J nor 2.0.1 has any of these messages.
    ///
    /// Unlike [`Self::tariffs`]/[`Self::reservation`], this is **not** part of
    /// [`crate::setup::setup`] today - `setup()` bounds its CSMS type by every block it wires at
    /// once, and extending that bound for a block this new is a larger, riskier change than this
    /// task's scope; call this explicitly alongside `setup()`/the rest of this builder until a
    /// future change folds it in. [`crate::hardware::CAPABILITY_GATES`]'s
    /// `der_control` row therefore records `has_handler: false` even though this method's
    /// handlers are real - see that row's docs.
    ///
    /// Only present when the `der-control` Cargo feature is enabled - see [`Self::reservation`]'s
    /// doc comment for why the method itself disappears rather than becoming a no-op.
    #[cfg(feature = "der-control")]
    pub async fn der_control<N>(self, csms: &N) -> Self
    where
        N: SetDERControlHandler
            + ClearDERControlHandler
            + GetDERControlHandler
            + AfrrSignalHandler
            + NotifyAllowedEnergyTransferHandler
            + Send
            + Sync
            + 'static,
    {
        csms.register_set_der_control_handler(self.runtime.actor())
            .await;
        csms.register_clear_der_control_handler(self.runtime.actor())
            .await;
        csms.register_get_der_control_handler(self.runtime.actor())
            .await;
        csms.register_afrr_signal_handler(self.runtime.actor())
            .await;
        csms.register_notify_allowed_energy_transfer_handler(self.runtime.actor())
            .await;

        self
    }

    /// Registers durable transaction state (`docs/PRODUCTION-ROADMAP.md` §7, workstream E):
    /// recovers whatever was in flight when the charge point last lost power, then persists every
    /// subsequent transaction event through `storage` for the life of the process.
    ///
    /// Register this **first**, before [`Self::transaction_events`]: recovery closes each
    /// interrupted transaction out as a `TransactionEvent(Ended)`, and while that event is
    /// buffered either way (the CSMS-facing subscription is taken up front in [`Self::start`]),
    /// registering persistence first also guarantees the restored transaction-id counter lands
    /// before any new transaction can consume an id.
    ///
    /// `storage` may be [`crate::hardware::NoStorage`], in which case this registers a
    /// persistence task that durably stores nothing and a recovery that finds nothing - the
    /// charge point runs exactly as it would without this call. Anything else needs the hardware
    /// binding to declare [`Capabilities::has_persistent_storage`]; callers driving this builder
    /// from declared capabilities (e.g. [`crate::setup::setup`]) should skip this call when it is
    /// `false`.
    ///
    /// `clock` stamps each record's start time - [`crate::clock::SystemClock`] on std, an
    /// RTC-backed [`crate::clock::Clock`] on embedded.
    pub async fn transaction_persistence<S, K>(mut self, storage: S, clock: K) -> Self
    where
        S: crate::hardware::Storage + Send + Sync + 'static,
        K: crate::clock::Clock + Send + Sync + 'static,
    {
        let Some(events) = Self::warn_if_taken(
            self.transaction_persistence_events.take(),
            "transaction_persistence",
        ) else {
            return self;
        };

        let store = Arc::new(TransactionStore::new(storage));
        // Before spawning the writer: recovery must not race a live write against the same keys.
        restore_transactions(&self.runtime.actor(), &store).await;

        self.executor.spawn(Box::pin(async move {
            run_transaction_persistence(events, &store, &clock).await;
        }));

        self
    }

    /// Registers durable local authorization list state (`docs/PRODUCTION-ROADMAP.md` §7.2,
    /// E2.4): recovers whatever list `SendLocalList` last installed before the charge point last
    /// lost power, then persists every subsequent change through `storage` for the life of the
    /// process.
    ///
    /// Independent of every other `*_persistence` method - a charge point may enable this alone,
    /// none of them, or any combination, and each behaves correctly on its own. `storage` may be
    /// [`crate::hardware::NoStorage`], in which case this behaves exactly as if it were never
    /// called.
    ///
    /// Register this before [`Self::local_authorization_list`] is reachable by a CSMS request -
    /// i.e. before that block's `SendLocalList`/`GetLocalListVersion` handlers are registered, or
    /// before the connection that would deliver such a request is established - so a request
    /// can't race the restore. In practice this just means calling this method during the same
    /// build chain, before `.build()`.
    ///
    /// Only present when the `local-auth-list` Cargo feature is enabled (C4.2).
    #[cfg(feature = "local-auth-list")]
    pub async fn local_authorization_list_persistence<S>(self, storage: S) -> Self
    where
        S: crate::hardware::Storage + Send + Sync + 'static,
    {
        let store = Arc::new(LocalAuthorizationListStore::new(storage));
        restore_local_authorization_list(&self.runtime.actor(), &store).await;

        let state_changes = self.runtime.actor().subscribe();
        self.executor.spawn(Box::pin(async move {
            run_local_authorization_list_persistence(state_changes, &store).await;
        }));

        self
    }

    /// Registers durable reservation state (`docs/PRODUCTION-ROADMAP.md` §7.2, E2.6): recovers
    /// whatever reservations were active before the charge point last lost power - dropping any
    /// whose expiry passed while it was off, see [`crate::persistence::restore_reservations`]'s
    /// docs for that decision - then persists every subsequent reservation change through
    /// `storage` for the life of the process.
    ///
    /// Independent of every other `*_persistence` method, same as
    /// [`Self::local_authorization_list_persistence`]. `storage` may be
    /// [`crate::hardware::NoStorage`].
    ///
    /// `clock` decides whether a persisted reservation's `expires_at` is trustworthy to compare
    /// against - [`crate::clock::SystemClock`] on std, an RTC-backed
    /// [`crate::clock::Clock`] on embedded.
    ///
    /// Register this before [`Self::reservation`] is reachable by a CSMS request, for the same
    /// race-avoidance reason as [`Self::local_authorization_list_persistence`].
    ///
    /// Only present when the `reservation` Cargo feature is enabled (C4.2).
    #[cfg(feature = "reservation")]
    pub async fn reservation_persistence<S, K>(self, storage: S, clock: K) -> Self
    where
        S: crate::hardware::Storage + Send + Sync + 'static,
        K: crate::clock::Clock + Send + Sync + 'static,
    {
        let store = Arc::new(ReservationStore::new(storage));
        restore_reservations(&self.runtime.actor(), &store, &clock).await;

        let state_changes = self.runtime.actor().subscribe();
        self.executor.spawn(Box::pin(async move {
            run_reservation_persistence(state_changes, &store).await;
        }));

        self
    }

    /// Registers durable device model attribute state (`docs/PRODUCTION-ROADMAP.md` §7.2, E2.3):
    /// recovers whatever `persistent`-flagged attribute values survived from before the charge
    /// point last lost power, then persists every subsequent change through `storage` for the
    /// life of the process.
    ///
    /// Independent of every other `*_persistence` method, same as
    /// [`Self::local_authorization_list_persistence`]. `storage` may be
    /// [`crate::hardware::NoStorage`].
    ///
    /// **Ordering**: call this *after* [`Self::start`] (which is unavoidable - there is no
    /// `ChargePointBuilder` to call this on beforehand) - `Self::start` already waits for the
    /// hardware binding's own `ChargePoint::start` to finish registering its variables before
    /// returning, which is exactly the ordering [`crate::persistence::restore_device_model`]'s
    /// docs require: a persisted value only lands on a variable the binding has already declared
    /// exists this boot. Call this before [`Self::device_model`] is reachable by a CSMS
    /// `SetVariables`/`GetVariables` request, for the same race-avoidance reason as
    /// [`Self::local_authorization_list_persistence`].
    pub async fn device_model_persistence<S>(self, storage: S) -> Self
    where
        S: crate::hardware::Storage + Send + Sync + 'static,
    {
        let store = Arc::new(DeviceModelStore::new(storage));
        restore_device_model(&self.runtime.actor(), &store).await;

        let state_changes = self.runtime.actor().subscribe();
        self.executor.spawn(Box::pin(async move {
            run_device_model_persistence(state_changes, &store).await;
        }));

        self
    }

    /// Registers the durable security log (`docs/PRODUCTION-ROADMAP.md` §7.2, E2.10): recovers
    /// whatever security events were logged before the charge point last lost power, then records
    /// and persists every subsequent one through `store` for the life of the process.
    ///
    /// `log` is the caller's handle onto the live log - hold on to it (it is cheap to clone behind
    /// the [`Arc`] this takes) to read the history back, and to clear it via
    /// [`crate::persistence::clear_security_log`]. It also fixes the bound on how much the log
    /// retains; see [`crate::security::SecurityEventLog::with_capacity`].
    ///
    /// Independent of [`Self::security_events`]/[`Self::security_events_persisted`] and of every
    /// other `*_persistence` method: the log records every event whether or not it is ever
    /// delivered to a CSMS, so a charge point may register this alone, with either security-event
    /// forwarder, or with none. `store` may be built over [`crate::hardware::NoStorage`], leaving
    /// an in-RAM-only log that starts empty each boot.
    ///
    /// `clock` stamps each entry - [`crate::clock::SystemClock`] on std, an RTC-backed
    /// [`crate::clock::Clock`] on embedded; hardware with no usable time source records the events
    /// with no timestamp rather than a fabricated one (see
    /// [`crate::security::SecurityLogEntry::recorded_at`]).
    ///
    /// Registering this a second time is a no-op (logged), like the other subscription-consuming
    /// blocks - see the type's docs.
    pub async fn security_log_persisted<S, K>(
        mut self,
        log: Arc<crate::security::SecurityEventLog>,
        store: crate::persistence::SecurityLogStore<S>,
        clock: K,
    ) -> Self
    where
        S: crate::hardware::Storage + Send + Sync + 'static,
        K: crate::clock::Clock + Send + Sync + 'static,
    {
        let Some(events) =
            Self::warn_if_taken(self.security_log_events.take(), "security_log_persisted")
        else {
            return self;
        };

        // Before spawning the writer: a live event must not be recorded ahead of the history it
        // follows, nor race a write against the same key.
        restore_security_log(&log, &store).await;

        self.executor.spawn(Box::pin(async move {
            run_security_log_persistence(events, &log, &store, &clock).await;
        }));

        self
    }

    /// Registers the Diagnostics block's log upload (`docs/PRODUCTION-ROADMAP.md` B5.1): inbound
    /// `GetLog` (2.x) / `GetDiagnostics` (1.6J), and the worker that performs each accepted upload
    /// through `transfer` while reporting its progress to `csms`.
    ///
    /// `log` is the same [`SecurityEventLog`](crate::security::SecurityEventLog) handle
    /// [`Self::security_log_persisted`] restores into, so a `GetLog` for the security log uploads
    /// the history that survived the last reboot rather than only what has happened since.
    ///
    /// The upload runs on its own task because OCPP's sequence requires the response to go out
    /// before the transfer starts - see [`crate::diagnostics`].
    ///
    /// Only present when the `diagnostics` Cargo feature is enabled (C4.2).
    #[cfg(feature = "diagnostics")]
    pub async fn log_uploads<N, F, B>(
        self,
        csms: &N,
        transfer: F,
        log: Arc<crate::security::SecurityEventLog>,
        backoff: B,
    ) -> Self
    where
        N: crate::diagnostics::GetLogHandler
            + crate::diagnostics::LogStatusNotifier
            + Clone
            + Send
            + Sync
            + 'static,
        F: crate::hardware::FileTransfer + Send + Sync + 'static,
        B: Backoff + Send + Sync + 'static,
    {
        let uploads = crate::diagnostics::LogUploadQueue::new();
        let state = Arc::new(crate::diagnostics::LogUploadState::new());
        csms.register_get_log_handler(self.runtime.actor(), uploads.clone(), state.clone())
            .await;

        let notifier = csms.clone();
        self.executor.spawn(Box::pin(async move {
            crate::diagnostics::run_log_uploads(
                uploads, &state, &transfer, &notifier, &log, &backoff,
            )
            .await;
        }));
        self
    }

    /// Registers the Firmware Management block (`docs/PRODUCTION-ROADMAP.md` B3.2/B3.3): inbound
    /// `UpdateFirmware` (and, for a 1.6J `csms`, the Security Whitepaper's `SignedUpdateFirmware`,
    /// see [`crate::firmware::SignedUpdateFirmwareHandler`]), and the worker that downloads,
    /// verifies a signed image's signature, waits, installs and reports every state change to
    /// `csms`.
    ///
    /// Needs both halves of the firmware hardware surface - `transfer` to fetch the image,
    /// `installer` to flash it - plus `verifier` to check a signed update's image before it is
    /// installed (B3.3; [`crate::hardware::NoFirmwareVerifier`] if this charge point never
    /// receives signed updates) - which is why, like [`Self::log_uploads`], this is builder-only:
    /// `setup()` has no way to receive any of them.
    ///
    /// Only present when the `firmware-management` Cargo feature is enabled (C4.2).
    #[cfg(feature = "firmware-management")]
    pub async fn firmware_updates<N, F, I, K, B, V>(
        self,
        csms: &N,
        transfer: F,
        installer: I,
        clock: K,
        backoff: B,
        verifier: V,
    ) -> Self
    where
        N: crate::firmware::UpdateFirmwareHandler
            + crate::firmware::SignedUpdateFirmwareHandler
            + crate::firmware::FirmwareStatusNotifier
            + Clone
            + Send
            + Sync
            + 'static,
        F: crate::hardware::FileTransfer + Send + Sync + 'static,
        I: crate::hardware::FirmwareInstaller + Send + Sync + 'static,
        K: crate::clock::Clock + Send + Sync + 'static,
        B: Backoff + Send + Sync + 'static,
        V: crate::hardware::FirmwareVerifier + Send + Sync + 'static,
    {
        let updates = crate::firmware::FirmwareUpdateQueue::new();
        let state = Arc::new(crate::firmware::FirmwareUpdateState::new());
        csms.register_update_firmware_handler(self.runtime.actor(), updates.clone(), state.clone())
            .await;
        csms.register_signed_update_firmware_handler(
            self.runtime.actor(),
            updates.clone(),
            state.clone(),
        )
        .await;

        let actor = self.runtime.actor();
        let notifier = csms.clone();
        self.executor.spawn(Box::pin(async move {
            crate::firmware::run_firmware_updates(
                &actor, updates, &state, &transfer, &installer, &notifier, &clock, &backoff,
                &verifier,
            )
            .await;
        }));
        self
    }

    /// Registers the local-controller Firmware Publishing block
    /// (`docs/PRODUCTION-ROADMAP.md` B3.4): inbound `PublishFirmware`/`UnpublishFirmware`, and the
    /// worker that downloads and publishes every accepted request, reporting every state change
    /// to `csms`. **2.x only** - 1.6J has no local-controller concept.
    ///
    /// Needs `transfer` to fetch the image (the same `hardware::FileTransfer` as
    /// [`Self::firmware_updates`]) and `publisher` to serve it on the local network, which is why,
    /// like [`Self::firmware_updates`], this is builder-only: `setup()` has no way to receive
    /// either. Independent of [`Self::firmware_updates`] - a charge point can publish firmware for
    /// others without ever calling it on itself, and vice versa.
    ///
    /// Only present when the `firmware-publishing` Cargo feature is enabled (C4.2).
    #[cfg(feature = "firmware-publishing")]
    pub async fn publish_firmware<N, R, F, B>(
        self,
        csms: &N,
        transfer: R,
        publisher: F,
        backoff: B,
    ) -> Self
    where
        N: crate::publish_firmware::PublishFirmwareHandler
            + crate::publish_firmware::PublishFirmwareStatusNotifier
            + Clone
            + Send
            + Sync
            + 'static,
        R: crate::hardware::FileTransfer + Send + Sync + 'static,
        F: crate::hardware::FirmwarePublisher + Send + Sync + 'static,
        B: Backoff + Send + Sync + 'static,
    {
        let publishes = crate::publish_firmware::PublishFirmwareQueue::new();
        let state = Arc::new(crate::publish_firmware::PublishFirmwareState::new());
        let publisher = Arc::new(publisher);
        csms.register_publish_firmware_handlers(
            self.runtime.actor(),
            publishes.clone(),
            publisher.clone(),
            state.clone(),
        )
        .await;

        let notifier = csms.clone();
        self.executor.spawn(Box::pin(async move {
            crate::publish_firmware::run_publish_firmware(
                publishes, &state, &transfer, &publisher, &notifier, &backoff,
            )
            .await;
        }));
        self
    }

    /// Registers certificate management (`docs/PRODUCTION-ROADMAP.md` B4.2): `InstallCertificate`,
    /// `DeleteCertificate` and `GetInstalledCertificateIds`, all answered from `store`.
    ///
    /// **2.x only** - 1.6J's certificate messages live in the Security Whitepaper, which
    /// `ocpp-types` does not generate. Builder-only for the same reason
    /// [`Self::log_uploads`] is: it needs a [`crate::hardware::CertificateStore`], which
    /// `setup()`'s signature cannot receive.
    ///
    /// Only present when the `certificate-management` Cargo feature is enabled (C4.2).
    #[cfg(feature = "certificate-management")]
    pub async fn certificates<N, S>(self, csms: &N, store: S) -> Self
    where
        N: crate::certificates::CertificateHandler + Send + Sync + 'static,
        S: crate::hardware::CertificateStore + Send + Sync + 'static,
    {
        csms.register_certificate_handlers(self.runtime.actor(), store)
            .await;
        self
    }

    /// Registers `GetCertificateStatus` (`docs/PRODUCTION-ROADMAP.md` B4.4): the CSMS asks this
    /// charge point to check a certificate's OCSP status via `checker`.
    ///
    /// **2.x only.** Builder-only for the same reason `Self::certificates` (only present with the certificate-management feature) is: it needs a
    /// [`crate::hardware::OcspChecker`], which `setup()`'s signature cannot receive. Separate
    /// from `Self::certificates` (only present with the certificate-management feature) because the two need genuinely different hardware capabilities
    /// - see [`crate::certificate_status`]'s module docs.
    ///
    /// Only present when the `ocsp-checking` Cargo feature is enabled (C4.2).
    #[cfg(feature = "ocsp-checking")]
    pub async fn ocsp_status<N, O>(self, csms: &N, checker: O) -> Self
    where
        N: crate::certificate_status::GetCertificateStatusHandler + Send + Sync + 'static,
        O: crate::hardware::OcspChecker + Send + Sync + 'static,
    {
        csms.register_get_certificate_status_handler(self.runtime.actor(), checker)
            .await;
        self
    }

    /// Registers `GetCertificateChainStatus` (`docs/PRODUCTION-ROADMAP.md` B4.4): the chain-wide
    /// equivalent of [`Self::ocsp_status`].
    ///
    /// **2.1 only** - 2.0.1 has no `GetCertificateChainStatus`. `clock` supplies the fallback
    /// `nextUpdate` timestamp when `checker` does not provide one - see
    /// [`crate::certificate_status::handle_get_certificate_chain_status`].
    ///
    /// Only present when the `ocsp-checking` Cargo feature is enabled (C4.2).
    #[cfg(feature = "ocsp-checking")]
    pub async fn ocsp_chain_status<N, O, C>(self, csms: &N, checker: O, clock: C) -> Self
    where
        N: crate::certificate_status::GetCertificateChainStatusHandler + Send + Sync + 'static,
        O: crate::hardware::OcspChecker + Send + Sync + 'static,
        C: crate::clock::Clock + Send + Sync + 'static,
    {
        csms.register_get_certificate_chain_status_handler(self.runtime.actor(), checker, clock)
            .await;
        self
    }

    /// Registers the Battery Swap functional block (`docs/PRODUCTION-ROADMAP.md` B8.3):
    /// `RequestBatterySwap` inbound handling, dispatching hardware preparation to `station`.
    ///
    /// **2.1 only** - battery swap does not exist before 2.1. Niche and feature-gated: only
    /// present when the `battery-swap` Cargo feature is compiled in, and refuses at runtime (C5)
    /// unless [`Capabilities::battery_swap`] is also declared - see [`crate::battery_swap`]'s
    /// docs. Builder-only for the same reason `Self::certificates` (only present with the certificate-management feature)/[`Self::log_uploads`] are:
    /// it needs a [`crate::hardware::BatterySwapStation`], which `setup()`'s signature cannot
    /// receive.
    ///
    /// Reporting the charge point's own `BatterySwap` events back to the CSMS as hardware detects
    /// them is a separate call - see [`crate::battery_swap::report_battery_swap_event`] and
    /// [`crate::battery_swap::run_battery_swap_events`], which this method does not spawn: unlike
    /// [`Self::display_messages`]'s render loop, there is no charge-point-state-derived trigger to
    /// poll here, only real hardware events an integrator's own code already has to be watching
    /// for.
    #[cfg(feature = "battery-swap")]
    pub async fn battery_swap<N, S>(self, csms: &N, station: S) -> Self
    where
        N: crate::battery_swap::RequestBatterySwapHandler + Send + Sync + 'static,
        S: crate::hardware::BatterySwapStation + Send + Sync + 'static,
    {
        csms.register_request_battery_swap_handler(self.runtime.actor(), station)
            .await;
        self
    }

    /// Registers a physical payment terminal's identity against `PaymentCtrlr`'s device-model
    /// variables (`docs/PRODUCTION-ROADMAP.md` B7.2).
    ///
    /// **2.1 only** - `PaymentCtrlr` and the messages built on it
    /// (`NotifySettlement`/`NotifyWebPaymentStarted`/`VatNumberValidation`, see
    /// [`crate::payment`]) do not exist before 2.1. Unlike [`Self::battery_swap`]/
    /// `Self::certificates` (only present with the certificate-management feature), this registers no CSMS handler at all - the Payment block has none
    /// (see [`crate::payment`]'s module docs) - only `terminal`'s identity, overwriting the empty
    /// placeholder [`crate::device_model::capability_gate_events`] already registered for
    /// `VendorName`/`Model`/`SerialNumber`/`FirmwareVersion`/`TerminalID`/
    /// `PaymentServiceProvider` at `start`/`start_with_limits` time (see [`Capabilities::payment`]).
    ///
    /// A failed [`PaymentTerminal::info`](crate::hardware::PaymentTerminal::info) is logged and
    /// leaves those placeholders in place rather than failing registration outright - the same
    /// reasoning as every other hardware read this crate treats as fallible (`CLAUDE.md`): a
    /// terminal identity this crate briefly can't read is not a reason to refuse starting up.
    ///
    /// Sending the block's own messages
    /// ([`crate::payment::report_settlement`]/[`crate::payment::report_web_payment_started`]/
    /// [`crate::payment::validate_vat_number`]) is a separate call this method does not make:
    /// unlike a render loop or a reconnect handler, there is no charge-point-state-derived trigger
    /// to spawn here, only real payment-terminal events an integrator's own code already has to be
    /// watching for.
    #[cfg(feature = "payment")]
    pub async fn payment<P>(self, terminal: P) -> Self
    where
        P: crate::hardware::PaymentTerminal + Send + Sync + 'static,
    {
        match terminal.info().await {
            Ok(info) => {
                let actor = self.runtime.actor();
                let component = || Component {
                    name: "PaymentCtrlr".into(),
                    instance: None,
                    evse: None,
                };
                let fields = [
                    ("VendorName", info.vendor_name),
                    ("Model", info.model),
                    ("SerialNumber", info.serial_number),
                    ("FirmwareVersion", info.firmware_version),
                    ("TerminalID", info.terminal_id),
                    ("PaymentServiceProvider", info.payment_service_provider),
                ];
                for (name, value) in fields {
                    let _ = actor
                        .send(ChargePointEvent::DeviceModel(
                            DeviceModelEvent::AttributeValueSet {
                                component: component(),
                                variable: Variable {
                                    name: name.into(),
                                    instance: None,
                                },
                                attribute_type: VariableAttributeType::Actual,
                                value,
                            },
                        ))
                        .await;
                }
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "failed to read payment terminal identity - PaymentCtrlr keeps its placeholder values"
                );
            }
        }
        self
    }

    /// Registers the Display Message functional block (`docs/PRODUCTION-ROADMAP.md` B6):
    /// `SetDisplayMessage`, `GetDisplayMessages`, `ClearDisplayMessage` inbound handling, plus the
    /// worker that renders whichever message
    /// [`crate::display_message::current_message`] derives onto `display` and keeps it current as
    /// charge point state changes - with no CSMS involvement needed for that second part.
    ///
    /// **2.x only** - 1.6J has no display-message concept at all. Builder-only for the same reason
    /// [`Self::log_uploads`]/`Self::certificates` (only present with the certificate-management feature) are: it needs a
    /// [`crate::hardware::Display`], which `setup()`'s signature cannot receive.
    #[cfg(feature = "display-message")]
    pub async fn display_messages<N, D>(self, csms: &N, display: D) -> Self
    where
        N: crate::display_message::SetDisplayMessageHandler
            + crate::display_message::GetDisplayMessagesHandler
            + crate::display_message::ClearDisplayMessageHandler
            + Send
            + Sync
            + 'static,
        D: crate::hardware::Display + Send + Sync + 'static,
    {
        let supported_formats = display.supported_formats().to_vec();
        // The same list the handler enforces, told to the CSMS: `DisplayMessageCtrlr.
        // SupportedFormats` was registered empty with the capability (see
        // `crate::device_model`'s `CAPABILITY_GATED_VARIABLES`) because only the hardware knows
        // what it can render. A CSMS reading the placeholder would compose messages that
        // `handle_set_display_message` then refuses with `NotSupportedMessageFormat`.
        let _ = self
            .runtime
            .actor()
            .send(ChargePointEvent::DeviceModel(
                DeviceModelEvent::AttributeValueSet {
                    component: Component {
                        name: "DisplayMessageCtrlr".into(),
                        instance: None,
                        evse: None,
                    },
                    variable: Variable {
                        name: "SupportedFormats".into(),
                        instance: None,
                    },
                    attribute_type: VariableAttributeType::Actual,
                    value: supported_formats
                        .iter()
                        .map(|format| format.name())
                        .collect::<alloc::vec::Vec<_>>()
                        .join(","),
                },
            ))
            .await;
        csms.register_set_display_message_handler(self.runtime.actor(), supported_formats)
            .await;
        csms.register_get_display_messages_handler(self.runtime.actor())
            .await;
        csms.register_clear_display_message_handler(self.runtime.actor())
            .await;

        let actor = self.runtime.actor();
        self.executor.spawn(Box::pin(async move {
            crate::display_message::run_display_updates(&actor, &display).await;
        }));
        self
    }

    /// Registers `GetTransactionStatus` (`docs/PRODUCTION-ROADMAP.md` B5.4): lets the CSMS ask
    /// whether a transaction is still ongoing and whether there are still transaction-related
    /// messages queued for it - see [`crate::transaction_status`] for the requirements this
    /// answers from.
    ///
    /// **2.x only** - see that module's docs for why 1.6J has no such message.
    ///
    /// Answers `messagesInQueue` from whatever offline queue
    /// [`Self::transaction_events`]/[`Self::transaction_events_persisted`] created, so **register
    /// one of those first** if the CSMS is to see a real backlog rather than always `false` - the
    /// same ordering requirement [`Self::boot_reason_persistence`] documents for its own
    /// dependency. Calling this without either is still correct, just less informative: with no
    /// queue wired, nothing this crate produces is ever queued, so `false` is the honest answer,
    /// not a fallback standing in for one.
    ///
    /// Only present when the `diagnostics` Cargo feature is enabled (C4.2).
    #[cfg(feature = "diagnostics")]
    pub async fn get_transaction_status<N>(self, csms: &N) -> Self
    where
        N: crate::transaction_status::GetTransactionStatusHandler + Send + Sync + 'static,
    {
        csms.register_get_transaction_status_handler(
            self.runtime.actor(),
            self.transaction_queue.clone(),
        )
        .await;
        self
    }

    /// Registers the Customer Information block (`docs/PRODUCTION-ROADMAP.md` B5.5): inbound
    /// `CustomerInformation`, and the worker that gathers and sends every accepted `report` and
    /// applies every accepted `clear` - see [`crate::customer_information`].
    ///
    /// **2.x only** - see that module's docs for why 1.6J has no such message.
    ///
    /// Needs nothing but this charge point's own in-memory state and `clock` (for
    /// `NotifyCustomerInformation`'s `generatedAt`), unlike [`Self::log_uploads`]/
    /// [`Self::firmware_updates`] - but is still builder-only rather than folded into
    /// [`crate::setup::setup`]'s response-then-work worker, so that a future
    /// [`crate::hardware`]-backed customer store can slot in here without changing `setup()`'s
    /// signature.
    ///
    /// Only present when the `diagnostics` Cargo feature is enabled (C4.2).
    #[cfg(feature = "diagnostics")]
    pub async fn customer_information<N, K>(self, csms: &N, clock: K) -> Self
    where
        N: crate::customer_information::CustomerInformationHandler
            + crate::customer_information::CustomerInformationNotifier
            + Clone
            + Send
            + Sync
            + 'static,
        K: crate::clock::Clock + Send + Sync + 'static,
    {
        let queue = crate::customer_information::CustomerInformationQueue::new();
        csms.register_customer_information_handler(self.runtime.actor(), queue.clone())
            .await;

        let actor = self.runtime.actor();
        let notifier = csms.clone();
        self.executor.spawn(Box::pin(async move {
            crate::customer_information::run_customer_information_requests(
                &actor, queue, &notifier, &clock,
            )
            .await;
        }));
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

/// Raises a `MemoryExhaustion` security event on `actor` - the overflow handler wired to the
/// status and transaction `OfflineQueue`s (see [`ChargePointBuilder::status_notifications`] /
/// [`ChargePointBuilder::transaction_events`]) when their bound is hit
/// (`docs/PRODUCTION-ROADMAP.md` §9.2, G2.1). Deliberately not used by the security-event queue
/// itself - see the comment at its call site in [`ChargePointBuilder::security_events`] for why
/// that would recurse.
async fn report_memory_exhaustion(actor: &crate::actor::ChargePointActor) {
    report_security_event(
        actor,
        SecurityEvent {
            event_type: SecurityEventType::MemoryExhaustion,
            tech_info: Some(String::from(
                "an offline-report queue reached its configured capacity and dropped a message",
            )),
        },
    )
    .await;
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
        /// Behind an `Arc` so [`ChargePoint::start`]'s `Arc<Self>` receiver can be forwarded to
        /// the wrapped binding - a wrapper cannot hand out an `Arc` onto a field it merely owns.
        pub(crate) inner: alloc::sync::Arc<T>,
        pub(crate) capabilities: Capabilities,
    }

    #[async_trait::async_trait]
    impl<T, E, C> ChargePoint<E, C> for WithCapabilities<T>
    where
        T: ChargePoint<E, C> + Send + Sync,
        E: Evse<C>,
        C: Connector,
    {
        type StartError = T::StartError;

        fn vendor_name(&self) -> &str {
            self.inner.vendor_name()
        }

        fn model_name(&self) -> &str {
            self.inner.model_name()
        }

        fn evses(&self) -> &[E] {
            self.inner.evses()
        }

        fn capabilities(&self) -> Capabilities {
            self.capabilities
        }

        async fn start(
            self: Arc<Self>,
            events: HardwareEventSender,
            commands: HardwareCommandReceiver,
        ) -> Result<(), Self::StartError> {
            self.inner.clone().start(events, commands).await
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

        fn vendor_name(&self) -> &str {
            "Test vendor"
        }

        fn model_name(&self) -> &str {
            "Test model"
        }

        fn evses(&self) -> &[TestEvse] {
            &self.evses
        }

        fn capabilities(&self) -> crate::hardware::Capabilities {
            crate::hardware::Capabilities::default()
        }

        async fn start(
            self: Arc<Self>,
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

        fn connectors(&self) -> &[TestConnector] {
            &self.connectors
        }

        async fn reboot(&self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    /// Like [`TestChargePoint`], but doesn't connect a cable on start - every connector boots
    /// `Available`. Used by tests that need a connector free to reserve (or otherwise act on
    /// while idle), rather than `TestChargePoint`'s always-occupied fixture.
    pub(crate) struct IdleTestChargePoint {
        pub(crate) evses: [TestEvse; 1],
    }

    #[async_trait::async_trait]
    impl ChargePoint<TestEvse, TestConnector> for IdleTestChargePoint {
        type StartError = Infallible;

        fn vendor_name(&self) -> &str {
            "Test vendor"
        }

        fn model_name(&self) -> &str {
            "Test model"
        }

        fn evses(&self) -> &[TestEvse] {
            &self.evses
        }

        fn capabilities(&self) -> crate::hardware::Capabilities {
            crate::hardware::Capabilities::default()
        }

        async fn start(
            self: Arc<Self>,
            _events: HardwareEventSender,
            _commands: HardwareCommandReceiver,
        ) -> Result<(), Self::StartError> {
            Ok(())
        }
    }

    /// A [`TestEvse`] with two connectors instead of one - used by tests that need two distinct
    /// `(evse_id, connector_id)` addresses on the same charge point (e.g. to drive two
    /// independent status changes without either one deduping the other away).
    pub(crate) struct TestEvseWithTwoConnectors {
        pub(crate) connectors: [TestConnector; 2],
    }

    #[async_trait::async_trait]
    impl Evse<TestConnector> for TestEvseWithTwoConnectors {
        type Error = TestConnectorError;

        fn connectors(&self) -> &[TestConnector] {
            &self.connectors
        }

        async fn reboot(&self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    /// Like [`IdleTestChargePoint`], but with two connectors on its one EVSE - see
    /// [`TestEvseWithTwoConnectors`].
    pub(crate) struct IdleTwoConnectorTestChargePoint {
        pub(crate) evses: [TestEvseWithTwoConnectors; 1],
    }

    #[async_trait::async_trait]
    impl ChargePoint<TestEvseWithTwoConnectors, TestConnector> for IdleTwoConnectorTestChargePoint {
        type StartError = Infallible;

        fn vendor_name(&self) -> &str {
            "Test vendor"
        }

        fn model_name(&self) -> &str {
            "Test model"
        }

        fn evses(&self) -> &[TestEvseWithTwoConnectors] {
            &self.evses
        }

        fn capabilities(&self) -> crate::hardware::Capabilities {
            crate::hardware::Capabilities::default()
        }

        async fn start(
            self: Arc<Self>,
            _events: HardwareEventSender,
            _commands: HardwareCommandReceiver,
        ) -> Result<(), Self::StartError> {
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

        async fn set_current_limit(&self, _limit_ma: Option<u32>) -> Result<(), Self::Error> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ChargePointBuilder;
    use super::test_support::{TestChargePoint, TestConnector, TestEvse};
    use crate::executor::TokioExecutor;
    use crate::hardware::{InMemoryStorage, NoStorage};
    use crate::persistence::QueueStore;
    use crate::provisioning::BootNotificationOutcome;
    use crate::provisioning::TokioBackoff;
    use crate::provisioning::test_support::FixedBootNotifier;
    use crate::security::{SecurityEventNotifier, report_security_event};
    use crate::state::{BootReasonCause, ConnectorState, RegistrationStatus, SecurityEvent};
    use alloc::boxed::Box;
    use alloc::sync::Arc;
    use core::future::Future;
    use core::pin::Pin;
    use core::sync::atomic::{AtomicBool, Ordering};

    fn accepted_boot_notifier() -> FixedBootNotifier {
        FixedBootNotifier(BootNotificationOutcome {
            status: RegistrationStatus::Accepted,
            interval_secs: 60,
            current_time: None,
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
            reason: Option<BootReasonCause>,
        ) -> Result<BootNotificationOutcome, Self::Error> {
            self.0.notify_boot(vendor_name, model_name, reason).await
        }
    }

    #[async_trait::async_trait]
    impl crate::provisioning::HeartbeatSender for ProvisioningOnlyCsms {
        type Error = core::convert::Infallible;

        async fn send_heartbeat(
            &self,
        ) -> Result<Option<chrono::DateTime<chrono::Utc>>, Self::Error> {
            Ok(None)
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
            .provisioning(&csms, TokioBackoff, crate::clock::SystemMonotonicClock)
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

    // G2.2 (docs/PRODUCTION-ROADMAP.md §9.2): the whole point of `start_with_limits` is that a
    // caller's bounds reach the state the actor owns, and that the default path still gets this
    // crate's documented defaults.
    #[tokio::test]
    async fn start_with_limits_bounds_the_growable_collections() {
        let (charge_point, _locked) = test_charge_point(true);

        let runtime = ChargePointBuilder::start_with_limits(
            charge_point,
            TokioExecutor,
            crate::state::StateLimits::default()
                .with_max_local_authorization_list_entries(7)
                .with_max_device_model_variables(64),
        )
        .await
        .unwrap()
        .build();

        let state = runtime.state();
        assert_eq!(state.local_authorization_list.max_entries, 7);
        assert_eq!(state.device_model.max_variables(), 64);
    }

    #[tokio::test]
    async fn start_uses_this_crates_default_limits() {
        let (charge_point, _locked) = test_charge_point(true);

        let runtime = ChargePointBuilder::start(charge_point, TokioExecutor)
            .await
            .unwrap()
            .build();

        let state = runtime.state();
        assert_eq!(
            state.local_authorization_list.max_entries,
            crate::state::DEFAULT_MAX_LOCAL_AUTHORIZATION_LIST_ENTRIES
        );
        assert_eq!(
            state.device_model.max_variables(),
            crate::state::DEFAULT_MAX_DEVICE_MODEL_VARIABLES
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
            .provisioning(&csms, TokioBackoff, crate::clock::SystemMonotonicClock)
            .await
            .provisioning(&csms, TokioBackoff, crate::clock::SystemMonotonicClock)
            .await
            .build();

        assert_eq!(
            runtime.state().registration,
            Some(RegistrationStatus::Accepted)
        );
    }

    /// A CSMS implementing only the Display Message block's three registration traits, all no-ops:
    /// this fixture exists to reach [`ChargePointBuilder::display_messages`], whose device-model
    /// side effect is what the test below observes, not to exercise any inbound handling.
    #[cfg(feature = "display-message")]
    #[derive(Clone)]
    struct DisplayOnlyCsms;

    #[cfg(feature = "display-message")]
    #[async_trait::async_trait]
    impl crate::display_message::SetDisplayMessageHandler for DisplayOnlyCsms {
        async fn register_set_display_message_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
            _supported_formats: alloc::vec::Vec<crate::state::MessageFormat>,
        ) {
        }
    }

    #[cfg(feature = "display-message")]
    #[async_trait::async_trait]
    impl crate::display_message::GetDisplayMessagesHandler for DisplayOnlyCsms {
        async fn register_get_display_messages_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    #[cfg(feature = "display-message")]
    #[async_trait::async_trait]
    impl crate::display_message::ClearDisplayMessageHandler for DisplayOnlyCsms {
        async fn register_clear_display_message_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
        ) {
        }
    }

    /// A [`crate::hardware::Display`] that renders nothing but reports a real, restricted format
    /// list - the case that matters, since a screen that can do plain text but not HTML is the
    /// normal one.
    #[cfg(feature = "display-message")]
    struct TwoFormatDisplay;

    #[cfg(feature = "display-message")]
    #[async_trait::async_trait]
    impl crate::hardware::Display for TwoFormatDisplay {
        type Error = core::convert::Infallible;

        async fn show(
            &self,
            _message: Option<&crate::state::DisplayedMessage>,
        ) -> Result<(), Self::Error> {
            Ok(())
        }

        fn supported_formats(&self) -> &[crate::state::MessageFormat] {
            &[
                crate::state::MessageFormat::Ascii,
                crate::state::MessageFormat::Utf8,
            ]
        }
    }

    /// `DisplayMessageCtrlr.SupportedFormats` is a *hardware* fact, so - like `PaymentCtrlr`'s
    /// identity variables - the empty placeholder registered with the capability is overwritten
    /// from the real `Display` when one is registered. A CSMS that reads it must learn the same
    /// list `handle_set_display_message` will enforce, or it composes messages that are then
    /// refused with `NotSupportedMessageFormat`.
    #[cfg(feature = "display-message")]
    #[tokio::test]
    async fn registering_a_display_advertises_the_formats_it_can_actually_render() {
        let (charge_point, _locked) = test_charge_point(true);
        let charge_point = super::test_support::WithCapabilities {
            inner: alloc::sync::Arc::new(charge_point),
            capabilities: crate::hardware::Capabilities {
                has_display: true,
                ..Default::default()
            },
        };

        let runtime = ChargePointBuilder::start(charge_point, TokioExecutor)
            .await
            .unwrap()
            .display_messages(&DisplayOnlyCsms, TwoFormatDisplay)
            .await
            .build();

        let state = runtime.state();
        let definition = state
            .device_model
            .get(
                &crate::state::Component {
                    name: "DisplayMessageCtrlr".into(),
                    instance: None,
                    evse: None,
                },
                &crate::state::Variable {
                    name: "SupportedFormats".into(),
                    instance: None,
                },
            )
            .expect("DisplayMessageCtrlr.SupportedFormats should be registered with `has_display`");

        assert_eq!(
            definition
                .attribute(crate::state::VariableAttributeType::Actual)
                .unwrap()
                .value,
            "ASCII,UTF8",
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

    type BoxedReconnectCallback =
        Box<dyn FnMut() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

    /// What [`FlakyStatusCsms::notify_status`] delivered - `(evse_id, connector_id, status,
    /// connector_state)` - factored into a named alias per `clippy::type_complexity`.
    type DeliveredStatus = (usize, usize, crate::state::ConnectorStatus, ConnectorState);

    /// A `StatusNotifier` + `ReconnectHandler` fake whose `notify_status` fails or succeeds
    /// depending on `should_fail`, and whose "reconnect" fires only when a test calls
    /// `fire_reconnect` - standing in for `ocpp-client`'s real reconnect signal (a dropped and
    /// restored WebSocket), mirroring `crate::connection::tests::FakeReconnectingClient`. Used to
    /// drive `status_notifications_persisted`'s end-to-end reboot-survival test: `should_fail` is
    /// set while the connection is "down", so every delivery attempt is queued and persisted
    /// rather than lost.
    #[derive(Clone, Default)]
    struct FlakyStatusCsms {
        should_fail: Arc<AtomicBool>,
        delivered: Arc<std::sync::Mutex<alloc::vec::Vec<DeliveredStatus>>>,
        callback: Arc<tokio::sync::Mutex<Option<BoxedReconnectCallback>>>,
    }

    /// The error [`FlakyStatusCsms::notify_status`] and [`FlakySecurityCsms::notify_security_event`]
    /// return while `should_fail` is set.
    #[derive(Debug)]
    struct FlakyCsmsError;

    impl core::fmt::Display for FlakyCsmsError {
        fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            formatter.write_str("flaky test CSMS refused the request")
        }
    }

    impl core::error::Error for FlakyCsmsError {}

    impl FlakyStatusCsms {
        /// Fires whatever reconnect handler `status_notifications_persisted` registered, as if
        /// the connection had just come back up.
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
    impl crate::availability::StatusNotifier for FlakyStatusCsms {
        type Error = FlakyCsmsError;

        async fn notify_status(
            &self,
            evse_id: usize,
            connector_id: usize,
            status: crate::state::ConnectorStatus,
            connector_state: ConnectorState,
        ) -> Result<(), Self::Error> {
            if self.should_fail.load(Ordering::SeqCst) {
                return Err(FlakyCsmsError);
            }
            self.delivered
                .lock()
                .unwrap()
                .push((evse_id, connector_id, status, connector_state));
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::connection::ReconnectHandler for FlakyStatusCsms {
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

    #[tokio::test]
    async fn status_notifications_persisted_survives_a_reboot_and_replays_in_order() {
        use super::test_support::{IdleTwoConnectorTestChargePoint, TestEvseWithTwoConnectors};

        let storage = Arc::new(InMemoryStorage::new());

        // --- before the cut: a cable connects on connector 0 (Available -> Occupied) while the
        // CSMS connection is down - the resulting `StatusNotification` gets queued and persisted
        // instead of lost. A *second* status change then happens on the very same connector while
        // the first is still stuck in the queue (`LockConfirmed`, Connected -> Locked - still
        // `Occupied`, so wire-visible to `ChargePointState` but not to a version whose own status
        // doesn't distinguish it): this used to be the shape that lost data, because
        // `DedupedStatusNotifier` recorded a status as sent as soon as it was *attempted*, even on
        // a failed send - so retrying the first message would see its own already-cached status
        // and be wrongly treated as a duplicate rather than re-attempted, and the genuinely new
        // second message would be deduped against that same wrongly-cached entry too, vanishing
        // both from the queue without either ever reaching the CSMS. `DedupedStatusNotifier` now
        // only records an entry once `inner` actually accepts it, so this is exercised here
        // rather than sidestepped: exactly one `StatusNotification` for connector 0 (the restored
        // `Occupied`) must still reach the CSMS below, with the second, merely-internal transition
        // correctly deduped away once delivery succeeds - not lost before it ever sends.
        let charge_point1 = super::test_support::IdleTestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector {
                    locked: Arc::new(AtomicBool::new(false)),
                    lock_succeeds: true,
                }],
            }],
        };
        let csms1 = FlakyStatusCsms {
            should_fail: Arc::new(AtomicBool::new(true)),
            ..Default::default()
        };
        let builder1 = ChargePointBuilder::start(charge_point1, TokioExecutor)
            .await
            .unwrap()
            .status_notifications_persisted(&csms1, QueueStore::new(storage.clone(), "status"))
            .await;
        builder1
            .runtime
            .actor()
            .send(crate::state::ChargePointEvent::Evse {
                evse_id: 0,
                event: crate::state::EvseEvent::Connector {
                    connector_id: 0,
                    event: crate::state::ConnectorEvent::CableConnected,
                },
            })
            .await
            .unwrap();
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        // The second, same-connector, still-`Occupied` transition described above - queued right
        // behind the first while the connection is still down.
        builder1
            .runtime
            .actor()
            .send(crate::state::ChargePointEvent::Evse {
                evse_id: 0,
                event: crate::state::EvseEvent::Connector {
                    connector_id: 0,
                    event: crate::state::ConnectorEvent::LockConfirmed,
                },
            })
            .await
            .unwrap();
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        drop(builder1);

        // --- the cut: nothing but `storage` survives.

        // --- after the reboot: a fresh charge point, with the CSMS connection now up from the
        // start, restores the backlog into its queue and then raises one more, brand new status
        // change (connector 1 faults) - each message's *first and only* delivery attempt this
        // boot succeeds, so both are forwarded, in the order they were queued: the restored one
        // first, then the new one.
        let charge_point2 = IdleTwoConnectorTestChargePoint {
            evses: [TestEvseWithTwoConnectors {
                connectors: [
                    TestConnector {
                        locked: Arc::new(AtomicBool::new(false)),
                        lock_succeeds: true,
                    },
                    TestConnector {
                        locked: Arc::new(AtomicBool::new(false)),
                        lock_succeeds: true,
                    },
                ],
            }],
        };
        let csms2 = FlakyStatusCsms::default();
        let runtime2 = ChargePointBuilder::start(charge_point2, TokioExecutor)
            .await
            .unwrap()
            .status_notifications_persisted(&csms2, QueueStore::new(storage.clone(), "status"))
            .await
            .build();
        runtime2
            .actor()
            .send(crate::state::ChargePointEvent::Evse {
                evse_id: 0,
                event: crate::state::EvseEvent::Connector {
                    connector_id: 1,
                    event: crate::state::ConnectorEvent::FaultDetected,
                },
            })
            .await
            .unwrap();
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }

        let delivered = csms2.delivered.lock().unwrap().clone();
        assert_eq!(
            delivered.len(),
            2,
            "both the restored and the new status change arrived"
        );
        assert_eq!(
            delivered[0],
            (
                0,
                0,
                crate::state::ConnectorStatus::Occupied,
                ConnectorState::Connected
            ),
            "the restored status change is delivered first"
        );
        assert_eq!(
            delivered[1],
            (
                0,
                1,
                crate::state::ConnectorStatus::Faulted,
                ConnectorState::Faulted
            ),
            "the newly raised status change is delivered second, preserving queue order"
        );
    }

    #[tokio::test]
    async fn status_notifications_persisted_flushes_the_backlog_on_reconnect() {
        // Complements `status_notifications_persisted_survives_a_reboot_and_replays_in_order`,
        // which proves the restored backlog is delivered once a *new* status change arrives.
        // This proves the other trigger `status_notifications_persisted` wires up - the
        // `ReconnectHandler` callback - actually flushes (and re-persists) the queue too, using
        // `FlakyStatusCsms::fire_reconnect`.
        let storage = Arc::new(InMemoryStorage::new());

        let charge_point1 = super::test_support::IdleTestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector {
                    locked: Arc::new(AtomicBool::new(false)),
                    lock_succeeds: true,
                }],
            }],
        };
        let csms1 = FlakyStatusCsms {
            should_fail: Arc::new(AtomicBool::new(true)),
            ..Default::default()
        };
        let builder1 = ChargePointBuilder::start(charge_point1, TokioExecutor)
            .await
            .unwrap()
            .status_notifications_persisted(&csms1, QueueStore::new(storage.clone(), "status"))
            .await;
        builder1
            .runtime
            .actor()
            .send(crate::state::ChargePointEvent::Evse {
                evse_id: 0,
                event: crate::state::EvseEvent::Connector {
                    connector_id: 0,
                    event: crate::state::ConnectorEvent::CableConnected,
                },
            })
            .await
            .unwrap();
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        drop(builder1);

        let charge_point2 = super::test_support::IdleTestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector {
                    locked: Arc::new(AtomicBool::new(false)),
                    lock_succeeds: true,
                }],
            }],
        };
        let csms2 = FlakyStatusCsms::default();
        let runtime2 = ChargePointBuilder::start(charge_point2, TokioExecutor)
            .await
            .unwrap()
            .status_notifications_persisted(&csms2, QueueStore::new(storage.clone(), "status"))
            .await
            .build();
        let _ = &runtime2;
        csms2.fire_reconnect().await;
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }

        assert_eq!(
            csms2.delivered.lock().unwrap().clone(),
            alloc::vec![(
                0,
                0,
                crate::state::ConnectorStatus::Occupied,
                ConnectorState::Connected
            )]
        );
    }

    #[tokio::test]
    async fn registering_status_notifications_persisted_twice_forwards_each_status_change_once() {
        // Mirrors `registering_status_notifications_twice_forwards_each_status_change_once`
        // exactly, but through the persisted registration method - the repeat registration must
        // still be a no-op, not a second forwarder.
        let (charge_point, _locked) = test_charge_point(true);
        let calls = Arc::new(core::sync::atomic::AtomicUsize::new(0));
        let csms = CountingStatusCsms {
            calls: calls.clone(),
        };

        let runtime = ChargePointBuilder::start(charge_point, TokioExecutor)
            .await
            .unwrap()
            .status_notifications_persisted(&csms, QueueStore::new(NoStorage, "status"))
            .await
            .status_notifications_persisted(&csms, QueueStore::new(NoStorage, "status"))
            .await
            .build();

        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        let before = calls.load(Ordering::SeqCst);

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

        assert_eq!(calls.load(Ordering::SeqCst) - before, 1);
    }

    /// A `SecurityEventNotifier` + `ReconnectHandler` fake, otherwise identical to
    /// [`FlakyStatusCsms`] - see that type's docs. Used to drive
    /// `security_events_persisted`'s end-to-end reboot-survival test.
    #[derive(Clone, Default)]
    struct FlakySecurityCsms {
        should_fail: Arc<AtomicBool>,
        delivered: Arc<std::sync::Mutex<alloc::vec::Vec<SecurityEvent>>>,
        callback: Arc<tokio::sync::Mutex<Option<BoxedReconnectCallback>>>,
    }

    impl FlakySecurityCsms {
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
    impl SecurityEventNotifier for FlakySecurityCsms {
        type Error = FlakyCsmsError;

        async fn notify_security_event(
            &self,
            event_type: &crate::state::SecurityEventType,
            tech_info: Option<&str>,
        ) -> Result<(), Self::Error> {
            if self.should_fail.load(Ordering::SeqCst) {
                return Err(FlakyCsmsError);
            }
            self.delivered.lock().unwrap().push(SecurityEvent {
                event_type: event_type.clone(),
                tech_info: tech_info.map(alloc::string::ToString::to_string),
            });
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::connection::ReconnectHandler for FlakySecurityCsms {
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

    #[tokio::test]
    async fn a_flood_of_non_critical_events_cannot_evict_a_queued_critical_one() {
        // The attack this closes: `InvalidMessages` and `AttemptedReplayAttacks` are the two
        // security events a remote party can generate at will, simply by throwing malformed
        // frames at the charge point. The notification queue is bounded and drops its *oldest*
        // entry on overflow (G2.2), so if those shared it with critical events, an attacker could
        // flush a queued `TamperDetectionActivated` out of it before the CSMS ever saw it - and
        // silence the report of their own physical intrusion.
        let charge_point = super::test_support::IdleTestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector {
                    locked: Arc::new(AtomicBool::new(false)),
                    lock_succeeds: true,
                }],
            }],
        };
        let csms = FlakySecurityCsms {
            // Offline, so everything raised below has to sit in the queue rather than going out.
            should_fail: Arc::new(AtomicBool::new(true)),
            ..Default::default()
        };
        let builder = ChargePointBuilder::start(charge_point, TokioExecutor)
            .await
            .unwrap()
            // A queue small enough that the flood below would certainly overflow it.
            .offline_queue_capacity(4)
            .security_events(&csms)
            .await;

        report_security_event(
            &builder.runtime.actor(),
            SecurityEvent {
                event_type: crate::state::SecurityEventType::TamperDetectionActivated,
                tech_info: Some("door switch tripped".into()),
            },
        )
        .await;
        for index in 0..50 {
            report_security_event(
                &builder.runtime.actor(),
                SecurityEvent {
                    event_type: crate::state::SecurityEventType::InvalidMessages,
                    tech_info: Some(alloc::format!("malformed frame {index}")),
                },
            )
            .await;
        }
        for _ in 0..200 {
            tokio::task::yield_now().await;
        }

        // The connection comes back: whatever is still queued goes out.
        csms.should_fail.store(false, Ordering::SeqCst);
        csms.fire_reconnect().await;
        for _ in 0..200 {
            tokio::task::yield_now().await;
        }

        let delivered = csms.delivered.lock().unwrap().clone();
        assert_eq!(
            delivered
                .iter()
                .map(|event| event.event_type.clone())
                .collect::<alloc::vec::Vec<_>>(),
            alloc::vec![
                crate::state::SecurityEventType::StartupOfTheDevice,
                crate::state::SecurityEventType::TamperDetectionActivated
            ],
            "both critical events must survive the flood, and the flood itself must never have \
             been queued for the CSMS at all"
        );
    }

    #[tokio::test]
    async fn security_events_persisted_survives_a_reboot_and_replays_in_order() {
        let storage = Arc::new(InMemoryStorage::new());

        let first = SecurityEvent {
            event_type: crate::state::SecurityEventType::TamperDetectionActivated,
            tech_info: Some("door switch tripped".into()),
        };
        let second = SecurityEvent {
            event_type: crate::state::SecurityEventType::Other("VendorThing".into()),
            tech_info: None,
        };

        // --- before the cut: two security events raised while the CSMS connection is down - both
        // get queued and persisted instead of lost.
        let charge_point1 = super::test_support::IdleTestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector {
                    locked: Arc::new(AtomicBool::new(false)),
                    lock_succeeds: true,
                }],
            }],
        };
        let csms1 = FlakySecurityCsms {
            should_fail: Arc::new(AtomicBool::new(true)),
            ..Default::default()
        };
        let builder1 = ChargePointBuilder::start(charge_point1, TokioExecutor)
            .await
            .unwrap()
            .security_events_persisted(&csms1, QueueStore::new(storage.clone(), "security"))
            .await;
        report_security_event(&builder1.runtime.actor(), first.clone()).await;
        report_security_event(&builder1.runtime.actor(), second.clone()).await;
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        drop(builder1);

        // --- the cut: nothing but `storage` survives.

        // --- after the reboot: a fresh charge point restores the backlog and delivers it, in
        // order, once the connection is confirmed back up via `fire_reconnect`.
        let charge_point2 = super::test_support::IdleTestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector {
                    locked: Arc::new(AtomicBool::new(false)),
                    lock_succeeds: true,
                }],
            }],
        };
        let csms2 = FlakySecurityCsms::default();
        let runtime2 = ChargePointBuilder::start(charge_point2, TokioExecutor)
            .await
            .unwrap()
            .security_events_persisted(&csms2, QueueStore::new(storage.clone(), "security"))
            .await
            .build();
        let _ = &runtime2;
        csms2.fire_reconnect().await;
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }

        // Two boots bracket the backlog, and both are meant. The first is the pre-cut charge
        // point's, raised while the connection was down and recovered from storage with the rest;
        // the last is *this* charge point's own, raised after the reboot. A CSMS reading this
        // sequence can see exactly where the power cut fell.
        assert_eq!(
            csms2
                .delivered
                .lock()
                .unwrap()
                .iter()
                .map(|event| event.event_type.clone())
                .collect::<alloc::vec::Vec<_>>(),
            alloc::vec![
                crate::state::SecurityEventType::StartupOfTheDevice,
                first.event_type.clone(),
                second.event_type.clone(),
                crate::state::SecurityEventType::StartupOfTheDevice,
            ]
        );
    }

    /// E2.10's end-to-end guarantee at the builder level: the security log is recorded
    /// independently of whether the CSMS ever accepted the events, and survives a power cut.
    #[tokio::test]
    async fn security_log_persisted_survives_a_reboot_and_is_restored_in_order() {
        use crate::persistence::{SecurityLogStore, restore_security_log};
        use crate::security::SecurityEventLog;

        let storage = Arc::new(InMemoryStorage::new());

        // --- before the cut: two security events raised. No CSMS is registered at all here, which
        // is the point: the log is not a delivery buffer.
        let charge_point1 = super::test_support::IdleTestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector {
                    locked: Arc::new(AtomicBool::new(false)),
                    lock_succeeds: true,
                }],
            }],
        };
        let log1 = Arc::new(SecurityEventLog::new());
        let builder1 = ChargePointBuilder::start(charge_point1, TokioExecutor)
            .await
            .unwrap()
            .security_log_persisted(
                log1.clone(),
                SecurityLogStore::new(storage.clone()),
                crate::clock::SystemClock,
            )
            .await;
        // `StartupOfTheDevice` is not raised by hand any more - `ChargePointBuilder::start`
        // raises it for real (F4.2), so the log below opens with a boot this test did not fake.
        report_security_event(
            &builder1.runtime.actor(),
            SecurityEvent {
                event_type: crate::state::SecurityEventType::TamperDetectionActivated,
                tech_info: Some("door switch tripped".into()),
            },
        )
        .await;
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert_eq!(log1.len(), 2);
        drop(builder1);

        // --- the cut: nothing but `storage` survives. A fresh charge point restores the history.
        let charge_point2 = super::test_support::IdleTestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector {
                    locked: Arc::new(AtomicBool::new(false)),
                    lock_succeeds: true,
                }],
            }],
        };
        let log2 = Arc::new(SecurityEventLog::new());
        let runtime2 = ChargePointBuilder::start(charge_point2, TokioExecutor)
            .await
            .unwrap()
            .security_log_persisted(
                log2.clone(),
                SecurityLogStore::new(storage.clone()),
                crate::clock::SystemClock,
            )
            .await
            .build();
        let _ = &runtime2;

        let recovered: alloc::vec::Vec<crate::state::SecurityEventType> = log2
            .entries()
            .into_iter()
            .map(|entry| entry.event.event_type)
            .collect();
        assert_eq!(
            recovered,
            alloc::vec![
                crate::state::SecurityEventType::StartupOfTheDevice,
                crate::state::SecurityEventType::TamperDetectionActivated
            ]
        );

        // Registering the block restored the log into the caller's handle, not just into some
        // task-private copy - a later `GetLog`/clear reads this same handle.
        let restored_again = SecurityEventLog::new();
        assert_eq!(
            restore_security_log(&restored_again, &SecurityLogStore::new(storage)).await,
            2
        );
    }

    /// E2.5's end-to-end guarantee at the builder level: a charge point that reboots while its
    /// CSMS is unreachable still recognises the cards it knew, and knows them *before* the
    /// authorization block can be asked about one.
    #[tokio::test]
    async fn authorization_cache_persistence_survives_a_reboot_and_restores_before_authorization() {
        use crate::authorization::offline_decision;
        use crate::state::{AuthorizationStatus, ChargePointEvent, IdToken, IdTokenKind};

        let storage = Arc::new(InMemoryStorage::new());
        let known = IdToken {
            value: "04A224B2".into(),
            kind: IdTokenKind::ISO14443,
        };

        // --- before the cut: the CSMS accepts a card while the charge point is online.
        let charge_point1 = super::test_support::IdleTestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector {
                    locked: Arc::new(AtomicBool::new(false)),
                    lock_succeeds: true,
                }],
            }],
        };
        let builder1 = ChargePointBuilder::start(charge_point1, TokioExecutor)
            .await
            .unwrap()
            .authorization_cache_persistence(storage.clone())
            .await;
        let _ = builder1
            .runtime
            .actor()
            .send(ChargePointEvent::AuthorizationCached {
                id_token: known.clone(),
                status: AuthorizationStatus::Accepted,
                cached_at: chrono::DateTime::from_timestamp(1_800_000_000, 0),
            })
            .await;
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        drop(builder1);

        // --- after the reboot: the cache is back before anything could present an identifier.
        let charge_point2 = super::test_support::IdleTestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector {
                    locked: Arc::new(AtomicBool::new(false)),
                    lock_succeeds: true,
                }],
            }],
        };
        let runtime2 = ChargePointBuilder::start(charge_point2, TokioExecutor)
            .await
            .unwrap()
            .authorization_cache_persistence(storage.clone())
            .await
            .build();

        assert_eq!(
            offline_decision(
                &runtime2.actor().state(),
                &known,
                chrono::DateTime::from_timestamp(1_800_000_060, 0)
            ),
            AuthorizationStatus::Accepted,
            "a card the CSMS accepted before the cut must still charge while it stays unreachable"
        );
    }

    /// E2.7's end-to-end guarantee at the builder level: the load limits a CSMS installed are
    /// still in force after a power cut, and are back in force *before* the projection first
    /// decides what the connector may draw.
    #[tokio::test]
    async fn charging_profile_persistence_survives_a_reboot_and_restores_before_the_projection() {
        use crate::smart_charging::{
            ChargingLimitProjection, ClearChargingProfileHandler, GetCompositeScheduleHandler,
            SetChargingProfileHandler,
        };
        use crate::state::ChargePointEvent;
        use crate::state::{
            ChargingProfile, ChargingProfileId, ChargingProfileKind, ChargingProfilePurpose,
            ChargingProfileScope, ChargingRateUnit, ChargingSchedule, ChargingSchedulePeriod,
        };

        fn installation_limit() -> ChargingProfile {
            ChargingProfile {
                id: ChargingProfileId(1),
                stack_level: 0,
                // A charge-point-wide cap, so it limits the connector whether or not anything is
                // charging - which is what makes "did the limit survive?" observable without
                // driving a whole session first.
                purpose: ChargingProfilePurpose::ChargePointMax,
                kind: ChargingProfileKind::Absolute,
                recurrency: None,
                valid_from: None,
                valid_to: None,
                transaction_id: None,
                schedules: alloc::vec![ChargingSchedule {
                    id: 1,
                    start_schedule: None,
                    duration_secs: None,
                    rate_unit: ChargingRateUnit::Amps,
                    min_charging_rate: None,
                    periods: alloc::vec![ChargingSchedulePeriod {
                        start_period_secs: 0,
                        limit: 20.0,
                        number_phases: None,
                    }],
                }],
                dyn_update_interval_secs: None,
                dyn_update_time: None,
            }
        }

        let storage = Arc::new(InMemoryStorage::new());

        // --- before the cut: install a limit and let it reach storage.
        let charge_point1 = super::test_support::IdleTestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector {
                    locked: Arc::new(AtomicBool::new(false)),
                    lock_succeeds: true,
                }],
            }],
        };
        let builder1 = ChargePointBuilder::start(charge_point1, TokioExecutor)
            .await
            .unwrap()
            .charging_profile_persistence(storage.clone(), crate::clock::SystemClock)
            .await;
        let _ = builder1
            .runtime
            .actor()
            .send(ChargePointEvent::ChargingProfileSet {
                scope: ChargingProfileScope::ChargePoint,
                profile: Box::new(installation_limit()),
            })
            .await;
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        drop(builder1);

        // --- after the reboot: registering persistence first, then smart charging, is all it
        // takes for the projection's very first evaluation to see the recovered profile.
        let charge_point2 = super::test_support::IdleTestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector {
                    locked: Arc::new(AtomicBool::new(false)),
                    lock_succeeds: true,
                }],
            }],
        };
        // A CSMS that registers the three smart-charging handlers and does nothing else: this
        // test is about what the *projection* sees at boot, not about the wire.
        #[derive(Clone, Default)]
        struct SmartChargingCsms;

        #[async_trait::async_trait]
        impl SetChargingProfileHandler for SmartChargingCsms {
            async fn register_set_charging_profile_handler(
                &self,
                _actor: crate::actor::ChargePointActor,
            ) {
            }
        }

        #[async_trait::async_trait]
        impl ClearChargingProfileHandler for SmartChargingCsms {
            async fn register_clear_charging_profile_handler(
                &self,
                _actor: crate::actor::ChargePointActor,
            ) {
            }
        }

        #[async_trait::async_trait]
        impl GetCompositeScheduleHandler for SmartChargingCsms {
            async fn register_get_composite_schedule_handler(
                &self,
                _actor: crate::actor::ChargePointActor,
                _projection: Arc<ChargingLimitProjection>,
            ) {
            }
        }

        let csms = SmartChargingCsms;
        let runtime2 = ChargePointBuilder::start(charge_point2, TokioExecutor)
            .await
            .unwrap()
            .charging_profile_persistence(storage.clone(), crate::clock::SystemClock)
            .await
            .smart_charging(
                &csms,
                Arc::new(ChargingLimitProjection::new()),
                crate::clock::SystemClock,
                crate::provisioning::TokioBackoff,
            )
            .await
            .build();
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }

        let state = runtime2.actor().state();
        assert_eq!(state.charging_profiles.len(), 1);
        assert_eq!(
            state.evses[0].charging_limits[0],
            Some(20_000),
            "the recovered limit must reach hardware, not just the store"
        );
    }

    /// The E2.11 end-to-end guarantee: a network profile slot the CSMS wrote before a reboot is
    /// back in the store before anything could write to it or select from it again.
    #[tokio::test]
    async fn network_profile_persistence_survives_a_reboot_and_restores_before_registration() {
        use crate::state::{
            ChargePointEvent, NetworkConnectionProfile, NetworkInterface, NetworkTransport,
        };

        fn moved_profile() -> NetworkConnectionProfile {
            NetworkConnectionProfile {
                csms_url: "wss://operator.example/ocpp".into(),
                interface: NetworkInterface::Any,
                transport: NetworkTransport::Json,
                security_profile: 2,
                message_timeout_secs: 30,
                identity: None,
            }
        }

        let storage = Arc::new(InMemoryStorage::new());

        // --- before the cut: the CSMS moves the charge point onto a new profile (A9).
        let charge_point1 = super::test_support::IdleTestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector {
                    locked: Arc::new(AtomicBool::new(false)),
                    lock_succeeds: true,
                }],
            }],
        };
        let builder1 = ChargePointBuilder::start(charge_point1, TokioExecutor)
            .await
            .unwrap()
            .network_profile_persistence(storage.clone())
            .await;
        let _ = builder1
            .runtime
            .actor()
            .send(ChargePointEvent::NetworkProfileSet {
                slot: 1,
                profile: Box::new(moved_profile()),
            })
            .await;
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        drop(builder1);

        // --- after the reboot: the slot is back before `network_profiles` can register the
        // inbound handler that would otherwise race the restore.
        let charge_point2 = super::test_support::IdleTestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector {
                    locked: Arc::new(AtomicBool::new(false)),
                    lock_succeeds: true,
                }],
            }],
        };
        let runtime2 = ChargePointBuilder::start(charge_point2, TokioExecutor)
            .await
            .unwrap()
            .network_profile_persistence(storage.clone())
            .await
            .build();

        let state = runtime2.actor().state();
        assert_eq!(
            state.network_profiles.get(1).map(|p| p.csms_url.as_str()),
            Some("wss://operator.example/ocpp"),
            "a charge point that reboots must come back on the profile the operator moved it to, \
             not the one its integrator compiled in"
        );
    }

    /// A `SecurityEventNotifier` + `ReconnectHandler` fake counting every `notify_security_event`
    /// call it receives - the security equivalent of [`CountingStatusCsms`], used to prove that
    /// registering `security_events_persisted` twice results in exactly one
    /// SecurityEventNotification per raised event, not two.
    #[derive(Clone, Default)]
    struct CountingSecurityCsms {
        calls: Arc<core::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl SecurityEventNotifier for CountingSecurityCsms {
        type Error = core::convert::Infallible;

        async fn notify_security_event(
            &self,
            _event_type: &crate::state::SecurityEventType,
            _tech_info: Option<&str>,
        ) -> Result<(), Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::connection::ReconnectHandler for CountingSecurityCsms {
        async fn register_reconnect_handler<F, FF>(&self, _callback: F)
        where
            F: FnMut() -> FF + Send + Sync + 'static,
            FF: Future<Output = ()> + Send + 'static,
        {
            // No transport to reconnect in this fixture - nothing to register against.
        }
    }

    #[tokio::test]
    async fn registering_security_events_persisted_twice_reports_each_event_once() {
        let charge_point = super::test_support::IdleTestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector {
                    locked: Arc::new(AtomicBool::new(false)),
                    lock_succeeds: true,
                }],
            }],
        };
        let csms = CountingSecurityCsms::default();

        let runtime = ChargePointBuilder::start(charge_point, TokioExecutor)
            .await
            .unwrap()
            .security_events_persisted(&csms, QueueStore::new(NoStorage, "security"))
            .await
            .security_events_persisted(&csms, QueueStore::new(NoStorage, "security"))
            .await
            .build();

        report_security_event(
            &runtime.actor(),
            SecurityEvent {
                event_type: crate::state::SecurityEventType::TamperDetectionActivated,
                tech_info: None,
            },
        )
        .await;
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }

        // Two notifications for two critical events - the boot `ChargePointBuilder::start` raises
        // (F4.2) and the tamper raised above - which is still *exactly one each*: the repeat
        // `security_events_persisted` registration must not have spawned a second forwarder, which
        // would double both.
        assert_eq!(csms.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn security_events_persisted_overflow_does_not_raise_memory_exhaustion() {
        // Guards the single most important behaviour `security_events_persisted` must carry over
        // from `security_events`: unlike the status/transaction queues, this queue's overflow
        // handler must NOT raise a `MemoryExhaustion` security event, since that event would feed
        // straight back into this same queue and, if it's already full, overflow again - an
        // unbounded loop.
        let charge_point = super::test_support::IdleTestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector {
                    locked: Arc::new(AtomicBool::new(false)),
                    lock_succeeds: true,
                }],
            }],
        };
        let csms = FlakySecurityCsms {
            should_fail: Arc::new(AtomicBool::new(true)),
            ..Default::default()
        };

        let builder = ChargePointBuilder::start(charge_point, TokioExecutor)
            .await
            .unwrap();
        // An independent observer subscription, alongside the one
        // `security_events_persisted` consumes below - so this test sees every real security
        // event raised on the actor, including any `MemoryExhaustion` the overflow handler might
        // (incorrectly) raise.
        let mut observer = builder.runtime.subscribe_security_events();

        let runtime = builder
            .security_events_persisted(&csms, QueueStore::new(NoStorage, "security"))
            .await
            .build();

        // The queue's default capacity is `crate::offline_queue::DEFAULT_CAPACITY` (100); with
        // the CSMS connection down for the whole test, nothing ever drains, so the 101st raised
        // event overflows it exactly once.
        for i in 0..(crate::offline_queue::DEFAULT_CAPACITY + 1) {
            report_security_event(
                &runtime.actor(),
                SecurityEvent {
                    event_type: crate::state::SecurityEventType::Other(alloc::format!("event-{i}")),
                    tech_info: None,
                },
            )
            .await;
        }
        for _ in 0..200 {
            tokio::task::yield_now().await;
        }

        let mut seen_memory_exhaustion = false;
        while let Ok(Ok(event)) =
            tokio::time::timeout(core::time::Duration::from_millis(50), observer.recv()).await
        {
            if event.event_type == crate::state::SecurityEventType::MemoryExhaustion {
                seen_memory_exhaustion = true;
            }
        }
        assert!(
            !seen_memory_exhaustion,
            "the security-event queue's own overflow must never raise MemoryExhaustion - see \
             `ChargePointBuilder::security_events_persisted`'s doc comment"
        );
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
            .provisioning(&csms, TokioBackoff, crate::clock::SystemMonotonicClock)
            .await
            .status_notifications(&csms)
            .await
            .transaction_events(&csms)
            .await
            .authorization(&csms, crate::clock::SystemClock)
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
            .tariffs(&csms)
            .await
            .build();

        assert_eq!(
            runtime.state().evses[0].connectors[0],
            ConnectorState::Faulted
        );
        assert!(!locked.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn registering_transaction_persistence_recovers_an_interrupted_transaction_at_boot() {
        use crate::persistence::{PersistedTransaction, SCHEMA_VERSION, TransactionStore};
        use crate::state::{
            IdToken, IdTokenKind, StopReason, Transaction, TransactionChargingState,
            TransactionEventKind, TransactionId,
        };

        // Storage as a previous run left it: a transaction was in flight, 4.2 kWh delivered, when
        // the power was cut.
        let storage = Arc::new(crate::hardware::InMemoryStorage::new());
        let seeding_store = TransactionStore::new(storage.clone());
        seeding_store
            .save(&PersistedTransaction {
                schema_version: SCHEMA_VERSION,
                evse_id: 0,
                connector_id: 0,
                transaction: Transaction {
                    id: TransactionId(3),
                    id_token: Some(IdToken {
                        value: "04A224B2".into(),
                        kind: IdTokenKind::ISO14443,
                    }),
                    charging_state: TransactionChargingState::Charging,
                    stop_reason: None,
                    seq_no: 9,
                    last_meter_sample: Some(crate::state::MeterSample {
                        energy_wh: 4_200,
                        ..Default::default()
                    }),
                    priority_charging: false,
                },
                started_at: None,
                meter_start: None,
            })
            .await;
        seeding_store.save_next_transaction_id(4).await;

        let (charge_point, _locked) = test_charge_point(true);
        let builder = ChargePointBuilder::start(charge_point, TokioExecutor)
            .await
            .unwrap();
        // Subscribe before the recovery runs, exactly as a registered Transactions block would
        // have (its subscription is taken up front in `start`).
        let mut reported = builder.runtime.subscribe_transaction_events();

        let runtime = builder
            .transaction_persistence(storage.clone(), crate::clock::SystemClock)
            .await
            .build();

        let closing = reported.recv().await.expect("a closing transaction event");
        assert_eq!(closing.kind, TransactionEventKind::Ended);
        assert_eq!(closing.transaction.id, TransactionId(3));
        assert_eq!(closing.transaction.stop_reason, Some(StopReason::PowerLoss));
        assert_eq!(
            closing
                .transaction
                .last_meter_sample
                .map(|sample| sample.energy_wh),
            Some(4_200)
        );
        assert_eq!(runtime.state().next_transaction_id, 4);
        // The record is consumed, so a second boot doesn't report the same session again.
        assert_eq!(seeding_store.load(0, 0).await, None);
    }

    #[tokio::test]
    async fn registering_local_authorization_list_persistence_recovers_the_list_at_boot() {
        use crate::persistence::LocalAuthorizationListStore;
        use crate::state::{AuthorizationStatus, IdToken, IdTokenKind, LocalListEntry};

        let entry = LocalListEntry {
            id_token: IdToken {
                value: "04A224B2".into(),
                kind: IdTokenKind::ISO14443,
            },
            status: AuthorizationStatus::Accepted,
        };
        let storage = Arc::new(crate::hardware::InMemoryStorage::new());
        LocalAuthorizationListStore::new(storage.clone())
            .save(5, core::slice::from_ref(&entry))
            .await;

        let (charge_point, _locked) = test_charge_point(true);
        let runtime = ChargePointBuilder::start(charge_point, TokioExecutor)
            .await
            .unwrap()
            .local_authorization_list_persistence(storage)
            .await
            .build();

        assert_eq!(runtime.state().local_authorization_list.version, 5);
        assert_eq!(
            runtime.state().local_authorization_list.entries,
            vec![entry]
        );
    }

    #[tokio::test]
    async fn registering_reservation_persistence_recovers_an_active_reservation_at_boot() {
        use crate::persistence::{PersistedReservationEntry, ReservationStore};
        use crate::state::{ConnectorState, IdToken, IdTokenKind, Reservation, ReservationId};

        let storage = Arc::new(crate::hardware::InMemoryStorage::new());
        ReservationStore::new(storage.clone())
            .save(&[PersistedReservationEntry {
                evse_id: 0,
                connector_id: 0,
                reservation: Reservation {
                    id: ReservationId(1),
                    id_token: IdToken {
                        value: "04A224B2".into(),
                        kind: IdTokenKind::ISO14443,
                    },
                    expires_at: None,
                },
            }])
            .await;

        let charge_point = super::test_support::IdleTestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector {
                    locked: Arc::new(AtomicBool::new(false)),
                    lock_succeeds: true,
                }],
            }],
        };
        let runtime = ChargePointBuilder::start(charge_point, TokioExecutor)
            .await
            .unwrap()
            .reservation_persistence(storage, crate::clock::SystemClock)
            .await
            .build();

        assert_eq!(
            runtime.state().evses[0].connectors[0],
            ConnectorState::Reserved
        );
    }

    #[tokio::test]
    async fn registering_device_model_persistence_recovers_a_persistent_attribute_at_boot() {
        use crate::persistence::{DeviceModelStore, PersistedDeviceModelAttribute};
        use crate::state::{Component, Variable, VariableAttributeType};

        // The built-in default `OCPPCommCtrlr`/`HeartbeatInterval` variable is already registered
        // by every fresh device model (`crate::state::DeviceModel::new`) and flagged
        // `persistent`, so a persisted override for it is exactly the case
        // `restore_device_model` is meant to apply.
        let storage = Arc::new(crate::hardware::InMemoryStorage::new());
        DeviceModelStore::new(storage.clone())
            .save(&[PersistedDeviceModelAttribute {
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
                value: "300".into(),
            }])
            .await;

        let (charge_point, _locked) = test_charge_point(true);
        let runtime = ChargePointBuilder::start(charge_point, TokioExecutor)
            .await
            .unwrap()
            .device_model_persistence(storage)
            .await
            .build();

        let component = Component {
            name: "OCPPCommCtrlr".into(),
            instance: None,
            evse: None,
        };
        let variable = Variable {
            name: "HeartbeatInterval".into(),
            instance: None,
        };
        assert_eq!(
            runtime
                .state()
                .device_model
                .get(&component, &variable)
                .unwrap()
                .attribute(VariableAttributeType::Actual)
                .unwrap()
                .value,
            "300"
        );
    }

    /// A CSMS type that records the `reason` every `notify_boot` call carried, in order, so a
    /// test can assert exactly what [`ChargePointBuilder::boot_reason_persistence`] fed into
    /// [`ChargePointBuilder::provisioning`].
    #[derive(Clone)]
    struct RecordingReasonCsms {
        outcome: BootNotificationOutcome,
        seen_reasons: Arc<std::sync::Mutex<alloc::vec::Vec<Option<BootReasonCause>>>>,
    }

    #[async_trait::async_trait]
    impl crate::provisioning::BootNotifier for RecordingReasonCsms {
        type Error = core::convert::Infallible;

        async fn notify_boot(
            &self,
            _vendor_name: &str,
            _model_name: &str,
            reason: Option<BootReasonCause>,
        ) -> Result<BootNotificationOutcome, Self::Error> {
            self.seen_reasons.lock().unwrap().push(reason);
            Ok(self.outcome)
        }
    }

    #[async_trait::async_trait]
    impl crate::provisioning::HeartbeatSender for RecordingReasonCsms {
        type Error = core::convert::Infallible;

        async fn send_heartbeat(
            &self,
        ) -> Result<Option<chrono::DateTime<chrono::Utc>>, Self::Error> {
            Ok(None)
        }
    }

    #[async_trait::async_trait]
    impl crate::connection::ReconnectHandler for RecordingReasonCsms {
        async fn register_reconnect_handler<F, FF>(&self, _callback: F)
        where
            F: FnMut() -> FF + Send + Sync + 'static,
            FF: core::future::Future<Output = ()> + Send + 'static,
        {
            // No transport to reconnect in this fixture - nothing to register against.
        }
    }

    #[tokio::test]
    async fn without_boot_reason_persistence_every_boot_notification_reports_no_persisted_cause() {
        let (charge_point, _locked) = test_charge_point(true);
        let csms = RecordingReasonCsms {
            outcome: BootNotificationOutcome {
                status: RegistrationStatus::Accepted,
                interval_secs: 60,
                current_time: None,
            },
            seen_reasons: Arc::new(std::sync::Mutex::new(alloc::vec::Vec::new())),
        };

        ChargePointBuilder::start(charge_point, TokioExecutor)
            .await
            .unwrap()
            .provisioning(&csms, TokioBackoff, crate::clock::SystemMonotonicClock)
            .await
            .build();

        assert_eq!(*csms.seen_reasons.lock().unwrap(), alloc::vec![None]);
    }

    #[tokio::test]
    async fn registering_boot_reason_persistence_reports_a_persisted_cause_and_then_clears_it() {
        use crate::persistence::BootReasonStore;

        let storage = Arc::new(crate::hardware::InMemoryStorage::new());
        // Storage as `crate::reset::handle_reset` left it before the reboot this boot follows: an
        // `Immediate` `Reset` was accepted just before power was lost.
        BootReasonStore::new(storage.clone())
            .save(BootReasonCause::RemoteReset)
            .await;

        let (charge_point, _locked) = test_charge_point(true);
        let csms = RecordingReasonCsms {
            outcome: BootNotificationOutcome {
                status: RegistrationStatus::Accepted,
                interval_secs: 60,
                current_time: None,
            },
            seen_reasons: Arc::new(std::sync::Mutex::new(alloc::vec::Vec::new())),
        };

        ChargePointBuilder::start(charge_point, TokioExecutor)
            .await
            .unwrap()
            .boot_reason_persistence(storage.clone())
            .await
            .provisioning(&csms, TokioBackoff, crate::clock::SystemMonotonicClock)
            .await
            .build();

        assert_eq!(
            *csms.seen_reasons.lock().unwrap(),
            alloc::vec![Some(BootReasonCause::RemoteReset)]
        );

        // The BootNotification carrying that cause was accepted - the persisted record must now
        // be gone, so an uncommanded restart before the *next* commanded reboot doesn't wrongly
        // keep reporting this one.
        assert_eq!(BootReasonStore::new(storage).load().await, None);
    }

    #[tokio::test]
    async fn a_reset_recorded_before_reboot_is_reported_as_the_cause_on_the_next_boot() {
        use crate::persistence::BootReasonStore;
        use crate::reset::handle_reset;
        use crate::state::{ResetKind, ResetTarget};

        let storage = Arc::new(crate::hardware::InMemoryStorage::new());
        let (charge_point, _locked) = test_charge_point(true);
        let runtime = ChargePointBuilder::start(charge_point, TokioExecutor)
            .await
            .unwrap()
            .boot_reason_persistence(storage.clone())
            .await
            .build();

        // Simulates a CSMS `Reset` (`OnIdle`, since the connector is available and nothing is in
        // progress, this fires immediately) arriving and being accepted, in the same process this
        // charge point is still running in - `handle_reset` is what
        // `crate::actor::ChargePointActor::set_boot_reason_recorder`'s installed hook is called
        // from.
        handle_reset(
            &runtime.actor(),
            ResetTarget::ChargePoint,
            ResetKind::OnIdle,
        )
        .await;

        // "The next boot": a fresh charge point over the same storage, as if the reboot the
        // above actually happened and this process restarted.
        assert_eq!(
            BootReasonStore::new(storage).load().await,
            Some(BootReasonCause::ScheduledReset)
        );
    }

    /// Captures whatever queue `ChargePointBuilder::get_transaction_status` hands its
    /// `register_get_transaction_status_handler`, so a test can drive
    /// [`crate::transaction_status::handle_get_transaction_status`] against the exact same queue
    /// [`Self::transaction_events`] filled - proving the two are wired to the same
    /// `OfflineQueue`, not two independent ones.
    #[derive(Clone, Default)]
    struct RecordingTransactionStatusCsms {
        should_fail: Arc<AtomicBool>,
        captured_queue: Arc<
            std::sync::Mutex<
                Option<
                    Arc<crate::offline_queue::OfflineQueue<crate::state::TransactionEventOccurred>>,
                >,
            >,
        >,
    }

    #[async_trait::async_trait]
    impl crate::transactions::TransactionNotifier for RecordingTransactionStatusCsms {
        type Error = FlakyCsmsError;

        async fn notify_transaction_event(
            &self,
            _evse_id: usize,
            _connector_id: usize,
            _kind: crate::state::TransactionEventKind,
            _transaction: crate::state::Transaction,
        ) -> Result<(), FlakyCsmsError> {
            if self.should_fail.load(Ordering::SeqCst) {
                return Err(FlakyCsmsError);
            }
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::connection::ReconnectHandler for RecordingTransactionStatusCsms {
        async fn register_reconnect_handler<F, FF>(&self, _callback: F)
        where
            F: FnMut() -> FF + Send + Sync + 'static,
            FF: Future<Output = ()> + Send + 'static,
        {
            // Not exercised here - this test is about queue wiring, not reconnect flush.
        }
    }

    #[async_trait::async_trait]
    impl crate::transaction_status::GetTransactionStatusHandler for RecordingTransactionStatusCsms {
        async fn register_get_transaction_status_handler(
            &self,
            _actor: crate::actor::ChargePointActor,
            queue: Option<
                Arc<crate::offline_queue::OfflineQueue<crate::state::TransactionEventOccurred>>,
            >,
        ) {
            *self.captured_queue.lock().unwrap() = queue;
        }
    }

    #[tokio::test]
    async fn get_transaction_status_answers_from_the_same_queue_transaction_events_fills() {
        use crate::state::{
            ChargePointEvent, ConnectorEvent, EvseEvent, IdToken, IdTokenKind, TransactionId,
        };
        use crate::transaction_status::{TransactionStatusQuery, handle_get_transaction_status};

        let charge_point = super::test_support::IdleTestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector {
                    locked: Arc::new(AtomicBool::new(false)),
                    lock_succeeds: true,
                }],
            }],
        };
        let csms = RecordingTransactionStatusCsms {
            // Offline throughout, so the transaction below sits in the queue rather than going
            // out - otherwise there would be nothing left to find when this test looks.
            should_fail: Arc::new(AtomicBool::new(true)),
            ..Default::default()
        };

        let runtime = ChargePointBuilder::start(charge_point, TokioExecutor)
            .await
            .unwrap()
            .transaction_events(&csms)
            .await
            .get_transaction_status(&csms)
            .await
            .build();

        // Starts transaction id 0 on the one connector this fixture has.
        for event in [
            ConnectorEvent::CableConnected,
            ConnectorEvent::LockConfirmed,
            ConnectorEvent::IdTokenPresented(IdToken {
                value: "04A224B2".into(),
                kind: IdTokenKind::ISO14443,
            }),
            ConnectorEvent::ChargingAuthorized(IdToken {
                value: "04A224B2".into(),
                kind: IdTokenKind::ISO14443,
            }),
        ] {
            runtime
                .actor()
                .send(ChargePointEvent::Evse {
                    evse_id: 0,
                    event: EvseEvent::Connector {
                        connector_id: 0,
                        event,
                    },
                })
                .await
                .unwrap();
        }
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }

        let queue = csms.captured_queue.lock().unwrap().clone().expect(
            "get_transaction_status registers after transaction_events, so a queue must \
                     have been captured",
        );
        assert!(!queue.is_empty());

        let status = handle_get_transaction_status(
            &runtime.actor(),
            Some(&queue),
            TransactionStatusQuery {
                transaction_id: Some(TransactionId(0)),
            },
        );
        assert_eq!(status.ongoing_indicator, Some(true));
        assert!(status.messages_in_queue);
    }

    /// `ChargePointBuilder::der_control` registers all five of the block's CSMS-initiated
    /// handlers - a CSMS implementing fewer of them fails to compile against the bound, and one
    /// implementing all five must see every `register_*` call actually invoked.
    #[cfg(feature = "der-control")]
    #[tokio::test]
    async fn der_control_registers_every_handler() {
        use crate::der_control::{
            AfrrSignalHandler, ClearDERControlHandler, GetDERControlHandler,
            NotifyAllowedEnergyTransferHandler, SetDERControlHandler,
        };

        #[derive(Clone, Default)]
        struct DerControlCsms {
            set: Arc<AtomicBool>,
            clear: Arc<AtomicBool>,
            get: Arc<AtomicBool>,
            afrr: Arc<AtomicBool>,
            allowed_energy_transfer: Arc<AtomicBool>,
        }

        #[async_trait::async_trait]
        impl SetDERControlHandler for DerControlCsms {
            async fn register_set_der_control_handler(
                &self,
                _actor: crate::actor::ChargePointActor,
            ) {
                self.set.store(true, Ordering::SeqCst);
            }
        }
        #[async_trait::async_trait]
        impl ClearDERControlHandler for DerControlCsms {
            async fn register_clear_der_control_handler(
                &self,
                _actor: crate::actor::ChargePointActor,
            ) {
                self.clear.store(true, Ordering::SeqCst);
            }
        }
        #[async_trait::async_trait]
        impl GetDERControlHandler for DerControlCsms {
            async fn register_get_der_control_handler(
                &self,
                _actor: crate::actor::ChargePointActor,
            ) {
                self.get.store(true, Ordering::SeqCst);
            }
        }
        #[async_trait::async_trait]
        impl AfrrSignalHandler for DerControlCsms {
            async fn register_afrr_signal_handler(&self, _actor: crate::actor::ChargePointActor) {
                self.afrr.store(true, Ordering::SeqCst);
            }
        }
        #[async_trait::async_trait]
        impl NotifyAllowedEnergyTransferHandler for DerControlCsms {
            async fn register_notify_allowed_energy_transfer_handler(
                &self,
                _actor: crate::actor::ChargePointActor,
            ) {
                self.allowed_energy_transfer.store(true, Ordering::SeqCst);
            }
        }

        let charge_point = super::test_support::IdleTestChargePoint {
            evses: [TestEvse {
                connectors: [TestConnector {
                    locked: Arc::new(AtomicBool::new(false)),
                    lock_succeeds: true,
                }],
            }],
        };
        let csms = DerControlCsms::default();
        let _runtime = ChargePointBuilder::start(charge_point, TokioExecutor)
            .await
            .unwrap()
            .der_control(&csms)
            .await
            .build();

        assert!(csms.set.load(Ordering::SeqCst));
        assert!(csms.clear.load(Ordering::SeqCst));
        assert!(csms.get.load(Ordering::SeqCst));
        assert!(csms.afrr.load(Ordering::SeqCst));
        assert!(csms.allowed_energy_transfer.load(Ordering::SeqCst));
    }
}
