use anyhow::{anyhow, Result};
use async_trait::async_trait;
use huehue::models::device_type::DeviceType;
use huehue::{Hue, Light};
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
        brightness: Option<f32>,
    },
    #[serde(rename = "set")]
    Set {
        light: Option<String>,
        room: Option<String>,
        brightness: Option<f32>,
    },
}

pub struct Integration {
    hue: Hue,
    light_name_to_id: HashMap<String, String>,
    room_name_to_light_group_id: HashMap<String, String>,
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
        // let hue = Hue::new_with_key(
        //     bridges.first().unwrap().address,
        //     device_type,
        //     env::var("HUE_USERNAME").unwrap(),
        // )
        // .await
        // .expect("Failed to run bridge information.");

        let mut hue_integration = Integration {
            hue: hue,
            light_name_to_id: HashMap::new(),
            room_name_to_light_group_id: HashMap::new(),
        };

        hue_integration.sync().await;

        println!(
            "Connected to hue bridge at {}\n{:?}\n{:?}",
            bridges.first().unwrap().address,
            hue_integration.light_name_to_id,
            hue_integration.room_name_to_light_group_id,
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

        let rooms = self.hue.rooms().await.unwrap();
        self.room_name_to_light_group_id.clear();

        for room in rooms {
            // seems to be needed to avoid some move issue, idk why
            let room_name = &room.name;
            for service_index in 0..room.services.len() {
                let service = &room.services[service_index];
                if service.rtype != "grouped_light".to_string() {
                    continue;
                }
                self.room_name_to_light_group_id
                    .insert(room_name.to_string(), service.rid.to_string());
            }
        }
    }

    async fn get_light_by_name(&self, name: &str) -> Result<huehue::Light> {
        let id = match self.light_name_to_id.get(name) {
            Some(x) => x.to_string(),
            None => return Err(anyhow!("Light named {} not found", name)),
        };

        Ok(self.hue.light_by_id(id).await?)
    }

    async fn get_room_light_by_name(&self, name: &str) -> Result<huehue::Light> {
        let id = match self.room_name_to_light_group_id.get(name) {
            Some(x) => x.to_string(),
            None => {
                return Err(anyhow!(
                    "Room named {} not found or didn't have any lights",
                    name
                ))
            }
        };

        Ok(self.hue.light_group_by_id(id).await?)
    }

    async fn toggle_light_action(&self, light_name: String, brightness: Option<f32>) -> Result<()> {
        let light = self.get_light_by_name(&light_name).await?;

        return self.toggle_light(light, brightness).await;
    }

    async fn set_light_action(&self, light_name: String, brightness: Option<f32>) -> Result<()> {
        let light = self.get_light_by_name(&light_name).await?;

        return self.set_light(light, brightness).await;
    }

    async fn toggle_light(&self, mut light: Light, brightness: Option<f32>) -> Result<()> {
        // turn light off if it is on, otherwise turn it on then set the brightness
        if light.on {
            return Ok(light.switch(false).await?);
        }

        return self.set_light(light, brightness).await;
    }

    async fn set_light(&self, mut light: Light, brightness_option: Option<f32>) -> Result<()> {
        // when brightness is none, just turn on the light
        // otherwise check if it is 0, and turn off the light
        // otherwise, turn on the light, then set the brightness
        // setting brightness first results in: device (light) is "soft off", command (.dimming.brightness) may not have effect
        let brightness = match brightness_option {
            Some(b) => b,
            None => return Ok(light.switch(false).await?),
        };

        if brightness == 0.0 {
            return Ok(light.switch(false).await?);
        }

        light.switch(true).await?;
        light.dimm(brightness).await?;

        Ok(())
    }

    async fn toggle_room_action(&self, room_name: String, brightness: Option<f32>) -> Result<()> {
        let light = self.get_room_light_by_name(&room_name).await?;

        return self.toggle_light(light, brightness).await;
    }

    async fn set_room_action(&self, room_name: String, brightness: Option<f32>) -> Result<()> {
        let light = self.get_room_light_by_name(&room_name).await?;

        return self.set_light(light, brightness).await;
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
            Actions::Toggle {
                light,
                room,
                brightness,
            } => {
                if let Some(light_name) = light {
                    return Ok(self.toggle_light_action(light_name, brightness).await?);
                }
                if let Some(room_name) = room {
                    return Ok(self.toggle_room_action(room_name, brightness).await?);
                }

                return Err(anyhow!("Either light or room options must be set"));
            }
            Actions::Set {
                light,
                room,
                brightness,
            } => {
                if let Some(light_name) = light {
                    return Ok(self.set_light_action(light_name, brightness).await?);
                }
                if let Some(room_name) = room {
                    return Ok(self.set_room_action(room_name, brightness).await?);
                }

                return Err(anyhow!("Either light or room options must be set"));
            }
        };
    }
}
