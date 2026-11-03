//! sdc-client: connects a local Stream Deck to an sdc-server over WebSocket.

mod device;
mod link;
mod session;
mod sleep;
mod update;

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

    /// Name of the server's `sdc.device` this deck should use (default: the server's
    /// `sdc.default_device` for this kind of deck)
    #[arg(long, env = "SDC_DEVICE")]
    device: Option<String>,

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

    /// While the server has the screen off, close the device so the Stream Deck can USB-suspend
    /// (saves power on battery; needs the udev rule from deploy/pi). A key press wakes it. Linux
    /// only; default on there
    #[arg(
        long,
        env = "SDC_USB_SUSPEND",
        default_value_t = cfg!(target_os = "linux"),
        value_parser = clap::builder::BoolishValueParser::new(),
        num_args = 0..=1,
        default_missing_value = "true",
        value_name = "BOOL"
    )]
    usb_suspend: bool,

    /// Do not replace this binary when the server offers a different one
    #[arg(long, env = "SDC_NO_UPDATE")]
    no_update: bool,
}

fn hostname() -> String {
    let name = std::fs::read_to_string("/etc/hostname").or_else(|_| {
        let out = std::process::Command::new("hostname").output()?;
        Ok::<_, std::io::Error>(String::from_utf8_lossy(&out.stdout).into_owned())
    });
    name.expect("cannot determine the hostname; pass --client-id")
        .trim()
        .to_string()
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
            let i = device::device_info(d.kind, d.serial.clone());
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
        let keys = deck.info.key_count();
        for k in 0..keys {
            deck.fill(k, device::hsv_color(k as f32 / keys as f32))
                .await?;
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
        device: args.device.filter(|d| !d.is_empty()),
        brightness: args.brightness,
        client_sha256: update::exe_sha256()?,
        auto_update: !args.no_update,
        usb_suspend: args.usb_suspend,
    };
    info!(
        server = %cfg.server,
        client_id = %cfg.client_id,
        sha256 = &cfg.client_sha256[..12],
        auto_update = cfg.auto_update,
        "sdc-client starting"
    );

    loop {
        match device::Deck::open_first().await {
            Ok(mut deck) => {
                let why = session::run(&cfg, &mut deck).await;
                warn!("device lost: {why}; looking for a Stream Deck again");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) => {
                warn!("{e:#}; retrying in 2s");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}
