//! BridgeCraft — clean-room asset browser in Rust (egui/eframe frontend).

mod app;

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use bc_thumbnails::{load_or_generate, CacheStore};

const THUMB_MAX_EDGE: u32 = 256;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("--rebuild-cache") {
        let folder = args.next().ok_or_else(|| {
            anyhow::anyhow!("usage: bridgecraft --rebuild-cache <folder> [cache-file]")
        })?;
        let cache_path = match args.next() {
            Some(path) => PathBuf::from(path),
            None => std::env::current_dir()?.join(".bridgecraft-thumbs.db"),
        };
        if args.next().is_some() {
            anyhow::bail!("usage: bridgecraft --rebuild-cache <folder> [cache-file]");
        }
        return rebuild_cache(Path::new(&folder), &cache_path);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let root = std::env::args().nth(1);

    // Thumbnail cache lives next to the CWD; the worker thread owns it.
    let cache_path = std::env::current_dir()
        .ok()
        .map(|d| d.join(".bridgecraft-thumbs.db"));

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_title("BridgeCraft"),
        ..Default::default()
    };

    eframe::run_native(
        "BridgeCraft",
        options,
        Box::new(|cc| Box::new(app::BridgeApp::new(cc, root, cache_path))),
    )
    .map_err(|e| anyhow::anyhow!("eframe exited with an error: {e}"))?;
    Ok(())
}


/// Rebuild thumbnails for supported images below `folder`, reporting individual failures.
fn rebuild_cache(folder: &Path, cache_path: &Path) -> anyhow::Result<()> {
    if !folder.is_dir() {
        anyhow::bail!("not a folder: {}", folder.display());
    }
    let cache = CacheStore::open(cache_path)?;
    let cache_real = std::fs::canonicalize(cache_path).ok();
    let mut pending = vec![folder.to_path_buf()];
    let (mut processed, mut failed) = (0usize, 0usize);

    while let Some(directory) = pending.pop() {
        for item in std::fs::read_dir(&directory)? {
            let item = item?;
            let path = item.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            if !metadata.is_file()
                || std::fs::canonicalize(&path).ok().as_ref() == cache_real.as_ref()
            {
                continue;
            }
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos() as i64)
                .unwrap_or(0);
            match load_or_generate(
                Some(&cache),
                &path,
                modified,
                metadata.len(),
                THUMB_MAX_EDGE,
            ) {
                Ok(_) => processed += 1,
                Err(error) => {
                    failed += 1;
                    eprintln!("Could not rebuild {}: {error}", path.display());
                }
            }
        }
    }
    let pruned = cache.prune_missing()?;
    println!(
        "Cache rebuild complete: {processed} processed, {failed} failed, {pruned} stale entries removed."
    );
    Ok(())
}
