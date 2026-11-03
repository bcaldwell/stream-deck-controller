//! The engine: owns the Lua state (which is `!Send`) and everything shown. It runs on a dedicated
//! thread and is driven by [`EngineEvent`]s. Three parts:
//!
//! - connections: decks ([`Client`]s, with power and page state) and Elgato plugins
//!   ([`plugin`]), each with a channel its output goes out on;
//! - render targets ([`target`]): the keys of every deck and the slots of every plugin share one
//!   implementation of rendering, dependency tracking, change detection, flash and presses;
//! - events: what happened (a key, a Home Assistant change, a reload, time passing).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Result;
use image::RgbaImage;
use sdc_protocol::{device_type, DeviceInfo, PluginClientMessage};
use tokio::runtime::Handle;
use tokio::sync::mpsc::{self, UnboundedSender, WeakUnboundedSender};

use crate::assets::{Assets, Fetcher, S3Config};
use crate::ha::{HaCommand, HaConfig, HaEvent};
use crate::lua_api::{Cell, CtxAt, CtxInfo, CtxSpan, DeviceDef, Host, LuaRuntime};
use crate::render::Renderer;

mod plugin;
mod status;
mod target;

use plugin::Plugin;
pub use plugin::PluginOut;
pub use status::EngineStatus;
pub use target::TargetId;
use target::{Deps, Hold, Target};

pub type ClientId = u64;

/// A rendered key image for one client.
#[derive(Debug, Clone)]
pub struct Frame {
    pub key: u8,
    pub jpeg: Vec<u8>,
}

/// What the engine sends to a client's connection.
#[derive(Debug, Clone)]
pub enum Out {
    Frame(Frame),
    /// Percent 0-100 (0 = display off).
    Brightness(u8),
}

/// How often the engine checks clients for idleness.
const TICK: Duration = Duration::from_secs(1);

/// Display state of a client; only ever left by a tick (dim/off) or a key press (wake).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Power {
    Active,
    Dim,
    Off,
}

/// Everything the engine reacts to. Connections are trusted to have checked their input: the
/// client or plugin exists, key indices are in range, plugin slots are subscribed.
pub enum EngineEvent {
    ClientConnected {
        /// A browser deck rather than a physical one (only shown differently in listings).
        web: bool,
        id: ClientId,
        client_id: String,
        device: DeviceInfo,
        /// The `sdc.device` the client asked for (`Hello.device_name`).
        want_device: Option<String>,
        tx: UnboundedSender<Out>,
    },
    ClientDisconnected(ClientId),
    Key {
        id: ClientId,
        key: u8,
        down: bool,
    },
    /// The client woke from USB suspend: a wake-only key press (no button runs).
    Wake {
        id: ClientId,
    },
    /// An Elgato plugin connected on `/plugin/ws`; it gets the export list right away.
    PluginConnected {
        id: ClientId,
        client_id: String,
        tx: UnboundedSender<PluginOut>,
    },
    PluginDisconnected(ClientId),
    /// A message from a connected plugin (anything but its hello).
    Plugin {
        id: ClientId,
        msg: PluginClientMessage,
    },
    /// Render a target again (a flash is over, a dial's turn has settled). The target may have
    /// gone away since this was scheduled.
    Redraw(TargetId),
    /// A key with a `long_press` handler has been down for `LONG_PRESS` (if it is still).
    LongPress(TargetId),
    /// The config's `sdc.device`s as JSON (name, pages, home page now), for the browser deck's
    /// layout choice.
    ListDevices(tokio::sync::oneshot::Sender<Vec<serde_json::Value>>),
    /// What is connected and how the config loaded, for the status page.
    Status(tokio::sync::oneshot::Sender<EngineStatus>),
    Ha(HaEvent),
    /// A background image download finished (key: the renderer's cache key).
    ImageFetched {
        key: String,
        result: Result<RgbaImage, String>,
    },
    Reload,
    /// Time passed: dim or switch off idle clients.
    Tick,
}

/// What is kept of a client that went away: page, last key press, device name.
type Remembered = (String, Instant, String);

impl DeviceDef {
    fn level(&self, power: Power) -> u8 {
        match power {
            Power::Active => self.brightness,
            Power::Dim => self.idle.dim_brightness,
            Power::Off => 0,
        }
    }
}

/// Device type used for `sdc.default_device`.
fn client_kind(web: bool, device: &DeviceInfo) -> &'static str {
    if web {
        "browser"
    } else {
        device_type(&device.model)
    }
}

struct Client {
    client_id: String,
    web: bool,
    device: DeviceInfo,
    /// `Hello.device_name`; kept to re-resolve the device on config reload.
    want_device: Option<String>,
    /// The `sdc.device` in effect.
    def: DeviceDef,
    tx: UnboundedSender<Out>,
    page: String,
    /// The page `def`'s home was when the client last went there: a deck still on it when it
    /// wakes from Off gets the home of the new time of day.
    home: String,
    /// What each key shows on `page`.
    cells: Vec<Cell>,
    /// One render target per key.
    keys: Vec<Target<Vec<u8>>>,
    connected_at: Instant,
    /// Last key press (or connect): drives dim/off.
    last_activity: Instant,
    /// Last key press, carried across reconnects: drives `home_after`.
    last_press: Instant,
    power: Power,
    /// Keys whose press woke the display: their release is swallowed too.
    swallow_up: HashSet<u8>,
}

impl Client {
    /// Recomputes `cells` (after the page or the config changed).
    fn layout(&mut self, rt: &LuaRuntime) {
        self.cells = rt.layout(&self.page, self.device.rows, self.device.cols);
    }

    fn set_page(&mut self, rt: &LuaRuntime, page: &str) {
        self.page = page.to_string();
        self.layout(rt);
        for key in &mut self.keys {
            key.flash_until = None;
            key.hold = Hold::Idle;
        }
    }

    /// The `ctx` Lua gets for `key` (`span`: when it is part of a multi-key button).
    fn ctx(&self, key: u8, span: Option<CtxSpan>) -> CtxInfo {
        CtxInfo {
            client_id: self.client_id.clone(),
            at: CtxAt::Key {
                key,
                page: self.page.clone(),
                rows: self.device.rows,
                cols: self.device.cols,
                span,
            },
        }
    }

    fn key_ids(&self, id: ClientId) -> impl Iterator<Item = TargetId> {
        (0..self.keys.len() as u8).map(move |k| TargetId::Key(id, k))
    }
}

pub struct Engine {
    config_path: PathBuf,
    host: Rc<Host>,
    rt: LuaRuntime,
    /// When `rt` was loaded, and the last failed reload since (cleared by a good one).
    loaded_at: Instant,
    reload_error: Option<(Instant, String)>,
    renderer: Renderer,
    clients: BTreeMap<ClientId, Client>,
    /// Page and last key press of clients that went away, by client_id, so a reconnect (deploy,
    /// client update) resumes where they were. Expires after the device's `home_after`; lost on
    /// restart.
    remembered: HashMap<String, Remembered>,
    /// Elgato plugin connections and the slots they subscribed.
    plugins: BTreeMap<ClientId, Plugin>,
    /// What every key and slot read when it was last rendered.
    deps: Deps,
    /// For work later (redraws): the runtime, and the engine's own event queue (held weakly, so
    /// it does not keep the event loop alive).
    handle: Handle,
    events: WeakUnboundedSender<EngineEvent>,
}

