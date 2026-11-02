//! End-to-end tests: a real WebSocket client talks to the real server stack, with
//! Home Assistant replaced either by channels (fake) or by a mock HA WebSocket server.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use sdc_protocol::{
    sha256_hex, ClientMessage, DeviceInfo, ImageFrame, PluginClientMessage, PluginServerMessage,
    ServerMessage, SlotKind, PROTOCOL_VERSION,
};
use sdc_server::clients::ClientBinaries;
use sdc_server::engine::{self, EngineEvent};
use sdc_server::ha::{self, EntityState, HaCommand, HaConfig, HaEvent};
use sdc_server::http;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const CONFIG: &str = r##"
sdc.page("home", {
  sdc.button{
    render = function()
      local l = ha.state("light.office")
      return { text = "Office", bg = (l and l.state == "on") and "#f5b400" or "#222222" }
    end,
    press = function() ha.call("light", "toggle", { entity_id = "light.office" }) end,
  },
  sdc.button{ render = { text = "Static" } },
})
sdc.device("deck", { pages = { "home" } })

sdc.export("lamp", sdc.button{
  label = "Lamp",
  render = function()
    local l = ha.state("light.office")
    return { text = "Lamp", bg = (l and l.state == "on") and "#f5b400" or "#222222" }
  end,
  press = function() ha.call("light", "toggle", { entity_id = "light.office" }) end,
})
sdc.export("static", sdc.button{ render = { text = "Static" } })
sdc.export("dimmer", sdc.dial{
  render = function()
    local l = ha.state("light.dial")
    return { title = "Dimmer", value = l and l.state or "?", indicator = 40 }
  end,
  rotate = function(ctx, ticks) ha.call("light", "turn_on", { entity_id = "light.dial", step = ticks }) end,
})
"##;

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("sdc-it-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn device() -> DeviceInfo {
    DeviceInfo {
        model: "Test Deck".into(),
        serial: None,
        rows: 3,
        cols: 5,
        key_size_px: 72,
        native: None,
    }
}

async fn start_server(
    tag: &str,
    ha_tx: UnboundedSender<HaCommand>,
) -> (UnboundedSender<EngineEvent>, String, TempDir) {
    start_server_with(tag, ha_tx, ClientBinaries::default()).await
}

async fn start_server_with(
    tag: &str,
    ha_tx: UnboundedSender<HaCommand>,
    binaries: ClientBinaries,
) -> (UnboundedSender<EngineEvent>, String, TempDir) {
    start_server_full(tag, ha_tx, binaries, None).await
}

async fn start_server_full(
    tag: &str,
    ha_tx: UnboundedSender<HaCommand>,
    binaries: ClientBinaries,
    token: Option<&str>,
) -> (UnboundedSender<EngineEvent>, String, TempDir) {
    start_server_deck(tag, ha_tx, binaries, token, http::DeckAuth::Sso).await
}

async fn start_server_deck(
    tag: &str,
    ha_tx: UnboundedSender<HaCommand>,
    binaries: ClientBinaries,
    token: Option<&str>,
    deck_auth: http::DeckAuth,
) -> (UnboundedSender<EngineEvent>, String, TempDir) {
    let dir = TempDir::new(tag);
    let cfg = dir.0.join("config.lua");
    std::fs::write(&cfg, CONFIG).unwrap();
    let engine_tx = engine::spawn(cfg, Some(ha_tx));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws", listener.local_addr().unwrap());
    tokio::spawn(http::serve(
        listener,
        engine_tx.clone(),
        Arc::new(binaries),
        token.map(Into::into),
        deck_auth,
    ));
    (engine_tx, url, dir)
}

async fn connect(url: &str) -> Client {
    connect_as(url, None, None).await
}

async fn connect_as(url: &str, target: Option<&str>, sha256: Option<&str>) -> Client {
    connect_with(url, target, sha256, device()).await.0
}

async fn connect_with(
    url: &str,
    target: Option<&str>,
    sha256: Option<&str>,
    device: DeviceInfo,
) -> (Client, bool) {
    let (mut c, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let hello = ClientMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        client_id: "test".into(),
        device,
        client_target: target.map(Into::into),
        client_sha256: sha256.map(Into::into),
        mirror: None,
        device_name: None,
    };
    c.send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
        .await
        .unwrap();
    match next_msg(&mut c).await {
        Message::Text(t) => match serde_json::from_str(&t).unwrap() {
            ServerMessage::Welcome { native_frames, .. } => (c, native_frames),
            other => panic!("expected welcome, got {other:?}"),
        },
        other => panic!("expected welcome, got {other:?}"),
    }
}

async fn next_msg(c: &mut Client) -> Message {
    tokio::time::timeout(Duration::from_secs(5), c.next())
        .await
        .expect("timeout")
        .unwrap()
        .unwrap()
}

/// The next image frame; the brightness message every connect sends is skipped.
async fn next_frame(c: &mut Client) -> (u8, Vec<u8>) {
    loop {
        match next_msg(c).await {
            Message::Binary(b) => {
                let f = ImageFrame::decode(&b).unwrap();
                return (f.key, f.image.to_vec());
            }
            Message::Text(t) if t.contains("\"brightness\"") => {}
            other => panic!("expected binary frame, got {other:?}"),
        }
    }
}

/// Frames that arrive within `quiet` of each other.
async fn drain_frames(c: &mut Client, quiet: Duration) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    while let Ok(Some(Ok(msg))) = tokio::time::timeout(quiet, c.next()).await {
        match msg {
            Message::Binary(b) => {
                let f = ImageFrame::decode(&b).unwrap();
                out.push((f.key, f.image.to_vec()));
            }
            Message::Text(t) if t.contains("\"brightness\"") => {}
            _ => break,
        }
    }
    out
}

