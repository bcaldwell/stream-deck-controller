//! A snapshot of the engine for the status page (`GET /status`).

use std::collections::BTreeMap;
use std::time::Instant;

use serde::Serialize;

use super::{Engine, Power};

/// Ages are in seconds before the snapshot was taken.
#[derive(Serialize)]
pub struct EngineStatus {
    pub config_path: String,
    pub config_loaded_s: u64,
    /// The last hot reload that failed (the previous config stays active).
    pub reload_error: Option<ReloadError>,
    pub decks: Vec<DeckStatus>,
    pub plugins: Vec<PluginStatus>,
}

#[derive(Serialize)]
pub struct ReloadError {
    pub age_s: u64,
    pub message: String,
}

#[derive(Serialize)]
pub struct DeckStatus {
    pub client_id: String,
    pub model: String,
    pub device: String,
    pub page: String,
    pub power: &'static str,
    pub last_press_s: u64,
    pub connected_s: u64,
    pub browser: bool,
}

#[derive(Serialize)]
pub struct PluginStatus {
    pub client_id: String,
    pub connected_s: u64,
    /// Slot -> export.
    pub slots: BTreeMap<String, String>,
}

impl Engine {
    pub(super) fn status(&self, now: Instant) -> EngineStatus {
        let age = |at: Instant| now.saturating_duration_since(at).as_secs();
        EngineStatus {
            config_path: self.config_path.display().to_string(),
            config_loaded_s: age(self.loaded_at),
            reload_error: self.reload_error.as_ref().map(|(at, message)| ReloadError {
                age_s: age(*at),
                message: message.clone(),
            }),
            decks: self
                .clients
                .values()
                .map(|c| DeckStatus {
                    client_id: c.client_id.clone(),
                    model: c.device.model.clone(),
                    device: c.def.name.clone(),
                    page: c.page.clone(),
                    power: match c.power {
                        Power::Active => "active",
                        Power::Dim => "dim",
                        Power::Off => "off",
                    },
                    last_press_s: age(c.last_press),
                    connected_s: age(c.connected_at),
                    browser: c.web,
                })
                .collect(),
            plugins: self
                .plugins
                .values()
                .map(|p| PluginStatus {
                    client_id: p.client_id.clone(),
                    connected_s: age(p.connected_at),
                    slots: p
                        .slots
                        .iter()
                        .map(|(slot, s)| (slot.clone(), s.export.clone()))
                        .collect(),
                })
                .collect(),
        }
    }
}
