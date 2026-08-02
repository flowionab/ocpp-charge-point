use crate::actor::{ActorError, ChargePointActor};
use crate::state::ChargePointEvent;

#[derive(Clone)]
pub struct HardwareEventSender {
    actor: ChargePointActor,
}

impl HardwareEventSender {
    pub(crate) fn new(actor: ChargePointActor) -> Self {
        Self { actor }
    }

    pub async fn send(&self, event: ChargePointEvent) -> Result<(), ActorError> {
        self.actor.send(event).await
    }
}
