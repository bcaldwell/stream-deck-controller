//! The Lua-facing API (`sdc`, `ha`) and evaluation of buttons.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use mlua::{serde::SerializeOptions, FromLua, Function, Lua, LuaSerdeExt, Table, UserData, Value};
use tokio::sync::mpsc::UnboundedSender;

use sdc_protocol::{ExportItem, SlotKind, DEVICE_TYPES};

use crate::ha::{EntityState, HaCommand};
use crate::render::{Chevron, Color, RenderSpec};

pub fn lerr(e: mlua::Error) -> anyhow::Error {
    anyhow!("{e}")
}

/// How long a built-in dial (`sdc.light_dial`, `sdc.volume_dial`) keeps stepping from and showing
/// its own value after the last turn, before it shows Home Assistant's state again. The engine
/// redraws a dial slot this long after a turn.
pub const DIAL_SETTLE: Duration = Duration::from_millis(1500);

/// The built-in dials, run against `sdc` when a config loads.
const DIALS_LUA: &str = include_str!("lua/dials.lua");

/// State shared between the engine and the Lua closures (single-threaded).
#[derive(Default)]
pub struct Host {
    pub states: RefCell<HashMap<String, EntityState>>,
    pub ha_tx: Option<UnboundedSender<HaCommand>>,
    tracking: RefCell<Option<HashSet<String>>>,
    pending_goto: RefCell<Option<String>>,
    /// Tests set `sdc.now()` by hand.
    #[cfg(test)]
    pub(crate) now_ms: std::cell::Cell<Option<f64>>,
}

impl Host {
    pub fn new(ha_tx: Option<UnboundedSender<HaCommand>>) -> Rc<Self> {
        Rc::new(Self {
            ha_tx,
            ..Default::default()
        })
    }

    /// `sdc.now()`: milliseconds on a monotonic clock (from the first call).
    fn now_ms(&self) -> f64 {
        #[cfg(test)]
        if let Some(t) = self.now_ms.get() {
            return t;
        }
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0
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
    /// Name shown in the plugin's export dropdown.
    label: Option<String>,
}

impl UserData for ButtonDef {}

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
    label: Option<String>,
}

impl UserData for DialDef {}

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

/// Size of an `sdc.span`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanDef {
    pub cols: u8,
    pub rows: u8,
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

/// A device's `idle = {...}`: dim, then switch the display off, after this long without a key
/// press; `home_after` returns the client to the device's home page. With none set a client
/// never idles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleConfig {
    pub dim_after: Option<Duration>,
    /// Brightness while dimmed (percent).
    pub dim_brightness: u8,
    pub off_after: Option<Duration>,
    /// Back to the home page after this long without a key press (also bounds how long a
    /// disconnected client's page is remembered).
    pub home_after: Option<Duration>,
}

impl Default for IdleConfig {
    fn default() -> Self {
        Self {
            dim_after: None,
            dim_brightness: DEFAULT_DIM_BRIGHTNESS,
            off_after: None,
            home_after: None,
        }
    }
}

/// Brightness of a device that sets no `brightness`.
pub const DEFAULT_BRIGHTNESS: u8 = 60;
const DEFAULT_DIM_BRIGHTNESS: u8 = 10;

/// Config-wide settings (`sdc.feedback`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// `sdc.feedback`: flash a key after its press handler ran (default true).
    pub feedback: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { feedback: true }
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
    /// Active brightness (percent); [`DEFAULT_BRIGHTNESS`] when the device sets none.
    pub brightness: u8,
    /// Dim/off/home timers; the default never idles.
    pub idle: IdleConfig,
}

