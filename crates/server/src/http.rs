//! HTTP side of the server: the `/ws` upgrade plus client distribution and health.
//!
//! - `GET /client/<target>`         client binary (404 if the server has none for the target)
//! - `GET /client/<target>.sha256`  its hex sha256
//! - `GET /install.sh`              Raspberry Pi installer (needs only the server URL)
//! - `GET /healthz`                 readiness probe

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedSender;

use crate::clients::ClientBinaries;
use crate::engine::EngineEvent;
use crate::ws::{self, WsState};

const INSTALL_SH: &str = include_str!("../../../deploy/pi/install.sh");

pub fn router(engine: UnboundedSender<EngineEvent>, binaries: Arc<ClientBinaries>) -> Router {
    Router::new()
        .route("/ws", get(ws::upgrade))
        .route("/client/:file", get(client_file))
        .route("/install.sh", get(install_sh))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(WsState { engine, binaries })
}

pub async fn serve(
    listener: TcpListener,
    engine: UnboundedSender<EngineEvent>,
    binaries: Arc<ClientBinaries>,
) {
    if let Err(e) = axum::serve(listener, router(engine, binaries)).await {
        tracing::error!("http server failed: {e}");
    }
}

async fn client_file(State(state): State<WsState>, Path(file): Path<String>) -> Response {
    let (target, want_hash) = match file.strip_suffix(".sha256") {
        Some(t) => (t, true),
        None => (file.as_str(), false),
    };
    match state.binaries.get(target) {
        Some(b) if want_hash => format!("{}\n", b.sha256).into_response(),
        Some(b) => (
            [(header::CONTENT_TYPE, "application/octet-stream")],
            b.bytes.clone(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "no client binary for that target\n").into_response(),
    }
}

async fn install_sh() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/x-shellscript")], INSTALL_SH)
}
