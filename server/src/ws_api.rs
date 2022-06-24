use crate::profiles;
use crate::Config;
use anyhow::{anyhow, Result};
use core::types::{ExecuteActionReq, ProfileButtonPressed, SetButtonUI, WsActions};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use image::{self, Pixel};
use std::collections::HashMap;
use std::io::{self, Cursor};
use std::sync::Arc;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::sync::RwLock;
use warp::ws::{Message, WebSocket};

pub struct Client {
    pub profile: String,
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
    let (tx, mut rx) = ws.split();

    let (profile_sync_tx, profile_sync_rx) = mpsc::unbounded_channel::<()>();

    let set_button_for_profile_join = tokio::spawn(set_button_for_profile(
        config.clone(),
        id,
        clients.clone(),
        tx,
        profile_sync_rx,
    ));

    profile_sync_tx.send(()).unwrap();

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
        let mut p: ProfileButtonPressed = serde_json::from_str(&msg_str).unwrap();
        if p.profile.is_none() {
            p.profile = Some(clients.read().await.get(&id).unwrap().profile.to_string())
        }

        println!("{:?}", &p);
        // p.profile = p.profile.unwrap_or()
        crate::rest_api::handle_button_pressed_action(
            p,
            event_processor.clone(),
            config.clone(),
            Some(id),
        )
        .await
        .unwrap();

        // todo: this is pretty silly, since every button press will trigger a full ui resyn, really
        // this should be smart and only resync if there are changes
        profile_sync_tx.send(()).unwrap();
    }
    set_button_for_profile_join.await.unwrap();

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

async fn set_button_for_profile(
    config: Arc<Config>,
    id: uuid::Uuid,
    clients: Clients,
    mut tx: SplitSink<WebSocket, Message>,
    mut profile_sync_rx: mpsc::UnboundedReceiver<()>,
) {
    while let Some(_) = profile_sync_rx.recv().await {
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
                    let mut loaded_image = image::open(&image).unwrap();

                    // apply background if color is also set
                    // steal this from the streamdeck library, to avoid it as a dependency for the api
                    if let Some(color) = &button_state.color {
                        let (r, g, b) = hex_color_components_from_str(&color).unwrap();
                        let rgba = loaded_image.as_mut_rgba8().unwrap();

                        let mut r = image::Rgba([r, g, b, 0]);
                        for p in rgba.pixels_mut() {
                            r.0[3] = 255 - p.0[3];

                            p.blend(&r);
                        }
                    }

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
    }
}

// steal this from the streamdeck library, to avoid it as a dependency for the api
fn hex_color_components_from_str(s: &str) -> Result<(u8, u8, u8)> {
    if s.len() != 6 && s.len() != 8 {
        return Err(anyhow!("Expected colour in the hex form: RRGGBB"));
    }

    let r = u8::from_str_radix(&s[0..2], 16).map_err(|e| anyhow!("int parsing error: {}", e))?;
    let g = u8::from_str_radix(&s[2..4], 16).map_err(|e| anyhow!("int parsing error: {}", e))?;
    let b = u8::from_str_radix(&s[4..6], 16).map_err(|e| anyhow!("int parsing error: {}", e))?;

    Ok((r, g, b))
}
