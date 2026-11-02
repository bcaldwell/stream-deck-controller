//! Server connection: reconnect loop and the per-connection session.

use std::collections::{BTreeMap, HashMap};
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

use crate::device::{Deck, DeviceEvent, KeyEvent};
use crate::sleep::{self, Poll, Sleeper};

/// If nothing (including pongs to our pings) arrives for this long, the link is dead.
const IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const PING_EVERY: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Brightness cap (percent) of the offline state.
const OFFLINE_BRIGHTNESS: u8 = 20;

/// How long a key shows the offline marker after a press while disconnected.
const FEEDBACK_FOR: Duration = Duration::from_secs(2);

/// Last JPEG received per key, so keys can be redrawn while disconnected.
type Cache = HashMap<u8, Vec<u8>>;

/// Tracks how long the server has been unreachable, to decide when the deck should show its
/// offline screen, and which keys are flashing press feedback. Outages leave the last images
/// untouched until the grace passes. Time is passed in, so this is testable without a device.
#[derive(Debug)]
pub struct Link {
    grace: Duration,
    /// When the connection was lost (or the client started); `None` while connected.
    down_since: Option<Instant>,
    offline_shown: bool,
    /// Keys showing press feedback, with when to restore them.
    feedback: BTreeMap<u8, Instant>,
}

impl Link {
    /// Starts out disconnected, as at process start.
    pub fn new(grace: Duration, now: Instant) -> Self {
        Self {
            grace,
            down_since: Some(now),
            offline_shown: false,
            feedback: BTreeMap::new(),
        }
    }

    /// The server connection is up. Returns whether the offline state was showing.
    pub fn up(&mut self) -> bool {
        self.down_since = None;
        // the server re-sends every image, which also replaces any feedback
        self.feedback.clear();
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
            self.feedback.clear(); // the offline screen replaces everything
            return true;
        }
        false
    }

    /// A key went down. True if feedback should be drawn on it now: only while disconnected
    /// and before the offline screen (which is its own feedback). Repeats extend the flash.
    pub fn press(&mut self, key: u8, now: Instant) -> bool {
        if self.down_since.is_none() || self.offline_shown {
            return false;
        }
        self.feedback.insert(key, now + FEEDBACK_FOR);
        true
    }

    /// Keys whose feedback has run out; the caller restores them.
    pub fn take_restores(&mut self, now: Instant) -> Vec<u8> {
        let keys: Vec<u8> = self
            .feedback
            .iter()
            .filter(|(_, t)| now >= **t)
            .map(|(k, _)| *k)
            .collect();
        for k in &keys {
            self.feedback.remove(k);
        }
        keys
    }

    /// The next time something is due: the offline screen or a feedback restore.
    pub fn wake_at(&self) -> Option<Instant> {
        self.feedback.values().copied().chain(self.deadline()).min()
    }
}

/// Draws the offline state: dimmed, black keys with a red dot on key 0.
async fn show_offline(cfg: &Config, deck: &Deck) -> Result<()> {
    warn!(after = ?cfg.offline_after, "server unreachable, showing offline state");
    deck.set_brightness(cfg.brightness.min(OFFLINE_BRIGHTNESS))
        .await?;
    deck.show_disconnected().await
}

/// Handles whatever is due while disconnected: the offline screen and feedback restores.
async fn offline_due(
    cfg: &Config,
    deck: &Deck,
    link: &mut Link,
    cache: &Cache,
    sleeper: &Sleeper,
    now: Instant,
) -> Result<()> {
    if sleeper.is_asleep() {
        return Ok(()); // the device is released; whatever is due is drawn after it wakes
    }
    if link.take_due(now) {
        show_offline(cfg, deck).await?;
    }
    for key in link.take_restores(now) {
        deck.restore_key(key, cache.get(&key).map(Vec::as_slice))
            .await?;
    }
    Ok(())
}

/// When something is due for the link, unless the deck is asleep (nothing is drawn then).
fn link_due(link: &Link, sleeper: &Sleeper) -> Option<Instant> {
    if sleeper.is_asleep() {
        None
    } else {
        link.wake_at()
    }
}

