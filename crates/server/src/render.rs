//! Key image renderer: turns a [`RenderSpec`] into a JPEG at the device's key size.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use ab_glyph::{point, Font, FontRef, PxScale, ScaleFont};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::{self, FilterType};
use image::{ExtendedColorType, ImageEncoder, Rgb, RgbImage, RgbaImage};

use crate::assets::{is_ha_key, redact, Assets, Fetcher, Source};

static FONT_BYTES: &[u8] = include_bytes!("../assets/OpenSans.ttf");

const JPEG_QUALITY: u8 = 90;
const PAD: f32 = 4.0;
const MIN_FONT_PX: f32 = 9.0;
const LINE: f32 = 1.15;
/// Layout of a key with an `icon` and `text` (fractions of the canvas's shorter side): the icon
/// starts `ICON_TOP` from the top and ends `ICON_GAP` above the text, which sits in a band
/// `TEXT_BAND` tall, `TEXT_MARGIN` above the bottom edge, with its last line on the band's bottom.
/// Icons are trimmed to their visible pixels when decoded, so these boxes are the glyph's size.
const ICON_TOP: f32 = 0.08;
const ICON_GAP: f32 = 0.03;
const TEXT_MARGIN: f32 = 0.05;
const TEXT_BAND: f32 = 0.27;
/// Taller band for text that wraps onto several lines.
const TEXT_BAND_MULTI: f32 = 0.38;
/// Box (fraction of the shorter side, centered) for a key with only an icon.
const ICON_ONLY: f32 = 0.66;
/// Alpha at or below this counts as empty when trimming an icon's transparent border.
const TRIM_ALPHA: u8 = 8;
/// How long a failed download is remembered before it is tried again.
const FAIL_TTL: Duration = Duration::from_secs(60);
pub const ERR_BG: Color = Color([0xb0, 0x00, 0x20]);
/// Decoded pictures are shrunk until their shorter side is at most this (keys are <= 120px, so
/// this keeps 2x headroom while a 1024px icon costs 16x less memory and resize time).
const MAX_SRC_SHORT: u32 = 256;
/// Downloaded pictures kept decoded (and failed downloads remembered); the least recently
/// rendered go first. Sources such as media artwork change with every track, so this must not
/// grow forever, while icons that are on a page stay hot.
const MAX_REMOTE: usize = 128;
/// Safety valve for the scaled-picture cache (it is also cleared on config reload).
const MAX_PREPARED: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color(pub [u8; 3]);

impl Color {
    pub const BLACK: Color = Color([0, 0, 0]);
    pub const WHITE: Color = Color([255, 255, 255]);

    /// Parses `#rgb`, `#rrggbb` or `#rrggbbaa` (alpha is ignored).
    pub fn parse(s: &str) -> Option<Color> {
        let h = s.trim().strip_prefix('#')?;
        if !h.is_ascii() {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
        match h.len() {
            3 => {
                let n = |i: usize| u8::from_str_radix(&h[i..i + 1], 16).ok().map(|v| v * 17);
                Some(Color([n(0)?, n(1)?, n(2)?]))
            }
            6 | 8 => Some(Color([byte(0)?, byte(2)?, byte(4)?])),
            _ => None,
        }
    }
}

/// What a button's `render` produces.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderSpec {
    pub text: Option<String>,
    pub bg: Color,
    pub fg: Color,
    /// Fit-with-padding picture over `bg` (local path, URL or bucket key).
    pub icon: Option<String>,
    /// Full-bleed picture (scaled to cover the key); text is drawn on top of it.
    pub image: Option<String>,
    pub font_size: Option<f32>,
}

impl Default for RenderSpec {
    fn default() -> Self {
        Self {
            text: None,
            bg: Color([0x1c, 0x1c, 0x1e]),
            fg: Color::WHITE,
            icon: None,
            image: None,
            font_size: None,
        }
    }
}

impl RenderSpec {
    pub fn error() -> Self {
        Self {
            text: Some("ERR".into()),
            bg: ERR_BG,
            ..Self::default()
        }
    }
}

/// A decoded picture with premultiplied alpha, shared by every render that uses it.
pub struct Loaded {
    /// Identifies the source in the scaled-picture cache (file path or remote cache key).
    id: String,
    img: RgbaImage,
}

impl Loaded {
    /// Premultiplies `img` (once, so scaling never bleeds the colour of transparent pixels) and
    /// shrinks it to at most [`MAX_SRC_SHORT`] on the shorter side.
    fn new(id: String, mut img: RgbaImage) -> Arc<Self> {
        img = trim_transparent(img);
        premultiply(&mut img);
        let short = img.width().min(img.height());
        if short > MAX_SRC_SHORT {
            let k = MAX_SRC_SHORT as f32 / short as f32;
            let w = ((img.width() as f32 * k).round() as u32).max(1);
            let h = ((img.height() as f32 * k).round() as u32).max(1);
            img = imageops::resize(&img, w, h, FilterType::Lanczos3);
        }
        Arc::new(Self { id, img })
    }
}

/// How a picture is scaled for a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Fit {
    /// Cover a `w` x `h` rectangle, center-cropped.
    Cover(u32, u32),
    /// Exactly this size (the aspect ratio is already right).
    Exact(u32, u32),
}

/// Pictures scaled for a key size, computed once per (source, fit) and shared between the
/// worker threads that rasterize a page. A `OnceLock` per entry makes concurrent requests for
/// the same scaling wait for one computation instead of repeating it.
type ScaledSlot = OnceLock<Arc<RgbaImage>>;

#[derive(Default)]
struct Scaled {
    map: Mutex<HashMap<(String, Fit), Arc<ScaledSlot>>>,
    /// Scalings actually computed (cache misses).
    computed: AtomicUsize,
}

impl Scaled {
    fn get(&self, src: &Loaded, fit: Fit) -> Arc<RgbaImage> {
        let slot = {
            let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
            if map.len() >= MAX_PREPARED {
                map.clear();
            }
            map.entry((src.id.clone(), fit)).or_default().clone()
        };
        slot.get_or_init(|| {
            self.computed.fetch_add(1, Ordering::Relaxed);
            Arc::new(match fit {
                Fit::Cover(w, h) => cover(&src.img, w, h),
                Fit::Exact(w, h) => resize_premul(&src.img, w, h, true),
            })
        })
        .clone()
    }

    fn clear(&self) {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    fn forget(&self, id: &str) {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(i, _), _| i != id);
    }
}

/// A map that drops its least recently used entries beyond `cap`. Lookups are linear in the
/// number of entries to evict only, so it is meant for small caps.
struct Lru<V> {
    cap: usize,
    tick: u64,
    map: HashMap<String, (V, u64)>,
}

