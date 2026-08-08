use crate::executor::Executor;
use crate::state::{
    AuthorizationRequested, BatterySwapEvent, ChargePointEffect, ChargePointEvent,
    ChargePointState, ConnectorStatusChanged, HardwareCommand, PriorityChargingChange,
    ReservationUpdate, ResetKind, SecurityEvent, TransactionEventOccurred, TriggeredMonitor,
};
use crate::sync::{
    BroadcastReceiver, BroadcastSender, Chan, OneShot, WatchReceiver, broadcast_channel,
    watch_channel,
};
use alloc::boxed::Box;
use alloc::sync::Arc;
use core::cell::RefCell;
use core::future::Future;
use core::pin::Pin;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

/// A boot-reason-recording hook, installed via [`ChargePointActor::set_boot_reason_recorder`] and
/// called by [`crate::reset::handle_reset`] - see that method's docs for the full picture.
pub type BootReasonRecorder =
    Arc<dyn Fn(ResetKind) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

enum Command {
    Event {
        event: ChargePointEvent,
        acknowledged: OneShot<()>,
    },
}

/// The broadcast senders the actor's run loop publishes effects on, grouped so `run` takes one
/// argument for them instead of one per effect kind.
#[derive(Clone)]
struct EffectSenders {
    commands: BroadcastSender<HardwareCommand>,
    status_notifications: BroadcastSender<ConnectorStatusChanged>,
    transaction_events: BroadcastSender<TransactionEventOccurred>,
    authorization_requests: BroadcastSender<AuthorizationRequested>,
    security_events: BroadcastSender<SecurityEvent>,
    reservation_updates: BroadcastSender<ReservationUpdate>,
    priority_charging_changes: BroadcastSender<PriorityChargingChange>,
    variable_monitor_events: BroadcastSender<TriggeredMonitor>,
    battery_swap_events: BroadcastSender<BatterySwapEvent>,
}

/// An error sending an event to a [`ChargePointActor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorError {
    /// The actor's run loop is no longer receiving events. Not expected in normal operation -
    /// the actor lives for the process lifetime in practice - but callers should not treat it as
    /// fatal; see [`crate::hardware::HardwareEventSender::send`].
    Stopped,
    /// The actor's mailbox is full: it is not draining events as fast as they are arriving, and
    /// this event was refused rather than queued (G4.5).
    ///
    /// A hardware binding seeing this should treat its own view of the hardware as unconfirmed -
    /// the state machine has *not* been told - and either retry or drive the connector to a
    /// fail-safe state. See [`crate::sync`]'s mailbox docs for why a full mailbox refuses instead
    /// of dropping.
    MailboxFull,
}

/// A cloneable handle onto the charge point's actor: the single owner of
/// [`ChargePointState`], serializing every [`ChargePointEvent`] applied to it and publishing the
/// resulting state and effects (hardware commands, status notifications, transaction events,
/// authorization requests, security events) to subscribers. Every functional block in this crate
/// is wired to the charge point through one of these handles rather than touching state
/// directly.
#[derive(Clone)]
pub struct ChargePointActor {
    mailbox: Chan<Command>,
    state: WatchReceiver<ChargePointState>,
    commands: BroadcastSender<HardwareCommand>,
    status_notifications: BroadcastSender<ConnectorStatusChanged>,
    transaction_events: BroadcastSender<TransactionEventOccurred>,
    authorization_requests: BroadcastSender<AuthorizationRequested>,
    security_events: BroadcastSender<SecurityEvent>,
    reservation_updates: BroadcastSender<ReservationUpdate>,
    priority_charging_changes: BroadcastSender<PriorityChargingChange>,
    variable_monitor_events: BroadcastSender<TriggeredMonitor>,
    battery_swap_events: BroadcastSender<BatterySwapEvent>,
    // A plain shared cell rather than a `Watch` - this is a settable-once, read-many hook, not a
    // stream of values anything needs to await a *change* in. See `set_boot_reason_recorder`'s
    // docs for what it's for and why `crate::reset::handle_reset` needs a synchronous way to
    // reach it that doesn't route through `ResetHandler`'s per-protocol-version implementations.
    boot_reason_recorder:
        Arc<BlockingMutex<CriticalSectionRawMutex, RefCell<Option<BootReasonRecorder>>>>,
}

