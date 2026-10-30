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
        let stats = rebuild_cache(Path::new(&folder), &cache_path)?;
        println!(
            "Cache rebuild complete: {} processed, {} failed, {} stale entries removed.",
            stats.processed, stats.failed, stats.pruned
        );
        return Ok(());
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

/// Summary counts for a completed cache rebuild.
struct RebuildStats {
    processed: usize,
    failed: usize,
    pruned: usize,
}

/// Rebuild thumbnails for supported images below `folder`, reporting
/// individual failures. Unreadable directories and undecodable files
/// are reported and skipped, not fatal.
fn rebuild_cache(folder: &Path, cache_path: &Path) -> anyhow::Result<RebuildStats> {
    if !folder.is_dir() {
        anyhow::bail!("not a folder: {}", folder.display());
    }
    let cache = CacheStore::open(cache_path)?;
    let cache_real = std::fs::canonicalize(cache_path).ok();
    let mut pending = vec![folder.to_path_buf()];
    let (mut processed, mut failed) = (0usize, 0usize);

    while let Some(directory) = pending.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                eprintln!("Skipping unreadable {}: {error}", directory.display());
                continue;
            }
        };
        for item in entries {
            let item = match item {
                Ok(item) => item,
                Err(error) => {
                    eprintln!("Skipping an entry in {}: {error}", directory.display());
                    continue;
                }
            };
            let path = item.path();
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    eprintln!("Skipping {}: {error}", path.display());
                    continue;
                }
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            if !metadata.is_file()
                || !bc_thumbnails::decoder::is_supported(&path)
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
    Ok(RebuildStats {
        processed,
        failed,
        pruned,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_image(path: &Path, w: u32, h: u32) {
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(w, h, |x, y| {
            image::Rgba([(x * 4 % 256) as u8, (y * 4 % 256) as u8, 128, 255])
        }));
        let format = match path.extension().and_then(|e| e.to_str()) {
            Some("jpg" | "jpeg") => image::ImageFormat::Jpeg,
            _ => image::ImageFormat::Png,
        };
        let file = std::fs::File::create(path).unwrap();
        let mut writer = std::io::BufWriter::new(file);
        img.write_to(&mut writer, format).unwrap();
    }

    fn run_with_cache(dir: &Path) -> RebuildStats {
        let cache_path = dir.join("thumbs.db");
        rebuild_cache(&dir.join("photos"), &cache_path).unwrap()
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn rebuild_processes_images_skips_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("photos");
        std::fs::create_dir_all(photos.join("sub")).unwrap();
        write_image(&photos.join("a.png"), 8, 4);
        write_image(&photos.join("UPPER.PNG"), 6, 6);
        write_image(&photos.join("sub/b.jpeg"), 4, 4);
        std::fs::write(photos.join("notes.txt"), b"not an image").unwrap();
        std::fs::write(photos.join("data.bin"), [0u8; 16]).unwrap();

        let stats = run_with_cache(dir.path());
        assert_eq!((stats.processed, stats.failed), (3, 0));

        // Entries landed in the cache and are still valid for current metadata.
        let cache = CacheStore::open(&dir.path().join("thumbs.db")).unwrap();
        let img = photos.join("a.png");
        let meta = std::fs::metadata(&img).unwrap();
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let hit = cache.get(&img, mtime, meta.len(), THUMB_MAX_EDGE).unwrap();
        assert!(hit.is_some(), "thumbnail missing after rebuild");
    }

    #[test]
    fn rebuild_prunes_stale_entries_and_survives_unreadable_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("photos");
        std::fs::create_dir_all(&photos).unwrap();
        write_image(&photos.join("a.png"), 4, 4);
        let gone = photos.join("gone.png");
        write_image(&gone, 4, 4);

        let cache_path = dir.path().join("thumbs.db");
        let stats = rebuild_cache(&photos, &cache_path).unwrap();
        assert_eq!((stats.processed, stats.failed), (2, 0));

        std::fs::remove_file(&gone).unwrap();
        // An unreadable subdirectory must be skipped, not fatal.
        #[cfg(unix)]
        {
            let locked = photos.join("locked");
            std::fs::create_dir(&locked).unwrap();
            set_mode(&locked, 0o000);
        }
        let stats = rebuild_cache(&photos, &cache_path).unwrap();
        #[cfg(unix)]
        set_mode(&photos.join("locked"), 0o755);
        assert_eq!((stats.processed, stats.failed), (1, 0));
        assert_eq!(stats.pruned, 1);
    }

    #[test]
    fn rebuild_rejects_missing_folder() {
        let dir = tempfile::tempdir().unwrap();
        let err = rebuild_cache(&dir.path().join("nope"), &dir.path().join("thumbs.db"));
        assert!(err.is_err());
    }
}