impl Engine {
    /// Loads the config (an error if it does not load). Remote images download on `handle`;
    /// their results, and delayed redraws, come back as events on `events`.
    pub fn new(
        config_path: PathBuf,
        ha_tx: UnboundedSender<HaCommand>,
        ha: Option<HaConfig>,
        handle: Handle,
        events: &UnboundedSender<EngineEvent>,
    ) -> Result<Self> {
        let host = Host::new(ha_tx);
        let rt = LuaRuntime::load(&config_path, host.clone())?;
        let base = config_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let weak = events.downgrade();
        let fetched = weak.clone();
        let fetcher = Fetcher::new(handle.clone(), move |(key, result)| {
            if let Some(tx) = fetched.upgrade() {
                let _ = tx.send(EngineEvent::ImageFetched { key, result });
            }
        });
        let renderer = Renderer::new(Assets::new(base, S3Config::from_env(), ha), fetcher);
        let mut engine = Self {
            config_path,
            host,
            rt,
            loaded_at: Instant::now(),
            reload_error: None,
            renderer,
            clients: BTreeMap::new(),
            remembered: HashMap::new(),
            plugins: BTreeMap::new(),
            deps: Deps::default(),
            handle,
            events: weak,
        };
        engine.config_loaded();
        Ok(engine)
    }

    /// Remote images still downloading.
    pub fn pending_fetches(&self) -> usize {
        self.renderer.pending()
    }

    /// Reloads the config. On failure the previous config stays active.
    pub fn reload(&mut self) -> Result<()> {
        let new = LuaRuntime::load(&self.config_path, self.host.clone())?;
        // The old runtime outlives the clients' cells that still point into it.
        let _old = std::mem::replace(&mut self.rt, new);
        self.loaded_at = Instant::now();
        self.config_loaded();
        Ok(())
    }

    /// Brings everything in line with the (new) `self.rt`.
    fn config_loaded(&mut self) {
        tracing::info!("loaded {}", self.config_path.display());
        self.renderer.reload();
        self.deps.clear();
        for name in self.rt.static_image_names() {
            self.renderer.prefetch(&name);
        }
        // Re-resolve every deck's device; a changed device or a page it no longer allows sends
        // the deck to its (new) home. A client is not woken or put to sleep here (the next tick
        // does that if the idle settings now say so), but is told about a changed brightness.
        let mut rebrightened = Vec::new();
        for (id, c) in &mut self.clients {
            let def = self
                .rt
                .resolve_device(c.want_device.as_deref(), client_kind(c.web, &c.device));
            if c.def.name != def.name || !def.pages.contains(&c.page) {
                c.page = self.rt.home(&def);
                c.home = c.page.clone();
            }
            if c.def.level(c.power) != def.level(c.power) {
                rebrightened.push(*id);
            }
            c.def = def;
            c.layout(&self.rt);
        }
        self.push_exports();
        self.render_all();
        for id in rebrightened {
            self.send_brightness(id);
        }
    }

    pub fn handle(&mut self, ev: EngineEvent, now: Instant) {
        match ev {
            EngineEvent::ClientConnected {
                web,
                id,
                client_id,
                device,
                want_device,
                tx,
            } => self.client_connected(id, web, client_id, device, want_device, tx, now),
            EngineEvent::ClientDisconnected(id) => {
                let c = self.clients.remove(&id).expect("a connected client");
                tracing::info!("client {} disconnected", c.client_id);
                // A newer connection of the same client already took over its page.
                if !self.clients.values().any(|o| o.client_id == c.client_id) {
                    self.remembered
                        .insert(c.client_id, (c.page, c.last_press, c.def.name));
                }
                self.deps.remove_conn(id);
            }
            EngineEvent::Key { id, key, down } => self.key_event(id, key, down, now),
            EngineEvent::Wake { id } => self.wake(id, now),
            EngineEvent::PluginConnected { id, client_id, tx } => {
                self.plugin_connected(id, client_id, tx)
            }
            EngineEvent::PluginDisconnected(id) => self.plugin_disconnected(id),
            EngineEvent::Plugin { id, msg } => self.plugin_message(id, msg, now),
            EngineEvent::Redraw(target) => self.redraw(target, now),
            EngineEvent::LongPress(target) => self.long_press(&target, now),
            EngineEvent::ListDevices(reply) => {
                let devices = self.rt.devices();
                let _ = reply.send(
                    devices
                        .into_iter()
                        .map(|d| {
                            serde_json::json!({
                                "name": d.name, "pages": d.pages, "home": self.rt.home(&d)
                            })
                        })
                        .collect(),
                );
            }
            EngineEvent::Status(reply) => {
                let _ = reply.send(self.status(now));
            }
            EngineEvent::Ha(e) => self.ha_event(e),
            EngineEvent::ImageFetched { key, result } => {
                self.renderer.finish_fetch(&key, result);
                self.dep_changed(&format!("img:{key}"));
            }
            EngineEvent::Reload => {
                self.reload_error = self.reload().err().map(|e| {
                    tracing::error!("config reload failed, keeping previous config: {e:#}");
                    (now, format!("{e:#}"))
                });
            }
            EngineEvent::Tick => self.tick(now),
        }
    }

