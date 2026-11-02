//! The Lua-facing API (`sdc`, `ha`) and evaluation of buttons.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use mlua::{serde::SerializeOptions, Function, Lua, LuaSerdeExt, Table, Value};
use tokio::sync::mpsc::UnboundedSender;

use sdc_protocol::{ExportItem, SlotKind, DEVICE_TYPES};

use crate::ha::{EntityState, HaCommand};
use crate::render::{Color, RenderSpec};

pub fn lerr(e: mlua::Error) -> anyhow::Error {
    anyhow!("{e}")
}

/// State shared between the engine and the Lua closures (single-threaded).
#[derive(Default)]
pub struct Host {
    pub states: RefCell<HashMap<String, EntityState>>,
    pub ha_tx: Option<UnboundedSender<HaCommand>>,
    tracking: RefCell<Option<HashSet<String>>>,
    pending_goto: RefCell<Option<String>>,
}

impl Host {
    pub fn new(ha_tx: Option<UnboundedSender<HaCommand>>) -> Rc<Self> {
        Rc::new(Self {
            ha_tx,
            ..Default::default()
        })
    }
}

#[derive(Clone)]
pub struct ButtonDef {
    render: Value,
    press: Option<Function>,
    release: Option<Function>,
    /// `feedback = false` turns the press flash off for this button.
    pub feedback: bool,
    /// Set for `sdc.span{...}`: the button covers a rectangle of keys.
    pub span: Option<SpanDef>,
}

impl ButtonDef {
    /// Whether the button has a press handler (a press without one is not worth a flash).
    pub fn has_press(&self) -> bool {
        self.press.is_some()
    }
}

/// `sdc.dial{...}`: a Stream Deck + dial (plugin only, see [`Export`]).
#[derive(Clone)]
pub struct DialDef {
    render: Value,
    rotate: Option<Function>,
    press: Option<Function>,
    release: Option<Function>,
    touch: Option<Function>,
}

/// What `sdc.export` registered: a button or span (shown as an image) or a dial.
#[derive(Clone)]
pub enum Export {
    Button(ButtonDef),
    Dial(DialDef),
}

/// The input a dial handler is called for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialInput {
    Rotate(i32),
    Press,
    Release,
    Touch,
}

/// What a dial's `render` produces.
#[derive(Debug, Clone, PartialEq)]
pub struct DialSpec {
    pub title: String,
    pub value: String,
    /// 0-100
    pub indicator: u8,
    pub icon: Option<String>,
    /// Behind the icon.
    pub bg: Color,
}

impl Default for DialSpec {
    fn default() -> Self {
        Self {
            title: String::new(),
            value: String::new(),
            indicator: 0,
            icon: None,
            bg: Color::BLACK,
        }
    }
}

pub struct DialOutcome {
    pub spec: DialSpec,
    pub deps: HashSet<String>,
    pub error: Option<String>,
}

/// Size of an `sdc.span`, and its own gap between keys (px at 72 px keys) if it sets one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanDef {
    pub cols: u8,
    pub rows: u8,
    pub gap: Option<u32>,
}

/// What a physical key shows on a page of a given grid size (see [`LuaRuntime::layout`]).
#[derive(Clone)]
pub enum Cell {
    Empty,
    Button(ButtonDef),
    /// One key of a span; `origin` is the span's top-left key.
    Span(SpanCell),
}

#[derive(Clone)]
pub struct SpanCell {
    pub def: ButtonDef,
    pub size: SpanDef,
    pub origin: u8,
    /// Row-major position of this key within the span.
    pub index: u8,
    /// Why the span cannot be shown (off the grid, overlapping); its keys show the ERR tile.
    pub error: Option<String>,
}

/// `sdc.idle{...}`: dim, then switch the display off, after this long without a key press;
/// `home_after` returns the client to the default page. With none set a client never idles.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IdleConfig {
    pub dim_after: Option<Duration>,
    /// Brightness while dimmed (percent).
    pub dim_brightness: u8,
    pub off_after: Option<Duration>,
    /// Back to the default page after this long without a key press (also bounds how long a
    /// disconnected client's page is remembered).
    pub home_after: Option<Duration>,
}

/// Brightness restored on wake when the config has no `sdc.brightness`.
pub const DEFAULT_BRIGHTNESS: u8 = 60;
const DEFAULT_DIM_BRIGHTNESS: u8 = 10;

/// Device-wide settings from the config (`sdc.idle`, `sdc.brightness`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub idle: IdleConfig,
    /// `sdc.brightness`, if set; clients are told about it on connect and on wake.
    pub brightness: Option<u8>,
    /// `sdc.key_gap`: default gap between keys for spans (px at 72 px keys).
    pub key_gap: Option<u32>,
    /// `sdc.feedback`: flash a key after its press handler ran (default true).
    pub feedback: bool,
}

impl Settings {
    /// Brightness of an awake display.
    pub fn active_brightness(&self) -> u8 {
        self.brightness.unwrap_or(DEFAULT_BRIGHTNESS)
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            idle: IdleConfig {
                dim_after: None,
                dim_brightness: DEFAULT_DIM_BRIGHTNESS,
                off_after: None,
                home_after: None,
            },
            brightness: None,
            key_gap: None,
            feedback: true,
        }
    }
}

/// `sdc.device{...}`: which pages a deck shows, where it starts, and its own idle/brightness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDef {
    pub name: String,
    /// Pages this device may show (all exist; validated after the config ran).
    pub pages: Vec<String>,
    /// Start/default page; one of `pages`.
    pub home: String,
    pub brightness: Option<u8>,
    /// Replaces the global `sdc.idle` for this device when set.
    pub idle: Option<IdleConfig>,
}

#[derive(Default)]
struct Registry {
    pages: Vec<(String, Vec<Option<ButtonDef>>)>,
    default: Option<String>,
    devices: Vec<DeviceDef>,
    /// `sdc.default_device`: device type (or `*`) -> device name.
    default_devices: Vec<(String, String)>,
    settings: Settings,
    exports: Vec<(String, Export, Option<String>)>,
}

/// Per-call context handed to Lua as `ctx`.
#[derive(Debug, Clone)]
pub struct CtxInfo {
    pub client_id: String,
    pub key: u8,
    pub page: String,
    pub rows: u8,
    pub cols: u8,
    /// Set when the call is for a span: its size and (for presses) the pressed key's position.
    pub span: Option<(u8, u8)>,
    pub span_index: Option<u8>,
    /// Set when the call is for an Elgato plugin slot: `ctx` then has `slot` (and no `key`,
    /// `page`, `rows`, `cols`).
    pub slot: Option<String>,
}

pub struct RenderOutcome {
    pub spec: RenderSpec,
    /// Entity ids read through `ha.state` during the render.
    pub deps: HashSet<String>,
    pub error: Option<String>,
}

pub struct LuaRuntime {
    lua: Lua,
    host: Rc<Host>,
    registry: Rc<RefCell<Registry>>,
}

