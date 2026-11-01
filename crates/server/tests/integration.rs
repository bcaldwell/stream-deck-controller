//! End-to-end tests: a real WebSocket client talks to the real server stack, with
//! Home Assistant replaced either by channels (fake) or by a mock HA WebSocket server.

use std::path::PathBuf;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use sdc_protocol::{ClientMessage, DeviceInfo, ImageFrame, ServerMessage, PROTOCOL_VERSION};
use sdc_server::engine::{self, EngineEvent};
use sdc_server::ha::{self, EntityState, HaCommand, HaConfig, HaEvent};
use sdc_server::ws;
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
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
    let dir = TempDir::new(tag);
    let cfg = dir.0.join("config.lua");
    std::fs::write(&cfg, CONFIG).unwrap();
    let engine_tx = engine::spawn(cfg, Some(ha_tx));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws", listener.local_addr().unwrap());
    tokio::spawn(ws::serve(listener, engine_tx.clone()));
    (engine_tx, url, dir)
}

async fn connect(url: &str) -> Client {
    let (mut c, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let hello = ClientMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        client_id: "test".into(),
        device: device(),
    };
    c.send(Message::Text(serde_json::to_string(&hello).unwrap()))
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
        serde_json::to_string(&ClientMessage::KeyDown { key }).unwrap(),
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
    };
    c.send(Message::Text(serde_json::to_string(&hello).unwrap()))
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
                json!({"type": "auth_required", "ha_version": "mock"}).to_string(),
            ))
            .await
            .unwrap();
            let auth: Value = match ws.next().await {
                Some(Ok(Message::Text(t))) => serde_json::from_str(&t).unwrap(),
                _ => continue,
            };
            if auth["access_token"] != "secret" {
                ws.send(Message::Text(json!({"type": "auth_invalid"}).to_string()))
                    .await
                    .unwrap();
                continue;
            }
            ws.send(Message::Text(json!({"type": "auth_ok"}).to_string()))
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
                                ws.send(Message::Text(json!({"id": id, "type": "result", "success": true, "result": null}).to_string())).await.unwrap();
                            }
                            "get_states" => {
                                ws.send(Message::Text(json!({"id": id, "type": "result", "success": true, "result": initial}).to_string())).await.unwrap();
                            }
                            "call_service" => {
                                let _ = calls_tx.send(m);
                                ws.send(Message::Text(json!({"id": id, "type": "result", "success": true, "result": {}}).to_string())).await.unwrap();
                            }
                            _ => {}
                        }
                    }
                    Some(ev) = push_rx.recv() => {
                        ws.send(Message::Text(json!({"id": sub_id, "type": "event", "event": ev}).to_string())).await.unwrap();
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