fn on(id: &str) -> EntityState {
    EntityState {
        entity_id: id.into(),
        state: "on".into(),
        attributes: json!({}),
    }
}

async fn keydown(c: &mut Client, key: u8) {
    c.send(Message::Text(
        serde_json::to_string(&ClientMessage::KeyDown { key })
            .unwrap()
            .into(),
    ))
    .await
    .unwrap();
}

#[tokio::test]
async fn fake_ha_full_flow() {
    let (ha_tx, mut ha_rx) = mpsc::unbounded_channel();
    let (engine_tx, url, _dir) = start_server("fake", ha_tx).await;
    let mut c = connect(&url).await;

    // 15 initial frames, one per key, valid JPEGs at 72x72
    let mut keys = Vec::new();
    for _ in 0..15 {
        let (k, jpeg) = next_frame(&mut c).await;
        let img = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((img.width(), img.height()), (72, 72));
        keys.push(k);
    }
    keys.sort_unstable();
    assert_eq!(keys, (0..15).collect::<Vec<u8>>());

    // key press -> call_service
    keydown(&mut c, 0).await;
    let cmd = tokio::time::timeout(Duration::from_secs(5), ha_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        cmd,
        HaCommand::CallService {
            domain: "light".into(),
            service: "toggle".into(),
            data: json!({"entity_id": "light.office"})
        }
    );

    // the press flashes key 0 and then restores it
    let flash = drain_frames(&mut c, Duration::from_millis(700)).await;
    assert_eq!(flash.iter().map(|f| f.0).collect::<Vec<_>>(), [0, 0]);

    // state change -> only key 0 re-sent
    engine_tx
        .send(EngineEvent::Ha(HaEvent::Changed(on("light.office"))))
        .unwrap();
    let (k, _) = next_frame(&mut c).await;
    assert_eq!(k, 0);
    assert!(drain_frames(&mut c, Duration::from_millis(300))
        .await
        .is_empty());

    // unrelated change -> nothing
    engine_tx
        .send(EngineEvent::Ha(HaEvent::Changed(on("light.other"))))
        .unwrap();
    assert!(drain_frames(&mut c, Duration::from_millis(300))
        .await
        .is_empty());
}

#[tokio::test]
async fn rejects_wrong_protocol_version() {
    let (ha_tx, _ha_rx) = mpsc::unbounded_channel();
    let (_e, url, _dir) = start_server("badver", ha_tx).await;
    let (mut c, _) = tokio_tungstenite::connect_async(url.as_str())
        .await
        .unwrap();
    let hello = ClientMessage::Hello {
        protocol_version: 999,
        client_id: "x".into(),
        device: device(),
        client_target: None,
        client_sha256: None,
        mirror: None,
        device_name: None,
    };
    c.send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
        .await
        .unwrap();
    match next_msg(&mut c).await {
        Message::Text(t) => assert!(matches!(
            serde_json::from_str(&t).unwrap(),
            ServerMessage::Error { .. }
        )),
        other => panic!("expected error, got {other:?}"),
    }
}

// ---- client distribution ------------------------------------------------

/// Minimal HTTP/1.0 GET against `ws://addr/ws`-style server urls: (status, body).
async fn http_get(ws_url: &str, path: &str) -> (u16, Vec<u8>) {
    http_get_auth(ws_url, path, None).await
}

async fn http_get_auth(ws_url: &str, path: &str, auth: Option<&str>) -> (u16, Vec<u8>) {
    let auth = auth.map_or(String::new(), |a| format!("Authorization: {a}\r\n"));
    let addr = ws_url
        .strip_prefix("ws://")
        .unwrap()
        .strip_suffix("/ws")
        .unwrap();
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\n{auth}\r\n").as_bytes())
        .await
        .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, raw[split..].to_vec())
}

#[tokio::test]
async fn serves_clients_and_health() {
    let (ha_tx, _ha_rx) = mpsc::unbounded_channel();
    let bin = b"\x7fELF fake client";
    let (_e, url, _dir) =
        start_server_with("http", ha_tx, ClientBinaries::with("aarch64-linux", bin)).await;

    let (st, body) = http_get(&url, "/client/aarch64-linux").await;
    assert_eq!((st, body.as_slice()), (200, &bin[..]));
    let (st, body) = http_get(&url, "/client/aarch64-linux.sha256").await;
    assert_eq!(st, 200);
    assert_eq!(String::from_utf8(body).unwrap().trim(), sha256_hex(bin));
    assert_eq!(http_get(&url, "/client/x86_64-linux").await.0, 404);
    assert_eq!(http_get(&url, "/client/x86_64-linux.sha256").await.0, 404);

    assert_eq!(http_get(&url, "/healthz").await, (200, b"ok".to_vec()));
    assert_eq!(http_get(&url, "/nope").await.0, 404);
}

// ---- client token ---------------------------------------------------------

/// Handshake status for a websocket upgrade carrying `auth` as the Authorization header.
async fn ws_handshake_status(url: &str, auth: Option<&str>) -> u16 {
    let mut req = url.into_client_request().unwrap();
    if let Some(a) = auth {
        req.headers_mut()
            .insert("Authorization", a.parse().unwrap());
    }
    match tokio_tungstenite::connect_async(req).await {
        Ok((_, resp)) => resp.status().as_u16(),
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => resp.status().as_u16(),
        Err(e) => panic!("unexpected handshake error: {e}"),
    }
}

