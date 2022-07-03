use anyhow::{anyhow, Result};
use async_trait::async_trait;
use huehue::models::device_type::DeviceType;
use huehue::{Hue, Light};
use std::collections::HashMap;
use std::env;
use std::time::Duration;

use crate::integration;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct IntegrationConfig {
    pub auth: String,
}

#[async_trait]
impl integration::IntegrationConfig for IntegrationConfig {
    async fn to_integration(&self, name: Option<String>) -> integration::IntegrationResult {
        return Ok(Box::new(
            Integration::new(name.unwrap_or("hue".to_string())).await,
        ));
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ToggleOrSetAction {
    light: Option<String>,
    room: Option<String>,
    brightness: Option<f32>,
    rel_brightness: Option<f32>,
}

// mayber use #[serde(untagged)] for this?
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "action")]
enum Actions {
    #[serde(rename = "toggle")]
    Toggle(ToggleOrSetAction),
    #[serde(rename = "set")]
    Set(ToggleOrSetAction),
}

pub struct Integration {
    name: String,
    hue: Hue,
    light_name_to_id: HashMap<String, String>,
    room_name_to_light_group_id: HashMap<String, String>,
}

impl Integration {
    pub async fn new(name: String) -> Integration {
        let bridges = Hue::bridges(Duration::from_secs(5)).await;
        let device_type = DeviceType::new("benjamin".to_owned(), "streamdeck".to_owned()).unwrap();

        let hue = Hue::new_with_key(
            bridges.first().expect("getting hue bridges failed").address,
            device_type,
            env::var("HUE_USERNAME").unwrap(),
        )
        .await
        .expect("Failed to run bridge information.");

        let mut hue_integration = Integration {
            name: name,
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

    async fn toggle_light_action(
        &self,
        light_name: String,
        brightness: Option<f32>,
        rel_brightness: Option<f32>,
    ) -> Result<()> {
        let light = self.get_light_by_name(&light_name).await?;

        return self.toggle_light(light, brightness, rel_brightness).await;
    }

    async fn set_light_action(
        &self,
        light_name: String,
        brightness: Option<f32>,
        rel_brightness: Option<f32>,
    ) -> Result<()> {
        let light = self.get_light_by_name(&light_name).await?;

        return self.set_light(light, brightness, rel_brightness).await;
    }

    async fn toggle_light(
        &self,
        mut light: Light,
        brightness: Option<f32>,
        rel_brightness: Option<f32>,
    ) -> Result<()> {
        // turn light off if it is on, otherwise turn it on then set the brightness
        if light.on {
            return Ok(light.switch(false).await?);
        }

        return self.set_light(light, brightness, rel_brightness).await;
    }

    async fn set_light(
        &self,
        mut light: Light,
        brightness_option: Option<f32>,
        rel_brightness_option: Option<f32>,
    ) -> Result<()> {
        // when brightness and rel_brightness is none, just turn off the light
        // otherwise check brightness:
        //   if it is 0, and turn off the light
        //   otherwise, turn on the light, then set the brightness
        // then check rel_brightness
        // setting brightness first results in: device (light) is "soft off", command (.dimming.brightness) may not have effect
        if rel_brightness_option.is_none() && brightness_option.is_none() {
            return Ok(light.switch(false).await?);
        }

        let brightness = match brightness_option {
            Some(b) => b,
            None => {
                self.determine_rel_brightness_val(&light, rel_brightness_option)
                    .await
            }
        };

        if brightness == 0.0 {
            return Ok(light.switch(false).await?);
        }

        light.switch(true).await?;
        light.dimm(brightness).await?;

        Ok(())
    }

    async fn determine_rel_brightness_val(
        &self,
        light: &Light,
        rel_brightness_option: Option<f32>,
    ) -> f32 {
        // default to 0, aka do nothing
        let rel_brightness = rel_brightness_option.unwrap_or(0.0);
        // not sure what to do here...
        let current_brightness = light.brightness.unwrap_or(0.0);
        let desired_brightness = current_brightness + rel_brightness;
        return desired_brightness.min(100.0).max(0.0);
    }

    async fn toggle_room_action(
        &self,
        room_name: String,
        brightness: Option<f32>,
        rel_brightness: Option<f32>,
    ) -> Result<()> {
        let light = self.get_room_light_by_name(&room_name).await?;

        return self.toggle_light(light, brightness, rel_brightness).await;
    }

    async fn set_room_action(
        &self,
        room_name: String,
        brightness: Option<f32>,
        rel_brightness: Option<f32>,
    ) -> Result<()> {
        let light = self.get_room_light_by_name(&room_name).await?;

        return self.set_light(light, brightness, rel_brightness).await;
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
            Actions::Toggle(toggle_action) => {
                if let Some(light_name) = toggle_action.light {
                    return Ok(self
                        .toggle_light_action(
                            light_name,
                            toggle_action.brightness,
                            toggle_action.rel_brightness,
                        )
                        .await?);
                }
                if let Some(room_name) = toggle_action.room {
                    return Ok(self
                        .toggle_room_action(
                            room_name,
                            toggle_action.brightness,
                            toggle_action.rel_brightness,
                        )
                        .await?);
                }

                return Err(anyhow!("Either light or room options must be set"));
            }
            Actions::Set(set_action) => {
                if let Some(light_name) = set_action.light {
                    return Ok(self
                        .set_light_action(
                            light_name,
                            set_action.brightness,
                            set_action.rel_brightness,
                        )
                        .await?);
                }
                if let Some(room_name) = set_action.room {
                    return Ok(self
                        .set_room_action(
                            room_name,
                            set_action.brightness,
                            set_action.rel_brightness,
                        )
                        .await?);
                }

                return Err(anyhow!("Either light or room options must be set"));
            }
        };
    }
}
