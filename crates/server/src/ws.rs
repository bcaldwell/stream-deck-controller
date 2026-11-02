//! WebSocket server for clients (`/ws`, bearer token), the Elgato plugin (`/plugin/ws`, same
//! token) and browser decks (`/deck/ws`, SSO).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Extension, State, WebSocketUpgrade};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::response::Response;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use sdc_protocol::{
    ClientMessage, ImageFrame, PluginClientMessage, PluginServerMessage, ServerMessage,
    PROTOCOL_VERSION,
};
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::clients::ClientBinaries;
use crate::engine::{EngineEvent, Out, PluginOut};
use crate::http::{self, DeckAuth, DeckUser};

static NEXT_CLIENT: AtomicU64 = AtomicU64::new(1);
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct WsState {
    pub engine: UnboundedSender<EngineEvent>,
    pub binaries: Arc<ClientBinaries>,
    /// Shared client token; `None` accepts everyone.
    pub token: Option<Arc<str>>,
    pub deck_auth: DeckAuth,
}

/// Upgrade handler for `/ws`.
pub async fn upgrade(
    State(state): State<WsState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(denied) = http::authorize(state.token.as_deref(), &headers) {
        tracing::warn!("rejected websocket connection: missing or invalid client token");
        return denied;
    }
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = handle_conn(socket, state, None).await {
            tracing::warn!("connection ended: {e:#}");
        }
    })
}

/// Upgrade handler for `/plugin/ws`: the Elgato plugin, authenticated like `/ws`.
pub async fn plugin_upgrade(
    State(state): State<WsState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(denied) = http::authorize(state.token.as_deref(), &headers) {
        tracing::warn!("rejected plugin websocket connection: missing or invalid client token");
        return denied;
    }
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = handle_plugin(socket, state.engine).await {
            tracing::warn!("plugin connection ended: {e:#}");
        }
    })
}

