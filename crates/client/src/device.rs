//! Stream Deck device abstraction: a real HID deck or a fake one for testing.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use elgato_streamdeck::{
    asynchronous::AsyncStreamDeck,
    info::{ImageMirroring, ImageMode, ImageRotation, Kind},
    list_devices, new_hidapi, DeviceStateUpdate,
};
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use sdc_protocol::{DeviceInfo, NativeImage, NativeMirror, NativeMode};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// How often the HID input is polled (Hz).
const POLL_RATE_HZ: f32 = 60.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    Down(u8),
    Up(u8),
}

/// Input from the device; `Err` means the device is gone/broken.
pub type DeviceEvent = std::result::Result<KeyEvent, String>;

enum Backend {
    Real(AsyncStreamDeck),
    /// Closed on purpose so the USB device can suspend (see [`Deck::release`]).
    Released,
    Fake {
        out_dir: Option<PathBuf>,
    },
}

pub struct Deck {
    pub info: DeviceInfo,
    backend: Backend,
    pub events: mpsc::Receiver<DeviceEvent>,
    reader: Option<JoinHandle<()>>,
    kind: Option<Kind>,
    /// Frames from the server are in `info.native` format and go to the device as they are
    /// (the server confirmed it in its welcome). Otherwise they are upright JPEGs.
    native_frames: AtomicBool,
    /// sysfs `power/runtime_status` of the deck's USB device (Linux only).
    status_path: Option<PathBuf>,
}

impl Drop for Deck {
    fn drop(&mut self) {
        if let Some(h) = self.reader.take() {
            h.abort();
        }
    }
}

/// Description of a detected device (for `--list`).
pub struct Detected {
    pub kind: Kind,
    pub serial: String,
}

/// Enumerate visual Stream Decks. Blocking (HID enumeration).
pub fn detect() -> Result<Vec<Detected>> {
    let hid = new_hidapi().context("initialising HID")?;
    Ok(list_devices(&hid)
        .into_iter()
        .filter(|(k, _)| k.is_visual())
        .map(|(kind, serial)| Detected { kind, serial })
        .collect())
}

/// The key image format of `kind`, in protocol terms. `None` for decks without key images.
pub fn native_image(kind: Kind) -> Option<NativeImage> {
    let f = kind.key_image_format();
    let mode = match f.mode {
        ImageMode::None => return None,
        ImageMode::BMP => NativeMode::Bmp,
        ImageMode::JPEG => NativeMode::Jpeg,
    };
    Some(NativeImage {
        mode,
        size: (f.size.0 as u32, f.size.1 as u32),
        rotation: match f.rotation {
            ImageRotation::Rot0 => 0,
            ImageRotation::Rot90 => 90,
            ImageRotation::Rot180 => 180,
            ImageRotation::Rot270 => 270,
        },
        mirror: match f.mirror {
            ImageMirroring::None => NativeMirror::None,
            ImageMirroring::X => NativeMirror::X,
            ImageMirroring::Y => NativeMirror::Y,
            ImageMirroring::Both => NativeMirror::Both,
        },
    })
}

pub fn device_info(kind: Kind, serial: Option<String>) -> DeviceInfo {
    let (rows, cols) = kind.key_layout();
    DeviceInfo {
        model: format!("{kind:?}"),
        serial,
        rows,
        cols,
        key_size_px: kind.key_image_format().size.0 as u32,
        native: native_image(kind),
    }
}

impl Deck {
    /// Open the first Stream Deck found. Errors if none.
    pub async fn open_first() -> Result<Deck> {
        let found = tokio::task::spawn_blocking(detect).await??;
        let Some(d) = found.into_iter().next() else {
            bail!("no Stream Deck found");
        };
        Self::open(d.kind, d.serial).await
    }

