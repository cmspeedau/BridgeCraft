//! bc-metadata: EXIF / IPTC / XMP metadata read & write.
//!
//! The structured metadata model (issue #5) lives here: an XMP-data-model-shaped
//! [`Metadata`] store is the canonical representation, with EXIF and IPTC
//! treated as projections. EXIF reading (JPEG `APP1` and TIFF-family
//! containers, pure Rust) is implemented in [`exif`].
//!
//! Clean-room per CLEANROOM.md. See `docs/design/0005-structured-metadata-model.md`.

pub mod exif;
pub mod iptc;
pub mod ratings;
pub mod xmp;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Namespace constants
// ---------------------------------------------------------------------------

/// Short, stable aliases used by the EXIF projection layer. The model itself
/// is namespace-agnostic (any string), so XMP readers use full URIs below.
pub const NS_EXIF: &str = "exif";
pub const NS_GPS: &str = "gps";

/// BridgeCraft-namespaced XMP extension namespace (issue #10, approve/reject flags).
pub const NS_BRIDGECRAFT: &str = "https://bridgecraft.app/xmp/1.0/";

/// XMP / Dublin Core / Photoshop / TIFF / IPTC namespace URIs (ISO 16684-1).
pub const NS_DC: &str = "http://purl.org/dc/elements/1.1/";
pub const NS_XMP: &str = "http://ns.adobe.com/xap/1.0/";
pub const NS_XMP_MM: &str = "http://ns.adobe.com/xap/1.0/mm/";
pub const NS_PHOTOSHOP: &str = "http://ns.adobe.com/photoshop/1.0/";
pub const NS_TIFF: &str = "http://ns.adobe.com/tiff/1.0/";
pub const NS_IPTC_CORE: &str = "http://iptc.org/std/Iptc4xmpCore/1.0/xmlns/";
pub const NS_IPTC_EXT: &str = "http://iptc.org/std/Iptc4xmpExt/2008-02-29/";

// ---------------------------------------------------------------------------
// Error taxonomy
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum MetadataError {
    #[error("unsupported file type")]
    Unsupported,

    #[error("malformed metadata: {0}")]
    Malformed(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("unsupported value type: {0}")]
    UnsupportedType(String),

    #[error("malformed metadata packet: {0}")]
    MalformedPacket(String),

    #[error("unsupported write target: {0}")]
    UnsupportedWriteTarget(String),

    #[error("invalid namespace: {0}")]
    InvalidNamespace(String),

    #[error("property not found")]
    NotFound,
}

// ---------------------------------------------------------------------------
// Qualified name
// ---------------------------------------------------------------------------

/// A namespaced property name (XMP QName). `namespace` is a URI in XMP mode;
/// projection layers may use short stable aliases (see `NS_EXIF`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct QualifiedName {
    pub namespace: String,
    pub name: String,
}

impl QualifiedName {
    pub fn new(namespace: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            name: name.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Value model
// ---------------------------------------------------------------------------

/// XMP array kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArrayKind {
    /// Unordered array (rdf:Bag).
    Bag,
    /// Ordered array (rdf:Seq).
    Seq,
    /// Alternatives (rdf:Alt); first entry is the default.
    Alt,
}

/// Language alternates (XMP `rdf:Alt` with `xml:lang` qualifiers). The default
/// entry is conventionally keyed by `"x-default"`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LangAlt {
    pub entries: BTreeMap<String, String>,
}

impl LangAlt {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, lang: impl Into<String>, text: impl Into<String>) {
        self.entries.insert(lang.into(), text.into());
    }

    pub fn get(&self, lang: &str) -> Option<&str> {
        self.entries.get(lang).map(String::as_str)
    }

    /// Default text: `x-default` if present, otherwise the first entry.
    pub fn default_text(&self) -> Option<&str> {
        if let Some(t) = self.entries.get("x-default") {
            return Some(t.as_str());
        }
        self.entries.values().next().map(String::as_str)
    }
}

/// A structured metadata value in the XMP data model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Text(String),
    Boolean(bool),
    Integer(i64),
    Real(f64),
    Date(String),
    Uri(String),
    Array(ArrayKind, Vec<Value>),
    AltText(LangAlt),
    Struct(BTreeMap<QualifiedName, Value>),
    Binary(Vec<u8>),
}

