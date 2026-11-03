//! Render targets: what the engine draws, whether it is a key of a deck or an Elgato plugin slot.
//!
//! Both are "something that renders a button or dial through Lua, depends on entities and
//! images, shows only what changed, can flash after a press, can be redrawn later and answers
//! presses (with an ERR tile when the handler fails)". This module is the one implementation of
//! that bookkeeping. What differs is the drawing: a deck rasterizes the page's cells into JPEGs
//! for its keys, a slot draws its export into a PNG or dial feedback; each has its own render.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Display;
use std::time::{Duration, Instant};

use sdc_protocol::SlotKind;

use super::{ClientId, Engine, EngineEvent, Frame, Out, PluginOut};
use crate::lua_api::{ButtonDef, Cell, CtxInfo, CtxSpan, DialDef, DialInput, Export, DIAL_SETTLE};
use crate::render::{encode_all, Color, Encoding, Grid, RenderSpec, Renderer};

/// How long a target shows the press flash.
const FLASH: Duration = Duration::from_millis(400);
/// Dial icons are rendered this big (the Stream Deck + shows them at about 40 px).
const DIAL_ICON_PX: u32 = 96;

/// Where a target lives: a key of a deck, or a slot of a plugin connection.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TargetId {
    Key(ClientId, u8),
    Slot(ClientId, String),
}

impl TargetId {
    /// The connection (deck or plugin) the target belongs to.
    fn conn(&self) -> ClientId {
        match self {
            TargetId::Key(c, _) | TargetId::Slot(c, _) => *c,
        }
    }
}

/// The state every target has; `T` is what it last sent (a deck key's JPEG, a slot's message).
pub(super) struct Target<T> {
    /// To suppress identical output.
    last: Option<T>,
    /// While set (and in the future) the target shows the press flash.
    pub(super) flash_until: Option<Instant>,
}

impl<T> Target<T> {
    pub(super) fn new() -> Self {
        Target {
            last: None,
            flash_until: None,
        }
    }
}

/// Targets <-> the things their last render read: an entity id, or `img:<cache key>` for a
/// remote image (so targets re-render when their download completes).
#[derive(Default)]
pub(super) struct Deps {
    of: HashMap<TargetId, HashSet<String>>,
    readers: HashMap<String, HashSet<TargetId>>,
}

impl Deps {
    pub(super) fn clear(&mut self) {
        self.of.clear();
        self.readers.clear();
    }

    /// Replaces what `id` depends on.
    fn set(&mut self, id: TargetId, deps: HashSet<String>) {
        self.remove(&id);
        for d in &deps {
            self.readers
                .entry(d.clone())
                .or_default()
                .insert(id.clone());
        }
        if !deps.is_empty() {
            self.of.insert(id, deps);
        }
    }

    pub(super) fn remove(&mut self, id: &TargetId) {
        if let Some(old) = self.of.remove(id) {
            for d in old {
                if let Some(set) = self.readers.get_mut(&d) {
                    set.remove(id);
                    if set.is_empty() {
                        self.readers.remove(&d);
                    }
                }
            }
        }
    }

    /// Forgets every target of a connection.
    pub(super) fn remove_conn(&mut self, conn: ClientId) {
        self.of.retain(|id, _| id.conn() != conn);
        for set in self.readers.values_mut() {
            set.retain(|id| id.conn() != conn);
        }
        self.readers.retain(|_, s| !s.is_empty());
    }

    pub(super) fn readers_of(&self, dep: &str) -> Vec<TargetId> {
        self.readers
            .get(dep)
            .into_iter()
            .flatten()
            .cloned()
            .collect()
    }

    #[cfg(test)]
    pub(super) fn reader_count(&self, dep: &str) -> usize {
        self.readers.get(dep).map_or(0, HashSet::len)
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.of.is_empty() && self.readers.is_empty()
    }
}

/// What a plugin slot shows: its export, when that exists and is of the slot's kind.
enum SlotContent {
    Key(ButtonDef),
    Dial(DialDef),
}

fn fail(info: &CtxInfo, what: &str, e: impl Display) {
    tracing::error!(client = %info.client_id, at = ?info.at, "{what}: {e}");
}

/// The dependency ids of the remote images `spec` shows (see [`Deps`]).
fn image_deps(renderer: &mut Renderer, spec: &RenderSpec) -> Vec<String> {
    renderer
        .remote_keys(spec)
        .into_iter()
        .map(|k| format!("img:{k}"))
        .collect()
}

