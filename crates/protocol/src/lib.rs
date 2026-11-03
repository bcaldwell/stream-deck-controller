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
//!
//! Elgato plugin: the Stream Deck plugin (`plugins/elgato`) is a different kind of client on its
//! own route, `/plugin/ws` (same bearer token). Instead of a deck with pages it subscribes
//! *slots* (Elgato action instances) to buttons and dials the Lua config exported with
//! `sdc.export`, see [`PluginClientMessage`] / [`PluginServerMessage`]. All messages are JSON text
//! frames; images are base64 PNGs because the Elgato SDK takes data URLs.
//!
//! Browser decks use the same messages on `/deck/ws` (behind the cluster's SSO instead of the
//! client token).

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

/// Device types a config can key `sdc.default_device` by (besides `"*"`): [`device_type`] of a
/// physical deck, or `"browser"` for browser decks.
pub const DEVICE_TYPES: &[&str] = &[
    "original", "mk2", "mini", "xl", "plus", "plus-xl", "neo", "pedal", "browser",
];

/// Short stable type of a physical deck from [`DeviceInfo::model`] (the `elgato-streamdeck`
/// `Kind` name the client sends, e.g. `Mk2Scissor` -> `mk2`). `"unknown"` for anything else.
pub fn device_type(model: &str) -> &'static str {
    match model {
        "Original" | "OriginalV2" => "original",
        "Mk2" | "Mk2Scissor" | "Mk2Module" | "Mk2ModuleScissor" => "mk2",
        "Mini" | "MiniDiscord" | "MiniMk2" | "MiniMk2Module" => "mini",
        "Xl" | "XlV2" | "XlV2Module" => "xl",
        "Plus" => "plus",
        "PlusXl" => "plus-xl",
        "Neo" => "neo",
        "Pedal" => "pedal",
        _ => "unknown",
    }
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
    /// The deck's native key image format. When present the server may send device-ready
    /// frames (see [`ServerMessage::Welcome::native_frames`]); absent from older clients and
    /// browser decks, which get upright JPEGs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<NativeImage>,
}

/// Mirroring a deck wants applied after rotation (mirrors `ImageMirroring`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeMirror {
    None,
    X,
    Y,
    Both,
}

/// The JPEG format a deck takes on its keys: the upright image is rotated clockwise by
/// `rotation` degrees, resized to `size`, mirrored, then encoded as JPEG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeImage {
    pub size: (u32, u32),
    /// 0, 90, 180 or 270 degrees clockwise.
    pub rotation: u16,
    pub mirror: NativeMirror,
}

impl NativeImage {
    /// Whether the server can produce these frames for a square `key_size_px` key: a size
    /// equal to the key size and a right-angle rotation.
    pub fn server_can_produce(&self, key_size_px: u32) -> bool {
        self.size == (key_size_px, key_size_px) && matches!(self.rotation, 0 | 90 | 180 | 270)
    }
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
        /// Name of the Lua device (`sdc.device`) this client wants; unknown or absent: the
        /// server picks one from `sdc.default_device`. (Not `device`: that is the hardware.)
        #[serde(default)]
        device_name: Option<String>,
    },
    KeyDown {
        key: u8,
    },
    KeyUp {
        key: u8,
    },
    /// The deck woke from USB suspend (a key press brought it back; the press itself was not
    /// seen). Handled like a key press on a dimmed/off deck that runs no button: the server
    /// restores the active brightness and restarts the idle timers.
    Wake,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome {
        protocol_version: u32,
        /// Binary frames are device-ready JPEGs in the `DeviceInfo::native` format the client
        /// sent (rotated, mirrored, encoded once), to be written to the deck as they are.
        /// Absent/false: upright JPEGs (browser decks).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        native_frames: bool,
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

/// What an export is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotKind {
    /// A button (`sdc.button` / `sdc.span`), shown as an image on a key.
    Key,
    /// An `sdc.dial`, shown as feedback on a Stream Deck + dial.
    Dial,
}

/// One entry of the server's export list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportItem {
    pub id: String,
    pub kind: SlotKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Plugin -> server (`/plugin/ws`). `slot` is the Elgato action context id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginClientMessage {
    PluginHello {
        protocol_version: u32,
        client_id: String,
    },
    /// Show export `export` in `slot`. Keys are rendered at `size_px` square. Subscribing an
    /// already subscribed slot replaces it.
    Subscribe {
        slot: String,
        export: String,
        kind: SlotKind,
        size_px: u32,
    },
    Unsubscribe {
        slot: String,
    },
    SlotDown {
        slot: String,
    },
    SlotUp {
        slot: String,
    },
    /// Net rotation since the last message (negative = counter-clockwise); the plugin batches.
    DialRotate {
        slot: String,
        ticks: i32,
    },
    DialDown {
        slot: String,
    },
    DialUp {
        slot: String,
    },
    TouchTap {
        slot: String,
    },
    /// Ask for [`PluginServerMessage::Exports`] again.
    ListExports,
}

