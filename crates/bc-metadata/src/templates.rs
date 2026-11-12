//! Custom metadata templates (roadmap Phase 3).
//!
//! A template is a named, persistable bundle of metadata properties that can be
//! applied to an asset with a chosen merge policy (replace / merge / append).
//! Pure logic lives here; file I/O helpers are included for JSON persistence.

use std::path::{Path, PathBuf};

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

/// Save a template to a JSON file.
pub fn save_template(path: &Path, template: &MetadataTemplate) -> Result<(), crate::MetadataError> {
    let json = serde_json::to_string_pretty(template)
        .map_err(|e| crate::MetadataError::Malformed(e.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(crate::MetadataError::Io)?;
    }
    std::fs::write(path, json.as_bytes()).map_err(crate::MetadataError::Io)?;
    Ok(())
}

/// Load a template from a JSON file.
pub fn load_template(path: &Path) -> Result<MetadataTemplate, crate::MetadataError> {
    let bytes = std::fs::read(path).map_err(crate::MetadataError::Io)?;
    let template: MetadataTemplate = serde_json::from_slice(&bytes)
        .map_err(|e| crate::MetadataError::Malformed(e.to_string()))?;
    Ok(template)
}

/// Load all templates from a directory (files ending in .json).
pub fn load_templates(dir: &Path) -> Vec<MetadataTemplate> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(t) = load_template(&path) {
                    out.push(t);
                }
            }
        }
    }
    out
}

/// Default templates directory under a config root.
pub fn templates_dir(config_root: &Path) -> PathBuf {
    config_root.join("metadata_templates")
}

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

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.json");
        let mut m = Metadata::new();
        m.set_text(NS_XMP, "Rating", "4".to_string());
        let tpl = MetadataTemplate::new("MyTpl", m).with_description("desc");
        save_template(&path, &tpl).unwrap();
        let back = load_template(&path).unwrap();
        assert_eq!(back, tpl);
    }

    #[test]
    fn load_templates_dir() {
        let dir = tempfile::tempdir().unwrap();
        let tdir = dir.path().join("templates");
        std::fs::create_dir_all(&tdir).unwrap();
        let mut m = Metadata::new();
        m.set_text(NS_XMP, "Rating", "1".to_string());
        save_template(&tdir.join("a.json"), &MetadataTemplate::new("a", m)).unwrap();
        let loaded = load_templates(&tdir);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "a");
    }
}
