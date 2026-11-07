//! bc-metadata: EXIF / IPTC / XMP metadata read & write.
//!
//! EXIF reading (JPEG `APP1` and TIFF-family containers, pure Rust, built
//! from the published TIFF/EXIF specs) is implemented in [`exif`]. IPTC,
//! XMP, the structured model (issue #5) and writing land later in Phase 3.
//! Clean-room per CLEANROOM.md.

pub mod exif;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum MetadataError {
    #[error("unsupported file type")]
    Unsupported,

    #[error("malformed metadata: {0}")]
    Malformed(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Placeholder trait for metadata readers (EXIF, IPTC, XMP).
pub trait Reader {
    /// Read all supported metadata fields from `data`.
    fn read(&self, data: &[u8]) -> Result<Metadata, MetadataError>;
}

/// Flat key/value metadata store; structured model arrives in Phase 3.
pub type Metadata = std::collections::BTreeMap<String, String>;

#[cfg(test)]
mod tests {
    use super::*;

    struct Noop;

    impl Reader for Noop {
        fn read(&self, _data: &[u8]) -> Result<Metadata, MetadataError> {
            Ok(Metadata::new())
        }
    }

    #[test]
    fn noop_reader_returns_empty() {
        assert!(Noop.read(b"x").unwrap().is_empty());
    }
}
