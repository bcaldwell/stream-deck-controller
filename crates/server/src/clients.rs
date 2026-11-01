//! Client binaries the server hands out (for `/client/<target>` and self-update).
//!
//! `SDC_CLIENT_DIR` holds `sdc-client-<target>` files (e.g. `sdc-client-aarch64-linux`).
//! They are read and hashed once at startup, so replacing them needs a server restart
//! (which is what a new image does anyway).

use std::collections::BTreeMap;
use std::path::Path;

use axum::body::Bytes;
use sdc_protocol::{sha256_hex, ServerMessage};

pub const DEFAULT_DIR: &str = "/usr/local/share/sdc/clients";
const PREFIX: &str = "sdc-client-";

pub struct ClientBinary {
    pub bytes: Bytes,
    pub sha256: String,
}

#[derive(Default)]
pub struct ClientBinaries {
    by_target: BTreeMap<String, ClientBinary>,
}

impl ClientBinaries {
    /// Load every `sdc-client-<target>` file in `dir`. A missing dir disables the feature.
    pub fn load(dir: &Path) -> Self {
        let mut me = Self::default();
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::info!(
                    "client distribution disabled: cannot read {}: {e}",
                    dir.display()
                );
                return me;
            }
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(target) = name.strip_prefix(PREFIX) else {
                continue;
            };
            match std::fs::read(entry.path()) {
                Ok(bytes) => {
                    let sha256 = sha256_hex(&bytes);
                    tracing::info!(
                        "serving client {target}: {} bytes, sha256 {}",
                        bytes.len(),
                        &sha256[..12]
                    );
                    me.by_target.insert(
                        target.to_string(),
                        ClientBinary {
                            bytes: bytes.into(),
                            sha256,
                        },
                    );
                }
                Err(e) => tracing::warn!("skipping {}: {e}", entry.path().display()),
            }
        }
        if me.by_target.is_empty() {
            tracing::info!(
                "no {PREFIX}* files in {}: client distribution disabled",
                dir.display()
            );
        }
        me
    }

    pub fn get(&self, target: &str) -> Option<&ClientBinary> {
        self.by_target.get(target)
    }

    /// The `Update` to send a client that reported `target`/`sha256`, if we have a different
    /// binary for that target. Clients that do not report both get nothing.
    pub fn update_for(&self, target: Option<&str>, sha256: Option<&str>) -> Option<ServerMessage> {
        let target = target?;
        let ours = self.get(target)?;
        if sha256? == ours.sha256 {
            return None;
        }
        Some(ServerMessage::Update {
            path: format!("/client/{target}"),
            sha256: ours.sha256.clone(),
        })
    }
}

impl ClientBinaries {
    /// A single in-memory binary (for tests).
    pub fn with(target: &str, bytes: &[u8]) -> Self {
        let mut me = Self::default();
        me.by_target.insert(
            target.into(),
            ClientBinary {
                bytes: Bytes::copy_from_slice(bytes),
                sha256: sha256_hex(bytes),
            },
        );
        me
    }
}
