use anyhow::Result;
use async_trait::async_trait;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Action {
    pub action: String,
    #[serde(flatten)]
    pub options: serde_json::value::Value,
}

#[async_trait]
pub trait Integration {
    // fn name(&self) -> &str;
    // fn actions(&self) -> Vec<&str>;
    async fn execute_action(&self, action: String, options: serde_json::value::Value)
        -> Result<()>;

    // fn resync(&self);
}
