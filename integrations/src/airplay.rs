use crate::integration;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use tokio::process::Command;

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub enum Protocol {
    #[serde(rename = "companion")]
    Companion,
    #[serde(rename = "airplay")]
    AirPlay,
    #[serde(rename = "raop")]
    RAOP,
}

impl Protocol {
    fn as_str(&self) -> &'static str {
        match self {
            Protocol::Companion => "companion",
            Protocol::AirPlay => "airplay",
            Protocol::RAOP => "raop",
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct Device {
    name: String,
    identifier: String,
    credentials: Option<String>,
    // todo: make this an enum
    protocol: Option<Protocol>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct IntegrationConfig {
    api_endpoint: String,
    devices: Vec<Device>,
}

#[async_trait]
impl integration::IntegrationConfig for IntegrationConfig {
    async fn to_integration(&self, name: Option<String>) -> integration::IntegrationResult {
        let i = Integration::new(
            name.unwrap_or("airplay".to_string()),
            &self.api_endpoint,
            &self.devices,
        )?;
        return Ok(Box::new(i));
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CommandAction {
    device: String,
    command: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct OpenAppAction {
    device: String,
    identifier: String,
}

// mayber use #[serde(untagged)] for this?
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "action")]
enum Actions {
    #[serde(rename = "command")]
    Command(CommandAction),
    #[serde(rename = "open_app")]
    OpenApp(OpenAppAction),
}

pub struct Integration {
    name: String,
    binary: Option<String>,
    atv_api_endpoint: Option<String>,
    devices: HashMap<String, Device>,
}

impl Integration {
    pub fn new(name: String, atv_api_endpoint: &str, devices: &Vec<Device>) -> Result<Integration> {
        let mut devices_map = HashMap::new();
        for device in devices {
            let mut device_copy = device.clone().to_owned();
            if let Some(creds) = device_copy.credentials {
                device_copy.credentials = Some(shellexpand::env(&creds)?.to_string());
            }
            device_copy.identifier = shellexpand::env(&device_copy.identifier)?.to_string();
            // device_copy.credentials = ;
            devices_map.insert(device.name.to_string(), device_copy);
        }

        return Ok(Integration {
            name: name,
            binary: Some("atvremote".to_string()),
            atv_api_endpoint: Some(atv_api_endpoint.to_string()),
            devices: devices_map,
        });
    }

    async fn run_atvremote_command(&self, options: Actions) -> Result<()> {
        let api_result = match &self.atv_api_endpoint {
            Some(atv_api_endpoint) => {
                self.run_atvremote_command_api(&options, atv_api_endpoint.to_string())
                    .await
            }
            None => Err(anyhow!("airplay: both binary and api are disabled")),
        };
        println!("{:?}", &api_result);
        if api_result.is_ok() {
            return api_result;
        }

        if let Some(binary) = &self.binary {
            return self
                .run_atvremote_command_binary(&options, binary.to_string())
                .await;
        }

        return api_result;
    }

    async fn run_atvremote_command_api(
        &self,
        options: &Actions,
        atv_api_endpoint: String,
    ) -> Result<()> {
        let (device, endpoint) = self.get_url_for_action(&options);
        let client = reqwest::Client::new();
        let mut r = client.get(format!("{}/{}", atv_api_endpoint, endpoint));

        let default_protocol = Protocol::AirPlay;
        let protocol = device
            .protocol
            .as_ref()
            .unwrap_or(&default_protocol)
            .as_str()
            .to_lowercase();

        if let Some(credentials) = &device.credentials {
            println!("adding creds");

            // cmd.env("CREDENTIALS", credentials);
            r = r.header("auto-connect", "true");
            r = r.header(format!("{}-credentials", &protocol), credentials);
        }

        let response = r.send().await?;
        if response.status() != 200 {
            return Err(anyhow!(
                "failed to run airplay command: {:?}",
                response.text().await.unwrap_or("".to_string())
            ));
        }

        Ok(())
    }

    fn get_url_for_action(&self, action: &Actions) -> (&Device, String) {
        match action {
            Actions::Command(command_action) => {
                let device = self.devices.get(&command_action.device).unwrap();
                (
                    device,
                    format!("command/{}/{}", device.identifier, command_action.command).to_string(),
                )
            }
            Actions::OpenApp(open_action) => {
                let device = self.devices.get(&open_action.device).unwrap();
                (
                    device,
                    format!(
                        "apps/{}/open/{}",
                        device.identifier, &open_action.identifier
                    )
                    .to_string(),
                )
            }
        }
    }

    async fn run_atvremote_command_binary(&self, options: &Actions, binary: String) -> Result<()> {
        let (device_name, command) = self.get_command_for_action(&options);
        let device = self.devices.get(&device_name).unwrap();
        let mut cmd = self.atvremote_command_for_device(&device, binary).await;
        cmd.arg(command);

        let output = cmd
            .output()
            .await
            .map_err(|e| anyhow!("airplay command failed: {}", e))?;

        if output.status.code() != Some(0) {
            return Err(anyhow!(
                "airplay command returned non-zero exit ({}): {} {}",
                output.status,
                String::from_utf8(output.stdout).unwrap(),
                String::from_utf8(output.stderr).unwrap(),
            ));
        }

        Ok(())
    }

    fn get_command_for_action(&self, action: &Actions) -> (String, String) {
        match action {
            Actions::Command(command_action) => (
                command_action.device.to_owned(),
                command_action.command.to_owned(),
            ),
            Actions::OpenApp(open_app_action) => (
                open_app_action.device.to_owned(),
                format!("launch_app={}", open_app_action.identifier),
            ),
        }
    }

    async fn atvremote_command_for_device(&self, device: &Device, binary: String) -> Command {
        let mut cmd = Command::new(binary);
        let default_protocol = Protocol::AirPlay;
        let protocol = device
            .protocol
            .as_ref()
            .unwrap_or(&default_protocol)
            .as_str()
            .to_lowercase();

        cmd.arg("-i").arg(&device.identifier);
        cmd.arg("--protocol").arg(&protocol);

        if let Some(credentials) = &device.credentials {
            // cmd.env("CREDENTIALS", credentials);
            cmd.arg(format!("--{}-credentials", &protocol))
                .arg(credentials);
        }

        return cmd;
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

        self.run_atvremote_command(options).await
    }
}