impl Value {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Boolean(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Value::Integer(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_real(&self) -> Option<f64> {
        match self {
            Value::Real(n) => Some(*n),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Native blocks (verbatim preservation)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeSource {
    Exif,
    Iptc,
    Xmp,
    Other,
}

/// A non-XMP-projectable payload preserved verbatim across read/write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeBlock {
    pub source: NativeSource,
    pub label: String,
    pub data: Vec<u8>,
    pub preserve: bool,
}

// ---------------------------------------------------------------------------
// Metadata store
// ---------------------------------------------------------------------------

/// Structured metadata store. XMP-shaped properties are canonical; native
/// blocks carry payloads that cannot be projected (§2 of the RFC).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Metadata {
    #[serde(with = "props_serde")]
    pub properties: BTreeMap<QualifiedName, Value>,
    pub native: Vec<NativeBlock>,
}

impl Metadata {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.properties.is_empty() && self.native.is_empty()
    }

    pub fn len(&self) -> usize {
        self.properties.len() + self.native.len()
    }

    pub fn set(&mut self, namespace: impl Into<String>, name: impl Into<String>, value: Value) {
        self.properties
            .insert(QualifiedName::new(namespace, name), value);
    }

    pub fn set_text(
        &mut self,
        namespace: impl Into<String>,
        name: impl Into<String>,
        value: impl Into<String>,
    ) {
        self.set(namespace, name, Value::Text(value.into()));
    }

    pub fn get(&self, namespace: &str, name: &str) -> Option<&Value> {
        self.properties.get(&QualifiedName::new(namespace, name))
    }

    pub fn text(&self, namespace: &str, name: &str) -> Option<&str> {
        self.get(namespace, name).and_then(Value::as_text)
    }

    pub fn remove(&mut self, namespace: &str, name: &str) -> Option<Value> {
        self.properties.remove(&QualifiedName::new(namespace, name))
    }

    pub fn properties(&self) -> &BTreeMap<QualifiedName, Value> {
        &self.properties
    }

    pub fn native(&self) -> &[NativeBlock] {
        &self.native
    }

    pub fn add_native(&mut self, block: NativeBlock) {
        self.native.push(block);
    }
}

// ---------------------------------------------------------------------------
// Traits
// ---------------------------------------------------------------------------

/// Placeholder trait for metadata readers (EXIF, IPTC, XMP).
pub trait Reader {
    /// Read all supported metadata fields from `data`.
    fn read(&self, data: &[u8]) -> Result<Metadata, MetadataError>;
}

/// Writer / update trait paired with [`Reader`]. Container-specific writers
/// implement `write`; `update` defaults to a full rewrite.
pub trait Writer {
    /// Write `metadata` into the container represented by `data`, returning the
    /// updated container bytes. Sidecar writers take an empty `data` slice.
    fn write(&self, data: &[u8], metadata: &Metadata) -> Result<Vec<u8>, MetadataError>;

    /// Apply `metadata` as an update to `data`. Defaults to a full `write`.
    fn update(&self, data: &[u8], metadata: &Metadata) -> Result<Vec<u8>, MetadataError> {
        self.write(data, metadata)
    }
}

// ---------------------------------------------------------------------------
// Write policy (default: Auto)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WritePolicy {
    InPlace,
    Sidecar,
    #[default]
    Auto,
}

// ---------------------------------------------------------------------------
// Serde helper: serialize properties as a list of (key, value) pairs so that
// the QualifiedName struct keys survive JSON (which only allows string keys).
// ---------------------------------------------------------------------------

mod props_serde {
    use super::*;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(props: &BTreeMap<QualifiedName, Value>, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let v: Vec<(&QualifiedName, &Value)> = props.iter().collect();
        v.serialize(s)
    }

    pub fn deserialize<'de, D>(d: D) -> Result<BTreeMap<QualifiedName, Value>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let v: Vec<(QualifiedName, Value)> = Vec::deserialize(d)?;
        Ok(v.into_iter().collect())
    }
}

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

    #[test]
    fn metadata_set_and_get_roundtrip() {
        let mut m = Metadata::new();
        assert!(m.is_empty());
        m.set_text(NS_EXIF, "Make", "Canon".to_string());
        m.set(NS_EXIF, "Orientation", Value::Integer(6));
        assert_eq!(m.text(NS_EXIF, "Make"), Some("Canon"));
        assert_eq!(
            m.get(NS_EXIF, "Orientation").and_then(Value::as_integer),
            Some(6)
        );
        assert!(!m.is_empty());
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn lang_alt_default_text() {
        let mut la = LangAlt::new();
        la.insert("fr-FR", "Bonjour");
        la.insert("x-default", "Hello");
        assert_eq!(la.default_text(), Some("Hello"));
    }

    #[test]
    fn array_and_struct_values() {
        let mut m = Metadata::new();
        m.set(
            NS_DC,
            "subject",
            Value::Array(
                ArrayKind::Bag,
                vec![Value::Text("a".into()), Value::Text("b".into())],
            ),
        );
        let mut st = BTreeMap::new();
        st.insert(
            QualifiedName::new(NS_XMP, "CreatorTool"),
            Value::Text("bc".into()),
        );
        m.set(NS_XMP, "nested", Value::Struct(st));
        assert_eq!(
            m.get(NS_DC, "subject").and_then(|v| match v {
                Value::Array(_, xs) => Some(xs.len()),
                _ => None,
            }),
            Some(2)
        );
        assert!(m.get(NS_XMP, "nested").is_some());
    }

    #[test]
    fn native_block_preserved() {
        let mut m = Metadata::new();
        m.add_native(NativeBlock {
            source: NativeSource::Exif,
            label: "maker_note".into(),
            data: vec![1, 2, 3],
            preserve: true,
        });
        assert_eq!(m.native().len(), 1);
        assert!(m.native()[0].preserve);
    }

    #[test]
    fn qualified_name_ordering() {
        let a = QualifiedName::new("a", "z");
        let b = QualifiedName::new("b", "a");
        assert!(a < b);
    }

    #[test]
    fn write_policy_default_is_auto() {
        assert_eq!(WritePolicy::default(), WritePolicy::Auto);
    }

    #[test]
    fn serde_roundtrip() {
        let mut m = Metadata::new();
        m.set_text(NS_EXIF, "Make", "Canon".to_string());
        let json = serde_json::to_string(&m).unwrap();
        let back: Metadata = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }
}