/// Upgrade handler for `/deck/ws` (already past the SSO check). Never looks at the client token.
pub async fn deck_upgrade(
    State(state): State<WsState>,
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
        if let Err(e) = handle_conn(socket, state, Some(user.0)).await {
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

/// `deck`: the SSO user of a browser connection (`/deck/ws`); `None` for a normal client.
async fn handle_conn(ws: WebSocket, state: WsState, deck: Option<String>) -> Result<()> {
    let WsState {
        engine, binaries, ..
    } = state;
    let (mut sink, mut stream) = ws.split();

    // Hello
    let hello = tokio::time::timeout(HELLO_TIMEOUT, async {
        loop {
            match stream.next().await {
                Some(Ok(Message::Text(t))) => {
                    return Ok(serde_json::from_str::<ClientMessage>(&t)?)
                }
                Some(Ok(Message::Close(_))) | None => bail!("closed before hello"),
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(e.into()),
            }
        }
    })
    .await
    .map_err(|_| anyhow!("timed out waiting for hello"))??;

    let ClientMessage::Hello {
        protocol_version,
        client_id,
        device,
        client_target,
        client_sha256,
        mirror,
        device_name,
    } = hello
    else {
        bail!("first message was not hello");
    };
    if protocol_version != PROTOCOL_VERSION {
        let msg = ServerMessage::Error {
            message: format!(
                "unsupported protocol version {protocol_version}, server speaks {PROTOCOL_VERSION}"
            ),
        };
        sink.send(Message::Text(serde_json::to_string(&msg)?.into()))
            .await?;
        let _ = sink.close().await;
        bail!("client {client_id} protocol version {protocol_version} unsupported");
    }
    if device.rows == 0 || device.cols == 0 || device.key_size_px == 0 || device.key_size_px > 1024
    {
        let msg = ServerMessage::Error {
            message: "invalid device info".into(),
        };
        sink.send(Message::Text(serde_json::to_string(&msg)?.into()))
            .await?;
        bail!("client {client_id} sent invalid device info");
    }
    let client_id = if deck.is_some() {
        web_client_id(&client_id)
    } else {
        client_id
    };
    let mirror = mirror.filter(|_| deck.is_some());
    if let Some(email) = &deck {
        tracing::info!(
            "{email} opened browser deck {client_id}{}",
            mirror
                .as_deref()
                .map(|m| format!(" mirroring {m}"))
                .unwrap_or_default()
        );
    }
    let key_count = device.key_count();

    sink.send(Message::Text(
        serde_json::to_string(&ServerMessage::Welcome {
            protocol_version: PROTOCOL_VERSION,
        })?
        .into(),
    ))
    .await?;

    let update = deck
        .is_none()
        .then(|| binaries.update_for(client_target.as_deref(), client_sha256.as_deref()))
        .flatten();
    if let Some(update) = update {
        tracing::info!(
            "client {client_id} ({}) runs a stale binary, telling it to update",
            client_target.as_deref().unwrap_or("?")
        );
        sink.send(Message::Text(serde_json::to_string(&update)?.into()))
            .await?;
    }

    let id = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = mpsc::unbounded_channel::<Out>();
    let _ = match mirror {
        Some(mirror) => engine.send(EngineEvent::ViewerConnected {
            id,
            mirror,
            device,
            tx,
        }),
        None => engine.send(EngineEvent::ClientConnected {
            web: deck.is_some(),
            id,
            client_id,
            device,
            want_device: device_name.filter(|d| !d.is_empty()),
            tx,
        }),
    };

    let result: Result<()> = async {
        loop {
            tokio::select! {
                out = rx.recv() => match out {
                    Some(Out::Frame(f)) => sink.send(Message::Binary(ImageFrame::encode(f.key, &f.jpeg).into())).await?,
                    Some(Out::Brightness(percent)) => {
                        let msg = ServerMessage::Brightness { percent };
                        sink.send(Message::Text(serde_json::to_string(&msg)?.into())).await?
                    }
                    Some(Out::Error(message)) => {
                        let msg = ServerMessage::Error { message };
                        sink.send(Message::Text(serde_json::to_string(&msg)?.into())).await?
                    }
                    None => return Ok(()),
                },
                msg = stream.next() => match msg {
                    None | Some(Ok(Message::Close(_))) => return Ok(()),
                    Some(Err(e)) => return Err(e.into()),
                    Some(Ok(Message::Text(t))) => match serde_json::from_str::<ClientMessage>(&t) {
                        Ok(ClientMessage::KeyDown { key }) if key < key_count => {
                            let _ = engine.send(EngineEvent::KeyDown { id, key });
                        }
                        Ok(ClientMessage::KeyUp { key }) if key < key_count => {
                            let _ = engine.send(EngineEvent::KeyUp { id, key });
                        }
                        Ok(ClientMessage::Wake) => {
                            let _ = engine.send(EngineEvent::Wake { id });
                        }
                        Ok(_) => tracing::debug!("ignoring unexpected client message"),
                        Err(e) => tracing::warn!("bad client message: {e}"),
                    },
                    Some(Ok(_)) => {}
                },
            }
        }
    }
    .await;

    let _ = engine.send(EngineEvent::ClientDisconnected(id));
    result
}

fn plugin_json(out: PluginOut) -> serde_json::Result<String> {
    let b64 = |png: Vec<u8>| BASE64.encode(png);
    serde_json::to_string(&match out {
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
    let send = |m: &PluginServerMessage| serde_json::to_string(m).map(|j| Message::Text(j.into()));

    let hello = tokio::time::timeout(HELLO_TIMEOUT, async {
        loop {
            match stream.next().await {
                Some(Ok(Message::Text(t))) => {
                    return Ok(serde_json::from_str::<PluginClientMessage>(&t)?)
                }
                Some(Ok(Message::Close(_))) | None => bail!("closed before hello"),
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(e.into()),
            }
        }
    })
    .await
    .map_err(|_| anyhow!("timed out waiting for hello"))??;
    let PluginClientMessage::PluginHello {
        protocol_version,
        client_id,
    } = hello
    else {
        bail!("first message was not plugin_hello");
    };
    if protocol_version != PROTOCOL_VERSION {
        let msg = PluginServerMessage::Error {
            message: format!(
                "unsupported protocol version {protocol_version}, server speaks {PROTOCOL_VERSION}"
            ),
        };
        sink.send(send(&msg)?).await?;
        let _ = sink.close().await;
        bail!("plugin {client_id} protocol version {protocol_version} unsupported");
    }
    sink.send(send(&PluginServerMessage::PluginWelcome {
        protocol_version: PROTOCOL_VERSION,
    })?)
    .await?;

    let id = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = mpsc::unbounded_channel::<PluginOut>();
    let _ = engine.send(EngineEvent::PluginConnected { id, client_id, tx });

    let result: Result<()> = async {
        loop {
            tokio::select! {
                out = rx.recv() => match out {
                    Some(out) => sink.send(Message::Text(plugin_json(out)?.into())).await?,
                    None => return Ok(()),
                },
                msg = stream.next() => match msg {
                    None | Some(Ok(Message::Close(_))) => return Ok(()),
                    Some(Err(e)) => return Err(e.into()),
                    Some(Ok(Message::Text(t))) => match serde_json::from_str::<PluginClientMessage>(&t) {
                        Ok(PluginClientMessage::PluginHello { .. }) => {}
                        Ok(msg) => {
                            let _ = engine.send(EngineEvent::Plugin { id, msg });
                        }
                        Err(e) => tracing::warn!("bad plugin message: {e}"),
                    },
                    Some(Ok(_)) => {}
                },
            }
        }
    }
    .await;

    let _ = engine.send(EngineEvent::PluginDisconnected(id));
    result
}
