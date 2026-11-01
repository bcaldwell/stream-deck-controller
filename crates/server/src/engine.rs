//! The engine: owns the Lua state (which is `!Send`), per-client state and the
//! entity -> key dependency map. It runs on a dedicated thread and is driven by
//! [`EngineEvent`]s; rendered frames go out through per-client channels.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Result;
use image::RgbaImage;
use sdc_protocol::DeviceInfo;
use tokio::runtime::Handle;
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::assets::{Assets, Fetcher, S3Config};
use crate::ha::{HaCommand, HaEvent};
use crate::lua_api::{CtxInfo, Host, IdleConfig, LuaRuntime, Settings};
use crate::render::{Color, RenderSpec, Renderer};

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

pub enum EngineEvent {
    ClientConnected {
        id: ClientId,
        client_id: String,
        device: DeviceInfo,
        tx: UnboundedSender<Out>,
    },
    ClientDisconnected(ClientId),
    KeyDown {
        id: ClientId,
        key: u8,
    },
    KeyUp {
        id: ClientId,
        key: u8,
    },
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

struct Client {
    client_id: String,
    device: DeviceInfo,
    tx: UnboundedSender<Out>,
    page: String,
    last_activity: Instant,
    power: Power,
    /// Keys whose press woke the display: their release is swallowed too.
    swallow_up: HashSet<u8>,
    /// Last image bytes sent per key, to suppress identical frames.
    sent: Vec<Option<Vec<u8>>>,
}

pub struct Engine {
    config_path: PathBuf,
    host: Rc<Host>,
    runtime: Option<LuaRuntime>,
    renderer: Renderer,
    clients: BTreeMap<ClientId, Client>,
    key_deps: HashMap<(ClientId, u8), HashSet<String>>,
    /// dependency -> keys whose last render read it: an entity id, or `img:<cache key>` for a
    /// remote image (so keys re-render when their download completes)
    rev_deps: HashMap<String, HashSet<(ClientId, u8)>>,
}

impl Engine {
    pub fn new(config_path: PathBuf, ha_tx: Option<UnboundedSender<HaCommand>>) -> Self {
        let base = config_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        Self {
            config_path,
            host: Host::new(ha_tx),
            runtime: None,
            renderer: Renderer::with_assets(Assets::new(base, S3Config::from_env())),
            clients: BTreeMap::new(),
            key_deps: HashMap::new(),
            rev_deps: HashMap::new(),
        }
    }

    /// Lets the renderer download remote images in the background on `handle`; results come
    /// back as [`EngineEvent::ImageFetched`] on `events` (held weakly, so it does not keep the
    /// event loop alive).
    pub fn enable_fetch(&mut self, handle: Handle, events: &UnboundedSender<EngineEvent>) {
        let events = events.downgrade();
        self.renderer
            .set_fetcher(Some(Fetcher::new(handle, move |(key, result)| {
                if let Some(tx) = events.upgrade() {
                    let _ = tx.send(EngineEvent::ImageFetched { key, result });
                }
            })));
    }

    /// Remote images still downloading.
    pub fn pending_fetches(&self) -> usize {
        self.renderer.pending()
    }

    /// (Re)loads the config. On failure the previous config stays active.
    pub fn reload(&mut self) -> Result<()> {
        let old = self.settings();
        let rt = LuaRuntime::load(&self.config_path, self.host.clone())?;
        tracing::info!(
            "loaded {} (pages: {})",
            self.config_path.display(),
            rt.page_names().join(", ")
        );
        self.runtime = Some(rt);
        self.renderer.clear_icon_cache();
        self.key_deps.clear();
        self.rev_deps.clear();
        let default = self.default_page();
        let rt = self.runtime.as_ref().expect("just set");
        for name in rt.static_image_names() {
            self.renderer.prefetch(&name);
        }
        for c in self.clients.values_mut() {
            if !rt.has_page(&c.page) {
                c.page = default.clone();
            }
        }
        self.render_all_clients();
        // Tell clients about brightness the new config changed; a client is not woken or put
        // to sleep here (the next tick does that if the idle settings now say so).
        let new = self.settings();
        let changed: Vec<_> = self
            .clients
            .iter()
            .filter(|(_, c)| level(&old, c.power) != level(&new, c.power))
            .map(|(id, _)| *id)
            .collect();
        for id in changed {
            self.send_brightness(id);
        }
        Ok(())
    }