#[tokio::test]
async fn client_token_guards_ws_and_client_downloads() {
    let (ha_tx, _ha_rx) = mpsc::unbounded_channel();
    let bin = b"\x7fELF fake client";
    let (_e, url, _dir) = start_server_full(
        "token",
        ha_tx,
        ClientBinaries::with("aarch64-linux", bin),
        Some("s3cret"),
    )
    .await;

    // websocket: 401 without / with a wrong token, upgrade with the right one
    assert_eq!(ws_handshake_status(&url, None).await, 401);
    assert_eq!(ws_handshake_status(&url, Some("Bearer wrong")).await, 401);
    assert_eq!(ws_handshake_status(&url, Some("s3cret")).await, 401);
    assert_eq!(ws_handshake_status(&url, Some("Bearer s3cret")).await, 101);

    // a full session works with the token
    let mut req = url.as_str().into_client_request().unwrap();
    req.headers_mut()
        .insert("Authorization", "Bearer s3cret".parse().unwrap());
    let (mut c, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let hello = ClientMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        client_id: "test".into(),
        device: device(),
        client_target: None,
        client_sha256: None,
        mirror: None,
        device_name: None,
    };
    c.send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
        .await
        .unwrap();
    assert!(matches!(next_msg(&mut c).await, Message::Text(_)));

    // client downloads
    for path in ["/client/aarch64-linux", "/client/aarch64-linux.sha256"] {
        assert_eq!(http_get(&url, path).await.0, 401);
        assert_eq!(http_get_auth(&url, path, Some("Bearer wrong")).await.0, 401);
        assert_eq!(
            http_get_auth(&url, path, Some("Bearer s3cret")).await.0,
            200
        );
    }
    // unknown targets are still 404 once authorized, 401 before
    assert_eq!(http_get(&url, "/client/x86_64-linux").await.0, 401);
    assert_eq!(
        http_get_auth(&url, "/client/x86_64-linux", Some("Bearer s3cret"))
            .await
            .0,
        404
    );

    // health stays open
    assert_eq!(http_get(&url, "/healthz").await, (200, b"ok".to_vec()));
}

#[tokio::test]
async fn no_token_configured_allows_everyone() {
    let (ha_tx, _ha_rx) = mpsc::unbounded_channel();
    let (_e, url, _dir) = start_server("open", ha_tx).await;
    assert_eq!(ws_handshake_status(&url, None).await, 101);
    assert_eq!(
        ws_handshake_status(&url, Some("Bearer whatever")).await,
        101
    );
}

#[tokio::test]
async fn stale_client_gets_update_after_welcome() {
    let (ha_tx, _ha_rx) = mpsc::unbounded_channel();
    let bin = b"new client";
    let (_e, url, _dir) =
        start_server_with("stale", ha_tx, ClientBinaries::with("aarch64-linux", bin)).await;

    let mut c = connect_as(&url, Some("aarch64-linux"), Some("deadbeef")).await;
    match next_msg(&mut c).await {
        Message::Text(t) => match serde_json::from_str(&t).unwrap() {
            ServerMessage::Update { path, sha256 } => {
                assert_eq!(path, "/client/aarch64-linux");
                assert_eq!(sha256, sha256_hex(bin));
            }
            other => panic!("expected update, got {other:?}"),
        },
        other => panic!("expected update, got {other:?}"),
    }
    // the client is still served normally
    assert!(next_frame(&mut c).await.0 < 15);
}

#[tokio::test]
async fn up_to_date_unknown_and_old_clients_get_no_update() {
    let (ha_tx, _ha_rx) = mpsc::unbounded_channel();
    let bin = b"new client";
    let (_e, url, _dir) =
        start_server_with("fresh", ha_tx, ClientBinaries::with("aarch64-linux", bin)).await;

    let same = sha256_hex(bin);
    for (target, sha) in [
        (Some("aarch64-linux"), Some(same.as_str())), // matching hash
        (Some("x86_64-linux"), Some("deadbeef")),     // no binary for that target
        (Some("aarch64-macos"), Some("deadbeef")),
        (Some("aarch64-linux"), None), // hash unknown
        (None, None),                  // old client
    ] {
        let mut c = connect_as(&url, target, sha).await;
        // first thing after Welcome is an image frame, never an Update
        next_frame(&mut c).await;
    }
}

// ---- mock Home Assistant -------------------------------------------------

struct MockHa {
    url: String,
    /// push a raw event payload (the `event` field) to the subscriber
    push: UnboundedSender<Value>,
    /// call_service messages received
    calls: UnboundedReceiver<Value>,
}