impl Engine {
    /// The flash state of `id`; `None` if the target is gone.
    fn flash_until(&mut self, id: &TargetId) -> Option<&mut Option<Instant>> {
        match id {
            TargetId::Key(c, key) => self
                .clients
                .get_mut(c)
                .map(|c| &mut c.keys[*key as usize].flash_until),
            TargetId::Slot(c, slot) => self
                .plugins
                .get_mut(c)
                .and_then(|p| p.slots.get_mut(slot))
                .map(|s| &mut s.target.flash_until),
        }
    }

    fn slot_content(&self, conn: ClientId, slot: &str) -> Result<SlotContent, String> {
        let s = &self.plugins[&conn].slots[slot];
        match (self.rt.export(&s.export), s.kind) {
            (Some(Export::Button(b)), SlotKind::Key) => Ok(SlotContent::Key(b)),
            (Some(Export::Dial(d)), SlotKind::Dial) => Ok(SlotContent::Dial(d)),
            (None, _) => Err(format!("no export '{}'", s.export)),
            (Some(Export::Button(_)), _) => {
                Err(format!("export '{}' is a button, not a dial", s.export))
            }
            (Some(Export::Dial(_)), _) => {
                Err(format!("export '{}' is a dial, not a key", s.export))
            }
        }
    }

    /// Re-renders `ids`; whatever changed is sent.
    pub(super) fn render(&mut self, ids: Vec<TargetId>) {
        let mut decks: BTreeMap<ClientId, BTreeSet<u8>> = BTreeMap::new();
        let mut slots = BTreeSet::new();
        for id in ids {
            match id {
                TargetId::Key(c, key) => {
                    decks.entry(c).or_default().insert(key);
                }
                TargetId::Slot(c, slot) => {
                    slots.insert((c, slot));
                }
            }
        }
        for (conn, keys) in decks {
            self.render_deck(conn, keys);
        }
        for (conn, slot) in slots {
            self.render_slot(conn, &slot);
        }
    }

    /// Renders keys of a deck: Lua runs one key after the other on this thread (a span is drawn
    /// once for all its keys), then the pictures are rasterized in parallel.
    fn render_deck(&mut self, conn: ClientId, keys: BTreeSet<u8>) {
        let started = Instant::now();
        let client = &self.clients[&conn];
        let size = client.device.key_size_px;
        let mut spans = HashSet::new();
        // The key of each picture in `jobs`, in order.
        let mut order = Vec::new();
        let mut jobs = Vec::new();
        for &key in &keys {
            // The keys drawn, what they read, and the picture (`None`: the ERR tile on each key).
            let (members, mut deps, art) = match &client.cells[key as usize] {
                Cell::Empty => (
                    vec![key],
                    HashSet::new(),
                    Some((
                        RenderSpec {
                            bg: Color::BLACK,
                            ..RenderSpec::default()
                        },
                        Grid::one(size),
                    )),
                ),
                Cell::Button(btn) => {
                    let info = client.ctx(key, None);
                    let out = self.rt.render(btn, &info);
                    if let Some(e) = &out.error {
                        fail(&info, "render", e);
                    }
                    let mut spec = out.spec;
                    spec.flash = client.keys[key as usize].flash_until.is_some();
                    (vec![key], out.deps, Some((spec, Grid::one(size))))
                }
                Cell::Span(sp) => {
                    if !spans.insert(sp.origin) {
                        continue;
                    }
                    let members = client
                        .cells
                        .iter()
                        .enumerate()
                        .filter(|(_, cell)| matches!(cell, Cell::Span(o) if o.origin == sp.origin))
                        .map(|(k, _)| k as u8)
                        .collect();
                    // The origin renders for the whole span.
                    let info = client.ctx(
                        sp.origin,
                        Some(CtxSpan {
                            cols: sp.size.cols,
                            rows: sp.size.rows,
                            index: None,
                        }),
                    );
                    let grid = Grid {
                        cols: sp.size.cols as u32,
                        rows: sp.size.rows as u32,
                        key: size,
                    };
                    if let Some(e) = &sp.error {
                        fail(&info, "render", e);
                        (members, HashSet::new(), None)
                    } else {
                        let out = self.rt.render(&sp.def, &info);
                        match out.error {
                            Some(e) => {
                                fail(&info, "render", e);
                                (members, out.deps, None)
                            }
                            None => (members, out.deps, Some((out.spec, grid))),
                        }
                    }
                }
            };
            match art {
                Some((spec, grid)) => {
                    deps.extend(image_deps(&mut self.renderer, &spec));
                    jobs.push(self.renderer.prepare(&spec, grid));
                }
                None => jobs.extend(
                    members
                        .iter()
                        .map(|_| self.renderer.prepare(&RenderSpec::error(), Grid::one(size))),
                ),
            }
            for &k in &members {
                self.deps.set(TargetId::Key(conn, k), deps.clone());
            }
            order.extend(members);
        }
        let jpegs = encode_all(jobs, Encoding::Jpeg(client.device.native.as_ref()));
        for (key, jpeg) in order.into_iter().zip(jpegs) {
            self.send_key(conn, key, jpeg);
        }
        tracing::debug!(
            conn,
            keys = keys.len(),
            ms = started.elapsed().as_secs_f64() * 1000.0,
            "rendered"
        );
    }

