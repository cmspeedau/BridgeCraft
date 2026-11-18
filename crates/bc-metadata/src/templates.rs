//! Custom metadata templates (roadmap Phase 3).
//!
//! A template is a named, persistable bundle of metadata properties that can be
//! applied to an asset with a chosen merge policy (replace / merge / append).
//!
//! Pure logic and the serialisable shape live here. **Persistence does not**: the
//! document, its path, its version and the atomic write belong to
//! `bc_core::MetadataTemplates`, beside the saved searches, because that is where
//! every other user-authored store in this project lives and where the one shared
//! atomic writer is. Four file helpers used to sit in this file with no caller and
//! a plain `fs::write`; deleting them is what stops a future caller from picking
//! the unsafe one by accident.

use serde::{Deserialize, Serialize};

use crate::Metadata;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TemplateMode {
    /// Overwrite the target's properties with the template's.
    #[default]
    Replace,
    /// Only fill properties the target does not already have.
    Merge,
    /// Overwrite individual properties, but merge array values (e.g. keywords).
    Append,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetadataTemplate {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub metadata: Metadata,
    #[serde(default)]
    pub mode: TemplateMode,
}

impl MetadataTemplate {
    pub fn new(name: impl Into<String>, metadata: Metadata) -> Self {
        Self {
            name: name.into(),
            description: None,
            metadata,
            mode: TemplateMode::default(),
        }
    }

    pub fn with_mode(mut self, mode: TemplateMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// Apply a template to a target metadata store using the template's mode.
pub fn apply(target: &mut Metadata, template: &MetadataTemplate) {
    apply_with_mode(target, &template.metadata, template.mode);
}

/// Apply a metadata bundle with an explicit mode.
pub fn apply_with_mode(target: &mut Metadata, source: &Metadata, mode: TemplateMode) {
    match mode {
        TemplateMode::Replace => {
            target.properties = source.properties.clone();
            // native blocks: replace only if source has any
            if !source.native.is_empty() {
                target.native = source.native.clone();
            }
        }
        TemplateMode::Merge => {
            for (k, v) in &source.properties {
                target
                    .properties
                    .entry(k.clone())
                    .or_insert_with(|| v.clone());
            }
            for nb in &source.native {
                if !target.native.iter().any(|b| b.label == nb.label) {
                    target.add_native(nb.clone());
                }
            }
        }
        TemplateMode::Append => {
            for (k, v) in &source.properties {
                match (target.properties.get(k), v) {
                    (Some(Value::Array(_, existing)), Value::Array(kind, incoming)) => {
                        let mut merged = existing.clone();
                        for item in incoming {
                            if !merged.contains(item) {
                                merged.push(item.clone());
                            }
                        }
                        target
                            .properties
                            .insert(k.clone(), Value::Array(*kind, merged));
                    }
                    _ => {
                        target.properties.insert(k.clone(), v.clone());
                    }
                }
            }
            for nb in &source.native {
                target.add_native(nb.clone());
            }
        }
    }
}

/// Apply and return a new Metadata (non-mutating convenience).
pub fn apply_template(target: &Metadata, template: &MetadataTemplate) -> Metadata {
    let mut out = target.clone();
    apply(&mut out, template);
    out
}

// Need Value in scope.
use crate::Value;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Metadata, NS_DC, NS_XMP};

    #[test]
    fn replace_overwrites() {
        let mut target = Metadata::new();
        target.set_text(NS_XMP, "Rating", "2".to_string());
        let mut src = Metadata::new();
        src.set_text(NS_XMP, "Rating", "5".to_string());
        let tpl = MetadataTemplate::new("t", src);
        apply(&mut target, &tpl);
        assert_eq!(target.text(NS_XMP, "Rating"), Some("5"));
    }

    #[test]
    fn merge_keeps_existing() {
        let mut target = Metadata::new();
        target.set_text(NS_XMP, "Rating", "2".to_string());
        let mut src = Metadata::new();
        src.set_text(NS_XMP, "Rating", "5".to_string());
        src.set_text(NS_DC, "title", "Hello".to_string());
        let tpl = MetadataTemplate::new("t", src).with_mode(TemplateMode::Merge);
        apply(&mut target, &tpl);
        assert_eq!(target.text(NS_XMP, "Rating"), Some("2"));
        assert_eq!(target.text(NS_DC, "title"), Some("Hello"));
    }

    #[test]
    fn append_merges_keywords() {
        use crate::keywords;
        let mut target = Metadata::new();
        keywords::add_keyword(&mut target, "A");
        let mut src = Metadata::new();
        keywords::add_keyword(&mut src, "B");
        keywords::add_keyword(&mut src, "A");
        let tpl = MetadataTemplate::new("t", src).with_mode(TemplateMode::Append);
        apply(&mut target, &tpl);
        let ks = keywords::subjects(&target);
        assert!(ks.contains(&"A".to_string()));
        assert!(ks.contains(&"B".to_string()));
        // no duplicate A
        assert_eq!(ks.iter().filter(|k| *k == "A").count(), 1);
    }

    /// The shape on disk, asserted here rather than in the store's tests: the
    /// store writes exactly this serialisation, so a change to any of these names
    /// is a change to files already on someone's disk and has to be a versioned
    /// one. `mode` and `description` are optional on read, which is what lets a
    /// document written before they existed still load.
    #[test]
    fn the_serialised_shape_is_the_contract_with_the_saved_document() {
        let mut m = Metadata::new();
        m.set_text(NS_XMP, "Rating", "4".to_string());
        let tpl = MetadataTemplate::new("MyTpl", m)
            .with_description("desc")
            .with_mode(TemplateMode::Append);

        let json = serde_json::to_string(&tpl).unwrap();
        for key in ["\"name\"", "\"description\"", "\"metadata\"", "\"mode\""] {
            assert!(json.contains(key), "{key} missing from {json}");
        }
        let back: MetadataTemplate = serde_json::from_str(&json).unwrap();
        assert_eq!(back, tpl);

        // A document with only the required members still reads, and gets the
        // default mode rather than failing: templates are meant to be hand-editable,
        // and rejecting `{"name":"plain","metadata":{}}` would report the user's own
        // file as corrupt.
        for minimal in [
            r#"{"name":"plain","metadata":{"properties":[]}}"#,
            r#"{"name":"plain","metadata":{}}"#,
        ] {
            let back: MetadataTemplate = serde_json::from_str(minimal).unwrap_or_else(|error| {
                panic!("{minimal} should read as a template, and did not: {error}")
            });
            assert_eq!(back.name, "plain");
            assert_eq!(back.description, None);
            assert_eq!(back.mode, TemplateMode::Replace);
            assert!(back.metadata.properties.is_empty());
            assert!(back.metadata.native.is_empty());
        }
    }
}
