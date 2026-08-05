use crate::state::HardwareCommand;
use crate::sync::{BroadcastReceiver, RecvError};

/// The hardware-facing half of the channel [`ChargePoint::start`](crate::hardware::ChargePoint::start)
/// receives: [`HardwareCommand`]s the state machine emits (lock/unlock a connector, open/close a
/// contactor) for a hardware binding to carry out, typically via
/// [`execute_hardware_command`](crate::hardware::execute_hardware_command).
pub struct HardwareCommandReceiver {
    receiver: BroadcastReceiver<HardwareCommand>,
}

impl HardwareCommandReceiver {
    pub(crate) fn new(receiver: BroadcastReceiver<HardwareCommand>) -> Self {
        Self { receiver }
    }

    /// Waits for the next [`HardwareCommand`]. `Err(RecvError::Closed)` is not expected in
    /// normal operation - the actor that produces commands lives for the process lifetime (see
    /// `src/sync.rs`'s module docs) - but a hardware binding should still stop its command loop
    /// on it rather than looping forever on an error.
    pub async fn recv(&mut self) -> Result<HardwareCommand, RecvError> {
        self.receiver.recv().await
    }
}
