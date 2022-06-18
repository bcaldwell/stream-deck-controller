use crate::integrations::integration::{self, Integration};
use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use warp::ws::WebSocket;
use warp::{http, Filter};

mod integrations;

type Actions = Vec<integration::Action>;

const ACTION_SPLIT_CHARS: [char; 2] = [':', ':'];

#[tokio::main]
async fn main() {
    let (tx, mut rx) = mpsc::channel::<Actions>(32);

    let integration_manager = IntegrationManager::new().await;

    let manager = tokio::spawn(async move {
        // Start receiving messages
        while let Some(actions) = rx.recv().await {
            match integration_manager.execute_actions(actions).await {
                Ok(_) => (),
                Err(e) => println!("err: {}", e),
            };
        }
    });

    let event_processor = warp::any().map(move || tx.clone());

    // GET /v1/ws -> websocket upgrade
    let ws_endpoint = warp::path("ws")
        // The `ws()` filter will prepare Websocket handshake...
        .and(warp::ws())
        .and(event_processor.clone())
        .map(|ws: warp::ws::Ws, event_processor| {
            // This will call our function if the handshake succeeds.
            ws.on_upgrade(move |socket| user_connected(socket, event_processor))
        });

    // POST /v1/actions/execute
    let execute_action_endpoint = warp::post()
        .and(warp::path("execute"))
        .and(warp::path::end())
        .and(warp::body::json())
        .and(event_processor.clone())
        .and_then(handle_execute_action);

    let actions_endpoint = warp::path("actions").and(execute_action_endpoint);
    let v1_endpoint = warp::path("v1").and(ws_endpoint.or(actions_endpoint));

    // GET / -> index html
    let index_endpoint = warp::path::end().map(|| warp::reply::reply());

    let routes = index_endpoint.or(v1_endpoint);

    warp::serve(routes).run(([127, 0, 0, 1], 8000)).await;
    manager.await.unwrap();
}

async fn handle_execute_action(
    action: Actions,
    event_processor: mpsc::Sender<Actions>,
) -> Result<impl warp::Reply, warp::Rejection> {
    println!("{:?}", action);

    event_processor.send(action).await.unwrap();
    Ok(warp::reply::with_status("accepted", http::StatusCode::OK))
}

async fn user_connected(ws: WebSocket, event_processor: mpsc::Sender<Actions>) {
    // Use a counter to assign a new unique ID for this user.
    // let my_id = NEXT_USER_ID.fetch_add(1, Ordering::Relaxed);

    // eprintln!("new chat user: {}", my_id);

    // Split the socket into a sender and receive of messages.
    let (mut tx, mut rx) = ws.split();

    // Use an unbounded channel to handle buffering and flushing of messages
    // to the websocket...
    // let mut rx = UnboundedReceiverStream::new(rx);

    // tokio::task::spawn(async move {
    //     while let Some(message) = rx.next().await {
    //         user_ws_tx
    //             .send(message)
    //             .unwrap_or_else(|e| {
    //                 eprintln!("websocket send error: {}", e);
    //             })
    //             .await;
    //     }
    // });

    // Save the sender in our list of connected users.
    // users.write().await.insert(my_id, tx);

    // Return a `Future` that is basically a state machine managing
    // this specific user's connection.

    // Every time the user sends a message, broadcast it to
    // all other users...
    while let Some(result) = rx.next().await {
        let msg = match result {
            Ok(msg) => msg,
            Err(e) => {
                eprintln!("websocket error(uid={}): {}", "my_id", e);
                break;
            }
        };
        println!("{:?}", msg);
        event_processor.send(Vec::new()).await.unwrap();
    }

    // user_ws_rx stream will keep processing as long as the user stays
    // connected. Once they disconnect, then...
    user_disconnected(0).await;
}

async fn user_disconnected(my_id: usize) {
    eprintln!("good bye user: {}", my_id);

    // Stream closed up, so remove from the user list
    // users.write().await.remove(&my_id);
}

struct IntegrationManager {
    integrations: HashMap<String, Box<dyn Integration + Send + Sync>>,
}

impl IntegrationManager {
    async fn new() -> IntegrationManager {
        let hue_integration = integrations::hue::Integration::new().await;

        let mut manager = IntegrationManager {
            integrations: HashMap::new(),
        };
        manager
            .integrations
            .insert("hue".to_string(), Box::new(hue_integration));

        return manager;
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
