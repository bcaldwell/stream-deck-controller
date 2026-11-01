//! Server connection: reconnect loop and the per-connection session.

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use futures_util::{SinkExt, StreamExt};
use sdc_protocol::{ClientMessage, ImageFrame, ServerMessage, PROTOCOL_VERSION};
use tokio::time::{interval, timeout, Instant};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tracing::{debug, error, info, warn};

use crate::device::{Deck, KeyEvent};

/// If nothing (including pongs to our pings) arrives for this long, the link is dead.
const IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const PING_EVERY: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Brightness cap (percent) of the offline state.
const OFFLINE_BRIGHTNESS: u8 = 20;

/// Tracks how long the server has been unreachable, to decide when the deck should show its
/// offline state. Short outages (deploys, client restarts) leave the last images untouched.
/// Time is passed in, so this is testable without a device.
#[derive(Debug)]
pub struct Link {
    grace: Duration,
    /// When the connection was lost (or the client started); `None` while connected.
    down_since: Option<Instant>,
    offline_shown: bool,
}

impl Link {
    /// Starts out disconnected, as at process start.
    pub fn new(grace: Duration, now: Instant) -> Self {
        Self {
            grace,
            down_since: Some(now),
            offline_shown: false,
        }
    }

    /// The server connection is up. Returns whether the offline state was showing.
    pub fn up(&mut self) -> bool {
        self.down_since = None;
        std::mem::take(&mut self.offline_shown)
    }

    /// The server connection was lost (keeps the original time if already down).
    pub fn down(&mut self, now: Instant) {
        self.down_since.get_or_insert(now);
    }

    /// When the offline state is due, if it is not showing and the link is down.
    pub fn deadline(&self) -> Option<Instant> {
        if self.offline_shown {
            return None;
        }
        self.down_since.map(|t| t + self.grace)
    }

    /// True once when the offline state should be drawn now; the caller then draws it.
    pub fn take_due(&mut self, now: Instant) -> bool {
        if self.deadline().is_some_and(|d| now >= d) {
            self.offline_shown = true;
            return true;
        }
        false
    }
}

/// Draws the offline state: dimmed, black keys with a red dot on key 0.
async fn show_offline(cfg: &Config, deck: &Deck) -> Result<()> {
    warn!(after = ?cfg.offline_after, "server unreachable, showing offline state");
    deck.set_brightness(cfg.brightness.min(OFFLINE_BRIGHTNESS))
        .await?;
    deck.show_disconnected().await
}

pub struct Config {
    pub server: String,
    /// Bearer token for the handshake and update downloads (None: send no credentials).
    pub token: Option<String>,
    pub client_id: String,
    pub brightness: u8,
    /// How long the server may be unreachable before the deck shows its offline state.
    pub offline_after: Duration,
    /// Hash of the running executable, reported in Hello (None: could not read it).
    pub client_sha256: Option<String>,
    /// Apply `Update` messages from the server (`--no-update` turns this off).
    pub auto_update: bool,
}

pub enum End {
    /// Server link ended; reconnect.
    Server(String),
    /// The Stream Deck went away; go look for a device again.
    Device(String),
}

/// What to do for a decoded server message.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Brightness(u8),
    Image { key: u8, jpeg: Vec<u8> },
    Update { path: String, sha256: String },
}

/// Pure decoding of a websocket frame into an [`Action`].
pub fn handle_message(msg: &Message) -> Result<Action> {
    match msg {
        Message::Text(t) => match serde_json::from_str::<ServerMessage>(t)
            .map_err(|e| anyhow!("bad server message {t:?}: {e}"))?
        {
            ServerMessage::Welcome { protocol_version } => {
                if protocol_version != PROTOCOL_VERSION {
                    warn!(
                        protocol_version,
                        ours = PROTOCOL_VERSION,
                        "protocol version mismatch"
                    );
                }
                Ok(Action::None)
            }
            ServerMessage::Brightness { percent } => Ok(Action::Brightness(percent.min(100))),
            ServerMessage::Update { path, sha256 } => Ok(Action::Update { path, sha256 }),
            ServerMessage::Error { message } => {
                error!("server error: {message}");
                Ok(Action::None)
            }
        },
        Message::Binary(b) => {
            let f = ImageFrame::decode(b).ok_or_else(|| anyhow!("empty binary frame"))?;
            Ok(Action::Image {
                key: f.key,
                jpeg: f.image.to_vec(),
            })
        }
        _ => Ok(Action::None),
    }
}