impl LuaRuntime {
    /// Loads and executes the config file in a fresh Lua state.
    pub fn load(path: &Path, host: Rc<Host>) -> Result<Self> {
        let src =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let rt = Self::new(host, path.parent())?;
        rt.lua
            .load(&src)
            .set_name(format!("@{}", path.display()))
            .exec()
            .map_err(lerr)?;
        rt.validate_devices()?;
        Ok(rt)
    }

    /// Loads config source from a string (tests).
    pub fn load_str(src: &str, host: Rc<Host>) -> Result<Self> {
        let rt = Self::new(host, None)?;
        rt.lua.load(src).set_name("=config").exec().map_err(lerr)?;
        rt.validate_devices()?;
        Ok(rt)
    }

    fn new(host: Rc<Host>, dir: Option<&Path>) -> Result<Self> {
        let lua = Lua::new();
        let registry = Rc::new(RefCell::new(Registry::default()));
        install(&lua, &host, &registry, dir).map_err(lerr)?;
        Ok(Self {
            lua,
            host,
            registry,
        })
    }

    pub fn default_page(&self) -> Option<String> {
        let r = self.registry.borrow();
        r.default
            .clone()
            .or_else(|| r.pages.first().map(|(n, _)| n.clone()))
    }

    pub fn settings(&self) -> Settings {
        self.registry.borrow().settings
    }

    /// Cross-checks `sdc.device` / `sdc.default_device` against the pages once the whole config
    /// has run (pages may be defined after the devices that use them).
    fn validate_devices(&self) -> Result<()> {
        let r = self.registry.borrow();
        for d in &r.devices {
            for p in &d.pages {
                if !r.pages.iter().any(|(n, _)| n == p) {
                    bail!("sdc.device('{}'): unknown page '{p}' in pages", d.name);
                }
            }
            if !d.pages.contains(&d.home) {
                bail!(
                    "sdc.device('{}'): home '{}' is not in its pages",
                    d.name,
                    d.home
                );
            }
        }
        for (kind, name) in &r.default_devices {
            if !r.devices.iter().any(|d| d.name == *name) {
                bail!("sdc.default_device: '{kind}' names unknown device '{name}'");
            }
        }
        Ok(())
    }

    /// Whether the config defines any `sdc.device` (otherwise everything works as before devices).
    pub fn has_devices(&self) -> bool {
        !self.registry.borrow().devices.is_empty()
    }

    /// The device a connecting client gets: the requested name if it exists, else
    /// `sdc.default_device` for its `kind` then `*`, else the first `sdc.device`. `None` when
    /// the config has no devices (legacy behaviour).
    pub fn resolve_device(&self, requested: Option<&str>, kind: &str) -> Option<DeviceDef> {
        let r = self.registry.borrow();
        let find = |name: &str| r.devices.iter().find(|d| d.name == name);
        if let Some(name) = requested {
            match find(name) {
                Some(d) => return Some(d.clone()),
                None => tracing::warn!("requested device '{name}' is not defined, using default"),
            }
        }
        [kind, "*"]
            .iter()
            .find_map(|k| {
                r.default_devices
                    .iter()
                    .find(|(t, _)| t == k)
                    .and_then(|(_, n)| find(n))
            })
            .or_else(|| r.devices.first())
            .cloned()
    }

    /// A defined device by name.
    pub fn device(&self, name: &str) -> Option<DeviceDef> {
        self.registry
            .borrow()
            .devices
            .iter()
            .find(|d| d.name == name)
            .cloned()
    }

    /// All defined devices, in definition order.
    pub fn devices(&self) -> Vec<DeviceDef> {
        self.registry.borrow().devices.clone()
    }

    pub fn has_page(&self, name: &str) -> bool {
        self.registry.borrow().pages.iter().any(|(n, _)| n == name)
    }

    pub fn page_names(&self) -> Vec<String> {
        self.registry
            .borrow()
            .pages
            .iter()
            .map(|(n, _)| n.clone())
            .collect()
    }

    pub fn button(&self, page: &str, key: u8) -> Option<ButtonDef> {
        let r = self.registry.borrow();
        let (_, keys) = r.pages.iter().find(|(n, _)| n == page)?;
        keys.get(key as usize)?.clone()
    }

    /// What every key of `page` shows on a `rows` x `cols` grid. Spans are placed per grid
    /// because the page list is row-major: a span at slot k covers k..k+cols-1 and the same
    /// columns below, and those other slots must be empty.
    pub fn layout(&self, page: &str, rows: u8, cols: u8) -> Vec<Cell> {
        let r = self.registry.borrow();
        let keys = r
            .pages
            .iter()
            .find(|(n, _)| n == page)
            .map(|(_, k)| k.as_slice())
            .unwrap_or_default();
        let count = rows as usize * cols as usize;
        let mut cells: Vec<Option<Cell>> = vec![None; count];
        for (k, def) in keys.iter().enumerate().take(count) {
            let Some(def) = def else { continue };
            let Some(size) = def.span else {
                if cells[k].is_none() {
                    cells[k] = Some(Cell::Button(def.clone()));
                }
                continue;
            };
            let (row, col) = (k / cols as usize, k % cols as usize);
            let mut error = None;
            if col + size.cols as usize > cols as usize || row + size.rows as usize > rows as usize
            {
                error = Some(format!(
                    "span {}x{} at key {k} does not fit the {rows}x{cols} grid",
                    size.cols, size.rows
                ));
            }
            let mut members = Vec::new();
            for dr in 0..size.rows as usize {
                for dc in 0..size.cols as usize {
                    let m = (row + dr) * cols as usize + col + dc;
                    if row + dr >= rows as usize || col + dc >= cols as usize {
                        continue;
                    }
                    if m != k && (keys.get(m).is_some_and(Option::is_some) || cells[m].is_some()) {
                        error.get_or_insert_with(|| {
                            format!("span at key {k} overlaps slot {m}, which must be false/nil")
                        });
                    }
                    members.push((dr * size.cols as usize + dc, m));
                }
            }
            for (index, m) in members {
                if m == k || cells[m].is_none() {
                    cells[m] = Some(Cell::Span(SpanCell {
                        def: def.clone(),
                        size,
                        origin: k as u8,
                        index: index as u8,
                        error: error.clone(),
                    }));
                }
            }
        }
        cells
            .into_iter()
            .map(|c| c.unwrap_or(Cell::Empty))
            .collect()
    }

    /// `icon`/`image` names used by buttons whose `render` is a static table or string (the
    /// ones that can be fetched ahead of time).
    pub fn static_image_names(&self) -> Vec<String> {
        let r = self.registry.borrow();
        let mut names = Vec::new();
        for (_, keys) in &r.pages {
            for btn in keys.iter().flatten() {
                if let Ok(spec) = parse_spec(&btn.render) {
                    names.extend(spec.icon.into_iter().chain(spec.image));
                }
            }
        }
        names.sort();
        names.dedup();
        names
    }