impl<V> Lru<V> {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            tick: 0,
            map: HashMap::new(),
        }
    }

    /// Marks `key` as just used.
    fn get(&mut self, key: &str) -> Option<&V> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(key).map(|(v, used)| {
            *used = tick;
            &*v
        })
    }

    /// Inserts `key` and returns the keys evicted to stay within the cap. Entries for which
    /// `keep` is true (downloads in flight) are never evicted, so the cap can be exceeded
    /// while that many are pending.
    fn insert(&mut self, key: String, v: V, keep: impl Fn(&V) -> bool) -> Vec<String> {
        self.tick += 1;
        self.map.insert(key.clone(), (v, self.tick));
        let mut evicted = Vec::new();
        while self.map.len() > self.cap {
            let oldest = self
                .map
                .iter()
                .filter(|(k, (v, _))| **k != key && !keep(v))
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone());
            let Some(k) = oldest else { break };
            self.map.remove(&k);
            evicted.push(k);
        }
        evicted
    }

    fn values(&self) -> impl Iterator<Item = &V> {
        self.map.values().map(|(v, _)| v)
    }

    fn retain(&mut self, f: impl Fn(&V) -> bool) {
        self.map.retain(|_, (v, _)| f(v));
    }
}

/// Everything needed to rasterize a key, shareable between threads (no caches or Lua).
struct Painter {
    font: FontRef<'static>,
    scaled: Scaled,
}

pub struct Renderer {
    painter: Arc<Painter>,
    assets: Assets,
    /// `icon`/`image` string -> where it lives (None: unresolvable, already logged).
    resolved: HashMap<String, Option<Source>>,
    local: HashMap<PathBuf, Option<Arc<Loaded>>>,
    /// Downloaded images by remote cache key (bounded, see [`MAX_REMOTE`]).
    remote: Lru<Slot>,
    /// Without one, remote images are reported as failed (they are never fetched inline).
    fetcher: Option<Fetcher>,
}

enum Slot {
    Loading,
    Ready(Arc<Loaded>),
    Failed(Instant),
}

/// Result of looking an image up for one render; never waits on the network.
#[derive(Clone)]
enum Img {
    Ready(Arc<Loaded>),
    Loading,
    Failed,
    /// Home Assistant artwork that could not be fetched: media art legitimately goes away, so
    /// this renders as if there were no picture, without an error tile.
    Missing,
}

/// A key ready to rasterize: the spec plus its pictures looked up (cheap, needs the renderer's
/// caches). Turning it into pixels needs no renderer state, so jobs can run in parallel.
pub struct Job {
    spec: RenderSpec,
    /// Canvas size; one key's size, or a span's virtual size including the gaps.
    w: u32,
    h: u32,
    /// Set for a span: how the canvas is cut into key tiles.
    span: Option<SpanGeom>,
    image: Option<Img>,
    icon: Option<Img>,
}

impl Job {
    /// Drops pictures that are [`Img::Missing`] so layout treats them as not given.
    fn without_missing(mut self) -> Self {
        let keep = |i: Option<Img>| i.filter(|i| !matches!(i, Img::Missing));
        self.image = keep(self.image);
        self.icon = keep(self.icon);
        self
    }
}

/// A rectangle of keys that shows one picture: `cols` x `rows` keys of `key` px with `gap` px
/// between neighbours (the space between the physical keys, which the picture continues over).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanGeom {
    pub cols: u32,
    pub rows: u32,
    pub key: u32,
    pub gap: u32,
}

/// Default gap between keys in px at 72 px keys. 0 cuts a span into plain tiles; a
/// gap matching the bezel (~18 on an MK.2) keeps lines straight across keys but
/// hides a strip of the picture, usually the middle where the subject is.
pub const DEFAULT_GAP: u32 = 0;

/// A configured gap (px at 72 px keys) scaled to `key` px keys.
pub fn gap_px(gap72: u32, key: u32) -> u32 {
    (gap72 as f32 * key as f32 / 72.0).round() as u32
}

impl SpanGeom {
    /// Size of the virtual canvas.
    pub fn canvas(&self) -> (u32, u32) {
        let len = |n: u32| n * self.key + n.saturating_sub(1) * self.gap;
        (len(self.cols), len(self.rows))
    }

    /// Top-left of tile (`c`, `r`) in the canvas.
    pub fn tile_origin(&self, c: u32, r: u32) -> (u32, u32) {
        (c * (self.key + self.gap), r * (self.key + self.gap))
    }
}

#[derive(Clone, Copy)]
struct Area {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Renderer {
    /// `base_dir` is the directory icon paths are relative to (the config's directory).
    pub fn new(base_dir: PathBuf) -> Self {
        Self::with_assets(Assets::new(base_dir, None))
    }

    pub fn with_assets(assets: Assets) -> Self {
        Self {
            painter: Arc::new(Painter {
                font: FontRef::try_from_slice(FONT_BYTES).expect("embedded font is valid"),
                scaled: Scaled::default(),
            }),
            assets,
            resolved: HashMap::new(),
            local: HashMap::new(),
            remote: Lru::new(MAX_REMOTE),
            fetcher: None,
        }
    }

    pub fn set_fetcher(&mut self, fetcher: Option<Fetcher>) {
        self.fetcher = fetcher;
    }

    /// Forgets local files, resolutions and failed downloads (config reload). Downloaded
    /// images stay: they do not depend on the config and refetching would blank the keys.
    pub fn clear_icon_cache(&mut self) {
        self.resolved.clear();
        self.local.clear();
        self.painter.scaled.clear();
        self.remote.retain(|s| !matches!(s, Slot::Failed(_)));
    }

    /// Number of downloads in flight.
    pub fn pending(&self) -> usize {
        self.remote
            .values()
            .filter(|s| matches!(s, Slot::Loading))
            .count()
    }

    /// Starts fetching `name` if it is a remote image that is not cached yet.
    pub fn prefetch(&mut self, name: &str) {
        self.lookup(name);
    }

    /// Cache keys of the remote images `spec` uses: when one of them arrives, the key must be
    /// re-rendered.
    pub fn remote_keys(&mut self, spec: &RenderSpec) -> Vec<String> {
        [spec.icon.as_deref(), spec.image.as_deref()]
            .into_iter()
            .flatten()
            .filter_map(|n| {
                let src = self.resolve(n)?;
                self.assets.remote_key(&src)
            })
            .collect()
    }

    /// Records the outcome of a background download started by [`Fetcher::spawn`].
    pub fn finish_fetch(&mut self, key: &str, result: Result<RgbaImage, String>) {
        let slot = match result {
            Ok(img) => Slot::Ready(Loaded::new(key.to_string(), img)),
            Err(e) => {
                if is_ha_key(key) {
                    tracing::debug!("no image from {}: {e}", redact(key));
                } else {
                    tracing::warn!("cannot fetch image {}: {e}", redact(key));
                }
                Slot::Failed(Instant::now())
            }
        };
        self.painter.scaled.forget(key);
        self.store(key.to_string(), slot);
    }

    /// Caches `slot`, dropping the pictures scaled from whatever the LRU evicts.
    fn store(&mut self, key: String, slot: Slot) {
        for old in self
            .remote
            .insert(key, slot, |s| matches!(s, Slot::Loading))
        {
            self.painter.scaled.forget(&old);
        }
    }

