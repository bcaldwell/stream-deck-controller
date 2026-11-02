//! Elgato plugin connections (`/plugin/ws`): slots subscribed to exported buttons and dials.
//!
//! A slot is one Elgato action instance. Unlike a deck's keys it has no page: it shows one
//! export, rendered at the size the plugin asked for, with its own dependency tracking, and
//! only sends what changed.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use sdc_protocol::{ExportItem, PluginClientMessage, SlotKind};
use tokio::sync::mpsc::UnboundedSender;

use super::{ClientId, Engine, FlashKey};
use crate::lua_api::{CtxInfo, DialInput, Export};
use crate::render::RenderSpec;

/// Slots a single plugin connection may hold.
const MAX_SLOTS: usize = 256;
/// Dial icons are rendered this big (the Stream Deck + shows them at about 40 px).
const DIAL_ICON_PX: u32 = 96;

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
    client_id: String,
    tx: UnboundedSender<PluginOut>,
    slots: HashMap<String, Slot>,
}

struct Slot {
    export: String,
    kind: SlotKind,
    size: u32,
    /// Fingerprint of what was last sent, to suppress identical messages.
    last: Option<Vec<u8>>,
}

/// Slot -> dependencies and back (see `Engine::key_deps`/`rev_deps`).
#[derive(Default)]
pub(super) struct SlotDeps {
    by_slot: HashMap<(ClientId, String), HashSet<String>>,
    by_dep: HashMap<String, HashSet<(ClientId, String)>>,
}

impl SlotDeps {
    pub(super) fn clear(&mut self) {
        self.by_slot.clear();
        self.by_dep.clear();
    }

    fn set(&mut self, id: ClientId, slot: &str, deps: HashSet<String>) {
        self.remove(id, slot);
        for d in &deps {
            self.by_dep
                .entry(d.clone())
                .or_default()
                .insert((id, slot.to_string()));
        }
        if !deps.is_empty() {
            self.by_slot.insert((id, slot.to_string()), deps);
        }
    }

    fn remove(&mut self, id: ClientId, slot: &str) {
        if let Some(old) = self.by_slot.remove(&(id, slot.to_string())) {
            for d in old {
                if let Some(set) = self.by_dep.get_mut(&d) {
                    set.remove(&(id, slot.to_string()));
                    if set.is_empty() {
                        self.by_dep.remove(&d);
                    }
                }
            }
        }
    }

    fn remove_client(&mut self, id: ClientId) {
        self.by_slot.retain(|(c, _), _| *c != id);
        for set in self.by_dep.values_mut() {
            set.retain(|(c, _)| *c != id);
        }
        self.by_dep.retain(|_, s| !s.is_empty());
    }

    fn slots_of(&self, dep: &str) -> Vec<(ClientId, String)> {
        let mut v: Vec<_> = self
            .by_dep
            .get(dep)
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        v.sort_unstable();
        v
    }
}

impl Engine {
    pub(super) fn plugin_connected(
        &mut self,
        id: ClientId,
        client_id: String,
        tx: UnboundedSender<PluginOut>,
    ) {
        tracing::info!("plugin {client_id} connected");
        let _ = tx.send(PluginOut::Exports(self.export_list()));
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
        if let Some(p) = self.plugins.remove(&id) {
            tracing::info!("plugin {} disconnected", p.client_id);
        }
        self.slot_deps.remove_client(id);
    }

    pub(super) fn plugin_message(&mut self, id: ClientId, msg: PluginClientMessage) {
        use PluginClientMessage as M;
        if !self.plugins.contains_key(&id) {
            return;
        }
        match msg {
            M::PluginHello { .. } => {}
            M::ListExports => {
                let items = self.export_list();
                self.plugin_send(id, PluginOut::Exports(items));
            }
            M::Subscribe {
                slot,
                export,
                kind,
                size_px,
            } => self.subscribe(id, slot, export, kind, size_px),
            M::Unsubscribe { slot } => {
                if let Some(p) = self.plugins.get_mut(&id) {
                    p.slots.remove(&slot);
                }
                self.slot_deps.remove(id, &slot);
            }
            M::SlotDown { slot } => self.key_input(id, &slot, true),
            M::SlotUp { slot } => self.key_input(id, &slot, false),
            M::DialRotate { slot, ticks } => self.dial_event(id, &slot, DialInput::Rotate(ticks)),
            M::DialDown { slot } => self.dial_event(id, &slot, DialInput::Press),
            M::DialUp { slot } => self.dial_event(id, &slot, DialInput::Release),
            M::TouchTap { slot } => self.dial_event(id, &slot, DialInput::Touch),
        }
    }

