use crate::state::HardwareCommand;
use crate::sync::{BroadcastReceiver, RecvError};

pub struct HardwareCommandReceiver {
    receiver: BroadcastReceiver<HardwareCommand>,
}

impl HardwareCommandReceiver {
    pub(crate) fn new(receiver: BroadcastReceiver<HardwareCommand>) -> Self {
        Self { receiver }
    }

    pub async fn recv(&mut self) -> Result<HardwareCommand, RecvError> {
        self.receiver.recv().await
    }
}