    /// Open the deck of `kind` with this serial.
    async fn open(kind: Kind, serial: String) -> Result<Deck> {
        let connect_serial = serial.clone();
        let dev = tokio::task::spawn_blocking(move || -> Result<AsyncStreamDeck> {
            let hid = new_hidapi()?;
            Ok(AsyncStreamDeck::connect(&hid, kind, &connect_serial)?)
        })
        .await??;

        let info = device_info(kind, Some(serial));
        let info_serial = info.serial.clone();
        info!(model = %info.model, serial = ?info.serial, rows = info.rows, cols = info.cols,
              key_px = info.key_size_px, "opened Stream Deck");

        let (tx, rx) = mpsc::channel(64);
        let reader_dev = dev.clone();
        let reader = tokio::spawn(async move {
            let reader = reader_dev.get_reader();
            loop {
                match reader.read(POLL_RATE_HZ).await {
                    Ok(updates) => {
                        for u in updates {
                            let ev = match u {
                                DeviceStateUpdate::ButtonDown(k) => KeyEvent::Down(k),
                                DeviceStateUpdate::ButtonUp(k) => KeyEvent::Up(k),
                                _ => continue, // encoders / touch not supported yet
                            };
                            if tx.send(Ok(ev)).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e.to_string())).await;
                        return;
                    }
                }
            }
        });
        Ok(Deck {
            info,
            backend: Backend::Real(dev),
            events: rx,
            reader: Some(reader),
            kind: Some(kind),
            native_frames: AtomicBool::new(false),
            status_path: if cfg!(target_os = "linux") {
                crate::sleep::find_status_path(info_serial.as_deref())
            } else {
                None
            },
        })
    }

    /// sysfs file holding the USB runtime power state; `None` where USB suspend cannot be
    /// observed (fake deck, not Linux, sysfs entry not found).
    pub fn status_path(&self) -> Option<&Path> {
        self.status_path.as_deref()
    }

    /// Close the HID device and stop reading, so nothing keeps the USB device busy and the
    /// kernel may autosuspend it. Its screen keeps (and shows) the last images until it suspends.
    pub async fn release(&mut self) {
        if let Some(h) = self.reader.take() {
            h.abort();
            let _ = h.await; // the reader holds a handle to the device until it is gone
        }
        self.backend = Backend::Released;
    }

    /// Open the same deck again after [`Deck::release`].
    pub async fn reopen(&mut self) -> Result<()> {
        let (Some(kind), Some(serial)) = (self.kind, self.info.serial.clone()) else {
            bail!("cannot reopen this device");
        };
        let native = self.native_frames();
        *self = Self::open(kind, serial).await?;
        self.set_native_frames(native);
        Ok(())
    }

    /// A pretend 3x5, 72px deck. Key events come from stdin lines (see [`parse_fake_command`]).
    pub fn fake(out_dir: Option<PathBuf>) -> Result<Deck> {
        Self::fake_with(out_dir, None)
    }

    /// Like [`Deck::fake`] with a native image format to report (tests).
    pub fn fake_with(out_dir: Option<PathBuf>, native: Option<NativeImage>) -> Result<Deck> {
        if let Some(d) = &out_dir {
            std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
        }
        let info = DeviceInfo {
            model: "Fake Stream Deck".into(),
            serial: Some("FAKE".into()),
            rows: 3,
            cols: 5,
            key_size_px: 72,
            native,
        };
        let (tx, rx) = mpsc::channel(64);
        let key_count = info.key_count();
        let reader = tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(tokio::io::stdin()).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match parse_fake_command(&line, key_count) {
                    Ok(evs) => {
                        for ev in evs {
                            if tx.send(Ok(ev)).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(e) => warn!("stdin: {e}"),
                }
            }
            // stdin closed: keep the channel open so the fake deck never "unplugs".
            std::future::pending::<()>().await;
        });
        Ok(Deck {
            info,
            backend: Backend::Fake { out_dir },
            events: rx,
            reader: Some(reader),
            kind: None,
            native_frames: AtomicBool::new(false),
            status_path: None,
        })
    }

    pub fn is_fake(&self) -> bool {
        matches!(self.backend, Backend::Fake { .. })
    }

    /// Whether frames can be written straight to this deck: it takes JPEGs, so a server that
    /// makes them in its native format saves the decode and second encode.
    pub fn can_passthrough(&self) -> bool {
        self.info
            .native
            .is_some_and(|n| n.mode == NativeMode::Jpeg && n.size.0 == self.info.key_size_px)
    }

    /// Whether server frames are device-ready (see [`Deck::set_native_frames`]).
    pub fn native_frames(&self) -> bool {
        self.native_frames.load(Ordering::Relaxed)
    }

    /// Set once the server confirmed native frames for this connection; only honoured when
    /// the deck can take them.
    pub fn set_native_frames(&self, on: bool) {
        self.native_frames
            .store(on && self.can_passthrough(), Ordering::Relaxed);
    }

    /// Show a frame as the server sent it: written to the device untouched if native frames
    /// were agreed, otherwise an upright JPEG (see [`Deck::set_image_jpeg`]).
    pub async fn set_image_frame(&self, key: u8, bytes: &[u8]) -> Result<()> {
        if !self.native_frames() {
            return self.set_image_jpeg(key, bytes).await;
        }
        if key >= self.info.key_count() {
            bail!(
                "key {key} out of range (deck has {} keys)",
                self.info.key_count()
            );
        }
        if !bytes.starts_with(&[0xff, 0xd8]) {
            bail!("bad JPEG: no SOI marker");
        }
        match &self.backend {
            Backend::Real(dev) => {
                dev.write_image(key, bytes).await?;
                dev.flush().await?;
            }
            Backend::Released => bail!("device is released (asleep)"),
            Backend::Fake { out_dir } => {
                // the files are for looking at: store them upright
                let img = self.decode_frame(bytes)?;
                if let Some(dir) = out_dir {
                    let mut jpeg = Vec::new();
                    img.write_to(&mut std::io::Cursor::new(&mut jpeg), ImageFormat::Jpeg)?;
                    write_fake_image(dir, key, &jpeg)?;
                }
                info!(key, bytes = bytes.len(), "fake: native image set");
            }
        }
        Ok(())
    }

    /// Decode a frame (native or upright, as agreed) into an upright image.
    pub fn decode_frame(&self, bytes: &[u8]) -> Result<DynamicImage> {
        let size = self.info.key_size_px;
        match self.info.native.filter(|_| self.native_frames()) {
            Some(n) => Ok(unorient(decode_key_image(bytes, size)?, &n)),
            None => decode_key_image(bytes, size),
        }
    }

    /// Decode a JPEG frame from the server and show it on `key`.
    pub async fn set_image_jpeg(&self, key: u8, jpeg: &[u8]) -> Result<()> {
        if key >= self.info.key_count() {
            bail!(
                "key {key} out of range (deck has {} keys)",
                self.info.key_count()
            );
        }
        match &self.backend {
            Backend::Real(dev) => {
                let img = decode_key_image(jpeg, self.info.key_size_px)?;
                dev.set_button_image(key, img).await?;
                dev.flush().await?;
            }
            Backend::Released => bail!("device is released (asleep)"),
            Backend::Fake { out_dir } => {
                // validate it decodes, then store the original bytes
                decode_key_image(jpeg, self.info.key_size_px)?;
                if let Some(dir) = out_dir {
                    write_fake_image(dir, key, jpeg)?;
                }
                info!(key, bytes = jpeg.len(), "fake: image set");
            }
        }
        Ok(())
    }

    pub async fn set_brightness(&self, percent: u8) -> Result<()> {
        let percent = percent.min(100);
        match &self.backend {
            Backend::Real(dev) => dev.set_brightness(percent).await?,
            Backend::Released => bail!("device is released (asleep)"),
            Backend::Fake { .. } => info!(percent, "fake: brightness"),
        }
        Ok(())
    }

    /// Blank every key.
    pub async fn clear(&self) -> Result<()> {
        match &self.backend {
            Backend::Real(dev) => dev.clear_all_button_images().await?,
            Backend::Released => bail!("device is released (asleep)"),
            Backend::Fake { out_dir } => {
                if let Some(dir) = out_dir {
                    for k in 0..self.info.key_count() {
                        let _ = std::fs::remove_file(dir.join(format!("key-{k}.jpg")));
                    }
                }
                info!("fake: cleared");
            }
        }
        Ok(())
    }

    /// Visual "not connected" state: all keys black except key 0, which has a small red dot.
    pub async fn show_disconnected(&self) -> Result<()> {
        self.clear().await?;
        let mut jpeg = Vec::new();
        disconnected_tile(self.info.key_size_px)
            .write_to(&mut std::io::Cursor::new(&mut jpeg), ImageFormat::Jpeg)?;
        self.set_image_jpeg(0, &jpeg).await
    }
}

