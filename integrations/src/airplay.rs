use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::{collections::HashMap, process::ExitStatus};
use tokio::process::Command;

use crate::integration;

// mayber use #[serde(untagged)] for this?
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "action")]
enum Actions {
    #[serde(rename = "command")]
    Command { device: String, command: String },
    #[serde(rename = "open_app")]
    OpenApp { device: String, identifer: String },
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct Device {
    name: String,
    identifier: String,
    credentials: Option<String>,
    // todo: make this an enum
    protocol: Option<String>,
}

pub struct Integration {
    binary: String,
    devices: HashMap<String, Device>,
}

impl Integration {
    pub fn new(devices: &Vec<Device>) -> Integration {
        let mut devices_map = HashMap::new();
        for device in devices {
            let mut device_copy = device.clone().to_owned();
            if let Some(creds) = device_copy.credentials {
                device_copy.credentials = Some(shellexpand::env(&creds).unwrap().to_string());
            }
            device_copy.identifier = shellexpand::env(&device_copy.identifier)
                .unwrap()
                .to_string();
            // device_copy.credentials = ;
            devices_map.insert(device.name.to_string(), device_copy);
        }

        return Integration {
            binary: "atvremote".to_string(),
            devices: devices_map,
        };
    }

    async fn run_atvremote_command(&self, device_name: String, command: String) -> Result<()> {
        let device = self.devices.get(&device_name).unwrap();
        let mut cmd = self.atvremote_command_for_device(&device).await;
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

        return Ok(());
    }

    async fn atvremote_command_for_device(&self, device: &Device) -> Command {
        let mut cmd = Command::new(self.binary.clone());
        let default_protocol = "airplay".to_string();
        let protocol = device
            .protocol
            .as_ref()
            .unwrap_or(&default_protocol)
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
    async fn execute_action(
        &self,
        _action: String,
        json_options: serde_json::value::Value,
    ) -> Result<()> {
        let options: Actions = serde_json::from_value(json_options).unwrap();

        return match options {
            Actions::Command { device, command } => {
                self.run_atvremote_command(device, command).await
            }
            Actions::OpenApp { device, identifer } => {
                self.run_atvremote_command(device, format!("launch_app={}", identifer))
                    .await
            }
        };
    }
}
