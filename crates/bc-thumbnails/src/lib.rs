//! bc-thumbnails: SQLite-backed thumbnail cache and decoders.
//!
//! Phase 0 skeleton only. Cache schema and decoder backends land in Phase 2.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ThumbError {
    #[error("decode failed: {0}")]
    Decode(String),

    #[error("cache error: {0}")]
    Cache(String),
}

/// A decoded thumbnail ready for display.
#[derive(Debug, Clone)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    /// RGBA8 pixel data, row-major.
    pub pixels: Vec<u8>,
}

#[cfg(test)]
mod tests {
    #[test]
    fn placeholder() {
        // Ensures the crate builds and runs tests in CI until Phase 2.
    }
}
