//! The Lua-facing API (`sdc`, `ha`) and evaluation of buttons.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use mlua::{serde::SerializeOptions, Function, Lua, LuaSerdeExt, Table, Value};
use tokio::sync::mpsc::UnboundedSender;

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
    /// Set for `sdc.span{...}`: the button covers a rectangle of keys.
    pub span: Option<SpanDef>,
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
        }
    }
}

#[derive(Default)]
struct Registry {
    pages: Vec<(String, Vec<Option<ButtonDef>>)>,
    default: Option<String>,
    settings: Settings,
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
        Ok(rt)
    }

    /// Loads config source from a string (tests).
    pub fn load_str(src: &str, host: Rc<Host>) -> Result<Self> {
        let rt = Self::new(host, None)?;
        rt.lua.load(src).set_name("=config").exec().map_err(lerr)?;
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

    fn make_ctx(&self, info: &CtxInfo) -> mlua::Result<Table> {
        let t = self.lua.create_table()?;
        t.set("client_id", info.client_id.as_str())?;
        t.set("key", info.key)?;
        t.set("page", info.page.as_str())?;
        t.set("rows", info.rows)?;
        t.set("cols", info.cols)?;
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
                    Value::Table(t) if matches!(t.get::<Option<String>>("__sdc")?.as_deref(), Some("button" | "span")) => {
                        let is_span = t.get::<String>("__sdc")? == "span";
                        Some(ButtonDef {
                            render: t.get("render")?,
                            press: t.get("press")?,
                            release: t.get("release")?,
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
            let mut idle = IdleConfig {
                dim_brightness: DEFAULT_DIM_BRIGHTNESS,
                ..Default::default()
            };
            for pair in t.pairs::<String, Value>() {
                let (k, v) = pair?;
                match k.as_str() {
                    "dim_after" => idle.dim_after = Some(seconds_arg(&k, &v)?),
                    "off_after" => idle.off_after = Some(seconds_arg(&k, &v)?),
                    "home_after" => idle.home_after = Some(seconds_arg(&k, &v)?),
                    "dim_brightness" => match v {
                        Value::Integer(i) => {
                            idle.dim_brightness = percent_arg("sdc.idle: 'dim_brightness'", i)?
                        }
                        _ => {
                            return Err(mlua::Error::runtime(
                                "sdc.idle: 'dim_brightness' must be an integer 0-100",
                            ))
                        }
                    },
                    _ => {
                        return Err(mlua::Error::runtime(format!(
                        "sdc.idle: unknown field '{k}' (use dim_after, dim_brightness, off_after, home_after)"
                    )))
                    }
                }
            }
            reg.borrow_mut().settings.idle = idle;
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

fn percent_arg(what: &str, v: i64) -> mlua::Result<u8> {
    u8::try_from(v)
        .ok()
        .filter(|p| *p <= 100)
        .ok_or_else(|| mlua::Error::runtime(format!("{what}: {v} is not a percentage (0-100)")))
}

fn seconds_arg(field: &str, v: &Value) -> mlua::Result<Duration> {
    let secs = match v {
        Value::Integer(i) => *i as f64,
        Value::Number(n) => *n,
        _ => 0.0,
    };
    if secs.is_finite() && secs > 0.0 {
        Ok(Duration::from_secs_f64(secs))
    } else {
        Err(mlua::Error::runtime(format!(
            "sdc.idle: '{field}' must be a number of seconds > 0"
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
}