impl Deck {
    /// Press feedback while offline: the cached image dimmed with a red bar along the bottom
    /// (just the bar on black when nothing is cached).
    pub async fn show_offline_key(&self, key: u8, cached: Option<&[u8]>) -> Result<()> {
        let size = self.info.key_size_px;
        let base = cached.and_then(|j| self.decode_frame(j).ok());
        let mut jpeg = Vec::new();
        offline_key_tile(base.as_ref(), size)
            .write_to(&mut std::io::Cursor::new(&mut jpeg), ImageFormat::Jpeg)?;
        self.set_image_jpeg(key, &jpeg).await
    }

    /// Undo [`Deck::show_offline_key`]: redraw the cached image (black when nothing is cached).
    pub async fn restore_key(&self, key: u8, cached: Option<&[u8]>) -> Result<()> {
        match cached {
            Some(frame) => self.set_image_frame(key, frame).await,
            None => {
                let mut jpeg = Vec::new();
                RgbImage::from_pixel(1, 1, Rgb([0, 0, 0]))
                    .write_to(&mut std::io::Cursor::new(&mut jpeg), ImageFormat::Jpeg)?;
                self.set_image_jpeg(key, &jpeg).await
            }
        }
    }
}

/// `base` dimmed to a third of its brightness, with a red bar along the bottom edge.
pub fn offline_key_tile(base: Option<&DynamicImage>, size: u32) -> DynamicImage {
    let mut img = match base {
        Some(b) => b.to_rgb8(),
        None => RgbImage::from_pixel(size, size, Rgb([0, 0, 0])),
    };
    let bar_top = (size as f32 * 0.78) as u32;
    for (_, y, px) in img.enumerate_pixels_mut() {
        *px = if y >= bar_top {
            Rgb([200, 0, 0])
        } else {
            Rgb(px.0.map(|c| c / 3))
        };
    }
    DynamicImage::ImageRgb8(img)
}

