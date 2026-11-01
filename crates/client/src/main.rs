//! sdc-client: connects a local Stream Deck to an sdc-server over WebSocket.

mod device;
mod session;
mod update;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Server WebSocket URL, e.g. ws://192.168.1.10:7700/ws
    #[arg(long, env = "SDC_SERVER", default_value = "ws://127.0.0.1:7700/ws")]
    server: String,

    /// Shared secret the server requires (sent as `Authorization: Bearer <token>`)
    #[arg(long, env = "SDC_CLIENT_TOKEN", hide_env_values = true)]
    token: Option<String>,

    /// Stable identifier for this client (default: hostname)
    #[arg(long, env = "SDC_CLIENT_ID")]
    client_id: Option<String>,

    /// Initial brightness 0-100
    #[arg(long, env = "SDC_BRIGHTNESS", default_value_t = 60,
          value_parser = clap::value_parser!(u8).range(0..=100))]
    brightness: u8,

    /// Hardware smoke test: paint each key a distinct colour, log key presses for N seconds, exit
    #[arg(long, value_name = "SECONDS")]
    test_pattern: Option<u64>,

    /// List detected Stream Decks and exit
    #[arg(long)]
    list: bool,

    /// Pretend to be a 3x5 72px deck (no hardware); key events are read from stdin
    /// as `press N`, `down N`, `up N`
    #[arg(long, env = "SDC_FAKE_DEVICE")]
    fake_device: bool,

    /// With --fake-device: write received images to this directory as key-<n>.jpg
    #[arg(long, env = "SDC_FAKE_OUT")]
    fake_out: Option<PathBuf>,

    /// Do not replace this binary when the server offers a different one
    #[arg(long, env = "SDC_NO_UPDATE")]
    no_update: bool,
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".into())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();

    if args.list {
        let found = tokio::task::spawn_blocking(device::detect).await??;
        if found.is_empty() {
            println!("no Stream Deck devices found");
        }
        for d in found {
            let i = device::device_info(d.kind, Some(d.serial.clone()));
            println!(
                "{:?} serial={} {}x{} keys, {}px",
                d.kind, d.serial, i.rows, i.cols, i.key_size_px
            );
        }
        return Ok(());
    }

    if let Some(secs) = args.test_pattern {
        let mut deck = device::Deck::open_first().await?;
        deck.set_brightness(args.brightness).await?;
        for k in 0..deck.info.key_count() {
            let hue = k as f32 / deck.info.key_count() as f32;
            let px = device::hsv_tile(deck.info.key_size_px, hue);
            let mut jpeg = Vec::new();
            px.write_to(
                &mut std::io::Cursor::new(&mut jpeg),
                image::ImageFormat::Jpeg,
            )?;
            deck.set_image_jpeg(k, &jpeg).await?;
        }
        info!("test pattern shown; logging key events for {secs}s");
        let end = tokio::time::sleep(Duration::from_secs(secs));
        tokio::pin!(end);
        loop {
            tokio::select! {
                _ = &mut end => break,
                ev = deck.events.recv() => match ev {
                    Some(Ok(e)) => info!("key event: {e:?}"),
                    Some(Err(e)) => anyhow::bail!("input error: {e}"),
                    None => break,
                },
            }
        }
        deck.clear().await?;
        return Ok(());
    }

    let cfg = session::Config {
        server: args.server,
        token: args.token.filter(|t| !t.is_empty()),
        client_id: args.client_id.unwrap_or_else(hostname),
        brightness: args.brightness,
        client_sha256: update::exe_sha256()
            .map_err(|e| warn!("cannot hash own executable, self-update disabled: {e:#}"))
            .ok(),
        auto_update: !args.no_update,
    };
    info!(
        server = %cfg.server,
        client_id = %cfg.client_id,
        sha256 = cfg.client_sha256.as_deref().map(|s| &s[..12.min(s.len())]),
        auto_update = cfg.auto_update,
        "sdc-client starting"
    );

    let mut fake = if args.fake_device {
        Some(device::Deck::fake(args.fake_out)?)
    } else {
        None
    };

    loop {
        let mut real;
        let deck = match fake.as_mut() {
            Some(d) => d,
            None => match device::Deck::open_first().await {
                Ok(d) => {
                    real = d;
                    &mut real
                }
                Err(e) => {
                    warn!("{e:#}; retrying in 2s");
                    device::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            },
        };
        let why = session::run(&cfg, deck).await;
        warn!("device lost: {why}; looking for a Stream Deck again");
        device::sleep(Duration::from_secs(1)).await;
    }
}
