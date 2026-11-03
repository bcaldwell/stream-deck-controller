use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use sdc_server::clients::{self, ClientBinaries};
use sdc_server::ha::{self, HaConfig};
use sdc_server::{engine, http, preview, watch};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "sdc-server",
    version,
    about = "Stream Deck controller server",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// Lua config file (default: $SDC_CONFIG or ./config.lua)
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Render a page to PNGs (3x5 keys, 72px) for visual checks without hardware
    Render {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Output directory
        #[arg(long, default_value = "preview")]
        out: PathBuf,
        /// Page to render (default: the first device's home page)
        #[arg(long)]
        page: Option<String>,
        /// JSON file with a list of fake HA states ({entity_id, state, attributes}) to render with
        #[arg(long)]
        states: Option<PathBuf>,
    },
}

fn config_path(arg: Option<PathBuf>) -> Result<PathBuf> {
    let p = arg
        .or_else(|| std::env::var_os("SDC_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("config.lua"));
    std::path::absolute(&p).with_context(|| format!("resolving {}", p.display()))
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();

    if let Some(Command::Render {
        config,
        out,
        page,
        states,
    }) = cli.command
    {
        return preview::run(
            config_path(config)?,
            &out,
            page,
            HaConfig::from_env().ok(),
            states,
        )
        .await;
    }

    let config = config_path(cli.config)?;
    let listen = std::env::var("SDC_LISTEN").unwrap_or_else(|_| "0.0.0.0:7700".into());

    // Without Home Assistant there is nothing to show: fail like a bad config would.
    let ha_cfg = HaConfig::from_env()?;
    let (ha_tx, ha_rx) = mpsc::unbounded_channel();
    let engine_tx = engine::spawn(config.clone(), ha_tx, ha_cfg.clone())?;
    // Bridge HA events into the engine; the HA task reconnects on its own.
    tokio::spawn(ha::run(ha_cfg, ha_rx, engine_tx.clone()));
    let _watcher = watch::spawn(&config, engine_tx.clone())
        .inspect_err(|e| tracing::warn!("config hot reload disabled: {e:#}"))
        .ok();

    let listener = TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    let client_dir = std::env::var_os("SDC_CLIENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(clients::DEFAULT_DIR));
    let binaries = Arc::new(ClientBinaries::load(&client_dir));
    let token = std::env::var("SDC_CLIENT_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    if token.is_none() {
        tracing::warn!(
            "SDC_CLIENT_TOKEN not set: clients are unauthenticated, anyone who can reach the \
             server can connect"
        );
    }
    let deck_auth = http::DeckAuth::from_env();
    if deck_auth == http::DeckAuth::None {
        tracing::warn!(
            "SDC_DECK_AUTH=none: /deck is unauthenticated, anyone who can reach the server can \
             use the browser deck"
        );
    }
    tracing::info!("listening on ws://{}/ws", listener.local_addr()?);
    tokio::select! {
        _ = http::serve(listener, engine_tx, binaries, token, deck_auth) => {}
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }
    Ok(())
}