/// A black key with a small red dot near the top-left corner.
pub fn disconnected_tile(size: u32) -> DynamicImage {
    let mut img = RgbImage::from_pixel(size, size, Rgb([0, 0, 0]));
    let s = size as f32;
    let (cx, cy, r) = (s * 0.22, s * 0.22, s * 0.09);
    for (x, y, px) in img.enumerate_pixels_mut() {
        let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
        if dx * dx + dy * dy <= r * r {
            *px = Rgb([200, 0, 0]);
        }
    }
    DynamicImage::ImageRgb8(img)
}

/// Solid colour tile for hue in 0..1 (used by `--test-pattern`).
pub fn hsv_tile(size: u32, hue: f32) -> DynamicImage {
    let h = hue * 6.0;
    let x = 1.0 - ((h % 2.0) - 1.0).abs();
    let (r, g, b) = match h as u32 {
        0 => (1.0, x, 0.0),
        1 => (x, 1.0, 0.0),
        2 => (0.0, 1.0, x),
        3 => (0.0, x, 1.0),
        4 => (x, 0.0, 1.0),
        _ => (1.0, 0.0, x),
    };
    let c = |v: f32| (v * 255.0) as u8;
    DynamicImage::ImageRgb8(RgbImage::from_pixel(size, size, Rgb([c(r), c(g), c(b)])))
}

