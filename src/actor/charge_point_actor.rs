use crate::state::{ChargePointEffect, ChargePointEvent, ChargePointState, HardwareCommand};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

const MAILBOX_CAPACITY: usize = 32;
const COMMAND_CAPACITY: usize = 32;

enum Command {
    Event {
        event: ChargePointEvent,
        acknowledged: oneshot::Sender<()>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorError {
    Stopped,
}

#[derive(Clone)]
pub struct ChargePointActor {
    sender: mpsc::Sender<Command>,
    state: watch::Receiver<ChargePointState>,
    commands: broadcast::Sender<HardwareCommand>,
}

impl ChargePointActor {
    pub fn spawn(connector_counts: impl IntoIterator<Item = usize>) -> Self {
        let state = ChargePointState::new(connector_counts);
        let (sender, receiver) = mpsc::channel(MAILBOX_CAPACITY);
        let (updates, state_receiver) = watch::channel(state.clone());
        let (commands, _) = broadcast::channel(COMMAND_CAPACITY);
        tokio::spawn(run(state, receiver, updates, commands.clone()));

        Self {
            sender,
            state: state_receiver,
            commands,
        }
    }

    pub async fn send(&self, event: ChargePointEvent) -> Result<(), ActorError> {
        let (acknowledged, receipt) = oneshot::channel();
        self.sender
            .send(Command::Event {
                event,
                acknowledged,
            })
            .await
            .map_err(|_| ActorError::Stopped)?;
        receipt.await.map_err(|_| ActorError::Stopped)
    }

    pub fn state(&self) -> ChargePointState {
        self.state.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<ChargePointState> {
        self.state.clone()
    }

    pub fn subscribe_commands(&self) -> broadcast::Receiver<HardwareCommand> {
        self.commands.subscribe()
    }
}

async fn run(
    mut state: ChargePointState,
    mut receiver: mpsc::Receiver<Command>,
    updates: watch::Sender<ChargePointState>,
    commands: broadcast::Sender<HardwareCommand>,
) {
    while let Some(command) = receiver.recv().await {
        match command {
            Command::Event {
                event,
                acknowledged,
            } => {
                for effect in state.apply(event) {
                    match effect {
                        ChargePointEffect::StateChanged => {
                            updates.send_replace(state.clone());
                        }
                        ChargePointEffect::HardwareCommand(command) => {
                            let _ = commands.send(command);
                        }
                    }
                }
                let _ = acknowledged.send(());
            }
        }
    }
}
