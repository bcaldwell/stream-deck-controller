//! Render targets: what the engine draws, whether it is a key of a deck or an Elgato plugin slot.
//!
//! Both are "something that renders a button or dial through Lua, depends on entities and
//! images, shows only what changed, can flash after a press, can be redrawn later and answers
//! presses (with an ERR tile when the handler fails)". This module is the one implementation of
//! that; the engine's clients and plugins only say which targets exist and where their output goes.

use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::time::{Duration, Instant};

use sdc_protocol::SlotKind;

use super::{ClientId, Engine, EngineEvent, Frame, Out, PluginOut};
use crate::lua_api::{
    ButtonDef, Cell, CtxInfo, DialDef, DialInput, DialSpec, Export, SpanCell, DIAL_SETTLE,
};
use crate::render::{encode_all, Color, Encoding, Grid, RenderSpec};

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

/// The state every target has.
#[derive(Default)]
pub(super) struct Target {
    /// What was last sent, to suppress identical output.
    last: Option<Shown>,
    /// While set (and in the future) the target shows the press flash.
    pub(super) flash_until: Option<Instant>,
}

/// A target's output, ready to send.
#[derive(Clone, PartialEq)]
enum Shown {
    /// An encoded image: a JPEG on a deck, a PNG on a plugin.
    Tile(Vec<u8>),
    Dial {
        title: String,
        value: String,
        indicator: u8,
        icon_png: Option<Vec<u8>>,
    },
    /// A slot that cannot show anything (its export is missing or of the wrong kind).
    Error(String),
}