/// Decode a JPEG and make sure it is exactly `size` x `size`.
pub fn decode_key_image(jpeg: &[u8], size: u32) -> Result<DynamicImage> {
    let img = image::load_from_memory_with_format(jpeg, ImageFormat::Jpeg)
        .map_err(|e| anyhow!("bad JPEG: {e}"))?;
    if img.width() != size || img.height() != size {
        debug!(w = img.width(), h = img.height(), size, "resizing frame");
        return Ok(img.resize_exact(size, size, image::imageops::FilterType::Triangle));
    }
    Ok(img)
}

/// Undoes a deck's native orientation (rotate, then mirror) to get the upright image back.
pub fn unorient(img: DynamicImage, n: &NativeImage) -> DynamicImage {
    let img = match n.mirror {
        NativeMirror::None => img,
        NativeMirror::X => img.fliph(),
        NativeMirror::Y => img.flipv(),
        NativeMirror::Both => img.fliph().flipv(),
    };
    match n.rotation {
        90 => img.rotate270(),
        180 => img.rotate180(),
        270 => img.rotate90(),
        _ => img,
    }
}

fn write_fake_image(dir: &Path, key: u8, jpeg: &[u8]) -> Result<()> {
    let path = dir.join(format!("key-{key}.jpg"));
    std::fs::write(&path, jpeg).with_context(|| format!("writing {}", path.display()))
}

/// Parse a stdin line for the fake device: `press N`, `down N`, `up N`.
pub fn parse_fake_command(line: &str, key_count: u8) -> Result<Vec<KeyEvent>> {
    let mut it = line.split_whitespace();
    let Some(cmd) = it.next() else {
        return Ok(vec![]);
    };
    let key: u8 = it
        .next()
        .ok_or_else(|| anyhow!("usage: press|down|up <key>"))?
        .parse()
        .map_err(|_| anyhow!("key must be a number"))?;
    if key >= key_count {
        bail!("key {key} out of range (0..{})", key_count);
    }
    Ok(match cmd {
        "press" => vec![KeyEvent::Down(key), KeyEvent::Up(key)],
        "down" => vec![KeyEvent::Down(key)],
        "up" => vec![KeyEvent::Up(key)],
        other => bail!("unknown command {other:?}; usage: press|down|up <key>"),
    })
}