#[derive(Default)]
struct Registry {
    pages: Vec<(String, Vec<Option<ButtonDef>>)>,
    devices: Vec<DeviceDef>,
    /// `sdc.default_device`: device type (or `*`) -> device name.
    default_devices: Vec<(String, String)>,
    settings: Settings,
    exports: Vec<(String, Export)>,
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

/// Test configs rarely care about devices: wraps `src` so that, unless it defines an
/// `sdc.device` itself, one named "test" lists every page it defined (a config without pages gets an empty page `__none`). The prelude stays on
/// the first line so error line numbers are unchanged.
#[cfg(test)]
pub(crate) fn test_config(src: &str) -> String {
    format!(
        "local __pages, __dev, __page, __device = {{}}, false, sdc.page, sdc.device; \
         sdc.page = function(n, ...) __pages[#__pages + 1] = n; return __page(n, ...) end; \
         sdc.device = function(...) __dev = true; return __device(...) end; {src}\n\
         if not __dev then if #__pages == 0 then __page('__none', {{}}); __pages[1] = '__none' end; \
         __device('test', {{ pages = __pages }}) end"
    )
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

    /// Loads config source from a string, exactly as written (tests of the loader itself).
    #[cfg(test)]
    pub(crate) fn load_raw(src: &str, host: Rc<Host>) -> Result<Self> {
        let rt = Self::new(host, None)?;
        rt.lua.load(src).set_name("=config").exec().map_err(lerr)?;
        rt.validate_devices()?;
        Ok(rt)
    }

    /// Like [`LuaRuntime::load_raw`], but a config that defines no `sdc.device` gets one named
    /// "test" showing every page it defined (see [`test_config`]).
    #[cfg(test)]
    pub(crate) fn load_str(src: &str, host: Rc<Host>) -> Result<Self> {
        Self::load_raw(&test_config(src), host)
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

    pub fn settings(&self) -> Settings {
        self.registry.borrow().settings
    }

    /// Cross-checks `sdc.device` / `sdc.default_device` against the pages once the whole config
    /// has run (pages may be defined after the devices that use them).
    fn validate_devices(&self) -> Result<()> {
        let r = self.registry.borrow();
        if r.devices.is_empty() {
            bail!("the config defines no sdc.device; add at least one (see docs/configuration.md#devices)");
        }
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

    /// The device a connecting client gets: the requested name if it exists, else
    /// `sdc.default_device` for its `kind` then `*`, else the first `sdc.device`.
    pub fn resolve_device(&self, requested: Option<&str>, kind: &str) -> DeviceDef {
        let r = self.registry.borrow();
        let find = |name: &str| r.devices.iter().find(|d| d.name == name);
        if let Some(name) = requested {
            match find(name) {
                Some(d) => return d.clone(),
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
            .expect("validated: at least one device")
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

    #[cfg(test)]
    fn button(&self, page: &str, key: u8) -> Option<ButtonDef> {
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
        let (res, deps) = self.tracked(&btn.render, info, parse_spec);
        let (spec, error) = match res {
            Ok(spec) => (spec, None),
            Err(e) => (RenderSpec::error(), Some(e.to_string())),
        };
        RenderOutcome { spec, deps, error }
    }

    /// Runs a dial's render, recording the entities it reads (like [`LuaRuntime::render`]).
    pub fn render_dial(&self, dial: &DialDef, info: &CtxInfo) -> DialOutcome {
        let (res, deps) = self.tracked(&dial.render, info, parse_dial_spec);
        let (spec, error) = match res {
            Ok(spec) => (spec, None),
            Err(e) => (
                DialSpec {
                    title: "ERR".into(),
                    bg: crate::render::ERR_BG,
                    ..DialSpec::default()
                },
                Some(e.to_string()),
            ),
        };
        DialOutcome { spec, deps, error }
    }

    /// Evaluates a `render` (a function of `ctx`, or a static value) through `parse`, together
    /// with the entities read through `ha.state` meanwhile.
    fn tracked<T>(
        &self,
        render: &Value,
        info: &CtxInfo,
        parse: fn(&Value) -> mlua::Result<T>,
    ) -> (mlua::Result<T>, HashSet<String>) {
        *self.host.tracking.borrow_mut() = Some(HashSet::new());
        let res = (|| match render {
            Value::Function(f) => parse(&f.call::<Value>(self.make_ctx(info)?)?),
            other => parse(other),
        })();
        (
            res,
            self.host.tracking.borrow_mut().take().unwrap_or_default(),
        )
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
            .map(|(id, e)| ExportItem {
                id: id.clone(),
                kind: match e {
                    Export::Button(_) => SlotKind::Key,
                    Export::Dial(_) => SlotKind::Dial,
                },
                label: match e {
                    Export::Button(b) => b.label.clone(),
                    Export::Dial(d) => d.label.clone(),
                },
            })
            .collect()
    }

    pub fn export(&self, id: &str) -> Option<Export> {
        let r = self.registry.borrow();
        r.exports
            .iter()
            .find(|(i, _)| i == id)
            .map(|(_, e)| e.clone())
    }

    /// Runs the dial handler for `input` (none set = nothing happens). A `goto_page` is ignored:
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
            self.lua.create_function(move |_, page: Value| {
                *host.pending_goto.borrow_mut() = Some(name_ref(&page, "page", "goto_page(...)")?);
                Ok(())
            })?,
        )?;
        Ok(t)
    }
}

/// A page or device handle, as returned by `sdc.page` / `sdc.device`: `{ __sdc = kind, name = ... }`
/// with a `__tostring` that gives the name.
fn handle(lua: &Lua, kind: &str, name: &str) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("__sdc", kind)?;
    t.set("name", name)?;
    let mt = lua.create_table()?;
    mt.set(
        "__tostring",
        lua.create_function(|_, t: Table| t.get::<String>("name"))?,
    )?;
    t.set_metatable(Some(mt))?;
    Ok(t)
}

/// The name behind a `kind` ("page" / "device") reference: a handle or a name string. `nil`
/// usually means a misspelled or not-yet-defined variable, so it says so.
fn name_ref(v: &Value, kind: &str, what: &str) -> mlua::Result<String> {
    match v {
        Value::String(s) => Ok(s.to_str()?.to_string()),
        Value::Table(t) if t.get::<Option<String>>("__sdc")?.as_deref() == Some(kind) => {
            t.get("name")
        }
        Value::Nil => Err(mlua::Error::runtime(format!(
            "{what} is nil — is the {kind} variable defined above?"
        ))),
        _ => Err(mlua::Error::runtime(format!(
            "{what} must be a {kind} (from sdc.{kind}) or a {kind} name"
        ))),
    }
}

/// The items of a Lua list in order, refusing holes: `{ a, b, c }` where `b` is an undefined
/// variable would otherwise silently end the list at `a`.
fn list_items(t: &Table, what: &str) -> mlua::Result<Vec<Value>> {
    let mut n = 0;
    for pair in t.clone().pairs::<Value, Value>() {
        match pair?.0 {
            Value::Integer(i) if i >= 1 => n = n.max(i as usize),
            _ => return Err(mlua::Error::runtime(format!("{what} must be a list"))),
        }
    }
    (1..=n).map(|i| t.get::<Value>(i)).collect()
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
        lua.create_function(|_, t: Table| button_def(&t, "sdc.button", None))?,
    )?;

    sdc.set(
        "span",
        lua.create_function(|_, t: Table| {
            let what = "sdc.span";
            let size = |name| -> mlua::Result<u8> {
                let n: u8 = field(&t, what, name)?;
                if (1..=32).contains(&n) {
                    Ok(n)
                } else {
                    Err(mlua::Error::runtime(format!(
                        "{what}: '{name}' must be an integer 1-32"
                    )))
                }
            };
            let size = SpanDef {
                cols: size("cols")?,
                rows: size("rows")?,
            };
            button_def(&t, what, Some(size))
        })?,
    )?;

    sdc.set(
        "dial",
        lua.create_function(|_, t: Table| {
            let what = "sdc.dial";
            Ok(DialDef {
                render: t.get("render")?,
                rotate: field(&t, what, "rotate")?,
                press: field(&t, what, "press")?,
                release: field(&t, what, "release")?,
                touch: field(&t, what, "touch")?,
                label: field(&t, what, "label")?,
            })
        })?,
    )?;

    let reg = registry.clone();
    sdc.set(
        "export",
        lua.create_function(move |_, (id, item): (String, Value)| {
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
            let export = if let Some(b) = userdata::<ButtonDef>(&item) {
                Export::Button(b)
            } else if let Some(d) = userdata::<DialDef>(&item) {
                Export::Dial(d)
            } else {
                return Err(mlua::Error::runtime(format!(
                    "sdc.export('{id}'): second argument is not an sdc.button, sdc.span or sdc.dial"
                )));
            };
            let mut r = reg.borrow_mut();
            if r.exports.iter().any(|(i, _)| *i == id) {
                return Err(mlua::Error::runtime(format!(
                    "sdc.export: duplicate id '{id}'"
                )));
            }
            r.exports.push((id, export));
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
        "page",
        lua.create_function(move |lua, (name, items): (String, Table)| {
            let mut keys: Vec<Option<ButtonDef>> = Vec::new();
            for pair in items.pairs::<Value, Value>() {
                let (k, v) = pair?;
                let idx = match k {
                    Value::Integer(i) if i >= 1 => i as usize - 1,
                    _ => {
                        return Err(mlua::Error::runtime(format!(
                            "page '{name}': keys must be positive integers"
                        )))
                    }
                };
                let def = if matches!(v, Value::Boolean(false)) {
                    None
                } else if let Some(b) = userdata::<ButtonDef>(&v) {
                    Some(b)
                } else if userdata::<DialDef>(&v).is_some() {
                    return Err(mlua::Error::runtime(format!(
                        "page '{name}': item {} is an sdc.dial, which only works as an export for the Elgato plugin (sdc.export)",
                        idx + 1
                    )));
                } else {
                    return Err(mlua::Error::runtime(format!(
                        "page '{name}': item {} is not an sdc.button(...) or sdc.span(...) (use false for an empty key)",
                        idx + 1
                    )));
                };
                if keys.len() <= idx {
                    keys.resize(idx + 1, None);
                }
                keys[idx] = def;
            }
            let mut r = reg.borrow_mut();
            match r.pages.iter_mut().find(|(n, _)| *n == name) {
                Some(slot) => slot.1 = keys,
                None => r.pages.push((name.clone(), keys)),
            }
            handle(lua, "page", &name)
        })?,
    )?;

    let reg = registry.clone();
    sdc.set(
        "device",
        lua.create_function(move |lua, (name, t): (String, Table)| {
            let what = format!("sdc.device('{name}')");
            if reg.borrow().devices.iter().any(|d| d.name == name) {
                return Err(mlua::Error::runtime(format!(
                    "sdc.device: duplicate device '{name}'"
                )));
            }
            let mut pages: Vec<String> = Vec::new();
            let mut home = None;
            let mut brightness = DEFAULT_BRIGHTNESS;
            let mut idle = IdleConfig::default();
            for pair in t.pairs::<String, Value>() {
                let (k, v) = pair?;
                let field = format!("{what}: '{k}'");
                match k.as_str() {
                    "pages" => {
                        let Value::Table(list) = &v else {
                            return Err(mlua::Error::runtime(format!(
                                "{field} must be a list of pages (handles or names)"
                            )));
                        };
                        for (i, p) in list_items(list, &field)?.iter().enumerate() {
                            let p = name_ref(p, "page", &format!("{what}: pages[{}]", i + 1))?;
                            if !pages.contains(&p) {
                                pages.push(p);
                            }
                        }
                    }
                    "home" => home = Some(name_ref(&v, "page", &field)?),
                    "brightness" => brightness = percent_arg(&field, &v)?,
                    "idle" => idle = idle_arg(&what, &v)?,
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
            let h = handle(lua, "device", &name)?;
            reg.borrow_mut().devices.push(DeviceDef {
                name,
                pages,
                home,
                brightness,
                idle,
            });
            Ok(h)
        })?,
    )?;

    let reg = registry.clone();
    sdc.set(
        "default_device",
        lua.create_function(move |_, t: Table| {
            let mut map = Vec::new();
            for pair in t.pairs::<String, Value>() {
                let (k, v) = pair?;
                if k != "*" && !DEVICE_TYPES.contains(&k.as_str()) {
                    return Err(mlua::Error::runtime(format!(
                        "sdc.default_device: unknown device type '{k}' (use {} or \"*\")",
                        DEVICE_TYPES.join(", ")
                    )));
                }
                let v = name_ref(&v, "device", &format!("sdc.default_device: '{k}'"))?;
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

    let h = host.clone();
    sdc.set("now", lua.create_function(move |_, ()| Ok(h.now_ms()))?)?;
    sdc.set("DIAL_SETTLE_MS", DIAL_SETTLE.as_millis() as f64)?;
    lua.load(DIALS_LUA)
        .set_name("@sdc/dials.lua")
        .call::<()>(sdc.clone())?;

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

/// Builds the button of `sdc.button` / `sdc.span` from its table.
fn button_def(t: &Table, what: &str, span: Option<SpanDef>) -> mlua::Result<ButtonDef> {
    Ok(ButtonDef {
        render: t.get("render")?,
        press: field(t, what, "press")?,
        release: field(t, what, "release")?,
        feedback: field::<Option<bool>>(t, what, "feedback")?.unwrap_or(true),
        span,
        label: field(t, what, "label")?,
    })
}

/// Reads `t[name]`, naming the constructor and field when the value has the wrong type.
fn field<T: FromLua>(t: &Table, what: &str, name: &str) -> mlua::Result<T> {
    t.get(name)
        .map_err(|e| mlua::Error::runtime(format!("{what}: '{name}': {e}")))
}

/// The `T` behind a value made by `sdc.button` / `sdc.span` / `sdc.dial`, if it is one.
fn userdata<T: UserData + Clone + 'static>(v: &Value) -> Option<T> {
    v.as_userdata()?.borrow::<T>().ok().map(|t| t.clone())
}

/// A text field of a render table: a string or a number.
fn text_field(t: &Table, name: &str) -> mlua::Result<Option<String>> {
    match t.get::<Value>(name)? {
        Value::Nil => Ok(None),
        Value::String(s) => Ok(Some(s.to_str()?.to_string())),
        Value::Integer(i) => Ok(Some(i.to_string())),
        Value::Number(n) => Ok(Some(n.to_string())),
        other => Err(mlua::Error::runtime(format!(
            "'{name}' must be a string, got {}",
            other.type_name()
        ))),
    }
}

fn parse_dial_spec(v: &Value) -> mlua::Result<DialSpec> {
    let mut spec = DialSpec::default();
    let Value::Table(t) = v else {
        return Err(mlua::Error::runtime(format!(
            "dial render must return a table, got {}",
            v.type_name()
        )));
    };
    spec.title = text_field(t, "title")?.unwrap_or_default();
    spec.value = text_field(t, "value")?.unwrap_or_default();
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

fn percent_arg(what: &str, v: &Value) -> mlua::Result<u8> {
    match v {
        Value::Integer(i) if (0..=100).contains(i) => Ok(*i as u8),
        _ => Err(mlua::Error::runtime(format!(
            "{what} must be an integer 0-100"
        ))),
    }
}

/// Parses a device's `idle` table.
fn idle_arg(what: &str, v: &Value) -> mlua::Result<IdleConfig> {
    let Value::Table(t) = v else {
        return Err(mlua::Error::runtime(format!(
            "{what}: 'idle' must be a table like {{ dim_after = 120, off_after = 600 }}"
        )));
    };
    let mut idle = IdleConfig::default();
    for pair in t.pairs::<String, Value>() {
        let (k, v) = pair?;
        match k.as_str() {
            "dim_after" => idle.dim_after = Some(seconds_arg(what, &k, &v)?),
            "off_after" => idle.off_after = Some(seconds_arg(what, &k, &v)?),
            "home_after" => idle.home_after = Some(seconds_arg(what, &k, &v)?),
            "dim_brightness" => {
                idle.dim_brightness = percent_arg(&format!("{what}: 'dim_brightness'"), &v)?
            }
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
    spec.text = text_field(t, "text")?;
    for (field, slot) in [("bg", &mut spec.bg), ("fg", &mut spec.fg)] {
        if let Some(s) = t.get::<Option<String>>(field)? {
            *slot = Color::parse(&s).ok_or_else(|| {
                mlua::Error::runtime(format!("'{field}': invalid color '{s}' (use #rrggbb)"))
            })?;
        }
    }
    spec.icon = t.get("icon")?;
    if let Some(s) = t.get::<Option<String>>("tint")? {
        spec.tint = Some(Color::parse(&s).ok_or_else(|| {
            mlua::Error::runtime(format!("'tint': invalid color '{s}' (use #rrggbb)"))
        })?);
    }
    spec.image = t.get("image")?;
    spec.font_size = t.get("font_size")?;
    spec.chevron = match t.get::<Value>("chevron")? {
        Value::Nil | Value::Boolean(false) => Chevron::None,
        Value::Boolean(true) => Chevron::Forward,
        Value::String(v) if v.to_str()? == "forward" => Chevron::Forward,
        Value::String(v) if v.to_str()? == "back" => Chevron::Back,
        other => {
            return Err(mlua::Error::runtime(format!(
                "chevron: expected true, false, \"forward\" or \"back\", got {other:?}"
            )))
        }
    };
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
    fn pages_and_buttons() {
        let rt = LuaRuntime::load_str(
            r#"
            local sdc = require("sdc")
            sdc.page("a", { sdc.button{ render = { text = "A" } }, false, sdc.button{ render = function() return { text = "C" } end } })
            sdc.page("b", { sdc.button{ render = { text = "B" } } })
            "#,
            Host::new(None),
        )
        .unwrap();
        assert_eq!(rt.page_names(), ["a", "b"]);
        assert!(rt.button("a", 0).is_some());
        assert!(rt.button("a", 1).is_none());
        assert!(rt.button("a", 2).is_some());
        assert!(rt.button("a", 3).is_none());
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
    fn pages_and_devices_are_handles_or_names() {
        let rt = LuaRuntime::load_str(
            r#"
            local other = sdc.page("other", {})
            local home = sdc.page("home", {
              sdc.button{ press = function(ctx) ctx.goto_page(other) end },
              sdc.button{ press = function(ctx) ctx.goto_page("home") end },
            })
            assert(tostring(home) == "home")
            local deck = sdc.device("deck", { pages = { home, "other" }, home = other })
            sdc.device("web", { pages = { home } })
            sdc.default_device { mk2 = deck, browser = "web" }
            "#,
            Host::new(None),
        )
        .unwrap();
        let d = rt.device("deck").unwrap();
        assert_eq!(
            (d.pages.as_slice(), d.home.as_str()),
            (&["home".to_string(), "other".to_string()][..], "other")
        );
        assert_eq!(rt.resolve_device(None, "mk2").name, "deck");
        assert_eq!(rt.resolve_device(None, "browser").name, "web");
        let goto = |key| {
            rt.press(&rt.button("home", key).unwrap(), &info("home", key), true)
                .unwrap()
        };
        assert_eq!(goto(0).as_deref(), Some("other"), "handle");
        assert_eq!(goto(1).as_deref(), Some("home"), "name");
    }

    #[test]
    fn undefined_page_variables_are_load_errors() {
        let err = |src: &str| {
            LuaRuntime::load_str(src, Host::new(None))
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_default()
        };
        // a typo'd variable is nil: inside the list it would otherwise silently end it
        let e = err(r#"local a = sdc.page("a", {}) local b = sdc.page("b", {})
                       sdc.device("d", { pages = { a, bb, b } })"#);
        assert!(
            e.contains("pages[2] is nil") && e.contains("defined above"),
            "{e}"
        );
        let e = err(r#"sdc.page("a", {}) sdc.device("d", { pages = { "a" } })
                       sdc.default_device { mk2 = sdc.page("a", {}) }"#);
        assert!(e.contains("must be a device"), "{e}");
        let e = err(r#"local a = sdc.page("a", {}) sdc.device("d", { pages = { a }, home = 5 })"#);
        assert!(e.contains("'home' must be a page"), "{e}");
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
    fn device_idle_and_brightness() {
        let rt = LuaRuntime::load_str(
            r#"sdc.page("p", {})
               sdc.device("a", { pages = { "p" }, brightness = 40,
                                 idle = { dim_after = 120, off_after = 600.5, home_after = 3600 } })
               sdc.device("b", { pages = { "p" } })"#,
            Host::new(None),
        )
        .unwrap();
        let a = rt.device("a").unwrap();
        assert_eq!(a.brightness, 40);
        assert_eq!(a.idle.dim_after, Some(Duration::from_secs(120)));
        assert_eq!(a.idle.off_after, Some(Duration::from_millis(600_500)));
        assert_eq!(a.idle.home_after, Some(Duration::from_secs(3600)));
        assert_eq!(a.idle.dim_brightness, 10);
        // nothing set: never idles, default brightness
        let b = rt.device("b").unwrap();
        assert_eq!(b.brightness, DEFAULT_BRIGHTNESS);
        assert_eq!(b.idle, IdleConfig::default());
        assert_eq!(rt.settings(), Settings::default());
        for bad in [
            "brightness = 101",
            "idle = { dim_after = 0 }",
            "idle = { dim_brightness = 200 }",
            "idle = { dim_aftr = 5 }",
            "idle = { home_after = -1 }",
        ] {
            let src =
                format!(r#"sdc.page("p", {{}}) sdc.device("d", {{ pages = {{ "p" }}, {bad} }})"#);
            assert!(
                LuaRuntime::load_str(&src, Host::new(None)).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn no_device_is_a_load_error() {
        for src in ["", r#"sdc.page("p", {})"#] {
            let e = LuaRuntime::load_raw(src, Host::new(None)).err().unwrap();
            assert!(
                format!("{e:#}").contains("the config defines no sdc.device; add at least one"),
                "{e:#}"
            );
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
        assert_eq!((a.home.as_str(), a.brightness), ("q", 40));
        assert_eq!(a.idle.dim_after, Some(Duration::from_secs(5)));
        let b = rt.device("b").unwrap();
        assert_eq!(
            (b.home.as_str(), b.idle),
            ("q", IdleConfig::default()),
            "home defaults to the first page"
        );
        let name = |req, kind| rt.resolve_device(req, kind).name;
        assert_eq!(name(Some("a"), "mk2"), "a");
        assert_eq!(name(None, "mk2"), "b");
        assert_eq!(name(None, "xl"), "a");
        assert_eq!(name(Some("zzz"), "mk2"), "b");
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
                "must be an integer 0-100",
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
            sdc.page("p", { false, sdc.span{ cols = 2, rows = 2, render = { text = "S" } },
                            false, false, sdc.button{ render = { text = "B" } } })
            "#,
            Host::new(None),
        )
        .unwrap();
        let cells = rt.layout("p", 3, 5);
        assert_eq!(cells.len(), 15);
        let spans: Vec<_> = cells
            .iter()
            .enumerate()
            .filter_map(|(k, c)| match c {
                Cell::Span(s) => Some((k, s.origin, s.index, s.error.is_some())),
                _ => None,
            })
            .collect();
        assert_eq!(
            spans,
            [
                (1, 1, 0, false),
                (2, 1, 1, false),
                (6, 1, 2, false),
                (7, 1, 3, false)
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
            "sdc.span{ cols = 1, rows = 1, press = 1 }",
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

    /// A runtime whose `ha.call`s land in the returned receiver, with `states` already known.
    fn dial_runtime(
        src: &str,
        states: &[(&str, &str, serde_json::Value)],
    ) -> (
        LuaRuntime,
        Rc<Host>,
        tokio::sync::mpsc::UnboundedReceiver<HaCommand>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let host = Host::new(Some(tx));
        host.now_ms.set(Some(0.0));
        for (id, st, attrs) in states {
            host.states.borrow_mut().insert(
                id.to_string(),
                EntityState {
                    entity_id: id.to_string(),
                    state: st.to_string(),
                    attributes: attrs.clone(),
                },
            );
        }
        let rt = LuaRuntime::load_str(src, host.clone()).unwrap();
        (rt, host, rx)
    }

    fn dial_of(rt: &LuaRuntime, id: &str) -> DialDef {
        let Some(Export::Dial(d)) = rt.export(id) else {
            panic!("{id} is not a dial")
        };
        d
    }

    fn call_data(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<HaCommand>,
    ) -> (String, serde_json::Value) {
        let HaCommand::CallService { service, data, .. } = rx.try_recv().expect("a call");
        (service, data)
    }

    #[test]
    fn light_dial_steps_from_its_own_value_until_settled() {
        let (rt, host, mut rx) = dial_runtime(
            r#"sdc.export("d", sdc.light_dial { entity = "light.o", label = "Office" })"#,
            &[(
                "light.o",
                "on",
                json!({"brightness": 128, "supported_color_modes": ["color_temp", "xy"],
                       "color_mode": "color_temp", "color_temp_kelvin": 3000,
                       "min_color_temp_kelvin": 2000, "max_color_temp_kelvin": 6000}),
            )],
        );
        let d = dial_of(&rt, "d");
        let ctx = slot_info("s");
        // HA still says 50% after every call; the dial keeps counting from its own value
        for want in [55, 60, 65] {
            rt.dial_input(&d, &ctx, DialInput::Rotate(1)).unwrap();
            assert_eq!(call_data(&mut rx).1["brightness_pct"], json!(want));
        }
        let o = rt.render_dial(&d, &ctx);
        assert_eq!(
            (
                o.spec.title.as_str(),
                o.spec.value.as_str(),
                o.spec.indicator
            ),
            ("Office", "65%", 65)
        );
        // after the settle time HA's state is shown and used again
        host.now_ms.set(Some(DIAL_SETTLE.as_millis() as f64 + 1.0));
        assert_eq!(rt.render_dial(&d, &ctx).spec.value, "50%");
        rt.dial_input(&d, &ctx, DialInput::Rotate(-2)).unwrap();
        assert_eq!(call_data(&mut rx).1["brightness_pct"], json!(40));

        // press cycles brightness -> warmth -> hue -> brightness
        rt.dial_input(&d, &ctx, DialInput::Press).unwrap();
        assert_eq!(rt.render_dial(&d, &ctx).spec.title, "Office · warmth");
        rt.dial_input(&d, &ctx, DialInput::Rotate(3)).unwrap();
        assert_eq!(call_data(&mut rx).1["color_temp_kelvin"], json!(3300));
        assert_eq!(rt.render_dial(&d, &ctx).spec.value, "3300K");
        rt.dial_input(&d, &ctx, DialInput::Press).unwrap();
        rt.dial_input(&d, &ctx, DialInput::Rotate(-1)).unwrap();
        // from white: a saturated colour, hue stepping down wraps
        assert_eq!(call_data(&mut rx).1["hs_color"], json!([350, 80]));
        assert_eq!(rt.render_dial(&d, &ctx).spec.title, "Office · hue");
        rt.dial_input(&d, &ctx, DialInput::Press).unwrap();
        assert_eq!(rt.render_dial(&d, &ctx).spec.title, "Office");
        // a tap toggles
        rt.dial_input(&d, &ctx, DialInput::Touch).unwrap();
        assert_eq!(call_data(&mut rx).0, "toggle");
    }

    #[test]
    fn light_dial_modes_follow_the_light() {
        let (rt, _host, mut rx) = dial_runtime(
            r#"sdc.export("d", sdc.light_dial { entity = "light.k" })"#,
            &[(
                "light.k",
                "off",
                json!({"supported_color_modes": ["color_temp"],
                "min_color_temp_kelvin": 2900, "max_color_temp_kelvin": 7000}),
            )],
        );
        let d = dial_of(&rt, "d");
        let ctx = slot_info("s");
        assert_eq!(rt.render_dial(&d, &ctx).spec.value, "off");
        // off: turning up starts from 0
        rt.dial_input(&d, &ctx, DialInput::Rotate(2)).unwrap();
        assert_eq!(call_data(&mut rx).1["brightness_pct"], json!(10));
        assert_eq!(rt.render_dial(&d, &ctx).spec.value, "10%");
        rt.dial_input(&d, &ctx, DialInput::Rotate(-5)).unwrap();
        assert_eq!(call_data(&mut rx).0, "turn_off");
        // no colour: brightness and warmth only
        rt.dial_input(&d, &ctx, DialInput::Press).unwrap();
        assert_eq!(rt.render_dial(&d, &ctx).spec.title, "light.k · warmth");
        rt.dial_input(&d, &ctx, DialInput::Press).unwrap();
        assert_eq!(rt.render_dial(&d, &ctx).spec.title, "light.k");
    }

    #[test]
    fn volume_dial_steps_and_mutes() {
        let (rt, _host, mut rx) = dial_runtime(
            r#"sdc.export("v", sdc.volume_dial { entity = "media_player.k", label = "Kitchen", step = 3 })"#,
            &[(
                "media_player.k",
                "playing",
                json!({"volume_level": 0.2, "is_volume_muted": false}),
            )],
        );
        let d = dial_of(&rt, "v");
        let ctx = slot_info("s");
        rt.dial_input(&d, &ctx, DialInput::Rotate(1)).unwrap();
        rt.dial_input(&d, &ctx, DialInput::Rotate(1)).unwrap();
        call_data(&mut rx);
        assert_eq!(call_data(&mut rx).1["volume_level"], json!(0.26));
        assert_eq!(rt.render_dial(&d, &ctx).spec.value, "26%");
        rt.dial_input(&d, &ctx, DialInput::Press).unwrap();
        assert_eq!(
            call_data(&mut rx),
            (
                "volume_mute".into(),
                json!({"entity_id": "media_player.k", "is_volume_muted": true})
            )
        );
    }

    #[test]
    fn built_in_dials_need_an_entity() {
        let e = LuaRuntime::load_str(r#"sdc.export("d", sdc.light_dial {})"#, Host::new(None))
            .err()
            .unwrap()
            .to_string();
        assert!(e.contains("sdc.light_dial: { entity"), "{e}");
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
