//! End-to-end tests: a real WebSocket client talks to the real server stack, with
//! Home Assistant replaced either by channels (fake) or by a mock HA WebSocket server.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use sdc_protocol::{
    sha256_hex, ClientMessage, DeviceInfo, ImageFrame, ServerMessage, PROTOCOL_VERSION,
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
    ));
    (engine_tx, url, dir)
}

async fn connect(url: &str) -> Client {
    connect_as(url, None, None).await
}

async fn connect_as(url: &str, target: Option<&str>, sha256: Option<&str>) -> Client {
    let (mut c, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let hello = ClientMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        client_id: "test".into(),
        device: device(),
        client_target: target.map(Into::into),
        client_sha256: sha256.map(Into::into),
    };
    c.send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
        .await
        .unwrap();
    match next_msg(&mut c).await {
        Message::Text(t) => assert!(matches!(
            serde_json::from_str(&t).unwrap(),
            ServerMessage::Welcome { .. }
        )),
        other => panic!("expected welcome, got {other:?}"),
    }
    c
}

async fn next_msg(c: &mut Client) -> Message {
    tokio::time::timeout(Duration::from_secs(5), c.next())
        .await
        .expect("timeout")
        .unwrap()
        .unwrap()
}

async fn next_frame(c: &mut Client) -> (u8, Vec<u8>) {
    match next_msg(c).await {
        Message::Binary(b) => {
            let f = ImageFrame::decode(&b).unwrap();
            (f.key, f.image.to_vec())
        }
        other => panic!("expected binary frame, got {other:?}"),
    }
}

/// Frames that arrive within `quiet` of each other.
async fn drain_frames(c: &mut Client, quiet: Duration) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    while let Ok(Some(Ok(Message::Binary(b)))) = tokio::time::timeout(quiet, c.next()).await {
        let f = ImageFrame::decode(&b).unwrap();
        out.push((f.key, f.image.to_vec()));
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