    /// Renders a plugin slot from its export, at the size the plugin asked for.
    pub(super) fn render_slot(&mut self, conn: ClientId, slot: &str) {
        let plugin = &self.plugins[&conn];
        let info = plugin.ctx(slot);
        let size = plugin.slots[slot].size;
        let flash = plugin.slots[slot].target.flash_until.is_some();
        let slot_name = slot.to_string();
        let mut deps = HashSet::new();
        let out = match self.slot_content(conn, slot) {
            Ok(SlotContent::Key(btn)) => {
                let r = self.rt.render(&btn, &info);
                if let Some(e) = &r.error {
                    fail(&info, "render", e);
                }
                let mut spec = r.spec;
                spec.flash = flash;
                deps = r.deps;
                deps.extend(image_deps(&mut self.renderer, &spec));
                PluginOut::KeyImage {
                    slot: slot_name,
                    png: self.renderer.render(&spec, size, Encoding::Png),
                }
            }
            Ok(SlotContent::Dial(dial)) => {
                let r = self.rt.render(&dial, &info);
                if let Some(e) = &r.error {
                    fail(&info, "render", e);
                }
                deps = r.deps;
                let icon = r.spec.icon.as_ref().map(|name| RenderSpec {
                    icon: Some(name.clone()),
                    bg: r.spec.bg,
                    ..RenderSpec::default()
                });
                if let Some(icon) = &icon {
                    deps.extend(image_deps(&mut self.renderer, icon));
                }
                PluginOut::DialFeedback {
                    slot: slot_name,
                    title: r.spec.title,
                    value: r.spec.value,
                    indicator: r.spec.indicator,
                    icon_png: icon.map(|s| self.renderer.render(&s, DIAL_ICON_PX, Encoding::Png)),
                }
            }
            Err(message) => {
                fail(&info, "render", &message);
                PluginOut::SlotError {
                    slot: slot_name,
                    message,
                }
            }
        };
        self.deps.set(TargetId::Slot(conn, slot.to_string()), deps);
        self.send_slot(conn, slot, out);
    }

    /// Sends a key's picture unless it is what the key already shows.
    fn send_key(&mut self, conn: ClientId, key: u8, jpeg: Vec<u8>) {
        let c = self.clients.get_mut(&conn).expect("rendered client exists");
        let target = &mut c.keys[key as usize];
        if target.last.as_ref() == Some(&jpeg) {
            return;
        }
        target.last = Some(jpeg.clone());
        // A closed receiver just means the connection is on its way out.
        let _ = c.tx.send(Out::Frame(Frame { key, jpeg }));
    }

    /// Sends `out` unless it is what the slot already shows.
    fn send_slot(&mut self, conn: ClientId, slot: &str, out: PluginOut) {
        let p = self.plugins.get_mut(&conn).expect("rendered plugin exists");
        let target = &mut p.slots.get_mut(slot).expect("rendered slot exists").target;
        if target.last.as_ref() == Some(&out) {
            return;
        }
        target.last = Some(out.clone());
        let _ = p.tx.send(out);
    }

