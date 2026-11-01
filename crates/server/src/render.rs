//! Key image renderer: turns a [`RenderSpec`] into a JPEG at the device's key size.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use ab_glyph::{point, Font, FontRef, PxScale, ScaleFont};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::{self, FilterType};
use image::{ExtendedColorType, ImageEncoder, Rgb, RgbImage, RgbaImage};

use crate::assets::{redact, Assets, Fetcher, Source};

static FONT_BYTES: &[u8] = include_bytes!("../assets/OpenSans.ttf");

const JPEG_QUALITY: u8 = 90;
const PAD: f32 = 4.0;
const MIN_FONT_PX: f32 = 9.0;
const LINE: f32 = 1.15;
/// How long a failed download is remembered before it is tried again.
const FAIL_TTL: Duration = Duration::from_secs(60);
const ERR_BG: Color = Color([0xb0, 0x00, 0x20]);

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

pub struct Renderer {
    font: FontRef<'static>,
    assets: Assets,
    /// `icon`/`image` string -> where it lives (None: unresolvable, already logged).
    resolved: HashMap<String, Option<Source>>,
    local: HashMap<PathBuf, Option<Rc<RgbaImage>>>,
    /// Downloaded images by remote cache key.
    remote: HashMap<String, Slot>,
    /// Without one, remote images are reported as failed (they are never fetched inline).
    fetcher: Option<Fetcher>,
}

enum Slot {
    Loading,
    Ready(Rc<RgbaImage>),
    Failed(Instant),
}

/// Result of looking an image up for one render; never waits on the network.
enum Img {
    Ready(Rc<RgbaImage>),
    Loading,
    Failed,
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
            font: FontRef::try_from_slice(FONT_BYTES).expect("embedded font is valid"),
            assets,
            resolved: HashMap::new(),
            local: HashMap::new(),
            remote: HashMap::new(),
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
        self.remote.retain(|_, s| !matches!(s, Slot::Failed(_)));
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
            Ok(img) => Slot::Ready(Rc::new(img)),
            Err(e) => {
                tracing::warn!("cannot fetch image {}: {e}", redact(key));
                Slot::Failed(Instant::now())
            }
        };
        self.remote.insert(key.to_string(), slot);
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
        let image = spec.image.as_deref().map(|n| self.lookup(n));
        let icon = spec.icon.as_deref().map(|n| self.lookup(n));
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

        match &image {
            Some(Img::Ready(im)) => {
                composite(&mut img, &cover(im, size), 0, 0);
                if text.is_some() {
                    darken_bottom(&mut img);
                }
            }
            Some(Img::Failed) => {
                let top = Area { h: s / 2.0, ..full };
                self.draw_err(&mut img, full, top);
            }
            Some(Img::Loading) | None => {}
        }
        match (&icon, icon_area) {
            (Some(Img::Ready(im)), Some(area)) => draw_icon(&mut img, im, area),
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
                        Ok(i) => Some(Rc::new(i.to_rgba8())),
                        Err(e) => {
                            tracing::warn!("cannot load icon {}: {e}", full.display());
                            None
                        }
                    });
            return loaded.clone().map_or(Img::Failed, Img::Ready);
        };
        match self.remote.get(&key) {
            Some(Slot::Ready(i)) => return Img::Ready(i.clone()),
            Some(Slot::Loading) => return Img::Loading,
            Some(Slot::Failed(at)) if at.elapsed() < FAIL_TTL => return Img::Failed,
            _ => {}
        }
        let started = match (&self.fetcher, self.assets.download_url(&src)) {
            (Some(f), Ok(url)) => {
                f.spawn(key.clone(), url);
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
            self.remote.insert(key, Slot::Loading);
            Img::Loading
        } else {
            self.remote.insert(key, Slot::Failed(Instant::now()));
            Img::Failed
        }
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

fn draw_icon(img: &mut RgbImage, icon: &RgbaImage, area: Area) {
    let (iw, ih) = (icon.width() as f32, icon.height() as f32);
    let k = (area.w / iw).min(area.h / ih);
    let (tw, th) = (
        ((iw * k).round() as u32).max(1),
        ((ih * k).round() as u32).max(1),
    );
    let scaled = resize_rgba(icon, tw, th);
    let ox = (area.x + (area.w - tw as f32) / 2.0).round() as i64;
    let oy = (area.y + (area.h - th as f32) / 2.0).round() as i64;
    composite(img, &scaled, ox, oy);
}

/// Scales `src` to cover a `size` x `size` square and center-crops the overflow.
pub fn cover(src: &RgbaImage, size: u32) -> RgbaImage {
    let k = (size as f32 / src.width() as f32).max(size as f32 / src.height() as f32);
    let w = ((src.width() as f32 * k).round() as u32).max(size);
    let h = ((src.height() as f32 * k).round() as u32).max(size);
    let scaled = resize_rgba(src, w, h);
    imageops::crop_imm(&scaled, (w - size) / 2, (h - size) / 2, size, size).to_image()
}

/// Lanczos resize that does not bleed the colour of transparent pixels into the edges
/// (resizing is done on premultiplied alpha).
fn resize_rgba(src: &RgbaImage, w: u32, h: u32) -> RgbaImage {
    let mut pre = src.clone();
    for p in pre.pixels_mut() {
        let a = p.0[3] as u32;
        for c in &mut p.0[..3] {
            *c = ((*c as u32 * a + 127) / 255) as u8;
        }
    }
    let mut out = imageops::resize(&pre, w, h, FilterType::Lanczos3);
    for p in out.pixels_mut() {
        let a = p.0[3] as u32;
        if a > 0 {
            for c in &mut p.0[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
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
}
