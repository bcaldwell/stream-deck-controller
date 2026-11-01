//! Wire protocol between the stream-deck-controller server and its clients.
//!
//! Transport: a single WebSocket connection per client (default path `/ws`).
//! - Text frames carry JSON-encoded [`ClientMessage`] / [`ServerMessage`].
//! - Binary frames (server -> client only) carry key images, see [`ImageFrame`].
//!
//! Flow: client connects and sends [`ClientMessage::Hello`]; server replies with
//! [`ServerMessage::Welcome`] and then pushes one image per key. After that the
//! client sends key events and the server pushes images whenever a key changes.
//!
//! Self-update: the server also distributes the client binary over plain HTTP on the same
//! port (`GET /client/<target>`, `GET /client/<target>.sha256`). The client reports its
//! [`client_target`] and the sha256 of its running executable in `Hello`; if the server has a
//! different binary for that target it sends [`ServerMessage::Update`] right after `Welcome`.
//! The client then downloads `path`, verifies `sha256`, replaces itself and restarts (the
//! service manager brings it back). Both fields are optional so older clients/servers interoperate
//! and the protocol version stays 1.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

/// Platform identifier used to pick a client binary, e.g. `aarch64-linux`, `x86_64-linux`,
/// `aarch64-macos`. `None` for platforms we do not know.
pub fn client_target() -> Option<&'static str> {
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        return None;
    };
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        return None;
    };
    Some(match (arch, os) {
        ("aarch64", "linux") => "aarch64-linux",
        ("x86_64", "linux") => "x86_64-linux",
        ("aarch64", _) => "aarch64-macos",
        _ => "x86_64-macos",
    })
}

/// Lowercase hex sha256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Physical layout of the deck attached to a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// Human readable model name, e.g. "Stream Deck Original V2".
    pub model: String,
    pub serial: Option<String>,
    pub rows: u8,
    pub cols: u8,
    /// Square key image size in pixels the server should render at.
    pub key_size_px: u32,
}

impl DeviceInfo {
    pub fn key_count(&self) -> u8 {
        self.rows * self.cols
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        protocol_version: u32,
        /// Stable id for this client (e.g. hostname); used for logging and later per-client config.
        client_id: String,
        device: DeviceInfo,
        /// See [`client_target`]; absent from older clients.
        #[serde(default)]
        client_target: Option<String>,
        /// Hex sha256 of the running client executable; absent from older clients.
        #[serde(default)]
        client_sha256: Option<String>,
    },
    KeyDown {
        key: u8,
    },
    KeyUp {
        key: u8,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome {
        protocol_version: u32,
    },
    /// Set brightness 0-100.
    Brightness {
        percent: u8,
    },
    Error {
        message: String,
    },
    /// The server has a different client binary for this client's target: download
    /// `path` (an HTTP path on the same host:port as the WebSocket), check it hashes to
    /// `sha256`, replace the running executable and restart.
    Update {
        path: String,
        sha256: String,
    },
}

/// Binary frame layout (server -> client): `[key: u8][image bytes...]`.
/// Image bytes are a JPEG already rendered at `key_size_px`, upright; the client
/// is responsible for any device-specific rotation/flip.
pub struct ImageFrame<'a> {
    pub key: u8,
    pub image: &'a [u8],
}

impl<'a> ImageFrame<'a> {
    pub fn encode(key: u8, image: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(image.len() + 1);
        buf.push(key);
        buf.extend_from_slice(image);
        buf
    }

    pub fn decode(buf: &'a [u8]) -> Option<Self> {
        let (&key, image) = buf.split_first()?;
        Some(Self { key, image })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_without_update_fields_parses() {
        let j = r#"{"type":"hello","protocol_version":1,"client_id":"a",
            "device":{"model":"m","serial":null,"rows":3,"cols":5,"key_size_px":72}}"#;
        match serde_json::from_str::<ClientMessage>(j).unwrap() {
            ClientMessage::Hello {
                client_target,
                client_sha256,
                ..
            } => assert!(client_target.is_none() && client_sha256.is_none()),
            _ => panic!("not hello"),
        }
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
