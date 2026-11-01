//! Key image renderer: turns a [`RenderSpec`] into a JPEG at the device's key size.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use ab_glyph::{point, Font, FontRef, PxScale, ScaleFont};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::{self, FilterType};
use image::{ExtendedColorType, ImageEncoder, Rgb, RgbImage, RgbaImage};

use crate::assets::{is_ha_key, redact, Assets, Fetcher, Source};

static FONT_BYTES: &[u8] = include_bytes!("../assets/OpenSans.ttf");

const JPEG_QUALITY: u8 = 90;
const PAD: f32 = 4.0;
const MIN_FONT_PX: f32 = 9.0;
const LINE: f32 = 1.15;
/// How long a failed download is remembered before it is tried again.
const FAIL_TTL: Duration = Duration::from_secs(60);
const ERR_BG: Color = Color([0xb0, 0x00, 0x20]);
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
    /// Cover a square of this size, center-cropped.
    Cover(u32),
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
                Fit::Cover(size) => cover(&src.img, size),
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
    size: u32,
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

    pub fn render_image(&mut self, spec: &RenderSpec, size: u32) -> RgbImage {
        let job = self.prepare(spec, size);
        self.painter.paint(&job)
    }

    /// Looks up the pictures `spec` needs (decoding local files, starting downloads).
    pub fn prepare(&mut self, spec: &RenderSpec, size: u32) -> Job {
        Job {
            spec: spec.clone(),
            size,
            image: spec.image.as_deref().map(|n| self.lookup(n)),
            icon: spec.icon.as_deref().map(|n| self.lookup(n)),
        }
        .without_missing()
    }

    /// Rasterizes and JPEG-encodes `jobs`, spread over the available cores.
    pub fn encode_all(&self, jobs: Vec<Job>) -> Vec<Vec<u8>> {
        let painter = &*self.painter;
        let run = |job: &Job| encode_jpeg(&painter.paint(job));
        let workers = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(jobs.len());
        if workers <= 1 {
            return jobs.iter().map(run).collect();
        }
        let next = AtomicUsize::new(0);
        let mut out: Vec<Option<Vec<u8>>> = vec![None; jobs.len()];
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
        out.into_iter().map(|j| j.expect("every job ran")).collect()
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
    fn paint(&self, job: &Job) -> RgbImage {
        let (spec, size) = (&job.spec, job.size);
        let mut img = RgbImage::from_pixel(size, size, Rgb(spec.bg.0));
        let s = size as f32;
        let pad = PAD * s / 72.0;
        let inner = Area {
            x: pad,
            y: pad,
            w: s - 2.0 * pad,
            h: s - 2.0 * pad,
        };
        let full = Area {
            x: 0.0,
            y: 0.0,
            w: s,
            h: s,
        };
        let (image, icon) = (&job.image, &job.icon);
        let text = spec.text.as_deref().filter(|t| !t.is_empty());

        let (icon_area, text_area, text_max) = match (icon.is_some(), text) {
            (true, Some(_)) => {
                let ih = (inner.h * 0.5).round();
                (
                    Some(Area { h: ih, ..inner }),
                    Some(Area {
                        y: inner.y + ih,
                        h: inner.h - ih,
                        ..inner
                    }),
                    s * 0.22,
                )
            }
            (true, None) => (Some(inner), None, 0.0),
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
                composite(&mut img, &self.scaled.get(im, Fit::Cover(size)), 0, 0);
                if text.is_some() {
                    darken_bottom(&mut img);
                }
            }
            Some(Img::Failed) => {
                let top = Area { h: s / 2.0, ..full };
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
                );
            }
            self.draw_text(&mut img, text, area, spec.fg, spec.font_size, text_max);
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
        );
    }

    fn draw_text(
        &self,
        img: &mut RgbImage,
        text: &str,
        area: Area,
        fg: Color,
        font_size: Option<f32>,
        auto_max: f32,
    ) {
        let (px, lines) = match font_size {
            Some(px) => (px, wrap(&self.font, px, text, area.w).0),
            None => self.fit(text, area, auto_max),
        };
        let sf = self.font.as_scaled(PxScale::from(px));
        let line_h = px * LINE;
        let total_h = line_h * lines.len() as f32;
        let mut top = area.y + (area.h - total_h) / 2.0;
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

/// Scales `src` (premultiplied) to cover a `size` x `size` square and center-crops the overflow.
pub fn cover(src: &RgbaImage, size: u32) -> RgbaImage {
    let k = (size as f32 / src.width() as f32).max(size as f32 / src.height() as f32);
    let w = ((src.width() as f32 * k).round() as u32).max(size);
    let h = ((src.height() as f32 * k).round() as u32).max(size);
    let scaled = resize_premul(src, w, h, false);
    let mut out =
        imageops::crop_imm(&scaled, (w - size) / 2, (h - size) / 2, size, size).to_image();
    unpremultiply(&mut out);
    out
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
        if a > 0 {
            for c in &mut p.0[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
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
            let out = cover(&src, size);
            assert_eq!((out.width(), out.height()), (size, size));
            assert_eq!(out.get_pixel(2, size / 2).0, [255, 0, 0, 255]);
            assert_eq!(out.get_pixel(size - 3, size / 2).0, [0, 0, 255, 255]);
        }
        // tall pictures too
        let out = cover(&RgbaImage::new(30, 300), 72);
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
            near(px(&icon, 6, 6), ERR_BG.0),
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
}