/// Run forever: connect/reconnect to the server while this deck is usable.
/// Returns only when the device fails.
pub async fn run(cfg: &Config, deck: &mut Deck) -> String {
    let mut backoff = Duration::from_millis(500);
    let mut link = Link::new(cfg.offline_after, Instant::now());
    loop {
        info!(server = %cfg.server, "connecting");
        let started = Instant::now();
        let res = session(cfg, deck, &mut link).await;
        link.down(Instant::now());
        match res {
            Ok(End::Device(why)) => return why,
            Ok(End::Server(why)) => warn!("disconnected from server: {why}"),
            Err(e) => warn!("server connection failed: {e:#}"),
        }
        // If we were up for a while, start the backoff over.
        if started.elapsed() > Duration::from_secs(30) {
            backoff = Duration::from_millis(500);
        }
        info!(?backoff, "retrying");
        let wake = Instant::now() + backoff;
        loop {
            let until = link.deadline().map_or(wake, |d| d.min(wake));
            tokio::time::sleep_until(until).await;
            if link.take_due(Instant::now()) {
                if let Err(e) = show_offline(cfg, deck).await {
                    return format!("device error: {e:#}");
                }
            }
            if Instant::now() >= wake {
                break;
            }
        }
        backoff = (backoff * 2).min(Duration::from_secs(15));
    }
}