    /// Scalings computed so far (cache misses); tests use it to check pictures are reused.
    pub fn scalings(&self) -> usize {
        self.painter.scaled.computed.load(Ordering::Relaxed)
    }

    pub fn blank(&self, size: u32) -> Vec<u8> {
        self.blank_with(Color::BLACK, size)
    }

    fn blank_with(&self, color: Color, size: u32) -> Vec<u8> {
        encode_jpeg(&RgbImage::from_pixel(size, size, Rgb(color.0)))
    }

    pub fn render(&mut self, spec: &RenderSpec, size: u32) -> Vec<u8> {
        encode_jpeg(&self.render_image(spec, size))
    }

    /// Like [`Renderer::render`] as a PNG (plugins hand it to Elgato's `setImage`).
    pub fn render_png(&mut self, spec: &RenderSpec, size: u32) -> Vec<u8> {
        encode_png(&self.render_image(spec, size))
    }

    pub fn render_image(&mut self, spec: &RenderSpec, size: u32) -> RgbImage {
        let job = self.prepare(spec, size);
        self.painter.paint(&job)
    }

    /// Looks up the pictures `spec` needs (decoding local files, starting downloads).
    pub fn prepare(&mut self, spec: &RenderSpec, size: u32) -> Job {
        self.prepare_sized(spec, size, size, None)
    }

    /// Like [`Renderer::prepare`] for a span: the picture is laid out on the whole canvas.
    pub fn prepare_span(&mut self, spec: &RenderSpec, geom: SpanGeom) -> Job {
        let (w, h) = geom.canvas();
        self.prepare_sized(spec, w, h, Some(geom))
    }

    fn prepare_sized(&mut self, spec: &RenderSpec, w: u32, h: u32, span: Option<SpanGeom>) -> Job {
        Job {
            spec: spec.clone(),
            w,
            h,
            span,
            image: spec.image.as_deref().map(|n| self.lookup(n)),
            icon: spec.icon.as_deref().map(|n| self.lookup(n)),
        }
        .without_missing()
    }

    /// Rasterizes `span` and cuts it into its key tiles (row-major), before JPEG encoding.
    pub fn render_span_images(&mut self, spec: &RenderSpec, geom: SpanGeom) -> Vec<RgbImage> {
        let job = self.prepare_span(spec, geom);
        self.painter.paint_tiles(&job)
    }

    /// Rasterizes and JPEG-encodes `jobs`, spread over the available cores. A job yields one
    /// JPEG, or for a span one per key (row-major); the results are concatenated in job order.
    pub fn encode_all(&self, jobs: Vec<Job>) -> Vec<Vec<u8>> {
        let painter = &*self.painter;
        let run = |job: &Job| -> Vec<Vec<u8>> {
            painter.paint_tiles(job).iter().map(encode_jpeg).collect()
        };
        let workers = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(jobs.len());
        if workers <= 1 {
            return jobs.iter().flat_map(run).collect();
        }
        let next = AtomicUsize::new(0);
        let mut out: Vec<Option<Vec<Vec<u8>>>> = vec![None; jobs.len()];
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..workers)
                .map(|_| {
                    scope.spawn(|| {
                        let mut done = Vec::new();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(job) = jobs.get(i) else { break };
                            done.push((i, run(job)));
                        }
                        done
                    })
                })
                .collect();
            for h in handles {
                for (i, jpeg) in h.join().expect("render worker panicked") {
                    out[i] = Some(jpeg);
                }
            }
        });
        out.into_iter()
            .flat_map(|j| j.expect("every job ran"))
            .collect()
    }

    /// Renders several keys at once (see [`Renderer::encode_all`]).
    pub fn render_many(&mut self, specs: &[RenderSpec], size: u32) -> Vec<Vec<u8>> {
        let jobs = specs.iter().map(|s| self.prepare(s, size)).collect();
        self.encode_all(jobs)
    }

    fn resolve(&mut self, name: &str) -> Option<Source> {
        if let Some(r) = self.resolved.get(name) {
            return r.clone();
        }
        let r = match self.assets.resolve(name) {
            Ok(src) => Some(src),
            Err(e) => {
                tracing::warn!("cannot resolve image {name:?}: {e}");
                None
            }
        };
        self.resolved.insert(name.to_string(), r.clone());
        r
    }

    /// Local files are decoded inline (cached); remote ones come from the cache or start a
    /// background download and report `Loading`.
    fn lookup(&mut self, name: &str) -> Img {
        let Some(src) = self.resolve(name) else {
            return Img::Failed;
        };
        let Some(key) = self.assets.remote_key(&src) else {
            let Source::File(full) = src else {
                return Img::Failed;
            };
            let loaded =
                self.local
                    .entry(full.clone())
                    .or_insert_with(|| match image::open(&full) {
                        Ok(i) => Some(Loaded::new(full.display().to_string(), i.to_rgba8())),
                        Err(e) => {
                            tracing::warn!("cannot load icon {}: {e}", full.display());
                            None
                        }
                    });
            return loaded.clone().map_or(Img::Failed, Img::Ready);
        };
        // failed Home Assistant artwork is not an error, see [`Img::Missing`]
        let failed = if is_ha_key(&key) {
            Img::Missing
        } else {
            Img::Failed
        };
        match self.remote.get(&key) {
            Some(Slot::Ready(i)) => return Img::Ready(i.clone()),
            Some(Slot::Loading) => return Img::Loading,
            Some(Slot::Failed(at)) if at.elapsed() < FAIL_TTL => return failed,
            _ => {}
        }
        let started = match (&self.fetcher, self.assets.download(&src)) {
            (Some(f), Ok(dl)) => {
                f.spawn(key.clone(), dl);
                true
            }
            (None, _) => {
                tracing::warn!("cannot fetch {}: no background fetcher", redact(&key));
                false
            }
            (_, Err(e)) => {
                tracing::warn!("cannot fetch {}: {e:#}", redact(&key));
                false
            }
        };
        if started {
            self.store(key, Slot::Loading);
            Img::Loading
        } else {
            self.store(key, Slot::Failed(Instant::now()));
            failed
        }
    }
}

impl Painter {
    /// The job's picture, cut into key tiles for a span (a single tile otherwise).
    fn paint_tiles(&self, job: &Job) -> Vec<RgbImage> {
        let img = self.paint(job);
        let Some(g) = job.span else {
            return vec![img];
        };
        (0..g.rows)
            .flat_map(|r| (0..g.cols).map(move |c| (c, r)))
            .map(|(c, r)| {
                let (x, y) = g.tile_origin(c, r);
                imageops::crop_imm(&img, x, y, g.key, g.key).to_image()
            })
            .collect()
    }

