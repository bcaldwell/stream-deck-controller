use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait Integration {
    // fn name(&self) -> &str;
    // fn actions(&self) -> Vec<&str>;
    async fn execute_action(&self, action: String, options: serde_json::value::Value)
        -> Result<()>;

    // fn resync(&self);
}
