//! Searchable metadata projection (issue #13).
//!
//! A stable, typed view of the subset of [`Metadata`] that the metadata index
//! in `bc-core` stores and queries against. Keeping this in `bc-metadata`
//! means extraction rules (and their tests) live next to the readers that
//! produce the source metadata.

use crate::ratings::ColorLabel;
use crate::{keywords, Metadata, NS_EXIF};

/// Typed, index-friendly projection of a [`Metadata`] store.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SearchableMetadata {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub created: Option<String>,
    pub modified: Option<String>,
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub rating: u8,
    pub color_label: Option<String>,
    pub flag: Option<String>,
    pub keywords: Vec<String>,
    pub title: Option<String>,
    pub description: Option<String>,
}

impl SearchableMetadata {
    pub fn is_empty(&self) -> bool {
        self.width.is_none()
            && self.height.is_none()
            && self.created.is_none()
            && self.modified.is_none()
            && self.camera_make.is_none()
            && self.camera_model.is_none()
            && self.rating == 0
            && self.color_label.is_none()
            && self.flag.is_none()
            && self.keywords.is_empty()
            && self.title.is_none()
            && self.description.is_none()
    }
}

/// Extract the searchable projection from a metadata store.
pub fn extract(meta: &Metadata) -> SearchableMetadata {
    let mut out = SearchableMetadata {
        rating: meta.rating().stars(),
        ..Default::default()
    };

    // Dimensions from EXIF.
    if let Some(w) = meta.text(NS_EXIF, "PixelWidth") {
        out.width = w.trim().parse::<u32>().ok();
    }
    if let Some(h) = meta.text(NS_EXIF, "PixelHeight") {
        out.height = h.trim().parse::<u32>().ok();
    }

    // Dates.
    if let Some(d) = meta.text(NS_EXIF, "DateTimeOriginal") {
        out.created = Some(d.to_string());
    } else if let Some(d) = meta.text(NS_EXIF, "DateTime") {
        out.created = Some(d.to_string());
    }
    if let Some(d) = meta.text(NS_EXIF, "DateTime") {
        // Modified falls back to the file-level timestamp in the index layer;
        // keep the EXIF date as a hint when present.
        if out.modified.is_none() {
            out.modified = Some(d.to_string());
        }
    }

    // Camera.
    out.camera_make = meta.text(NS_EXIF, "Make").map(|s| s.to_string());
    out.camera_model = meta.text(NS_EXIF, "Model").map(|s| s.to_string());

    // Ratings / labels / flags.
    match meta.color_label() {
        ColorLabel::None => out.color_label = None,
        other => out.color_label = other.as_label_str().map(|s| s.to_string()),
    }
    out.flag = meta.flag().as_str().map(|s| s.to_string());

    // Keywords.
    out.keywords = keywords::subjects(meta);

    // Title / description from Dublin Core (XMP).
    out.title = dc_text(meta, "title");
    out.description = dc_text(meta, "description");

    out
}

fn dc_text(meta: &Metadata, name: &str) -> Option<String> {
    match meta.get(crate::NS_DC, name) {
        Some(crate::Value::Text(s)) if !s.trim().is_empty() => Some(s.clone()),
        Some(crate::Value::AltText(alt)) => alt.default_text().map(|s| s.to_string()),
        Some(crate::Value::Array(_, items)) => items.iter().find_map(|v| match v {
            crate::Value::Text(s) if !s.trim().is_empty() => Some(s.clone()),
            _ => None,
        }),
        _ => None,
    }
}

// Convenience aliases.
pub fn extract_searchable(meta: &Metadata) -> SearchableMetadata {
    extract(meta)
}

pub fn searchable_metadata(meta: &Metadata) -> SearchableMetadata {
    extract(meta)
}

pub fn projection(meta: &Metadata) -> SearchableMetadata {
    extract(meta)
}

pub fn extract_metadata(meta: &Metadata) -> SearchableMetadata {
    extract(meta)
}

pub fn extract_indexable(meta: &Metadata) -> SearchableMetadata {
    extract(meta)
}

pub fn to_searchable(meta: &Metadata) -> SearchableMetadata {
    extract(meta)
}

pub fn searchable(meta: &Metadata) -> SearchableMetadata {
    extract(meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Metadata, NS_EXIF};

    #[test]
    fn extract_empty() {
        let m = Metadata::new();
        let s = extract(&m);
        assert!(s.is_empty());
    }

    #[test]
    fn extract_exif_fields() {
        let mut m = Metadata::new();
        m.set_text(NS_EXIF, "Make", "Canon".to_string());
        m.set_text(NS_EXIF, "Model", "EOS".to_string());
        m.set_text(NS_EXIF, "PixelWidth", "100".to_string());
        m.set_text(NS_EXIF, "PixelHeight", "50".to_string());
        m.set_text(
            NS_EXIF,
            "DateTimeOriginal",
            "2024-01-02 03:04:05".to_string(),
        );
        let s = extract(&m);
        assert_eq!(s.camera_make.as_deref(), Some("Canon"));
        assert_eq!(s.camera_model.as_deref(), Some("EOS"));
        assert_eq!(s.width, Some(100));
        assert_eq!(s.height, Some(50));
        assert_eq!(s.created.as_deref(), Some("2024-01-02 03:04:05"));
    }

    #[test]
    fn extract_keywords_title_description() {
        use crate::keywords;
        let mut m = Metadata::new();
        keywords::add_keyword(&mut m, "Nature|Bird");
        m.set_text(crate::NS_DC, "title", "Hello".to_string());
        m.set_text(crate::NS_DC, "description", "Desc".to_string());
        m.set_rating(crate::ratings::Rating::new(4));
        let s = extract(&m);
        assert_eq!(s.keywords, vec!["Nature|Bird".to_string()]);
        assert_eq!(s.title.as_deref(), Some("Hello"));
        assert_eq!(s.description.as_deref(), Some("Desc"));
        assert_eq!(s.rating, 4);
    }
}