/// Sleep helper used by retry loops.
pub async fn sleep(d: Duration) {
    tokio::time::sleep(d).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(size: u32) -> Vec<u8> {
        let mut out = Vec::new();
        DynamicImage::ImageRgb8(RgbImage::from_pixel(size, size, Rgb([10, 200, 30])))
            .write_to(&mut std::io::Cursor::new(&mut out), ImageFormat::Jpeg)
            .unwrap();
        out
    }

    #[test]
    fn parses_commands() {
        assert_eq!(
            parse_fake_command("press 3", 15).unwrap(),
            vec![KeyEvent::Down(3), KeyEvent::Up(3)]
        );
        assert_eq!(
            parse_fake_command(" down 14 ", 15).unwrap(),
            vec![KeyEvent::Down(14)]
        );
        assert_eq!(
            parse_fake_command("up 0", 15).unwrap(),
            vec![KeyEvent::Up(0)]
        );
        assert!(parse_fake_command("", 15).unwrap().is_empty());
        assert!(parse_fake_command("press 15", 15).is_err());
        assert!(parse_fake_command("press x", 15).is_err());
        assert!(parse_fake_command("press", 15).is_err());
        assert!(parse_fake_command("jump 1", 15).is_err());
    }

    #[test]
    fn decodes_and_resizes() {
        assert_eq!(decode_key_image(&jpeg(72), 72).unwrap().width(), 72);
        let r = decode_key_image(&jpeg(100), 72).unwrap();
        assert_eq!((r.width(), r.height()), (72, 72));
        assert!(decode_key_image(b"not a jpeg", 72).is_err());
    }

    #[test]
    fn device_info_for_original() {
        let i = device_info(Kind::OriginalV2, Some("X".into()));
        assert_eq!((i.rows, i.cols, i.key_size_px), (3, 5, 72));
        assert_eq!(i.key_count(), 15);
    }

    #[tokio::test]
    async fn fake_deck_writes_images() {
        let dir = std::env::temp_dir().join(format!("sdc-fake-{}", std::process::id()));
        let deck = Deck::fake(Some(dir.clone())).unwrap();
        deck.set_image_jpeg(3, &jpeg(72)).await.unwrap();
        assert!(dir.join("key-3.jpg").exists());
        assert!(deck.set_image_jpeg(15, &jpeg(72)).await.is_err());
        deck.show_disconnected().await.unwrap();
        let tile = disconnected_tile(72).to_rgb8();
        assert_eq!(tile.get_pixel(16, 16).0, [200, 0, 0]);
        assert_eq!(tile.get_pixel(50, 50).0, [0, 0, 0]);
        assert!(!dir.join("key-3.jpg").exists());
        assert!(dir.join("key-0.jpg").exists());

        // press feedback and restore
        deck.show_offline_key(3, Some(&jpeg(72))).await.unwrap();
        let shown = std::fs::read(dir.join("key-3.jpg")).unwrap();
        let px = decode_key_image(&shown, 72).unwrap().to_rgb8();
        assert!(px.get_pixel(36, 10).0[1] < 100, "dimmed (was 200 green)");
        assert!(px.get_pixel(36, 68).0[0] > 150, "red bar at the bottom");
        deck.restore_key(3, Some(&jpeg(72))).await.unwrap();
        assert_eq!(std::fs::read(dir.join("key-3.jpg")).unwrap(), jpeg(72));
        std::fs::remove_dir_all(dir).ok();
    }

    fn mk2() -> NativeImage {
        native_image(Kind::Mk2).unwrap()
    }

    /// A 72px JPEG with a red top-left block on blue.
    fn marked_jpeg(orient: impl Fn(RgbImage) -> RgbImage) -> Vec<u8> {
        let mut img = RgbImage::from_pixel(72, 72, Rgb([0, 0, 255]));
        for y in 0..24 {
            for x in 0..24 {
                img.put_pixel(x, y, Rgb([255, 0, 0]));
            }
        }
        let mut out = Vec::new();
        DynamicImage::ImageRgb8(orient(img))
            .write_to(&mut std::io::Cursor::new(&mut out), ImageFormat::Jpeg)
            .unwrap();
        out
    }

    #[test]
    fn native_formats_follow_the_kinds() {
        let m = mk2();
        assert_eq!(
            (m.mode, m.size, m.rotation, m.mirror),
            (NativeMode::Jpeg, (72, 72), 0, NativeMirror::Both)
        );
        let o = native_image(Kind::Original).unwrap();
        assert_eq!(o.mode, NativeMode::Bmp);
        let mini = native_image(Kind::Mini).unwrap();
        assert_eq!((mini.rotation, mini.mirror), (90, NativeMirror::X));
        assert_eq!(native_image(Kind::PlusXl).unwrap().rotation, 270);
        assert!(native_image(Kind::Pedal).is_none());
        assert_eq!(device_info(Kind::Mk2, None).native, Some(m));
    }

    #[tokio::test]
    async fn passthrough_only_for_jpeg_decks() {
        let jpeg_deck = Deck::fake_with(None, Some(mk2())).unwrap();
        assert!(jpeg_deck.can_passthrough());
        jpeg_deck.set_native_frames(true);
        assert!(jpeg_deck.native_frames());
        // BMP decks, decks without a format and wrong sizes always take the decode path
        let bmp = Deck::fake_with(None, native_image(Kind::Original)).unwrap();
        assert!(!bmp.can_passthrough());
        bmp.set_native_frames(true);
        assert!(!bmp.native_frames());
        let plain = Deck::fake(None).unwrap();
        plain.set_native_frames(true);
        assert!(!plain.native_frames());
    }

    #[test]
    fn unorient_inverts_the_vendored_conversion() {
        for kind in [Kind::Mk2, Kind::Plus, Kind::PlusXl, Kind::Xl] {
            let n = native_image(kind).unwrap();
            let size = n.size.0;
            let mut img = RgbImage::from_pixel(size, size, Rgb([0, 0, 0]));
            for y in 0..size / 4 {
                for x in 0..size / 2 {
                    img.put_pixel(x, y, Rgb([255, 255, 255]));
                }
            }
            // what set_button_image would send for this upright image
            let sent = elgato_streamdeck::images::convert_image_with_format(
                kind.key_image_format(),
                DynamicImage::ImageRgb8(img),
            )
            .unwrap();
            let back = unorient(
                image::load_from_memory_with_format(&sent, ImageFormat::Jpeg).unwrap(),
                &n,
            )
            .to_rgb8();
            assert!(back.get_pixel(size / 4, 2).0[0] > 200, "{kind:?} marker");
            assert!(back.get_pixel(size * 3 / 4, 2).0[0] < 60, "{kind:?} rest");
            assert!(back.get_pixel(2, size - 3).0[0] < 60, "{kind:?} bottom");
        }
    }

    #[tokio::test]
    async fn fake_deck_stores_native_frames_upright() {
        let dir = std::env::temp_dir().join(format!("sdc-fake-native-{}", std::process::id()));
        let deck = Deck::fake_with(Some(dir.clone()), Some(mk2())).unwrap();
        // not agreed yet: the frame is taken as upright and stored as is
        let upright = marked_jpeg(|i| i);
        deck.set_image_frame(1, &upright).await.unwrap();
        assert_eq!(std::fs::read(dir.join("key-1.jpg")).unwrap(), upright);

        deck.set_native_frames(true);
        // MK.2 frame: the server rotated the marker to the bottom-right
        let native = marked_jpeg(|i| image::imageops::rotate180(&i));
        deck.set_image_frame(2, &native).await.unwrap();
        let stored = image::open(dir.join("key-2.jpg")).unwrap().to_rgb8();
        assert!(
            stored.get_pixel(8, 8).0[0] > 200,
            "marker upright, top-left"
        );
        assert!(stored.get_pixel(64, 64).0[2] > 200, "blue elsewhere");
        // garbage is rejected rather than sent to the deck
        assert!(deck.set_image_frame(2, b"nope").await.is_err());

        // offline feedback decodes the native cache entry upright
        deck.show_offline_key(3, Some(&native)).await.unwrap();
        let shown = image::open(dir.join("key-3.jpg")).unwrap().to_rgb8();
        assert!(
            shown.get_pixel(8, 8).0[0] > 40,
            "dimmed marker stays top-left"
        );
        assert!(shown.get_pixel(8, 8).0[0] < 120);
        assert!(shown.get_pixel(36, 68).0[0] > 150, "red bar at the bottom");
        deck.restore_key(3, Some(&native)).await.unwrap();
        let back = image::open(dir.join("key-3.jpg")).unwrap().to_rgb8();
        assert!(back.get_pixel(8, 8).0[0] > 200);
        std::fs::remove_dir_all(dir).ok();
    }
}