async fn mock_ha(initial: Vec<Value>) -> MockHa {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (push, mut push_rx) = mpsc::unbounded_channel::<Value>();
    let (calls_tx, calls) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        // serve connections one after another (the client may reconnect)
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(Message::Text(
                json!({"type": "auth_required", "ha_version": "mock"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
            let auth: Value = match ws.next().await {
                Some(Ok(Message::Text(t))) => serde_json::from_str(&t).unwrap(),
                _ => continue,
            };
            if auth["access_token"] != "secret" {
                ws.send(Message::Text(
                    json!({"type": "auth_invalid"}).to_string().into(),
                ))
                .await
                .unwrap();
                continue;
            }
            ws.send(Message::Text(json!({"type": "auth_ok"}).to_string().into()))
                .await
                .unwrap();
            let mut sub_id = 0;
            loop {
                tokio::select! {
                    msg = ws.next() => {
                        let t = match msg {
                            Some(Ok(Message::Text(t))) => t,
                            Some(Ok(_)) => continue,
                            _ => break,
                        };
                        let m: Value = serde_json::from_str(&t).unwrap();
                        let id = m["id"].clone();
                        match m["type"].as_str().unwrap() {
                            "subscribe_events" => {
                                sub_id = id.as_u64().unwrap();
                                ws.send(Message::Text(json!({"id": id, "type": "result", "success": true, "result": null}).to_string().into())).await.unwrap();
                            }
                            "get_states" => {
                                ws.send(Message::Text(json!({"id": id, "type": "result", "success": true, "result": initial}).to_string().into())).await.unwrap();
                            }
                            "call_service" => {
                                let _ = calls_tx.send(m);
                                ws.send(Message::Text(json!({"id": id, "type": "result", "success": true, "result": {}}).to_string().into())).await.unwrap();
                            }
                            _ => {}
                        }
                    }
                    Some(ev) = push_rx.recv() => {
                        ws.send(Message::Text(json!({"id": sub_id, "type": "event", "event": ev}).to_string().into())).await.unwrap();
                    }
                }
            }
        }
    });
    MockHa { url, push, calls }
}

fn ha_state(id: &str, s: &str) -> Value {
    json!({"entity_id": id, "state": s, "attributes": {"brightness": 200}, "last_changed": "x"})
}

#[tokio::test]
async fn real_ha_client_against_mock_ha() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("debug")
        .with_test_writer()
        .try_init();
    let mut mock = mock_ha(vec![ha_state("light.office", "off")]).await;
    let (ha_tx, ha_rx) = mpsc::unbounded_channel();
    let (engine_tx, url, _dir) = start_server("mock", ha_tx).await;
    tokio::spawn(ha::run(
        HaConfig::new(&mock.url, "secret"),
        ha_rx,
        engine_tx.clone(),
    ));

    let mut c = connect(&url).await;
    // initial 15 frames; the HA seed may or may not re-send some depending on timing
    let mut initial = std::collections::HashMap::new();
    for _ in 0..15 {
        let (k, j) = next_frame(&mut c).await;
        initial.insert(k, j);
    }
    for (k, j) in drain_frames(&mut c, Duration::from_millis(400)).await {
        initial.insert(k, j);
    }

    // press -> call_service reaches HA with the right payload
    keydown(&mut c, 0).await;
    let call = tokio::time::timeout(Duration::from_secs(5), mock.calls.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(call["domain"], "light");
    assert_eq!(call["service"], "toggle");
    assert_eq!(call["service_data"]["entity_id"], "light.office");
    drain_frames(&mut c, Duration::from_millis(700)).await; // press flash

    // HA pushes state_changed -> key 0 changes, others don't
    mock.push
        .send(json!({"event_type": "state_changed", "data": {"entity_id": "light.office", "new_state": ha_state("light.office", "on")}}))
        .unwrap();
    let (k, jpeg) = next_frame(&mut c).await;
    assert_eq!(k, 0);
    assert_ne!(&jpeg, initial.get(&0).unwrap());
    assert!(drain_frames(&mut c, Duration::from_millis(300))
        .await
        .is_empty());
}

// ---- browser deck (/deck) ----

const DECK_CONFIG: &str = r##"
sdc.page("home", {
  sdc.button{ render = { text = "Next", bg = "#cc0000" }, press = function(ctx) ctx.goto_page("other") end },
  sdc.button{ render = { text = "Home", bg = "#cc0000" } },
})
sdc.page("other", {
  sdc.button{ render = { text = "Back", bg = "#0000cc" }, press = function(ctx) ctx.goto_page("home") end },
  sdc.button{ render = { text = "Other", bg = "#0000cc" } },
})
sdc.device("main", { pages = { "home", "other" } })
"##;

const EMAIL: &str = "X-Auth-Request-Email: me@example.com\r\n";

/// Server with the two-page deck config; returns the `ws://.../ws` url (host:port source).
async fn start_deck_server(
    tag: &str,
    token: Option<&str>,
    deck_auth: http::DeckAuth,
) -> (String, TempDir) {
    start_deck_server_with(tag, DECK_CONFIG, token, deck_auth).await
}

async fn start_deck_server_with(
    tag: &str,
    config: &str,
    token: Option<&str>,
    deck_auth: http::DeckAuth,
) -> (String, TempDir) {
    let dir = TempDir::new(tag);
    let cfg = dir.0.join("config.lua");
    std::fs::write(&cfg, config).unwrap();
    let (ha_tx, _ha_rx) = mpsc::unbounded_channel();
    let engine_tx = engine::spawn(cfg, Some(ha_tx));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws", listener.local_addr().unwrap());
    tokio::spawn(http::serve(
        listener,
        engine_tx,
        Arc::new(ClientBinaries::default()),
        token.map(Into::into),
        deck_auth,
    ));
    (url, dir)
}

fn deck_url(ws_url: &str) -> String {
    ws_url.replace("/ws", "/deck/ws")
}

async fn http_get_raw(ws_url: &str, path: &str, headers: &str) -> (u16, Vec<u8>) {
    let addr = ws_url
        .strip_prefix("ws://")
        .unwrap()
        .strip_suffix("/ws")
        .unwrap();
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\n{headers}\r\n").as_bytes())
        .await
        .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    (
        head.split(' ').nth(1).unwrap().parse().unwrap(),
        raw[split..].to_vec(),
    )
}

/// A browser (or, without `email`, an unauthenticated) websocket on `/deck/ws`.
async fn deck_handshake(
    ws_url: &str,
    headers: &[(&str, &str)],
) -> Result<Client, tokio_tungstenite::tungstenite::Error> {
    let mut req = deck_url(ws_url).into_client_request().unwrap();
    for (k, v) in headers {
        req.headers_mut().insert(
            tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.parse().unwrap(),
        );
    }
    tokio_tungstenite::connect_async(req).await.map(|(c, _)| c)
}

/// Connects a browser deck and sends Hello; returns the socket (Welcome not yet read).
async fn deck_connect(ws_url: &str, client_id: &str, mirror: Option<&str>, px: u32) -> Client {
    deck_connect_as(ws_url, client_id, mirror, px, None).await
}

async fn deck_connect_as(
    ws_url: &str,
    client_id: &str,
    mirror: Option<&str>,
    px: u32,
    device: Option<&str>,
) -> Client {
    let mut c = deck_handshake(ws_url, &[("x-auth-request-email", "me@example.com")])
        .await
        .unwrap();
    let hello = ClientMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        client_id: client_id.into(),
        device: DeviceInfo {
            model: "Web Deck".into(),
            serial: None,
            rows: 3,
            cols: 5,
            key_size_px: px,
            native: None,
        },
        client_target: None,
        client_sha256: None,
        mirror: mirror.map(Into::into),
        device_name: device.map(Into::into),
    };
    c.send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
        .await
        .unwrap();
    c
}