    fn plugin_send(&self, id: ClientId, out: PluginOut) {
        if let Some(p) = self.plugins.get(&id) {
            // A closed receiver just means the plugin is on its way out.
            let _ = p.tx.send(out);
        }
    }

    fn export_list(&self) -> Vec<ExportItem> {
        self.runtime
            .as_ref()
            .map(|rt| rt.exports())
            .unwrap_or_default()
    }

    /// Tells every plugin the (possibly new) export list.
    pub(super) fn push_exports(&mut self) {
        let items = self.export_list();
        for p in self.plugins.values() {
            let _ = p.tx.send(PluginOut::Exports(items.clone()));
        }
    }

    fn subscribe(&mut self, id: ClientId, slot: String, export: String, kind: SlotKind, size: u32) {
        let Some(p) = self.plugins.get_mut(&id) else {
            return;
        };
        let reject = if size == 0 || size > 1024 {
            Some(format!("size_px {size} is not 1-1024"))
        } else if p.slots.len() >= MAX_SLOTS && !p.slots.contains_key(&slot) {
            Some(format!("more than {MAX_SLOTS} slots"))
        } else {
            None
        };
        if let Some(message) = reject {
            p.slots.remove(&slot);
            self.slot_deps.remove(id, &slot);
            self.plugin_send(id, PluginOut::SlotError { slot, message });
            return;
        }
        p.slots.insert(
            slot.clone(),
            Slot {
                export,
                kind,
                size,
                last: None,
            },
        );
        self.render_slot(id, &slot);
    }

    pub(super) fn render_all_slots(&mut self) {
        let all: Vec<_> = self
            .plugins
            .iter()
            .flat_map(|(id, p)| p.slots.keys().map(|s| (*id, s.clone())))
            .collect();
        for (id, slot) in all {
            self.render_slot(id, &slot);
        }
    }

    /// Re-renders the slots whose last render read `dep` (called from `entity_changed`).
    pub(super) fn slots_depending_on(&mut self, dep: &str) {
        for (id, slot) in self.slot_deps.slots_of(dep) {
            self.render_slot(id, &slot);
        }
    }

    fn slot_ctx(&self, id: ClientId, slot: &str) -> Option<CtxInfo> {
        let p = self.plugins.get(&id)?;
        Some(CtxInfo {
            client_id: p.client_id.clone(),
            key: 0,
            page: String::new(),
            rows: 0,
            cols: 0,
            span: None,
            span_index: None,
            slot: Some(slot.to_string()),
        })
    }

