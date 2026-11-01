//! Home Assistant WebSocket client.
//!
//! The rest of the server only sees a channel boundary: [`HaCommand`]s go in,
//! [`HaEvent`]s come out (delivered as [`EngineEvent::Ha`]). Tests can therefore
//! substitute a fake by driving those channels directly.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::tungstenite::Message;

use crate::engine::EngineEvent;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EntityState {
    pub entity_id: String,
    pub state: String,
    #[serde(default)]
    pub attributes: Value,
}

#[derive(Debug, Clone)]
pub enum HaEvent {
    /// Full state snapshot (initial connect and every reconnect).
    Seed(Vec<EntityState>),
    Changed(EntityState),
    Removed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum HaCommand {
    CallService {
        domain: String,
        service: String,
        data: Value,
    },
}

#[derive(Debug, Clone)]
pub struct HaConfig {
    pub ws_url: String,
    pub token: String,
}

impl HaConfig {
    pub fn new(url: &str, token: &str) -> Self {
        Self {
            ws_url: ws_url(url),
            token: token.to_string(),
        }
    }

    /// Reads `HA_URL` / `HA_TOKEN`; `None` if either is unset.
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("HA_URL").ok().filter(|s| !s.is_empty())?;
        let token = std::env::var("HA_TOKEN").ok().filter(|s| !s.is_empty())?;
        Some(Self::new(&url, &token))
    }
}

/// `http://host:8123` -> `ws://host:8123/api/websocket` (https -> wss).
pub fn ws_url(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    let u = if let Some(rest) = u.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = u.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        u.to_string()
    };
    if u.ends_with("/api/websocket") {
        u
    } else {
        format!("{u}/api/websocket")
    }
}

/// Runs forever (until `cmds` closes), reconnecting with exponential backoff.
pub async fn run(
    cfg: HaConfig,
    mut cmds: UnboundedReceiver<HaCommand>,
    events: UnboundedSender<EngineEvent>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match session(&cfg, &mut cmds, &events, &mut backoff).await {
            Ok(()) => return,
            Err(e) => {
                tracing::warn!("home assistant connection lost: {e:#}; retrying in {backoff:?}")
            }
        }
        // Commands issued while disconnected are dropped rather than fired late.
        let sleep = tokio::time::sleep(backoff);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                cmd = cmds.recv() => match cmd {
                    Some(c) => tracing::warn!("home assistant offline, dropping {c:?}"),
                    None => return,
                },
            }
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

const SUBSCRIBE_ID: u64 = 1;
const STATES_ID: u64 = 2;

/// One connection. `Ok(())` means the command channel closed (shut down).
async fn session(
    cfg: &HaConfig,
    cmds: &mut UnboundedReceiver<HaCommand>,
    events: &UnboundedSender<EngineEvent>,
    backoff: &mut Duration,
) -> Result<()> {
    tracing::info!("connecting to home assistant at {}", cfg.ws_url);
    let (ws, _) = tokio_tungstenite::connect_async(cfg.ws_url.as_str())
        .await
        .context("connect")?;
    let (mut tx, mut rx) = ws.split();

    // auth handshake
    loop {
        let msg = next_json(&mut rx).await?;
        match msg["type"].as_str() {
            Some("auth_required") => {
                tx.send(Message::Text(
                    json!({"type": "auth", "access_token": cfg.token}).to_string(),
                ))
                .await?;
            }
            Some("auth_ok") => break,
            Some("auth_invalid") => bail!("home assistant rejected the access token"),
            other => bail!("unexpected message during auth: {other:?}"),
        }
    }
    tracing::info!("home assistant authenticated");
    *backoff = Duration::from_secs(1);

    // Subscribe before fetching states so no change falls in between.
    send_json(
        &mut tx,
        json!({"id": SUBSCRIBE_ID, "type": "subscribe_events", "event_type": "state_changed"}),
    )
    .await?;
    send_json(&mut tx, json!({"id": STATES_ID, "type": "get_states"})).await?;

    let mut next_id = 10u64;
    let period = Duration::from_secs(30);
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    loop {
        tokio::select! {
            msg = next_json(&mut rx) => handle_message(msg?, events)?,
            cmd = cmds.recv() => match cmd {
                None => return Ok(()),
                Some(HaCommand::CallService { domain, service, data }) => {
                    tracing::info!("call_service {domain}.{service} {data}");
                    next_id += 1;
                    let mut m = json!({"id": next_id, "type": "call_service", "domain": domain, "service": service});
                    if data.as_object().is_some_and(|o| !o.is_empty()) {
                        m["service_data"] = data;
                    }
                    send_json(&mut tx, m).await?;
                }
            },
            _ = ping.tick() => tx.send(Message::Ping(Vec::new())).await?,
        }
    }
}

fn handle_message(msg: Value, events: &UnboundedSender<EngineEvent>) -> Result<()> {
    let emit = |e: HaEvent| {
        let _ = events.send(EngineEvent::Ha(e));
    };
    match msg["type"].as_str() {
        Some("result") => {
            if msg["success"] != json!(true) {
                tracing::warn!("home assistant command failed: {}", msg["error"]);
            } else if msg["id"].as_u64() == Some(STATES_ID) {
                let states: Vec<EntityState> = msg["result"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| serde_json::from_value(v.clone()).ok())
                            .collect()
                    })
                    .unwrap_or_default();
                tracing::info!("home assistant seeded {} states", states.len());
                emit(HaEvent::Seed(states));
            }
        }
        Some("event") if msg["id"].as_u64() == Some(SUBSCRIBE_ID) => {
            let data = &msg["event"]["data"];
            match serde_json::from_value::<EntityState>(data["new_state"].clone()) {
                Ok(s) => emit(HaEvent::Changed(s)),
                Err(_) => {
                    if let Some(id) = data["entity_id"].as_str() {
                        emit(HaEvent::Removed(id.to_string()));
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

async fn send_json<S>(tx: &mut S, v: Value) -> Result<()>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    tx.send(Message::Text(v.to_string())).await?;
    Ok(())
}

async fn next_json<S>(rx: &mut S) -> Result<Value>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match rx.next().await {
            None => return Err(anyhow!("connection closed")),
            Some(Err(e)) => return Err(e.into()),
            Some(Ok(Message::Text(t))) => return Ok(serde_json::from_str(&t)?),
            Some(Ok(Message::Close(_))) => return Err(anyhow!("closed by home assistant")),
            Some(Ok(_)) => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_derivation() {
        assert_eq!(
            ws_url("http://ha.local:8123"),
            "ws://ha.local:8123/api/websocket"
        );
        assert_eq!(
            ws_url("https://ha.example.com/"),
            "wss://ha.example.com/api/websocket"
        );
        assert_eq!(ws_url("ws://x:1/api/websocket"), "ws://x:1/api/websocket");
    }
}
