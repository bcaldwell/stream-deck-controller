//! WebSocket server for clients (`/ws`), the Elgato plugin (`/plugin/ws`) and browser decks
//! (`/deck/ws`). Authentication happens in `http.rs`; these handlers are the trust boundary for
//! what a connection says: the hello is validated here, and so is every key index and plugin
//! slot id, so the engine can trust the events it gets.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Extension, State, WebSocketUpgrade};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, Stream, StreamExt};
use sdc_protocol::{
    ClientMessage, ImageFrame, PluginClientMessage, PluginServerMessage, ServerMessage,
    PROTOCOL_VERSION,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::clients::ClientBinaries;
use crate::engine::{EngineEvent, Out, PluginOut};
use crate::http::{AppState, DeckUser};

static NEXT_CLIENT: AtomicU64 = AtomicU64::new(1);
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Slots a single plugin connection may hold.
const MAX_SLOTS: usize = 256;
/// Largest image a plugin slot may ask for, in px.
const MAX_SLOT_PX: u32 = 1024;

/// Upgrade handler for `/ws`.
pub async fn upgrade(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = handle_deck(socket, state.engine, state.binaries, None).await {
            tracing::warn!("connection ended: {e:#}");
        }
    })
}

/// Upgrade handler for `/plugin/ws`: the Elgato plugin.
pub async fn plugin_upgrade(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = handle_plugin(socket, state.engine).await {
            tracing::warn!("plugin connection ended: {e:#}");
        }
    })
}

/// Upgrade handler for `/deck/ws` (already past the SSO check).
pub async fn deck_upgrade(
    State(state): State<AppState>,
    Extension(user): Extension<DeckUser>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // Cookies authenticate this route, so refuse pages of other origins (cross-site hijacking).
    if !same_origin(&headers) {
        tracing::warn!("rejected deck websocket from {}: foreign origin", user.0);
        return (StatusCode::FORBIDDEN, "cross-origin websocket refused\n").into_response();
    }
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = handle_deck(socket, state.engine, state.binaries, Some(user.0)).await {
            tracing::warn!("deck connection ended: {e:#}");
        }
    })
}

/// No `Origin` (not a browser) or one whose host is the request's `Host`.
fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let authority = origin.split_once("://").map_or(origin, |(_, rest)| rest);
    host.is_some_and(|h| h.eq_ignore_ascii_case(authority))
}

/// Browser deck ids are `web-...` so a browser can never take over a physical deck's identity.
fn web_client_id(raw: &str) -> String {
    let clean: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(48)
        .collect();
    if clean.starts_with("web-") {
        clean
    } else {
        format!("web-{clean}")
    }
}

fn text(msg: &impl Serialize) -> Result<Message> {
    Ok(Message::Text(serde_json::to_string(msg)?.into()))
}

/// The first text message of a connection, parsed; errors if it takes longer than
/// [`HELLO_TIMEOUT`] or the socket closes first.
async fn read_hello<T: DeserializeOwned>(
    stream: &mut (impl Stream<Item = Result<Message, axum::Error>> + Unpin),
) -> Result<T> {
    tokio::time::timeout(HELLO_TIMEOUT, async {
        loop {
            match stream.next().await {
                Some(Ok(Message::Text(t))) => return Ok(serde_json::from_str(&t)?),
                Some(Ok(Message::Close(_))) | None => bail!("closed before hello"),
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(e.into()),
            }
        }
    })
    .await
    .map_err(|_| anyhow!("timed out waiting for hello"))?
}

/// The error to tell a client whose hello has the wrong protocol version.
fn version_error(version: u32) -> Option<String> {
    (version != PROTOCOL_VERSION).then(|| {
        format!("unsupported protocol version {version}, server speaks {PROTOCOL_VERSION}")
    })
}

/// Runs a connection after its hello: sends what arrives on `rx` (as `encode` says) and hands
/// every message from the socket to `on_msg`, until either side closes.
async fn pump<O, M: DeserializeOwned>(
    mut sink: SplitSink<WebSocket, Message>,
    mut stream: SplitStream<WebSocket>,
    mut rx: UnboundedReceiver<O>,
    encode: impl Fn(O) -> Result<Message>,
    mut on_msg: impl FnMut(M),
) -> Result<()> {
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Some(out) => sink.send(encode(out)?).await?,
                None => return Ok(()),
            },
            msg = stream.next() => match msg {
                None | Some(Ok(Message::Close(_))) => return Ok(()),
                Some(Err(e)) => return Err(e.into()),
                Some(Ok(Message::Text(t))) => match serde_json::from_str(&t) {
                    Ok(msg) => on_msg(msg),
                    Err(e) => tracing::warn!("bad message: {e}"),
                },
                Some(Ok(_)) => {}
            },
        }
    }
}

