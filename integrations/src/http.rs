use anyhow::{anyhow, Result};
use async_trait::async_trait;

use crate::integration;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct IntegrationConfig {}

#[async_trait]
impl integration::IntegrationConfig for IntegrationConfig {
    async fn to_integration(&self, name: Option<String>) -> integration::IntegrationResult {
        return Ok(Box::new(Integration::new(
            name.unwrap_or("http".to_string()),
        )));
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct GetAction {
    url: String,
}

// mayber use #[serde(untagged)] for this?
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "action")]
enum Actions {
    #[serde(rename = "get")]
    Get(GetAction),
}

pub struct Integration {
    name: String,
}

impl Integration {
    pub fn new(name: String) -> Integration {
        return Integration { name: name };
    }

    async fn execute_get_request(&self, url: String) -> Result<()> {
        let client = reqwest::Client::new();
        let r = client.get(&url);

        let response = r.send().await?;
        if !response.status().is_success() {
            return Err(anyhow!(
                "failed to get url ({}): {:?}",
                url,
                response.text().await.unwrap_or("".to_string())
            ));
        }

        Ok(())
    }
}

#[async_trait]
impl integration::Integration for Integration {
    fn name(&self) -> &str {
        return &self.name;
    }

    async fn execute_action(
        &self,
        _action: String,
        json_options: serde_json::value::Value,
    ) -> Result<()> {
        let options: Actions = serde_json::from_value(json_options).unwrap();

        match options {
            Actions::Get(get_action) => self.execute_get_request(get_action.url).await,
        }
    }
}
