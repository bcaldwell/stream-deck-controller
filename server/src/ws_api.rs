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
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::RwLock;
use tokio::time::sleep;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{error, info};
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
            error!("error sending websocket msg: {}", e);
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
    info!("new websocket client: {}", id);

    // Split the socket into a sender and receive of messages.

    let (profile_sync_tx, profile_sync_rx) = mpsc::unbounded_channel::<()>();
    let profile_sync_tx = Arc::new(profile_sync_tx);
    tokio::spawn(profile_sync_task(
        config.clone(),
        id,
        clients.clone(),
        profile_sync_rx,
        image_cache,
    ));

    match profile_sync_tx.send(()) {
        Ok(_) => (),
        Err(err) => error!(error=?err, uuid=?id, "failed to sending initial profile"),
    };

    while let Some(result) = rx.next().await {
        match handle_msg(
            id,
            clients.clone(),
            profile_sync_tx.clone(),
            event_processor.clone(),
            config.clone(),
            result,
        )
        .await
        {
            Ok(_) => (),
            Err(err) => error!(error=?err, uuid=?id, "error handling message"),
        }
    }
    client_disconnected(clients, id).await;
    // user_ws_rx stream will keep processing as long as the user stays
    // connected. Once they disconnect, then...
}

async fn handle_msg(
    id: uuid::Uuid,
    clients: Arc<RwLock<HashMap<uuid::Uuid, Client>>>,
    profile_sync_tx: Arc<UnboundedSender<()>>,
    event_processor: mpsc::Sender<ExecuteActionReq>,
    config: Arc<Config>,
    result: Result<Message, warp::Error>,
) -> Result<()> {
    let msg = match result {
        Ok(msg) => msg,
        Err(e) => {
            error!(error=?e, uuid=?id, "websocket error");
            return Err(anyhow::Error::msg(e));
        }
    };

    if msg.is_ping() || msg.is_pong() {
        return Ok(());
    }

    if !msg.is_text() {
        info!(?id, "unknown message type, ignoring");
        return Ok(());
    }

    let msg_str = String::from_utf8(msg.into_bytes().to_vec())?;
    let mut p: ProfileButtonPressed = serde_json::from_str(&msg_str)?;
    if p.profile.is_none() {
        p.profile = Some(
            clients
                .read()
                .await
                .get(&id)
                .ok_or_else(|| anyhow!("failed to find client"))?
                .profile
                .to_string(),
        )
    }

    info!("{:?}", &p);
    // p.profile = p.profile.unwrap_or()
    crate::rest_api::handle_button_pressed_action(
        p,
        event_processor.clone(),
        config.clone(),
        Some(id),
    )
    .await
    .map_err(|err| anyhow!("button pressed handler rejected the request: {:?}", err))?;

    // todo: this is pretty silly, since every button press will trigger a full ui resyn, really
    // this should be smart and only resync if there are changes
    info!("Sending profile");
    Ok(profile_sync_tx.send(())?)
}

async fn client_disconnected(clients: Clients, id: uuid::Uuid) {
    info!("websocket disconnected: {}", id);

    // Stream closed up, so remove from the user list
    clients.write().await.remove(&id);
}

async fn profile_sync_task(
    config: Arc<Config>,
    id: uuid::Uuid,
    clients: Clients,
    mut profile_sync_rx: mpsc::UnboundedReceiver<()>,
    image_cache: ImageCache,
) {
    while let Some(_) = profile_sync_rx.recv().await {
        match handle_profile_sync_request(&config, &clients, id, &image_cache).await {
            Ok(_) => (),
            Err(err) => {
                error!(error=?err, uuid=?id, "failed to sync profile")
            }
        }
    }
}

async fn handle_profile_sync_request(
    config: &Arc<Config>,
    clients: &Arc<RwLock<HashMap<uuid::Uuid, Client>>>,
    id: uuid::Uuid,
    image_cache: &Arc<RwLock<HashMap<String, String>>>,
) -> Result<()> {
    let mut button_config = Vec::new();
    let profile = profiles::get_profile_by_name(
        &config.as_ref().profiles,
        clients
            .read()
            .await
            .get(&id)
            .ok_or_else(|| anyhow!("failed to get client for id"))?
            .profile
            .to_string(),
    )
    .ok_or_else(|| anyhow!("failed to find profile for client"))?;

    for button in &profile.buttons {
        let button_state: &SetButtonUI = &button.states.as_ref().unwrap()[0];

        let image = match &button_state.image {
            Some(image) => {
                get_image(image, button_state, image_cache)
                    .await
                    // log error, because its getting eaten
                    .map_err(|err| {
                        error!(error=?err, image, "failed to get image, skipping");
                        err
                    })
                    .ok()
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
    let msg = match serde_json::to_string(&msg) {
        Ok(msg) => msg,
        Err(err) => {
            error!(error=?err, "failed to convert set button event to string, aborting");
            return Err(anyhow::Error::msg(err));
        }
    };
    let msg = Message::text(msg);
    info!(client=?id, profile=?profile.name, "sending set button event");
    match send_ws_message(&id, clients.clone(), msg).await {
        Ok(_) => (),
        Err(err) => error!(error =?err, client=?id, "failed to send button pressed message"),
    };
    Ok(())
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

    {
        let cached_locked = image_cache.read().await;
        let cached_image = cached_locked.get(&cache_key);
        match cached_image {
            Some(cached_image) => {
                return Ok(cached_image.to_string());
            }
            None => (),
        }
    }
    let mut loaded_image = fetch_image(image).await?;

    // apply background if color is also set
    // steal this from the streamdeck library, to avoid it as a dependency for the api
    info!(image=?image, color=?button_state.color, "loading image");
    if let Some(color) = &button_state.color {
        let (r, g, b) = hex_color_components_from_str(&color)
            .map_err(|err| anyhow!("unable to decoed hex color: {}", err))?;
        let rgba = loaded_image.as_mut_rgba8().ok_or_else(|| {
            anyhow!("unable to convert image to have transparent layer, is it a png?")
        })?;

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
        .map_err(|err| anyhow!("unable to write image to buffer: {}", err))?;

    let base64_encoded = base64::encode(
        &buffered_image
            .into_inner()
            .map_err(|err| anyhow!("unable convert buffered image: {}", err))?,
    );
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
