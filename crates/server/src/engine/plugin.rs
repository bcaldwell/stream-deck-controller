//! Elgato plugin connections (`/plugin/ws`): slots subscribed to exported buttons and dials.
//!
//! A slot is one Elgato action instance. Unlike a deck's keys it has no page: it shows one
//! export, rendered at the size the plugin asked for. Each slot is a render target (see
//! `target.rs`); this module only keeps the connection state and maps messages onto targets.

use std::collections::HashMap;
use std::time::Instant;

use sdc_protocol::{ExportItem, PluginClientMessage, SlotKind};
use tokio::sync::mpsc::UnboundedSender;

use super::target::{Target, TargetId};
use super::{ClientId, Engine};
use crate::lua_api::DialInput;

/// What the engine sends to a plugin connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginOut {
    Exports(Vec<ExportItem>),
    KeyImage {
        slot: String,
        png: Vec<u8>,
    },
    DialFeedback {
        slot: String,
        title: String,
        value: String,
        indicator: u8,
        icon_png: Option<Vec<u8>>,
    },
    SlotError {
        slot: String,
        message: String,
    },
}

pub(super) struct Plugin {
    pub(super) client_id: String,
    pub(super) tx: UnboundedSender<PluginOut>,
    pub(super) slots: HashMap<String, Slot>,
}

pub(super) struct Slot {
    pub(super) export: String,
    pub(super) kind: SlotKind,
    /// Image size in px.
    pub(super) size: u32,
    pub(super) target: Target,
}

impl Engine {
    pub(super) fn plugin_connected(
        &mut self,
        id: ClientId,
        client_id: String,
        tx: UnboundedSender<PluginOut>,
    ) {
        tracing::info!("plugin {client_id} connected");
        let _ = tx.send(PluginOut::Exports(self.rt.exports()));
        self.plugins.insert(
            id,
            Plugin {
                client_id,
                tx,
                slots: HashMap::new(),
            },
        );
    }

    pub(super) fn plugin_disconnected(&mut self, id: ClientId) {
        let p = self.plugins.remove(&id).expect("a connected plugin");
        tracing::info!("plugin {} disconnected", p.client_id);
        self.deps.remove_conn(id);
    }

    /// A message from plugin `id`; its slot ids and sizes were checked by the connection.
    pub(super) fn plugin_message(&mut self, id: ClientId, msg: PluginClientMessage, now: Instant) {
        use PluginClientMessage as M;
        let slot = |s: String| TargetId::Slot(id, s);
        match msg {
            M::PluginHello { .. } => unreachable!("the connection consumes the hello"),
            M::ListExports => {
                let items = self.rt.exports();
                let _ = self.plugins[&id].tx.send(PluginOut::Exports(items));
            }
            M::Subscribe {
                slot: name,
                export,
                kind,
                size_px,
            } => {
                self.plugins
                    .get_mut(&id)
                    .expect("a connected plugin")
                    .slots
                    .insert(
                        name.clone(),
                        Slot {
                            export,
                            kind,
                            size: size_px,
                            target: Target::default(),
                        },
                    );
                self.render(vec![slot(name)]);
            }
            M::Unsubscribe { slot: name } => {
                self.plugins
                    .get_mut(&id)
                    .expect("a connected plugin")
                    .slots
                    .remove(&name);
                self.deps.remove(&slot(name));
            }
            M::SlotDown { slot: s } => self.press(&slot(s), true, now),
            M::SlotUp { slot: s } => self.press(&slot(s), false, now),
            M::DialRotate { slot: s, ticks } => self.turn(&slot(s), DialInput::Rotate(ticks)),
            M::DialDown { slot: s } => self.turn(&slot(s), DialInput::Press),
            M::DialUp { slot: s } => self.turn(&slot(s), DialInput::Release),
            M::TouchTap { slot: s } => self.turn(&slot(s), DialInput::Touch),
        }
    }

    /// Tells every plugin the (possibly new) export list.
    pub(super) fn push_exports(&self) {
        let items = self.rt.exports();
        for p in self.plugins.values() {
            let _ = p.tx.send(PluginOut::Exports(items.clone()));
        }
    }
}
