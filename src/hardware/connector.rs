use alloc::boxed::Box;

#[async_trait::async_trait]
pub trait Connector {
    type Error: core::error::Error + Send + Sync + 'static;

    async fn lock(&self) -> Result<(), Self::Error>;
    async fn unlock(&self) -> Result<(), Self::Error>;
    async fn close_contactor(&self) -> Result<(), Self::Error>;
    async fn open_contactor(&self) -> Result<(), Self::Error>;
}
