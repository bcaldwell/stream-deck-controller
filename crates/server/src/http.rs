//! HTTP side of the server: the `/ws` upgrade plus client distribution and health.
//!
//! - `GET /client/<target>`         client binary (404 if the server has none for the target)
//! - `GET /client/<target>.sha256`  its hex sha256
//! - `GET /healthz`                 readiness probe
//!
//! With a client token configured, `/ws` and `/client/*` need `Authorization: Bearer <token>`.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedSender;

use crate::clients::ClientBinaries;
use crate::engine::EngineEvent;
use crate::ws::{self, WsState};

/// `token`: the shared client token (`None` allows everyone).
pub fn router(
    engine: UnboundedSender<EngineEvent>,
    binaries: Arc<ClientBinaries>,
    token: Option<String>,
) -> Router {
    Router::new()
        .route("/ws", get(ws::upgrade))
        .route("/client/:file", get(client_file))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(WsState {
            engine,
            binaries,
            token: token.map(Into::into),
        })
}

pub async fn serve(
    listener: TcpListener,
    engine: UnboundedSender<EngineEvent>,
    binaries: Arc<ClientBinaries>,
    token: Option<String>,
) {
    if let Err(e) = axum::serve(listener, router(engine, binaries, token)).await {
        tracing::error!("http server failed: {e}");
    }
}

/// `Ok` if no token is required or `headers` carry `Authorization: Bearer <token>`;
/// otherwise the 401 response to send.
#[allow(clippy::result_large_err)]
pub fn authorize(token: Option<&str>, headers: &HeaderMap) -> Result<(), Response> {
    let Some(want) = token else { return Ok(()) };
    let got = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if got.is_some_and(|g| constant_time_eq(g.as_bytes(), want.as_bytes())) {
        return Ok(());
    }
    Err((
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        "missing or invalid client token\n",
    )
        .into_response())
}

/// Equality that does not stop at the first differing byte (the length is not secret).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

async fn client_file(
    State(state): State<WsState>,
    headers: HeaderMap,
    Path(file): Path<String>,
) -> Response {
    if let Err(denied) = authorize(state.token.as_deref(), &headers) {
        return denied;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_compares_bytes() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn authorize_requires_exact_bearer() {
        let with = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::AUTHORIZATION, v.parse().unwrap());
            h
        };
        assert!(authorize(None, &HeaderMap::new()).is_ok());
        assert!(authorize(Some("s3cret"), &with("Bearer s3cret")).is_ok());
        assert!(authorize(Some("s3cret"), &with("Bearer nope")).is_err());
        assert!(authorize(Some("s3cret"), &with("s3cret")).is_err());
        assert!(authorize(Some("s3cret"), &HeaderMap::new()).is_err());
    }
}
