//! Key image renderer: turns a [`RenderSpec`] into a JPEG at the device's key size.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use ab_glyph::{point, Font, FontRef, PxScale, ScaleFont};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::{self, FilterType};
use image::{ExtendedColorType, ImageEncoder, Rgb, RgbImage, RgbaImage};

static FONT_BYTES: &[u8] = include_bytes!("../assets/OpenSans.ttf");

const JPEG_QUALITY: u8 = 90;
const PAD: f32 = 4.0;
const MIN_FONT_PX: f32 = 9.0;
const LINE: f32 = 1.15;

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
    pub icon: Option<String>,
    pub font_size: Option<f32>,
}

impl Default for RenderSpec {
    fn default() -> Self {
        Self {
            text: None,
            bg: Color([0x1c, 0x1c, 0x1e]),
            fg: Color::WHITE,
            icon: None,
            font_size: None,
        }
    }
}

impl RenderSpec {
    pub fn error() -> Self {
        Self {
            text: Some("ERR".into()),
            bg: Color([0xb0, 0x00, 0x20]),
            ..Self::default()
        }
    }
}

pub struct Renderer {
    font: FontRef<'static>,
    base_dir: PathBuf,
    icons: HashMap<PathBuf, Option<Rc<RgbaImage>>>,
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
        Self {
            font: FontRef::try_from_slice(FONT_BYTES).expect("embedded font is valid"),
            base_dir,
            icons: HashMap::new(),
        }
    }

    pub fn clear_icon_cache(&mut self) {
        self.icons.clear();
    }

    pub fn blank(&self, size: u32) -> Vec<u8> {
        encode_jpeg(&RgbImage::from_pixel(size, size, Rgb(Color::BLACK.0)))
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
        let icon = spec.icon.as_deref().and_then(|p| self.load_icon(p));
        let text = spec.text.as_deref().filter(|t| !t.is_empty());

        let (icon_area, text_area, text_max) = match (&icon, text) {
            (Some(_), Some(_)) => {
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
            (Some(_), None) => (Some(inner), None, 0.0),
            (None, Some(_)) => (None, Some(inner), s * 0.32),
            (None, None) => (None, None, 0.0),
        };

        if let (Some(icon), Some(area)) = (&icon, icon_area) {
            draw_icon(&mut img, icon, area, spec.bg);
        }
        if let (Some(text), Some(area)) = (text, text_area) {
            self.draw_text(&mut img, text, area, spec.fg, spec.font_size, text_max);
        }
        img
    }

    fn load_icon(&mut self, path: &str) -> Option<Rc<RgbaImage>> {
        let full = self.base_dir.join(path);
        self.icons
            .entry(full.clone())
            .or_insert_with(|| match image::open(&full) {
                Ok(i) => Some(Rc::new(i.to_rgba8())),
                Err(e) => {
                    tracing::warn!("cannot load icon {}: {e}", full.display());
                    None
                }
            })
            .clone()
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

fn draw_icon(img: &mut RgbImage, icon: &RgbaImage, area: Area, _bg: Color) {
    let (iw, ih) = (icon.width() as f32, icon.height() as f32);
    let k = (area.w / iw).min(area.h / ih);
    let (tw, th) = (
        ((iw * k).round() as u32).max(1),
        ((ih * k).round() as u32).max(1),
    );
    let scaled = imageops::resize(icon, tw, th, FilterType::Lanczos3);
    let ox = (area.x + (area.w - tw as f32) / 2.0).round() as i64;
    let oy = (area.y + (area.h - th as f32) / 2.0).round() as i64;
    for (x, y, p) in scaled.enumerate_pixels() {
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
}
