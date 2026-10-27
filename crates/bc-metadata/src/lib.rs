//! bc-metadata: EXIF / IPTC / XMP metadata read & write.
//!
//! Phase 0 skeleton only. Real implementation lands in Phase 3,
//! using published open specs (ISO 16684 XMP, EXIF, IPTC) — see CLEANROOM.md.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum MetadataError {
    #[error("unsupported file type")]
    Unsupported,

    #[error("malformed metadata: {0}")]
    Malformed(String),
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
