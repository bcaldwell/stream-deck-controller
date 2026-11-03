//! The deck loop: key presses and server events, USB suspend and wake.
//!
//! The server connection lives in `link.rs` and reports through a channel. While it is down the
//! deck keeps showing the last images, and a key press flashes that key.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::convert::Infallible;
use std::time::Duration;

use anyhow::{anyhow, Result};
use sdc_protocol::ClientMessage;
use tokio::sync::mpsc;
use tokio::time::{sleep_until, Instant};
use tracing::{debug, info, warn};

use crate::device::{Deck, KeyEvent};
use crate::link::{self, LinkEvent};
use crate::sleep::{self, Sleeper, Step};

/// How long a key stays red after a press while the server is unreachable.
const FLASH_FOR: Duration = Duration::from_millis(400);
const FLASH_COLOR: [u8; 3] = [200, 0, 0];

#[derive(Clone)]
pub struct Config {
    pub server: String,
    /// Bearer token for the handshake and update downloads (None: send no credentials).
    pub token: Option<String>,
    pub client_id: String,
    pub brightness: u8,
    /// Requested server-side device layout (`sdc.device` name), sent in Hello.
    pub device: Option<String>,
    /// Hash of the running executable, reported in Hello.
    pub client_sha256: String,
    /// Apply `Update` messages from the server (`--no-update` turns this off).
    pub auto_update: bool,
    /// Release the device while the screen is off so it can USB-suspend (Linux only).
    pub usb_suspend: bool,
}

/// Runs the connection to the server and serves the deck until the deck fails; returns why.
pub async fn run(cfg: &Config, deck: &mut Deck) -> String {
    let (events_tx, events) = mpsc::channel(64);
    let (keys, keys_rx) = mpsc::unbounded_channel();
    let link = tokio::spawn(link::run(
        cfg.clone(),
        deck.info.clone(),
        events_tx,
        keys_rx,
    ));
    let res = serve(cfg, deck, events, keys).await;
    link.abort();
    match res {
        Err(e) => format!("{e:#}"),
        Ok(never) => match never {},
    }
}

struct State {
    connected: bool,
    sleeper: Sleeper,
    /// Last frame per key, to restore a flashed key and to redraw after a wake.
    cache: HashMap<u8, Vec<u8>>,
    /// Keys whose frame changed while the device was released.
    dirty: BTreeSet<u8>,
    /// Flashing keys and when to restore them.
    flashing: BTreeMap<u8, Instant>,
}

async fn serve(
    cfg: &Config,
    deck: &mut Deck,
    mut events: mpsc::Receiver<LinkEvent>,
    keys: mpsc::UnboundedSender<ClientMessage>,
) -> Result<Infallible> {
    let usb_suspend = cfg.usb_suspend && deck.status_path.is_some();
    let mut st = State {
        connected: false,
        sleeper: Sleeper::new(usb_suspend),
        cache: HashMap::new(),
        dirty: BTreeSet::new(),
        flashing: BTreeMap::new(),
    };
    deck.set_brightness(cfg.brightness).await?;
    loop {
        let due = st
            .flashing
            .values()
            .copied()
            .chain(st.sleeper.next_at())
            .min();
        tokio::select! {
            _ = async { sleep_until(due.unwrap()).await }, if due.is_some() => {
                tick(cfg, deck, &mut st, &keys).await?;
            }
            ev = deck.events.recv(), if !st.sleeper.is_asleep() => {
                match ev.ok_or_else(|| anyhow!("device event channel closed"))?.map_err(|e| anyhow!(e))? {
                    KeyEvent::Down(key) if !st.connected => {
                        debug!(key, "key pressed while disconnected");
                        deck.fill(key, FLASH_COLOR).await?;
                        st.flashing.insert(key, Instant::now() + FLASH_FOR);
                    }
                    KeyEvent::Down(key) if st.connected => { let _ = keys.send(ClientMessage::KeyDown { key }); }
                    KeyEvent::Up(key) if st.connected => { let _ = keys.send(ClientMessage::KeyUp { key }); }
                    _ => {}
                }
            }
            ev = events.recv() => {
                let ev = ev.ok_or_else(|| anyhow!("server link ended"))?;
                on_link(deck, &mut st, ev).await?;
            }
        }
    }
}

/// A timer is due: restore flashed keys, release the device, poll for a wake.
async fn tick(
    cfg: &Config,
    deck: &mut Deck,
    st: &mut State,
    keys: &mpsc::UnboundedSender<ClientMessage>,
) -> Result<()> {
    let now = Instant::now();
    let due: Vec<u8> = st
        .flashing
        .iter()
        .filter(|(_, t)| now >= **t)
        .map(|(k, _)| *k)
        .collect();
    for key in due {
        st.flashing.remove(&key);
        match st.cache.get(&key) {
            Some(frame) => deck.write_frame(key, frame).await?,
            None => deck.fill(key, [0, 0, 0]).await?,
        }
    }
    let status_path = deck.status_path.clone();
    let step = st
        .sleeper
        .step(now, || status_path.as_deref().and_then(sleep::read_status));
    match step {
        Step::Nothing => {}
        Step::Release => {
            deck.release().await;
            info!("screen off: released the deck so it can USB-suspend");
        }
        Step::Wake => {
            info!(connected = st.connected, "deck woke from USB suspend");
            reopen(deck, st).await?;
            if st.connected {
                // the server restores the brightness and restarts its idle timers
                let _ = keys.send(ClientMessage::Wake);
            } else {
                deck.set_brightness(cfg.brightness).await?;
            }
        }
        Step::GiveUp => {
            warn!("deck did not USB-suspend (is the udev rule installed?); USB suspend disabled");
            reopen(deck, st).await?;
        }
    }
    Ok(())
}

async fn on_link(deck: &mut Deck, st: &mut State, ev: LinkEvent) -> Result<()> {
    match ev {
        LinkEvent::Up => st.connected = true,
        LinkEvent::Down => st.connected = false,
        LinkEvent::Brightness(p) => {
            let was_asleep = st.sleeper.is_asleep();
            st.sleeper.brightness(p, Instant::now());
            if was_asleep && p > 0 {
                reopen(deck, st).await?;
            }
            if !st.sleeper.is_asleep() {
                deck.set_brightness(p).await?;
            }
        }
        LinkEvent::Image(key, jpeg) => {
            let changed = st.cache.get(&key) != Some(&jpeg);
            if st.sleeper.is_asleep() {
                st.dirty.extend(changed.then_some(key));
            } else {
                deck.write_frame(key, &jpeg).await?;
            }
            st.cache.insert(key, jpeg);
        }
    }
    Ok(())
}

/// Reopens the released device and draws the frames that arrived meanwhile.
async fn reopen(deck: &mut Deck, st: &mut State) -> Result<()> {
    deck.reopen().await?;
    for key in std::mem::take(&mut st.dirty) {
        if let Some(frame) = st.cache.get(&key) {
            deck.write_frame(key, frame).await?;
        }
    }
    Ok(())
}