    fn paint(&self, job: &Job) -> RgbImage {
        let spec = &job.spec;
        let (w, h) = (job.w, job.h);
        let mut img = RgbImage::from_pixel(w, h, Rgb(spec.bg.0));
        // sizes of padding, text and shadow follow the shorter side of the canvas
        let s = w.min(h) as f32;
        let pad = PAD * s / 72.0;
        let inner = Area {
            x: pad,
            y: pad,
            w: w as f32 - 2.0 * pad,
            h: h as f32 - 2.0 * pad,
        };
        let box_ = s * ICON_ONLY;
        let icon_only = Area {
            x: (w as f32 - box_) / 2.0,
            y: (h as f32 - box_) / 2.0,
            w: box_,
            h: box_,
        };
        let full = Area {
            x: 0.0,
            y: 0.0,
            w: w as f32,
            h: h as f32,
        };
        let (image, icon) = (&job.image, &job.icon);
        let text = spec.text.as_deref().filter(|t| !t.is_empty());

        let (icon_area, text_area, text_max) = match (icon.is_some(), text) {
            // big icon on top, the label in a band hugging the bottom edge
            (true, Some(t)) => {
                // a second line of text takes room from the icon, so it stays readable
                let lines = wrap(&self.font, s * 0.22, t, inner.w).0.len();
                let band = if lines > 1 {
                    TEXT_BAND_MULTI
                } else {
                    TEXT_BAND
                };
                let top = (s * ICON_TOP).round();
                let margin = (s * TEXT_MARGIN).round();
                let band = (s * band).round();
                let text_y = h as f32 - margin - band;
                let icon_h = text_y - (s * ICON_GAP).round() - top;
                (
                    Some(Area {
                        x: (w as f32 - icon_h) / 2.0,
                        y: top,
                        w: icon_h,
                        h: icon_h,
                    }),
                    Some(Area {
                        x: pad,
                        y: text_y,
                        w: inner.w,
                        h: band,
                    }),
                    // every line must fit in the band, so the label never runs into the icon
                    (s * 0.22).min(band / (lines.max(1) as f32 * LINE)),
                )
            }
            (true, None) => (Some(icon_only), None, 0.0),
            // label along the bottom of a full-bleed picture
            (false, Some(_)) if image.is_some() => (
                None,
                Some(Area {
                    y: inner.y + inner.h * 0.55,
                    h: inner.h * 0.45,
                    ..inner
                }),
                s * 0.22,
            ),
            (false, Some(_)) => (None, Some(inner), s * 0.32),
            (false, None) => (None, None, 0.0),
        };

        match image {
            Some(Img::Ready(im)) => {
                composite(&mut img, &self.scaled.get(im, Fit::Cover(w, h)), 0, 0);
                if text.is_some() {
                    darken_bottom(&mut img);
                }
            }
            Some(Img::Failed) => {
                let top = Area {
                    h: full.h / 2.0,
                    ..full
                };
                self.draw_err(&mut img, full, top);
            }
            Some(Img::Loading | Img::Missing) | None => {}
        }
        match (icon, icon_area) {
            (Some(Img::Ready(im)), Some(area)) => self.draw_icon(&mut img, im, area),
            (Some(Img::Failed), Some(area)) => self.draw_err(&mut img, area, area),
            _ => {}
        }
        if let (Some(text), Some(area)) = (text, text_area) {
            if image.is_some() {
                // drop shadow so the label stays legible on busy pictures
                let o = s / 72.0;
                let shadow = Area {
                    x: area.x + o,
                    y: area.y + o,
                    ..area
                };
                self.draw_text(
                    &mut img,
                    text,
                    shadow,
                    Color::BLACK,
                    spec.font_size,
                    text_max,
                    false,
                );
            }
            let bottom = icon.is_some() && image.is_none();
            // under an icon even an explicit size may not overflow the band into the icon
            let font_size = if bottom {
                spec.font_size.map(|f| f.min(text_max))
            } else {
                spec.font_size
            };
            self.draw_text(&mut img, text, area, spec.fg, font_size, text_max, bottom);
        }
        img
    }

    fn draw_icon(&self, img: &mut RgbImage, icon: &Loaded, area: Area) {
        let (iw, ih) = (icon.img.width() as f32, icon.img.height() as f32);
        let k = (area.w / iw).min(area.h / ih);
        let (tw, th) = (
            ((iw * k).round() as u32).max(1),
            ((ih * k).round() as u32).max(1),
        );
        let scaled = self.scaled.get(icon, Fit::Exact(tw, th));
        let ox = (area.x + (area.w - tw as f32) / 2.0).round() as i64;
        let oy = (area.y + (area.h - th as f32) / 2.0).round() as i64;
        composite(img, &scaled, ox, oy);
    }