/// Server -> plugin (`/plugin/ws`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginServerMessage {
    PluginWelcome {
        protocol_version: u32,
    },
    /// All exports; sent after the welcome, on `ListExports` and whenever the config reloads.
    Exports {
        items: Vec<ExportItem>,
    },
    /// A key's image (PNG), sent only when it changed.
    KeyImage {
        slot: String,
        png_base64: String,
    },
    /// A dial's content, sent only when it changed. `indicator` is 0-100.
    DialFeedback {
        slot: String,
        title: String,
        value: String,
        indicator: u8,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        icon_png_base64: Option<String>,
    },
    /// The slot cannot show its export (unknown, removed, wrong kind, bad size).
    SlotError {
        slot: String,
        message: String,
    },
    Error {
        message: String,
    },
}

/// Binary frame layout (server -> client): `[key: u8][image bytes...]`.
/// Image bytes are a JPEG already rendered at `key_size_px`. Normally upright, and the client
/// is responsible for any device-specific rotation/flip; if `Welcome.native_frames` is set they
/// are already in the client's native format.
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
                device_name,
                ..
            } => {
                assert!(client_target.is_none() && client_sha256.is_none() && device_name.is_none())
            }
            _ => panic!("not hello"),
        }
    }

    #[test]
    fn old_welcome_and_device_parse_and_native_roundtrips() {
        let w: ServerMessage =
            serde_json::from_str(r#"{"type":"welcome","protocol_version":1}"#).unwrap();
        assert!(matches!(
            w,
            ServerMessage::Welcome {
                native_frames: false,
                ..
            }
        ));
        let w = ServerMessage::Welcome {
            protocol_version: 1,
            native_frames: false,
        };
        assert_eq!(
            serde_json::to_string(&w).unwrap(),
            r#"{"type":"welcome","protocol_version":1}"#
        );
        let w: ServerMessage =
            serde_json::from_str(r#"{"type":"welcome","protocol_version":1,"native_frames":true}"#)
                .unwrap();
        assert!(matches!(
            w,
            ServerMessage::Welcome {
                native_frames: true,
                ..
            }
        ));
        let d = DeviceInfo {
            model: "m".into(),
            serial: None,
            rows: 3,
            cols: 5,
            key_size_px: 72,
            native: Some(NativeImage {
                size: (72, 72),
                rotation: 180,
                mirror: NativeMirror::None,
            }),
        };
        let j = serde_json::to_string(&d).unwrap();
        assert!(j.contains(r#""native":{"size":[72,72],"rotation":180,"mirror":"none"}"#));
        assert_eq!(serde_json::from_str::<DeviceInfo>(&j).unwrap(), d);
        let mut old = d.clone();
        old.native = None;
        assert!(!serde_json::to_string(&old).unwrap().contains("native"));
        assert!(d.native.unwrap().server_can_produce(72));
        assert!(!d.native.unwrap().server_can_produce(96));
    }

    #[test]
    fn device_types_from_kind_names() {
        assert_eq!(device_type("OriginalV2"), "original");
        assert_eq!(device_type("Mk2Scissor"), "mk2");
        assert_eq!(device_type("XlV2"), "xl");
        assert_eq!(device_type("Plus"), "plus");
        assert_eq!(device_type("Fake Stream Deck"), "unknown");
        assert!(["Original", "MiniMk2", "Neo", "Pedal", "PlusXl"]
            .iter()
            .all(|m| DEVICE_TYPES.contains(&device_type(m))));
    }

    #[test]
    fn plugin_messages_roundtrip() {
        let m = PluginClientMessage::Subscribe {
            slot: "s".into(),
            export: "e".into(),
            kind: SlotKind::Dial,
            size_px: 144,
        };
        let j = serde_json::to_string(&m).unwrap();
        assert_eq!(
            j,
            r#"{"type":"subscribe","slot":"s","export":"e","kind":"dial","size_px":144}"#
        );
        assert_eq!(serde_json::from_str::<PluginClientMessage>(&j).unwrap(), m);
        let j = r#"{"type":"list_exports"}"#;
        assert_eq!(
            serde_json::from_str::<PluginClientMessage>(j).unwrap(),
            PluginClientMessage::ListExports
        );
        let e = PluginServerMessage::Exports {
            items: vec![ExportItem {
                id: "a".into(),
                kind: SlotKind::Key,
                label: None,
            }],
        };
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"type":"exports","items":[{"id":"a","kind":"key"}]}"#
        );
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