    /// Replaces what `id` shows with the red ERR tile (until its next render).
    fn show_error_tile(&mut self, id: &TargetId) {
        match id {
            TargetId::Key(c, key) => {
                let device = &self.clients[c].device;
                let job = self
                    .renderer
                    .prepare(&RenderSpec::error(), Grid::one(device.key_size_px));
                let jpeg = encode_all(vec![job], Encoding::Jpeg(device.native.as_ref())).remove(0);
                self.send_key(*c, *key, jpeg);
            }
            TargetId::Slot(c, slot) => {
                let size = self.plugins[c].slots[slot].size;
                let png = self
                    .renderer
                    .render(&RenderSpec::error(), size, Encoding::Png);
                let slot_name = slot.clone();
                self.send_slot(
                    *c,
                    slot,
                    PluginOut::KeyImage {
                        slot: slot_name,
                        png,
                    },
                );
            }
        }
    }

    /// Re-renders the targets that read `dep`.
    pub(super) fn dep_changed(&mut self, dep: &str) {
        let ids = self.deps.readers_of(dep);
        self.render(ids);
    }

    /// Runs the press (`down`) or release handler of a key or key slot.
    pub(super) fn press(&mut self, id: &TargetId, down: bool, now: Instant) {
        let (btn, info, in_span) = match id {
            TargetId::Key(c, key) => {
                let client = &self.clients[c];
                match &client.cells[*key as usize] {
                    Cell::Button(b) => (b.clone(), client.ctx(*key, None), false),
                    Cell::Span(sp) if sp.error.is_none() => {
                        let span = CtxSpan {
                            cols: sp.size.cols,
                            rows: sp.size.rows,
                            index: Some(sp.index),
                        };
                        (sp.def.clone(), client.ctx(*key, Some(span)), true)
                    }
                    _ => return,
                }
            }
            TargetId::Slot(c, slot) => match self.slot_content(*c, slot) {
                Ok(SlotContent::Key(b)) => (b, self.plugins[c].ctx(slot), false),
                _ => return,
            },
        };
        match self.rt.press(&btn, &info, down) {
            Ok(Some(page)) => match id {
                TargetId::Key(c, _) => self.goto_page(*c, &page),
                TargetId::Slot(..) => {
                    tracing::debug!("goto_page({page}) from a plugin key ignored")
                }
            },
            Ok(None) => {
                if down && btn.has_press() && btn.feedback && !in_span {
                    self.flash(id, now);
                }
            }
            Err(e) => {
                fail(
                    &info,
                    if down { "press" } else { "release" },
                    format!("{e:#}"),
                );
                self.show_error_tile(id);
            }
        }
    }

    /// Runs a dial slot's handler for `input`, then redraws it.
    pub(super) fn turn(&mut self, conn: ClientId, slot: &str, input: DialInput) {
        let Ok(SlotContent::Dial(dial)) = self.slot_content(conn, slot) else {
            return;
        };
        let info = self.plugins[&conn].ctx(slot);
        if let Err(e) = self.rt.dial_input(&dial, &info, input) {
            fail(&info, &format!("{input:?}"), format!("{e:#}"));
        }
        // Handlers may change Lua state that `render` reads (a dial's mode, say); unchanged
        // output is not resent.
        self.render_slot(conn, slot);
        if matches!(input, DialInput::Rotate(_)) {
            // Built-in dials show their own value while being turned; redraw once that is over so
            // the slot shows Home Assistant's state again even if no state change arrives.
            self.redraw_later(
                TargetId::Slot(conn, slot.to_string()),
                DIAL_SETTLE + Duration::from_millis(50),
            );
        }
    }

    /// Shows the press flash on `id` for [`FLASH`].
    fn flash(&mut self, id: &TargetId, now: Instant) {
        if !self.rt.feedback() {
            return;
        }
        *self.flash_until(id).expect("pressed target exists") = Some(now + FLASH);
        self.render(vec![id.clone()]);
        self.redraw_later(id.clone(), FLASH);
    }

    /// Sends [`EngineEvent::Redraw`] for `id` after `delay`.
    fn redraw_later(&self, id: TargetId, delay: Duration) {
        let events = self.events.clone();
        self.handle.spawn(async move {
            tokio::time::sleep(delay).await;
            if let Some(tx) = events.upgrade() {
                let _ = tx.send(EngineEvent::Redraw(id));
            }
        });
    }

    /// A scheduled redraw: ends the target's flash if it is over, then renders it.
    pub(super) fn redraw(&mut self, id: TargetId, now: Instant) {
        // The one place a target may have gone since the event was scheduled.
        let Some(flash_until) = self.flash_until(&id) else {
            return;
        };
        if flash_until.is_some_and(|until| until <= now) {
            *flash_until = None;
        }
        self.render(vec![id]);
    }
}
