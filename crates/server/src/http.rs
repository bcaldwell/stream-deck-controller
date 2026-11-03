//! HTTP side of the server: the `/ws` upgrade plus client distribution and health.
//!
//! - `GET /client/<target>`         client binary (404 if the server has none for the target)
//! - `GET /client/<target>.sha256`  its hex sha256
//! - `GET /healthz`                 readiness probe
//! - `GET /plugin/ws`               websocket for the Elgato plugin (same token as `/ws`)
//!
//! With a client token configured, `/ws`, `/plugin/ws` and `/client/*` need `Authorization: Bearer <token>`.
//!
//! The browser deck lives under `/deck` and is authenticated differently: the cluster's SSO
//! (Traefik forwardAuth -> oauth2-proxy) puts the user in `X-Auth-Request-Email`, which every
//! `/deck` route requires unless `SDC_DECK_AUTH=none`.
//!
//! - `GET /deck`                          the UI (also `/deck/`), one self-contained page
//! - `GET /deck/manifest.webmanifest`     PWA manifest (landscape, fullscreen)
//! - `GET /deck/icon.svg`                 its icon
//! - `GET /deck/ws`                       websocket for browser decks (same protocol as `/ws`)
//! - `GET /deck/api/devices`              configured `sdc.device`s (`[{name, pages, home}]`)

use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedSender;

use crate::clients::ClientBinaries;
use crate::engine::EngineEvent;
use crate::ws;

/// How `/deck` routes are authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeckAuth {
    /// Require the SSO proxy's `X-Auth-Request-Email` header (401 without).
    Sso,
    /// No authentication (local development only).
    None,
}

impl DeckAuth {
    /// From `SDC_DECK_AUTH`: `none` disables the check, anything else keeps it.
    pub fn from_env() -> Self {
        match std::env::var("SDC_DECK_AUTH").as_deref() {
            Ok("none") => Self::None,
            _ => Self::Sso,
        }
    }
}

/// The user a `/deck` request was authenticated as (request extension).
#[derive(Debug, Clone)]
pub struct DeckUser(pub String);

const AUTH_EMAIL_HEADER: &str = "x-auth-request-email";
const NO_STORE: (header::HeaderName, &str) = (header::CACHE_CONTROL, "no-store");

#[derive(Clone)]
pub struct AppState {
    pub engine: UnboundedSender<EngineEvent>,
    pub binaries: Arc<ClientBinaries>,
}

/// A static page served as is.
fn page(content_type: &'static str, body: &'static str) -> axum::routing::MethodRouter<AppState> {
    get(move || async move { ([(header::CONTENT_TYPE, content_type), NO_STORE], body) })
}

/// `token`: the shared client token (`None` allows everyone).
pub fn router(
    engine: UnboundedSender<EngineEvent>,
    binaries: Arc<ClientBinaries>,
    token: Option<String>,
    deck_auth: DeckAuth,
) -> Router {
    let html = || {
        page(
            "text/html; charset=utf-8",
            include_str!("../assets/deck.html"),
        )
    };
    let deck = Router::new()
        .route("/deck", html())
        .route("/deck/", html())
        .route(
            "/deck/manifest.webmanifest",
            page(
                "application/manifest+json",
                include_str!("../assets/deck.webmanifest"),
            ),
        )
        .route(
            "/deck/icon.svg",
            page("image/svg+xml", include_str!("../assets/deck-icon.svg")),
        )
        .route("/deck/ws", get(ws::deck_upgrade))
        .route("/deck/api/devices", get(deck_devices))
        .route_layer(middleware::from_fn_with_state(deck_auth, deck_guard));
    let clients = Router::new()
        .route("/ws", get(ws::upgrade))
        .route("/plugin/ws", get(ws::plugin_upgrade))
        .route("/client/{file}", get(client_file))
        .route_layer(middleware::from_fn_with_state(
            token.map(Arc::<str>::from),
            client_guard,
        ));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(clients)
        .merge(deck)
        .with_state(AppState { engine, binaries })
}

pub async fn serve(
    listener: TcpListener,
    engine: UnboundedSender<EngineEvent>,
    binaries: Arc<ClientBinaries>,
    token: Option<String>,
    deck_auth: DeckAuth,
) {
    if let Err(e) = axum::serve(listener, router(engine, binaries, token, deck_auth)).await {
        tracing::error!("http server failed: {e}");
    }
}

/// Requires `Authorization: Bearer <token>` on `/ws`, `/plugin/ws` and `/client/*` when a token
/// is configured.
async fn client_guard(State(token): State<Option<Arc<str>>>, req: Request, next: Next) -> Response {
    if let Some(want) = &token {
        let got = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if !got.is_some_and(|g| constant_time_eq(g.as_bytes(), want.as_bytes())) {
            tracing::warn!(
                "rejected {}: missing or invalid client token",
                req.uri().path()
            );
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                "missing or invalid client token\n",
            )
                .into_response();
        }
    }
    next.run(req).await
}

/// Requires the SSO header on `/deck` routes and records the user for the handlers.
async fn deck_guard(State(auth): State<DeckAuth>, mut req: Request, next: Next) -> Response {
    let email = req
        .headers()
        .get(AUTH_EMAIL_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|e| !e.is_empty());
    let user = match (email, auth) {
        (Some(e), _) => e.to_string(),
        (None, DeckAuth::None) => "anonymous".to_string(),
        (None, DeckAuth::Sso) => {
            return (StatusCode::UNAUTHORIZED, "sign in required\n").into_response()
        }
    };
    req.extensions_mut().insert(DeckUser(user));
    next.run(req).await
}

/// The config's `sdc.device`s for the browser deck's layout choice.
async fn deck_devices(State(state): State<AppState>) -> Response {
    let (tx, rx) = tokio::sync::oneshot::channel();
    if state.engine.send(EngineEvent::ListDevices(tx)).is_err() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let Ok(devices) = rx.await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let devices: Vec<_> = devices
        .into_iter()
        .map(|d| json!({ "name": d.name, "pages": d.pages, "home": d.home }))
        .collect();
    (
        [(header::CONTENT_TYPE, "application/json"), NO_STORE],
        json!(devices).to_string(),
    )
        .into_response()
}

/// Equality that does not stop at the first differing byte (the length is not secret).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

async fn client_file(State(state): State<AppState>, Path(file): Path<String>) -> Response {
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
}
