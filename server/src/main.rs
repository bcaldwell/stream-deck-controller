use anyhow::{anyhow, Result};
use integrations::Integration;
use sdc_core::types::{Actions, ExecuteActionReq, Profiles};
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::task::JoinHandle;
use tracing::info;
use tracing_subscriber;

mod profiles;
mod rest_api;
mod ws_api;

const ACTION_SPLIT_CHARS: [char; 2] = [':', ':'];

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct IntegrationConfiguration<T: integrations::IntegrationConfig> {
    name: Option<String>,
    #[serde(flatten)]
    options: T,
}

impl<T: integrations::IntegrationConfig> IntegrationConfiguration<T> {
    async fn as_integration(&self) -> integrations::IntegrationResult {
        return self.options.to_integration(self.name.clone()).await;
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
enum IntegrationsConfiguration {
    Hue(IntegrationConfiguration<integrations::hue::IntegrationConfig>),
    Homebridge(IntegrationConfiguration<integrations::integrations::homebridge::IntegrationConfig>),
    Airplay(IntegrationConfiguration<integrations::airplay::IntegrationConfig>),
    Http(IntegrationConfiguration<integrations::http::IntegrationConfig>),
}

impl IntegrationsConfiguration {
    async fn as_integration(&self) -> integrations::IntegrationResult {
        match self {
            IntegrationsConfiguration::Hue(c) => c.as_integration().await,
            IntegrationsConfiguration::Homebridge(c) => c.as_integration().await,
            IntegrationsConfiguration::Airplay(c) => c.as_integration().await,
            IntegrationsConfiguration::Http(c) => c.as_integration().await,
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Config {
    integrations: Vec<IntegrationsConfiguration>,
    profiles: Profiles,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let config_file =
        &env::var("STREAM_DECK_CONTROLLER_CONFIG").unwrap_or("./config.yaml".to_string());
    let config = read_config(config_file).expect("failed to read config file");
    info!(config_file, config = ?config, "parsed config file");
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
                        // eat this error, we will try again later when the client requests the image
                        _ = ws_api::get_image(&image, &state, &image_cache).await;
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
        let (tx, rx) = mpsc::channel::<ExecuteActionReq>(32);

        let mut manager = IntegrationManager {
            integrations: HashMap::new(),
            rx: rx,
            ws_clients: ws_clients,
        };

        for integration in &config_ref.as_ref().integrations {
            let i = integration.as_integration().await.unwrap();
            manager.add_integration(i);
        }

        info!(
            "enabled integration names: {:?}",
            manager.integrations.keys()
        );

        return (manager, tx);
    }

    fn add_integration(&mut self, integration: Box<dyn Integration + Send + Sync>) {
        self.integrations
            .insert(integration.name().to_string(), integration);
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
                    info!("{}", msg);
                    msg
                }
            };
            // okay to eat this error, since that means the reciever is closed
            _ = execute_actions_req.tx.send(response.to_string());
        }

        return ();
    });
}
