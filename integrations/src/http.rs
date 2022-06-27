use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::__private::de;
use std::{collections::HashMap, process::ExitStatus};
use tokio::process::Command;

use crate::integration;

// mayber use #[serde(untagged)] for this?
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "action")]
enum Actions {
    #[serde(rename = "get")]
    Get { url: String },
}

pub struct Integration {}

impl Integration {
    pub fn new() -> Integration {
        return Integration {};
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
    async fn execute_action(
        &self,
        _action: String,
        json_options: serde_json::value::Value,
    ) -> Result<()> {
        let options: Actions = serde_json::from_value(json_options).unwrap();

        match options {
            Actions::Get { url } => self.execute_get_request(url).await,
        }
    }
}
