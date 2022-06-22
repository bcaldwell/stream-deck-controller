use anyhow::{anyhow, Result};
use core::integrations::integration::Integration;
use core::integrations::{self};
use core::types::{Actions, ExecuteActionReq, Profiles};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::task::JoinHandle;

// mod integrations;
mod profiles;
mod rest_api;
mod ws_api;

const ACTION_SPLIT_CHARS: [char; 2] = [':', ':'];

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Config {
    profiles: Profiles,
}

#[tokio::main]
async fn main() {
    let config = read_config("./config_sample.yaml").expect("failed to read config file");
    println!("config: {:?}", config);
    let config_ref = Arc::new(config);

    let (integration_manager, integration_manager_tx) = IntegrationManager::new().await;
    let manager_handle = start_integration_manager(integration_manager);

    let api_service = rest_api::start_rest_api(config_ref, integration_manager_tx);

    api_service.await;
    manager_handle.await.unwrap();
}

fn read_config(filepath: &str) -> Result<Config> {
    let file_contents = std::fs::read_to_string(filepath)?;

    let map: Config = serde_yaml::from_str(&file_contents)?;
    Ok(map)
}

struct IntegrationManager {
    integrations: HashMap<String, Box<dyn Integration + Send + Sync>>,
    rx: Receiver<ExecuteActionReq>,
}

impl IntegrationManager {
    async fn new() -> (IntegrationManager, Sender<ExecuteActionReq>) {
        let hue_integration = integrations::hue::Integration::new().await;
        let (tx, rx) = mpsc::channel::<ExecuteActionReq>(32);

        let mut manager = IntegrationManager {
            integrations: HashMap::new(),
            rx: rx,
        };

        manager
            .integrations
            .insert("hue".to_string(), Box::new(hue_integration));

        return (manager, tx);
    }

    async fn execute_actions(&self, actions: Actions) -> Result<()> {
        for action in actions {
            let split_index = action.action.find(ACTION_SPLIT_CHARS);
            let (integration_name, action_name) = match split_index {
                Some(i) => (
                    &action.action[..i],
                    &action.action[i + ACTION_SPLIT_CHARS.len()..],
                ),
                None => {
                    return Err(anyhow!(
                        "action {} was invalid, must contain separator.",
                        action.action
                    ))
                }
            };

            let mut options = action.options.clone();
            options["action"] = serde_json::Value::String(action_name.to_string());
            let integration_option = self.integrations.get(integration_name);

            match integration_option {
                Some(integration) => {
                    integration
                        .as_ref()
                        .execute_action(action_name.to_string(), options)
                        .await?;
                }
                None => return Err(anyhow!("unknown integration {}", integration_name)),
            }
        }

        Ok(())
    }
}

fn start_integration_manager(mut integration_manager: IntegrationManager) -> JoinHandle<()> {
    return tokio::spawn(async move {
        // Start receiving messages
        while let Some(execute_actions_req) = integration_manager.rx.recv().await {
            let response = match integration_manager
                .execute_actions(execute_actions_req.actions)
                .await
            {
                Ok(_) => "success".to_string(),
                Err(e) => {
                    let msg = format!("error executing request: {}", e);
                    println!("{}", msg);
                    msg
                }
            };
            // okay to eat this error, since that means the reciever is closed
            _ = execute_actions_req.tx.send(response.to_string());
        }

        return ();
    });
}