    pub fn handle(&mut self, ev: EngineEvent) {
        self.handle_at(ev, Instant::now());
    }

    /// Like [`Engine::handle`] with an explicit clock (tests).
    pub fn handle_at(&mut self, ev: EngineEvent, now: Instant) {
        match ev {
            EngineEvent::ClientConnected {
                id,
                client_id,
                device,
                tx,
            } => {
                tracing::info!(
                    "client {client_id} connected ({}x{} @ {}px)",
                    device.rows,
                    device.cols,
                    device.key_size_px
                );
                let sent = vec![None; device.key_count() as usize];
                let page = self.default_page();
                self.clients.insert(
                    id,
                    Client {
                        client_id,
                        device,
                        tx,
                        page,
                        last_activity: now,
                        power: Power::Active,
                        swallow_up: HashSet::new(),
                        sent,
                    },
                );
                if self.settings().brightness.is_some() {
                    self.send_brightness(id);
                }
                self.render_client(id);
            }
            EngineEvent::ClientDisconnected(id) => {
                if let Some(c) = self.clients.remove(&id) {
                    tracing::info!("client {} disconnected", c.client_id);
                }
                self.drop_deps(|(cid, _)| *cid == id);
            }
            EngineEvent::KeyDown { id, key } => self.key_event(id, key, true, now),
            EngineEvent::KeyUp { id, key } => self.key_event(id, key, false, now),
            EngineEvent::Tick => self.tick(now),
            EngineEvent::Ha(e) => self.ha_event(e),
            EngineEvent::ImageFetched { key, result } => {
                self.renderer.finish_fetch(&key, result);
                self.entity_changed(&format!("img:{key}"));
            }
            EngineEvent::Reload => {
                if let Err(e) = self.reload() {
                    tracing::error!("config reload failed, keeping previous config: {e:#}");
                }
            }
        }
    }

    /// Blocking event loop; returns when all senders are dropped.
    pub fn run(mut self, mut rx: mpsc::UnboundedReceiver<EngineEvent>) {
        while let Some(ev) = rx.blocking_recv() {
            self.handle(ev);
        }
    }

    /// Switches a client to `page` (no-op with a warning if it doesn't exist).
    pub fn goto_page(&mut self, id: ClientId, page: &str) {
        if !self.runtime.as_ref().is_some_and(|rt| rt.has_page(page)) {
            tracing::warn!("goto_page: no such page '{page}'");
            return;
        }
        if let Some(c) = self.clients.get_mut(&id) {
            c.page = page.to_string();
            self.render_client(id);
        }
    }

    fn default_page(&self) -> String {
        self.runtime
            .as_ref()
            .and_then(|rt| rt.default_page())
            .unwrap_or_default()
    }

    fn ha_event(&mut self, e: HaEvent) {
        match e {
            HaEvent::Seed(states) => {
                *self.host.states.borrow_mut() = states
                    .into_iter()
                    .map(|s| (s.entity_id.clone(), s))
                    .collect();
                // Everything may have changed while we were away.
                self.render_all_clients();
            }
            HaEvent::Changed(s) => {
                let id = s.entity_id.clone();
                let prev = self.host.states.borrow_mut().insert(id.clone(), s.clone());
                if prev.as_ref() != Some(&s) {
                    self.entity_changed(&id);
                }
            }
            HaEvent::Removed(id) => {
                if self.host.states.borrow_mut().remove(&id).is_some() {
                    self.entity_changed(&id);
                }
            }
        }
    }

    fn entity_changed(&mut self, entity: &str) {
        let mut keys: Vec<_> = self
            .rev_deps
            .get(entity)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        keys.sort_unstable();
        for chunk in keys.chunk_by(|a, b| a.0 == b.0) {
            let ids: Vec<u8> = chunk.iter().map(|(_, k)| *k).collect();
            self.render_keys(chunk[0].0, &ids);
        }
    }

