use crate::actor::{ActorError, ChargePointActor};
use crate::state::ChargePointEvent;

/// The hardware-facing half of the channel [`ChargePoint::start`](crate::hardware::ChargePoint::start)
/// receives: push a [`ChargePointEvent`] whenever hardware observes something the state machine
/// needs to know about (a cable connected, an id token presented, a meter sample, a fault). A
/// thin, cloneable handle onto the charge point's actor - it exposes nothing beyond `send` so a
/// hardware binding can't reach into other actor state.
#[derive(Clone)]
pub struct HardwareEventSender {
    actor: ChargePointActor,
}

impl HardwareEventSender {
    pub(crate) fn new(actor: ChargePointActor) -> Self {
        Self { actor }
    }

    /// Feeds `event` into the charge point's state machine and waits for it to be applied.
    /// `Err(ActorError::Stopped)` is not expected in normal operation - see
    /// [`HardwareCommandReceiver::recv`](crate::hardware::HardwareCommandReceiver::recv) for why
    /// - but callers should not treat it as fatal.
    pub async fn send(&self, event: ChargePointEvent) -> Result<(), ActorError> {
        self.actor.send(event).await
    }
}
