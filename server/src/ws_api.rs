use crate::profiles;
use crate::Config;
use core::types::{ExecuteActionReq, ProfileButtonPressed, SetButtonUI, WsActions};
use futures_util::{SinkExt, StreamExt};
use image::{self, imageops, DynamicImage, GenericImageView};
use std::collections::HashMap;
use std::io::{self, Cursor};
use std::sync::Arc;
use tokio::sync::mpsc::{self};
use tokio::sync::RwLock;
use warp::ws::{Message, WebSocket};

pub struct Client {
    profile: String,
}
pub type Clients = Arc<RwLock<HashMap<uuid::Uuid, Client>>>;

pub async fn ws_client_connected(
    ws: WebSocket,
    event_processor: mpsc::Sender<ExecuteActionReq>,
    config: Arc<Config>,
    clients: Clients,
) {
    let id = uuid::Uuid::new_v4();

    clients.write().await.insert(
        id,
        Client {
            profile: "default".to_string(),
        },
    );

    eprintln!("new websocket client: {}", id);

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

    let mut button_config = Vec::new();
    let profile = profiles::get_profile_by_name(
        &config.as_ref().profiles,
        clients.read().await.get(&id).unwrap().profile.to_string(),
    )
    .unwrap();

    for button in &profile.buttons {
        let button_state: &SetButtonUI = &button.states.as_ref().unwrap()[0];

        let image = match &button_state.image {
            Some(image) => {
                let loaded_image = image::open(&image).unwrap();
                let mut buffered_image = std::io::BufWriter::new(Vec::new());
                loaded_image
                    .write_to(&mut buffered_image, image::ImageOutputFormat::Png)
                    .unwrap();

                Some(base64::encode(&buffered_image.into_inner().unwrap()))
            }
            None => None,
        };

        button_config.push(SetButtonUI {
            image: image,
            color: button_state.color.clone(),
        });
    }

    let msg = WsActions::SetButtons {
        buttons: button_config,
    };

    tx.send(Message::text(serde_json::to_string(&msg).unwrap()))
        .await
        .unwrap();
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

        if !msg.is_text() {
            continue;
        }

        let msg_str = String::from_utf8(msg.into_bytes().to_vec()).unwrap();
        let p: ProfileButtonPressed = serde_json::from_str(&msg_str).unwrap();

        println!("{:?}", &p);
        // p.profile = p.profile.unwrap_or()
        crate::rest_api::handle_button_pressed_action(p, event_processor.clone(), config.clone())
            .await;
    }

    // user_ws_rx stream will keep processing as long as the user stays
    // connected. Once they disconnect, then...
    client_disconnected(clients, id).await;
}

// async fn handle_msg(result: Result<Message, Error>) -> Result<()> {

// }

async fn client_disconnected(clients: Clients, id: uuid::Uuid) {
    eprintln!("websocket disconnected: {}", id);

    // Stream closed up, so remove from the user list
    clients.write().await.remove(&id);
}