    /// Fills `fill` with the error colour and writes "ERR" in `label`.
    fn draw_err(&self, img: &mut RgbImage, fill: Area, label: Area) {
        let (x0, y0) = (fill.x as u32, fill.y as u32);
        let (x1, y1) = (
            ((fill.x + fill.w) as u32).min(img.width()),
            ((fill.y + fill.h) as u32).min(img.height()),
        );
        for y in y0..y1 {
            for x in x0..x1 {
                img.put_pixel(x, y, Rgb(ERR_BG.0));
            }
        }
        self.draw_text(
            img,
            "ERR",
            label,
            Color::WHITE,
            None,
            (label.h * 0.6).min(img.width() as f32 * 0.32),
            false,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_text(
        &self,
        img: &mut RgbImage,
        text: &str,
        area: Area,
        fg: Color,
        font_size: Option<f32>,
        auto_max: f32,
        bottom: bool,
    ) {
        let (px, lines) = match font_size {
            Some(px) => (px, wrap(&self.font, px, text, area.w).0),
            None => self.fit(text, area, auto_max),
        };
        let sf = self.font.as_scaled(PxScale::from(px));
        let line_h = px * LINE;
        let total_h = line_h * lines.len() as f32;
        let mut top = if bottom {
            area.y + area.h - total_h
        } else {
            area.y + (area.h - total_h) / 2.0
        };
        for line in &lines {
            let w = text_width(&self.font, px, line);
            let x = area.x + (area.w - w) / 2.0;
            let baseline = top + (line_h - (sf.ascent() - sf.descent())) / 2.0 + sf.ascent();
            draw_line(img, &self.font, px, line, x, baseline, fg);
            top += line_h;
        }
    }

    /// Largest font size (<= `max`) at which the wrapped text fits without breaking words.
    fn fit(&self, text: &str, area: Area, max: f32) -> (f32, Vec<String>) {
        let mut px = max.max(MIN_FONT_PX).floor();
        loop {
            let (lines, broke) = wrap(&self.font, px, text, area.w);
            let fits = lines.len() as f32 * px * LINE <= area.h;
            if (fits && !broke) || px <= MIN_FONT_PX {
                return (px, lines);
            }
            px -= 1.0;
        }
    }
}

fn text_width(font: &FontRef, px: f32, s: &str) -> f32 {
    let sf = font.as_scaled(PxScale::from(px));
    let mut w = 0.0;
    let mut prev = None;
    for c in s.chars() {
        let id = sf.glyph_id(c);
        if let Some(p) = prev {
            w += sf.kern(p, id);
        }
        w += sf.h_advance(id);
        prev = Some(id);
    }
    w
}

/// Greedy word wrap. Returns the lines and whether any word had to be split mid-word.
fn wrap(font: &FontRef, px: f32, text: &str, max_w: f32) -> (Vec<String>, bool) {
    let mut lines = Vec::new();
    let mut broke = false;
    for para in text.split('\n') {
        let mut cur = String::new();
        for word in para.split_whitespace() {
            let candidate = if cur.is_empty() {
                word.to_string()
            } else {
                format!("{cur} {word}")
            };
            if text_width(font, px, &candidate) <= max_w {
                cur = candidate;
                continue;
            }
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            // word alone too wide: split by characters
            let mut piece = String::new();
            for c in word.chars() {
                piece.push(c);
                if text_width(font, px, &piece) > max_w && piece.chars().count() > 1 {
                    piece.pop();
                    lines.push(std::mem::take(&mut piece));
                    piece.push(c);
                    broke = true;
                }
            }
            cur = piece;
        }
        lines.push(cur);
    }
    (lines, broke)
}

fn draw_line(
    img: &mut RgbImage,
    font: &FontRef,
    px: f32,
    line: &str,
    x: f32,
    baseline: f32,
    fg: Color,
) {
    let scale = PxScale::from(px);
    let sf = font.as_scaled(scale);
    let mut cx = x;
    let mut prev = None;
    for c in line.chars() {
        let id = sf.glyph_id(c);
        if let Some(p) = prev {
            cx += sf.kern(p, id);
        }
        let glyph = id.with_scale_and_position(scale, point(cx, baseline));
        cx += sf.h_advance(id);
        prev = Some(id);
        if let Some(og) = font.outline_glyph(glyph) {
            let b = og.px_bounds();
            og.draw(|gx, gy, cov| {
                let (px_x, px_y) = (b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32);
                if px_x >= 0
                    && px_y >= 0
                    && (px_x as u32) < img.width()
                    && (px_y as u32) < img.height()
                {
                    blend(
                        img.get_pixel_mut(px_x as u32, px_y as u32),
                        fg.0,
                        cov.clamp(0.0, 1.0),
                    );
                }
            });
        }
    }
}

fn blend(dst: &mut Rgb<u8>, src: [u8; 3], a: f32) {
    for (d, s) in dst.0.iter_mut().zip(src) {
        *d = (*d as f32 * (1.0 - a) + s as f32 * a).round() as u8;
    }
}

/// Scales `src` (premultiplied) to cover a `w` x `h` rectangle and center-crops the overflow.
pub fn cover(src: &RgbaImage, w: u32, h: u32) -> RgbaImage {
    let k = (w as f32 / src.width() as f32).max(h as f32 / src.height() as f32);
    let sw = ((src.width() as f32 * k).round() as u32).max(w);
    let sh = ((src.height() as f32 * k).round() as u32).max(h);
    let scaled = resize_premul(src, sw, sh, false);
    let mut out = imageops::crop_imm(&scaled, (sw - w) / 2, (sh - h) / 2, w, h).to_image();
    unpremultiply(&mut out);
    out
}

/// Crops a picture's fully transparent border, so icons drawn with different amounts of padding
/// in their files come out the same size and centered on their visible shape. Opaque pictures
/// (and fully transparent ones) are returned unchanged.
fn trim_transparent(img: RgbaImage) -> RgbaImage {
    let (w, h) = img.dimensions();
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for (x, y, p) in img.enumerate_pixels() {
        if p.0[3] > TRIM_ALPHA {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x + 1);
            y1 = y1.max(y + 1);
        }
    }
    if x0 >= x1 || (x0, y0, x1, y1) == (0, 0, w, h) {
        return img;
    }
    imageops::crop_imm(&img, x0, y0, x1 - x0, y1 - y0).to_image()
}

fn premultiply(img: &mut RgbaImage) {
    for p in img.pixels_mut() {
        let a = p.0[3] as u32;
        for c in &mut p.0[..3] {
            *c = ((*c as u32 * a + 127) / 255) as u8;
        }
    }
}

fn unpremultiply(img: &mut RgbaImage) {
    for p in img.pixels_mut() {
        let a = p.0[3] as u32;
        for c in &mut p.0[..3] {
            if let Some(q) = (*c as u32 * 255 + a / 2).checked_div(a) {
                *c = q.min(255) as u8;
            }
        }
    }
}

/// Lanczos resize of a premultiplied picture (so transparent pixels do not bleed their colour
/// into the edges); optionally converts the result back to straight alpha.
fn resize_premul(src: &RgbaImage, w: u32, h: u32, straight: bool) -> RgbaImage {
    let mut out = imageops::resize(src, w, h, FilterType::Lanczos3);
    if straight {
        unpremultiply(&mut out);
    }
    out
}

/// Alpha-composites `over` onto `img` with its top-left corner at (`ox`, `oy`).
fn composite(img: &mut RgbImage, over: &RgbaImage, ox: i64, oy: i64) {
    for (x, y, p) in over.enumerate_pixels() {
        let (dx, dy) = (ox + x as i64, oy + y as i64);
        if dx < 0 || dy < 0 || dx >= img.width() as i64 || dy >= img.height() as i64 {
            continue;
        }
        blend(
            img.get_pixel_mut(dx as u32, dy as u32),
            [p.0[0], p.0[1], p.0[2]],
            p.0[3] as f32 / 255.0,
        );
    }
}

/// Subtle dark gradient over the lower half so text on top of a picture stays legible.
fn darken_bottom(img: &mut RgbImage) {
    let h = img.height() as f32;
    let start = h * 0.4;
    for y in 0..img.height() {
        let t = ((y as f32 - start) / (h - start)).clamp(0.0, 1.0);
        if t == 0.0 {
            continue;
        }
        for x in 0..img.width() {
            blend(img.get_pixel_mut(x, y), [0, 0, 0], 0.7 * t);
        }
    }
}

fn encode_png(img: &RgbImage) -> Vec<u8> {
    let mut buf = Vec::new();
    PngEncoder::new(&mut buf)
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            ExtendedColorType::Rgb8,
        )
        .expect("png encoding of an in-memory image cannot fail");
    buf
}

