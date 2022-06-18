use anyhow::{anyhow, Result};
use async_trait::async_trait;
use huehue::models::device_type::DeviceType;
use huehue::Hue;
use std::collections::HashMap;
use std::env;
use std::time::Duration;

use crate::integrations::integration;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "action")]
enum Actions {
    #[serde(rename = "toggle")]
    Toggle {
        light: Option<String>,
        room: Option<String>,
    },
}

pub struct Integration {
    hue: Hue,
    light_name_to_id: HashMap<String, String>,
}

impl Integration {
    pub async fn new() -> Integration {
        let bridges = Hue::bridges(Duration::from_secs(5)).await;
        let device_type = DeviceType::new("benjamin".to_owned(), "streamdeck".to_owned()).unwrap();

        let hue = Hue::new_with_key(
            bridges.first().unwrap().address,
            device_type,
            env::var("HUE_USERNAME").unwrap(),
        )
        .await
        .expect("Failed to run bridge information.");

        let mut hue_integration = Integration {
            hue: hue,
            light_name_to_id: HashMap::new(),
        };

        hue_integration.sync().await;

        println!(
            "Connected to hue bridge at {}\n{:?}",
            bridges.first().unwrap().address,
            hue_integration.light_name_to_id,
        );

        return hue_integration;
    }

    async fn sync(&mut self) {
        let lights = self.hue.lights().await.unwrap();
        self.light_name_to_id.clear();

        for light in lights {
            self.light_name_to_id
                .insert(light.name, light.id.to_string());
        }
    }

    async fn get_light_by_name(&self, name: &str) -> Result<huehue::Light> {
        let id = match self.light_name_to_id.get(name) {
            Some(x) => x.to_string(),
            None => return Err(anyhow!("Light named {} not found", name)),
        };

        Ok(self.hue.lights_by_id(id).await?)
    }

    async fn toggle_light_action(&self, light_name: String) -> Result<()> {
        let mut light = self.get_light_by_name(&light_name).await?;
        Ok(light.switch(!light.on).await?)
    }

    async fn toggle_room_action(&self, room_name: String) -> Result<()> {
        let mut light = self.get_light_by_name(&room_name).await?;
        Ok(light.switch(!light.on).await?)
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
            Actions::Toggle { light, room } => {
                if let Some(light_name) = light {
                    return Ok(self.toggle_light_action(light_name).await?);
                }
                if let Some(room_name) = room {
                    return Ok(self.toggle_room_action(room_name).await?);
                }

                return Err(anyhow!("Either light or room options must be set"));
            }
        };
    }
}