async fn expect_welcome(c: &mut Client) {
    match next_msg(c).await {
        Message::Text(t) => assert!(matches!(
            serde_json::from_str(&t).unwrap(),
            ServerMessage::Welcome { .. }
        )),
        other => panic!("expected welcome, got {other:?}"),
    }
}

/// The 15 initial frames of a deck, by key.
async fn initial_frames(c: &mut Client) -> Vec<Vec<u8>> {
    let mut frames = vec![Vec::new(); 15];
    for _ in 0..15 {
        let (k, jpeg) = next_frame(c).await;
        frames[k as usize] = jpeg;
    }
    frames
}

async fn deck_clients(ws_url: &str) -> Vec<Value> {
    let (status, body) = http_get_raw(ws_url, "/deck/api/clients", EMAIL).await;
    assert_eq!(status, 200);
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn deck_routes_require_sso_header() {
    let (url, _dir) = start_deck_server("deck-auth", Some("s3cret"), http::DeckAuth::Sso).await;
    for path in [
        "/deck",
        "/deck/",
        "/deck/manifest.webmanifest",
        "/deck/icon.svg",
        "/deck/api/clients",
    ] {
        assert_eq!(http_get_raw(&url, path, "").await.0, 401, "{path}");
        // the client bearer token is not an SSO login
        let bearer = "Authorization: Bearer s3cret\r\n";
        assert_eq!(http_get_raw(&url, path, bearer).await.0, 401, "{path}");
        assert_eq!(http_get_raw(&url, path, EMAIL).await.0, 200, "{path}");
    }
    let (_, page) = http_get_raw(&url, "/deck", EMAIL).await;
    assert!(String::from_utf8_lossy(&page).contains("<title>Stream Deck</title>"));
    let (_, manifest) = http_get_raw(&url, "/deck/manifest.webmanifest", EMAIL).await;
    let manifest: Value = serde_json::from_slice(&manifest).unwrap();
    assert_eq!(manifest["orientation"], "landscape");
    assert_eq!(manifest["display"], "fullscreen");

    // websocket: SSO header needed, the bearer token is no substitute
    let denied = |r: Result<Client, _>| match r {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => resp.status().as_u16(),
        other => panic!("expected http error, got {:?}", other.map(|_| ())),
    };
    assert_eq!(denied(deck_handshake(&url, &[]).await), 401);
    assert_eq!(
        denied(deck_handshake(&url, &[("authorization", "Bearer s3cret")]).await),
        401
    );
    assert!(
        deck_handshake(&url, &[("x-auth-request-email", "me@example.com")])
            .await
            .is_ok()
    );
    // ... and the SSO header does not open /ws
    assert_eq!(ws_handshake_status(&url, None).await, 401);
    // cross-site pages cannot drive the deck with the user's cookies
    let foreign = deck_handshake(
        &url,
        &[
            ("x-auth-request-email", "me@example.com"),
            ("origin", "https://evil.example"),
        ],
    )
    .await;
    assert_eq!(denied(foreign), 403);
    // health stays open
    assert_eq!(http_get_raw(&url, "/healthz", "").await.0, 200);
}

#[tokio::test]
async fn deck_devices_api_lists_configured_devices_behind_sso() {
    let config = format!(
        r##"{DECK_CONFIG}
        sdc.device("one", {{ pages = {{ "home" }} }})
        sdc.device("both", {{ pages = {{ "other", "home" }}, home = "other" }})
        "##
    );
    let (url, _dir) =
        start_deck_server_with("deck-devices", &config, None, http::DeckAuth::Sso).await;
    assert_eq!(http_get_raw(&url, "/deck/api/devices", "").await.0, 401);
    let (status, body) = http_get_raw(&url, "/deck/api/devices", EMAIL).await;
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!([
            {"name": "main", "pages": ["home", "other"], "home": "home"},
            {"name": "one", "pages": ["home"], "home": "home"},
            {"name": "both", "pages": ["other", "home"], "home": "other"},
        ])
    );
    // a browser deck asking for a layout starts on that device's home page
    let mut web = deck_connect_as(&url, "web-dev", None, 144, Some("both")).await;
    expect_welcome(&mut web).await;
    initial_frames(&mut web).await;
    let list = deck_clients(&url).await;
    assert_eq!(list[0]["page"], "other");
}

#[tokio::test]
async fn deck_auth_none_bypasses_sso_check() {
    let (url, _dir) = start_deck_server("deck-open", None, http::DeckAuth::None).await;
    assert_eq!(http_get_raw(&url, "/deck", "").await.0, 200);
    assert_eq!(http_get_raw(&url, "/deck/api/clients", "").await.0, 200);
    assert!(deck_handshake(&url, &[]).await.is_ok());
}

