

#[async_trait::async_trait]
pub trait NetworkBridge {
    async fn disconnect(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

}