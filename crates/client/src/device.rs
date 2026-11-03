//! The Stream Deck itself: finding, opening, releasing and drawing on it. Only decks that take
//! JPEG key images (MK.2 and newer) are supported.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{anyhow, bail, Result};
use elgato_streamdeck::{
    asynchronous::AsyncStreamDeck,
    info::{ImageMirroring, ImageMode, ImageRotation, Kind},
    DeviceStateUpdate,
};
use image::{DynamicImage, Rgb, RgbImage};
use sdc_protocol::{DeviceInfo, NativeImage, NativeMirror};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::info;

/// How often the HID input is polled (Hz).
const POLL_RATE_HZ: f32 = 60.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    Down(u8),
    Up(u8),
}

/// Input from the device; `Err` means the device is gone/broken.
pub type DeviceEvent = std::result::Result<KeyEvent, String>;

pub struct Deck {
    pub info: DeviceInfo,
    pub events: mpsc::Receiver<DeviceEvent>,
    kind: Kind,
    serial: String,
    /// `None` while released on purpose so the USB device can suspend.
    dev: Option<AsyncStreamDeck>,
    reader: Option<JoinHandle<()>>,
    /// sysfs `power/runtime_status` of the deck's USB device (Linux only).
    pub status_path: Option<PathBuf>,
}

impl Drop for Deck {
    fn drop(&mut self) {
        if let Some(h) = self.reader.take() {
            h.abort();
        }
    }
}

/// A detected deck (for `--list`).
pub struct Detected {
    pub kind: Kind,
    pub serial: String,
}

fn supported(kind: Kind) -> bool {
    kind.is_visual() && matches!(kind.key_image_format().mode, ImageMode::JPEG)
}

/// Linux: find the decks in sysfs. hidapi's own enumeration takes seconds on a Pi Zero, and
/// the deck must be reopened quickly after every USB suspend.
#[cfg(target_os = "linux")]
fn scan() -> Result<Vec<(Detected, std::ffi::CString)>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/sys/class/hidraw")?.flatten() {
        let Ok(uevent) = std::fs::read_to_string(entry.path().join("device/uevent")) else {
            continue;
        };
        let prop = |name: &str| {
            uevent
                .lines()
                .find_map(|l| l.strip_prefix(name)?.strip_prefix('='))
        };
        // HID_ID=0003:00000FD9:00000080 (bus:vendor:product)
        let mut ids = prop("HID_ID").unwrap_or_default().split(':').skip(1);
        let mut id = || u16::from_str_radix(ids.next()?, 16).ok();
        let (Some(vid), Some(pid), Some(serial)) = (id(), id(), prop("HID_UNIQ")) else {
            continue;
        };
        let Some(kind) = Kind::from_vid_pid(vid, pid).filter(|k| supported(*k)) else {
            continue;
        };
        let node = format!("/dev/{}", entry.file_name().to_string_lossy());
        found.push((
            Detected {
                kind,
                serial: serial.to_string(),
            },
            std::ffi::CString::new(node)?,
        ));
    }
    Ok(found)
}

/// Enumerate supported Stream Decks. Blocking.
#[cfg(target_os = "linux")]
pub fn detect() -> Result<Vec<Detected>> {
    Ok(scan()?.into_iter().map(|(d, _)| d).collect())
}

#[cfg(target_os = "linux")]
fn connect(kind: Kind, serial: &str) -> Result<AsyncStreamDeck> {
    let (_, node) = scan()?
        .into_iter()
        .find(|(d, _)| d.serial == serial)
        .ok_or_else(|| anyhow!("Stream Deck {serial} not found"))?;
    let deck = elgato_streamdeck::StreamDeck::connect_path(kind, &node)?;
    Ok(AsyncStreamDeck::from(deck))
}

#[cfg(not(target_os = "linux"))]
pub fn detect() -> Result<Vec<Detected>> {
    let hid = elgato_streamdeck::new_hidapi()?;
    Ok(elgato_streamdeck::list_devices(&hid)
        .into_iter()
        .filter(|(k, _)| supported(*k))
        .map(|(kind, serial)| Detected { kind, serial })
        .collect())
}

#[cfg(not(target_os = "linux"))]
fn connect(kind: Kind, serial: &str) -> Result<AsyncStreamDeck> {
    let hid = elgato_streamdeck::new_hidapi()?;
    Ok(AsyncStreamDeck::connect(&hid, kind, serial)?)
}