impl ChargePointActor {
    /// Creates a fresh [`ChargePointState`] with one EVSE per entry in `connector_counts` (each
    /// value is that EVSE's connector count), and spawns its run loop via `executor`. The
    /// returned handle is ready to use immediately - `spawn` does not wait for any state
    /// transition, since a new charge point starts in [`crate::state::LifecycleState::Booting`]
    /// with no transitions pending.
    pub fn spawn(
        connector_counts: impl IntoIterator<Item = usize>,
        executor: &dyn Executor,
    ) -> Self {
        Self::spawn_with_limits(
            connector_counts,
            executor,
            crate::state::StateLimits::default(),
        )
    }

    /// [`Self::spawn`] with caller-chosen bounds on the state's growable collections - see
    /// [`crate::state::StateLimits`] and `docs/PRODUCTION-ROADMAP.md` §9.2 (G2.2). Integrators
    /// going through [`crate::builder::ChargePointBuilder`] pass limits to
    /// [`crate::builder::ChargePointBuilder::start_with_limits`] instead.
    pub fn spawn_with_limits(
        connector_counts: impl IntoIterator<Item = usize>,
        executor: &dyn Executor,
        limits: crate::state::StateLimits,
    ) -> Self {
        Self::spawn_with_watchdog(
            connector_counts,
            executor,
            limits,
            Arc::new(crate::hardware::NoWatchdog),
        )
    }

    /// [`Self::spawn_with_limits`], additionally feeding `watchdog` once per applied event so the
    /// hardware can tell that the actor is still draining its mailbox (G4.3).
    ///
    /// Separate constructor rather than a parameter on the others because most charge points have
    /// no watchdog peripheral and should not have to name one - see
    /// [`crate::hardware::Watchdog`] for why this is fed from the run loop and nowhere else.
    pub fn spawn_with_watchdog(
        connector_counts: impl IntoIterator<Item = usize>,
        executor: &dyn Executor,
        limits: crate::state::StateLimits,
        watchdog: Arc<dyn crate::hardware::Watchdog>,
    ) -> Self {
        let state = ChargePointState::with_limits(connector_counts, limits);
        let mailbox = Chan::new();
        let (updates, state_receiver) = watch_channel(state.clone());
        let commands = broadcast_channel();
        let status_notifications = broadcast_channel();
        let transaction_events = broadcast_channel();
        let authorization_requests = broadcast_channel();
        let security_events = broadcast_channel();
        let reservation_updates = broadcast_channel();
        let priority_charging_changes = broadcast_channel();
        let variable_monitor_events = broadcast_channel();
        let battery_swap_events = broadcast_channel();
        let effects = EffectSenders {
            commands: commands.clone(),
            status_notifications: status_notifications.clone(),
            transaction_events: transaction_events.clone(),
            authorization_requests: authorization_requests.clone(),
            security_events: security_events.clone(),
            reservation_updates: reservation_updates.clone(),
            priority_charging_changes: priority_charging_changes.clone(),
            variable_monitor_events: variable_monitor_events.clone(),
            battery_swap_events: battery_swap_events.clone(),
        };
        executor.spawn(Box::pin(run(
            state,
            mailbox.clone(),
            updates,
            effects,
            watchdog,
        )));

        Self {
            mailbox,
            state: state_receiver,
            commands,
            status_notifications,
            transaction_events,
            authorization_requests,
            security_events,
            reservation_updates,
            priority_charging_changes,
            variable_monitor_events,
            battery_swap_events,
            boot_reason_recorder: Arc::new(BlockingMutex::new(RefCell::new(None))),
        }
    }