async fn session(cfg: &Config, deck: &mut Deck, link: &mut Link) -> Result<End> {
    let mut request = cfg.server.as_str().into_client_request()?;
    if let Some(token) = &cfg.token {
        request.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::try_from(format!("Bearer {token}"))
                .map_err(|_| anyhow!("token contains characters not allowed in a header"))?,
        );
    }
    // The offline state can fall due while the connect attempt is still pending.
    let connect = timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request));
    tokio::pin!(connect);
    let connected = loop {
        let due = link.deadline();
        tokio::select! {
            r = &mut connect => break r,
            _ = async { tokio::time::sleep_until(due.unwrap()).await }, if due.is_some() => {
                if link.take_due(Instant::now()) {
                    if let Err(e) = show_offline(cfg, deck).await {
                        return Ok(End::Device(format!("device error: {e:#}")));
                    }
                }
            }
        }
    };
    let (ws, _) = match connected.map_err(|_| anyhow!("connect timed out"))? {
        Ok(ok) => ok,
        Err(WsError::Http(resp)) if resp.status() == StatusCode::UNAUTHORIZED => {
            error!("server rejected token (HTTP 401): check SDC_CLIENT_TOKEN / --token");
            bail!("server rejected token");
        }
        Err(e) => return Err(e.into()),
    };
    let (mut tx, mut rx) = ws.split();
    info!("connected");
    link.up();

    let hello = ClientMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        client_id: cfg.client_id.clone(),
        device: deck.info.clone(),
        client_target: sdc_protocol::client_target().map(str::to_string),
        client_sha256: cfg.client_sha256.clone(),
    };
    tx.send(Message::text(serde_json::to_string(&hello)?))
        .await?;
    if let Err(e) = deck.set_brightness(cfg.brightness).await {
        return Ok(End::Device(format!("device error: {e:#}")));
    }
    // No clear here: the deck keeps its last images until the server's frames replace them.

    let mut ping = interval(PING_EVERY);
    let mut last_rx = Instant::now();
    // One update attempt per connection, so an unwritable install does not retry in a loop.
    let mut update_tried = false;
    loop {
        tokio::select! {
            ev = deck.events.recv() => {
                let msg = match ev {
                    Some(Ok(KeyEvent::Down(key))) => ClientMessage::KeyDown { key },
                    Some(Ok(KeyEvent::Up(key))) => ClientMessage::KeyUp { key },
                    Some(Err(e)) => return Ok(End::Device(e)),
                    None => return Ok(End::Device("device event channel closed".into())),
                };
                debug!(?msg, "key event");
                tx.send(Message::text(serde_json::to_string(&msg)?)).await?;
            }
            m = rx.next() => {
                let Some(m) = m else { return Ok(End::Server("closed by server".into())) };
                let m = m?;
                last_rx = Instant::now();
                if let Message::Close(f) = &m {
                    return Ok(End::Server(format!("close frame: {f:?}")));
                }
                match handle_message(&m) {
                    Ok(Action::None) => {}
                    Ok(Action::Brightness(p)) => {
                        info!(percent = p, "brightness");
                        if let Err(e) = deck.set_brightness(p).await {
                            return Ok(End::Device(format!("device error: {e:#}")));
                        }
                    }
                    Ok(Action::Update { path, sha256 }) => {
                        if !cfg.auto_update {
                            info!("server offers a client update; ignored (--no-update)");
                        } else if update_tried || cfg.client_sha256.as_deref() == Some(sha256.as_str()) {
                            debug!("ignoring update message");
                        } else {
                            update_tried = true;
                            info!(%path, "server offers a new client binary, updating");
                            let server = cfg.server.clone();
                            let token = cfg.token.clone();
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
                    }
                    Ok(Action::Image { key, jpeg }) => {
                        if let Err(e) = deck.set_image_jpeg(key, &jpeg).await {
                            if deck.is_fake() || e.downcast_ref::<elgato_streamdeck::StreamDeckError>().is_none() {
                                warn!(key, "image rejected: {e:#}");
                            } else {
                                return Ok(End::Device(format!("device error: {e:#}")));
                            }
                        }
                    }
                    Err(e) => warn!("{e:#}"),
                }
            }
            _ = ping.tick() => {
                if last_rx.elapsed() > IDLE_TIMEOUT {
                    return Ok(End::Server("idle timeout".into()));
                }
                tx.send(Message::Ping(vec![].into())).await?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_text_messages() {
        let m = Message::text(r#"{"type":"brightness","percent":300}"#);
        // u8 overflow is a parse error, not a panic
        assert!(handle_message(&m).is_err());
        let m = Message::text(r#"{"type":"brightness","percent":150}"#);
        assert_eq!(handle_message(&m).unwrap(), Action::Brightness(100));
        let m = Message::text(r#"{"type":"welcome","protocol_version":1}"#);
        assert_eq!(handle_message(&m).unwrap(), Action::None);
        let m = Message::text(r#"{"type":"update","path":"/client/x","sha256":"ab"}"#);
        assert_eq!(
            handle_message(&m).unwrap(),
            Action::Update {
                path: "/client/x".into(),
                sha256: "ab".into()
            }
        );
        let m = Message::text(r#"{"type":"error","message":"boom"}"#);
        assert_eq!(handle_message(&m).unwrap(), Action::None);
        assert!(handle_message(&Message::text("garbage")).is_err());
    }

    #[test]
    fn handles_binary_frames() {
        let m = Message::Binary(ImageFrame::encode(7, &[1, 2, 3]).into());
        assert_eq!(
            handle_message(&m).unwrap(),
            Action::Image {
                key: 7,
                jpeg: vec![1, 2, 3]
            }
        );
        assert!(handle_message(&Message::Binary(vec![].into())).is_err());
    }

    #[test]
    fn link_shows_offline_only_after_grace() {
        let t0 = Instant::now();
        let secs = Duration::from_secs;
        // startup: counts as down from the start, nothing drawn before the grace
        let mut l = Link::new(secs(20), t0);
        assert_eq!(l.deadline(), Some(t0 + secs(20)));
        assert!(!l.take_due(t0 + secs(19)));
        assert!(l.take_due(t0 + secs(20)));
        assert!(!l.take_due(t0 + secs(21)), "drawn only once");
        assert_eq!(l.deadline(), None);
        // reconnect reports that the offline state has to be undone
        assert!(l.up());
        assert!(!l.up());
        assert_eq!(l.deadline(), None, "connected: nothing due");
        assert!(!l.take_due(t0 + secs(1000)));

        // a short outage never draws anything
        l.down(t0 + secs(100));
        l.down(t0 + secs(105)); // repeated failures keep the original loss time
        assert_eq!(l.deadline(), Some(t0 + secs(120)));
        assert!(!l.take_due(t0 + secs(119)));
        assert!(!l.up());
        assert!(!l.take_due(t0 + secs(130)));

        // a second long outage draws again
        l.down(t0 + secs(200));
        assert!(l.take_due(t0 + secs(220)));
        assert!(l.up());
    }

    #[test]
    fn hello_roundtrip() {
        let hello = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_id: "pi".into(),
            device: crate::device::device_info(elgato_streamdeck::info::Kind::Original, None),
            client_target: Some("aarch64-linux".into()),
            client_sha256: None,
        };
        let s = serde_json::to_string(&hello).unwrap();
        assert!(s.contains(r#""type":"hello""#) && s.contains(r#""key_size_px":72"#));
    }
}