#[tokio::test]
async fn api_lists_physical_and_browser_decks_but_not_viewers() {
    let (url, _dir) = start_deck_server("deck-list", None, http::DeckAuth::Sso).await;
    assert!(deck_clients(&url).await.is_empty());

    let mut pi = connect_as(&url, None, None).await; // client_id "test", 72 px
    initial_frames(&mut pi).await;
    let mut own = deck_connect(&url, "web-own", None, 144).await;
    expect_welcome(&mut own).await;
    initial_frames(&mut own).await;
    let mut viewer = deck_connect(&url, "web-view", Some("test"), 144).await;
    expect_welcome(&mut viewer).await;
    initial_frames(&mut viewer).await;

    let list = deck_clients(&url).await;
    assert_eq!(list.len(), 2, "{list:?}");
    assert_eq!(
        list[0],
        json!({"client_id": "test", "model": "Test Deck", "rows": 3, "cols": 5, "page": "home", "web": false})
    );
    assert_eq!(list[1]["client_id"], "web-own");
    assert_eq!(list[1]["web"], true);
}

#[tokio::test]
async fn mirror_viewer_follows_and_drives_the_mirrored_deck() {
    let (url, _dir) = start_deck_server("deck-mirror", None, http::DeckAuth::Sso).await;
    let mut pi = connect_as(&url, None, None).await; // client_id "test", 72 px
    let pi_home = initial_frames(&mut pi).await;
    let mut own = deck_connect(&url, "web-own", None, 144).await;
    expect_welcome(&mut own).await;
    initial_frames(&mut own).await;

    let mut viewer = deck_connect(&url, "web-view", Some("test"), 144).await;
    expect_welcome(&mut viewer).await;
    // frames of the mirrored deck's page, rendered at the viewer's own key size
    let (k, jpeg) = {
        let (k, j) = next_frame(&mut viewer).await;
        (k, j)
    };
    assert_eq!(image::load_from_memory(&jpeg).unwrap().width(), 144);
    let mut view_home = vec![Vec::new(); 15];
    view_home[k as usize] = jpeg;
    for _ in 1..15 {
        let (k, j) = next_frame(&mut viewer).await;
        view_home[k as usize] = j;
    }
    let red = image::load_from_memory(&view_home[1]).unwrap().to_rgb8();
    assert!(red.get_pixel(5, 5)[0] > 150 && red.get_pixel(5, 5)[2] < 60);

    // pressing on the viewer switches the real deck (and the viewer) to the other page
    keydown(&mut viewer, 0).await;
    let pi_other = drain_frames(&mut pi, Duration::from_millis(500)).await;
    assert!(!pi_other.is_empty(), "mirrored deck did not change page");
    assert_ne!(pi_other[0].1, pi_home[pi_other[0].0 as usize]);
    let view_other = drain_frames(&mut viewer, Duration::from_millis(500)).await;
    assert!(!view_other.is_empty(), "viewer did not change page");
    let blue = image::load_from_memory(&view_other.iter().find(|f| f.0 == 1).unwrap().1)
        .unwrap()
        .to_rgb8();
    assert!(blue.get_pixel(5, 5)[2] > 150);

    // ... and a press on the real deck is visible on the viewer
    keydown(&mut pi, 0).await;
    let back = drain_frames(&mut viewer, Duration::from_millis(500)).await;
    assert!(!back.is_empty(), "viewer did not follow the real deck");

    // an own-deck browser is independent of all of it
    drain_frames(&mut pi, Duration::from_millis(300)).await;
    assert!(drain_frames(&mut own, Duration::from_millis(300))
        .await
        .is_empty());
    keydown(&mut own, 0).await;
    assert!(!drain_frames(&mut own, Duration::from_millis(500))
        .await
        .is_empty());
    assert!(drain_frames(&mut pi, Duration::from_millis(300))
        .await
        .is_empty());
    assert!(drain_frames(&mut viewer, Duration::from_millis(300))
        .await
        .is_empty());
}

#[tokio::test]
async fn mirroring_an_unknown_deck_is_an_error() {
    let (url, _dir) = start_deck_server("deck-nomirror", None, http::DeckAuth::Sso).await;
    let mut viewer = deck_connect(&url, "web-view", Some("ghost"), 144).await;
    expect_welcome(&mut viewer).await;
    match next_msg(&mut viewer).await {
        Message::Text(t) => assert!(matches!(
            serde_json::from_str(&t).unwrap(),
            ServerMessage::Error { .. }
        )),
        other => panic!("expected error, got {other:?}"),
    }
}