    /// Feeds `event` into the charge point's state machine and waits for it to be fully applied
    /// (state updated and every resulting effect published to subscribers) before returning.
    /// Events from every sender are applied one at a time, in the order `send` is called, so
    /// callers never observe a torn or partially-applied state.
    ///
    /// `Err(ActorError::Stopped)` is not expected in normal operation - see [`ActorError`] -
    /// but callers should not treat it as fatal.
    ///
    /// `Err(ActorError::MailboxFull)` means the actor is not draining fast enough and this event
    /// was **not** applied (G4.5). It is not dropped silently: the caller is told, because a
    /// hardware binding that believes an event landed will go on to believe the state machine
    /// agrees with its hardware.
    pub async fn send(&self, event: ChargePointEvent) -> Result<(), ActorError> {
        let acknowledged = OneShot::new();
        if !self.mailbox.send(Command::Event {
            event,
            acknowledged: acknowledged.clone(),
        }) {
            // Deliberately only logged, never reported through the actor: raising a
            // `MemoryExhaustion` security event here would mean sending an event into the very
            // mailbox that is full, which overflows again - the same unbounded feedback loop the
            // offline security-event queue documents at its own overflow handler.
            tracing::error!(
                "the charge point actor's mailbox is full; refusing an event rather than dropping \
                 a state-machine transition, which would wedge a connector"
            );
            return Err(ActorError::MailboxFull);
        }
        acknowledged.wait().await;
        Ok(())
    }

    /// A snapshot of the charge point's current state. Cheap to call repeatedly - it reads the
    /// latest value already published to the underlying watch channel rather than round-tripping
    /// through the actor.
    pub fn state(&self) -> ChargePointState {
        self.state.borrow()
    }

    /// Subscribes to every future state change. Unlike [`state`](Self::state), the returned
    /// receiver can be awaited for the *next* change rather than only reading the latest value.
    pub fn subscribe(&self) -> WatchReceiver<ChargePointState> {
        self.state.clone()
    }

    /// Subscribes to every [`HardwareCommand`] the state machine emits (lock/unlock a connector,
    /// open/close a contactor). A hardware binding drains this via
    /// [`HardwareCommandReceiver`](crate::hardware::HardwareCommandReceiver) rather than calling
    /// this directly.
    pub fn subscribe_commands(&self) -> BroadcastReceiver<HardwareCommand> {
        self.commands.subscribe()
    }

    /// Subscribes to every connector status change, reported to the CSMS via StatusNotification
    /// by the Availability functional block (see [`crate::availability::run_status_notifications`]).
    pub fn subscribe_status_notifications(&self) -> BroadcastReceiver<ConnectorStatusChanged> {
        self.status_notifications.subscribe()
    }

    /// Subscribes to every transaction lifecycle event (started/updated/ended), reported to the
    /// CSMS via TransactionEvent by the Transactions functional block (see
    /// [`crate::transactions::run_transaction_events`]).
    pub fn subscribe_transaction_events(&self) -> BroadcastReceiver<TransactionEventOccurred> {
        self.transaction_events.subscribe()
    }

    /// Subscribes to every presented identifier awaiting an authorization decision, asked of the
    /// CSMS via Authorize by the Authorization functional block (see
    /// [`crate::authorization::run_authorization_requests`]).
    pub fn subscribe_authorization_requests(&self) -> BroadcastReceiver<AuthorizationRequested> {
        self.authorization_requests.subscribe()
    }

    /// Subscribes to every security-relevant event, reported to the CSMS via
    /// SecurityEventNotification by the Security functional block (see
    /// [`crate::security::run_security_events`]).
    pub fn subscribe_security_events(&self) -> BroadcastReceiver<SecurityEvent> {
        self.security_events.subscribe()
    }

