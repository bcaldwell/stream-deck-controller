use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;

use crate::homebridge::{Homebridge, HomebridgeDevice};
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
    async fn into_integration(&self, name: Option<String>) -> integration::IntegrationResult {
        let username = shellexpand::env(&self.username)?.to_string();
        let password = shellexpand::env(&self.password)?.to_string();

        let i = Integration::new(
            name.unwrap_or(DEFAULT_NAME.to_string()),
            &self.api_endpoint,
            &username,
            &password,
        )
        .await?;
        return Ok(i.into());
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct BaseAction {
    uuid: Option<String>,
    device: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct BrightnessAction {
    brightness: Option<f32>,
    rel_brightness: Option<f32>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ToggleAction {
    #[serde(flatten)]
    base_action: BaseAction,
    #[serde(flatten)]
    brightness_action: BrightnessAction,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct SetAction {
    #[serde(flatten)]
    base_action: BaseAction,
    #[serde(flatten)]
    brightness_action: BrightnessAction,
    on: Option<bool>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "action")]
enum Actions {
    #[serde(rename = "toggle")]
    Toggle(ToggleAction),
    #[serde(rename = "set")]
    Set(SetAction),
}

pub struct Integration {
    name: String,
    homebridge: Homebridge,
    device_name_to_id: HashMap<String, String>,
}

impl Integration {
    pub async fn new(
        name: String,
        endpoint: &str,
        username: &str,
        password: &str,
    ) -> Result<Integration> {
        let homebridge = Homebridge::new(endpoint, username, password).await?;

        let devices = homebridge.devices().await?;
        let mut integration = Integration {
            name: name,
            homebridge: homebridge,
            device_name_to_id: HashMap::new(),
        };

        integration.device_name_to_id.clear();

        for device in devices {
            integration
                .device_name_to_id
                .insert(device.name(), device.unique_id());
        }

        return Ok(integration);
    }

    async fn get_device_by_name_or_id(
        &self,
        id: &Option<String>,
        name: &Option<String>,
    ) -> Result<HomebridgeDevice> {
        match name {
            Some(name) => return self.get_device_by_name(&name).await,
            None => (),
        };

        match id {
            Some(id) => return self.get_device_by_id(&id).await,
            None => (),
        };

        Err(anyhow!("either uuid or device fields must be set"))
    }

    async fn get_device_by_id(&self, id: &str) -> Result<HomebridgeDevice> {
        return self.homebridge.device_by_id(id.to_string()).await;
    }

    async fn get_device_by_name(&self, name: &str) -> Result<HomebridgeDevice> {
        let id = self
            .device_name_to_id
            .get(name)
            .context(format!("unable to find device named: {}", name))?;

        return self.homebridge.device_by_id(id.to_string()).await;
    }

    async fn set_device(&self, device: &mut HomebridgeDevice, action: &SetAction) -> Result<()> {
        // set brightness action if anything is set there
        if action.brightness_action.brightness.is_some()
            || action.brightness_action.rel_brightness.is_some()
        {
            match action.on {
                None | Some(true) => {
                    return self.set_dimmable(device, &action.brightness_action).await
                }
                Some(false) => return device.switch(false).await,
            }
        }

        match action.on {
            Some(true) => device.switch(true).await,
            None | Some(false) => device.switch(false).await,
        }
    }

    async fn toggle_device(
        &self,
        device: &mut HomebridgeDevice,
        on: bool,
        action: &ToggleAction,
    ) -> Result<()> {
        if on {
            return device.switch(false).await;
        }

        return match device.brightness() {
            Some(_) => self.set_dimmable(device, &action.brightness_action).await,
            None => device.switch(true).await,
        };
    }

    async fn set_dimmable(
        &self,
        device: &mut HomebridgeDevice,
        action: &BrightnessAction,
    ) -> Result<()> {
        let brightness = match device.brightness() {
            Some(brightness) => Some(brightness as f32),
            None => None,
        };
        let device_state = crate::utils::light_utils::calc_light_state(
            brightness,
            action.brightness,
            action.rel_brightness,
        );

        device.switch(device_state.on).await?;

        match device_state.brightness {
            Some(brightness) => device.dimm(brightness as u64).await?,
            None => (),
        };

        return Ok(());
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
        let options: Actions = serde_json::from_value(json_options).map_err(|err| {
            anyhow!(
                "unable to convert action to {} action: {:?}",
                self.name(),
                err
            )
        })?;
        match options {
            Actions::Toggle(action) => {
                let mut device = self
                    .get_device_by_name_or_id(&action.base_action.uuid, &action.base_action.device)
                    .await?;

                return match device.on() {
                    Some(on) => self.toggle_device(&mut device, on, &action).await,
                    None => Err(anyhow!("device {:?} does not support on", device.name())),
                };
            }
            Actions::Set(action) => {
                let mut device = self
                    .get_device_by_name_or_id(&action.base_action.uuid, &action.base_action.device)
                    .await?;

                return self.set_device(&mut device, &action).await;
            }
        }
    }
}
