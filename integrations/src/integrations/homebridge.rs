use anyhow::{anyhow, Result};
use async_trait::async_trait;

use crate::homebridge::Homebridge;
use crate::integrations::integration;

const DEFAULT_NAME: &str = "homebridge";

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct IntegrationConfig {
    api_endpoint: String,
    username: String,
    password: String,
}

#[async_trait]
impl integration::IntegrationConfig for IntegrationConfig {
    async fn to_integration(&self, name: Option<String>) -> integration::IntegrationResult {
        let username = shellexpand::env(&self.username)?.to_string();
        let password = shellexpand::env(&self.password)?.to_string();

        let i = Integration::new(
            name.unwrap_or(DEFAULT_NAME.to_string()),
            &self.api_endpoint,
            &username,
            &password,
        )
        .await?;
        return Ok(Box::new(i));
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ToggleAction {
    device: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "action")]
enum Actions {
    #[serde(rename = "toggle")]
    Toggle(ToggleAction),
}

pub struct Integration {
    name: String,
    homebridge: Homebridge,
}

impl Integration {
    pub async fn new(
        name: String,
        endpoint: &str,
        username: &str,
        password: &str,
    ) -> Result<Integration> {
        let homebridge = Homebridge::new(endpoint, username, password).await?;

        return Ok(Integration {
            name: name,
            homebridge: homebridge,
        });
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
            Actions::Toggle(action) => {
                let mut response = self.homebridge.get_device(action.device).await?;
                let on = match response.on() {
                    Some(true) => false,
                    Some(false) => true,
                    _ => return Err(anyhow!("device does not support switch")),
                };

                response.switch(on).await?;
            }
        }

        Ok(())
    }
}