    /// Runs a button's render, recording the entities it reads. Never fails: errors
    /// are reported in the outcome (and the spec is the error tile).
    pub fn render(&self, btn: &ButtonDef, info: &CtxInfo) -> RenderOutcome {
        *self.host.tracking.borrow_mut() = Some(HashSet::new());
        let res = self.eval_render(btn, info);
        let deps = self.host.tracking.borrow_mut().take().unwrap_or_default();
        match res {
            Ok(spec) => RenderOutcome {
                spec,
                deps,
                error: None,
            },
            Err(e) => RenderOutcome {
                spec: RenderSpec::error(),
                deps,
                error: Some(e.to_string()),
            },
        }
    }

    fn eval_render(&self, btn: &ButtonDef, info: &CtxInfo) -> mlua::Result<RenderSpec> {
        let v = match &btn.render {
            Value::Function(f) => f.call::<Value>(self.make_ctx(info)?)?,
            other => other.clone(),
        };
        parse_spec(&v)
    }

    /// Runs the press (`down`) or release handler. Returns a requested page switch.
    pub fn press(&self, btn: &ButtonDef, info: &CtxInfo, down: bool) -> Result<Option<String>> {
        let Some(f) = (if down { &btn.press } else { &btn.release }) else {
            return Ok(None);
        };
        *self.host.pending_goto.borrow_mut() = None;
        let ctx = self.make_ctx(info).map_err(lerr)?;
        f.call::<()>(ctx).map_err(lerr)?;
        Ok(self.host.pending_goto.borrow_mut().take())
    }

    /// The exports plugins can see, in definition order.
    pub fn exports(&self) -> Vec<ExportItem> {
        self.registry
            .borrow()
            .exports
            .iter()
            .map(|(id, e, label)| ExportItem {
                id: id.clone(),
                kind: match e {
                    Export::Button(_) => SlotKind::Key,
                    Export::Dial(_) => SlotKind::Dial,
                },
                label: label.clone(),
            })
            .collect()
    }

    pub fn export(&self, id: &str) -> Option<Export> {
        let r = self.registry.borrow();
        r.exports
            .iter()
            .find(|(i, ..)| i == id)
            .map(|(_, e, _)| e.clone())
    }

    /// Runs a dial's render, recording the entities it reads (like [`LuaRuntime::render`]).
    pub fn render_dial(&self, dial: &DialDef, info: &CtxInfo) -> DialOutcome {
        *self.host.tracking.borrow_mut() = Some(HashSet::new());
        let res = (|| {
            let v = match &dial.render {
                Value::Function(f) => f.call::<Value>(self.make_ctx(info)?)?,
                other => other.clone(),
            };
            parse_dial_spec(&v)
        })();
        let deps = self.host.tracking.borrow_mut().take().unwrap_or_default();
        match res {
            Ok(spec) => DialOutcome {
                spec,
                deps,
                error: None,
            },
            Err(e) => DialOutcome {
                spec: DialSpec {
                    title: "ERR".into(),
                    bg: crate::render::ERR_BG,
                    ..DialSpec::default()
                },
                deps,
                error: Some(e.to_string()),
            },
        }
    }

    /// Runs the dial handler for `input` (none set = nothing happens). A `goto_page` is dropped:
    /// plugins have no pages.
    pub fn dial_input(&self, dial: &DialDef, info: &CtxInfo, input: DialInput) -> Result<()> {
        let f = match input {
            DialInput::Rotate(_) => &dial.rotate,
            DialInput::Press => &dial.press,
            DialInput::Release => &dial.release,
            DialInput::Touch => &dial.touch,
        };
        let Some(f) = f else { return Ok(()) };
        *self.host.pending_goto.borrow_mut() = None;
        let ctx = self.make_ctx(info).map_err(lerr)?;
        match input {
            DialInput::Rotate(ticks) => f.call::<()>((ctx, ticks)),
            _ => f.call::<()>(ctx),
        }
        .map_err(lerr)?;
        if let Some(page) = self.host.pending_goto.borrow_mut().take() {
            tracing::debug!("goto_page({page}) from a plugin dial ignored");
        }
        Ok(())
    }

    fn make_ctx(&self, info: &CtxInfo) -> mlua::Result<Table> {
        let t = self.lua.create_table()?;
        t.set("client_id", info.client_id.as_str())?;
        if let Some(slot) = &info.slot {
            t.set("slot", slot.as_str())?;
        } else {
            t.set("key", info.key)?;
            t.set("page", info.page.as_str())?;
            t.set("rows", info.rows)?;
            t.set("cols", info.cols)?;
        }
        if let Some((cols, rows)) = info.span {
            t.set("span_cols", cols)?;
            t.set("span_rows", rows)?;
        }
        if let Some(i) = info.span_index {
            t.set("span_index", i)?;
        }
        let host = self.host.clone();
        t.set(
            "goto_page",
            self.lua.create_function(move |_, name: String| {
                *host.pending_goto.borrow_mut() = Some(name);
                Ok(())
            })?,
        )?;
        Ok(t)
    }
}

