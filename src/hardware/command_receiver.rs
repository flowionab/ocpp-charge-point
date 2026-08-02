use crate::state::HardwareCommand;
use tokio::sync::broadcast;

pub struct HardwareCommandReceiver {
    receiver: broadcast::Receiver<HardwareCommand>,
}

impl HardwareCommandReceiver {
    pub(crate) fn new(receiver: broadcast::Receiver<HardwareCommand>) -> Self {
        Self { receiver }
    }

    pub async fn recv(&mut self) -> Result<HardwareCommand, broadcast::error::RecvError> {
        self.receiver.recv().await
    }
}
