//! BridgeCraft — clean-room asset browser in Rust.
//! GUI shell is a placeholder until the Phase 0 framework spike concludes.

use std::path::Path;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    tracing::info!("BridgeCraft starting — GUI placeholder (thumbnail engine wired)");

    let root = std::env::args()
        .nth(1)
        .unwrap_or_else(|| ".".to_string());
    let root = Path::new(&root);

    // Phase 2: thumbnail cache lives next to the binary's CWD.
    let cache_path = std::env::current_dir()?.join(".bridgecraft-thumbs.db");
    let cache = match bc_thumbnails::CacheStore::open(&cache_path) {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!("thumbnail cache unavailable, decoding without cache: {e}");
            None
        }
    };

    let entries = bc_core::fs::list_dir(root)?;
    const MAX_EDGE: u32 = 256;

    let mut hits = 0usize;
    let mut misses = 0usize;
    let mut errors = 0usize;

    for entry in &entries {
        if entry.is_dir {
            continue;
        }
        let mtime_ns = entry
            .modified
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);

        match bc_thumbnails::load_or_generate(
            cache.as_ref(),
            &entry.path,
            mtime_ns,
            entry.size,
            MAX_EDGE,
        ) {
            // First pass in a fresh directory: everything decodes; a second
            // identical run should report hits. We probe the cache once more
            // afterwards to classify hits/misses for the demo log.
            Ok(_) => {
                if cache
                    .as_ref()
                    .map(|c| c.get(&entry.path, mtime_ns, entry.size, MAX_EDGE).is_ok_and(|r| r.is_some()))
                    .unwrap_or(false)
                {
                    hits += 1;
                } else {
                    misses += 1;
                }
            }
            Err(e) => {
                errors += 1;
                tracing::debug!("skip {}: {e}", entry.path.display());
            }
        }
    }

    if let Some(cache) = &cache {
        let pruned = cache.prune_missing().unwrap_or(0);
        if pruned > 0 {
            tracing::info!("pruned {pruned} stale cache entries");
        }
    }

    tracing::info!(
        "scanned {}: {} entries, thumbs: {hits} cached, {misses} decoded, {errors} failed",
        root.display(),
        entries.len(),
    );

    Ok(())
}
