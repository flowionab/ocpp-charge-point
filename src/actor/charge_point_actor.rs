use crate::executor::Executor;
use crate::state::{
    AuthorizationRequested, ChargePointEffect, ChargePointEvent, ChargePointState,
    ConnectorStatusChanged, HardwareCommand, SecurityEvent, TransactionEventOccurred,
};
use crate::sync::{
    broadcast_channel, watch_channel, BroadcastReceiver, BroadcastSender, Chan, OneShot,
    WatchReceiver,
};
use alloc::boxed::Box;

enum Command {
    Event {
        event: ChargePointEvent,
        acknowledged: OneShot<()>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorError {
    Stopped,
}

#[derive(Clone)]
pub struct ChargePointActor {
    mailbox: Chan<Command>,
    state: WatchReceiver<ChargePointState>,
    commands: BroadcastSender<HardwareCommand>,
    status_notifications: BroadcastSender<ConnectorStatusChanged>,
    transaction_events: BroadcastSender<TransactionEventOccurred>,
    authorization_requests: BroadcastSender<AuthorizationRequested>,
    security_events: BroadcastSender<SecurityEvent>,
}

impl ChargePointActor {
    pub fn spawn(
        connector_counts: impl IntoIterator<Item = usize>,
        executor: &dyn Executor,
    ) -> Self {
        let state = ChargePointState::new(connector_counts);
        let mailbox = Chan::new();
        let (updates, state_receiver) = watch_channel(state.clone());
        let commands = broadcast_channel();
        let status_notifications = broadcast_channel();
        let transaction_events = broadcast_channel();
        let authorization_requests = broadcast_channel();
        let security_events = broadcast_channel();
        executor.spawn(Box::pin(run(
            state,
            mailbox.clone(),
            updates,
            commands.clone(),
            status_notifications.clone(),
            transaction_events.clone(),
            authorization_requests.clone(),
            security_events.clone(),
        )));

        Self {
            mailbox,
            state: state_receiver,
            commands,
            status_notifications,
            transaction_events,
            authorization_requests,
            security_events,
        }
    }

    pub async fn send(&self, event: ChargePointEvent) -> Result<(), ActorError> {
        let acknowledged = OneShot::new();
        self.mailbox.send(Command::Event {
            event,
            acknowledged: acknowledged.clone(),
        });
        acknowledged.wait().await;
        Ok(())
    }

    pub fn state(&self) -> ChargePointState {
        self.state.borrow()
    }

    pub fn subscribe(&self) -> WatchReceiver<ChargePointState> {
        self.state.clone()
    }

    pub fn subscribe_commands(&self) -> BroadcastReceiver<HardwareCommand> {
        self.commands.subscribe()
    }

    pub fn subscribe_status_notifications(&self) -> BroadcastReceiver<ConnectorStatusChanged> {
        self.status_notifications.subscribe()
    }

    pub fn subscribe_transaction_events(&self) -> BroadcastReceiver<TransactionEventOccurred> {
        self.transaction_events.subscribe()
    }

    pub fn subscribe_authorization_requests(&self) -> BroadcastReceiver<AuthorizationRequested> {
        self.authorization_requests.subscribe()
    }

    pub fn subscribe_security_events(&self) -> BroadcastReceiver<SecurityEvent> {
        self.security_events.subscribe()
    }
}

async fn run(
    mut state: ChargePointState,
    mailbox: Chan<Command>,
    updates: crate::sync::WatchSender<ChargePointState>,
    commands: BroadcastSender<HardwareCommand>,
    status_notifications: BroadcastSender<ConnectorStatusChanged>,
    transaction_events: BroadcastSender<TransactionEventOccurred>,
    authorization_requests: BroadcastSender<AuthorizationRequested>,
    security_events: BroadcastSender<SecurityEvent>,
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
                    commands.send(command);
                }
                ChargePointEffect::StatusNotification(changed) => {
                    status_notifications.send(changed);
                }
                ChargePointEffect::TransactionEvent(occurred) => {
                    transaction_events.send(occurred);
                }
                ChargePointEffect::AuthorizationRequested(requested) => {
                    authorization_requests.send(requested);
                }
                ChargePointEffect::SecurityEventOccurred(event) => {
                    security_events.send(event);
                }
            }
        }
        acknowledged.send(());
    }
}