    /// Blocking event loop; returns when all senders are dropped.
    pub fn run(mut self, mut rx: mpsc::UnboundedReceiver<EngineEvent>) {
        while let Some(ev) = rx.blocking_recv() {
            self.handle(ev, Instant::now());
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn client_connected(
        &mut self,
        id: ClientId,
        web: bool,
        client_id: String,
        device: DeviceInfo,
        want_device: Option<String>,
        tx: UnboundedSender<Out>,
        now: Instant,
    ) {
        let def = self
            .rt
            .resolve_device(want_device.as_deref(), client_kind(web, &device));
        tracing::info!(
            "client {client_id} connected ({}x{} @ {}px, device {})",
            device.rows,
            device.cols,
            device.key_size_px,
            def.name
        );
        // Resume the page of the previous connection (still listed if its disconnect has not
        // arrived yet), unless it is not on this device any more or home_after passed.
        let prev = self
            .clients
            .values()
            .find(|c| c.client_id == client_id)
            .map(|c| (c.page.clone(), c.last_press, c.def.name.clone()))
            .or_else(|| self.remembered.remove(&client_id));
        let home = self.rt.home(&def);
        let (page, last_press) = match prev {
            Some((page, at, dev))
                if dev == def.name
                    && def.pages.contains(&page)
                    && !home_due(def.idle.home_after, at, now) =>
            {
                (page, at)
            }
            _ => (home.clone(), now),
        };
        let mut client = Client {
            client_id,
            web,
            keys: (0..device.key_count()).map(|_| Target::new()).collect(),
            cells: Vec::new(),
            device,
            want_device,
            def,
            tx,
            page,
            connected_at: now,
            home,
            last_activity: now,
            last_press,
            power: Power::Active,
            swallow_up: HashSet::new(),
        };
        client.layout(&self.rt);
        let keys: Vec<_> = client.key_ids(id).collect();
        self.clients.insert(id, client);
        self.send_brightness(id);
        self.render(keys);
    }

    /// Sends a client to its device's home page as of now.
    fn go_home(&mut self, id: ClientId) {
        let c = self.clients.get_mut(&id).expect("a connected client");
        c.home = self.rt.home(&c.def);
        let home = c.home.clone();
        self.show_page(id, &home);
    }

    /// A client's display came back from Off. One left on its home page since before the home
    /// changed (a time-of-day `home`) shows the current one.
    fn woke_from_off(&mut self, id: ClientId) {
        let c = &self.clients[&id];
        if c.page == c.home && self.rt.home(&c.def) != c.home {
            self.go_home(id);
        }
    }

    /// Switches a client to `page` if its device includes it.
    fn goto_page(&mut self, id: ClientId, page: &str) {
        let c = &self.clients[&id];
        if !c.def.pages.iter().any(|p| p == page) {
            tracing::warn!("goto_page: page '{page}' is not on device '{}'", c.def.name);
            return;
        }
        self.show_page(id, page);
    }

    /// Switches a client to `page` whatever its device includes (the page preview).
    pub fn show_page(&mut self, id: ClientId, page: &str) {
        let c = self.clients.get_mut(&id).expect("a connected client");
        c.set_page(&self.rt, page);
        let keys = c.key_ids(id).collect();
        self.render(keys);
    }

    fn ha_event(&mut self, e: HaEvent) {
        match e {
            HaEvent::Seed(states) => {
                *self.host.states.borrow_mut() = states
                    .into_iter()
                    .map(|s| (s.entity_id.clone(), s))
                    .collect();
                // Everything may have changed while we were away.
                self.render_all();
            }
            HaEvent::Changed(s) => {
                let id = s.entity_id.clone();
                let prev = self.host.states.borrow_mut().insert(id.clone(), s.clone());
                if prev.as_ref() != Some(&s) {
                    self.dep_changed(&id);
                }
            }
            HaEvent::Removed(id) => {
                if self.host.states.borrow_mut().remove(&id).is_some() {
                    self.dep_changed(&id);
                }
            }
        }
    }

    fn render_all(&mut self) {
        let keys = self.clients.iter().flat_map(|(id, c)| c.key_ids(*id));
        let slots = self
            .plugins
            .iter()
            .flat_map(|(id, p)| p.slots.keys().map(|s| TargetId::Slot(*id, s.clone())));
        let all = keys.chain(slots).collect();
        self.render(all);
    }

    /// Sends the brightness that matches the client's current power state.
    fn send_brightness(&self, id: ClientId) {
        let c = &self.clients[&id];
        let _ = c.tx.send(Out::Brightness(c.def.level(c.power)));
    }

    /// Dims, then switches off, clients that have been idle for the configured time, and sends
    /// them (and forgets disconnected ones) home after `home_after`.
    fn tick(&mut self, now: Instant) {
        self.remembered.retain(|_, (_, at, dev)| {
            !home_due(
                self.rt.device(dev).and_then(|d| d.idle.home_after),
                *at,
                now,
            )
        });
        let home: Vec<_> = self
            .clients
            .iter()
            .filter(|(_, c)| c.page != c.home && home_due(c.def.idle.home_after, c.last_press, now))
            .map(|(id, _)| *id)
            .collect();
        for id in home {
            // Rendered even while the display is off, so waking shows home.
            tracing::info!(client = id, "idle for home_after, going home");
            self.go_home(id);
        }
        let mut changed = Vec::new();
        for (id, c) in &mut self.clients {
            let idle = now.saturating_duration_since(c.last_activity);
            let want = if c.def.idle.off_after.is_some_and(|t| idle >= t) {
                Power::Off
            } else if c.def.idle.dim_after.is_some_and(|t| idle >= t) {
                Power::Dim
            } else {
                Power::Active
            };
            if want != c.power {
                tracing::info!(client = %c.client_id, from = ?c.power, to = ?want, "power");
                c.power = want;
                changed.push(*id);
            }
        }
        for id in changed {
            self.send_brightness(id);
        }
    }

    /// A deck woke from USB suspend. Same as a wake-only press: restore the brightness of an
    /// idle client, restart the idle clocks, run nothing.
    fn wake(&mut self, id: ClientId, now: Instant) {
        let c = self.clients.get_mut(&id).expect("a connected client");
        c.last_activity = now;
        c.last_press = now;
        if c.power != Power::Active {
            tracing::info!(client = %c.client_id, "woken from USB suspend");
            let was_off = c.power == Power::Off;
            c.power = Power::Active;
            if was_off {
                self.woke_from_off(id);
            }
            self.send_brightness(id);
        }
    }

    fn key_event(&mut self, id: ClientId, key: u8, down: bool, now: Instant) {
        let c = self.clients.get_mut(&id).expect("a connected client");
        if down {
            c.last_activity = now;
            c.last_press = now;
            if c.power != Power::Active {
                // The press only wakes the display; it is not a button press.
                tracing::info!(client = %c.client_id, key, "woken by key press");
                let was_off = c.power == Power::Off;
                c.power = Power::Active;
                c.swallow_up.insert(key);
                if was_off {
                    self.woke_from_off(id);
                }
                self.send_brightness(id);
                return;
            }
        } else if c.swallow_up.remove(&key) {
            return;
        }
        tracing::debug!(client = %c.client_id, page = %c.page, key, down, "key event");
        self.press(&TargetId::Key(id, key), down, now);
    }

    #[cfg(test)]
    fn dep_count(&self, entity: &str) -> usize {
        self.deps.reader_count(entity)
    }
}

/// Whether `home_after` has passed since `last_press`.
fn home_due(home_after: Option<Duration>, last_press: Instant, now: Instant) -> bool {
    home_after.is_some_and(|t| now.saturating_duration_since(last_press) >= t)
}

/// Spawns the engine thread (Lua is `!Send`, so it is constructed on that thread); call it
/// inside a tokio runtime. Fails if the config does not load.
pub fn spawn(
    config_path: PathBuf,
    ha_tx: UnboundedSender<HaCommand>,
    ha: HaConfig,
) -> Result<UnboundedSender<EngineEvent>> {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = Handle::current();
    let ticks = tx.downgrade();
    handle.spawn(async move {
        let mut every = tokio::time::interval(TICK);
        loop {
            every.tick().await;
            match ticks.upgrade() {
                Some(tx) if tx.send(EngineEvent::Tick).is_ok() => {}
                _ => break,
            }
        }
    });
    let (started, ready) = std::sync::mpsc::sync_channel(1);
    let events = tx.clone();
    std::thread::Builder::new()
        .name("sdc-engine".into())
        .spawn(
            move || match Engine::new(config_path, ha_tx, Some(ha), handle, &events) {
                Ok(engine) => {
                    drop(events);
                    let _ = started.send(Ok(()));
                    engine.run(rx);
                }
                Err(e) => {
                    let _ = started.send(Err(e));
                }
            },
        )
        .expect("spawn engine thread");
    ready.recv().expect("engine thread reports its start")?;
    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::EntityState;
    use crate::lua_api::DialInput;
    use crate::render::{Encoding, RenderSpec};
    use image::RgbaImage;
    use sdc_protocol::SlotKind;
    use serde_json::json;
    use std::io::Write;
    use tokio::sync::mpsc::UnboundedReceiver;

    const CONFIG: &str = r##"
        sdc.page("home", {
          sdc.button{ render = function()
              local l = ha.state("light.a")
              return { text = "A", bg = (l and l.state == "on") and "#f5b400" or "#222222" }
            end,
            press = function() ha.call("light", "toggle", { entity_id = "light.a" }) end },
          sdc.button{ render = function()
              local l = ha.state("light.b")
              return { text = "B", bg = (l and l.state == "on") and "#f5b400" or "#222222" }
            end },
          sdc.button{ render = { text = "Next" }, press = function(ctx) ctx.goto_page("two") end },
          sdc.button{ render = function() error("nope") end },
        })
        sdc.page("two", { sdc.button{ render = { text = "Back" }, press = function(ctx) ctx.goto_page("home") end } })
    "##;

    fn write_config(dir: &Path, src: &str) -> PathBuf {
        let p = dir.join("config.lua");
        std::fs::File::create(&p)
            .unwrap()
            .write_all(crate::lua_api::test_config(src).as_bytes())
            .unwrap();
        p
    }

    fn device() -> DeviceInfo {
        DeviceInfo {
            model: "t".into(),
            serial: None,
            rows: 3,
            cols: 5,
            key_size_px: 72,
            native: None,
        }
    }

    fn st(id: &str, s: &str) -> EntityState {
        EntityState {
            entity_id: id.into(),
            state: s.into(),
            attributes: json!({}),
        }
    }

    fn drain_out(rx: &mut UnboundedReceiver<Out>) -> Vec<Out> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// Frames only.
    fn drain(rx: &mut UnboundedReceiver<Out>) -> Vec<Frame> {
        drain_out(rx)
            .into_iter()
            .filter_map(|o| match o {
                Out::Frame(f) => Some(f),
                Out::Brightness(_) => None,
            })
            .collect()
    }

    fn brightness(outs: &[Out]) -> Vec<u8> {
        outs.iter()
            .filter_map(|o| match o {
                Out::Brightness(p) => Some(*p),
                Out::Frame(_) => None,
            })
            .collect()
    }

    fn keys(frames: &[Frame]) -> Vec<u8> {
        let mut k: Vec<u8> = frames.iter().map(|f| f.key).collect();
        k.sort_unstable();
        k
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// Minimal self-cleaning temp dir (avoids a dev-dependency).
    mod tempdir {
        use std::path::{Path, PathBuf};
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new() -> Self {
                static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let p = std::env::temp_dir().join(format!(
                    "sdc-test-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    /// An engine on a config in a temp dir. Events are handled `at` whole seconds after `t0`.
    struct Rig {
        e: Engine,
        ha: UnboundedReceiver<HaCommand>,
        dir: tempdir::Dir,
        t0: Instant,
    }

    impl Rig {
        fn without_client(src: &str) -> Self {
            let dir = tempdir::Dir::new();
            let path = write_config(dir.path(), src);
            let (ha_tx, ha) = mpsc::unbounded_channel();
            let (events, _) = mpsc::unbounded_channel();
            let e = Engine::new(path, ha_tx, None, Handle::current(), &events).unwrap();
            Rig {
                e,
                ha,
                dir,
                t0: Instant::now(),
            }
        }

        /// A rig with client 1 ("c1", a 3x5 deck) connected at `t0`.
        fn new(src: &str) -> (Self, UnboundedReceiver<Out>) {
            let mut rig = Self::without_client(src);
            let rx = rig.connect(1, "c1", "t", false, None, 0);
            (rig, rx)
        }

        fn connect(
            &mut self,
            id: ClientId,
            name: &str,
            model: &str,
            web: bool,
            want: Option<&str>,
            at: u64,
        ) -> UnboundedReceiver<Out> {
            let (tx, rx) = mpsc::unbounded_channel();
            self.at(
                EngineEvent::ClientConnected {
                    web,
                    id,
                    client_id: name.into(),
                    device: DeviceInfo {
                        model: model.into(),
                        ..device()
                    },
                    want_device: want.map(Into::into),
                    tx,
                },
                at,
            );
            rx
        }

        fn at(&mut self, ev: EngineEvent, at: u64) {
            self.e.handle(ev, self.t0 + secs(at));
        }

        fn key(&mut self, key: u8, down: bool, at: u64) {
            self.at(EngineEvent::Key { id: 1, key, down }, at);
        }

        fn tick(&mut self, at: u64) {
            self.at(EngineEvent::Tick, at);
        }

        fn ha_calls(&mut self) -> usize {
            std::iter::from_fn(|| self.ha.try_recv().ok()).count()
        }

        fn page(&self, id: ClientId) -> &str {
            &self.e.clients[&id].page
        }

        fn rewrite(&self, src: &str) {
            write_config(self.dir.path(), src);
        }

        fn device_and_page(&self, id: ClientId) -> (&str, &str) {
            let c = &self.e.clients[&id];
            (&c.def.name, &c.page)
        }
    }

    #[tokio::test]
    async fn state_changes_rerender_only_dependents() {
        let (mut r, mut rx) = Rig::new(CONFIG);
        let frames = drain(&mut rx);
        assert_eq!(frames.len(), 15);
        // key 3 errored at connect and shows the error tile
        let err =
            r.e.renderer
                .render(&RenderSpec::error(), 72, Encoding::Jpeg(None));
        assert_eq!(frames[3].jpeg, err);
        assert_eq!(r.e.dep_count("light.a"), 1);

        r.at(EngineEvent::Ha(HaEvent::Changed(st("light.a", "on"))), 1);
        assert_eq!(keys(&drain(&mut rx)), [0]);
        // identical change, unrelated entity, unchanged reload: nothing
        r.at(EngineEvent::Ha(HaEvent::Changed(st("light.a", "on"))), 2);
        r.at(EngineEvent::Ha(HaEvent::Changed(st("light.zzz", "on"))), 3);
        r.at(EngineEvent::Reload, 4);
        assert!(drain(&mut rx).is_empty());
        // a snapshot only sends what looks different
        r.at(EngineEvent::Ha(HaEvent::Seed(vec![st("light.b", "on")])), 5);
        assert_eq!(keys(&drain(&mut rx)), [0, 1]);
    }

    #[tokio::test]
    async fn presses_call_ha_and_switch_pages_and_disconnect_drops_deps() {
        let (mut r, mut rx) = Rig::new(CONFIG);
        r.key(0, true, 1);
        assert!(
            matches!(r.ha.try_recv().unwrap(), HaCommand::CallService { service, .. } if service == "toggle")
        );
        r.key(2, true, 2);
        assert_eq!(r.page(1), "two");
        // light.a is no longer read once the page changed
        assert_eq!(r.e.dep_count("light.a"), 0);
        r.key(0, true, 3);
        assert_eq!(r.page(1), "home");
        drain(&mut rx);
        r.at(EngineEvent::ClientDisconnected(1), 4);
        assert!(r.e.deps.is_empty());
    }

    #[tokio::test]
    async fn reload_keeps_page_and_survives_bad_config() {
        let (mut r, mut rx) = Rig::new(CONFIG);
        r.e.goto_page(1, "two");
        drain(&mut rx);
        // bad config: previous kept
        r.rewrite("this is not lua (");
        r.at(EngineEvent::Reload, 1);
        assert!(r.e.rt.has_page("two"));
        assert!(drain(&mut rx).is_empty());
        // good config that still has "two"
        r.rewrite(
            r##"sdc.page("home", {}) sdc.page("two", { sdc.button{ render = { text = "New" } } })"##,
        );
        r.at(EngineEvent::Reload, 2);
        assert_eq!(r.page(1), "two");
        assert!(!drain(&mut rx).is_empty());
        // config without "two": back to default
        r.rewrite(r##"sdc.page("home", {})"##);
        r.at(EngineEvent::Reload, 3);
        assert_eq!(r.page(1), "home");
    }

    const IDLE_CONFIG: &str = r##"
        sdc.device("d", { pages = { "home" }, brightness = 80,
                          idle = { dim_after = 120, dim_brightness = 10, off_after = 600 } })
        sdc.page("home", {
          sdc.button{ render = { text = "A" },
            press = function() ha.call("light", "toggle", { entity_id = "light.a" }) end,
            release = function() ha.call("light", "turn_off", { entity_id = "light.a" }) end },
        })
    "##;

    #[tokio::test]
    async fn idle_dims_then_turns_off_and_a_press_wakes_without_running() {
        let (mut r, mut rx) = Rig::new(IDLE_CONFIG);
        // the configured brightness is sent on connect
        assert_eq!(brightness(&drain_out(&mut rx)), [80]);
        r.tick(119);
        assert!(drain_out(&mut rx).is_empty());
        r.tick(120);
        assert_eq!(brightness(&drain_out(&mut rx)), [10]);
        r.tick(500); // still dim: nothing more to say
        assert!(drain_out(&mut rx).is_empty());
        r.tick(600);
        assert_eq!(brightness(&drain_out(&mut rx)), [0]);

        // off: the press restores brightness and runs nothing; its release is swallowed too
        r.key(0, true, 1000);
        assert_eq!(brightness(&drain_out(&mut rx)), [80]);
        r.key(0, false, 1000);
        assert_eq!(r.ha_calls(), 0);

        // awake: presses run, and each one restarts the idle clock
        r.key(0, true, 1001);
        r.key(0, false, 1001);
        assert_eq!(r.ha_calls(), 2);
        r.tick(1001 + 119);
        assert!(brightness(&drain_out(&mut rx)).is_empty());
        r.tick(1001 + 120);
        assert_eq!(brightness(&drain_out(&mut rx)), [10]);

        // a wake message does the same without a key
        r.at(EngineEvent::Wake { id: 1 }, 1200);
        assert_eq!(brightness(&drain_out(&mut rx)), [80]);
        assert_eq!(r.ha_calls(), 0);
        // awake already: no brightness message, but the idle clock restarts
        r.at(EngineEvent::Wake { id: 1 }, 1300);
        r.tick(1300 + 119);
        assert!(brightness(&drain_out(&mut rx)).is_empty());
        r.tick(1300 + 120);
        assert_eq!(brightness(&drain_out(&mut rx)), [10]);
    }

    #[tokio::test]
    async fn default_brightness_and_reload_keep_idle_state() {
        let cfg = r##"sdc.page("home", {}) sdc.device("d", { pages = { "home" }, idle = { dim_after = 10 } })"##;
        let (mut r, mut rx) = Rig::new(cfg);
        drain_out(&mut rx);
        r.tick(11);
        assert_eq!(brightness(&drain_out(&mut rx)), [10]);
        // reloading the same config leaves a dimmed client alone
        r.at(EngineEvent::Reload, 12);
        assert!(brightness(&drain_out(&mut rx)).is_empty());
        r.key(0, true, 13);
        assert_eq!(brightness(&drain_out(&mut rx)), [60], "default brightness");
        // dropping the idle setting while dimmed wakes the client on the next tick
        r.tick(30);
        assert_eq!(brightness(&drain_out(&mut rx)), [10]);
        r.rewrite(r##"sdc.page("home", {})"##);
        r.at(EngineEvent::Reload, 31);
        assert!(brightness(&drain_out(&mut rx)).is_empty());
        r.tick(32);
        assert_eq!(brightness(&drain_out(&mut rx)), [60]);
        // a changed brightness reaches an awake client right away
        r.rewrite(
            r##"sdc.page("home", {}) sdc.device("d", { pages = { "home" }, brightness = 30 })"##,
        );
        r.at(EngineEvent::Reload, 33);
        assert_eq!(brightness(&drain_out(&mut rx)), [30]);
    }

    const HOME_CONFIG: &str = r##"
        sdc.device("d", { pages = { "home", "two" },
                          idle = { dim_after = 120, off_after = 600, home_after = 3600 } })
        sdc.page("home", { sdc.button{ render = { text = "H" }, press = function(ctx) ctx.goto_page("two") end } })
        sdc.page("two", { sdc.button{ render = { text = "T" } } })
    "##;

    #[tokio::test]
    async fn page_is_remembered_across_reconnect() {
        let (mut r, rx) = Rig::new(HOME_CONFIG);
        drop(rx);
        r.e.goto_page(1, "two");
        r.at(EngineEvent::ClientDisconnected(1), 0);
        let mut rx2 = r.connect(2, "c1", "t", false, None, 10);
        assert_eq!(r.page(2), "two");
        assert_eq!(drain(&mut rx2).len(), 15);
        // a different client_id starts at home
        r.connect(3, "other", "t", false, None, 10);
        assert_eq!(r.page(3), "home");
        // reconnect before the old disconnect arrived keeps the page, and the stale
        // disconnect does not overwrite the memory
        r.connect(4, "c1", "t", false, None, 11);
        assert_eq!(r.page(4), "two");
        r.at(EngineEvent::ClientDisconnected(2), 12);
        r.at(EngineEvent::ClientDisconnected(4), 13);
        r.connect(5, "c1", "t", false, None, 14);
        assert_eq!(r.page(5), "two");
    }

    #[tokio::test]
    async fn remembered_page_expires_after_home_after_or_when_the_config_drops_it() {
        let (mut r, rx) = Rig::new(HOME_CONFIG);
        drop(rx);
        r.key(0, true, 0); // goes to "two"
        assert_eq!(r.page(1), "two");
        r.at(EngineEvent::ClientDisconnected(1), 1);
        // the press time survives the reconnect: 3599s after it, still "two"
        r.connect(2, "c1", "t", false, None, 3599);
        assert_eq!(r.page(2), "two");
        r.at(EngineEvent::ClientDisconnected(2), 3599);
        r.connect(3, "c1", "t", false, None, 3600);
        assert_eq!(r.page(3), "home");
        // tick forgets expired entries
        r.at(EngineEvent::ClientDisconnected(3), 3600);
        assert!(r.e.remembered.contains_key("c1"));
        r.tick(3600 + 3600);
        assert!(r.e.remembered.is_empty());

        // a page removed by a reload is not resumed
        r.connect(4, "c2", "t", false, None, 8000);
        r.e.goto_page(4, "two");
        r.at(EngineEvent::ClientDisconnected(4), 8001);
        r.rewrite(r##"sdc.page("home", {})"##);
        r.at(EngineEvent::Reload, 8002);
        r.connect(5, "c2", "t", false, None, 8005);
        assert_eq!(r.page(5), "home");
    }

    #[tokio::test]
    async fn home_after_returns_home_even_when_off_and_wakes_restart_the_clock() {
        let (mut r, mut rx) = Rig::new(HOME_CONFIG);
        drain_out(&mut rx);
        r.key(0, true, 0);
        assert_eq!(r.page(1), "two");
        drain_out(&mut rx);
        r.tick(600);
        assert_eq!(brightness(&drain_out(&mut rx)), [0]);
        r.tick(3599);
        assert_eq!(r.page(1), "two");
        r.tick(3600);
        assert_eq!(r.page(1), "home");
        assert!(!drain(&mut rx).is_empty(), "home is rendered while off");
        // waking keeps it on home; the waking press does nothing else
        r.key(0, true, 4000);
        assert_eq!(r.page(1), "home");
        assert_eq!(brightness(&drain_out(&mut rx)), [60]);

        // a wake-only press restarted the home clock
        r.key(0, true, 4001); // -> two
        r.tick(4001 + 700); // off
        r.key(0, true, 5000); // wakes only
        r.key(0, false, 5000);
        assert_eq!(r.page(1), "two");
        r.tick(5000 + 3599);
        assert_eq!(r.page(1), "two");
        r.tick(5000 + 3600);
        assert_eq!(r.page(1), "home");
    }

    const DEVICES_CONFIG: &str = r##"
        sdc.page("home", { sdc.button{ render = { text = "H" }, press = function(ctx) ctx.goto_page("lights") end } })
        sdc.page("lights", { sdc.button{ render = { text = "L" }, press = function(ctx) ctx.goto_page("tv") end } })
        sdc.page("tv", { sdc.button{ render = { text = "T" } } })
        sdc.device("a", { pages = { "home", "lights" }, home = "home",
                          idle = { home_after = 100 } })
        sdc.device("b", { pages = { "tv", "home" }, home = "tv", brightness = 30 })
        sdc.default_device { mk2 = "a", browser = "b", xl = "b" }
    "##;

    #[tokio::test]
    async fn device_resolution_order_and_brightness() {
        let mut r = Rig::without_client(DEVICES_CONFIG);
        let conn = |r: &mut Rig, id, model, web, want| {
            r.connect(id, &format!("c{id}"), model, web, want, 0)
        };
        // explicit name beats the type default
        let mut rx = conn(&mut r, 1, "Mk2", false, Some("b"));
        assert_eq!(r.device_and_page(1), ("b", "tv"));
        assert_eq!(brightness(&drain_out(&mut rx)), [30]);
        // type default
        let mut rx = conn(&mut r, 2, "Mk2Scissor", false, None);
        assert_eq!(r.device_and_page(2), ("a", "home"));
        assert_eq!(brightness(&drain_out(&mut rx)), [60], "default brightness");
        // an unknown name falls through to the type default
        conn(&mut r, 3, "Mk2", false, Some("nope"));
        assert_eq!(r.device_and_page(3), ("a", "home"));
        conn(&mut r, 4, "XlV2", false, None);
        assert_eq!(r.device_and_page(4), ("b", "tv"));
        conn(&mut r, 5, "Web Deck", true, None);
        assert_eq!(r.device_and_page(5), ("b", "tv"));
        // no entry for the type: the first device
        conn(&mut r, 6, "Neo", false, None);
        assert_eq!(r.device_and_page(6), ("a", "home"));
    }

    #[tokio::test]
    async fn device_limits_pages_and_sets_home() {
        let mut r = Rig::without_client(DEVICES_CONFIG);
        let mut rx = r.connect(1, "c1", "Mk2", false, None, 0); // device a
        drain(&mut rx);
        r.key(0, true, 0); // home -> lights
        assert_eq!(r.page(1), "lights");
        drain(&mut rx);
        r.key(0, true, 0); // lights -> tv: not on a
        assert_eq!(r.page(1), "lights");
        assert!(drain(&mut rx).is_empty());
        // home_after 100 leads to the device's home
        r.tick(99);
        assert_eq!(r.page(1), "lights");
        r.tick(100);
        assert_eq!(r.page(1), "home");
        // device b sets no idle: never goes home
        r.connect(2, "c2", "XlV2", false, None, 0);
        r.e.goto_page(2, "home");
        r.tick(100_000);
        assert_eq!(r.page(2), "home");
    }

    #[tokio::test]
    async fn reload_re_resolves_devices_and_a_remembered_page_follows_the_device() {
        let mut r = Rig::without_client(DEVICES_CONFIG);
        r.connect(1, "c1", "Mk2", false, None, 0); // a
        r.e.goto_page(1, "lights");
        // default for mk2 now b: the client moves to b's home
        r.rewrite(&DEVICES_CONFIG.replace(r#"mk2 = "a""#, r#"mk2 = "b""#));
        r.at(EngineEvent::Reload, 0);
        assert_eq!(r.device_and_page(1), ("b", "tv"));
        // same device but the page was dropped from it: home
        r.rewrite(
            &DEVICES_CONFIG
                .replace(r#"mk2 = "a""#, r#"mk2 = "b""#)
                .replace(r#"{ "tv", "home" }"#, r#"{ "home" }"#)
                .replace(r#"home = "tv""#, r#"home = "home""#),
        );
        r.at(EngineEvent::Reload, 0);
        assert_eq!(r.device_and_page(1), ("b", "home"));
        // an invalid device config keeps the old one
        r.rewrite(r#"sdc.page("p", {}) sdc.device("x", { pages = { "q" } })"#);
        r.at(EngineEvent::Reload, 0);
        assert_eq!(r.device_and_page(1), ("b", "home"));

        // a reconnect resumes on the same device, but starts at home on another one
        r.rewrite(DEVICES_CONFIG);
        r.at(EngineEvent::Reload, 1);
        r.e.goto_page(1, "lights");
        r.at(EngineEvent::ClientDisconnected(1), 1);
        r.connect(2, "c1", "Mk2", false, None, 2);
        assert_eq!(r.device_and_page(2), ("a", "lights"));
        r.at(EngineEvent::ClientDisconnected(2), 3);
        r.connect(3, "c1", "Mk2", false, Some("b"), 4);
        assert_eq!(r.device_and_page(3), ("b", "tv"));
    }

    /// Serves a solid red PNG at /ok.png and 404 elsewhere; returns its base URL.
    async fn image_server() -> String {
        let mut png = Vec::new();
        RgbaImage::from_pixel(40, 20, image::Rgba([255, 0, 0, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let app = axum::Router::new().route(
            "/ok.png",
            axum::routing::get(move || {
                let png = png.clone();
                async move { ([(axum::http::header::CONTENT_TYPE, "image/png")], png) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        base
    }

    fn center(jpeg: &[u8]) -> [u8; 3] {
        image::load_from_memory(jpeg)
            .unwrap()
            .to_rgb8()
            .get_pixel(36, 36)
            .0
    }

    async fn next_event(rx: &mut UnboundedReceiver<EngineEvent>) -> EngineEvent {
        tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("timed out waiting for the download")
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_image_renders_placeholder_then_rerenders_when_fetched() {
        let base = image_server().await;
        let dir = tempdir::Dir::new();
        let cfg = write_config(
            dir.path(),
            &format!(
                r##"sdc.page("home", {{
                  sdc.button{{ render = {{ image = "{base}/ok.png", bg = "#000080" }} }},
                  sdc.button{{ render = {{ image = "{base}/missing.png", bg = "#000080" }} }},
                  sdc.button{{ render = {{ text = "plain" }} }},
                }})"##
            ),
        );
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let (ha_tx, _ha) = mpsc::unbounded_channel();
        // loading prefetches both static images
        let mut e = Engine::new(cfg, ha_tx, None, Handle::current(), &ev_tx).unwrap();
        assert_eq!(e.pending_fetches(), 2);

        let (tx, mut rx) = mpsc::unbounded_channel();
        e.handle(
            EngineEvent::ClientConnected {
                web: false,
                id: 1,
                client_id: "c1".into(),
                device: device(),
                want_device: None,
                tx,
            },
            Instant::now(),
        );
        // cache miss: the key renders right away, without the picture
        let first = drain(&mut rx);
        assert_eq!(first.len(), 15);
        let c = center(&first[0].jpeg);
        assert!(c[0] < 10 && c[2] > 100, "background only, got {c:?}");
        assert_eq!(e.dep_count(&format!("img:{base}/ok.png")), 1);

        // each download completes and re-renders only the keys that use it
        for _ in 0..2 {
            let ev = next_event(&mut ev_rx).await;
            assert!(matches!(ev, EngineEvent::ImageFetched { .. }));
            e.handle(ev, Instant::now());
        }
        assert_eq!(e.pending_fetches(), 0);
        let after = drain(&mut rx);
        assert_eq!(keys(&after), [0, 1]);
        let ok = after.iter().find(|f| f.key == 0).unwrap();
        let bad = after.iter().find(|f| f.key == 1).unwrap();
        assert!(center(&ok.jpeg)[0] > 200, "fetched picture fills the key");
        assert!(
            center(&bad.jpeg)[0] > 120,
            "failed fetch shows the error tile"
        );

        // a reload keeps downloaded images but retries failed ones
        e.handle(EngineEvent::Reload, Instant::now());
        assert_eq!(e.pending_fetches(), 1);
        assert_eq!(keys(&drain(&mut rx)), [1]);
    }

    const SPAN_CONFIG: &str = r##"
        sdc.page("music", {
          false, sdc.span{ cols = 2, rows = 2,
            render = function()
              local l = ha.state("light.a")
              return { text = "T", bg = (l and l.state == "on") and "#f5b400" or "#222222" }
            end,
            press = function(ctx)
              ha.call("test", "press", { key = ctx.key, index = ctx.span_index })
            end },
          false, false, sdc.button{ render = { text = "X" } },
          false, false, false, false, false,
          sdc.button{ render = { text = "L" } },
        })
        -- 2 columns at the last column runs off the 5 column grid
        sdc.page("off", { false, false, false, false, sdc.span{ cols = 2, rows = 1, render = { text = "T" } } })
        -- the slot below the origin is not empty
        sdc.page("clash", { sdc.span{ cols = 1, rows = 2, render = { text = "T" } }, false, false, false, false,
                            sdc.button{ render = { text = "B" } } })
    "##;

    #[tokio::test]
    async fn spans_render_once_for_all_keys_and_report_the_pressed_key() {
        let (mut r, mut rx) = Rig::new(SPAN_CONFIG);
        r.e.goto_page(1, "music");
        drain(&mut rx);
        // a 2x2 span at slot 1 covers keys 1, 2, 6, 7
        r.at(EngineEvent::Ha(HaEvent::Changed(st("light.a", "on"))), 0);
        assert_eq!(keys(&drain(&mut rx)), [1, 2, 6, 7]);
        assert_eq!(r.e.dep_count("light.a"), 4);
        r.at(EngineEvent::Ha(HaEvent::Changed(st("light.a", "on"))), 0);
        assert!(drain(&mut rx).is_empty());

        let mut got = Vec::new();
        for key in [1u8, 2, 6, 7] {
            r.key(key, true, 0);
            match r.ha.try_recv().unwrap() {
                HaCommand::CallService { data, .. } => got.push((
                    data["key"].as_u64().unwrap(),
                    data["index"].as_u64().unwrap(),
                )),
            }
        }
        assert_eq!(got, [(1, 0), (2, 1), (6, 2), (7, 3)]);
        // outside the span nothing is called
        r.key(0, true, 0);
        assert_eq!(r.ha_calls(), 0);
    }

    #[tokio::test]
    async fn span_off_grid_or_overlapping_shows_err_on_its_keys() {
        let (mut r, mut rx) = Rig::new(SPAN_CONFIG);
        let err =
            r.e.renderer
                .render(&RenderSpec::error(), 72, Encoding::Jpeg(None));
        drain(&mut rx);
        r.e.goto_page(1, "off");
        let f = drain(&mut rx);
        // only the key that is on the grid exists; it shows ERR, and pressing it does nothing
        assert_eq!(f.iter().find(|f| f.key == 4).unwrap().jpeg, err);
        r.key(4, true, 0);
        assert_eq!(r.ha_calls(), 0);
        r.e.goto_page(1, "clash");
        let f = drain(&mut rx);
        for k in [0u8, 5] {
            assert_eq!(f.iter().find(|f| f.key == k).unwrap().jpeg, err, "key {k}");
        }
    }

    /// Dimmest channel of the top-edge pixel of a key image: the flash ring is white there.
    fn edge(f: &Frame) -> u8 {
        let img = image::load_from_memory(&f.jpeg).unwrap().to_rgb8();
        *img.get_pixel(36, 1).0.iter().min().unwrap()
    }

    const FLASH_PAGE: &str = r#"
        sdc.page("home", {
          sdc.button{ render = { text = "A" }, press = function() end },
          sdc.button{ render = { text = "B" }, press = function() end, feedback = false },
          sdc.button{ render = { text = "C" }, press = function(ctx) ctx.goto_page("two") end },
          sdc.button{ render = { text = "D" }, press = function() error("nope") end },
          sdc.button{ render = { text = "E" } },
        })
        sdc.page("two", {})
    "#;

    #[tokio::test]
    async fn press_flashes_the_key_then_clears_it() {
        let (mut r, mut rx) = Rig::new(FLASH_PAGE);
        drain(&mut rx);
        let redraw = |r: &mut Rig, ms: u64| {
            r.e.handle(
                EngineEvent::Redraw(TargetId::Key(1, 0)),
                r.t0 + Duration::from_millis(ms),
            )
        };
        r.key(0, true, 0);
        let f = drain(&mut rx);
        assert_eq!(keys(&f), [0]);
        assert!(edge(&f[0]) > 200, "ring while flashing");
        redraw(&mut r, 399);
        assert!(drain(&mut rx).is_empty(), "still flashing");
        // nothing else changed: the flash still ends and the key goes back to normal
        redraw(&mut r, 400);
        let f = drain(&mut rx);
        assert_eq!(keys(&f), [0]);
        assert!(edge(&f[0]) < 80, "ring gone");
        redraw(&mut r, 1400);
        assert!(drain(&mut rx).is_empty(), "no frame once settled");
        // a second press restarts the flash
        r.key(0, true, 2);
        assert_eq!(drain(&mut rx).len(), 1);
    }

    #[tokio::test]
    async fn no_flash_for_errors_goto_opt_out_or_missing_press() {
        let (mut r, mut rx) = Rig::new(FLASH_PAGE);
        drain(&mut rx);
        r.key(1, true, 0);
        assert!(drain(&mut rx).is_empty(), "feedback = false");
        r.key(3, true, 0);
        let f = drain(&mut rx);
        assert_eq!(f.len(), 1);
        assert!(edge(&f[0]) < 80, "an ERR tile, not a ring");
        r.key(4, true, 0);
        assert!(drain(&mut rx).is_empty(), "no press handler");
        // goto_page repaints the whole new page without a ring on the pressed key
        r.key(2, true, 0);
        assert!(drain(&mut rx).iter().all(|f| edge(f) < 80));
    }

    #[tokio::test]
    async fn feedback_off_and_wake_presses_do_not_flash() {
        let (mut r, mut rx) = Rig::new(&format!("sdc.feedback(false) {FLASH_PAGE}"));
        drain(&mut rx);
        r.key(0, true, 0);
        assert!(drain(&mut rx).is_empty());

        let cfg = format!(
            r#"{FLASH_PAGE} sdc.device("d", {{ pages = {{ "home", "two" }}, idle = {{ dim_after = 10 }} }})"#
        );
        let (mut r, mut rx) = Rig::new(&cfg);
        r.tick(11);
        drain_out(&mut rx);
        r.key(0, true, 12);
        assert!(drain(&mut rx).is_empty());
    }

    const PLUGIN_CONFIG: &str = r##"
        sdc.page("home", {})
        sdc.export("lamp", sdc.button{
          render = function()
            local l = ha.state("light.a")
            return { text = "Lamp", bg = (l and l.state == "on") and "#f5b400" or "#222222" }
          end,
          press = function() ha.call("light", "toggle", { entity_id = "light.a" }) end })
        local mode = {}
        sdc.export("dimmer", sdc.dial{
          render = function(ctx) return { title = mode[ctx.slot] or "Dim", value = "1", indicator = 5 } end,
          press = function(ctx) mode[ctx.slot] = "Warm" end })
    "##;

    fn subscribe(slot: &str, export: &str, kind: SlotKind) -> PluginClientMessage {
        PluginClientMessage::Subscribe {
            slot: slot.into(),
            export: export.into(),
            kind,
            size_px: 72,
        }
    }

    #[tokio::test]
    async fn plugin_slots_share_the_deck_behaviour() {
        let mut r = Rig::without_client(PLUGIN_CONFIG);
        let (tx, mut rx) = mpsc::unbounded_channel();
        r.at(
            EngineEvent::PluginConnected {
                id: 7,
                client_id: "p".into(),
                tx,
            },
            0,
        );
        let plugin = |r: &mut Rig, msg| r.at(EngineEvent::Plugin { id: 7, msg }, 0);
        let outs = |rx: &mut UnboundedReceiver<PluginOut>| -> Vec<PluginOut> {
            std::iter::from_fn(|| rx.try_recv().ok()).collect()
        };
        assert!(matches!(&outs(&mut rx)[..], [PluginOut::Exports(items)] if items.len() == 2));

        plugin(&mut r, subscribe("k", "lamp", SlotKind::Key));
        plugin(&mut r, subscribe("d", "dimmer", SlotKind::Dial));
        plugin(&mut r, subscribe("bad", "dimmer", SlotKind::Key));
        let o = outs(&mut rx);
        assert!(matches!(&o[0], PluginOut::KeyImage { slot, .. } if slot == "k"));
        assert!(
            matches!(&o[1], PluginOut::DialFeedback { slot, title, .. } if slot == "d" && title == "Dim")
        );
        assert!(matches!(&o[2], PluginOut::SlotError { slot, .. } if slot == "bad"));

        // dependency tracking and change suppression
        assert_eq!(r.e.dep_count("light.a"), 1);
        r.at(EngineEvent::Ha(HaEvent::Changed(st("light.a", "on"))), 1);
        assert!(matches!(&outs(&mut rx)[..], [PluginOut::KeyImage { slot, .. }] if slot == "k"));
        r.at(EngineEvent::Ha(HaEvent::Changed(st("light.a", "on"))), 2);
        assert!(outs(&mut rx).is_empty());

        // a press calls Lua and flashes; the scheduled redraw ends the flash
        plugin(&mut r, PluginClientMessage::SlotDown { slot: "k".into() });
        assert_eq!(r.ha_calls(), 1);
        assert!(matches!(&outs(&mut rx)[..], [PluginOut::KeyImage { .. }]));
        r.at(EngineEvent::Redraw(TargetId::Slot(7, "k".into())), 3);
        assert!(matches!(&outs(&mut rx)[..], [PluginOut::KeyImage { .. }]));

        // a dial handler may change what it renders; input for the wrong kind does nothing
        plugin(&mut r, PluginClientMessage::DialDown { slot: "d".into() });
        assert!(
            matches!(&outs(&mut rx)[..], [PluginOut::DialFeedback { title, .. }] if title == "Warm")
        );
        r.e.turn(7, "k", DialInput::Press);
        assert!(outs(&mut rx).is_empty());

        // unsubscribing and disconnecting forget the dependencies; a late redraw is ignored
        plugin(
            &mut r,
            PluginClientMessage::Unsubscribe { slot: "k".into() },
        );
        assert_eq!(r.e.dep_count("light.a"), 0);
        r.at(EngineEvent::PluginDisconnected(7), 4);
        r.at(EngineEvent::Redraw(TargetId::Slot(7, "d".into())), 5);
        assert!(r.e.deps.is_empty());
    }

    const LONG_PRESS_BUTTON: &str = r##"
        local calls = function(what) return function() ha.call("t", what, {}) end end
        local plain = sdc.button{ render = { text = "P" }, press = calls("plain_press"), release = calls("plain_release") }
        local long = sdc.button{ render = { text = "L" },
          press = calls("press"), release = calls("release"), long_press = calls("long") }
        sdc.page("home", { plain, long, sdc.button{ render = { text = "Go" }, long_press = function(ctx) ctx.goto_page("two") end } })
        sdc.page("two", {})
        sdc.export("long", long)
    "##;

    /// The services Lua called, in order.
    fn ha_services(r: &mut Rig) -> Vec<String> {
        std::iter::from_fn(|| r.ha.try_recv().ok())
            .map(|c| match c {
                HaCommand::CallService { service, .. } => service,
            })
            .collect()
    }

    #[tokio::test]
    async fn long_press_runs_instead_of_press_and_release() {
        let (mut r, _rx) = Rig::new(LONG_PRESS_BUTTON);
        // without a long_press handler nothing changes: press on down, release on up
        r.key(0, true, 1);
        r.key(0, false, 1);
        assert_eq!(ha_services(&mut r), ["plain_press", "plain_release"]);

        // released early: press and release run together on the release
        r.key(1, true, 2);
        assert!(ha_services(&mut r).is_empty());
        r.key(1, false, 2);
        assert_eq!(ha_services(&mut r), ["press", "release"]);
        // the timer of that press fires during the next one: too early to count
        r.key(1, true, 3);
        r.at(EngineEvent::LongPress(TargetId::Key(1, 1)), 3);
        assert!(ha_services(&mut r).is_empty());
        r.key(1, false, 3);
        ha_services(&mut r);

        // still held when the timer fires: the long press runs and the release is swallowed
        r.key(1, true, 10);
        r.at(EngineEvent::LongPress(TargetId::Key(1, 1)), 11);
        assert_eq!(ha_services(&mut r), ["long"]);
        r.key(1, false, 12);
        assert!(ha_services(&mut r).is_empty());
        // and a timer after the release does nothing
        r.at(EngineEvent::LongPress(TargetId::Key(1, 1)), 13);
        assert!(ha_services(&mut r).is_empty());

        // a long press may change the page
        r.key(2, true, 20);
        r.at(EngineEvent::LongPress(TargetId::Key(1, 2)), 21);
        assert_eq!(r.page(1), "two");
        r.key(2, false, 22);

        // a plugin key slot behaves the same
        let (tx, _plugin_rx) = mpsc::unbounded_channel();
        r.at(
            EngineEvent::PluginConnected {
                id: 7,
                client_id: "p".into(),
                tx,
            },
            30,
        );
        let plugin = |r: &mut Rig, msg, at| r.at(EngineEvent::Plugin { id: 7, msg }, at);
        plugin(&mut r, subscribe("k", "long", SlotKind::Key), 30);
        plugin(
            &mut r,
            PluginClientMessage::SlotDown { slot: "k".into() },
            31,
        );
        plugin(&mut r, PluginClientMessage::SlotUp { slot: "k".into() }, 31);
        assert_eq!(ha_services(&mut r), ["press", "release"]);
        plugin(
            &mut r,
            PluginClientMessage::SlotDown { slot: "k".into() },
            40,
        );
        r.at(EngineEvent::LongPress(TargetId::Slot(7, "k".into())), 41);
        plugin(&mut r, PluginClientMessage::SlotUp { slot: "k".into() }, 42);
        assert_eq!(ha_services(&mut r), ["long"]);
    }

    #[tokio::test]
    async fn long_press_does_not_run_when_the_press_only_woke_the_display() {
        let (mut r, _rx) = Rig::new(&format!(
            r##"{LONG_PRESS_BUTTON}
            sdc.device("d", {{ pages = {{ "home", "two" }}, idle = {{ off_after = 10 }} }})"##
        ));
        r.tick(11);
        r.key(1, true, 20);
        r.key(1, false, 20);
        r.at(EngineEvent::LongPress(TargetId::Key(1, 1)), 21);
        assert!(ha_services(&mut r).is_empty());
    }

    #[tokio::test]
    async fn home_function_is_evaluated_whenever_a_client_goes_home() {
        let (mut r, mut rx) = Rig::new(
            r##"
            local evening = false
            sdc.page("morning", {
              sdc.button{ render = { text = "Eve" }, press = function() evening = true end },
              sdc.button{ render = { text = "Away" }, press = function(ctx) ctx.goto_page("other") end },
            })
            sdc.page("tv", { sdc.button{ render = { text = "Away" }, press = function(ctx) ctx.goto_page("other") end } })
            sdc.page("other", {})
            sdc.device("d", {
              pages = { "morning", "tv", "other" },
              home = function() return evening and "tv" or "morning" end,
              idle = { off_after = 10, home_after = 1000 },
            })
            "##,
        );
        drain(&mut rx);
        assert_eq!(r.page(1), "morning");

        // evening comes while the deck sits on the morning home; it keeps it until it wakes
        r.key(0, true, 1);
        assert_eq!(r.page(1), "morning");
        r.tick(12);
        r.tick(13);
        assert_eq!(r.page(1), "morning");
        r.key(0, true, 20);
        assert_eq!(r.page(1), "tv");

        // a deck on another page is not moved by waking, only by home_after
        r.key(0, true, 21);
        assert_eq!(r.page(1), "other");
        r.tick(40);
        r.key(0, true, 41);
        assert_eq!(r.page(1), "other");
        r.tick(41 + 1000);
        assert_eq!(r.page(1), "tv");

        // a new connection starts at the current home
        r.at(EngineEvent::ClientDisconnected(1), 2000);
        let _rx = r.connect(2, "c2", "t", false, None, 2001);
        assert_eq!(r.page(2), "tv");
    }
}