fn install(
    lua: &Lua,
    host: &Rc<Host>,
    registry: &Rc<RefCell<Registry>>,
    dir: Option<&Path>,
) -> mlua::Result<()> {
    let globals = lua.globals();
    let sdc = lua.create_table()?;

    sdc.set(
        "button",
        lua.create_function(|_, t: Table| {
            check_button_table(&t, "sdc.button")?;
            t.set("__sdc", "button")?;
            Ok(t)
        })?,
    )?;

    sdc.set(
        "span",
        lua.create_function(|_, t: Table| {
            check_button_table(&t, "sdc.span")?;
            for field in ["cols", "rows"] {
                let v: Value = t.get(field)?;
                if !matches!(v, Value::Integer(1..=32)) {
                    return Err(mlua::Error::runtime(format!(
                        "sdc.span: '{field}' must be an integer 1-32"
                    )));
                }
            }
            let gap: Value = t.get("gap")?;
            if !matches!(gap, Value::Nil | Value::Integer(0..=72)) {
                return Err(mlua::Error::runtime(
                    "sdc.span: 'gap' must be an integer 0-72 (px at 72 px keys)",
                ));
            }
            t.set("__sdc", "span")?;
            Ok(t)
        })?,
    )?;

    sdc.set(
        "dial",
        lua.create_function(|_, t: Table| {
            for field in ["rotate", "press", "release", "touch"] {
                let v: Value = t.get(field)?;
                if !matches!(v, Value::Nil | Value::Function(_)) {
                    return Err(mlua::Error::runtime(format!(
                        "sdc.dial: '{field}' must be a function"
                    )));
                }
            }
            let r: Value = t.get("render")?;
            if !matches!(r, Value::Function(_) | Value::Table(_)) {
                return Err(mlua::Error::runtime(
                    "sdc.dial: 'render' (a function or a table) is required",
                ));
            }
            t.set("__sdc", "dial")?;
            Ok(t)
        })?,
    )?;

    let reg = registry.clone();
    sdc.set(
        "export",
        lua.create_function(move |_, (id, item): (String, Table)| {
            if id.is_empty()
                || id.len() > 64
                || !id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
            {
                return Err(mlua::Error::runtime(format!(
                    "sdc.export: invalid id {id:?} (1-64 characters of A-Z a-z 0-9 _ - .)"
                )));
            }
            let export = match item.get::<Option<String>>("__sdc")?.as_deref() {
                Some("button" | "span") => Export::Button(button_def(&item)?),
                Some("dial") => Export::Dial(DialDef {
                    render: item.get("render")?,
                    rotate: item.get("rotate")?,
                    press: item.get("press")?,
                    release: item.get("release")?,
                    touch: item.get("touch")?,
                }),
                _ => {
                    return Err(mlua::Error::runtime(format!(
                        "sdc.export('{id}'): second argument is not an sdc.button, sdc.span or sdc.dial"
                    )))
                }
            };
            let label = match item.get::<Value>("label")? {
                Value::String(s) => Some(s.to_str()?.to_string()),
                _ => None,
            };
            let mut r = reg.borrow_mut();
            if r.exports.iter().any(|(i, ..)| *i == id) {
                return Err(mlua::Error::runtime(format!(
                    "sdc.export: duplicate id '{id}'"
                )));
            }
            r.exports.push((id, export, label));
            Ok(item)
        })?,
    )?;

    let reg = registry.clone();
    sdc.set(
        "feedback",
        lua.create_function(move |_, on: bool| {
            reg.borrow_mut().settings.feedback = on;
            Ok(())
        })?,
    )?;

    let reg = registry.clone();
    sdc.set(
        "key_gap",
        lua.create_function(move |_, px: i64| {
            let px = u32::try_from(px).ok().filter(|p| *p <= 72).ok_or_else(|| {
                mlua::Error::runtime(format!("sdc.key_gap: {px} is not 0-72 (px at 72 px keys)"))
            })?;
            reg.borrow_mut().settings.key_gap = Some(px);
            Ok(())
        })?,
    )?;

    let reg = registry.clone();
    sdc.set(
        "page",
        lua.create_function(move |_, (name, items): (String, Table)| {
            let mut keys: Vec<Option<ButtonDef>> = Vec::new();
            for pair in items.pairs::<Value, Value>() {
                let (k, v) = pair?;
                let idx = match k {
                    Value::Integer(i) if i >= 1 => i as usize - 1,
                    _ => return Err(mlua::Error::runtime(format!("page '{name}': keys must be positive integers"))),
                };
                let def = match v {
                    Value::Nil | Value::Boolean(false) => None,
                    Value::Table(t) if matches!(t.get::<Option<String>>("__sdc")?.as_deref(), Some("button" | "span")) => Some(button_def(&t)?),
                    Value::Table(t) if t.get::<Option<String>>("__sdc")?.as_deref() == Some("dial") => {
                        return Err(mlua::Error::runtime(format!(
                            "page '{name}': item {} is an sdc.dial, which only works as an export for the Elgato plugin (sdc.export)",
                            idx + 1
                        )))
                    }
                    _ => {
                        return Err(mlua::Error::runtime(format!(
                            "page '{name}': item {} is not an sdc.button(...) or sdc.span(...) (use false for an empty key)",
                            idx + 1
                        )))
                    }
                };
                if keys.len() <= idx {
                    keys.resize(idx + 1, None);
                }
                keys[idx] = def;
            }
            let mut r = reg.borrow_mut();
            match r.pages.iter_mut().find(|(n, _)| *n == name) {
                Some(slot) => slot.1 = keys,
                None => r.pages.push((name, keys)),
            }
            Ok(())
        })?,
    )?;

    let reg = registry.clone();
    sdc.set(
        "default_page",
        lua.create_function(move |_, name: String| {
            reg.borrow_mut().default = Some(name);
            Ok(())
        })?,
    )?;

    let reg = registry.clone();
    sdc.set(
        "brightness",
        lua.create_function(move |_, percent: i64| {
            reg.borrow_mut().settings.brightness = Some(percent_arg("sdc.brightness", percent)?);
            Ok(())
        })?,
    )?;

    let reg = registry.clone();
    sdc.set(
        "idle",
        lua.create_function(move |_, t: Table| {
            reg.borrow_mut().settings.idle = idle_arg("sdc.idle", &t)?;
            Ok(())
        })?,
    )?;

    let reg = registry.clone();
    sdc.set(
        "device",
        lua.create_function(move |_, (name, t): (String, Table)| {
            let what = format!("sdc.device('{name}')");
            if name.is_empty() {
                return Err(mlua::Error::runtime("sdc.device: name must not be empty"));
            }
            if reg.borrow().devices.iter().any(|d| d.name == name) {
                return Err(mlua::Error::runtime(format!(
                    "sdc.device: duplicate device '{name}'"
                )));
            }
            let mut pages: Vec<String> = Vec::new();
            let mut home = None;
            let mut brightness = None;
            let mut idle = None;
            for pair in t.pairs::<String, Value>() {
                let (k, v) = pair?;
                match (k.as_str(), v) {
                    ("pages", Value::Table(list)) => {
                        for p in list.sequence_values::<String>() {
                            let p = p.map_err(|_| {
                                mlua::Error::runtime(format!("{what}: pages must be page names"))
                            })?;
                            if !pages.contains(&p) {
                                pages.push(p);
                            }
                        }
                    }
                    ("home", Value::String(h)) => home = Some(h.to_str()?.to_string()),
                    ("brightness", Value::Integer(i)) => {
                        brightness = Some(percent_arg(&format!("{what}: 'brightness'"), i)?)
                    }
                    ("idle", Value::Table(i)) => idle = Some(idle_arg(&what, &i)?),
                    ("pages", _) => {
                        return Err(mlua::Error::runtime(format!(
                            "{what}: 'pages' must be a list of page names"
                        )))
                    }
                    ("home", _) => {
                        return Err(mlua::Error::runtime(format!(
                            "{what}: 'home' must be a page name"
                        )))
                    }
                    ("brightness", _) => {
                        return Err(mlua::Error::runtime(format!(
                            "{what}: 'brightness' must be an integer 0-100"
                        )))
                    }
                    ("idle", _) => {
                        return Err(mlua::Error::runtime(format!(
                            "{what}: 'idle' must be a table like sdc.idle's"
                        )))
                    }
                    _ => {
                        return Err(mlua::Error::runtime(format!(
                            "{what}: unknown field '{k}' (use pages, home, brightness, idle)"
                        )))
                    }
                }
            }
            if pages.is_empty() {
                return Err(mlua::Error::runtime(format!(
                    "{what}: 'pages' must list at least one page"
                )));
            }
            let home = home.unwrap_or_else(|| pages[0].clone());
            reg.borrow_mut().devices.push(DeviceDef {
                name,
                pages,
                home,
                brightness,
                idle,
            });
            Ok(())
        })?,
    )?;

    let reg = registry.clone();
    sdc.set(
        "default_device",
        lua.create_function(move |_, t: Table| {
            let mut map = Vec::new();
            for pair in t.pairs::<Value, Value>() {
                let (k, v) = pair?;
                let (Value::String(k), Value::String(v)) = (&k, &v) else {
                    return Err(mlua::Error::runtime(
                        "sdc.default_device: expected { <device type or \"*\"> = \"<device name>\" }",
                    ));
                };
                let (k, v) = (k.to_str()?.to_string(), v.to_str()?.to_string());
                if k != "*" && !DEVICE_TYPES.contains(&k.as_str()) {
                    return Err(mlua::Error::runtime(format!(
                        "sdc.default_device: unknown device type '{k}' (use {} or \"*\")",
                        DEVICE_TYPES.join(", ")
                    )));
                }
                map.push((k, v));
            }
            reg.borrow_mut().default_devices = map;
            Ok(())
        })?,
    )?;

    // ha
    let ha = lua.create_table()?;
    let h = host.clone();
    ha.set(
        "state",
        lua.create_function(move |lua, id: String| {
            if let Some(t) = h.tracking.borrow_mut().as_mut() {
                t.insert(id.clone());
            }
            let states = h.states.borrow();
            let Some(s) = states.get(&id) else {
                return Ok(Value::Nil);
            };
            let opts = SerializeOptions::new()
                .serialize_none_to_null(false)
                .serialize_unit_to_null(false);
            let t = lua.create_table()?;
            t.set("entity_id", s.entity_id.as_str())?;
            t.set("state", s.state.as_str())?;
            t.set("attributes", lua.to_value_with(&s.attributes, opts)?)?;
            Ok(Value::Table(t))
        })?,
    )?;
    let h = host.clone();
    ha.set(
        "call",
        lua.create_function(
            move |lua, (domain, service, data): (String, String, Option<Value>)| {
                let data: serde_json::Value = match data {
                    Some(v) => lua.from_value(v)?,
                    None => serde_json::json!({}),
                };
                match &h.ha_tx {
                    Some(tx) => {
                        let _ = tx.send(HaCommand::CallService {
                            domain,
                            service,
                            data,
                        });
                    }
                    None => tracing::warn!(
                        "ha.call({domain}.{service}) ignored: home assistant not configured"
                    ),
                }
                Ok(())
            },
        )?,
    )?;
    sdc.set("ha", ha.clone())?;
    globals.set("ha", ha)?;
    globals.set("sdc", sdc.clone())?;

    // require("sdc") plus relative requires from the config directory
    let package: Table = globals.get("package")?;
    let loaded: Table = package.get("loaded")?;
    loaded.set("sdc", sdc)?;
    if let Some(dir) = dir {
        let d = dir.display();
        let old: String = package.get("path")?;
        package.set("path", format!("{d}/?.lua;{d}/?/init.lua;{old}"))?;
    }
    Ok(())
}