    /// Subscribes to reservations that ended without the CSMS asking - reported to it as
    /// ReservationStatusUpdate by the Reservation functional block (see
    /// [`crate::reservation::run_reservation_status_updates`]).
    pub fn subscribe_reservation_updates(&self) -> BroadcastReceiver<ReservationUpdate> {
        self.reservation_updates.subscribe()
    }

    /// Subscribes to priority-charging grants the charge point made on its own - reported to the
    /// CSMS as `NotifyPriorityCharging` by the Smart Charging functional block (see
    /// [`crate::smart_charging::run_priority_charging_notifications`]). A grant the CSMS asked
    /// for with `UsePriorityCharging` never appears here; see
    /// [`ChargePointEvent::PriorityChargingSet`](crate::state::ChargePointEvent::PriorityChargingSet).
    pub fn subscribe_priority_charging_changes(&self) -> BroadcastReceiver<PriorityChargingChange> {
        self.priority_charging_changes.subscribe()
    }

    /// Subscribes to every threshold/delta variable monitor that fired - reported to the CSMS via
    /// `NotifyEvent` by the variable monitoring engine (see
    /// [`crate::variable_monitoring::run_variable_monitor_events`]). A periodic monitor never
    /// appears here - see [`crate::variable_monitoring::run_periodic_variable_monitors`], which
    /// reads the store directly on its own clock instead.
    pub fn subscribe_variable_monitor_events(&self) -> BroadcastReceiver<TriggeredMonitor> {
        self.variable_monitor_events.subscribe()
    }

    /// Subscribes to every battery-swap lifecycle event hardware reports - forwarded to the CSMS
    /// via `BatterySwap` by the Battery Swap functional block (see
    /// [`crate::battery_swap::run_battery_swap_events`]). **2.1 only.**
    pub fn subscribe_battery_swap_events(&self) -> BroadcastReceiver<BatterySwapEvent> {
        self.battery_swap_events.subscribe()
    }

    /// Installs `recorder`, called by [`crate::reset::handle_reset`] with the accepted `Reset`'s
    /// `ResetKind` synchronously *before* the `ResetRequested` event that may immediately produce
    /// a `HardwareCommand::Reboot` is sent to this actor - so a durable-storage-backed `recorder`
    /// (see [`crate::persistence::BootReasonStore`], wired up by
    /// [`crate::builder::ChargePointBuilder::boot_reason_persistence`]) is guaranteed to have
    /// finished writing before that reboot could possibly reach hardware. See
    /// `docs/PRODUCTION-ROADMAP.md` §7.2/§7.4 (the boot-reason row of E2, and E4.2).
    ///
    /// This lives on the actor, rather than as a parameter threaded through
    /// [`crate::reset::ResetHandler`] and its three per-protocol-version implementations, because
    /// `handle_reset` is the single protocol-agnostic choke point every one of those already
    /// calls - reaching it via the actor handle they already carry avoids widening that trait (and
    /// every implementor, including test doubles) just to plumb an optional storage handle through.
    ///
    /// Only one recorder can be installed at a time - a second call replaces the first, exactly
    /// like every other `*_persistence` builder method's "last registration wins" behaviour.
    /// Never installing one (the default) makes `handle_reset` skip recording entirely - a charge
    /// point that never calls `boot_reason_persistence` behaves exactly as if this hook didn't
    /// exist.
    pub fn set_boot_reason_recorder(&self, recorder: BootReasonRecorder) {
        self.boot_reason_recorder
            .lock(|cell| *cell.borrow_mut() = Some(recorder));
    }

    /// The recorder installed via [`Self::set_boot_reason_recorder`], if any. `pub(crate)` -
    /// intended for [`crate::reset::handle_reset`] only.
    pub(crate) fn boot_reason_recorder(&self) -> Option<BootReasonRecorder> {
        self.boot_reason_recorder.lock(|cell| cell.borrow().clone())
    }
}

