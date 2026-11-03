//! The engine: owns the Lua state (which is `!Send`), per-client state and the
//! entity -> key dependency map. It runs on a dedicated thread and is driven by
//! [`EngineEvent`]s; rendered frames go out through per-client channels.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Result;
use image::RgbaImage;
use sdc_protocol::{device_type, DeviceInfo, PluginClientMessage};
use tokio::runtime::Handle;
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::assets::{Assets, Fetcher, HaApi, S3Config};
use crate::ha::{HaCommand, HaEvent};
use crate::lua_api::{Cell, CtxInfo, DeviceDef, Host, IdleConfig, LuaRuntime, SpanCell};
use crate::render::{Color, RenderSpec, Renderer, SpanGeom};

mod plugin;

pub use plugin::PluginOut;
use plugin::{Plugin, SlotDeps};

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

/// A configured device, for the browser deck's layout choice (`GET /deck/api/devices`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DeviceListing {
    pub name: String,
    pub pages: Vec<String>,
    pub home: String,
}

/// How often the engine checks clients for idleness.
const TICK: Duration = Duration::from_secs(1);
/// How long a key shows the press flash.
const FLASH: Duration = Duration::from_millis(400);

/// Display state of a client; only ever left by a tick (dim/off) or a key press (wake).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Power {
    Active,
    Dim,
    Off,
}

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
    /// Redraw one plugin slot (a dial whose turn has settled, see [`crate::lua_api::DIAL_SETTLE`]).
    RedrawSlot {
        id: ClientId,
        slot: String,
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
    /// The config's `sdc.device`s, for the browser deck's layout choice.
    ListDevices(tokio::sync::oneshot::Sender<Vec<DeviceListing>>),
    KeyDown {
        id: ClientId,
        key: u8,
    },
    KeyUp {
        id: ClientId,
        key: u8,
    },
    /// The client woke from USB suspend: a wake-only key press (no button runs).
    Wake {
        id: ClientId,
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

/// What is kept of a client that went away: page, last key press, device name.
type Remembered = (String, Instant, String);

/// What a client's `sdc.device` allows.
#[derive(Clone)]
struct Policy {
    /// Start page, also where `home_after` leads.
    home: String,
    /// Pages the client may show.
    pages: Vec<String>,
    idle: IdleConfig,
    brightness: u8,
    /// Name of the `sdc.device`.
    device: String,
}

impl Policy {
    fn allows(&self, page: &str) -> bool {
        self.pages.iter().any(|n| n == page)
    }

    fn level(&self, power: Power) -> u8 {
        match power {
            Power::Active => self.brightness,
            Power::Dim => self.idle.dim_brightness,
            Power::Off => 0,
        }
    }
}

impl From<DeviceDef> for Policy {
    fn from(d: DeviceDef) -> Self {
        Policy {
            home: d.home,
            pages: d.pages,
            idle: d.idle,
            brightness: d.brightness,
            device: d.name,
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
    policy: Policy,
    tx: UnboundedSender<Out>,
    page: String,
    /// Last key press (or connect): drives dim/off.
    last_activity: Instant,
    /// Last key press, carried across reconnects: drives `home_after`.
    last_press: Instant,
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
    /// Page and last key press of clients that went away, by client_id, so a reconnect (deploy,
    /// client update) resumes where they were. Expires after the device's `home_after`; lost on
    /// restart.
    remembered: HashMap<String, Remembered>,
    key_deps: HashMap<(ClientId, u8), HashSet<String>>,
    /// dependency -> keys whose last render read it: an entity id, or `img:<cache key>` for a
    /// remote image (so keys re-render when their download completes)
    rev_deps: HashMap<String, HashSet<(ClientId, u8)>>,
    /// Elgato plugin connections and the slots they subscribed.
    plugins: BTreeMap<ClientId, Plugin>,
    /// The same dependency tracking as `key_deps`/`rev_deps`, for plugin slots.
    slot_deps: SlotDeps,
    /// Keys and plugin slots showing the press flash, with when it ends.
    flashes: HashMap<(ClientId, FlashKey), Instant>,
    /// Wakes the event loop when a flash is due (set by [`Engine::enable_fetch`]).
    timer: Option<(Handle, mpsc::WeakUnboundedSender<EngineEvent>)>,
}

/// What a press flash is on: a key of a client, or a plugin slot.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FlashKey {
    Key(u8),
    Slot(String),
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
            renderer: Renderer::with_assets(
                Assets::new(base, S3Config::from_env()).with_ha(HaApi::from_env()),
            ),
            clients: BTreeMap::new(),
            remembered: HashMap::new(),
            key_deps: HashMap::new(),
            rev_deps: HashMap::new(),
            plugins: BTreeMap::new(),
            slot_deps: SlotDeps::default(),
            flashes: HashMap::new(),
            timer: None,
        }
    }

    /// Lets the renderer download remote images in the background on `handle`; results come
    /// back as [`EngineEvent::ImageFetched`] on `events` (held weakly, so it does not keep the
    /// event loop alive).
    pub fn enable_fetch(&mut self, handle: Handle, events: &UnboundedSender<EngineEvent>) {
        self.timer = Some((handle.clone(), events.downgrade()));
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
        let rt = LuaRuntime::load(&self.config_path, self.host.clone())?;
        tracing::info!(
            "loaded {} (pages: {})",
            self.config_path.display(),
            rt.page_names().join(", ")
        );
        let rt = &*self.runtime.insert(rt);
        self.renderer.clear_icon_cache();
        self.key_deps.clear();
        self.rev_deps.clear();
        self.slot_deps.clear();
        for name in rt.static_image_names() {
            self.renderer.prefetch(&name);
        }
        // Re-resolve every deck's device; a changed device or a page it no longer allows sends
        // the deck to its (new) home. A client is not woken or put to sleep here (the next tick
        // does that if the idle settings now say so), but is told about a changed brightness.
        let mut rebrightened = Vec::new();
        for (id, c) in &mut self.clients {
            let policy = Policy::from(
                rt.resolve_device(c.want_device.as_deref(), client_kind(c.web, &c.device)),
            );
            if c.policy.device != policy.device || !rt.has_page(&c.page) || !policy.allows(&c.page)
            {
                c.page = policy.home.clone();
            }
            if c.policy.level(c.power) != policy.level(c.power) {
                rebrightened.push(*id);
            }
            c.policy = policy;
        }
        self.push_exports();
        self.render_all_clients();
        for id in rebrightened {
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
                web,
                id,
                client_id,
                device,
                want_device,
                tx,
            } => {
                let Some(rt) = &self.runtime else {
                    tracing::warn!("client {client_id} connected before a config was loaded");
                    return;
                };
                let policy = Policy::from(
                    rt.resolve_device(want_device.as_deref(), client_kind(web, &device)),
                );
                tracing::info!(
                    "client {client_id} connected ({}x{} @ {}px, device {})",
                    device.rows,
                    device.cols,
                    device.key_size_px,
                    policy.device
                );
                let sent = vec![None; device.key_count() as usize];
                // Resume the page of the previous connection (still listed if its disconnect
                // has not arrived yet), unless it is gone from the config, not on this
                // device, or home_after passed.
                let prev = self
                    .clients
                    .values()
                    .find(|c| c.client_id == client_id)
                    .map(|c| (c.page.clone(), c.last_press, c.policy.device.clone()))
                    .or_else(|| self.remembered.remove(&client_id));
                let (page, last_press) = match prev {
                    Some((page, at, dev))
                        if dev == policy.device
                            && rt.has_page(&page)
                            && policy.allows(&page)
                            && !home_due(policy.idle.home_after, at, now) =>
                    {
                        (page, at)
                    }
                    _ => (policy.home.clone(), now),
                };
                self.clients.insert(
                    id,
                    Client {
                        client_id: client_id.clone(),
                        web,
                        device,
                        want_device,
                        policy,
                        tx,
                        page,
                        last_activity: now,
                        last_press,
                        power: Power::Active,
                        swallow_up: HashSet::new(),
                        sent,
                    },
                );
                self.send_brightness(id);
                self.render_client(id);
            }
            EngineEvent::PluginConnected { id, client_id, tx } => {
                self.plugin_connected(id, client_id, tx)
            }
            EngineEvent::PluginDisconnected(id) => self.plugin_disconnected(id),
            EngineEvent::RedrawSlot { id, slot } => self.render_slot(id, &slot),
            EngineEvent::Plugin { id, msg } => self.plugin_message(id, msg),
            EngineEvent::ListDevices(reply) => {
                let _ = reply.send(
                    self.runtime
                        .as_ref()
                        .map(|rt| {
                            rt.devices()
                                .into_iter()
                                .map(|d| DeviceListing {
                                    name: d.name,
                                    pages: d.pages,
                                    home: d.home,
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                );
            }
            EngineEvent::ClientDisconnected(id) => {
                if let Some(c) = self.clients.remove(&id) {
                    tracing::info!("client {} disconnected", c.client_id);
                    // A newer connection of the same client already took over its page.
                    if !self.clients.values().any(|o| o.client_id == c.client_id) {
                        self.remembered
                            .insert(c.client_id, (c.page, c.last_press, c.policy.device));
                    }
                }
                self.drop_deps(|(cid, _)| *cid == id);
            }
            EngineEvent::KeyDown { id, key } => self.key_event(id, key, true, now),
            EngineEvent::KeyUp { id, key } => self.key_event(id, key, false, now),
            EngineEvent::Wake { id } => self.wake(id, now),
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

    /// Switches a client to `page` (no-op with a warning if it doesn't exist or its device does
    /// not include it).
    pub fn goto_page(&mut self, id: ClientId, page: &str) {
        if let Some(c) = self.clients.get(&id) {
            if !c.policy.allows(page) {
                tracing::warn!(
                    "goto_page: page '{page}' is not on device '{}'",
                    c.policy.device
                );
                return;
            }
        }
        self.show_page(id, page);
    }

    /// Like [`Engine::goto_page`] but ignoring the client's device (the page preview).
    pub fn show_page(&mut self, id: ClientId, page: &str) {
        if !self.runtime.as_ref().is_some_and(|rt| rt.has_page(page)) {
            tracing::warn!("goto_page: no such page '{page}'");
            return;
        }
        if let Some(c) = self.clients.get_mut(&id) {
            c.page = page.to_string();
            self.flashes
                .retain(|(cid, k), _| *cid != id || !matches!(k, FlashKey::Key(_)));
            self.render_client(id);
        }
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
        self.slots_depending_on(entity);
    }

    fn feedback(&self) -> bool {
        self.runtime
            .as_ref()
            .is_some_and(|rt| rt.settings().feedback)
    }

    /// Sends the brightness that matches the client's current power state.
    fn send_brightness(&mut self, id: ClientId) {
        let Some(c) = self.clients.get(&id) else {
            return;
        };
        let _ = c.tx.send(Out::Brightness(c.policy.level(c.power)));
    }

    /// Dims, then switches off, clients that have been idle for the configured time, and sends
    /// them (and forgets disconnected ones) home after `home_after`.
    pub fn tick(&mut self, now: Instant) {
        self.end_flashes(now);
        let Some(rt) = &self.runtime else { return };
        self.remembered.retain(|_, (_, at, dev)| {
            !home_due(rt.device(dev).and_then(|d| d.idle.home_after), *at, now)
        });
        let home: Vec<_> = self
            .clients
            .iter()
            .filter(|(_, c)| {
                let p = &c.policy;
                c.page != p.home && home_due(p.idle.home_after, c.last_press, now)
            })
            .map(|(id, c)| (*id, c.policy.home.clone()))
            .collect();
        for (id, page) in home {
            // Rendered even while the display is off, so waking shows home.
            tracing::info!(client = id, page = %page, "idle for home_after, going home");
            self.goto_page(id, &page);
        }
        let mut changed = Vec::new();
        for (id, c) in &mut self.clients {
            let want = power_after(
                &c.policy.idle,
                now.saturating_duration_since(c.last_activity),
            );
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
        let Some(c) = self.clients.get_mut(&id) else {
            return;
        };
        c.last_activity = now;
        c.last_press = now;
        if c.power != Power::Active {
            tracing::info!(client = %c.client_id, "woken from USB suspend");
            c.power = Power::Active;
            self.send_brightness(id);
        }
    }

    fn key_event(&mut self, id: ClientId, key: u8, down: bool, now: Instant) {
        let Some(c) = self.clients.get_mut(&id) else {
            return;
        };
        if down {
            c.last_activity = now;
            c.last_press = now;
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
        let (btn, info) = match rt
            .layout(&c.page, c.device.rows, c.device.cols)
            .into_iter()
            .nth(key as usize)
        {
            Some(Cell::Button(b)) => (b, ctx_info(c, key)),
            Some(Cell::Span(sp)) if sp.error.is_none() => {
                let mut info = ctx_info(c, key);
                info.span = Some((sp.size.cols, sp.size.rows));
                info.span_index = Some(sp.index);
                (sp.def, info)
            }
            _ => return,
        };
        tracing::debug!(client = %c.client_id, page = %c.page, key, down, "key event");
        match rt.press(&btn, &info, down) {
            Ok(Some(page)) => self.goto_page(id, &page),
            Ok(None) => {
                if down && btn.has_press() && btn.feedback && info.span.is_none() {
                    self.flash_key(id, key, now);
                }
            }
            Err(e) => {
                tracing::error!(
                    "{} key {key} {}: {e:#}",
                    info.page,
                    if down { "press" } else { "release" }
                );
                let jpeg = self.renderer.render_for(
                    &RenderSpec::error(),
                    c.device.key_size_px,
                    c.device.native.as_ref(),
                );
                self.send_if_changed(id, key, jpeg);
            }
        }
    }

    /// Shows the press flash on `key` of deck `id` for [`FLASH`].
    fn flash_key(&mut self, id: ClientId, key: u8, now: Instant) {
        if !self.feedback() {
            return;
        }
        self.flashes.insert((id, FlashKey::Key(key)), now + FLASH);
        self.render_keys(id, &[key]);
        self.schedule_flash_end();
    }

    /// Shows the press flash on a plugin slot for [`FLASH`].
    fn flash_slot(&mut self, id: ClientId, slot: &str, now: Instant) {
        if !self.feedback() {
            return;
        }
        self.flashes
            .insert((id, FlashKey::Slot(slot.to_string())), now + FLASH);
        self.render_slot(id, slot);
        self.schedule_flash_end();
    }

    /// Sends a [`EngineEvent::Tick`] once the flash is over (the regular tick is too coarse).
    fn schedule_flash_end(&self) {
        if let Some((handle, events)) = &self.timer {
            let events = events.clone();
            handle.spawn(async move {
                tokio::time::sleep(FLASH).await;
                if let Some(tx) = events.upgrade() {
                    let _ = tx.send(EngineEvent::Tick);
                }
            });
        }
    }

    /// Re-renders the keys and slots whose flash is over.
    fn end_flashes(&mut self, now: Instant) {
        let due: Vec<_> = self
            .flashes
            .iter()
            .filter(|(_, until)| **until <= now)
            .map(|(k, _)| k.clone())
            .collect();
        for k in due {
            self.flashes.remove(&k);
            match k {
                (id, FlashKey::Key(key)) => self.render_keys(id, &[key]),
                (id, FlashKey::Slot(slot)) => self.render_slot(id, &slot),
            }
        }
    }

    fn render_all_clients(&mut self) {
        let ids: Vec<_> = self.clients.keys().copied().collect();
        for id in ids {
            self.render_client(id);
        }
        self.render_all_slots();
    }

    fn render_client(&mut self, id: ClientId) {
        let Some(c) = self.clients.get(&id) else {
            return;
        };
        let keys: Vec<u8> = (0..c.device.key_count()).collect();
        self.render_keys(id, &keys);
    }

    /// Renders `keys` of a client: Lua render functions run one after the other on this thread,
    /// then the resulting descriptors are rasterized and encoded in parallel. A span is rendered
    /// once for all of its keys, whichever of them was asked for.
    fn render_keys(&mut self, id: ClientId, keys: &[u8]) {
        let Some(c) = self.clients.get(&id) else {
            return;
        };
        let started = Instant::now();
        let size = c.device.key_size_px;
        let rt = self.runtime.as_ref().expect("a client implies a config");
        let cells = rt.layout(&c.page, c.device.rows, c.device.cols);
        // keys each job produces a tile for, in order, with the entities their render read
        let mut groups: Vec<(Vec<u8>, HashSet<String>)> = Vec::with_capacity(keys.len());
        let mut jobs = Vec::with_capacity(keys.len());
        let mut spans_done = HashSet::new();
        let black = RenderSpec {
            bg: Color::BLACK,
            ..RenderSpec::default()
        };
        for &key in keys {
            let cell = cells.get(key as usize).cloned().unwrap_or(Cell::Empty);
            let (spec, deps) = match &cell {
                Cell::Button(btn) => {
                    let info = ctx_info(c, key);
                    let out = rt.render(btn, &info);
                    if let Some(e) = &out.error {
                        tracing::error!("render {} key {key} for {}: {e}", info.page, c.client_id);
                    }
                    let mut spec = out.spec;
                    spec.flash = self.flashes.contains_key(&(id, FlashKey::Key(key)));
                    (spec, out.deps)
                }
                Cell::Span(sp) => {
                    if !spans_done.insert(sp.origin) {
                        continue;
                    }
                    let members: Vec<u8> = cells
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| matches!(c, Cell::Span(o) if o.origin == sp.origin))
                        .map(|(k, _)| k as u8)
                        .collect();
                    let (spec, deps, failed) = self.render_span(rt, c, sp);
                    let deps: HashSet<String> = deps
                        .into_iter()
                        .chain(
                            self.renderer
                                .remote_keys(&spec)
                                .into_iter()
                                .map(|k| format!("img:{k}")),
                        )
                        .collect();
                    if failed {
                        // the red ERR tile on every key of the span
                        for k in members {
                            jobs.push(self.renderer.prepare(&RenderSpec::error(), size));
                            groups.push((vec![k], deps.clone()));
                        }
                    } else {
                        let geom = SpanGeom {
                            cols: sp.size.cols as u32,
                            rows: sp.size.rows as u32,
                            key: size,
                        };
                        jobs.push(self.renderer.prepare_span(&spec, geom));
                        groups.push((members, deps));
                    }
                    continue;
                }
                Cell::Empty => (black.clone(), HashSet::new()),
            };
            let mut deps = deps;
            deps.extend(
                self.renderer
                    .remote_keys(&spec)
                    .into_iter()
                    .map(|k| format!("img:{k}")),
            );
            jobs.push(self.renderer.prepare(&spec, size));
            groups.push((vec![key], deps));
        }
        let page = c.page.clone();
        let mut jpegs = self
            .renderer
            .encode_all(jobs, c.device.native.as_ref())
            .into_iter();
        tracing::debug!(
            client = id,
            page,
            keys = keys.len(),
            ms = started.elapsed().as_secs_f64() * 1000.0,
            "rendered keys"
        );
        for (group, deps) in groups {
            for key in group {
                let Some(jpeg) = jpegs.next() else { return };
                self.set_deps(id, key, deps.clone());
                self.send_if_changed(id, key, jpeg);
            }
        }
    }

    /// Runs a span's render. Returns the descriptor, the entities it read and whether the span
    /// cannot be shown (layout problem or a Lua error), in which case its keys show ERR.
    fn render_span(
        &self,
        rt: &LuaRuntime,
        c: &Client,
        sp: &SpanCell,
    ) -> (RenderSpec, HashSet<String>, bool) {
        if let Some(e) = &sp.error {
            tracing::error!("render {} for {}: {e}", c.page, c.client_id);
            return (RenderSpec::error(), HashSet::new(), true);
        }
        let mut info = ctx_info(c, sp.origin);
        info.span = Some((sp.size.cols, sp.size.rows));
        let out = rt.render(&sp.def, &info);
        if let Some(e) = &out.error {
            tracing::error!(
                "render {} span at key {} for {}: {e}",
                info.page,
                sp.origin,
                c.client_id
            );
        }
        (out.spec, out.deps, out.error.is_some())
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

/// Whether `home_after` has passed since `last_press`.
fn home_due(home_after: Option<Duration>, last_press: Instant, now: Instant) -> bool {
    home_after.is_some_and(|t| now.saturating_duration_since(last_press) >= t)
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

fn ctx_info(c: &Client, key: u8) -> CtxInfo {
    CtxInfo {
        client_id: c.client_id.clone(),
        key,
        page: c.page.clone(),
        rows: c.device.rows,
        cols: c.device.cols,
        span: None,
        span_index: None,
        slot: None,
    }
}

/// Spawns the engine thread (Lua is `!Send`, so it is constructed on that thread); call it
/// inside a tokio runtime. A failing initial config load is logged; the engine keeps running so a
/// subsequent hot reload can fix it.
pub fn spawn(
    config_path: PathBuf,
    ha_tx: Option<UnboundedSender<HaCommand>>,
) -> UnboundedSender<EngineEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = Handle::current();
    let events = tx.clone();
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
    std::thread::Builder::new()
        .name("sdc-engine".into())
        .spawn(move || {
            let mut engine = Engine::new(config_path, ha_tx);
            engine.enable_fetch(handle, &events);
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
            web: false,
            id: 1,
            client_id: "c1".into(),
            device: device(),
            want_device: None,
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
        sdc.device("d", { pages = { "home" }, brightness = 80,
                          idle = { dim_after = 120, dim_brightness = 10, off_after = 600 } })
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
                web: false,
                id: 2,
                client_id: "c2".into(),
                device: device(),
                want_device: None,
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
        assert!(brightness(&drain_out(&mut rx)).is_empty());
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
    fn without_idle_config_never_idles() {
        let (mut e, mut rx, mut ha, _d) = setup(CONFIG);
        let t0 = Instant::now();
        e.tick(t0 + secs(100_000));
        e.handle_at(EngineEvent::KeyDown { id: 1, key: 0 }, t0 + secs(100_001));
        assert!(brightness(&drain_out(&mut rx)).is_empty());
        assert!(ha.try_recv().is_ok(), "the press runs normally");
    }

    #[test]
    fn default_wake_brightness_and_reload_keep_idle_state() {
        let cfg = r##"sdc.page("home", {}) sdc.device("d", { pages = { "home" }, idle = { dim_after = 10 } })"##;
        let (mut e, mut rx, _ha, d) = setup(cfg);
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
        // dropping the idle setting while dimmed wakes the client on the next tick
        e.tick(t0 + secs(30));
        assert_eq!(brightness(&drain_out(&mut rx)), [10]);
        write_config(d.path(), r##"sdc.page("home", {})"##);
        e.handle_at(EngineEvent::Reload, t0 + secs(31));
        assert!(brightness(&drain_out(&mut rx)).is_empty());
        e.tick(t0 + secs(32));
        assert_eq!(brightness(&drain_out(&mut rx)), [60]);
        // a changed brightness reaches an awake client right away
        write_config(
            d.path(),
            r##"sdc.page("home", {}) sdc.device("d", { pages = { "home" }, brightness = 30 })"##,
        );
        e.handle_at(EngineEvent::Reload, t0 + secs(33));
        assert_eq!(brightness(&drain_out(&mut rx)), [30]);
    }

    const HOME_CONFIG: &str = r##"
        sdc.device("d", { pages = { "home", "two" },
                          idle = { dim_after = 120, off_after = 600, home_after = 3600 } })
        sdc.page("home", { sdc.button{ render = { text = "H" }, press = function(ctx) ctx.goto_page("two") end } })
        sdc.page("two", { sdc.button{ render = { text = "T" } } })
    "##;

    fn connect_at(e: &mut Engine, id: ClientId, name: &str, t: Instant) -> UnboundedReceiver<Out> {
        let (tx, rx) = mpsc::unbounded_channel();
        e.handle_at(
            EngineEvent::ClientConnected {
                web: false,
                id,
                client_id: name.into(),
                device: device(),
                want_device: None,
                tx,
            },
            t,
        );
        rx
    }

    #[test]
    fn page_is_remembered_across_reconnect() {
        let (mut e, rx, _ha, _d) = setup(HOME_CONFIG);
        drop(rx);
        let t0 = Instant::now();
        e.goto_page(1, "two");
        e.handle_at(EngineEvent::ClientDisconnected(1), t0);
        let mut rx2 = connect_at(&mut e, 2, "c1", t0 + secs(10));
        assert_eq!(e.clients[&2].page, "two");
        assert_eq!(drain(&mut rx2).len(), 15);
        // a different client_id starts at home
        connect_at(&mut e, 3, "other", t0 + secs(10));
        assert_eq!(e.clients[&3].page, "home");
        // reconnect before the old disconnect arrived keeps the page, and the stale
        // disconnect does not overwrite the memory
        connect_at(&mut e, 4, "c1", t0 + secs(11));
        assert_eq!(e.clients[&4].page, "two");
        e.handle_at(EngineEvent::ClientDisconnected(2), t0 + secs(12));
        e.handle_at(EngineEvent::ClientDisconnected(4), t0 + secs(13));
        connect_at(&mut e, 5, "c1", t0 + secs(14));
        assert_eq!(e.clients[&5].page, "two");
    }

    #[test]
    fn remembered_page_expires_after_home_after() {
        let (mut e, rx, _ha, _d) = setup(HOME_CONFIG);
        drop(rx);
        let t0 = Instant::now();
        e.handle_at(EngineEvent::KeyDown { id: 1, key: 0 }, t0); // goes to "two"
        assert_eq!(e.clients[&1].page, "two");
        e.handle_at(EngineEvent::ClientDisconnected(1), t0 + secs(1));
        // the press time survives the reconnect: 3599s after it, still "two"
        connect_at(&mut e, 2, "c1", t0 + secs(3599));
        assert_eq!(e.clients[&2].page, "two");
        e.handle_at(EngineEvent::ClientDisconnected(2), t0 + secs(3599));
        connect_at(&mut e, 3, "c1", t0 + secs(3600));
        assert_eq!(e.clients[&3].page, "home");
        // tick forgets expired entries
        e.handle_at(EngineEvent::ClientDisconnected(3), t0 + secs(3600));
        assert!(e.remembered.contains_key("c1"));
        e.tick(t0 + secs(3600 + 3600));
        assert!(e.remembered.is_empty());
    }

    #[test]
    fn home_after_returns_to_default_even_when_off() {
        let (mut e, mut rx, _ha, _d) = setup(HOME_CONFIG);
        drain_out(&mut rx);
        let t0 = Instant::now();
        e.handle_at(EngineEvent::KeyDown { id: 1, key: 0 }, t0);
        assert_eq!(e.clients[&1].page, "two");
        drain_out(&mut rx);
        e.tick(t0 + secs(600));
        assert_eq!(brightness(&drain_out(&mut rx)), [0]);
        e.tick(t0 + secs(3599));
        assert_eq!(e.clients[&1].page, "two");
        e.tick(t0 + secs(3600));
        assert_eq!(e.clients[&1].page, "home");
        assert!(!drain(&mut rx).is_empty(), "home is rendered while off");
        // waking keeps it on home; the waking press does nothing else
        e.handle_at(EngineEvent::KeyDown { id: 1, key: 0 }, t0 + secs(4000));
        assert_eq!(e.clients[&1].page, "home");
        assert_eq!(brightness(&drain_out(&mut rx)), [60]);
    }

    #[test]
    fn wake_message_wakes_without_running_buttons() {
        let (mut e, mut rx, mut ha, _d) = setup(IDLE_CONFIG);
        drain_out(&mut rx);
        let t0 = Instant::now();
        e.tick(t0 + secs(130));
        assert_eq!(brightness(&drain_out(&mut rx)), [10]);
        e.handle_at(EngineEvent::Wake { id: 1 }, t0 + secs(131));
        assert_eq!(brightness(&drain_out(&mut rx)), [80]);
        assert!(ha.try_recv().is_err(), "no button ran");
        // awake already: no brightness message, but the idle clock restarts
        e.handle_at(EngineEvent::Wake { id: 1 }, t0 + secs(200));
        assert!(brightness(&drain_out(&mut rx)).is_empty());
        e.tick(t0 + secs(200 + 119));
        assert!(brightness(&drain_out(&mut rx)).is_empty());
        e.tick(t0 + secs(200 + 130));
        assert_eq!(brightness(&drain_out(&mut rx)), [10]);
        // a Wake for an unknown client is ignored
        e.handle_at(EngineEvent::Wake { id: 99 }, t0 + secs(400));
    }

    #[test]
    fn waking_before_home_after_keeps_page() {
        let (mut e, mut rx, _ha, _d) = setup(HOME_CONFIG);
        drain_out(&mut rx);
        let t0 = Instant::now();
        e.handle_at(EngineEvent::KeyDown { id: 1, key: 0 }, t0);
        e.tick(t0 + secs(700)); // off
        e.handle_at(EngineEvent::KeyDown { id: 1, key: 0 }, t0 + secs(1000)); // wakes only
        e.handle_at(EngineEvent::KeyUp { id: 1, key: 0 }, t0 + secs(1000));
        assert_eq!(e.clients[&1].page, "two");
        // the wake press restarted the home clock
        e.tick(t0 + secs(1000 + 3599));
        assert_eq!(e.clients[&1].page, "two");
        e.tick(t0 + secs(1000 + 3600));
        assert_eq!(e.clients[&1].page, "home");
    }

    #[test]
    fn remembered_page_removed_by_reload_falls_back_to_default() {
        let (mut e, rx, _ha, d) = setup(HOME_CONFIG);
        drop(rx);
        let t0 = Instant::now();
        e.goto_page(1, "two");
        e.handle_at(EngineEvent::ClientDisconnected(1), t0);
        write_config(d.path(), r##"sdc.page("home", {})"##);
        e.handle_at(EngineEvent::Reload, t0);
        connect_at(&mut e, 2, "c1", t0 + secs(5));
        assert_eq!(e.clients[&2].page, "home");
    }

    const DEVICES_CONFIG: &str = r##"
        sdc.page("home", { sdc.button{ render = { text = "H" }, press = function(ctx) ctx.goto_page("lights") end } })
        sdc.page("lights", { sdc.button{ render = { text = "L" }, press = function(ctx) ctx.goto_page("tv") end } })
        sdc.page("tv", { sdc.button{ render = { text = "T" } } })
        sdc.device("a", { pages = { "home", "lights" }, home = "home",
                          idle = { home_after = 100 } })
        sdc.device("b", { pages = { "tv", "home" }, home = "tv", brightness = 30 })
        sdc.default_device { mk2 = "a", browser = "b", ["*"] = "b" }
    "##;

    fn device_engine(src: &str) -> (Engine, tempdir::Dir) {
        let dir = tempdir::Dir::new();
        let path = write_config(dir.path(), src);
        let mut e = Engine::new(path, None);
        e.reload().unwrap();
        (e, dir)
    }

    fn connect_dev(
        e: &mut Engine,
        id: ClientId,
        model: &str,
        web: bool,
        want: Option<&str>,
        t: Instant,
    ) -> UnboundedReceiver<Out> {
        let (tx, rx) = mpsc::unbounded_channel();
        e.handle_at(
            EngineEvent::ClientConnected {
                web,
                id,
                client_id: format!("c{id}"),
                device: DeviceInfo {
                    model: model.into(),
                    ..device()
                },
                want_device: want.map(Into::into),
                tx,
            },
            t,
        );
        rx
    }

    fn dev_of(e: &Engine, id: ClientId) -> (String, String) {
        (
            e.clients[&id].policy.device.clone(),
            e.clients[&id].page.clone(),
        )
    }

    #[test]
    fn device_resolution_order() {
        let (mut e, d) = device_engine(DEVICES_CONFIG);
        let t = Instant::now();
        // explicit name beats the type default
        connect_dev(&mut e, 1, "Mk2", false, Some("b"), t);
        assert_eq!(dev_of(&e, 1), ("b".into(), "tv".into()));
        // type default
        connect_dev(&mut e, 2, "Mk2Scissor", false, None, t);
        assert_eq!(dev_of(&e, 2), ("a".into(), "home".into()));
        // an unknown name falls through to the type default
        connect_dev(&mut e, 3, "Mk2", false, Some("nope"), t);
        assert_eq!(dev_of(&e, 3), ("a".into(), "home".into()));
        // "*" for a type without an entry
        connect_dev(&mut e, 4, "XlV2", false, None, t);
        assert_eq!(dev_of(&e, 4), ("b".into(), "tv".into()));
        // browser type
        connect_dev(&mut e, 5, "Web Deck", true, None, t);
        assert_eq!(dev_of(&e, 5), ("b".into(), "tv".into()));

        // no "*" and no matching type: the first device
        write_config(
            d.path(),
            &DEVICES_CONFIG
                .replace(r#"["*"] = "b""#, "")
                .replace(r#"browser = "b","#, ""),
        );
        e.handle_at(EngineEvent::Reload, t);
        connect_dev(&mut e, 6, "XlV2", false, None, t);
        assert_eq!(dev_of(&e, 6), ("a".into(), "home".into()));
    }

    #[test]
    fn goto_page_outside_the_device_is_ignored() {
        let (mut e, _d) = device_engine(DEVICES_CONFIG);
        let t = Instant::now();
        let mut rx = connect_dev(&mut e, 1, "Mk2", false, None, t); // device a
        drain(&mut rx);
        e.handle_at(EngineEvent::KeyDown { id: 1, key: 0 }, t); // home -> lights
        assert_eq!(e.clients[&1].page, "lights");
        drain(&mut rx);
        e.handle_at(EngineEvent::KeyDown { id: 1, key: 0 }, t); // lights -> tv: not on a
        assert_eq!(e.clients[&1].page, "lights");
        assert!(drain(&mut rx).is_empty());
        e.goto_page(1, "nonexistent");
        assert_eq!(e.clients[&1].page, "lights");
    }

    #[test]
    fn home_after_goes_to_the_device_home() {
        let (mut e, _d) = device_engine(DEVICES_CONFIG);
        let t0 = Instant::now();
        // device a: home_after 100, home "home"
        connect_dev(&mut e, 1, "Mk2", false, None, t0);
        e.handle_at(EngineEvent::KeyDown { id: 1, key: 0 }, t0);
        assert_eq!(e.clients[&1].page, "lights");
        e.tick(t0 + secs(99));
        assert_eq!(e.clients[&1].page, "lights");
        e.tick(t0 + secs(100));
        assert_eq!(e.clients[&1].page, "home");
        // device b sets no idle: never goes home
        connect_dev(&mut e, 2, "XlV2", false, None, t0);
        e.goto_page(2, "home");
        e.tick(t0 + secs(100_000));
        assert_eq!(e.clients[&2].page, "home");
    }

    #[test]
    fn device_brightness_and_default() {
        let (mut e, _d) = device_engine(DEVICES_CONFIG);
        let t = Instant::now();
        let mut b = connect_dev(&mut e, 1, "XlV2", false, None, t);
        assert_eq!(brightness(&drain_out(&mut b)), [30]);
        let mut a = connect_dev(&mut e, 2, "Mk2", false, None, t);
        assert_eq!(brightness(&drain_out(&mut a)), [60], "default brightness");
    }

    #[test]
    fn reload_re_resolves_devices_and_remembered_page_follows_the_device() {
        let (mut e, d) = device_engine(DEVICES_CONFIG);
        let t = Instant::now();
        connect_dev(&mut e, 1, "Mk2", false, None, t); // a
        e.goto_page(1, "lights");
        // default for mk2 now b: the client moves to b's home
        write_config(
            d.path(),
            &DEVICES_CONFIG.replace(r#"mk2 = "a""#, r#"mk2 = "b""#),
        );
        e.handle_at(EngineEvent::Reload, t);
        assert_eq!(dev_of(&e, 1), ("b".into(), "tv".into()));
        // same device but the page was dropped from it: home
        write_config(
            d.path(),
            &DEVICES_CONFIG
                .replace(r#"mk2 = "a""#, r#"mk2 = "b""#)
                .replace(r#"{ "tv", "home" }"#, r#"{ "home" }"#)
                .replace(r#"home = "tv""#, r#"home = "home""#),
        );
        e.handle_at(EngineEvent::Reload, t);
        assert_eq!(dev_of(&e, 1), ("b".into(), "home".into()));
        // an invalid device config keeps the old one
        write_config(
            d.path(),
            r#"sdc.page("p", {}) sdc.device("x", { pages = { "q" } })"#,
        );
        e.handle_at(EngineEvent::Reload, t);
        assert_eq!(dev_of(&e, 1), ("b".into(), "home".into()));
    }

    #[test]
    fn reconnect_on_another_device_starts_at_its_home() {
        let (mut e, d) = device_engine(DEVICES_CONFIG);
        let t = Instant::now();
        connect_dev(&mut e, 1, "Mk2", false, None, t); // a
        e.goto_page(1, "home");
        e.goto_page(1, "lights");
        e.handle_at(EngineEvent::ClientDisconnected(1), t);
        // same device: resumes
        connect_dev(&mut e, 1, "Mk2", false, None, t + secs(1));
        assert_eq!(dev_of(&e, 1), ("a".into(), "lights".into()));
        e.handle_at(EngineEvent::ClientDisconnected(1), t + secs(2));
        // another device (asked for by name): home of that device
        connect_dev(&mut e, 1, "Mk2", false, Some("b"), t + secs(3));
        assert_eq!(dev_of(&e, 1), ("b".into(), "tv".into()));
        drop(d);
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
            web: false,
            id: 1,
            client_id: "c1".into(),
            device: device(),
            want_device: None,
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

    fn key_set(frames: &[Frame]) -> Vec<u8> {
        let mut k: Vec<u8> = frames.iter().map(|f| f.key).collect();
        k.sort_unstable();
        k
    }

    #[test]
    fn span_dependency_rerenders_every_key_of_the_span() {
        let (mut e, mut rx, _ha, _d) = setup(SPAN_CONFIG);
        e.goto_page(1, "music");
        drain(&mut rx);
        // a 2x2 span at slot 1 covers keys 1, 2, 6, 7
        e.handle(EngineEvent::Ha(HaEvent::Changed(st("light.a", "on"))));
        assert_eq!(key_set(&drain(&mut rx)), [1, 2, 6, 7]);
        assert_eq!(e.dep_count("light.a"), 4);
        // unchanged state -> nothing
        e.handle(EngineEvent::Ha(HaEvent::Changed(st("light.a", "on"))));
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn press_on_any_span_key_reports_key_and_index() {
        let (mut e, mut rx, mut ha, _d) = setup(SPAN_CONFIG);
        e.goto_page(1, "music");
        drain(&mut rx);
        let mut got = Vec::new();
        for key in [1u8, 2, 6, 7] {
            e.handle(EngineEvent::KeyDown { id: 1, key });
            match ha.try_recv().unwrap() {
                HaCommand::CallService { data, .. } => got.push((
                    data["key"].as_u64().unwrap(),
                    data["index"].as_u64().unwrap(),
                )),
            }
        }
        assert_eq!(got, [(1, 0), (2, 1), (6, 2), (7, 3)]);
        // outside the span nothing is called
        e.handle(EngineEvent::KeyDown { id: 1, key: 0 });
        assert!(ha.try_recv().is_err());
    }

    #[test]
    fn span_off_grid_or_overlapping_shows_err_on_its_keys() {
        let (mut e, mut rx, mut ha, _d) = setup(SPAN_CONFIG);
        let err = e.renderer.render(&RenderSpec::error(), 72);
        e.goto_page(1, "off");
        let f = drain(&mut rx);
        // only the key that is on the grid exists; it shows ERR, and pressing it does nothing
        assert_eq!(f.iter().find(|f| f.key == 4).unwrap().jpeg, err);
        e.handle(EngineEvent::KeyDown { id: 1, key: 4 });
        assert!(ha.try_recv().is_err());
        e.goto_page(1, "clash");
        let f = drain(&mut rx);
        for k in [0u8, 5] {
            assert_eq!(f.iter().find(|f| f.key == k).unwrap().jpeg, err, "key {k}");
        }
    }

    #[test]
    fn span_tiles_differ_for_an_image_and_share_the_dependency() {
        // regular buttons and a span on the same page keep their frames when only the span changes
        let (mut e, mut rx, _ha, _d) = setup(SPAN_CONFIG);
        e.goto_page(1, "music");
        drain(&mut rx);
        e.handle(EngineEvent::Ha(HaEvent::Changed(st("light.a", "on"))));
        let f = drain(&mut rx);
        assert!(f.iter().all(|f| [1, 2, 6, 7].contains(&f.key)));
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

    fn press_frames(
        e: &mut Engine,
        rx: &mut UnboundedReceiver<Out>,
        key: u8,
        t: Instant,
    ) -> Vec<Frame> {
        e.handle_at(EngineEvent::KeyDown { id: 1, key }, t);
        drain(rx)
    }

    #[test]
    fn press_flashes_the_key_then_clears_it() {
        let (mut e, mut rx, _ha, _d) = setup(FLASH_PAGE);
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let f = press_frames(&mut e, &mut rx, 0, t0);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].key, 0);
        assert!(edge(&f[0]) > 200, "ring while flashing");
        e.tick(t0 + ms(399));
        assert!(drain(&mut rx).is_empty(), "still flashing");
        // nothing else changed: the flash still ends and the key goes back to normal
        e.tick(t0 + ms(400));
        let f = drain(&mut rx);
        assert_eq!(f.len(), 1);
        assert!(edge(&f[0]) < 80, "ring gone");
        e.tick(t0 + ms(1400));
        assert!(drain(&mut rx).is_empty(), "no frame once settled");
        // a second press restarts the flash
        assert_eq!(press_frames(&mut e, &mut rx, 0, t0 + ms(2000)).len(), 1);
    }

    #[test]
    fn no_flash_for_errors_goto_opt_out_or_missing_press() {
        let (mut e, mut rx, _ha, _d) = setup(FLASH_PAGE);
        let t0 = Instant::now();
        let f = press_frames(&mut e, &mut rx, 1, t0);
        assert!(f.is_empty(), "feedback = false");
        let f = press_frames(&mut e, &mut rx, 3, t0);
        assert_eq!(f.len(), 1);
        assert!(edge(&f[0]) < 80, "an ERR tile, not a ring");
        assert!(
            press_frames(&mut e, &mut rx, 4, t0).is_empty(),
            "no press handler"
        );
        // goto_page repaints the whole new page without a ring on the pressed key
        let f = press_frames(&mut e, &mut rx, 2, t0);
        assert!(f.iter().all(|f| edge(f) < 80));
        e.tick(t0 + Duration::from_millis(500));
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn sdc_feedback_false_disables_the_flash() {
        let cfg = format!("sdc.feedback(false) {FLASH_PAGE}");
        let (mut e, mut rx, _ha, _d) = setup(&cfg);
        assert!(press_frames(&mut e, &mut rx, 0, Instant::now()).is_empty());
    }

    #[test]
    fn wake_press_does_not_flash() {
        let cfg = format!(
            r#"{FLASH_PAGE} sdc.device("d", {{ pages = {{ "home", "two" }}, idle = {{ dim_after = 10 }} }})"#
        );
        let (mut e, mut rx, _ha, _d) = setup(&cfg);
        let t0 = Instant::now();
        e.tick(t0 + secs(11));
        drain_out(&mut rx);
        assert!(press_frames(&mut e, &mut rx, 0, t0 + secs(12)).is_empty());
    }
}