/// The definition behind a table made by `sdc.button` or `sdc.span`.
fn button_def(t: &Table) -> mlua::Result<ButtonDef> {
    let is_span = t.get::<String>("__sdc")? == "span";
    Ok(ButtonDef {
        render: t.get("render")?,
        press: t.get("press")?,
        release: t.get("release")?,
        feedback: t.get::<Option<bool>>("feedback")?.unwrap_or(true),
        span: is_span
            .then(|| -> mlua::Result<SpanDef> {
                Ok(SpanDef {
                    cols: t.get("cols")?,
                    rows: t.get("rows")?,
                    gap: t.get("gap")?,
                })
            })
            .transpose()?,
    })
}

fn parse_dial_spec(v: &Value) -> mlua::Result<DialSpec> {
    let mut spec = DialSpec::default();
    let Value::Table(t) = v else {
        return Err(mlua::Error::runtime(format!(
            "dial render must return a table, got {}",
            v.type_name()
        )));
    };
    let text = |v: Value, what: &str| -> mlua::Result<String> {
        match v {
            Value::Nil => Ok(String::new()),
            Value::String(s) => Ok(s.to_str()?.to_string()),
            Value::Integer(i) => Ok(i.to_string()),
            Value::Number(n) => Ok(n.to_string()),
            other => Err(mlua::Error::runtime(format!(
                "'{what}' must be a string, got {}",
                other.type_name()
            ))),
        }
    };
    spec.title = text(t.get("title")?, "title")?;
    spec.value = text(t.get("value")?, "value")?;
    spec.indicator = match t.get::<Value>("indicator")? {
        Value::Nil => 0,
        Value::Integer(i) => i.clamp(0, 100) as u8,
        Value::Number(n) if n.is_finite() => n.round().clamp(0.0, 100.0) as u8,
        other => {
            return Err(mlua::Error::runtime(format!(
                "'indicator' must be a number 0-100, got {}",
                other.type_name()
            )))
        }
    };
    spec.icon = t.get("icon")?;
    if let Some(s) = t.get::<Option<String>>("bg")? {
        spec.bg = Color::parse(&s).ok_or_else(|| {
            mlua::Error::runtime(format!("'bg': invalid color '{s}' (use #rrggbb)"))
        })?;
    }
    Ok(spec)
}

fn percent_arg(what: &str, v: i64) -> mlua::Result<u8> {
    u8::try_from(v)
        .ok()
        .filter(|p| *p <= 100)
        .ok_or_else(|| mlua::Error::runtime(format!("{what}: {v} is not a percentage (0-100)")))
}

/// Parses the table of `sdc.idle` (and a device's `idle`).
fn idle_arg(what: &str, t: &Table) -> mlua::Result<IdleConfig> {
    let mut idle = IdleConfig {
        dim_brightness: DEFAULT_DIM_BRIGHTNESS,
        ..Default::default()
    };
    for pair in t.pairs::<String, Value>() {
        let (k, v) = pair?;
        match k.as_str() {
            "dim_after" => idle.dim_after = Some(seconds_arg(what, &k, &v)?),
            "off_after" => idle.off_after = Some(seconds_arg(what, &k, &v)?),
            "home_after" => idle.home_after = Some(seconds_arg(what, &k, &v)?),
            "dim_brightness" => match v {
                Value::Integer(i) => {
                    idle.dim_brightness = percent_arg(&format!("{what}: 'dim_brightness'"), i)?
                }
                _ => {
                    return Err(mlua::Error::runtime(format!(
                        "{what}: 'dim_brightness' must be an integer 0-100"
                    )))
                }
            },
            _ => {
                return Err(mlua::Error::runtime(format!(
                    "{what}: unknown field '{k}' (use dim_after, dim_brightness, off_after, home_after)"
                )))
            }
        }
    }
    Ok(idle)
}

fn seconds_arg(what: &str, field: &str, v: &Value) -> mlua::Result<Duration> {
    let secs = match v {
        Value::Integer(i) => *i as f64,
        Value::Number(n) => *n,
        _ => 0.0,
    };
    if secs.is_finite() && secs > 0.0 {
        Ok(Duration::from_secs_f64(secs))
    } else {
        Err(mlua::Error::runtime(format!(
            "{what}: '{field}' must be a number of seconds > 0"
        )))
    }
}