async fn run(
    mut state: ChargePointState,
    mailbox: Chan<Command>,
    updates: crate::sync::WatchSender<ChargePointState>,
    effects: EffectSenders,
    watchdog: Arc<dyn crate::hardware::Watchdog>,
) {
    loop {
        let Command::Event {
            event,
            acknowledged,
        } = mailbox.recv().await;

        tracing::info!(event = ?event, "new charge point event");
        for effect in state.apply(event) {
            match effect {
                ChargePointEffect::StateChanged => {
                    tracing::info!(state = ?state, "charge point state updated");
                    updates.send_replace(state.clone());
                }
                ChargePointEffect::HardwareCommand(command) => {
                    effects.commands.send(command);
                }
                ChargePointEffect::StatusNotification(changed) => {
                    effects.status_notifications.send(changed);
                }
                ChargePointEffect::TransactionEvent(occurred) => {
                    effects.transaction_events.send(occurred);
                }
                ChargePointEffect::AuthorizationRequested(requested) => {
                    effects.authorization_requests.send(requested);
                }
                ChargePointEffect::ReservationEnded(update) => {
                    effects.reservation_updates.send(update);
                }
                ChargePointEffect::SecurityEventOccurred(event) => {
                    effects.security_events.send(event);
                }
                ChargePointEffect::PriorityChargingChanged(change) => {
                    effects.priority_charging_changes.send(change);
                }
                ChargePointEffect::VariableMonitorTriggered(triggered) => {
                    effects.variable_monitor_events.send(triggered);
                }
                ChargePointEffect::BatterySwapEventOccurred(event) => {
                    effects.battery_swap_events.send(event);
                }
            }
        }
        // G4.3: fed here and nowhere else. This is the one place that proves the property worth
        // proving - the actor is draining its mailbox and applying events - and it is fed *after*
        // the effects are dispatched, so an event that wedges mid-apply stops the feeding rather
        // than petting the dog on its way in. See `crate::hardware::Watchdog`.
        watchdog.pet().await;
        acknowledged.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn the_watchdog_is_fed_once_per_applied_event_not_on_a_timer() {
        use crate::hardware::Watchdog;

        struct CountingWatchdog {
            pets: Arc<BlockingMutex<CriticalSectionRawMutex, RefCell<u32>>>,
        }

        #[async_trait::async_trait]
        impl Watchdog for CountingWatchdog {
            async fn pet(&self) {
                self.pets.lock(|pets| *pets.borrow_mut() += 1);
            }
        }

        let pets = Arc::new(BlockingMutex::new(RefCell::new(0)));
        let actor = ChargePointActor::spawn_with_watchdog(
            [1],
            &crate::executor::TokioExecutor,
            crate::state::StateLimits::default(),
            Arc::new(CountingWatchdog { pets: pets.clone() }),
        );

        // Nothing has been applied yet, so nothing has been proven.
        pets.lock(|p| assert_eq!(*p.borrow(), 0));

        let _ = actor.send(ChargePointEvent::BootCompleted).await;
        let _ = actor.send(ChargePointEvent::SetUnavailable).await;
        let _ = actor.send(ChargePointEvent::SetAvailable).await;

        // Three events applied, three proofs of life - the count tracks work done, not time
        // elapsed, which is the whole point: a timer-fed watchdog would keep petting happily
        // while the actor sat wedged with a contactor closed.
        pets.lock(|p| assert_eq!(*p.borrow(), 3));
    }

    #[tokio::test]
    async fn a_charge_point_with_no_watchdog_behaves_exactly_as_before() {
        let actor = ChargePointActor::spawn([1], &crate::executor::TokioExecutor);

        let _ = actor.send(ChargePointEvent::BootCompleted).await;

        assert_eq!(
            actor.state().lifecycle,
            crate::state::LifecycleState::Available
        );
    }
}
