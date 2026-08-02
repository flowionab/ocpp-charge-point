use crate::hardware::Connector;
use alloc::boxed::Box;

#[async_trait::async_trait]
pub trait Evse<C: Connector> {
    async fn connectors(&self) -> &[C];
}
