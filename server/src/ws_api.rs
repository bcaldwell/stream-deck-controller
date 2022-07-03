use crate::profiles;
use crate::Config;
use anyhow::{anyhow, Result};
use futures_util::FutureExt;
use futures_util::StreamExt;
use image::{self, Pixel};
use sdc_core::types::{ExecuteActionReq, ProfileButtonPressed, SetButtonUI, WsActions};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::RwLock;
use tokio::time::sleep;
use tokio_stream::wrappers::UnboundedReceiverStream;
use warp::ws::{Message, WebSocket};

const PING_INTERVAL_MIN: u64 = 15;

pub struct Client {
    pub uuid: uuid::Uuid,
    pub profile: String,
    pub sender: mpsc::UnboundedSender<std::result::Result<Message, warp::Error>>,
}
pub type Clients = Arc<RwLock<HashMap<uuid::Uuid, Client>>>;
pub type ImageCache = Arc<RwLock<HashMap<String, String>>>;

pub async fn ping_ws_clients(clients: Clients) {
    loop {
        ping_all_ws_clients(clients.clone()).await;
        // ping every 10 minutes
        sleep(std::time::Duration::from_secs(60 * PING_INTERVAL_MIN)).await;
    }
}

async fn ping_all_ws_clients(clients: Clients) {
    let lock = clients.read().await;
    for (_, client) in lock.iter() {
        // ignore the response, since its just a ping, doesn't really matter
        _ = client.sender.send(Ok(Message::ping("ping")));
    }
}

pub async fn ws_client_connected(
    ws: WebSocket,
    event_processor: mpsc::Sender<ExecuteActionReq>,
    config: Arc<Config>,
    clients: Clients,
    image_cache: ImageCache,
) {
    let id = uuid::Uuid::new_v4();

    let (client_ws_sender, mut rx) = ws.split();
    let (client_sender, client_rcv) = mpsc::unbounded_channel();

    let client_rcv = UnboundedReceiverStream::new(client_rcv);
    tokio::task::spawn(client_rcv.forward(client_ws_sender).map(|result| {
        if let Err(e) = result {
            eprintln!("error sending websocket msg: {}", e);
        }
    }));

    clients.write().await.insert(
        id,
        Client {
            uuid: id,
            profile: "default".to_string(),
            sender: client_sender,
        },
    );
    eprintln!("new websocket client: {}", id);

    // Split the socket into a sender and receive of messages.

    let (profile_sync_tx, profile_sync_rx) = mpsc::unbounded_channel::<()>();

    tokio::spawn(set_button_for_profile(
        config.clone(),
        id,
        clients.clone(),
        profile_sync_rx,
        image_cache,
    ));

    profile_sync_tx.send(()).unwrap();

    while let Some(result) = rx.next().await {
        let msg = match result {
            Ok(msg) => msg,
            Err(e) => {
                eprintln!("websocket error(uid={}): {}", id, e);
                break;
            }
        };

        if msg.is_ping() || msg.is_pong() {
            continue;
        }

        if !msg.is_text() {
            println!("unknown message type from {:?}, ignoring", id);
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
        println!("Sending profile");
        profile_sync_tx.send(()).unwrap();
    }
    client_disconnected(clients, id).await;
    // user_ws_rx stream will keep processing as long as the user stays
    // connected. Once they disconnect, then...
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
    mut profile_sync_rx: mpsc::UnboundedReceiver<()>,
    image_cache: ImageCache,
) {
    while let Some(_) = profile_sync_rx.recv().await {
        let start_time = std::time::SystemTime::now();
        let mut button_config = Vec::new();
        let profile = profiles::get_profile_by_name(
            &config.as_ref().profiles,
            clients.read().await.get(&id).unwrap().profile.to_string(),
        )
        .unwrap();

        for button in &profile.buttons {
            let button_state: &SetButtonUI = &button.states.as_ref().unwrap()[0];

            let image = match &button_state.image {
                Some(image) => get_image(image, button_state, &image_cache).await.ok(),
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

        println!(
            "time taken: {}",
            std::time::SystemTime::now()
                .duration_since(start_time)
                .unwrap()
                .as_micros()
        );
        send_ws_message(
            &id,
            clients.clone(),
            Message::text(serde_json::to_string(&msg).unwrap()),
        )
        .await
        .unwrap();
    }
}

async fn send_ws_message(
    id: &uuid::Uuid,
    clients: Clients,
    message: Message,
) -> Result<(), mpsc::error::SendError<Result<Message, warp::Error>>> {
    let locked = clients.read().await;
    match locked.get(&id) {
        Some(c) => return c.sender.send(Ok(message)),
        None => Ok(()),
    }
}

pub async fn get_image(
    image: &String,
    button_state: &SetButtonUI,
    image_cache: &Arc<RwLock<HashMap<String, String>>>,
) -> Result<String> {
    let cache_key = format!(
        "{}-{}",
        image,
        button_state.color.as_ref().unwrap_or(&"".to_string())
    );

    let cached_locked = image_cache.read().await;
    let cached_image = cached_locked.get(&cache_key);
    if cached_image.is_some() {
        let response = cached_image.unwrap().to_string();
        // need to force drop this so it doesn't block
        return Ok(response);
    }

    drop(cached_locked);
    let mut loaded_image = fetch_image(image).await?;

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
        .resize(100, 100, image::imageops::FilterType::Nearest)
        .write_to(&mut buffered_image, image::ImageOutputFormat::Png)
        .unwrap();

    let base64_encoded = base64::encode(&buffered_image.into_inner().unwrap());
    image_cache
        .write()
        .await
        .insert(cache_key, base64_encoded.to_string());
    Ok(base64_encoded)
}

async fn fetch_image(image: &String) -> Result<image::DynamicImage> {
    if image.starts_with("http") {
        return load_image_from_url(image).await;
    }

    let loaded_image = image::open(&image)?;
    Ok(loaded_image)
}

async fn load_image_from_url(image: &String) -> Result<image::DynamicImage> {
    let img_bytes = reqwest::get(image).await?.bytes().await?;
    let image = image::load_from_memory(&img_bytes)?;
    Ok(image)
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