/// The key image format of a JPEG deck, in protocol terms.
fn native_image(kind: Kind) -> NativeImage {
    let f = kind.key_image_format();
    NativeImage {
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
    }
}

pub fn device_info(kind: Kind, serial: String) -> DeviceInfo {
    let (rows, cols) = kind.key_layout();
    DeviceInfo {
        model: format!("{kind:?}"),
        serial: Some(serial),
        rows,
        cols,
        key_size_px: kind.key_image_format().size.0 as u32,
        native: Some(native_image(kind)),
    }
}

impl Deck {
    /// Open the first Stream Deck found. Errors if none.
    pub async fn open_first() -> Result<Deck> {
        let found = tokio::task::spawn_blocking(detect).await??;
        let Some(d) = found.into_iter().next() else {
            bail!("no Stream Deck found");
        };
        let (dev, events, reader) = start(d.kind, &d.serial).await?;
        let status_path = cfg!(target_os = "linux")
            .then(|| crate::sleep::find_status_path(&d.serial))
            .flatten();
        Ok(Deck {
            info: device_info(d.kind, d.serial.clone()),
            events,
            kind: d.kind,
            serial: d.serial,
            dev: Some(dev),
            reader: Some(reader),
            status_path,
        })
    }

    /// Close the HID device and stop reading, so nothing keeps the USB device busy and the
    /// kernel may autosuspend it. Its screen keeps (and shows) the last images until it suspends.
    pub async fn release(&mut self) {
        if let Some(h) = self.reader.take() {
            h.abort();
            let _ = h.await; // the reader holds a handle to the device until it is gone
        }
        self.dev = None;
    }

    /// Open the same deck again after [`Deck::release`].
    pub async fn reopen(&mut self) -> Result<()> {
        let (dev, events, reader) = start(self.kind, &self.serial).await?;
        self.dev = Some(dev);
        self.events = events;
        self.reader = Some(reader);
        Ok(())
    }

    fn dev(&self) -> Result<&AsyncStreamDeck> {
        self.dev.as_ref().ok_or_else(|| anyhow!("deck is released"))
    }

    /// Show a frame from the server: a JPEG already in the deck's native format.
    pub async fn write_frame(&self, key: u8, jpeg: &[u8]) -> Result<()> {
        let dev = self.dev()?;
        dev.write_image(key, jpeg).await?;
        Ok(dev.flush().await?)
    }

    /// Fill a key with a solid colour.
    pub async fn fill(&self, key: u8, rgb: [u8; 3]) -> Result<()> {
        let size = self.info.key_size_px;
        let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(size, size, Rgb(rgb)));
        let dev = self.dev()?;
        dev.set_button_image(key, img).await?;
        Ok(dev.flush().await?)
    }

    pub async fn set_brightness(&self, percent: u8) -> Result<()> {
        Ok(self.dev()?.set_brightness(percent.min(100)).await?)
    }

    pub async fn clear(&self) -> Result<()> {
        Ok(self.dev()?.clear_all_button_images().await?)
    }
}

type Started = (AsyncStreamDeck, mpsc::Receiver<DeviceEvent>, JoinHandle<()>);

/// Open the deck and start forwarding its key events.
async fn start(kind: Kind, serial: &str) -> Result<Started> {
    let t = Instant::now();
    let serial = serial.to_string();
    let dev = tokio::task::spawn_blocking(move || connect(kind, &serial)).await??;
    info!(model = ?kind, took = ?t.elapsed(), "opened Stream Deck");

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
    Ok((dev, rx, reader))
}

/// Solid colour for hue in 0..1 (used by `--test-pattern`).
pub fn hsv_color(hue: f32) -> [u8; 3] {
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
    [r, g, b].map(|v| (v * 255.0) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_formats_follow_the_kinds() {
        let m = device_info(Kind::Mk2, "X".into());
        assert_eq!((m.rows, m.cols, m.key_size_px), (3, 5, 72));
        let n = m.native.unwrap();
        assert_eq!(
            (n.size, n.rotation, n.mirror),
            ((72, 72), 0, NativeMirror::Both)
        );
        assert_eq!(native_image(Kind::PlusXl).rotation, 270);
    }

    #[test]
    fn only_jpeg_decks_are_supported() {
        assert!(supported(Kind::Mk2));
        assert!(supported(Kind::Plus));
        assert!(!supported(Kind::Original)); // BMP
        assert!(!supported(Kind::Mini));
        assert!(!supported(Kind::Pedal)); // no screens
    }
}
