use anyhow::{anyhow, Result};
use integrations::Integration;
use sdc_core::types::{Actions, ExecuteActionReq, Profiles};
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::task::JoinHandle;
use warp::ws;

// mod integrations;
mod profiles;
mod rest_api;
mod ws_api;

const ACTION_SPLIT_CHARS: [char; 2] = [':', ':'];

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Config {
    atv_api_endpoint: String,
    devices: Vec<integrations::airplay::Device>,
    profiles: Profiles,
}

#[tokio::main]
async fn main() {
    let config = read_config(
        &env::var("STREAM_DECK_CONTROLLER_CONFIG").unwrap_or("./config.yaml".to_string()),
    )
    .expect("failed to read config file");
    println!("config: {:?}", config);
    let config_ref = Arc::new(config);

    let ws_clients = ws_api::Clients::default();
    let image_cache = ws_api::ImageCache::default();
    tokio::task::spawn(populat_image_cache(config_ref.clone(), image_cache.clone()));

    let (integration_manager, integration_manager_tx) =
        IntegrationManager::new(ws_clients.clone(), &config_ref).await;
    let manager_handle = start_integration_manager(integration_manager);

    let api_service = rest_api::start_rest_api(
        config_ref,
        integration_manager_tx,
        ws_clients.clone(),
        image_cache.clone(),
    );

    tokio::task::spawn(ws_api::ping_ws_clients(ws_clients.clone()));

    api_service.await;
    manager_handle.await.unwrap();
}

fn read_config(filepath: &str) -> Result<Config> {
    let file_contents = std::fs::read_to_string(filepath)?;

    let map: Config = serde_yaml::from_str(&file_contents)?;
    Ok(map)
}

async fn populat_image_cache(config_ref: Arc<Config>, image_cache: ws_api::ImageCache) {
    for profile in &config_ref.as_ref().profiles {
        for button in &profile.buttons {
            if let Some(states) = &button.states {
                for state in states {
                    if let Some(image) = &state.image {
                        ws_api::get_image(&image, &state, &image_cache).await;
                    }
                }
            }
        }
    }
}

struct IntegrationManager {
    integrations: HashMap<String, Box<dyn Integration + Send + Sync>>,
    rx: Receiver<ExecuteActionReq>,
    ws_clients: ws_api::Clients,
}

impl IntegrationManager {
    async fn new(
        ws_clients: ws_api::Clients,
        config_ref: &Arc<Config>,
    ) -> (IntegrationManager, Sender<ExecuteActionReq>) {
        let hue_integration = integrations::hue::Integration::new().await;
        let http_integration = integrations::http::Integration::new();
        let airplay_integration = integrations::airplay::Integration::new(
            &config_ref.atv_api_endpoint,
            &config_ref.devices,
        );
        let (tx, rx) = mpsc::channel::<ExecuteActionReq>(32);

        let mut manager = IntegrationManager {
            integrations: HashMap::new(),
            rx: rx,
            ws_clients: ws_clients,
        };

        manager
            .integrations
            .insert("hue".to_string(), Box::new(hue_integration));

        manager
            .integrations
            .insert("airplay".to_string(), Box::new(airplay_integration));

        manager
            .integrations
            .insert("http".to_string(), Box::new(http_integration));

        return (manager, tx);
    }

    async fn execute_actions(
        &self,
        requestor_uuid: Option<uuid::Uuid>,
        actions: Actions,
    ) -> Result<()> {
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

            if integration_name == "profile" {
                if action_name != "set" {
                    return Err(anyhow!(
                        "unknown action for profile integration {}",
                        action_name
                    ));
                }

                let profile_value = &action
                    .options
                    .get("profile")
                    .expect("invalid profile selection");

                let r = match profile_value {
                    serde_json::Value::String(profile_name) => {
                        let u = requestor_uuid.unwrap();
                        let mut clients = self.ws_clients.write().await;
                        let client = clients.get_mut(&u).unwrap();
                        client.profile = profile_name.to_string();
                        Ok(())
                    }
                    _ => Ok(()),
                };

                return r;
            }

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
                .execute_actions(
                    execute_actions_req.requestor_uuid,
                    execute_actions_req.actions,
                )
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
