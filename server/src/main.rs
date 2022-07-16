use anyhow::{anyhow, Result};
use integrations::{Integration, IntegrationEnum, IntegrationsConfigurationEnum, IntoIntegration};
use sdc_core::types::{Actions, ExecuteActionReq, Profiles};
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::task::JoinHandle;
use tracing::{error, info};
use tracing_subscriber;

mod profiles;
mod rest_api;
mod ws_api;

const ACTION_SPLIT_CHARS: [char; 2] = [':', ':'];

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Config {
    integrations: Vec<IntegrationsConfigurationEnum>,
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
        IntegrationManager::new(ws_clients.clone(), &config_ref)
            .await
            .unwrap_or_else(|err| {
                error!(error = ?err, "failed to create integration manager, cannot recover");
                std::process::exit(1);
            });
    let manager_handle = start_integration_manager(integration_manager);

    let api_service = rest_api::start_rest_api(
        config_ref,
        integration_manager_tx,
        ws_clients.clone(),
        image_cache.clone(),
    );

    tokio::task::spawn(ws_api::ping_ws_clients(ws_clients.clone()));

    let (_, manager_handle) = tokio::join!(api_service, manager_handle);
    match manager_handle {
        Ok(_) => (),
        Err(err) => error!(error = ?err, "failure in integration manager"),
    }
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
                        match ws_api::get_image(&image, &state, &image_cache).await {
                            Ok(_) => (),
                            Err(err) => error!(error=?err, "error populating image cache"),
                        };
                    }
                }
            }
        }
    }
}

struct IntegrationManager {
    integrations: HashMap<String, IntegrationEnum>,
    rx: Receiver<ExecuteActionReq>,
    ws_clients: ws_api::Clients,
}

impl IntegrationManager {
    async fn new(
        ws_clients: ws_api::Clients,
        config_ref: &Arc<Config>,
    ) -> Result<(IntegrationManager, Sender<ExecuteActionReq>)> {
        let (tx, rx) = mpsc::channel::<ExecuteActionReq>(32);

        let mut manager = IntegrationManager {
            integrations: HashMap::new(),
            rx: rx,
            ws_clients: ws_clients,
        };

        for integration in &config_ref.as_ref().integrations {
            info!(
                integration = integration.to_string(),
                "setting up integration"
            );
            let i = integration.into_integration().await.map_err(|err| {
                error!(?integration, error = ?err, "failed to create integration");
                anyhow!(
                    "failed to create integration {:?} with {:?}",
                    integration.to_string(),
                    err
                )
            })?;

            manager.add_integration(i);
        }

        info!(
            "enabled integration names: {:?}",
            manager.integrations.keys()
        );

        return Ok((manager, tx));
    }

    fn add_integration(&mut self, integration: IntegrationEnum) {
        self.integrations
            .insert(integration.name().to_string(), integration);
    }

    async fn execute_actions(
        &self,
        requestor_uuid: Option<uuid::Uuid>,
        actions: Actions,
    ) -> Result<()> {
        for action in actions {
            let requestor_uuid = requestor_uuid
                .ok_or_else(|| anyhow!("recieved excute actions request for unknown requestor"))?;

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
                        let mut clients = self.ws_clients.write().await;
                        let client = clients.get_mut(&requestor_uuid).ok_or_else(|| {
                            anyhow!("unable to get websocket client for {:?}", requestor_uuid)
                        })?;
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