    fn settings(&self) -> Settings {
        self.runtime
            .as_ref()
            .map(LuaRuntime::settings)
            .unwrap_or_default()
    }

    /// Sends the brightness that matches the client's current power state.
    fn send_brightness(&mut self, id: ClientId) {
        let settings = self.settings();
        if let Some(c) = self.clients.get_mut(&id) {
            let _ = c.tx.send(Out::Brightness(level(&settings, c.power)));
        }
    }

    /// Dims, then switches off, clients that have been idle for the configured time.
    pub fn tick(&mut self, now: Instant) {
        let idle = self.settings().idle;
        let mut changed = Vec::new();
        for (id, c) in self.clients.iter_mut() {
            let want = power_after(&idle, now.saturating_duration_since(c.last_activity));
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

    fn key_event(&mut self, id: ClientId, key: u8, down: bool, now: Instant) {
        let Some(c) = self.clients.get_mut(&id) else {
            return;
        };
        if down {
            c.last_activity = now;
            if c.power != Power::Active {
                // The press only wakes the display; it is not a button press.
                tracing::info!(client = %c.client_id, key, "woken by key press");
                c.power = Power::Active;
                c.swallow_up.insert(key);
                self.send_brightness(id);
                return;
            }
        } else if c.swallow_up.remove(&key) {
            return;
        }
        let Some(c) = self.clients.get(&id) else {
            return;
        };
        let Some(rt) = &self.runtime else { return };
        let Some(btn) = rt.button(&c.page, key) else {
            return;
        };
        let info = ctx_info(c, key);
        tracing::debug!(client = %c.client_id, page = %c.page, key, down, "key event");
        match rt.press(&btn, &info, down) {
            Ok(Some(page)) => self.goto_page(id, &page),
            Ok(None) => {}
            Err(e) => {
                tracing::error!(
                    "{} key {key} {}: {e:#}",
                    info.page,
                    if down { "press" } else { "release" }
                );
                let jpeg = self
                    .renderer
                    .render(&RenderSpec::error(), c.device.key_size_px);
                self.send_if_changed(id, key, jpeg);
            }
        }
    }

    fn render_all_clients(&mut self) {
        let ids: Vec<_> = self.clients.keys().copied().collect();
        for id in ids {
            self.render_client(id);
        }
    }

    fn render_client(&mut self, id: ClientId) {
        let Some(c) = self.clients.get(&id) else {
            return;
        };
        let keys: Vec<u8> = (0..c.device.key_count()).collect();
        self.render_keys(id, &keys);
    }

    /// Renders `keys` of a client: Lua render functions run one after the other on this thread,
    /// then the resulting descriptors are rasterized and encoded in parallel.
    fn render_keys(&mut self, id: ClientId, keys: &[u8]) {
        let Some(c) = self.clients.get(&id) else {
            return;
        };
        let started = Instant::now();
        let size = c.device.key_size_px;
        let mut jobs = Vec::with_capacity(keys.len());
        let mut all_deps = Vec::with_capacity(keys.len());
        for &key in keys {
            let found = self
                .runtime
                .as_ref()
                .and_then(|rt| rt.button(&c.page, key).map(|b| (rt, b)));
            let (spec, deps) = match found {
                Some((rt, btn)) => {
                    let info = ctx_info(c, key);
                    let out = rt.render(&btn, &info);
                    if let Some(e) = &out.error {
                        tracing::error!("render {} key {key} for {}: {e}", info.page, c.client_id);
                    }
                    let mut deps = out.deps;
                    deps.extend(
                        self.renderer
                            .remote_keys(&out.spec)
                            .into_iter()
                            .map(|k| format!("img:{k}")),
                    );
                    (out.spec, deps)
                }
                None => (
                    RenderSpec {
                        bg: Color::BLACK,
                        ..RenderSpec::default()
                    },
                    HashSet::new(),
                ),
            };
            jobs.push(self.renderer.prepare(&spec, size));
            all_deps.push(deps);
        }
        let page = c.page.clone();
        let jpegs = self.renderer.encode_all(jobs);
        tracing::debug!(
            client = id,
            page,
            keys = keys.len(),
            ms = started.elapsed().as_secs_f64() * 1000.0,
            "rendered keys"
        );
        for ((&key, deps), jpeg) in keys.iter().zip(all_deps).zip(jpegs) {
            self.set_deps(id, key, deps);
            self.send_if_changed(id, key, jpeg);
        }
    }

    fn send_if_changed(&mut self, id: ClientId, key: u8, jpeg: Vec<u8>) {
        let Some(c) = self.clients.get_mut(&id) else {
            return;
        };
        let Some(slot) = c.sent.get_mut(key as usize) else {
            return;
        };
        if slot.as_ref() == Some(&jpeg) {
            return;
        }
        *slot = Some(jpeg.clone());
        // A closed receiver just means the client is on its way out.
        let _ = c.tx.send(Out::Frame(Frame { key, jpeg }));
    }

    fn set_deps(&mut self, id: ClientId, key: u8, deps: HashSet<String>) {
        if let Some(old) = self.key_deps.remove(&(id, key)) {
            for e in old {
                if let Some(set) = self.rev_deps.get_mut(&e) {
                    set.remove(&(id, key));
                }
            }
        }
        for e in &deps {
            self.rev_deps
                .entry(e.clone())
                .or_default()
                .insert((id, key));
        }
        if !deps.is_empty() {
            self.key_deps.insert((id, key), deps);
        }
    }

    fn drop_deps(&mut self, pred: impl Fn(&(ClientId, u8)) -> bool) {
        self.key_deps.retain(|k, _| !pred(k));
        for set in self.rev_deps.values_mut() {
            set.retain(|k| !pred(k));
        }
        self.rev_deps.retain(|_, s| !s.is_empty());
    }

    #[cfg(test)]
    fn dep_count(&self, entity: &str) -> usize {
        self.rev_deps.get(entity).map_or(0, HashSet::len)
    }
}

/// The power state a client is in after being idle for `idle`.
fn power_after(cfg: &IdleConfig, idle: Duration) -> Power {
    if cfg.off_after.is_some_and(|t| idle >= t) {
        Power::Off
    } else if cfg.dim_after.is_some_and(|t| idle >= t) {
        Power::Dim
    } else {
        Power::Active
    }
}

/// Brightness percent to show in a power state.
fn level(settings: &Settings, power: Power) -> u8 {
    match power {
        Power::Active => settings.active_brightness(),
        Power::Dim => settings.idle.dim_brightness,
        Power::Off => 0,
    }
}

fn ctx_info(c: &Client, key: u8) -> CtxInfo {
    CtxInfo {
        client_id: c.client_id.clone(),
        key,
        page: c.page.clone(),
        rows: c.device.rows,
        cols: c.device.cols,
    }
}

/// Spawns the engine thread (Lua is `!Send`, so it is constructed on that thread).
/// A failing initial config load is logged; the engine keeps running so a
/// subsequent hot reload can fix it.
pub fn spawn(
    config_path: PathBuf,
    ha_tx: Option<UnboundedSender<HaCommand>>,
) -> UnboundedSender<EngineEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = Handle::try_current().ok();
    let events = tx.clone();
    if let Some(h) = &handle {
        let ticks = tx.downgrade();
        h.spawn(async move {
            let mut every = tokio::time::interval(TICK);
            loop {
                every.tick().await;
                match ticks.upgrade() {
                    Some(tx) if tx.send(EngineEvent::Tick).is_ok() => {}
                    _ => break,
                }
            }
        });
    }
    std::thread::Builder::new()
        .name("sdc-engine".into())
        .spawn(move || {
            let mut engine = Engine::new(config_path, ha_tx);
            if let Some(h) = handle {
                engine.enable_fetch(h, &events);
            }
            drop(events);
            if let Err(e) = engine.reload() {
                tracing::error!("failed to load config: {e:#}");
            }
            engine.run(rx);
        })
        .expect("spawn engine thread");
    tx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::EntityState;
    use image::RgbaImage;
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
            .write_all(src.as_bytes())
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

    fn setup(
        src: &str,
    ) -> (
        Engine,
        UnboundedReceiver<Out>,
        UnboundedReceiver<HaCommand>,
        tempdir::Dir,
    ) {
        let dir = tempdir::Dir::new();
        let path = write_config(dir.path(), src);
        let (ha_tx, ha_rx) = mpsc::unbounded_channel();
        let mut e = Engine::new(path, Some(ha_tx));
        e.reload().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        e.handle(EngineEvent::ClientConnected {
            id: 1,
            client_id: "c1".into(),
            device: device(),
            tx,
        });
        assert_eq!(drain(&mut rx).len(), 15);
        (e, rx, ha_rx, dir)
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

    #[test]
    fn state_change_rerenders_only_dependents() {
        let (mut e, mut rx, _ha, _d) = setup(CONFIG);
        assert_eq!(e.dep_count("light.a"), 1);
        e.handle(EngineEvent::Ha(HaEvent::Changed(st("light.a", "on"))));
        let frames = drain(&mut rx);
        assert_eq!(frames.iter().map(|f| f.key).collect::<Vec<_>>(), [0]);
        // identical change: no frame
        e.handle(EngineEvent::Ha(HaEvent::Changed(st("light.a", "on"))));
        assert!(drain(&mut rx).is_empty());
        // unrelated entity: no frame
        e.handle(EngineEvent::Ha(HaEvent::Changed(st("light.zzz", "on"))));
        assert!(drain(&mut rx).is_empty());
        // light.b -> key 1 only
        e.handle(EngineEvent::Ha(HaEvent::Changed(st("light.b", "on"))));
        assert_eq!(
            drain(&mut rx).iter().map(|f| f.key).collect::<Vec<_>>(),
            [1]
        );
    }

    #[test]
    fn seed_only_sends_visibly_changed_keys() {
        let (mut e, mut rx, _ha, _d) = setup(CONFIG);
        e.handle(EngineEvent::Ha(HaEvent::Seed(vec![st("light.b", "on")])));
        assert_eq!(
            drain(&mut rx).iter().map(|f| f.key).collect::<Vec<_>>(),
            [1]
        );
    }

    #[test]
    fn error_tile_on_render_error() {
        let (mut e, mut rx, _ha, d) = setup(CONFIG);
        // key 3 errored at connect; its frame equals the error tile
        let mut r = Renderer::new(d.path().to_path_buf());
        let err = r.render(&RenderSpec::error(), 72);
        e.handle(EngineEvent::Reload);
        assert!(drain(&mut rx).is_empty()); // unchanged => nothing re-sent
        let c = e.clients.get(&1).unwrap();
        assert_eq!(c.sent[3].as_ref().unwrap(), &err);
    }

    #[test]
    fn press_calls_ha_and_goto_switches_page() {
        let (mut e, mut rx, mut ha, _d) = setup(CONFIG);
        e.handle(EngineEvent::KeyDown { id: 1, key: 0 });
        assert!(
            matches!(ha.try_recv().unwrap(), HaCommand::CallService { service, .. } if service == "toggle")
        );
        e.handle(EngineEvent::KeyDown { id: 1, key: 2 });
        assert_eq!(e.clients[&1].page, "two");
        let frames = drain(&mut rx);
        assert!(!frames.is_empty());
        // light.a dependency gone with the page change
        assert_eq!(e.dep_count("light.a"), 0);
        e.handle(EngineEvent::KeyDown { id: 1, key: 0 });
        assert_eq!(e.clients[&1].page, "home");
    }

    #[test]
    fn disconnect_drops_deps() {
        let (mut e, _rx, _ha, _d) = setup(CONFIG);
        e.handle(EngineEvent::ClientDisconnected(1));
        assert_eq!(e.dep_count("light.a"), 0);
        assert!(e.key_deps.is_empty());
    }

    #[test]
    fn reload_keeps_page_and_survives_bad_config() {
        let (mut e, mut rx, _ha, d) = setup(CONFIG);
        e.goto_page(1, "two");
        drain(&mut rx);
        // bad config: previous kept
        write_config(d.path(), "this is not lua (");
        e.handle(EngineEvent::Reload);
        assert!(e.runtime.as_ref().unwrap().has_page("two"));
        assert!(drain(&mut rx).is_empty());
        // good config that still has "two"
        write_config(
            d.path(),
            r##"sdc.page("home", {}) sdc.page("two", { sdc.button{ render = { text = "New" } } })"##,
        );
        e.handle(EngineEvent::Reload);
        assert_eq!(e.clients[&1].page, "two");
        assert!(!drain(&mut rx).is_empty());
        // config without "two": back to default
        write_config(d.path(), r##"sdc.page("home", {})"##);
        e.handle(EngineEvent::Reload);
        assert_eq!(e.clients[&1].page, "home");
    }

    const IDLE_CONFIG: &str = r##"
        sdc.idle { dim_after = 120, dim_brightness = 10, off_after = 600 }
        sdc.brightness(80)
        sdc.page("home", {
          sdc.button{ render = { text = "A" },
            press = function() ha.call("light", "toggle", { entity_id = "light.a" }) end,
            release = function() ha.call("light", "turn_off", { entity_id = "light.a" }) end },
        })
    "##;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn idle_dims_then_turns_off() {
        let (mut e, rx, _ha, _d) = setup(IDLE_CONFIG);
        drop(rx);
        // reconnect so the idle clock starts at a known instant
        let t0 = Instant::now();
        let (tx, mut rx2) = mpsc::unbounded_channel();
        e.handle_at(
            EngineEvent::ClientConnected {
                id: 2,
                client_id: "c2".into(),
                device: device(),
                tx,
            },
            t0,
        );
        // the configured brightness is sent on connect
        assert_eq!(brightness(&drain_out(&mut rx2)), [80]);
        e.tick(t0 + secs(119));
        assert!(drain_out(&mut rx2).is_empty());
        e.tick(t0 + secs(120));
        assert_eq!(brightness(&drain_out(&mut rx2)), [10]);
        e.tick(t0 + secs(500)); // still dim: nothing more to say
        assert!(drain_out(&mut rx2).is_empty());
        e.tick(t0 + secs(600));
        assert_eq!(brightness(&drain_out(&mut rx2)), [0]);
        e.tick(t0 + secs(9000));
        assert!(drain_out(&mut rx2).is_empty());
    }

    #[test]
    fn key_press_wakes_and_is_consumed() {
        let (mut e, mut rx, mut ha, _d) = setup(IDLE_CONFIG);
        drain_out(&mut rx);
        let t0 = Instant::now();
        let down = |e: &mut Engine, t| e.handle_at(EngineEvent::KeyDown { id: 1, key: 0 }, t);
        let up = |e: &mut Engine, t| e.handle_at(EngineEvent::KeyUp { id: 1, key: 0 }, t);
        let calls = |ha: &mut UnboundedReceiver<HaCommand>| {
            std::iter::from_fn(|| ha.try_recv().ok()).count()
        };

        // dimmed: the press restores brightness, runs nothing; its release is swallowed too
        e.tick(t0 + secs(130));
        assert_eq!(brightness(&drain_out(&mut rx)), [10]);
        down(&mut e, t0 + secs(131));
        assert_eq!(brightness(&drain_out(&mut rx)), [80]);
        up(&mut e, t0 + secs(131));
        assert_eq!(calls(&mut ha), 0);

        // awake: presses run, and each one restarts the idle clock
        down(&mut e, t0 + secs(200));
        up(&mut e, t0 + secs(200));
        assert_eq!(calls(&mut ha), 2);
        e.tick(t0 + secs(200 + 119));
        assert!(drain_out(&mut rx).is_empty());
        e.tick(t0 + secs(200 + 120));
        assert_eq!(brightness(&drain_out(&mut rx)), [10]);

        // off: same
        e.tick(t0 + secs(200 + 600));
        assert_eq!(brightness(&drain_out(&mut rx)), [0]);
        down(&mut e, t0 + secs(1000));
        assert_eq!(brightness(&drain_out(&mut rx)), [80]);
        up(&mut e, t0 + secs(1000));
        assert_eq!(calls(&mut ha), 0);
        down(&mut e, t0 + secs(1001));
        assert_eq!(calls(&mut ha), 1);
    }

    #[test]
    fn without_idle_config_no_brightness_messages() {
        let (mut e, mut rx, mut ha, _d) = setup(CONFIG);
        let t0 = Instant::now();
        e.tick(t0 + secs(100_000));
        e.handle_at(EngineEvent::KeyDown { id: 1, key: 0 }, t0 + secs(100_001));
        assert!(brightness(&drain_out(&mut rx)).is_empty());
        assert!(ha.try_recv().is_ok(), "the press runs normally");
    }

    #[test]
    fn default_wake_brightness_and_reload_keep_idle_state() {
        let cfg = r##"sdc.idle { dim_after = 10 } sdc.page("home", {})"##;
        let (mut e, mut rx, _ha, d) = setup(cfg);
        assert!(
            drain_out(&mut rx).is_empty(),
            "no sdc.brightness: nothing on connect"
        );
        let t0 = Instant::now();
        e.tick(t0 + secs(11));
        assert_eq!(brightness(&drain_out(&mut rx)), [10]);
        // reloading the same config leaves a dimmed client alone
        e.handle_at(EngineEvent::Reload, t0 + secs(12));
        assert!(brightness(&drain_out(&mut rx)).is_empty());
        e.handle_at(EngineEvent::KeyDown { id: 1, key: 0 }, t0 + secs(13));
        assert_eq!(
            brightness(&drain_out(&mut rx)),
            [60],
            "default active brightness"
        );
        // dropping sdc.idle while dimmed wakes the client on the next tick
        e.tick(t0 + secs(30));
        assert_eq!(brightness(&drain_out(&mut rx)), [10]);
        write_config(d.path(), r##"sdc.page("home", {})"##);
        e.handle_at(EngineEvent::Reload, t0 + secs(31));
        assert!(brightness(&drain_out(&mut rx)).is_empty());
        e.tick(t0 + secs(32));
        assert_eq!(brightness(&drain_out(&mut rx)), [60]);
        // a changed sdc.brightness reaches an awake client right away
        write_config(d.path(), r##"sdc.brightness(30) sdc.page("home", {})"##);
        e.handle_at(EngineEvent::Reload, t0 + secs(33));
        assert_eq!(brightness(&drain_out(&mut rx)), [30]);
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
        let mut e = Engine::new(cfg, None);
        e.enable_fetch(Handle::current(), &ev_tx);
        e.reload().unwrap(); // prefetches both static images
        assert_eq!(e.pending_fetches(), 2);

        let (tx, mut rx) = mpsc::unbounded_channel();
        e.handle(EngineEvent::ClientConnected {
            id: 1,
            client_id: "c1".into(),
            device: device(),
            tx,
        });
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
            e.handle(ev);
        }
        assert_eq!(e.pending_fetches(), 0);
        let after = drain(&mut rx);
        let mut keys: Vec<_> = after.iter().map(|f| f.key).collect();
        keys.sort_unstable();
        assert_eq!(keys, [0, 1]);
        let ok = after.iter().find(|f| f.key == 0).unwrap();
        let bad = after.iter().find(|f| f.key == 1).unwrap();
        assert!(center(&ok.jpeg)[0] > 200, "fetched picture fills the key");
        assert!(
            center(&bad.jpeg)[0] > 120,
            "failed fetch shows the error tile"
        );

        // a reload keeps downloaded images but retries failed ones
        e.handle(EngineEvent::Reload);
        assert_eq!(e.pending_fetches(), 1);
        let resent = drain(&mut rx);
        assert_eq!(resent.iter().map(|f| f.key).collect::<Vec<_>>(), [1]);
    }
}
