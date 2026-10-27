//! bc-thumbnails: SQLite-backed thumbnail cache and image decoders.

pub mod cache;
pub mod decoder;

use std::path::Path;

pub use cache::CacheStore;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ThumbError {
    #[error("decode failed for {path}: {source}")]
    Decode {
        path: String,
        source: image::ImageError,
    },

    #[error("cache error: {0}")]
    Cache(#[from] rusqlite::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// A decoded thumbnail ready for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    /// RGBA8 pixel data, row-major.
    pub pixels: Vec<u8>,
}

impl Thumbnail {
    /// True if the pixel buffer size matches the dimensions.
    pub fn is_consistent(&self) -> bool {
        self.pixels.len() == (self.width as usize) * (self.height as usize) * 4
    }
}

/// Load a thumbnail from the cache if valid for `(mtime_ns, size)`,
/// otherwise decode from disk and store it back.
///
/// `cache` may be `None` to always decode without caching (tests, previews).
pub fn load_or_generate(
    cache: Option<&CacheStore>,
    path: &Path,
    mtime_ns: i64,
    size: u64,
    max_edge: u32,
) -> Result<Thumbnail, ThumbError> {
    if let Some(cache) = cache {
        match cache.get(path, mtime_ns, size, max_edge) {
            Ok(Some(thumb)) => return Ok(thumb),
            Ok(None) => {}
            Err(e) => tracing::warn!("cache read failed, falling back to decode: {e}"),
        }
    }

    let thumb = decoder::decode_thumbnail(path, max_edge)?;

    if let Some(cache) = cache {
        if let Err(e) = cache.put(path, mtime_ns, size, max_edge, &thumb) {
            tracing::warn!("cache write failed: {e}");
        }
    }

    Ok(thumb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_test_png(path: &Path, w: u32, h: u32) {
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(w, h, |x, y| {
            image::Rgba([(x * 4 % 256) as u8, (y * 4 % 256) as u8, 128, 255])
        }));
        let file = std::fs::File::create(path).unwrap();
        img.write_to(
            &mut std::io::BufWriter::new(std::io::BufWriter::new(
                std::io::BufWriter::new(std::io::BufWriter::new(std::io::BufWriter::new(
                    std::io::BufWriter::new(std::io::BufWriter::new(file),
                ),),
            ),),
        ),), image::ImageFormat::Png)
        .unwrap();
    }

    #[test]
    fn decode_respects_max_edge() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.png");
        write_test_png(&p, 300, 150);

        let thumb = decoder::decode_thumbnail(&p, 100).unwrap();
        assert!(thumb.width <= 100 && thumb.height <= 100);
        assert!(thumb.is_consistent());
        // aspect preserved
        assert_eq!(thumb.width, 100);
        assert_eq!(thumb.height, 50);
    }

    #[test]
    fn cache_roundtrip_and_invalidation() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.png");
        write_test_png(&p, 60, 40);

        let cache = CacheStore::open_in_memory().unwrap();
        let t1 = load_or_generate(Some(&cache), &p, 111, 999, 256).unwrap();
        // Second call must come from cache (identical result even if file changed
        // on disk — validity is keyed on mtime/size, which we keep constant).
        let t2 = load_or_generate(Some(&cache), &p, 111, 999, 256).unwrap();
        assert_eq!(t1, t2);

        // Changed mtime => cache miss => re-decode.
        let t3 = load_or_generate(Some(&cache), &p, 222, 999, 256).unwrap();
        assert_eq!(t1, t3); // same file, freshly decoded
    }
}
