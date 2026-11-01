//! Config hot reload: watch the config's parent directory (so atomic-save editors
//! that replace the file keep working) and debounce bursts of events.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc::UnboundedSender;

use crate::engine::EngineEvent;

const DEBOUNCE: Duration = Duration::from_millis(250);

/// Keep the returned watcher alive for as long as reloads are wanted.
pub fn spawn(
    config_path: &Path,
    engine: UnboundedSender<EngineEvent>,
) -> Result<RecommendedWatcher> {
    let dir = config_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(move |ev| {
        let _ = tx.send(ev);
    })?;
    // Non-recursive: the config dir may be a repo root containing `target/`.
    watcher.watch(&dir, RecursiveMode::NonRecursive)?;
    std::thread::Builder::new()
        .name("sdc-watch".into())
        .spawn(move || {
            while let Ok(first) = rx.recv() {
                if !relevant(&first) {
                    continue;
                }
                while rx.recv_timeout(DEBOUNCE).is_ok() {}
                tracing::info!("config change detected, reloading");
                if engine.send(EngineEvent::Reload).is_err() {
                    return;
                }
            }
        })?;
    Ok(watcher)
}

fn relevant(ev: &notify::Result<notify::Event>) -> bool {
    use notify::EventKind::*;
    matches!(ev, Ok(e) if matches!(e.kind, Create(_) | Modify(_) | Remove(_)))
}