/// A deck: a physical one on `/ws`, or (`web`: the SSO user) a browser on `/deck/ws`.
async fn handle_deck(
    ws: WebSocket,
    engine: UnboundedSender<EngineEvent>,
    binaries: Arc<ClientBinaries>,
    web: Option<String>,
) -> Result<()> {
    let (mut sink, mut stream) = ws.split();

    let ClientMessage::Hello {
        protocol_version,
        client_id,
        mut device,
        client_target,
        client_sha256,
        device_name,
    } = read_hello(&mut stream).await?
    else {
        bail!("first message was not hello");
    };
    let invalid = version_error(protocol_version).or_else(|| {
        (device.rows == 0
            || device.cols == 0
            || device.key_size_px == 0
            || device.key_size_px > 1024)
            .then(|| "invalid device info".to_string())
    });
    if let Some(message) = invalid {
        sink.send(text(&ServerMessage::Error {
            message: message.clone(),
        })?)
        .await?;
        bail!("client {client_id}: {message}");
    }
    let client_id = match &web {
        Some(email) => {
            let id = web_client_id(&client_id);
            tracing::info!("{email} opened browser deck {id}");
            id
        }
        None => client_id,
    };
    let key_count = device.key_count();
    // Device-ready frames only for a physical deck whose format we can produce; everyone else
    // keeps getting upright JPEGs.
    if web.is_some()
        || !device
            .native
            .is_some_and(|n| n.server_can_produce(device.key_size_px))
    {
        device.native = None;
    }

    sink.send(text(&ServerMessage::Welcome {
        protocol_version: PROTOCOL_VERSION,
        native_frames: device.native.is_some(),
    })?)
    .await?;
    let update = web
        .is_none()
        .then(|| binaries.update_for(client_target.as_deref(), client_sha256.as_deref()))
        .flatten();
    if let Some(update) = update {
        tracing::info!(
            "client {client_id} ({}) runs a stale binary, telling it to update",
            client_target.as_deref().unwrap_or("?")
        );
        sink.send(text(&update)?).await?;
    }

    let id = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::unbounded_channel();
    let _ = engine.send(EngineEvent::ClientConnected {
        web: web.is_some(),
        id,
        client_id,
        device,
        want_device: device_name.filter(|d| !d.is_empty()),
        tx,
    });
    let result = pump(
        sink,
        stream,
        rx,
        |out| match out {
            Out::Frame(f) => Ok(Message::Binary(ImageFrame::encode(f.key, &f.jpeg).into())),
            Out::Brightness(percent) => text(&ServerMessage::Brightness { percent }),
        },
        |msg: ClientMessage| {
            let ev = match msg {
                ClientMessage::KeyDown { key } if key < key_count => EngineEvent::Key {
                    id,
                    key,
                    down: true,
                },
                ClientMessage::KeyUp { key } if key < key_count => EngineEvent::Key {
                    id,
                    key,
                    down: false,
                },
                ClientMessage::Wake => EngineEvent::Wake { id },
                // another hello, or a key the deck does not have
                _ => return,
            };
            let _ = engine.send(ev);
        },
    )
    .await;
    let _ = engine.send(EngineEvent::ClientDisconnected(id));
    result
}

fn plugin_json(out: PluginOut) -> Result<Message> {
    let b64 = |png: Vec<u8>| BASE64.encode(png);
    text(&match out {
        PluginOut::Exports(items) => PluginServerMessage::Exports { items },
        PluginOut::KeyImage { slot, png } => PluginServerMessage::KeyImage {
            slot,
            png_base64: b64(png),
        },
        PluginOut::DialFeedback {
            slot,
            title,
            value,
            indicator,
            icon_png,
        } => PluginServerMessage::DialFeedback {
            slot,
            title,
            value,
            indicator,
            icon_png_base64: icon_png.map(b64),
        },
        PluginOut::SlotError { slot, message } => PluginServerMessage::SlotError { slot, message },
    })
}

async fn handle_plugin(ws: WebSocket, engine: UnboundedSender<EngineEvent>) -> Result<()> {
    let (mut sink, mut stream) = ws.split();

    let PluginClientMessage::PluginHello {
        protocol_version,
        client_id,
    } = read_hello(&mut stream).await?
    else {
        bail!("first message was not plugin_hello");
    };
    if let Some(message) = version_error(protocol_version) {
        sink.send(text(&PluginServerMessage::Error {
            message: message.clone(),
        })?)
        .await?;
        bail!("plugin {client_id}: {message}");
    }
    sink.send(text(&PluginServerMessage::PluginWelcome {
        protocol_version: PROTOCOL_VERSION,
    })?)
    .await?;

    let id = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::unbounded_channel();
    let _ = engine.send(EngineEvent::PluginConnected {
        id,
        client_id,
        tx: tx.clone(),
    });
    // The slots this connection subscribed: input for any other slot is dropped here.
    let mut slots = HashSet::new();
    let result = pump(sink, stream, rx, plugin_json, |msg: PluginClientMessage| {
        use PluginClientMessage as M;
        match &msg {
            M::PluginHello { .. } => return,
            M::ListExports => {}
            M::Subscribe { slot, size_px, .. } => {
                let reject = if !(1..=MAX_SLOT_PX).contains(size_px) {
                    Some(format!("size_px {size_px} is not 1-{MAX_SLOT_PX}"))
                } else if slots.len() >= MAX_SLOTS && !slots.contains(slot) {
                    Some(format!("more than {MAX_SLOTS} slots"))
                } else {
                    None
                };
                if let Some(message) = reject {
                    // The rejected slot loses whatever it showed before.
                    if slots.remove(slot) {
                        let msg = M::Unsubscribe { slot: slot.clone() };
                        let _ = engine.send(EngineEvent::Plugin { id, msg });
                    }
                    let _ = tx.send(PluginOut::SlotError {
                        slot: slot.clone(),
                        message,
                    });
                    return;
                }
                slots.insert(slot.clone());
            }
            M::Unsubscribe { slot } => {
                if !slots.remove(slot) {
                    return;
                }
            }
            M::SlotDown { slot }
            | M::SlotUp { slot }
            | M::DialRotate { slot, .. }
            | M::DialDown { slot }
            | M::DialUp { slot }
            | M::TouchTap { slot } => {
                if !slots.contains(slot) {
                    return;
                }
            }
        }
        let _ = engine.send(EngineEvent::Plugin { id, msg });
    })
    .await;
    let _ = engine.send(EngineEvent::PluginDisconnected(id));
    result
}
