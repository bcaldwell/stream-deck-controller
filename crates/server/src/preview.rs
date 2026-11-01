//! `sdc-server render`: render a page to PNGs without any hardware.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Result};
use image::{Rgb, RgbImage};
use sdc_protocol::DeviceInfo;
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::engine::{Engine, EngineEvent, Out};
use crate::ha::{self, HaConfig, HaEvent};

const ROWS: u8 = 3;
const COLS: u8 = 5;
const KEY_PX: u32 = 72;
const GAP: u32 = 6;
const FETCH_WAIT: Duration = Duration::from_secs(30);

pub async fn run(
    config: PathBuf,
    out: &Path,
    page: Option<String>,
    ha_cfg: Option<HaConfig>,
    states: Option<PathBuf>,
) -> Result<()> {
    let (ha_cmd_tx, ha_cmd_rx) = mpsc::unbounded_channel();
    let mut engine = Engine::new(config, Some(ha_cmd_tx));
    // Image downloads report back here; unlike the server, we wait for them below.
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    engine.enable_fetch(Handle::current(), &ev_tx);
    engine.reload()?;

    if let Some(cfg) = ha_cfg {
        tokio::spawn(ha::run(cfg, ha_cmd_rx, ev_tx.clone()));
        // wait for the initial state snapshot
        let seeded = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(ev) = ev_rx.recv().await {
                let is_seed = matches!(ev, EngineEvent::Ha(HaEvent::Seed(_)));
                engine.handle(ev);
                if is_seed {
                    return true;
                }
            }
            false
        })
        .await;
        if !matches!(seeded, Ok(true)) {
            tracing::warn!("no state snapshot from home assistant; rendering with empty states");
        }
    }

    if let Some(f) = states {
        let states: Vec<ha::EntityState> = serde_json::from_slice(&std::fs::read(&f)?)?;
        engine.handle(EngineEvent::Ha(HaEvent::Seed(states)));
    }

    let (tx, mut rx) = mpsc::unbounded_channel();
    let device = DeviceInfo {
        model: "preview".into(),
        serial: None,
        rows: ROWS,
        cols: COLS,
        key_size_px: KEY_PX,
    };
    engine.handle(EngineEvent::ClientConnected {
        id: 1,
        client_id: "preview".into(),
        device,
        tx,
    });
    if let Some(p) = &page {
        engine.goto_page(1, p);
    }

    let waited = tokio::time::timeout(FETCH_WAIT, async {
        while engine.pending_fetches() > 0 {
            match ev_rx.recv().await {
                Some(ev) => engine.handle(ev),
                None => break,
            }
        }
    })
    .await;
    if waited.is_err() {
        tracing::warn!("timed out waiting for image downloads; rendering without them");
    }

    let mut tiles: Vec<Option<RgbImage>> = vec![None; (ROWS * COLS) as usize];
    while let Ok(out) = rx.try_recv() {
        let Out::Frame(f) = out else { continue };
        tiles[f.key as usize] = Some(image::load_from_memory(&f.jpeg)?.to_rgb8());
    }
    if tiles.iter().all(Option::is_none) {
        bail!("nothing rendered (does the config define any pages?)");
    }

    std::fs::create_dir_all(out)?;
    let w = COLS as u32 * (KEY_PX + GAP) + GAP;
    let h = ROWS as u32 * (KEY_PX + GAP) + GAP;
    let mut grid = RgbImage::from_pixel(w, h, Rgb([40, 40, 44]));
    for (i, t) in tiles.iter().enumerate() {
        let black = RgbImage::new(KEY_PX, KEY_PX);
        let t = t.as_ref().unwrap_or(&black);
        t.save(out.join(format!("key_{i:02}.png")))?;
        let (r, c) = (i as u32 / COLS as u32, i as u32 % COLS as u32);
        image::imageops::replace(
            &mut grid,
            t,
            (GAP + c * (KEY_PX + GAP)) as i64,
            (GAP + r * (KEY_PX + GAP)) as i64,
        );
    }
    grid.save(out.join("page.png"))?;
    println!(
        "wrote {} tiles and page.png to {}",
        tiles.len(),
        out.display()
    );
    Ok(())
}
