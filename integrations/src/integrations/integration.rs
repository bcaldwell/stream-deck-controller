use crate::integrations::IntegrationEnum;

use anyhow::Result;
use async_trait::async_trait;
use enum_dispatch::enum_dispatch;

pub type IntegrationResult = Result<IntegrationEnum>;

// IntegrationConfig is implemented by the integration, to convert some configuration (on the struct) to an integration in the ingegration enum
#[async_trait]
pub trait IntegrationConfig {
    async fn into_integration(&self, name: Option<String>) -> IntegrationResult;
}

// Intregration is the core logic of an integration
#[async_trait]
#[enum_dispatch]
pub trait Integration {
    fn name(&self) -> &str;
    async fn execute_action(&self, action: String, options: serde_json::value::Value)
        -> Result<()>;
}

// IntoIntegration is a helper trait for converting an integration into an integration result
// can't use the normal into trait since converting to an integration can fail, so this returns a result
// I think this can be try_into trait but not sure how to convert it
#[async_trait]
#[enum_dispatch]
pub trait IntoIntegration {
    async fn into_integration(&self) -> IntegrationResult;
}

// IntegrationConfiguration implements IntoIntegration, by calling IntegrationConfig, with the embeded name
// This struct is used for the configuration of integrations, and is what is exposed to the end user
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct IntegrationConfiguration<T: IntegrationConfig> {
    name: Option<String>,
    #[serde(flatten)]
    options: T,
}

#[async_trait]
impl<T> IntoIntegration for IntegrationConfiguration<T>
where
    T: IntegrationConfig + Sync + Send,
{
    async fn into_integration(&self) -> IntegrationResult {
        return self.options.into_integration(self.name.clone()).await;
    }
}