#[tokio::test]
async fn browser_cannot_impersonate_a_physical_deck() {
    let (url, _dir) = start_deck_server("deck-impersonate", None, http::DeckAuth::Sso).await;
    let mut pi = connect_as(&url, None, None).await; // "test"
    initial_frames(&mut pi).await;
    let mut b = deck_connect(&url, "test", None, 144).await;
    expect_welcome(&mut b).await;
    initial_frames(&mut b).await;
    let ids: Vec<_> = deck_clients(&url)
        .await
        .iter()
        .map(|d| d["client_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, ["test", "web-test"]);
}

// ---- Elgato plugin (/plugin/ws) ---------------------------------------------

fn plugin_url(ws_url: &str) -> String {
    ws_url.replace("/ws", "/plugin/ws")
}

async fn plugin_send(c: &mut Client, m: PluginClientMessage) {
    c.send(Message::Text(serde_json::to_string(&m).unwrap().into()))
        .await
        .unwrap();
}

async fn plugin_next(c: &mut Client) -> PluginServerMessage {
    match next_msg(c).await {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("expected text, got {other:?}"),
    }
}

/// Messages that arrive within `quiet` of each other.
async fn plugin_drain(c: &mut Client, quiet: Duration) -> Vec<PluginServerMessage> {
    let mut out = Vec::new();
    while let Ok(Some(Ok(Message::Text(t)))) = tokio::time::timeout(quiet, c.next()).await {
        out.push(serde_json::from_str(&t).unwrap());
    }
    out
}

/// Connects, says hello, returns after the welcome and the export list.
async fn plugin_connect(url: &str) -> (Client, Vec<sdc_protocol::ExportItem>) {
    let (mut c, _) = tokio_tungstenite::connect_async(plugin_url(url))
        .await
        .unwrap();
    plugin_send(
        &mut c,
        PluginClientMessage::PluginHello {
            protocol_version: PROTOCOL_VERSION,
            client_id: "test-plugin".into(),
        },
    )
    .await;
    assert!(matches!(
        plugin_next(&mut c).await,
        PluginServerMessage::PluginWelcome { .. }
    ));
    let PluginServerMessage::Exports { items } = plugin_next(&mut c).await else {
        panic!("expected exports")
    };
    (c, items)
}

fn subscribe(slot: &str, export: &str, kind: SlotKind) -> PluginClientMessage {
    PluginClientMessage::Subscribe {
        slot: slot.into(),
        export: export.into(),
        kind,
        size_px: 144,
    }
}

fn png_size(b64: &str) -> (u32, u32) {
    use base64::Engine as _;
    let png = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .unwrap();
    let img = image::load_from_memory_with_format(&png, image::ImageFormat::Png).unwrap();
    (img.width(), img.height())
}

#[tokio::test]
async fn plugin_ws_uses_the_client_token() {
    let (ha_tx, _ha_rx) = mpsc::unbounded_channel();
    let (_e, url, _dir) = start_server_full(
        "plugin-token",
        ha_tx,
        ClientBinaries::default(),
        Some("s3cret"),
    )
    .await;
    let url = plugin_url(&url);
    assert_eq!(ws_handshake_status(&url, None).await, 401);
    assert_eq!(ws_handshake_status(&url, Some("Bearer nope")).await, 401);
    assert_eq!(ws_handshake_status(&url, Some("Bearer s3cret")).await, 101);
    // the SSO header does not open it
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("X-Auth-Request-Email", "me@example.com".parse().unwrap());
    assert!(tokio_tungstenite::connect_async(req).await.is_err());
}

#[tokio::test]
async fn plugin_subscribe_render_dependencies_and_input() {
    let (ha_tx, mut ha_rx) = mpsc::unbounded_channel();
    let (engine_tx, url, dir) = start_server("plugin-flow", ha_tx).await;
    let (mut c, items) = plugin_connect(&url).await;
    assert_eq!(
        items
            .iter()
            .map(|i| (i.id.as_str(), i.kind, i.label.as_deref()))
            .collect::<Vec<_>>(),
        [
            ("lamp", SlotKind::Key, Some("Lamp")),
            ("static", SlotKind::Key, None),
            ("dimmer", SlotKind::Dial, None),
        ]
    );

    plugin_send(&mut c, subscribe("k1", "lamp", SlotKind::Key)).await;
    plugin_send(&mut c, subscribe("k2", "static", SlotKind::Key)).await;
    plugin_send(&mut c, subscribe("d1", "dimmer", SlotKind::Dial)).await;
    plugin_send(&mut c, subscribe("bad", "nope", SlotKind::Key)).await;
    let mut got = Vec::new();
    for _ in 0..4 {
        got.push(plugin_next(&mut c).await);
    }
    for m in &got {
        match m {
            PluginServerMessage::KeyImage { png_base64, .. } => {
                assert_eq!(png_size(png_base64), (144, 144))
            }
            PluginServerMessage::DialFeedback {
                slot,
                title,
                value,
                indicator,
                icon_png_base64,
            } => {
                assert_eq!(
                    (slot.as_str(), title.as_str(), value.as_str(), *indicator),
                    ("d1", "Dimmer", "?", 40)
                );
                assert!(icon_png_base64.is_none());
            }
            PluginServerMessage::SlotError { slot, message } => {
                assert_eq!(slot, "bad");
                assert!(message.contains("no export"), "{message}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(
        got.iter()
            .filter(|m| matches!(m, PluginServerMessage::KeyImage { .. }))
            .count(),
        2
    );

    // HA change: only the dependent key slot gets a new image
    engine_tx
        .send(EngineEvent::Ha(HaEvent::Changed(on("light.office"))))
        .unwrap();
    let msgs = plugin_drain(&mut c, Duration::from_millis(400)).await;
    assert!(
        matches!(msgs.as_slice(), [PluginServerMessage::KeyImage { slot, .. }] if slot == "k1"),
        "{msgs:?}"
    );
    // an unrelated change and an identical one: nothing
    engine_tx
        .send(EngineEvent::Ha(HaEvent::Changed(on("light.office"))))
        .unwrap();
    engine_tx
        .send(EngineEvent::Ha(HaEvent::Changed(on("light.other"))))
        .unwrap();
    assert!(plugin_drain(&mut c, Duration::from_millis(300))
        .await
        .is_empty());
    // the dial's dependency: new feedback for the dial only
    engine_tx
        .send(EngineEvent::Ha(HaEvent::Changed(on("light.dial"))))
        .unwrap();
    let msgs = plugin_drain(&mut c, Duration::from_millis(400)).await;
    assert!(
        matches!(msgs.as_slice(), [PluginServerMessage::DialFeedback { slot, value, .. }] if slot == "d1" && value == "on"),
        "{msgs:?}"
    );

    // input
    let expect_call = |data: Value, service: &'static str| HaCommand::CallService {
        domain: "light".into(),
        service: service.into(),
        data,
    };
    plugin_send(&mut c, PluginClientMessage::SlotDown { slot: "k1".into() }).await;
    let cmd = tokio::time::timeout(Duration::from_secs(5), ha_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        cmd,
        expect_call(json!({"entity_id": "light.office"}), "toggle")
    );
    // the press flashes the slot, then restores it
    let msgs = plugin_drain(&mut c, Duration::from_millis(700)).await;
    assert!(
        matches!(msgs.as_slice(), [PluginServerMessage::KeyImage { slot: a, .. }, PluginServerMessage::KeyImage { slot: b, .. }] if a == "k1" && b == "k1"),
        "{msgs:?}"
    );
    plugin_send(
        &mut c,
        PluginClientMessage::DialRotate {
            slot: "d1".into(),
            ticks: -4,
        },
    )
    .await;
    let cmd = tokio::time::timeout(Duration::from_secs(5), ha_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        cmd,
        expect_call(json!({"entity_id": "light.dial", "step": -4}), "turn_on")
    );

    // unsubscribed slots stop updating
    plugin_send(
        &mut c,
        PluginClientMessage::Unsubscribe { slot: "k1".into() },
    )
    .await;
    // the websocket and the HA event reach the engine on different paths
    tokio::time::sleep(Duration::from_millis(200)).await;
    engine_tx
        .send(EngineEvent::Ha(HaEvent::Changed(EntityState {
            entity_id: "light.office".into(),
            state: "off".into(),
            attributes: json!({}),
        })))
        .unwrap();
    assert!(plugin_drain(&mut c, Duration::from_millis(300))
        .await
        .is_empty());

    // reload without the export: its slot gets a SlotError, the list is pushed again
    plugin_send(&mut c, subscribe("k1", "lamp", SlotKind::Key)).await;
    assert!(matches!(
        plugin_next(&mut c).await,
        PluginServerMessage::KeyImage { .. }
    ));
    std::fs::write(
        dir.0.join("config.lua"),
        r#"sdc.page("home", {})
           sdc.device("deck", { pages = { "home" } })
           sdc.export("static", sdc.button{ render = { text = "Static" } })"#,
    )
    .unwrap();
    engine_tx.send(EngineEvent::Reload).unwrap();
    let msgs = plugin_drain(&mut c, Duration::from_millis(400)).await;
    assert!(
        msgs.iter()
            .any(|m| matches!(m, PluginServerMessage::Exports { items } if items.len() == 1)),
        "{msgs:?}"
    );
    assert!(
        msgs.iter()
            .any(|m| matches!(m, PluginServerMessage::SlotError { slot, .. } if slot == "k1")),
        "{msgs:?}"
    );
    assert!(
        msgs.iter()
            .any(|m| matches!(m, PluginServerMessage::SlotError { slot, .. } if slot == "d1")),
        "{msgs:?}"
    );
    // k2 still exists and is unchanged: no image resent
    assert!(!msgs
        .iter()
        .any(|m| matches!(m, PluginServerMessage::KeyImage { .. })));
}

#[tokio::test]
async fn native_frames_are_rotated_once_and_confirmed() {
    use sdc_protocol::{NativeImage, NativeMirror, NativeMode};
    let (ha_tx, _ha_rx) = mpsc::unbounded_channel();
    let (_engine, url, _dir) = start_server("native", ha_tx).await;
    let fmt = |size, mode| NativeImage {
        mode,
        size: (size, size),
        rotation: 0,
        mirror: NativeMirror::Both, // MK.2
    };

    // plain client: upright frames, no confirmation
    let (mut plain, native_frames) = connect_with(&url, None, None, device()).await;
    assert!(!native_frames);
    let upright = initial_frames(&mut plain).await;

    // MK.2-style client: confirmed, and every frame is the upright one rotated 180 degrees
    let mut dev = device();
    dev.native = Some(fmt(72, NativeMode::Jpeg));
    let (mut mk2, native_frames) = connect_with(&url, None, None, dev).await;
    assert!(native_frames);
    let native = initial_frames(&mut mk2).await;
    let mut differing = 0;
    for (u, n) in upright.iter().zip(&native) {
        let u = image::load_from_memory(u).unwrap().to_rgb8();
        let n = image::load_from_memory(n).unwrap().to_rgb8();
        assert_eq!(n.dimensions(), (72, 72));
        let want = image::imageops::rotate180(&u);
        let err: u64 = want
            .pixels()
            .zip(n.pixels())
            .map(|(a, b)| {
                a.0.iter()
                    .zip(b.0)
                    .map(|(x, y)| x.abs_diff(y) as u64)
                    .sum::<u64>()
            })
            .sum();
        assert!(
            err / (72 * 72 * 3) < 6,
            "native frame is the rotated render"
        );
        differing += (u != n) as usize;
    }
    assert!(differing > 0);

    // a format the server cannot produce (BMP, or a size that is not the key size) is declined
    for bad in [fmt(72, NativeMode::Bmp), fmt(80, NativeMode::Jpeg)] {
        let mut dev = device();
        dev.native = Some(bad);
        let (mut c, native_frames) = connect_with(&url, None, None, dev).await;
        assert!(!native_frames);
        initial_frames(&mut c).await;
    }
}
