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
use crate::device_model::{GetVariablesHandler, SetVariablesHandler};
use crate::executor::Executor;
use crate::hardware::ChargePoint;
use crate::hardware::Connector;
use crate::hardware::Evse;
use crate::hardware::{Capabilities, warn_on_feature_mismatches};
#[cfg(feature = "local-auth-list")]
use crate::local_authorization_list::{GetLocalListVersionHandler, SendLocalListHandler};
use crate::offline_queue::{OfflineQueue, OverflowPolicy, run_with_offline_queue};
use crate::persistence::{
    BootReasonStore, ChargingProfileSnapshotStore, DeviceModelStore, LocalAuthorizationListStore,
    QueueStore, ReservationStore, TransactionStore, flush_and_persist_security_event_queue,
    flush_and_persist_status_notification_queue, flush_and_persist_transaction_event_queue,
    restore_charging_profiles, restore_device_model, restore_local_authorization_list,
    restore_reservations, restore_security_event_queue, restore_security_log,
    restore_status_notification_queue, restore_transaction_event_queue, restore_transactions,
    run_charging_profile_persistence, run_device_model_persistence,
    run_local_authorization_list_persistence, run_persisted_security_event_queue,
    run_persisted_status_notification_queue, run_persisted_transaction_event_queue,
    run_reservation_persistence, run_security_log_persistence, run_transaction_persistence,
};
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
            transaction_persistence_events: Some(transaction_persistence_events),
            security_log_events: Some(security_log_events),
            boot_reason: None,
            boot_reason_clearer: None,
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
        let status_queue = Arc::new(OfflineQueue::new());
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
        let status_queue = Arc::new(OfflineQueue::new());
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
        csms.register_reconnect_handler(move || {
            let queue = status_queue.clone();
            let store = store.clone();
            let notifier = status_notifier.clone();
            async move {
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

        // `DropNewest`, unlike the status/security queues above: a queued `TransactionEvent`
        // carries a billable energy reading, and evicting the oldest to make room for a new one
        // would permanently lose that billing data. Rejecting the newest instead only delays
        // fresh transaction activity - it never disturbs what's already queued - see
        // `OverflowPolicy`'s docs.
        let transaction_queue =
            Arc::new(OfflineQueue::new().with_overflow_policy(OverflowPolicy::DropNewest));
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
        let transaction_queue =
            Arc::new(OfflineQueue::new().with_overflow_policy(OverflowPolicy::DropNewest));
        // Restore before any live traffic is wired up - so an event that arrives during start-up
        // can never be delivered ahead of an older one the backlog restores.
        restore_transaction_event_queue(&transaction_queue, &store).await;
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
        csms.register_reconnect_handler(move || {
            let queue = transaction_queue.clone();
            let store = store.clone();
            let csms = reconnect_csms.clone();
            async move {
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
            run_with_offline_queue(
                security_events,
                &forwarder_queue,
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
        let security_queue = Arc::new(OfflineQueue::new());
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
        csms.register_reconnect_handler(move || {
            let queue = security_queue.clone();
            let store = store.clone();
            let csms = reconnect_csms.clone();
            async move {
                flush_and_persist_security_event_queue(&queue, &store, move |event| {
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

    /// Like [`TestChargePoint`], but doesn't connect a cable on start - every connector boots
    /// `Available`. Used by tests that need a connector free to reserve (or otherwise act on
    /// while idle), rather than `TestChargePoint`'s always-occupied fixture.
    pub(crate) struct IdleTestChargePoint {
        pub(crate) evses: [TestEvse; 1],
    }

    #[async_trait::async_trait]
    impl ChargePoint<TestEvse, TestConnector> for IdleTestChargePoint {
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

        async fn connectors(&self) -> &[TestConnector] {
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

        async fn vendor_name(&self) -> &str {
            "Test vendor"
        }

        async fn model_name(&self) -> &str {
            "Test model"
        }

        async fn evses(&self) -> &[TestEvseWithTwoConnectors] {
            &self.evses
        }

        async fn capabilities(&self) -> crate::hardware::Capabilities {
            crate::hardware::Capabilities::default()
        }

        async fn start(
            &self,
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

        assert_eq!(
            csms2.delivered.lock().unwrap().clone(),
            alloc::vec![first, second]
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
        report_security_event(
            &builder1.runtime.actor(),
            SecurityEvent {
                event_type: crate::state::SecurityEventType::StartupOfTheDevice,
                tech_info: None,
            },
        )
        .await;
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

        // Exactly one SecurityEventNotification for the one raised event: the repeat
        // `security_events_persisted` registration must not have spawned a second forwarder.
        assert_eq!(csms.calls.load(Ordering::SeqCst), 1);
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
}