fn is_device_error(deck: &Deck, e: &anyhow::Error) -> bool {
    !deck.is_fake()
        && e.downcast_ref::<elgato_streamdeck::StreamDeckError>()
            .is_some()
}

/// Reopens the released device, draws the frames that arrived meanwhile, then applies `brightness`.
async fn resume(
    deck: &mut Deck,
    sleeper: &mut Sleeper,
    cache: &Cache,
    brightness: Option<u8>,
) -> Result<(), String> {
    deck.reopen()
        .await
        .map_err(|e| format!("reopening the device failed: {e:#}"))?;
    for key in sleeper.take_dirty() {
        let Some(jpeg) = cache.get(&key) else {
            continue;
        };
        if let Err(e) = deck.set_image_jpeg(key, jpeg).await {
            if is_device_error(deck, &e) {
                return Err(format!("device error: {e:#}"));
            }
            warn!(key, "image rejected: {e:#}");
        }
    }
    if let Some(p) = brightness {
        deck.set_brightness(p)
            .await
            .map_err(|e| format!("device error: {e:#}"))?;
    }
    Ok(())
}

/// Releases the device once the settle time after brightness 0 is over, and while asleep
/// watches for the deck waking up. `Ok(true)`: it woke and the server (`connected`) should be
/// told with [`ClientMessage::Wake`]. `Err`: the device is gone; look for it again.
async fn sleep_step(
    cfg: &Config,
    deck: &mut Deck,
    sleeper: &mut Sleeper,
    cache: &Cache,
    connected: bool,
    now: Instant,
) -> Result<bool, String> {
    if sleeper.take_due(now) {
        deck.release().await;
        info!("screen off: released the deck so it can USB-suspend");
        return Ok(false);
    }
    let path = deck.status_path().map(|p| p.to_path_buf());
    match sleeper.poll(now, || path.as_deref().and_then(sleep::read_status)) {
        Poll::Idle => Ok(false),
        Poll::Woken => {
            let plan =
                sleep::wake_plan(connected, sleeper.take_pending_brightness(), cfg.brightness);
            info!(connected, "deck woke from USB suspend");
            resume(deck, sleeper, cache, plan.brightness).await?;
            Ok(plan.send_wake)
        }
        Poll::GaveUp => {
            warn!("deck did not USB-suspend (is the udev rule installed?); USB suspend disabled");
            resume(deck, sleeper, cache, None).await?;
            Ok(false)
        }
    }
}

/// A device event while disconnected: a press flashes the offline marker; nothing is sent.
async fn offline_event(
    deck: &Deck,
    link: &mut Link,
    cache: &Cache,
    ev: Option<DeviceEvent>,
) -> Result<()> {
    match ev {
        Some(Ok(KeyEvent::Down(key))) if link.press(key, Instant::now()) => {
            debug!(key, "key pressed while offline");
            deck.show_offline_key(key, cache.get(&key).map(Vec::as_slice))
                .await
        }
        Some(Ok(_)) => Ok(()),
        Some(Err(e)) => Err(anyhow!(e)),
        None => Err(anyhow!("device event channel closed")),
    }
}

