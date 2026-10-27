//! BridgeCraft — clean-room asset browser in Rust.
//! Phase 1: file browsing core running as a textual shell (GUI lands after
//! the framework spike decision record).

use bc_core::browser::BrowserState;
use bc_core::fs::{SortKey, SortDir};
use bc_core::watcher::Watcher;
use std::path::Path;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    tracing::info!("BridgeCraft starting — textual shell (Phase 1)");

    let root = std::env::args()
        .nth(1)
        .unwrap_or_else(|| ".".to_string());

    let mut state = BrowserState::default();
    state.open(Path::new(&root))?;
    state.set_sort(SortKey::Name, SortDir::Asc);
    tracing::info!("openened {}: {} entries", root, state.len());
    for (i, e) in state.entries.iter().take(20).enumerate() {
        let marker = if state.selection.contains(i) { "[*]" } else { "[ ]" };
        let kind = if e.is_dir { "DIR " } else { "FILE" };
        tracing::info!("{} {} {}", marker, kind, e.name().to_string_lossy());
    }
    if state.len() > 20 {
        tracing::info!("... and {} more", state.len() - 20);
    }

    // Demonstrate the live watcher for one second.
    let watcher = Watcher::new(Path::new(&root))?;
    tracing::info!("watching {} for changes (2s demo)...", root);
    std::thread::sleep(std::time::Duration::from_secs(2));
    let events = watcher.drain();
    tracing::info!("{} filesystem events observed", events.len());

    Ok(())
}
