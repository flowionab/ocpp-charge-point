use crate::actor::{ActorError, ChargePointActor};
use crate::hardware::{HardwareCommandReceiver, HardwareEventSender};
use crate::state::{ChargePointEvent, ChargePointState};
use tokio::sync::watch;

pub struct ChargePointRuntime<T = ()> {
    hardware: T,
    actor: ChargePointActor,
}

impl<T> ChargePointRuntime<T> {
    pub fn new(hardware: T, connector_counts: impl IntoIterator<Item = usize>) -> Self {
        Self {
            hardware,
            actor: ChargePointActor::spawn(connector_counts),
        }
    }

    pub async fn send(&self, event: ChargePointEvent) -> Result<(), ActorError> {
        self.actor.send(event).await
    }

    pub fn hardware_events(&self) -> HardwareEventSender {
        HardwareEventSender::new(self.actor.clone())
    }

    pub fn hardware_commands(&self) -> HardwareCommandReceiver {
        HardwareCommandReceiver::new(self.actor.subscribe_commands())
    }

    pub fn state(&self) -> ChargePointState {
        self.actor.state()
    }

    pub fn subscribe(&self) -> watch::Receiver<ChargePointState> {
        self.actor.subscribe()
    }

    pub(crate) fn hardware(&self) -> &T {
        &self.hardware
    }
}

#[cfg(test)]
mod tests {
    use super::ChargePointRuntime;
    use crate::state::{
        ChargePointEvent, ConnectorEvent, ConnectorState, EvseEvent, LifecycleState,
    };

    #[tokio::test]
    async fn runtime_routes_hardware_events_to_the_supervisor_actor() {
        let runtime = ChargePointRuntime::new((), [1]);
        let mut states = runtime.subscribe();

        runtime.send(ChargePointEvent::BootCompleted).await.unwrap();
        states.changed().await.unwrap();

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
        states.changed().await.unwrap();

        assert_eq!(runtime.state().lifecycle, LifecycleState::Available);
        assert_eq!(
            runtime.state().evses[0].connectors[0],
            ConnectorState::Connected
        );
    }
}