fn check_button_table(t: &Table, what: &str) -> mlua::Result<()> {
    for field in ["press", "release"] {
        let v: Value = t.get(field)?;
        if !matches!(v, Value::Nil | Value::Function(_)) {
            return Err(mlua::Error::runtime(format!(
                "{what}: '{field}' must be a function"
            )));
        }
    }
    if !matches!(t.get::<Value>("feedback")?, Value::Nil | Value::Boolean(_)) {
        return Err(mlua::Error::runtime(format!(
            "{what}: 'feedback' must be true or false"
        )));
    }
    let r: Value = t.get("render")?;
    if !matches!(
        r,
        Value::Nil | Value::Function(_) | Value::Table(_) | Value::String(_)
    ) {
        return Err(mlua::Error::runtime(format!(
            "{what}: 'render' must be a function or a table"
        )));
    }
    Ok(())
}

fn parse_spec(v: &Value) -> mlua::Result<RenderSpec> {
    let mut spec = RenderSpec::default();
    let t = match v {
        Value::Nil | Value::Boolean(false) => return Ok(spec),
        Value::String(s) => {
            spec.text = Some(s.to_str()?.to_string());
            return Ok(spec);
        }
        Value::Table(t) => t,
        other => {
            return Err(mlua::Error::runtime(format!(
                "render must return a table, string or nil, got {}",
                other.type_name()
            )))
        }
    };
    spec.text = match t.get::<Value>("text")? {
        Value::Nil => None,
        Value::String(s) => Some(s.to_str()?.to_string()),
        Value::Integer(i) => Some(i.to_string()),
        Value::Number(n) => Some(n.to_string()),
        other => {
            return Err(mlua::Error::runtime(format!(
                "'text' must be a string, got {}",
                other.type_name()
            )))
        }
    };
    for (field, slot) in [("bg", &mut spec.bg), ("fg", &mut spec.fg)] {
        if let Some(s) = t.get::<Option<String>>(field)? {
            *slot = Color::parse(&s).ok_or_else(|| {
                mlua::Error::runtime(format!("'{field}': invalid color '{s}' (use #rrggbb)"))
            })?;
        }
    }
    spec.icon = t.get("icon")?;
    spec.image = t.get("image")?;
    spec.font_size = t.get("font_size")?;
    spec.chevron = t.get::<Option<bool>>("chevron")?.unwrap_or(false);
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn info(page: &str, key: u8) -> CtxInfo {
        CtxInfo {
            client_id: "c".into(),
            key,
            page: page.into(),
            rows: 3,
            cols: 5,
            span: None,
            span_index: None,
            slot: None,
        }
    }

    fn state(id: &str, s: &str) -> EntityState {
        EntityState {
            entity_id: id.into(),
            state: s.into(),
            attributes: json!({"brightness": 128}),
        }
    }

    #[test]
    fn pages_and_defaults() {
        let rt = LuaRuntime::load_str(
            r#"
            local sdc = require("sdc")
            sdc.page("a", { sdc.button{ render = { text = "A" } }, false, sdc.button{ render = function() return { text = "C" } end } })
            sdc.page("b", { sdc.button{ render = { text = "B" } } })
            "#,
            Host::new(None),
        )
        .unwrap();
        assert_eq!(rt.default_page().as_deref(), Some("a"));
        assert_eq!(rt.page_names(), ["a", "b"]);
        assert!(rt.button("a", 0).is_some());
        assert!(rt.button("a", 1).is_none());
        assert!(rt.button("a", 2).is_some());
        assert!(rt.button("a", 3).is_none());
        let rt2 = LuaRuntime::load_str(
            r#"sdc.page("x", {}) sdc.page("y", {}) sdc.default_page("y")"#,
            Host::new(None),
        )
        .unwrap();
        assert_eq!(rt2.default_page().as_deref(), Some("y"));
    }

    #[test]
    fn static_and_function_render() {
        let rt = LuaRuntime::load_str(
            r##"
            sdc.page("p", {
              sdc.button{ render = { text = "Hi", bg = "#112233", font_size = 20 } },
              sdc.button{ render = function(ctx) return { text = ctx.page .. ctx.key, fg = "#ff0000" } end },
              sdc.button{ render = "plain" },
            })
            "##,
            Host::new(None),
        )
        .unwrap();
        let a = rt.render(&rt.button("p", 0).unwrap(), &info("p", 0));
        assert_eq!(a.spec.text.as_deref(), Some("Hi"));
        assert_eq!(a.spec.bg, Color([0x11, 0x22, 0x33]));
        assert_eq!(a.spec.font_size, Some(20.0));
        assert!(a.error.is_none());
        let b = rt.render(&rt.button("p", 1).unwrap(), &info("p", 1));
        assert_eq!(b.spec.text.as_deref(), Some("p1"));
        assert_eq!(b.spec.fg, Color([255, 0, 0]));
        let c = rt.render(&rt.button("p", 2).unwrap(), &info("p", 2));
        assert_eq!(c.spec.text.as_deref(), Some("plain"));
    }

    #[test]
    fn goto_page_from_press() {
        let rt = LuaRuntime::load_str(
            r#"sdc.page("p", { sdc.button{ press = function(ctx) ctx.goto_page("other") end },
                               sdc.button{ press = function() end } })"#,
            Host::new(None),
        )
        .unwrap();
        let goto = rt
            .press(&rt.button("p", 0).unwrap(), &info("p", 0), true)
            .unwrap();
        assert_eq!(goto.as_deref(), Some("other"));
        assert_eq!(
            rt.press(&rt.button("p", 1).unwrap(), &info("p", 1), true)
                .unwrap(),
            None
        );
        // no release handler
        assert_eq!(
            rt.press(&rt.button("p", 0).unwrap(), &info("p", 0), false)
                .unwrap(),
            None
        );
    }

    #[test]
    fn ha_state_tracking_and_attributes() {
        let host = Host::new(None);
        host.states
            .borrow_mut()
            .insert("light.a".into(), state("light.a", "on"));
        let rt = LuaRuntime::load_str(
            r#"sdc.page("p", { sdc.button{ render = function()
                 local l = ha.state("light.a")
                 local m = ha.state("light.missing")
                 return { text = l.state .. ":" .. l.attributes.brightness .. ":" .. tostring(m) }
               end } })"#,
            host,
        )
        .unwrap();
        let o = rt.render(&rt.button("p", 0).unwrap(), &info("p", 0));
        assert_eq!(o.spec.text.as_deref(), Some("on:128:nil"));
        assert_eq!(
            o.deps,
            HashSet::from(["light.a".to_string(), "light.missing".to_string()])
        );
    }

    #[test]
    fn lua_error_gives_error_tile() {
        let rt = LuaRuntime::load_str(
            r#"sdc.page("p", { sdc.button{ render = function() error("boom") end, press = function() error("bang") end } })"#,
            Host::new(None),
        )
        .unwrap();
        let b = rt.button("p", 0).unwrap();
        let o = rt.render(&b, &info("p", 0));
        assert_eq!(o.spec, RenderSpec::error());
        assert!(o.error.unwrap().contains("boom"));
        assert!(rt.press(&b, &info("p", 0), true).is_err());
    }

    #[test]
    fn ha_call_sends_command() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let rt = LuaRuntime::load_str(
            r#"sdc.page("p", { sdc.button{ press = function() ha.call("light", "toggle", { entity_id = "light.a" }) end } })"#,
            Host::new(Some(tx)),
        )
        .unwrap();
        rt.press(&rt.button("p", 0).unwrap(), &info("p", 0), true)
            .unwrap();
        assert_eq!(
            rx.try_recv().unwrap(),
            HaCommand::CallService {
                domain: "light".into(),
                service: "toggle".into(),
                data: json!({"entity_id": "light.a"})
            }
        );
    }

    #[test]
    fn bad_page_item_fails_load() {
        assert!(LuaRuntime::load_str(r#"sdc.page("p", { 42 })"#, Host::new(None)).is_err());
        assert!(LuaRuntime::load_str(r#"sdc.button{ press = 1 }"#, Host::new(None)).is_err());
    }

    #[test]
    fn idle_and_brightness_settings() {
        let rt = LuaRuntime::load_str("", Host::new(None)).unwrap();
        assert_eq!(rt.settings(), Settings::default());
        assert_eq!(rt.settings().active_brightness(), DEFAULT_BRIGHTNESS);
        let rt = LuaRuntime::load_str(
            "sdc.idle { dim_after = 120, off_after = 600.5, home_after = 3600 } sdc.brightness(40)",
            Host::new(None),
        )
        .unwrap();
        let s = rt.settings();
        assert_eq!(s.idle.dim_after, Some(Duration::from_secs(120)));
        assert_eq!(s.idle.off_after, Some(Duration::from_millis(600_500)));
        assert_eq!(s.idle.home_after, Some(Duration::from_secs(3600)));
        assert_eq!(s.idle.dim_brightness, 10);
        assert_eq!(s.active_brightness(), 40);
        for bad in [
            "sdc.brightness(101)",
            "sdc.idle { dim_after = 0 }",
            "sdc.idle { dim_brightness = 200 }",
            "sdc.idle { dim_aftr = 5 }",
            "sdc.idle { home_after = -1 }",
        ] {
            assert!(LuaRuntime::load_str(bad, Host::new(None)).is_err(), "{bad}");
        }
    }

    #[test]
    fn devices_parse_and_resolve() {
        let rt = LuaRuntime::load_str(
            r#"
            sdc.device("a", { pages = { "p", "q", "p" }, home = "q", brightness = 40,
                              idle = { dim_after = 5 } })
            sdc.device("b", { pages = { "q" } })
            sdc.default_device { mk2 = "b", ["*"] = "a" }
            sdc.page("p", {}) sdc.page("q", {})
            "#,
            Host::new(None),
        )
        .unwrap();
        let a = rt.device("a").unwrap();
        assert_eq!(a.pages, ["p", "q"]);
        assert_eq!((a.home.as_str(), a.brightness), ("q", Some(40)));
        assert_eq!(a.idle.unwrap().dim_after, Some(Duration::from_secs(5)));
        let b = rt.device("b").unwrap();
        assert_eq!(
            (b.home.as_str(), b.idle),
            ("q", None),
            "home defaults to the first page"
        );
        let name = |req, kind| rt.resolve_device(req, kind).map(|d| d.name);
        assert_eq!(name(Some("a"), "mk2").as_deref(), Some("a"));
        assert_eq!(name(None, "mk2").as_deref(), Some("b"));
        assert_eq!(name(None, "xl").as_deref(), Some("a"));
        assert_eq!(name(Some("zzz"), "mk2").as_deref(), Some("b"));
        let rt = LuaRuntime::load_str("sdc.page('p', {})", Host::new(None)).unwrap();
        assert!(!rt.has_devices() && rt.resolve_device(Some("a"), "mk2").is_none());
    }

    #[test]
    fn device_config_errors() {
        let err = |src: &str| {
            LuaRuntime::load_str(src, Host::new(None))
                .err()
                .unwrap_or_else(|| panic!("accepted: {src}"))
                .to_string()
        };
        let pages = r#"sdc.page("p", {}) sdc.page("q", {}) "#;
        let cases = [
            (
                r#"sdc.device("a", { pages = { "nope" } })"#,
                "unknown page 'nope'",
            ),
            (
                r#"sdc.device("a", { pages = { "p" }, home = "q" })"#,
                "home 'q' is not in its pages",
            ),
            (
                r#"sdc.device("a", { pages = { "p" } }) sdc.device("a", { pages = { "q" } })"#,
                "duplicate device 'a'",
            ),
            (r#"sdc.device("a", { pages = {} })"#, "at least one page"),
            (
                r#"sdc.device("a", { pages = { "p" }, brightnes = 5 })"#,
                "unknown field 'brightnes'",
            ),
            (
                r#"sdc.device("a", { pages = { "p" }, brightness = 101 })"#,
                "not a percentage",
            ),
            (
                r#"sdc.device("a", { pages = { "p" }, idle = { dim_after = 0 } })"#,
                "sdc.device('a')",
            ),
            (
                r#"sdc.device("a", { pages = { "p" } }) sdc.default_device { mk2 = "zz" }"#,
                "unknown device 'zz'",
            ),
            (
                r#"sdc.device("a", { pages = { "p" } }) sdc.default_device { mk3 = "a" }"#,
                "unknown device type 'mk3'",
            ),
            (
                r#"sdc.default_device { ["*"] = "a" }"#,
                "unknown device 'a'",
            ),
        ];
        for (src, want) in cases {
            let e = err(&format!("{pages}{src}"));
            assert!(e.contains(want), "{src}: {e}");
        }
        // pages may be defined after the device that uses them
        LuaRuntime::load_str(
            r#"sdc.device("a", { pages = { "late" } }) sdc.page("late", {})"#,
            Host::new(None),
        )
        .unwrap();
    }

    #[test]
    fn span_layout_and_validation() {
        let rt = LuaRuntime::load_str(
            r#"
            sdc.key_gap(10)
            sdc.page("p", { false, sdc.span{ cols = 2, rows = 2, gap = 5, render = { text = "S" } },
                            false, false, sdc.button{ render = { text = "B" } } })
            "#,
            Host::new(None),
        )
        .unwrap();
        assert_eq!(rt.settings().key_gap, Some(10));
        let cells = rt.layout("p", 3, 5);
        assert_eq!(cells.len(), 15);
        let spans: Vec<_> = cells
            .iter()
            .enumerate()
            .filter_map(|(k, c)| match c {
                Cell::Span(s) => Some((k, s.origin, s.index, s.error.is_some(), s.size.gap)),
                _ => None,
            })
            .collect();
        assert_eq!(
            spans,
            [
                (1, 1, 0, false, Some(5)),
                (2, 1, 1, false, Some(5)),
                (6, 1, 2, false, Some(5)),
                (7, 1, 3, false, Some(5))
            ]
        );
        assert!(matches!(cells[4], Cell::Button(_)));
        // a 4 column grid: the button at slot 4 is outside the span's covered slots
        let narrow = rt.layout("p", 3, 4);
        assert!(matches!(&narrow[1], Cell::Span(s) if s.error.is_none()));
        // 2 rows do not fit a 1 row grid
        let short = rt.layout("p", 1, 5);
        assert!(
            matches!(&short[1], Cell::Span(s) if s.error.as_deref().is_some_and(|e| e.contains("does not fit")))
        );
        // a button in a slot the span covers
        let rt = LuaRuntime::load_str(
            r#"sdc.page("p", { sdc.span{ cols = 2, rows = 1, render = { text = "S" } }, sdc.button{} })"#,
            Host::new(None),
        )
        .unwrap();
        let cells = rt.layout("p", 3, 5);
        assert!(
            matches!(&cells[0], Cell::Span(s) if s.error.as_deref().is_some_and(|e| e.contains("overlaps")))
        );
        assert!(matches!(&cells[1], Cell::Span(_)));
        for bad in [
            "sdc.span{ rows = 2 }",
            "sdc.span{ cols = 0, rows = 1 }",
            "sdc.span{ cols = 1, rows = 1, gap = -1 }",
            "sdc.span{ cols = 1, rows = 1, press = 1 }",
            "sdc.key_gap(-3)",
        ] {
            assert!(LuaRuntime::load_str(bad, Host::new(None)).is_err(), "{bad}");
        }
    }

    fn slot_info(slot: &str) -> CtxInfo {
        CtxInfo {
            slot: Some(slot.into()),
            ..info("", 0)
        }
    }

    #[test]
    fn export_registers_and_returns_the_button() {
        let rt = LuaRuntime::load_str(
            r##"
            local b = sdc.export("living", sdc.button{ label = "Living", render = { text = "L" } })
            sdc.page("p", { b })                       -- same button in a page
            sdc.export("dim", sdc.dial{ render = function() return { title = "T" } end })
            sdc.export("wide", sdc.span{ cols = 2, rows = 2, render = { text = "S" } })
            "##,
            Host::new(None),
        )
        .unwrap();
        assert!(rt.button("p", 0).is_some());
        let items: Vec<_> = rt
            .exports()
            .into_iter()
            .map(|e| (e.id, e.kind, e.label))
            .collect();
        assert_eq!(
            items,
            [
                ("living".into(), SlotKind::Key, Some("Living".into())),
                ("dim".into(), SlotKind::Dial, None),
                ("wide".into(), SlotKind::Key, None),
            ]
        );
        assert!(rt.export("nope").is_none());
        // an exported span renders as a plain key: no span size in ctx
        let Some(Export::Button(def)) = rt.export("wide") else {
            panic!("not a button")
        };
        let o = rt.render(&def, &slot_info("s"));
        assert_eq!(o.spec.text.as_deref(), Some("S"));
    }

    #[test]
    fn export_rejects_duplicates_bad_ids_and_non_buttons() {
        for bad in [
            r#"sdc.export("a", sdc.button{}) sdc.export("a", sdc.button{})"#,
            r#"sdc.export("", sdc.button{})"#,
            r#"sdc.export("has space", sdc.button{})"#,
            r#"sdc.export("a/b", sdc.button{})"#,
            r#"sdc.export("a", {})"#,
            r#"sdc.dial{}"#,
            r#"sdc.dial{ render = function() end, rotate = 1 }"#,
            r#"sdc.page("p", { sdc.dial{ render = function() end } })"#,
        ] {
            assert!(LuaRuntime::load_str(bad, Host::new(None)).is_err(), "{bad}");
        }
    }

    #[test]
    fn dial_render_and_inputs() {
        let host = Host::new(None);
        host.states
            .borrow_mut()
            .insert("light.a".into(), state("light.a", "on"));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let host = Rc::new(Host {
            ha_tx: Some(tx),
            states: host.states.clone(),
            ..Default::default()
        });
        let rt = LuaRuntime::load_str(
            r##"
            sdc.export("d", sdc.dial {
              render = function(ctx)
                local l = ha.state("light.a")
                return { title = ctx.slot, value = 42, indicator = 250, icon = "i.png", bg = "#102030" }
              end,
              rotate = function(ctx, t) ha.call("light", "turn_on", { entity_id = "light.a", step = t }) end,
              press = function(ctx) ha.call("light", "toggle", { entity_id = "light.a" }); ctx.goto_page("x") end,
            })
            "##,
            host,
        )
        .unwrap();
        let Some(Export::Dial(d)) = rt.export("d") else {
            panic!("not a dial")
        };
        let o = rt.render_dial(&d, &slot_info("slot-1"));
        assert!(o.error.is_none());
        assert_eq!(o.spec.title, "slot-1");
        assert_eq!(o.spec.value, "42");
        assert_eq!(o.spec.indicator, 100);
        assert_eq!(o.spec.icon.as_deref(), Some("i.png"));
        assert_eq!(o.spec.bg, Color([0x10, 0x20, 0x30]));
        assert_eq!(o.deps, HashSet::from(["light.a".to_string()]));
        rt.dial_input(&d, &slot_info("slot-1"), DialInput::Rotate(-3))
            .unwrap();
        assert_eq!(
            rx.try_recv().unwrap(),
            HaCommand::CallService {
                domain: "light".into(),
                service: "turn_on".into(),
                data: json!({"entity_id": "light.a", "step": -3})
            }
        );
        // press runs (goto_page is dropped); release/touch have no handler
        rt.dial_input(&d, &slot_info("slot-1"), DialInput::Press)
            .unwrap();
        assert!(rx.try_recv().is_ok());
        rt.dial_input(&d, &slot_info("slot-1"), DialInput::Touch)
            .unwrap();
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn plugin_goto_page_is_a_noop() {
        let rt = LuaRuntime::load_str(
            r#"sdc.export("b", sdc.button{ press = function(ctx) ctx.goto_page("other") end })"#,
            Host::new(None),
        )
        .unwrap();
        let Some(Export::Button(b)) = rt.export("b") else {
            panic!("not a button")
        };
        // the runtime reports the request; the engine drops it for plugin slots
        assert_eq!(
            rt.press(&b, &slot_info("s"), true).unwrap().as_deref(),
            Some("other")
        );
    }

    #[test]
    fn dial_render_error_gives_error_dial() {
        let rt = LuaRuntime::load_str(
            r#"sdc.export("d", sdc.dial{ render = function() error("boom") end })"#,
            Host::new(None),
        )
        .unwrap();
        let Some(Export::Dial(d)) = rt.export("d") else {
            panic!()
        };
        let o = rt.render_dial(&d, &slot_info("s"));
        assert_eq!(o.spec.title, "ERR");
        assert!(o.error.unwrap().contains("boom"));
    }
}
