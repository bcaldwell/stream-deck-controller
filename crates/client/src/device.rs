//! Stream Deck device abstraction: a real HID deck or a fake one for testing.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use elgato_streamdeck::{
    asynchronous::AsyncStreamDeck, info::Kind, list_devices, new_hidapi, DeviceStateUpdate,
};
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use sdc_protocol::DeviceInfo;
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
    Fake { out_dir: Option<PathBuf> },
}

pub struct Deck {
    pub info: DeviceInfo,
    backend: Backend,
    pub events: mpsc::Receiver<DeviceEvent>,
    reader: Option<JoinHandle<()>>,
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

pub fn device_info(kind: Kind, serial: Option<String>) -> DeviceInfo {
    let (rows, cols) = kind.key_layout();
    DeviceInfo {
        model: format!("{kind:?}"),
        serial,
        rows,
        cols,
        key_size_px: kind.key_image_format().size.0 as u32,
    }
}

impl Deck {
    /// Open the first Stream Deck found. Errors if none.
    pub async fn open_first() -> Result<Deck> {
        let found = tokio::task::spawn_blocking(detect).await??;
        let Some(d) = found.into_iter().next() else {
            bail!("no Stream Deck found");
        };
        let serial = d.serial.clone();
        let kind = d.kind;
        let dev = tokio::task::spawn_blocking(move || -> Result<AsyncStreamDeck> {
            let hid = new_hidapi()?;
            Ok(AsyncStreamDeck::connect(&hid, kind, &serial)?)
        })
        .await??;

        let info = device_info(kind, Some(d.serial));
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
        })
    }

    /// A pretend 3x5, 72px deck. Key events come from stdin lines (see [`parse_fake_command`]).
    pub fn fake(out_dir: Option<PathBuf>) -> Result<Deck> {
        if let Some(d) = &out_dir {
            std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
        }
        let info = DeviceInfo {
            model: "Fake Stream Deck".into(),
            serial: Some("FAKE".into()),
            rows: 3,
            cols: 5,
            key_size_px: 72,
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
        })
    }

    pub fn is_fake(&self) -> bool {
        matches!(self.backend, Backend::Fake { .. })
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
            Backend::Fake { .. } => info!(percent, "fake: brightness"),
        }
        Ok(())
    }

    /// Blank every key.
    pub async fn clear(&self) -> Result<()> {
        match &self.backend {
            Backend::Real(dev) => dev.clear_all_button_images().await?,
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

    /// Visual "not connected" state: all keys blank except key 0, which is solid red.
    pub async fn show_disconnected(&self) -> Result<()> {
        self.clear().await?;
        let mut jpeg = Vec::new();
        disconnected_tile(self.info.key_size_px)
            .write_to(&mut std::io::Cursor::new(&mut jpeg), ImageFormat::Jpeg)?;
        self.set_image_jpeg(0, &jpeg).await
    }
}

pub fn disconnected_tile(size: u32) -> DynamicImage {
    DynamicImage::ImageRgb8(RgbImage::from_pixel(size, size, Rgb([140, 0, 0])))
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
        assert!(!dir.join("key-3.jpg").exists());
        assert!(dir.join("key-0.jpg").exists());
        std::fs::remove_dir_all(dir).ok();
    }
}