pub struct Config {
    pub server: String,
    /// Bearer token for the handshake and update downloads (None: send no credentials).
    pub token: Option<String>,
    pub client_id: String,
    pub brightness: u8,
    /// Requested server-side device layout (`sdc.device` name), sent in Hello.
    pub device: Option<String>,
    /// How long the server may be unreachable before the deck shows its offline screen.
    pub offline_after: Duration,
    /// Hash of the running executable, reported in Hello (None: could not read it).
    pub client_sha256: Option<String>,
    /// Apply `Update` messages from the server (`--no-update` turns this off).
    pub auto_update: bool,
    /// Release the device while the screen is off so it can USB-suspend (Linux only).
    pub usb_suspend: bool,
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
    let mut cache = Cache::new();
    if cfg.usb_suspend && deck.status_path().is_none() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| info!("USB suspend not available for this device, keeping it open"));
    }
    let mut sleeper = Sleeper::new(cfg.usb_suspend && deck.status_path().is_some());
    loop {
        info!(server = %cfg.server, "connecting");
        let started = Instant::now();
        let res = session(cfg, deck, &mut link, &mut cache, &mut sleeper).await;
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
            let until = [link_due(&link, &sleeper), sleeper.next_at()]
                .into_iter()
                .flatten()
                .fold(wake, Instant::min);
            tokio::select! {
                _ = tokio::time::sleep_until(until) => {}
                ev = deck.events.recv(), if !sleeper.is_asleep() => {
                    if let Err(e) = offline_event(deck, &mut link, &cache, ev).await {
                        return format!("device error: {e:#}");
                    }
                }
            }
            if let Err(e) = sleep_step(cfg, deck, &mut sleeper, &cache, false, Instant::now()).await
            {
                return e;
            }
            if let Err(e) =
                offline_due(cfg, deck, &mut link, &cache, &sleeper, Instant::now()).await
            {
                return format!("device error: {e:#}");
            }
            if Instant::now() >= wake {
                break;
            }
        }
        backoff = (backoff * 2).min(Duration::from_secs(15));
    }
}

