use anyhow::Result;
use async_trait::async_trait;

pub type IntegrationResult = Result<Box<dyn Integration + Send + Sync>>;

#[async_trait]
pub trait IntegrationConfig {
    async fn to_integration(&self, name: Option<String>) -> IntegrationResult;
}

#[async_trait]
pub trait Integration {
    fn name(&self) -> &str;
    // fn actions(&self) -> Vec<&str>;
    async fn execute_action(&self, action: String, options: serde_json::value::Value)
        -> Result<()>;

    // fn resync(&self);
}
