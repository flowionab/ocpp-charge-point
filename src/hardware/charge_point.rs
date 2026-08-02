use crate::hardware::Connector;
use crate::hardware::Evse;
use crate::hardware::HardwareCommandReceiver;
use crate::hardware::HardwareEventSender;
use alloc::boxed::Box;

#[async_trait::async_trait]
pub trait ChargePoint<E: Evse<C>, C: Connector> {
    type StartError: core::error::Error + Send + Sync + 'static;

    async fn vendor_name(&self) -> &str;
    async fn model_name(&self) -> &str;
    async fn evses(&self) -> &[E];
    async fn start(
        &self,
        events: HardwareEventSender,
        commands: HardwareCommandReceiver,
    ) -> Result<(), Self::StartError>;
}