fn encode_jpeg(img: &RgbImage) -> Vec<u8> {
    let mut buf = Vec::new();
    JpegEncoder::new_with_quality(&mut buf, JPEG_QUALITY)
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            ExtendedColorType::Rgb8,
        )
        .expect("jpeg encoding of an in-memory image cannot fail");
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_parsing() {
        assert_eq!(Color::parse("#f5b400"), Some(Color([0xf5, 0xb4, 0x00])));
        assert_eq!(Color::parse("#fff"), Some(Color([255, 255, 255])));
        assert_eq!(Color::parse("#11223344"), Some(Color([0x11, 0x22, 0x33])));
        assert_eq!(Color::parse("red"), None);
        assert_eq!(Color::parse("#12"), None);
    }

    #[test]
    fn output_has_requested_dimensions() {
        let mut r = Renderer::new(PathBuf::from("."));
        for size in [72u32, 96, 120] {
            let spec = RenderSpec {
                text: Some("Living Room Lamp".into()),
                ..Default::default()
            };
            let jpeg = r.render(&spec, size);
            let img = image::load_from_memory(&jpeg).unwrap();
            assert_eq!((img.width(), img.height()), (size, size));
        }
    }

    #[test]
    fn text_changes_pixels_and_rendering_is_deterministic() {
        let mut r = Renderer::new(PathBuf::from("."));
        let a = r.render(
            &RenderSpec {
                text: Some("A".into()),
                ..Default::default()
            },
            72,
        );
        let b = r.render(
            &RenderSpec {
                text: Some("B".into()),
                ..Default::default()
            },
            72,
        );
        assert_ne!(a, b);
        assert_eq!(
            a,
            r.render(
                &RenderSpec {
                    text: Some("A".into()),
                    ..Default::default()
                },
                72
            )
        );
    }

    #[test]
    fn long_word_wraps_without_panicking() {
        let mut r = Renderer::new(PathBuf::from("."));
        let spec = RenderSpec {
            text: Some("Supercalifragilisticexpialidocious and more words here".into()),
            ..Default::default()
        };
        let _ = r.render(&spec, 72);
    }

    #[test]
    fn missing_icon_is_tolerated() {
        let mut r = Renderer::new(PathBuf::from("/nonexistent"));
        let spec = RenderSpec {
            icon: Some("nope.png".into()),
            text: Some("x".into()),
            ..Default::default()
        };
        let _ = r.render(&spec, 72);
    }

    fn png_dir(tag: &str, files: &[(&str, RgbaImage)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sdc-render-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, img) in files {
            img.save(dir.join(name)).unwrap();
        }
        dir
    }

    fn px(jpeg: &[u8], x: u32, y: u32) -> [u8; 3] {
        image::load_from_memory(jpeg)
            .unwrap()
            .to_rgb8()
            .get_pixel(x, y)
            .0
    }

    fn near(a: [u8; 3], b: [u8; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| x.abs_diff(y) < 24)
    }

    #[test]
    fn cover_fills_square_and_crops_the_center() {
        // 2:1 picture, left half red, right half blue
        let mut src = RgbaImage::new(200, 100);
        for (x, _, p) in src.enumerate_pixels_mut() {
            *p = if x < 100 {
                image::Rgba([255, 0, 0, 255])
            } else {
                image::Rgba([0, 0, 255, 255])
            };
        }
        for size in [72u32, 96] {
            let out = cover(&src, size, size);
            assert_eq!((out.width(), out.height()), (size, size));
            assert_eq!(out.get_pixel(2, size / 2).0, [255, 0, 0, 255]);
            assert_eq!(out.get_pixel(size - 3, size / 2).0, [0, 0, 255, 255]);
        }
        // tall pictures too
        let out = cover(&RgbaImage::new(30, 300), 72, 72);
        assert_eq!((out.width(), out.height()), (72, 72));
    }

    #[test]
    fn image_is_full_bleed_icon_keeps_padding_and_alpha() {
        let green = RgbaImage::from_pixel(40, 20, image::Rgba([0, 200, 0, 255]));
        let clear = RgbaImage::from_pixel(32, 32, image::Rgba([255, 0, 0, 0]));
        let dir = png_dir("bleed", &[("g.png", green), ("clear.png", clear)]);
        let mut r = Renderer::new(dir.clone());
        let bg = Color([0x20, 0x20, 0x80]);

        let full = r.render(
            &RenderSpec {
                image: Some("g.png".into()),
                bg,
                ..Default::default()
            },
            72,
        );
        for (x, y) in [(0, 0), (71, 0), (0, 71), (71, 71), (36, 36)] {
            assert!(near(px(&full, x, y), [0, 200, 0]), "({x},{y})");
        }

        // `icon` is fitted with padding over bg: corners stay bg, the middle is the picture
        let icon = r.render(
            &RenderSpec {
                icon: Some("g.png".into()),
                bg,
                ..Default::default()
            },
            72,
        );
        assert!(near(px(&icon, 1, 1), bg.0));
        assert!(near(px(&icon, 36, 36), [0, 200, 0]));

        // a fully transparent icon leaves the background untouched
        let transparent = r.render(
            &RenderSpec {
                icon: Some("clear.png".into()),
                bg,
                ..Default::default()
            },
            72,
        );
        assert_eq!(transparent, r.blank_with(bg, 72));

        // text over a picture is drawn on a darkened bottom
        let labelled = r.render(
            &RenderSpec {
                image: Some("g.png".into()),
                text: Some("Movie".into()),
                bg,
                ..Default::default()
            },
            72,
        );
        assert!(px(&labelled, 2, 70)[1] < 120, "bottom is darkened");
        assert!(near(px(&labelled, 36, 2), [0, 200, 0]), "top is untouched");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_image_shows_error_only_in_its_area() {
        let mut r = Renderer::new(PathBuf::from("/nonexistent"));
        let bg = Color([0x20, 0x20, 0x80]);
        let icon = r.render(
            &RenderSpec {
                icon: Some("nope.png".into()),
                text: Some("x".into()),
                bg,
                ..Default::default()
            },
            72,
        );
        assert!(
            near(px(&icon, 36, 15), ERR_BG.0),
            "icon area is the error tile"
        );
        assert!(
            near(px(&icon, 3, 68), bg.0),
            "the rest of the key is intact"
        );
        let full = r.render(
            &RenderSpec {
                image: Some("nope.png".into()),
                bg,
                ..Default::default()
            },
            72,
        );
        assert!(near(px(&full, 2, 70), ERR_BG.0));
    }

    #[test]
    fn remote_image_without_fetcher_fails_instead_of_blocking() {
        let mut r = Renderer::new(PathBuf::from("."));
        let spec = RenderSpec {
            image: Some("http://127.0.0.1:9/never.png".into()),
            ..Default::default()
        };
        assert_eq!(
            r.remote_keys(&spec),
            ["http://127.0.0.1:9/never.png".to_string()]
        );
        let _ = r.render(&spec, 72);
        assert_eq!(r.pending(), 0);
    }

    #[test]
    fn lru_evicts_least_recently_used_but_not_pending() {
        let mut l: Lru<u8> = Lru::new(2);
        assert!(l.insert("a".into(), 0, |_| false).is_empty());
        assert!(l.insert("b".into(), 0, |_| false).is_empty());
        l.get("a"); // b is now the oldest
        assert_eq!(l.insert("c".into(), 0, |_| false), ["b"]);
        // entries in flight (value 1) stay even over the cap
        let mut l: Lru<u8> = Lru::new(1);
        l.insert("p".into(), 1, |v| *v == 1);
        assert!(l.insert("q".into(), 1, |v| *v == 1).is_empty());
        assert_eq!(l.map.len(), 2);
    }

    #[test]
    fn remote_cache_is_bounded_and_forgets_scalings() {
        let mut r = Renderer::new(PathBuf::from("."));
        let spec = |i: usize| RenderSpec {
            image: Some(format!("http://h/{i}.png")),
            ..Default::default()
        };
        for i in 0..MAX_REMOTE + 20 {
            let key = format!("http://h/{i}.png");
            if i % 2 == 0 {
                r.finish_fetch(
                    &key,
                    Ok(RgbaImage::from_pixel(8, 8, image::Rgba([9, 9, 9, 255]))),
                );
                r.render(&spec(i), 72); // scales it
            } else {
                r.finish_fetch(&key, Err("HTTP 404".into()));
            }
        }
        assert_eq!(r.remote.map.len(), MAX_REMOTE);
        let scaled = r.painter.scaled.map.lock().unwrap().len();
        assert!(
            scaled <= MAX_REMOTE / 2,
            "scalings of evicted images are dropped: {scaled}"
        );
        // the newest entries survive, the oldest are gone
        assert!(r
            .remote
            .get(&format!("http://h/{}.png", MAX_REMOTE + 19))
            .is_some());
        assert!(r.remote.get("http://h/0.png").is_none());
    }

    #[test]
    fn hot_images_survive_a_flood_of_changing_sources() {
        let mut r = Renderer::new(PathBuf::from("."));
        let hot = "http://h/hot.png";
        let px = || Ok(RgbaImage::from_pixel(8, 8, image::Rgba([9, 9, 9, 255])));
        r.finish_fetch(hot, px());
        let spec = RenderSpec {
            image: Some(hot.into()),
            ..Default::default()
        };
        for i in 0..MAX_REMOTE * 3 {
            r.finish_fetch(&format!("http://h/track{i}.png"), px());
            r.render(&spec, 72); // the icon is on screen, so it is used all the time
        }
        assert!(matches!(r.remote.get(hot), Some(Slot::Ready(_))));
    }

    #[test]
    fn missing_ha_artwork_renders_without_an_error_tile() {
        let ha = crate::assets::HaApi::new("http://127.0.0.1:9", "t");
        let assets = Assets::new(PathBuf::from("/nonexistent"), None).with_ha(Some(ha));
        let mut r = Renderer::with_assets(assets);
        let bg = Color([0x20, 0x20, 0x80]);
        let path = "/api/media_player_proxy/media_player.tv?token=x";
        let spec = RenderSpec {
            image: Some(path.into()),
            text: Some("x".into()),
            bg,
            ..Default::default()
        };
        // no fetcher: fails right away; then a failed download reported by the fetcher
        let first = r.render(&spec, 72);
        assert!(near(px(&first, 2, 2), bg.0), "no ERR tile");
        let key = r.remote_keys(&spec).pop().unwrap();
        r.finish_fetch(&key, Err("HTTP 404".into()));
        let again = r.render(&spec, 72);
        assert!(near(px(&again, 2, 2), bg.0));
        assert!(
            near(px(&again, 2, 70), bg.0),
            "laid out as if there were no picture"
        );
        // other failures still show it, and so does HA being unconfigured
        let mut plain = Renderer::new(PathBuf::from("/nonexistent"));
        assert!(near(px(&plain.render(&spec, 72), 2, 2), ERR_BG.0));
    }

    /// A dir with `n` distinct large noisy-alpha icons (like real artwork: big, transparent).
    fn big_icons(tag: &str, n: usize, side: u32) -> (PathBuf, Vec<RenderSpec>) {
        let files: Vec<_> = (0..n)
            .map(|i| {
                let img = RgbaImage::from_fn(side, side, |x, y| {
                    let a = if (x / 64 + y / 64) % 2 == 0 { 255 } else { 90 };
                    image::Rgba([(x + i as u32 * 7) as u8, (y * 3) as u8, (x ^ y) as u8, a])
                });
                (format!("i{i}.png"), img)
            })
            .collect();
        let refs: Vec<_> = files.iter().map(|(n, i)| (n.as_str(), i.clone())).collect();
        let dir = png_dir(tag, &refs);
        let specs = files
            .iter()
            .map(|(name, _)| RenderSpec {
                icon: Some(name.clone()),
                text: Some("Lamp".into()),
                ..Default::default()
            })
            .collect();
        (dir, specs)
    }

    #[test]
    fn scaled_pictures_are_computed_once_per_size() {
        // one source used by 15 keys, then a second page render, then another key size
        let (dir, mut specs) = big_icons("cache", 1, 1024);
        let spec = specs.pop().unwrap();
        let page = vec![spec; 15];
        let mut r = Renderer::new(dir.clone());
        let first = r.render_many(&page, 72);
        assert_eq!(r.scalings(), 1, "15 keys share one scaling");
        let second = r.render_many(&page, 72);
        assert_eq!(r.scalings(), 1, "a re-render reuses it");
        assert_eq!(first, second);
        r.render_many(&page[..1], 96);
        assert_eq!(r.scalings(), 2, "a new key size scales again");
        r.clear_icon_cache();
        r.render_many(&page[..1], 72);
        assert_eq!(r.scalings(), 3, "a reload drops the cache");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn parallel_render_matches_serial() {
        let (dir, specs) = big_icons("par", 6, 300);
        let mut r = Renderer::new(dir.clone());
        let serial: Vec<_> = specs.iter().map(|s| r.render(s, 72)).collect();
        assert_eq!(r.render_many(&specs, 72), serial);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// `cargo test -p sdc-server --release -- --ignored --nocapture page_render_timing`
    #[test]
    #[ignore]
    fn page_render_timing() {
        let (dir, specs) = big_icons("bench", 15, 1024);
        let mut r = Renderer::new(dir.clone());
        for pass in ["cold", "warm"] {
            let t = Instant::now();
            let serial: Vec<_> = specs.iter().map(|s| r.render(s, 72)).collect();
            eprintln!("{pass} serial   page of 15 icons: {:?}", t.elapsed());
            let t = Instant::now();
            let many = r.render_many(&specs, 72);
            eprintln!("{pass} parallel page of 15 icons: {:?}", t.elapsed());
            assert_eq!(serial, many);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn span_tiles_are_crops_of_one_canvas_skipping_the_gaps() {
        // a picture whose red is x and green is y over the 162x162 canvas of a 2x2 span
        let geom = SpanGeom {
            cols: 2,
            rows: 2,
            key: 72,
            gap: 18,
        };
        assert_eq!(geom.canvas(), (162, 162));
        let mut src = RgbaImage::new(162, 162);
        for (x, y, p) in src.enumerate_pixels_mut() {
            *p = image::Rgba([x as u8, y as u8, 0, 255]);
        }
        let dir = png_dir("span", &[("grad.png", src)]);
        let mut r = Renderer::new(dir);
        let spec = RenderSpec {
            image: Some("grad.png".into()),
            ..Default::default()
        };
        let tiles = r.render_span_images(&spec, geom);
        assert_eq!(tiles.len(), 4);
        for t in &tiles {
            assert_eq!((t.width(), t.height()), (72, 72));
        }
        for (i, t) in tiles.iter().enumerate() {
            let (c, row) = (i as u32 % 2, i as u32 / 2);
            let (ox, oy) = (c * 90, row * 90);
            for (x, y) in [(0, 0), (71, 0), (0, 71), (71, 71), (30, 40)] {
                let p = t.get_pixel(x, y).0;
                let want = [(ox + x) as u8, (oy + y) as u8, 0];
                assert!(
                    p[0].abs_diff(want[0]) <= 2 && p[1].abs_diff(want[1]) <= 2,
                    "tile {i} ({x},{y}): {p:?} vs {want:?}"
                );
            }
        }
        // the edge of the left tile and the start of the right one skip the 18 px gap
        assert!(tiles[1].get_pixel(0, 0).0[0] > tiles[0].get_pixel(71, 0).0[0] + 15);
        // the JPEG path gives the same number of tiles
        let jobs = vec![r.prepare_span(&spec, geom), r.prepare(&spec, 72)];
        assert_eq!(r.encode_all(jobs).len(), 5);
    }

    #[test]
    fn span_text_is_laid_out_on_the_whole_canvas() {
        let geom = SpanGeom {
            cols: 2,
            rows: 2,
            key: 72,
            gap: 18,
        };
        let mut r = Renderer::new(PathBuf::from("."));
        let spec = RenderSpec {
            text: Some("W".into()),
            ..Default::default()
        };
        let tiles = r.render_span_images(&spec, geom);
        let bg = spec.bg.0;
        // one big glyph centred on the canvas straddles the gaps: every tile shows a part of
        // it near the canvas centre and nothing near its own outer corner
        let ink = |t: &RgbImage, xs: std::ops::Range<u32>, ys: std::ops::Range<u32>| {
            xs.flat_map(|x| ys.clone().map(move |y| (x, y)))
                .any(|(x, y)| t.get_pixel(x, y).0 != bg)
        };
        assert!(ink(&tiles[0], 50..72, 50..72));
        assert!(ink(&tiles[3], 0..22, 0..22));
        assert!(!ink(&tiles[0], 0..20, 0..20));
        assert!(!ink(&tiles[3], 52..72, 52..72));
        assert_eq!(gap_px(18, 96), 24);
    }

    /// Bounding box (y range) of pixels matching `f`.
    fn y_extent(img: &RgbImage, f: impl Fn(&Rgb<u8>) -> bool) -> Option<(u32, u32)> {
        let ys: Vec<u32> = img
            .enumerate_pixels()
            .filter(|(_, _, p)| f(p))
            .map(|(_, y, _)| y)
            .collect();
        Some((*ys.iter().min()?, *ys.iter().max()?))
    }

    #[test]
    fn icon_with_text_is_big_and_the_text_hugs_the_bottom() {
        let green = RgbaImage::from_pixel(64, 64, image::Rgba([0, 200, 0, 255]));
        let dir = png_dir("layout", &[("sq.png", green)]);
        for size in [72u32, 144] {
            let mut r = Renderer::new(dir.clone());
            let img = r.render_image(
                &RenderSpec {
                    icon: Some("sq.png".into()),
                    text: Some("80%".into()),
                    ..Default::default()
                },
                size,
            );
            let s = size as f32;
            let is_icon = |p: &Rgb<u8>| p.0[1] > 150 && p.0[0] < 60;
            let (top, bottom) = y_extent(&img, is_icon).unwrap();
            let icon_h = (bottom - top + 1) as f32;
            // before: the icon was at most half of the inner area (~0.44 of the key)
            assert!(icon_h >= 0.52 * s, "icon {icon_h}px of {size}");
            assert!(top as f32 <= 0.1 * s);
            let (t_top, t_bottom) = y_extent(&img, |p| p.0[0] > 150 && p.0[1] > 150).unwrap();
            assert!(t_top > bottom, "text below the icon");
            assert!(t_bottom as f32 >= 0.84 * s, "text baseline low: {t_bottom}");
            assert!(
                t_bottom as f32 <= s - 0.03 * s,
                "text keeps a margin: {t_bottom}"
            );
        }
    }

    #[test]
    fn icons_are_trimmed_and_centered_on_their_visible_pixels() {
        // a glyph drawn in the top-left corner of a big transparent canvas
        let mut padded = RgbaImage::from_pixel(100, 100, image::Rgba([0, 0, 0, 0]));
        for y in 5..25 {
            for x in 10..30 {
                padded.put_pixel(x, y, image::Rgba([0, 200, 0, 255]));
            }
        }
        let trimmed = trim_transparent(padded);
        assert_eq!(trimmed.dimensions(), (20, 20));
        let opaque = RgbaImage::from_pixel(8, 4, image::Rgba([1, 2, 3, 255]));
        assert_eq!(trim_transparent(opaque).dimensions(), (8, 4));
        let empty = RgbaImage::from_pixel(8, 4, image::Rgba([0, 0, 0, 0]));
        assert_eq!(trim_transparent(empty).dimensions(), (8, 4));

        let mut corner = RgbaImage::from_pixel(100, 100, image::Rgba([0, 0, 0, 0]));
        for y in 0..20 {
            for x in 0..20 {
                corner.put_pixel(x, y, image::Rgba([0, 200, 0, 255]));
            }
        }
        let dir = png_dir("trim", &[("corner.png", corner)]);
        let mut r = Renderer::new(dir);
        let img = r.render_image(
            &RenderSpec {
                icon: Some("corner.png".into()),
                ..Default::default()
            },
            72,
        );
        let is_icon = |p: &Rgb<u8>| p.0[1] > 150 && p.0[0] < 60;
        let (top, bottom) = y_extent(&img, is_icon).unwrap();
        let mid = (top + bottom) as f32 / 2.0;
        assert!(
            (mid - 35.5).abs() <= 1.5,
            "centered vertically: {top}..{bottom}"
        );
        assert!(
            (bottom - top + 1) as f32 >= 0.6 * 72.0,
            "fills the icon box"
        );
    }

    #[test]
    fn two_line_icon_text_fits_below_the_icon() {
        let green = RgbaImage::from_pixel(64, 64, image::Rgba([0, 200, 0, 255]));
        let dir = png_dir("layout2", &[("sq.png", green)]);
        let mut r = Renderer::new(dir);
        let img = r.render_image(
            &RenderSpec {
                icon: Some("sq.png".into()),
                text: Some("Office\n100%".into()),
                ..Default::default()
            },
            72,
        );
        let (_, bottom) = y_extent(&img, |p| p.0[1] > 150 && p.0[0] < 60).unwrap();
        let (t_top, t_bottom) = y_extent(&img, |p| p.0[0] > 150 && p.0[1] > 150).unwrap();
        assert!(t_top > bottom && t_bottom < 72);
    }
}
