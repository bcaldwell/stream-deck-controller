//! Wire protocol between the stream-deck-controller server and its clients.
//!
//! Transport: a single WebSocket connection per client (default path `/ws`).
//! - Text frames carry JSON-encoded [`ClientMessage`] / [`ServerMessage`].
//! - Binary frames (server -> client only) carry key images, see [`ImageFrame`].
//!
//! Flow: client connects and sends [`ClientMessage::Hello`]; server replies with
//! [`ServerMessage::Welcome`] and then pushes one image per key. After that the
//! client sends key events and the server pushes images whenever a key changes.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

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
