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

use axum::extract::Request;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedSender;

use crate::clients::ClientBinaries;
use crate::engine::{DeviceListing, EngineEvent};
use crate::ws::{self, WsState};

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

/// `token`: the shared client token (`None` allows everyone).
pub fn router(
    engine: UnboundedSender<EngineEvent>,
    binaries: Arc<ClientBinaries>,
    token: Option<String>,
    deck_auth: DeckAuth,
) -> Router {
    let deck = Router::new()
        .route("/deck", get(deck_page))
        .route("/deck/", get(deck_page))
        .route("/deck/manifest.webmanifest", get(deck_manifest))
        .route("/deck/icon.svg", get(deck_icon))
        .route("/deck/ws", get(ws::deck_upgrade))
        .route("/deck/api/devices", get(deck_devices))
        .route_layer(middleware::from_fn_with_state(deck_auth, deck_guard));
    Router::new()
        .route("/ws", get(ws::upgrade))
        .route("/plugin/ws", get(ws::plugin_upgrade))
        .route("/client/{file}", get(client_file))
        .route("/healthz", get(|| async { "ok" }))
        .merge(deck)
        .with_state(WsState {
            engine,
            binaries,
            token: token.map(Into::into),
            deck_auth,
        })
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

const NO_STORE: (header::HeaderName, &str) = (header::CACHE_CONTROL, "no-store");

async fn deck_page() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            NO_STORE,
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        include_str!("../assets/deck.html"),
    )
        .into_response()
}

async fn deck_manifest() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/manifest+json"),
            NO_STORE,
        ],
        include_str!("../assets/deck.webmanifest"),
    )
        .into_response()
}

async fn deck_icon() -> Response {
    (
        [(header::CONTENT_TYPE, "image/svg+xml")],
        include_str!("../assets/deck-icon.svg"),
    )
        .into_response()
}

/// The config's `sdc.device`s for the browser deck's layout choice.
async fn deck_devices(State(state): State<WsState>) -> Response {
    let (tx, rx) = tokio::sync::oneshot::channel();
    if state.engine.send(EngineEvent::ListDevices(tx)).is_err() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let devices: Vec<DeviceListing> = rx.await.unwrap_or_default();
    match serde_json::to_string(&devices) {
        Ok(body) => ([(header::CONTENT_TYPE, "application/json"), NO_STORE], body).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
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