impl Shown {
    fn into_plugin(self, slot: &str) -> PluginOut {
        let slot = slot.to_string();
        match self {
            Shown::Tile(png) => PluginOut::KeyImage { slot, png },
            Shown::Dial {
                title,
                value,
                indicator,
                icon_png,
            } => PluginOut::DialFeedback {
                slot,
                title,
                value,
                indicator,
                icon_png,
            },
            Shown::Error(message) => PluginOut::SlotError { slot, message },
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

/// What a target is set to show.
enum Content {
    Empty,
    Button(ButtonDef),
    /// A key covered by a multi-key button (which may not fit its page: `error`).
    Span(SpanCell),
    Dial(DialDef),
    /// A slot whose export is missing or of the wrong kind.
    Invalid(String),
}

/// What Lua asked a target to draw.
enum Art {
    Tile(RenderSpec),
    /// One picture over all keys of a span.
    Span(RenderSpec, Grid),
    Dial {
        spec: DialSpec,
        icon: Option<RenderSpec>,
    },
    /// Cannot be shown: ERR on a deck's keys, a `SlotError` for a plugin.
    Error(String),
}

impl Art {
    /// The spec whose pictures the output depends on.
    fn spec(&self) -> Option<&RenderSpec> {
        match self {
            Art::Tile(s) | Art::Span(s, _) => Some(s),
            Art::Dial { icon, .. } => icon.as_ref(),
            Art::Error(_) => None,
        }
    }
}

/// One Lua render: the targets it produces output for (a span has several), what they read, and
/// what to draw.
struct Drawn {
    ids: Vec<TargetId>,
    deps: HashSet<String>,
    art: Art,
}

fn fail(info: &CtxInfo, what: &str, e: impl Display) {
    tracing::error!(client = %info.client_id, page = %info.page, key = info.key, slot = ?info.slot, "{what}: {e}");
}

impl Engine {
    fn target(&mut self, id: &TargetId) -> Option<&mut Target> {
        match id {
            TargetId::Key(c, key) => self.clients.get_mut(c).map(|c| &mut c.keys[*key as usize]),
            TargetId::Slot(c, slot) => self
                .plugins
                .get_mut(c)
                .and_then(|p| p.slots.get_mut(slot))
                .map(|s| &mut s.target),
        }
    }

    fn content(&self, id: &TargetId) -> Content {
        match id {
            TargetId::Key(c, key) => match &self.clients[c].cells[*key as usize] {
                Cell::Empty => Content::Empty,
                Cell::Button(b) => Content::Button(b.clone()),
                Cell::Span(sp) => Content::Span(sp.clone()),
            },
            TargetId::Slot(c, slot) => {
                let s = &self.plugins[c].slots[slot];
                match (self.rt.export(&s.export), s.kind) {
                    (Some(Export::Button(b)), SlotKind::Key) => Content::Button(b),
                    (Some(Export::Dial(d)), SlotKind::Dial) => Content::Dial(d),
                    (found, _) => Content::Invalid(match found {
                        None => format!("no export '{}'", s.export),
                        Some(Export::Button(_)) => {
                            format!("export '{}' is a button, not a dial", s.export)
                        }
                        Some(Export::Dial(_)) => {
                            format!("export '{}' is a dial, not a key", s.export)
                        }
                    }),
                }
            }
        }
    }

    /// The `ctx` Lua gets for `id`.
    fn ctx(&self, id: &TargetId, content: &Content) -> CtxInfo {
        let mut info = match id {
            TargetId::Key(c, key) => {
                let c = &self.clients[c];
                CtxInfo {
                    client_id: c.client_id.clone(),
                    key: *key,
                    page: c.page.clone(),
                    rows: c.device.rows,
                    cols: c.device.cols,
                    span: None,
                    span_index: None,
                    slot: None,
                }
            }
            TargetId::Slot(c, slot) => CtxInfo {
                client_id: self.plugins[c].client_id.clone(),
                key: 0,
                page: String::new(),
                rows: 0,
                cols: 0,
                span: None,
                span_index: None,
                slot: Some(slot.clone()),
            },
        };
        if let Content::Span(sp) = content {
            info.span = Some((sp.size.cols, sp.size.rows));
            info.span_index = Some(sp.index);
        }
        info
    }

    /// Re-renders `ids`: Lua runs one target after the other on this thread, then the pictures
    /// are rasterized (a deck's keys in parallel) and whatever changed is sent.
    pub(super) fn render(&mut self, mut ids: Vec<TargetId>) {
        ids.sort_unstable();
        ids.dedup();
        for conn in ids.chunk_by(|a, b| a.conn() == b.conn()) {
            self.render_conn(conn);
        }
    }

    fn render_conn(&mut self, ids: &[TargetId]) {
        let started = Instant::now();
        let mut spans = HashSet::new();
        let drawn: Vec<Drawn> = ids
            .iter()
            .filter_map(|id| self.draw(id, &mut spans))
            .collect();
        let mut shown = self.rasterize(ids[0].conn(), &drawn).into_iter();
        for d in drawn {
            for id in d.ids {
                self.deps.set(id.clone(), d.deps.clone());
                self.show(&id, shown.next().expect("one output per target"));
            }
        }
        tracing::debug!(
            conn = ids[0].conn(),
            targets = ids.len(),
            ms = started.elapsed().as_secs_f64() * 1000.0,
            "rendered"
        );
    }

    /// Runs the Lua render of `id`. A span is drawn once for all its keys (`spans`: the origins
    /// done so far).
    fn draw(&mut self, id: &TargetId, spans: &mut HashSet<u8>) -> Option<Drawn> {
        let content = self.content(id);
        let mut info = self.ctx(id, &content);
        let (ids, deps, art) = match content {
            Content::Empty => (
                vec![id.clone()],
                HashSet::new(),
                Art::Tile(RenderSpec {
                    bg: Color::BLACK,
                    ..RenderSpec::default()
                }),
            ),
            Content::Invalid(msg) => {
                fail(&info, "render", &msg);
                (vec![id.clone()], HashSet::new(), Art::Error(msg))
            }
            Content::Button(btn) => {
                let out = self.rt.render(&btn, &info);
                if let Some(e) = &out.error {
                    fail(&info, "render", e);
                }
                let mut spec = out.spec;
                spec.flash = self
                    .target(id)
                    .expect("rendered target exists")
                    .flash_until
                    .is_some();
                (vec![id.clone()], out.deps, Art::Tile(spec))
            }
            Content::Span(sp) => {
                if !spans.insert(sp.origin) {
                    return None;
                }
                // The origin renders for the whole span.
                info.key = sp.origin;
                info.span_index = None;
                let TargetId::Key(c, _) = id else {
                    unreachable!("only decks have spans")
                };
                let client = &self.clients[c];
                let ids = client
                    .cells
                    .iter()
                    .enumerate()
                    .filter(|(_, cell)| matches!(cell, Cell::Span(o) if o.origin == sp.origin))
                    .map(|(k, _)| TargetId::Key(*c, k as u8))
                    .collect();
                let grid = Grid {
                    cols: sp.size.cols as u32,
                    rows: sp.size.rows as u32,
                    key: client.device.key_size_px,
                };
                if let Some(e) = &sp.error {
                    fail(&info, "render", e);
                    (ids, HashSet::new(), Art::Error(e.clone()))
                } else {
                    let out = self.rt.render(&sp.def, &info);
                    match out.error {
                        Some(e) => {
                            fail(&info, "render", &e);
                            (ids, out.deps, Art::Error(e))
                        }
                        None => (ids, out.deps, Art::Span(out.spec, grid)),
                    }
                }
            }
            Content::Dial(dial) => {
                let out = self.rt.render(&dial, &info);
                if let Some(e) = &out.error {
                    fail(&info, "render", e);
                }
                let icon = out.spec.icon.as_ref().map(|name| RenderSpec {
                    icon: Some(name.clone()),
                    bg: out.spec.bg,
                    ..RenderSpec::default()
                });
                (
                    vec![id.clone()],
                    out.deps,
                    Art::Dial {
                        spec: out.spec,
                        icon,
                    },
                )
            }
        };
        let mut deps = deps;
        if let Some(spec) = art.spec() {
            deps.extend(
                self.renderer
                    .remote_keys(spec)
                    .into_iter()
                    .map(|k| format!("img:{k}")),
            );
        }
        Some(Drawn { ids, deps, art })
    }

    /// Turns what was drawn for the targets of connection `conn` into output, one per target in
    /// order.
    fn rasterize(&mut self, conn: ClientId, drawn: &[Drawn]) -> Vec<Shown> {
        if let Some(c) = self.clients.get(&conn) {
            let size = c.device.key_size_px;
            let jobs = drawn
                .iter()
                .flat_map(|d| match &d.art {
                    Art::Tile(spec) => vec![self.renderer.prepare(spec, Grid::one(size))],
                    Art::Span(spec, grid) => vec![self.renderer.prepare(spec, *grid)],
                    Art::Error(_) => d
                        .ids
                        .iter()
                        .map(|_| self.renderer.prepare(&RenderSpec::error(), Grid::one(size)))
                        .collect(),
                    Art::Dial { .. } => unreachable!("decks have no dials"),
                })
                .collect();
            return encode_all(jobs, Encoding::Jpeg(c.device.native.as_ref()))
                .into_iter()
                .map(Shown::Tile)
                .collect();
        }
        drawn
            .iter()
            .map(|d| {
                let TargetId::Slot(_, slot) = &d.ids[0] else {
                    unreachable!("a connection is a deck or a plugin")
                };
                let size = self.plugins[&conn].slots[slot].size;
                match &d.art {
                    Art::Tile(spec) => Shown::Tile(self.renderer.render(spec, size, Encoding::Png)),
                    Art::Dial { spec, icon } => Shown::Dial {
                        title: spec.title.clone(),
                        value: spec.value.clone(),
                        indicator: spec.indicator,
                        icon_png: icon
                            .as_ref()
                            .map(|s| self.renderer.render(s, DIAL_ICON_PX, Encoding::Png)),
                    },
                    Art::Error(message) => Shown::Error(message.clone()),
                    Art::Span(..) => unreachable!("slots have no spans"),
                }
            })
            .collect()
    }

    /// Sends `shown` unless it is what the target already has.
    fn show(&mut self, id: &TargetId, shown: Shown) {
        let target = self.target(id).expect("rendered target exists");
        if target.last.as_ref() == Some(&shown) {
            return;
        }
        target.last = Some(shown.clone());
        // A closed receiver just means the connection is on its way out.
        match id {
            TargetId::Key(c, key) => {
                let Shown::Tile(jpeg) = shown else {
                    unreachable!("decks only show tiles")
                };
                let _ = self.clients[c]
                    .tx
                    .send(Out::Frame(Frame { key: *key, jpeg }));
            }
            TargetId::Slot(c, slot) => {
                let _ = self.plugins[c].tx.send(shown.into_plugin(slot));
            }
        }
    }

    /// Replaces the output of `id` with the red ERR tile (until its next render).
    fn show_error_tile(&mut self, id: &TargetId) {
        let drawn = [Drawn {
            ids: vec![id.clone()],
            deps: HashSet::new(),
            art: Art::Tile(RenderSpec::error()),
        }];
        let shown = self.rasterize(id.conn(), &drawn).remove(0);
        self.show(id, shown);
    }

    /// Re-renders the targets that read `dep`.
    pub(super) fn dep_changed(&mut self, dep: &str) {
        let ids = self.deps.readers_of(dep);
        self.render(ids);
    }

    /// Runs the press (`down`) or release handler of a key or key slot.
    pub(super) fn press(&mut self, id: &TargetId, down: bool, now: Instant) {
        let content = self.content(id);
        let info = self.ctx(id, &content);
        let (btn, in_span) = match content {
            Content::Button(b) => (b, false),
            Content::Span(sp) if sp.error.is_none() => (sp.def, true),
            _ => return,
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
    pub(super) fn turn(&mut self, id: &TargetId, input: DialInput) {
        let content = self.content(id);
        let info = self.ctx(id, &content);
        let Content::Dial(dial) = content else { return };
        if let Err(e) = self.rt.dial_input(&dial, &info, input) {
            fail(&info, &format!("{input:?}"), format!("{e:#}"));
        }
        // Handlers may change Lua state that `render` reads (a dial's mode, say); unchanged
        // output is not resent.
        self.render(vec![id.clone()]);
        if matches!(input, DialInput::Rotate(_)) {
            // Built-in dials show their own value while being turned; redraw once that is over so
            // the slot shows Home Assistant's state again even if no state change arrives.
            self.redraw_later(id.clone(), DIAL_SETTLE + Duration::from_millis(50));
        }
    }

    /// Shows the press flash on `id` for [`FLASH`].
    fn flash(&mut self, id: &TargetId, now: Instant) {
        if !self.rt.feedback() {
            return;
        }
        self.target(id).expect("pressed target exists").flash_until = Some(now + FLASH);
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
        let Some(target) = self.target(&id) else {
            return;
        };
        if target.flash_until.is_some_and(|until| until <= now) {
            target.flash_until = None;
        }
        self.render(vec![id]);
    }
}