async fn session(
    cfg: &Config,
    deck: &mut Deck,
    link: &mut Link,
    cache: &mut Cache,
    sleeper: &mut Sleeper,
) -> Result<End> {
    let mut request = cfg.server.as_str().into_client_request()?;
    if let Some(token) = &cfg.token {
        request.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::try_from(format!("Bearer {token}"))
                .map_err(|_| anyhow!("token contains characters not allowed in a header"))?,
        );
    }
    // Offline timers and key presses are served while the connect attempt is pending.
    let connect = timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request));
    tokio::pin!(connect);
    let connected = loop {
        let due = [link_due(link, sleeper), sleeper.next_at()]
            .into_iter()
            .flatten()
            .min();
        tokio::select! {
            r = &mut connect => break r,
            _ = async { tokio::time::sleep_until(due.unwrap()).await }, if due.is_some() => {
                if let Err(why) = sleep_step(cfg, deck, sleeper, cache, false, Instant::now()).await {
                    return Ok(End::Device(why));
                }
                if let Err(e) = offline_due(cfg, deck, link, cache, sleeper, Instant::now()).await {
                    return Ok(End::Device(format!("device error: {e:#}")));
                }
            }
            ev = deck.events.recv(), if !sleeper.is_asleep() => {
                if let Err(e) = offline_event(deck, link, cache, ev).await {
                    return Ok(End::Device(format!("device error: {e:#}")));
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
        mirror: None,
        device_name: cfg.device.clone(),
    };
    tx.send(Message::text(serde_json::to_string(&hello)?))
        .await?;
    if sleeper.is_asleep() {
        // stay asleep; the brightness is applied when a press wakes the deck
        sleeper.set_pending_brightness(cfg.brightness);
    } else {
        sleeper.brightness(cfg.brightness, Instant::now());
        if let Err(e) = deck.set_brightness(cfg.brightness).await {
            return Ok(End::Device(format!("device error: {e:#}")));
        }
    }
    // No clear here: the deck keeps its last images until the server's frames replace them.

    let mut ping = interval(PING_EVERY);
    let mut last_rx = Instant::now();
    // One update attempt per connection, so an unwritable install does not retry in a loop.
    let mut update_tried = false;
    loop {
        let sleep_at = sleeper.next_at();
        tokio::select! {
            _ = async { tokio::time::sleep_until(sleep_at.unwrap()).await }, if sleep_at.is_some() => {
                match sleep_step(cfg, deck, sleeper, cache, true, Instant::now()).await {
                    Ok(true) => tx.send(Message::text(serde_json::to_string(&ClientMessage::Wake)?)).await?,
                    Ok(false) => {}
                    Err(why) => return Ok(End::Device(why)),
                }
            }
            ev = deck.events.recv(), if !sleeper.is_asleep() => {
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
                        if sleeper.brightness(p, Instant::now()) {
                            if let Err(why) = resume(deck, sleeper, cache, Some(p)).await {
                                return Ok(End::Device(why));
                            }
                        } else if !sleeper.is_asleep() {
                            if let Err(e) = deck.set_brightness(p).await {
                                return Ok(End::Device(format!("device error: {e:#}")));
                            }
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
                        let changed = cache.get(&key) != Some(&jpeg);
                        cache.insert(key, jpeg.clone());
                        // while asleep the device is closed: drawn when it wakes
                        if !sleeper.image(key, changed) {
                            if let Err(e) = deck.set_image_jpeg(key, &jpeg).await {
                                if is_device_error(deck, &e) {
                                    return Ok(End::Device(format!("device error: {e:#}")));
                                }
                                warn!(key, "image rejected: {e:#}");
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
        let mut l = Link::new(secs(1800), t0);
        assert_eq!(l.deadline(), Some(t0 + secs(1800)));
        assert!(!l.take_due(t0 + secs(1799)));
        assert!(l.take_due(t0 + secs(1800)));
        assert!(!l.take_due(t0 + secs(1801)), "drawn only once");
        assert_eq!(l.deadline(), None);
        // reconnect reports that the offline state has to be undone
        assert!(l.up());
        assert!(!l.up());
        assert_eq!(l.deadline(), None, "connected: nothing due");
        assert!(!l.take_due(t0 + secs(5000)));

        // a short outage never draws anything
        l.down(t0 + secs(6000));
        l.down(t0 + secs(6005)); // repeated failures keep the original loss time
        assert_eq!(l.deadline(), Some(t0 + secs(7800)));
        assert_eq!(l.wake_at(), l.deadline());
        assert!(!l.take_due(t0 + secs(7799)));
        assert!(l.take_restores(t0 + secs(7000)).is_empty());
        assert!(!l.up());
        assert!(!l.take_due(t0 + secs(9000)));

        // a second long outage draws again
        l.down(t0 + secs(10000));
        assert!(l.take_due(t0 + secs(11800)));
        assert!(l.up());
    }

    #[test]
    fn link_press_feedback_while_offline() {
        let t0 = Instant::now();
        let secs = Duration::from_secs;
        let mut l = Link::new(secs(1800), t0);
        // press while disconnected: feedback on that key, restored after 2s
        assert!(l.press(3, t0 + secs(10)));
        assert_eq!(l.wake_at(), Some(t0 + secs(12)));
        assert!(l.take_restores(t0 + secs(11)).is_empty());
        // a repeat press extends it; other keys are tracked independently
        assert!(l.press(3, t0 + secs(11)));
        assert!(l.press(7, t0 + secs(11)));
        assert!(l.take_restores(t0 + secs(12)).is_empty());
        assert_eq!(l.take_restores(t0 + secs(13)), vec![3, 7]);
        assert!(l.take_restores(t0 + secs(14)).is_empty(), "restored once");
        assert_eq!(l.wake_at(), l.deadline());

        // the long fallback shows the offline screen and cancels pending feedback
        assert!(l.press(1, t0 + secs(1799)));
        assert!(l.take_due(t0 + secs(1800)));
        assert!(l.take_restores(t0 + secs(2000)).is_empty());
        assert!(
            !l.press(1, t0 + secs(1801)),
            "offline screen is its own feedback"
        );

        // reconnect restores; connected presses are sent, not flashed
        assert!(l.up());
        assert!(!l.press(1, t0 + secs(1900)));
        assert_eq!(l.wake_at(), None);

        // reconnect drops pending feedback (the server re-sends images)
        l.down(t0 + secs(2000));
        assert!(l.press(2, t0 + secs(2001)));
        assert!(!l.up());
        assert!(l.take_restores(t0 + secs(2100)).is_empty());
    }

    #[test]
    fn hello_roundtrip() {
        let hello = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_id: "pi".into(),
            device: crate::device::device_info(elgato_streamdeck::info::Kind::Original, None),
            client_target: Some("aarch64-linux".into()),
            client_sha256: None,
            mirror: None,
            device_name: None,
        };
        let s = serde_json::to_string(&hello).unwrap();
        assert!(s.contains(r#""type":"hello""#) && s.contains(r#""key_size_px":72"#));
    }
}