    /// Renders one slot and sends the result if it differs from what the plugin has.
    pub(super) fn render_slot(&mut self, id: ClientId, slot: &str) {
        let Some(info) = self.slot_ctx(id, slot) else {
            return;
        };
        let Some(s) = self.plugins.get(&id).and_then(|p| p.slots.get(slot)) else {
            return;
        };
        let (kind, size, export_id) = (s.kind, s.size, s.export.clone());
        let slot_s = slot.to_string();
        let export = self.runtime.as_ref().and_then(|rt| rt.export(&export_id));
        let (fingerprint, deps, out) = match (export, kind) {
            (Some(Export::Button(def)), SlotKind::Key) => {
                let rt = self.runtime.as_ref().expect("export implies runtime");
                let mut r = rt.render(&def, &info);
                r.spec.flash = self
                    .flashes
                    .contains_key(&(id, FlashKey::Slot(slot.to_string())));
                if let Some(e) = &r.error {
                    tracing::error!("render export {export_id} for {}: {e}", info.client_id);
                }
                let mut deps = r.deps;
                deps.extend(
                    self.renderer
                        .remote_keys(&r.spec)
                        .into_iter()
                        .map(|k| format!("img:{k}")),
                );
                let png = self.renderer.render_png(&r.spec, size);
                (png.clone(), deps, PluginOut::KeyImage { slot: slot_s, png })
            }
            (Some(Export::Dial(def)), SlotKind::Dial) => {
                let rt = self.runtime.as_ref().expect("export implies runtime");
                let r = rt.render_dial(&def, &info);
                if let Some(e) = &r.error {
                    tracing::error!("render dial {export_id} for {}: {e}", info.client_id);
                }
                let mut deps = r.deps;
                let d = r.spec;
                let icon_png = d.icon.as_ref().map(|name| {
                    let spec = RenderSpec {
                        icon: Some(name.clone()),
                        bg: d.bg,
                        ..RenderSpec::default()
                    };
                    deps.extend(
                        self.renderer
                            .remote_keys(&spec)
                            .into_iter()
                            .map(|k| format!("img:{k}")),
                    );
                    self.renderer.render_png(&spec, DIAL_ICON_PX)
                });
                let mut fp = format!("{}\0{}\0{}\0", d.title, d.value, d.indicator).into_bytes();
                fp.extend(icon_png.iter().flatten());
                (
                    fp,
                    deps,
                    PluginOut::DialFeedback {
                        slot: slot_s,
                        title: d.title,
                        value: d.value,
                        indicator: d.indicator,
                        icon_png,
                    },
                )
            }
            (found, _) => {
                let message = match found {
                    None => format!("no export '{export_id}'"),
                    Some(Export::Button(_)) => {
                        format!("export '{export_id}' is a button, not a dial")
                    }
                    Some(Export::Dial(_)) => format!("export '{export_id}' is a dial, not a key"),
                };
                (
                    format!("error\0{message}").into_bytes(),
                    HashSet::new(),
                    PluginOut::SlotError {
                        slot: slot_s,
                        message,
                    },
                )
            }
        };
        self.slot_deps.set(id, slot, deps);
        self.send_slot(id, slot, fingerprint, out);
    }

    fn send_slot(&mut self, id: ClientId, slot: &str, fingerprint: Vec<u8>, out: PluginOut) {
        let Some(s) = self
            .plugins
            .get_mut(&id)
            .and_then(|p| p.slots.get_mut(slot))
        else {
            return;
        };
        if s.last.as_ref() == Some(&fingerprint) {
            return;
        }
        s.last = Some(fingerprint);
        self.plugin_send(id, out);
    }

    fn slot_export(&self, id: ClientId, slot: &str, kind: SlotKind) -> Option<(CtxInfo, Export)> {
        let s = self.plugins.get(&id)?.slots.get(slot)?;
        if s.kind != kind {
            tracing::debug!("slot {slot}: input for the wrong kind ignored");
            return None;
        }
        let export = self.runtime.as_ref()?.export(&s.export)?;
        Some((self.slot_ctx(id, slot)?, export))
    }

    fn key_input(&mut self, id: ClientId, slot: &str, down: bool) {
        let now = Instant::now();
        let Some((info, Export::Button(def))) = self.slot_export(id, slot, SlotKind::Key) else {
            return;
        };
        let Some(rt) = &self.runtime else { return };
        match rt.press(&def, &info, down) {
            Ok(Some(page)) => tracing::debug!("goto_page({page}) from a plugin key ignored"),
            Ok(None) => {
                if down && def.has_press() && def.feedback {
                    self.flash_slot(id, slot, now);
                }
            }
            Err(e) => {
                tracing::error!(
                    "export key {slot} {}: {e:#}",
                    if down { "press" } else { "release" }
                );
                let Some(size) = self
                    .plugins
                    .get(&id)
                    .and_then(|p| p.slots.get(slot))
                    .map(|s| s.size)
                else {
                    return;
                };
                let png = self.renderer.render_png(&RenderSpec::error(), size);
                self.send_slot(
                    id,
                    slot,
                    png.clone(),
                    PluginOut::KeyImage {
                        slot: slot.to_string(),
                        png,
                    },
                );
            }
        }
    }

    fn dial_event(&mut self, id: ClientId, slot: &str, input: DialInput) {
        let Some((info, Export::Dial(def))) = self.slot_export(id, slot, SlotKind::Dial) else {
            return;
        };
        let Some(rt) = &self.runtime else { return };
        if let Err(e) = rt.dial_input(&def, &info, input) {
            tracing::error!("export dial {slot} {input:?}: {e:#}");
        }
        // Handlers may change Lua state that `render` reads (a dial's mode, say); unchanged output is not resent.
        self.render_slot(id, slot);
    }
}
