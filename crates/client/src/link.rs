//! The server connection: connect, say hello, forward messages, reconnect with backoff.
//! Runs as a task next to the deck loop in `session.rs` and talks to it over channels.

use std::future::Future;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use futures_util::{SinkExt, StreamExt};
use sdc_protocol::{ClientMessage, DeviceInfo, ImageFrame, ServerMessage, PROTOCOL_VERSION};
use tokio::sync::mpsc;
use tokio::time::{interval, timeout, Instant};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tracing::{debug, error, info, warn};

use crate::session::Config;

/// If nothing (including pongs to our pings) arrives for this long, the link is dead.
const IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const PING_EVERY: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MIN_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(15);

/// What the link tells the deck loop.
#[derive(Debug)]
pub enum LinkEvent {
    Up,
    Down,
    Brightness(u8),
    /// A key image, a JPEG in the deck's native format.
    Image(u8, Vec<u8>),
}

/// Connect and reconnect forever. `keys` carries what the deck sends to the server; anything
/// sent while not connected is dropped.
pub async fn run(
    cfg: Config,
    device: DeviceInfo,
    events: mpsc::Sender<LinkEvent>,
    mut keys: mpsc::UnboundedReceiver<ClientMessage>,
) {
    let mut backoff = MIN_BACKOFF;
    loop {
        info!(server = %cfg.server, "connecting");
        let started = Instant::now();
        let res = serve(&cfg, &device, &events, &mut keys).await;
        let _ = events.send(LinkEvent::Down).await;
        match res {
            Ok(why) => warn!("disconnected from server: {why}"),
            Err(e) => warn!("server connection failed: {e:#}"),
        }
        // If we were up for a while, start the backoff over.
        if started.elapsed() > Duration::from_secs(30) {
            backoff = MIN_BACKOFF;
        }
        info!(?backoff, "retrying");
        drop_keys(&mut keys, tokio::time::sleep(backoff)).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Waits for `fut`, throwing away key events meanwhile: presses made while disconnected
/// must not be replayed once the link is back.
async fn drop_keys<T>(
    keys: &mut mpsc::UnboundedReceiver<ClientMessage>,
    fut: impl Future<Output = T>,
) -> T {
    tokio::pin!(fut);
    loop {
        tokio::select! {
            v = &mut fut => return v,
            Some(_) = keys.recv() => {}
        }
    }
}

/// One connection, up to its end. `Ok` carries why it ended.
async fn serve(
    cfg: &Config,
    device: &DeviceInfo,
    events: &mpsc::Sender<LinkEvent>,
    keys: &mut mpsc::UnboundedReceiver<ClientMessage>,
) -> Result<String> {
    let mut request = cfg.server.as_str().into_client_request()?;
    if let Some(token) = &cfg.token {
        request.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::try_from(format!("Bearer {token}"))
                .map_err(|_| anyhow!("token contains characters not allowed in a header"))?,
        );
    }
    let connect = timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request));
    let connected = drop_keys(keys, connect)
        .await
        .map_err(|_| anyhow!("connect timed out"))?;
    let (ws, _) = match connected {
        Ok(ok) => ok,
        Err(WsError::Http(resp)) if resp.status() == StatusCode::UNAUTHORIZED => {
            error!("server rejected token (HTTP 401): check SDC_CLIENT_TOKEN / --token");
            bail!("server rejected token");
        }
        Err(e) => return Err(e.into()),
    };
    let (mut tx, mut rx) = ws.split();
    info!("connected");

    let hello = ClientMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        client_id: cfg.client_id.clone(),
        device: device.clone(),
        client_target: sdc_protocol::client_target().map(str::to_string),
        client_sha256: Some(cfg.client_sha256.clone()),
        device_name: cfg.device.clone(),
    };
    tx.send(Message::text(serde_json::to_string(&hello)?))
        .await?;
    // Send failures below mean the deck loop ended, and this task is aborted with it.
    let _ = events.send(LinkEvent::Up).await;

    let mut ping = interval(PING_EVERY);
    let mut last_rx = Instant::now();
    loop {
        tokio::select! {
            Some(msg) = keys.recv() => {
                debug!(?msg, "key event");
                tx.send(Message::text(serde_json::to_string(&msg)?)).await?;
            }
            m = rx.next() => {
                let Some(m) = m else { return Ok("closed by server".into()) };
                let m = m?;
                last_rx = Instant::now();
                match m {
                    Message::Text(t) => match serde_json::from_str::<ServerMessage>(&t) {
                        Ok(ServerMessage::Welcome { .. }) => {}
                        Ok(ServerMessage::Brightness { percent }) => {
                            info!(percent, "brightness");
                            let _ = events.send(LinkEvent::Brightness(percent)).await;
                        }
                        Ok(ServerMessage::Update { path, sha256 }) => update(cfg, path, sha256).await,
                        Ok(ServerMessage::Error { message }) => error!("server error: {message}"),
                        Err(e) => warn!("bad server message {t:?}: {e}"),
                    },
                    Message::Binary(b) => {
                        if let Some(f) = ImageFrame::decode(&b) {
                            let _ = events.send(LinkEvent::Image(f.key, f.image.to_vec())).await;
                        }
                    }
                    Message::Close(f) => return Ok(format!("close frame: {f:?}")),
                    _ => {}
                }
            }
            _ = ping.tick() => {
                if last_rx.elapsed() > IDLE_TIMEOUT {
                    return Ok("idle timeout".into());
                }
                tx.send(Message::Ping(vec![].into())).await?;
            }
        }
    }
}

/// Replace this binary with the one the server offers and exit so the service manager starts
/// it. On failure keeps running the current version.
async fn update(cfg: &Config, path: String, sha256: String) {
    if !cfg.auto_update {
        info!("server offers a client update; ignored (--no-update)");
        return;
    }
    info!(%path, "server offers a new client binary, updating");
    let (server, token) = (cfg.server.clone(), cfg.token.clone());
    let res = tokio::task::spawn_blocking(move || {
        crate::update::apply(&server, token.as_deref(), &path, &sha256)
    })
    .await;
    match res {
        Ok(Ok(())) => {
            info!("client updated; exiting so the service manager restarts the new version");
            std::process::exit(0);
        }
        Ok(Err(e)) => warn!("self-update failed, staying on this version: {e:#}"),
        Err(e) => warn!("self-update task failed: {e}"),
    }
}
