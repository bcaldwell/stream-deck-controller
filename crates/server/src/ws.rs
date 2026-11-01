//! WebSocket server for clients (`/ws`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use futures_util::{SinkExt, StreamExt};
use sdc_protocol::{ClientMessage, ImageFrame, ServerMessage, PROTOCOL_VERSION};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::Message;

use crate::engine::{EngineEvent, Frame};

static NEXT_CLIENT: AtomicU64 = AtomicU64::new(1);
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn serve(listener: TcpListener, engine: UnboundedSender<EngineEvent>) {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let engine = engine.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, engine).await {
                        tracing::warn!("connection from {addr} ended: {e:#}");
                    }
                });
            }
            Err(e) => {
                tracing::error!("accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

async fn handle_conn(stream: TcpStream, engine: UnboundedSender<EngineEvent>) -> Result<()> {
    let check_path = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
        if req.uri().path() == "/ws" {
            Ok(resp)
        } else {
            let mut r = ErrorResponse::new(Some("not found".to_string()));
            *r.status_mut() = StatusCode::NOT_FOUND;
            Err(r)
        }
    };
    let ws = tokio_tungstenite::accept_hdr_async(stream, check_path).await?;
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
        sink.send(Message::Text(serde_json::to_string(&msg)?))
            .await?;
        let _ = sink.close().await;
        bail!("client {client_id} protocol version {protocol_version} unsupported");
    }
    if device.rows == 0 || device.cols == 0 || device.key_size_px == 0 || device.key_size_px > 1024
    {
        let msg = ServerMessage::Error {
            message: "invalid device info".into(),
        };
        sink.send(Message::Text(serde_json::to_string(&msg)?))
            .await?;
        bail!("client {client_id} sent invalid device info");
    }
    let key_count = device.key_count();

    sink.send(Message::Text(serde_json::to_string(
        &ServerMessage::Welcome {
            protocol_version: PROTOCOL_VERSION,
        },
    )?))
    .await?;

    let id = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = mpsc::unbounded_channel::<Frame>();
    let _ = engine.send(EngineEvent::ClientConnected {
        id,
        client_id,
        device,
        tx,
    });

    let result: Result<()> = async {
        loop {
            tokio::select! {
                frame = rx.recv() => match frame {
                    Some(f) => sink.send(Message::Binary(ImageFrame::encode(f.key, &f.jpeg))).await?,
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
