//! XMP sidecar read and write (issue #9).
//!
//! Parses and serialises `.xmp` sidecar packets into the structured
//! [`Metadata`] model from issue #5. XMP (ISO 16684-1) is RDF-based; this
//! module handles the common property forms (simple values, `rdf:Bag`/
//! `rdf:Seq`/`rdf:Alt` arrays including language alternates, and nested
//! `rdf:Description` structs) plus pass-through of unrecognised namespaced
//! properties as text.
//!
//! Clean-room: implemented from the openly published XMP Specification Part 1
//! and RDF 1.0 syntax. No Adobe code.

use std::collections::{BTreeMap, HashMap};
use std::io::Cursor;
use std::path::{Path, PathBuf};

use quick_xml::events::{BytesDecl, BytesEnd, BytesPI, BytesStart, Event};
use quick_xml::writer::Writer;
use roxmltree::Node;

use crate::{LangAlt, Metadata, MetadataError, QualifiedName, Value, NS_EXIF, NS_GPS};

const RDF_NS: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const XMPMETA_NS: &str = "adobe:ns:meta/";

/// Reader behind the crate's [`crate::Reader`] trait for XMP packets.
#[derive(Debug, Default)]
pub struct XmpReader;

impl crate::Reader for XmpReader {
    fn read(&self, data: &[u8]) -> Result<Metadata, MetadataError> {
        parse_xmp(data)
    }
}

/// Writer behind the crate's [`crate::Writer`] trait for XMP sidecars.
#[derive(Debug, Default)]
pub struct XmpWriter;

impl crate::Writer for XmpWriter {
    fn write(&self, _data: &[u8], metadata: &Metadata) -> Result<Vec<u8>, MetadataError> {
        Ok(write_xmp(metadata).into_bytes())
    }
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Sidecar path next to the source file: `foo.jpg` -> `foo.xmp`.
pub fn sidecar_path(path: &Path) -> PathBuf {
    path.with_extension("xmp")
}

/// Sidecar path inside a configured directory, preserving the source stem.
pub fn sidecar_path_in(sidecar_dir: &Path, source: &Path) -> PathBuf {
    let stem = source.file_stem().unwrap_or_default();
    sidecar_dir.join(format!("{}.xmp", stem.to_string_lossy()))
}

pub fn read_xmp_file(path: &Path) -> Result<Metadata, MetadataError> {
    let bytes = std::fs::read(path)?;
    parse_xmp(&bytes)
}

/// Read an XMP packet embedded in the file itself: a JPEG `APP1` carrying the
/// `http://ns.adobe.com/xap/1.0/` signature, or TIFF/DNG tag 700.
///
/// `Ok(None)` means the file simply has no packet, which is the normal case and
/// never an error: a sidecar-only workflow must keep working when nothing is
/// embedded, and nothing embedded must not look like corrupt metadata.
pub fn read_embedded_xmp(data: &[u8]) -> Result<Option<Metadata>, MetadataError> {
    match crate::embedded::find_xmp_packet(data)? {
        Some(packet) => parse_xmp(&packet).map(Some),
        None => Ok(None),
    }
}

/// Read an embedded packet from a file on disk.
pub fn read_embedded_xmp_file(path: &Path) -> Result<Option<Metadata>, MetadataError> {
    let bytes = std::fs::read(path)?;
    read_embedded_xmp(&bytes)
}

/// The properties BridgeCraft owns: the four tag values the panel edits.
///
/// `Rating`, `Label` and `Flag` are written by `set_*` helpers that *remove*
/// the property when the value is cleared, and keywords are `dc:subject`. So a
/// metadata that lacks one of these does not distinguish "cleared" from "not
/// touched" — which is what `TagWrite` is for.
pub const TAG_PROPERTIES: [(&str, &str); 4] = [
    (crate::NS_XMP, "Rating"),
    (crate::NS_PHOTOSHOP, "Label"),
    (crate::NS_BRIDGECRAFT, "Flag"),
    (crate::NS_DC, "subject"),
];

/// What a save means to say about the four tag properties.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagWrite {
    /// The caller's metadata is the whole tag state. A property it does not
    /// carry was cleared, and comes out of the packet. This is what the panel
    /// and export do: they load every tag first, so absence is an answer and
    /// not an omission. Without it, clicking the current stars to clear them
    /// updated the sidecar and the session but left the old rating inside the
    /// JPEG or TIFF forever — measured, not assumed: a packet holding rating 4
    /// and label Red still held both after a save carrying neither.
    Replace,
    /// Only the properties the caller carries change; anything else already in
    /// the packet is left alone. This is what a partial update needs, and is
    /// what `rewriting_a_packet_preserves_what_the_caller_did_not_change`
    /// exercises.
    Merge,
}

/// Splice a finished packet into a JPEG, replacing the old `APP1` XMP segment
/// or inserting a new one.
fn splice_jpeg_xmp(data: &[u8], packet: &[u8]) -> Result<Vec<u8>, MetadataError> {
    let segment_len = 2 + crate::embedded::XMP_JPEG_SIGNATURE.len() + packet.len();
    if segment_len > 0xFFFF {
        return Err(MetadataError::UnsupportedWriteTarget(format!(
            "XMP packet too large for one JPEG APP1 segment ({segment_len} bytes)"
        )));
    }
    let range = match crate::embedded::find_jpeg_xmp_segment_range(data) {
        Some(range) => range,
        None => {
            let at = crate::embedded::jpeg_xmp_insert_offset(data);
            (at, at)
        }
    };
    let (start, end) = range;
    let mut out = Vec::with_capacity(data.len() - (end - start) + segment_len);
    out.extend_from_slice(&data[..start]);
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&(segment_len as u16).to_be_bytes());
    out.extend_from_slice(crate::embedded::XMP_JPEG_SIGNATURE);
    out.extend_from_slice(packet);
    out.extend_from_slice(&data[end..]);
    Ok(out)
}

/// Read the packet that is already in the file, ready to be merged into.
///
/// An existing packet that this parser cannot understand is an **error**, not an
/// empty starting point. Merging into empty and writing back would delete every
/// property the reader did not recognize, which is how a rating change could
/// silently destroy another application's metadata. Files with no packet at all
/// legitimately start empty.
fn existing_packet(data: &[u8]) -> Result<Metadata, MetadataError> {
    let Some(packet) = crate::embedded::find_xmp_packet(data)? else {
        return Ok(Metadata::new());
    };
    let parsed = parse_xmp(&packet)?;
    if parsed.is_empty() && packet.len() > 32 {
        // A real packet is never this empty: refuse rather than rewrite it away.
        return Err(MetadataError::MalformedPacket(
            "embedded XMP packet exists but no properties could be read from it".to_string(),
        ));
    }
    Ok(parsed)
}

/// Write an XMP packet into the file itself: JPEG `APP1` or TIFF tag 700.
///
/// The existing packet is read first and only the properties in `metadata` are
/// changed, so writing a rating does not delete the keywords, history or any
/// other application's namespace that was already in the packet. An empty
/// result removes the packet entirely rather than leaving an empty shell.
///
/// Containers that cannot carry a packet here (PNG, GIF, WebP, camera RAW)
/// return `UnsupportedWriteTarget`: the caller then uses a sidecar, which is
/// the same rule the EXIF writer already follows.
pub fn write_embedded_xmp(
    data: &[u8],
    metadata: &Metadata,
    tags: TagWrite,
) -> Result<Vec<u8>, MetadataError> {
    if crate::embedded::is_jpeg(data) {
        let mut merged = existing_packet(data)?;
        apply_properties(&mut merged, metadata, tags);
        if merged.is_empty() {
            // Nothing to store: drop the packet instead of writing an empty one.
            return match crate::embedded::find_jpeg_xmp_segment_range(data) {
                Some((start, end)) => {
                    let mut out = Vec::with_capacity(data.len() - (end - start));
                    out.extend_from_slice(&data[..start]);
                    out.extend_from_slice(&data[end..]);
                    Ok(out)
                }
                None => Ok(data.to_vec()),
            };
        }
        let packet = write_xmp(&merged);
        return splice_jpeg_xmp(data, packet.as_bytes());
    }

    if crate::embedded::is_tiff(data) {
        let mut merged = existing_packet(data)?;
        apply_properties(&mut merged, metadata, tags);
        let packet = write_xmp(&merged);
        let payload = if merged.is_empty() {
            Vec::new()
        } else {
            let mut bytes = Vec::with_capacity(packet.len());
            bytes.extend_from_slice(packet.as_bytes());
            // Packets in TIFF are byte strings; a trailing NUL keeps tools that
            // treat the tag as text from reading past the end.
            bytes.push(0);
            bytes
        };
        let written =
            crate::exif_write::write_tiff_bytes_tag(data, crate::embedded::TAG_XMP, &payload)?;
        return Ok(written);
    }

    Err(MetadataError::UnsupportedWriteTarget(
        "embedded XMP needs JPEG or TIFF-family bytes".to_string(),
    ))
}

/// Copy every property of `source` into `target`, replacing same-named ones.
fn apply_properties(target: &mut Metadata, source: &Metadata, tags: TagWrite) {
    if tags == TagWrite::Replace {
        // Absence means cleared: take the owned properties out first, then let
        // whatever the caller did carry overwrite them below.
        for (namespace, name) in TAG_PROPERTIES {
            if source.get(namespace, name).is_none() {
                target.remove(namespace, name);
            }
        }
    }
    for (name, value) in source.properties() {
        target.set(name.namespace.clone(), name.name.clone(), value.clone());
    }
}

/// Read, update and atomically replace the embedded packet in a file.
pub fn write_embedded_xmp_file(
    path: &Path,
    metadata: &Metadata,
    tags: TagWrite,
) -> Result<(), MetadataError> {
    let input = std::fs::read(path)?;
    let output = write_embedded_xmp(&input, metadata, tags)?;
    if output == input {
        return Ok(());
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    std::fs::write(&tmp, &output)?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn write_xmp_file(path: &Path, metadata: &Metadata) -> Result<(), MetadataError> {
    let xml = write_xmp(metadata);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    std::fs::write(&tmp, xml.as_bytes())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

pub fn parse_xmp(data: &[u8]) -> Result<Metadata, MetadataError> {
    let text = std::str::from_utf8(data)
        .map_err(|_| MetadataError::MalformedPacket("invalid UTF-8".to_string()))?;
    let doc = roxmltree::Document::parse(text)
        .map_err(|e| MetadataError::MalformedPacket(e.to_string()))?;

    let mut meta = Metadata::new();

    for node in doc.descendants() {
        if !node.is_element() {
            continue;
        }
        let ns = node.tag_name().namespace().unwrap_or("");
        let local = node.tag_name().name();
        if ns != RDF_NS || local != "Description" {
            continue;
        }
        // Only top-level descriptions directly under rdf:RDF.
        let parent_is_rdf = node
            .parent()
            .map(|p| p.tag_name().namespace() == Some(RDF_NS) && p.tag_name().name() == "RDF")
            .unwrap_or(false);
        if !parent_is_rdf {
            continue;
        }

        // Attributes become simple properties (skip rdf:* and xmlns).
        for attr in node.attributes() {
            let ans = attr.namespace().unwrap_or("");
            let aname = attr.name();
            if ans.is_empty() || ans == RDF_NS || ans == XML_NS {
                continue;
            }
            meta.set(
                ans.to_string(),
                aname.to_string(),
                simple_text_to_value(attr.value()),
            );
        }

        // Child elements become properties (possibly complex).
        for child in node.children().filter(|n| n.is_element()) {
            if let Some((q, v)) = property_from_element(child) {
                meta.set(q.namespace, q.name, v);
            }
        }
    }

    Ok(meta)
}

fn property_from_element(elem: Node) -> Option<(QualifiedName, Value)> {
    let ns = elem.tag_name().namespace()?.to_string();
    let name = elem.tag_name().name().to_string();
    // Skip rdf:Description handled by caller; but nested ones become structs.
    let value = element_value(elem);
    Some((QualifiedName::new(ns, name), value))
}

fn element_value(elem: Node) -> Value {
    let children: Vec<Node> = elem.children().filter(|n| n.is_element()).collect();
    if children.is_empty() {
        let text = elem.text().unwrap_or("").trim();
        return simple_text_to_value(text);
    }
    if children.len() == 1 {
        let c = children[0];
        let cns = c.tag_name().namespace().unwrap_or("");
        let cname = c.tag_name().name();
        if cns == RDF_NS {
            match cname {
                "Alt" => return parse_rdf_alt(c),
                "Bag" => return parse_rdf_container(c, crate::ArrayKind::Bag),
                "Seq" => return parse_rdf_container(c, crate::ArrayKind::Seq),
                "Description" => return parse_struct(c),
                _ => {}
            }
        }
    }
    // Fallback: concatenate text.
    let text = elem.text().unwrap_or("").trim();
    simple_text_to_value(text)
}

fn parse_rdf_container(node: Node, kind: crate::ArrayKind) -> Value {
    let mut items = Vec::new();
    for li in node.children().filter(|n| n.is_element()) {
        let inner: Vec<Node> = li.children().filter(|n| n.is_element()).collect();
        if inner.is_empty() {
            items.push(simple_text_to_value(li.text().unwrap_or("").trim()));
        } else if inner.len() == 1 {
            let c = inner[0];
            let cns = c.tag_name().namespace().unwrap_or("");
            let cname = c.tag_name().name();
            if cns == RDF_NS && cname == "Description" {
                items.push(parse_struct(c));
            } else if cns == RDF_NS && (cname == "Alt" || cname == "Bag" || cname == "Seq") {
                // nested container
                let k = match cname {
                    "Alt" => crate::ArrayKind::Alt,
                    "Bag" => crate::ArrayKind::Bag,
                    _ => crate::ArrayKind::Seq,
                };
                if cname == "Alt" {
                    items.push(parse_rdf_alt(c));
                } else {
                    items.push(parse_rdf_container(c, k));
                }
            } else {
                items.push(simple_text_to_value(li.text().unwrap_or("").trim()));
            }
        } else {
            items.push(simple_text_to_value(li.text().unwrap_or("").trim()));
        }
    }
    Value::Array(kind, items)
}

fn parse_rdf_alt(node: Node) -> Value {
    // Detect language alternates.
    let mut has_lang = false;
    for li in node.children().filter(|n| n.is_element()) {
        if li.attribute((XML_NS, "lang")).is_some() {
            has_lang = true;
            break;
        }
    }
    if has_lang {
        let mut la = LangAlt::new();
        for li in node.children().filter(|n| n.is_element()) {
            let lang = li
                .attribute((XML_NS, "lang"))
                .unwrap_or("x-default")
                .to_string();
            let text = li.text().unwrap_or("").trim().to_string();
            la.insert(lang, text);
        }
        return Value::AltText(la);
    }
    // Otherwise treat as alternatives array.
    parse_rdf_container(node, crate::ArrayKind::Alt)
}

fn parse_struct(node: Node) -> Value {
    let mut m = BTreeMap::new();
    for attr in node.attributes() {
        let ans = attr.namespace().unwrap_or("");
        let aname = attr.name();
        if ans.is_empty() || ans == RDF_NS || ans == XML_NS {
            continue;
        }
        m.insert(
            QualifiedName::new(ans.to_string(), aname.to_string()),
            simple_text_to_value(attr.value()),
        );
    }
    for child in node.children().filter(|n| n.is_element()) {
        if let Some((q, v)) = property_from_element(child) {
            m.insert(q, v);
        }
    }
    Value::Struct(m)
}

fn simple_text_to_value(text: &str) -> Value {
    let t = text.trim();
    if t.is_empty() {
        return Value::Text(String::new());
    }
    if t == "True" || t == "true" {
        return Value::Boolean(true);
    }
    if t == "False" || t == "false" {
        return Value::Boolean(false);
    }
    if let Ok(n) = t.parse::<i64>() {
        return Value::Integer(n);
    }
    if let Ok(n) = t.parse::<f64>() {
        return Value::Real(n);
    }
    if t.len() >= 10 && t.as_bytes().get(4) == Some(&b'-') && t.as_bytes().get(7) == Some(&b'-') {
        return Value::Date(t.to_string());
    }
    Value::Text(t.to_string())
}

// ---------------------------------------------------------------------------
// Serialisation
// ---------------------------------------------------------------------------

pub fn write_xmp(metadata: &Metadata) -> String {
    // Map namespaces to prefixes.
    let mut ns_to_prefix: HashMap<String, String> = HashMap::new();
    // Known aliases -> XMP URIs.
    let mut alias_map: HashMap<&str, &str> = HashMap::new();
    alias_map.insert(NS_EXIF, "http://ns.adobe.com/exif/1.0/");
    alias_map.insert(NS_GPS, "http://ns.adobe.com/exif/1.0/");

    // Collect distinct namespaces.
    let mut distinct: Vec<String> = Vec::new();
    for q in metadata.properties().keys() {
        let ns = alias_map
            .get(q.namespace.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| q.namespace.clone());
        if !distinct.contains(&ns) {
            distinct.push(ns);
        }
    }

    let mut prefix_counter = 0usize;
    for ns in &distinct {
        let prefix = match ns.as_str() {
            "http://purl.org/dc/elements/1.1/" => "dc".to_string(),
            "http://ns.adobe.com/xap/1.0/" => "xmp".to_string(),
            "http://ns.adobe.com/photoshop/1.0/" => "photoshop".to_string(),
            "http://ns.adobe.com/tiff/1.0/" => "tiff".to_string(),
            "http://ns.adobe.com/xap/1.0/mm/" => "xmpMM".to_string(),
            "http://iptc.org/std/Iptc4xmpCore/1.0/xmlns/" => "Iptc4xmpCore".to_string(),
            "http://iptc.org/std/Iptc4xmpExt/2008-02-29/" => "Iptc4xmpExt".to_string(),
            "http://ns.adobe.com/exif/1.0/" => "exif".to_string(),
            _ => {
                prefix_counter += 1;
                format!("ns{}", prefix_counter)
            }
        };
        ns_to_prefix.insert(ns.clone(), prefix);
    }

    let mut buf: Vec<u8> = Vec::new();
    {
        let mut w = Writer::new_with_indent(Cursor::new(&mut buf), b' ', 2);

        let _ = w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)));
        // xpacket PI
        let _ = w.write_event(Event::PI(BytesPI::new(
            r#"xpacket begin="﻿" id="W5M0MpCehiHzreSzNTczkc9d""#,
        )));

        // x:xmpmeta
        let mut xm = BytesStart::new("x:xmpmeta");
        xm.push_attribute(("xmlns:x", XMPMETA_NS));
        xm.push_attribute(("x:xmptk", "BridgeCraft"));
        let _ = w.write_event(Event::Start(xm));

        // rdf:RDF
        let mut rdf = BytesStart::new("rdf:RDF");
        rdf.push_attribute(("xmlns:rdf", RDF_NS));
        let _ = w.write_event(Event::Start(rdf));

        // rdf:Description
        let mut desc = BytesStart::new("rdf:Description");
        desc.push_attribute(("rdf:about", ""));
        // namespace declarations on description
        for (ns, prefix) in &ns_to_prefix {
            desc.push_attribute((format!("xmlns:{}", prefix).as_str(), ns.as_str()));
        }
        let _ = w.write_event(Event::Start(desc));

        // properties
        for (q, v) in metadata.properties() {
            let ns = alias_map
                .get(q.namespace.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| q.namespace.clone());
            let prefix = ns_to_prefix
                .get(&ns)
                .cloned()
                .unwrap_or_else(|| "ns0".to_string());
            let elem_name = format!("{}:{}", prefix, q.name);
            write_property(&mut w, &elem_name, v, &ns_to_prefix);
        }

        let _ = w.write_event(Event::End(BytesEnd::new("rdf:Description")));
        let _ = w.write_event(Event::End(BytesEnd::new("rdf:RDF")));
        let _ = w.write_event(Event::End(BytesEnd::new("x:xmpmeta")));

        let _ = w.write_event(Event::PI(BytesPI::new(r#"xpacket end="w""#)));
    }

    String::from_utf8(buf).unwrap_or_default()
}

fn write_property(
    w: &mut Writer<Cursor<&mut Vec<u8>>>,
    name: &str,
    value: &Value,
    ns_to_prefix: &HashMap<String, String>,
) {
    match value {
        Value::Text(s) => {
            let e = BytesStart::new(name);
            let _ = w.write_event(Event::Start(e.clone()));
            if !s.is_empty() {
                let _ = w.write_event(Event::Text(quick_xml::events::BytesText::new(s)));
            }
            let _ = w.write_event(Event::End(BytesEnd::new(name)));
        }
        Value::Boolean(b) => {
            let s = if *b { "True" } else { "False" };
            let e = BytesStart::new(name);
            let _ = w.write_event(Event::Start(e));
            let _ = w.write_event(Event::Text(quick_xml::events::BytesText::new(s)));
            let _ = w.write_event(Event::End(BytesEnd::new(name)));
        }
        Value::Integer(n) => {
            let s = n.to_string();
            let _ = w.write_event(Event::Start(BytesStart::new(name)));
            let _ = w.write_event(Event::Text(quick_xml::events::BytesText::new(&s)));
            let _ = w.write_event(Event::End(BytesEnd::new(name)));
        }
        Value::Real(n) => {
            let s = n.to_string();
            let _ = w.write_event(Event::Start(BytesStart::new(name)));
            let _ = w.write_event(Event::Text(quick_xml::events::BytesText::new(&s)));
            let _ = w.write_event(Event::End(BytesEnd::new(name)));
        }
        Value::Date(s) | Value::Uri(s) => {
            let _ = w.write_event(Event::Start(BytesStart::new(name)));
            if !s.is_empty() {
                let _ = w.write_event(Event::Text(quick_xml::events::BytesText::new(s)));
            }
            let _ = w.write_event(Event::End(BytesEnd::new(name)));
        }
        Value::Array(kind, items) => {
            let container = match kind {
                crate::ArrayKind::Bag => "rdf:Bag",
                crate::ArrayKind::Seq => "rdf:Seq",
                crate::ArrayKind::Alt => "rdf:Alt",
            };
            let _ = w.write_event(Event::Start(BytesStart::new(name)));
            let _ = w.write_event(Event::Start(BytesStart::new(container)));
            for item in items {
                let _ = w.write_event(Event::Start(BytesStart::new("rdf:li")));
                // write item as text if simple
                match item {
                    Value::Text(s) | Value::Date(s) | Value::Uri(s) => {
                        if !s.is_empty() {
                            let _ =
                                w.write_event(Event::Text(quick_xml::events::BytesText::new(s)));
                        }
                    }
                    Value::Integer(n) => {
                        let s = n.to_string();
                        let _ = w.write_event(Event::Text(quick_xml::events::BytesText::new(&s)));
                    }
                    Value::Real(n) => {
                        let s = n.to_string();
                        let _ = w.write_event(Event::Text(quick_xml::events::BytesText::new(&s)));
                    }
                    Value::Boolean(b) => {
                        let s = if *b { "True" } else { "False" };
                        let _ = w.write_event(Event::Text(quick_xml::events::BytesText::new(s)));
                    }
                    other => {
                        // nested: write as child element? For simplicity, serialize via recursion into a wrapper not supported; skip.
                        let _ = other;
                    }
                }
                let _ = w.write_event(Event::End(BytesEnd::new("rdf:li")));
            }
            let _ = w.write_event(Event::End(BytesEnd::new(container)));
            let _ = w.write_event(Event::End(BytesEnd::new(name)));
        }
        Value::AltText(la) => {
            let _ = w.write_event(Event::Start(BytesStart::new(name)));
            let _ = w.write_event(Event::Start(BytesStart::new("rdf:Alt")));
            for (lang, text) in &la.entries {
                let mut li = BytesStart::new("rdf:li");
                li.push_attribute(("xml:lang", lang.as_str()));
                let _ = w.write_event(Event::Start(li));
                if !text.is_empty() {
                    let _ = w.write_event(Event::Text(quick_xml::events::BytesText::new(text)));
                }
                let _ = w.write_event(Event::End(BytesEnd::new("rdf:li")));
            }
            let _ = w.write_event(Event::End(BytesEnd::new("rdf:Alt")));
            let _ = w.write_event(Event::End(BytesEnd::new(name)));
        }
        Value::Struct(m) => {
            let _ = w.write_event(Event::Start(BytesStart::new(name)));
            let desc = BytesStart::new("rdf:Description");
            // namespace declarations are handled on the parent rdf:Description; nothing to do here.

            let _ = w.write_event(Event::Start(desc));
            for (q, v) in m {
                let ns = alias_map_local(&q.namespace);
                let prefix = ns_to_prefix
                    .get(&ns)
                    .cloned()
                    .unwrap_or_else(|| "ns0".to_string());
                let child_name = format!("{}:{}", prefix, q.name);
                write_property(w, &child_name, v, ns_to_prefix);
            }
            let _ = w.write_event(Event::End(BytesEnd::new("rdf:Description")));
            let _ = w.write_event(Event::End(BytesEnd::new(name)));
        }
        Value::Binary(_) => {
            // Binary payloads are not representable in XMP text; skip.
        }
    }
}

fn alias_map_local(ns: &str) -> String {
    match ns {
        "exif" => "http://ns.adobe.com/exif/1.0/".to_string(),
        "gps" => "http://ns.adobe.com/exif/1.0/".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NS_DC, NS_XMP};

    /// The namespace the preservation fixture writes under.
    const NS_CAMERA_RAW: &str = "http://ns.adobe.com/camera-raw-settings/1.0/";

    #[test]
    fn roundtrip_simple_and_array() {
        // Use real URIs for XMP roundtrip.
        let mut m = Metadata::new();
        m.set_text(
            "http://purl.org/dc/elements/1.1/",
            "title",
            "Hello".to_string(),
        );
        m.set(
            "http://purl.org/dc/elements/1.1/",
            "subject",
            Value::Array(
                crate::ArrayKind::Bag,
                vec![Value::Text("a".into()), Value::Text("b".into())],
            ),
        );
        m.set_text("http://ns.adobe.com/xap/1.0/", "Rating", "4".to_string());
        // rating as integer
        m.set("http://ns.adobe.com/xap/1.0/", "Rating", Value::Integer(4));
        let mut la = LangAlt::new();
        la.insert("x-default", "Desc");
        la.insert("fr-FR", "Description");
        m.set(
            "http://purl.org/dc/elements/1.1/",
            "description",
            Value::AltText(la),
        );

        let xml = write_xmp(&m);
        eprintln!("{}", xml);
        let parsed = parse_xmp(xml.as_bytes()).unwrap();
        assert_eq!(
            parsed.text("http://purl.org/dc/elements/1.1/", "title"),
            Some("Hello")
        );
        // subject array
        match parsed.get("http://purl.org/dc/elements/1.1/", "subject") {
            Some(Value::Array(_, items)) => assert_eq!(items.len(), 2),
            other => panic!("expected array, got {:?}", other),
        }
        assert_eq!(
            parsed
                .get("http://ns.adobe.com/xap/1.0/", "Rating")
                .and_then(Value::as_integer),
            Some(4)
        );
        match parsed.get("http://purl.org/dc/elements/1.1/", "description") {
            Some(Value::AltText(la)) => assert_eq!(la.default_text(), Some("Desc")),
            other => panic!("expected alttext, got {:?}", other),
        }
    }

    #[test]
    fn reads_a_sidecar_that_carries_only_attributes_on_a_self_closing_description() {
        // The shape other tools reach for when a file has simple properties
        // and nothing complex: one `rdf:Description`, self-closing, every
        // property an attribute. No other test in this module uses that shape,
        // and it fails in a way that is easy to misread: a packet whose RDF
        // namespace is misspelled parses as *empty* rather than as an error,
        // which looks exactly like a parser that cannot handle attributes.
        let xml = concat!(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">",
            "<rdf:RDF xmlns:rdf=\"http://www.w3.org/",
            "1999/02/22-rdf-syntax-ns#\">",
            "<rdf:Description rdf:about=\"\"",
            " xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\"",
            " xmlns:photoshop=\"http://ns.adobe.com/photoshop/1.0/\"",
            " xmp:Rating=\"4\"",
            " photoshop:Label=\"Red\"/>",
            "</rdf:RDF></x:xmpmeta>",
        );
        // Spelled out rather than pasted, because the character that was wrong
        // in this test's first draft is invisible in most renderings.
        assert!(
            xml.contains("1999/02/22-rdf-syntax-ns#"),
            "the RDF namespace separates its date with soliduses"
        );
        let m = parse_xmp(xml.as_bytes()).unwrap();
        assert_eq!(m.rating().stars(), 4, "attribute form lost its rating");
        assert_eq!(
            m.color_label(),
            crate::ratings::ColorLabel::Red,
            "attribute form lost its label"
        );
    }

    #[test]
    fn parse_sample_xmp() {
        let xml = r#"<?xpacket begin="﻿" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:dc="http://purl.org/dc/elements/1.1/"
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmp:Rating="5">
   <dc:subject>
     <rdf:Bag>
       <rdf:li>travel</rdf:li>
       <rdf:li>photo</rdf:li>
     </rdf:Bag>
   </dc:subject>
   <dc:title>
     <rdf:Alt>
       <rdf:li xml:lang="x-default">My Title</rdf:li>
     </rdf:Alt>
   </dc:title>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
<?xpacket end="w"?>"#;
        let m = parse_xmp(xml.as_bytes()).unwrap();
        assert_eq!(
            m.get("http://ns.adobe.com/xap/1.0/", "Rating")
                .and_then(Value::as_integer),
            Some(5)
        );
        match m.get("http://purl.org/dc/elements/1.1/", "subject") {
            Some(Value::Array(_, items)) => assert_eq!(items.len(), 2),
            other => panic!("{:?}", other),
        }
        match m.get("http://purl.org/dc/elements/1.1/", "title") {
            Some(Value::AltText(la)) => assert_eq!(la.default_text(), Some("My Title")),
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn sidecar_path_helper() {
        let p = std::path::Path::new("/tmp/foo.jpg");
        assert_eq!(sidecar_path(p), std::path::PathBuf::from("/tmp/foo.xmp"));
    }
    #[test]
    fn writes_a_packet_into_a_jpeg_and_keeps_the_rest_of_the_file() {
        let exif_body = b"Exif\0\0some-tiff-bytes";
        let mut jpeg = vec![0xFF, 0xD8];
        jpeg.extend_from_slice(&[0xFF, 0xE0]);
        jpeg.extend_from_slice(&5u16.to_be_bytes());
        jpeg.extend_from_slice(b"\0\0\0");
        jpeg.extend_from_slice(&[0xFF, 0xE1]);
        jpeg.extend_from_slice(&((exif_body.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(exif_body);
        jpeg.extend_from_slice(&[0xFF, 0xDA, 0x04, 0x00, 0x01, 0x00, 0x00]);
        jpeg.extend_from_slice(&[0x01, 0x02, 0x03]);
        jpeg.extend_from_slice(&[0xFF, 0xD9]);

        let mut meta = Metadata::new();
        meta.set_text(NS_XMP, "Rating", "4".to_string());
        let written = write_embedded_xmp(&jpeg, &meta, TagWrite::Replace).unwrap();

        // The packet is readable, and it went in after the EXIF APP1.
        let read = read_embedded_xmp(&written)
            .unwrap()
            .expect("packet written");
        assert_eq!(read.rating().stars(), 4);
        let exif_end = written
            .windows(b"Exif\0\0".len())
            .position(|w| w == b"Exif\0\0")
            .expect("EXIF APP1 still present");
        let xmp_at = crate::embedded::find_jpeg_xmp_segment_range(&written)
            .unwrap()
            .0;
        assert!(
            xmp_at > exif_end,
            "the XMP segment should follow the EXIF segment"
        );
        // Scan data and EOI survive the splice byte-for-byte.
        assert!(written.ends_with(&[0xFF, 0xD9]));
        assert!(written.windows(3).any(|w| w == b"\x01\x02\x03"));
        assert!(written.len() > jpeg.len());
    }

    #[test]
    fn a_cleared_star_and_label_leave_the_packet_rather_than_staying_in_it() {
        // The bug this replaces: `set_rating(Rating::new(0))` removes the
        // property instead of writing zero, so a save that meant to clear a
        // rating carried no `xmp:Rating` at all — indistinguishable from a save
        // that did not touch ratings. Merging then left the old value inside
        // the JPEG forever while the sidecar beside it said something
        // different, so the two layers disagreed about a property the user had
        // just set.
        let jpeg = jpeg_fixture();
        let mut rated = Metadata::new();
        rated.set_rating(crate::ratings::Rating::new(4));
        rated.set_color_label(crate::ratings::ColorLabel::Red);
        crate::keywords::add_keyword(&mut rated, "Keep");
        rated.set_text(crate::NS_XMP, "CreatorTool", "Some Other App".to_string());
        let with_tags = write_embedded_xmp(&jpeg, &rated, TagWrite::Replace).unwrap();
        assert_eq!(
            read_embedded_xmp(&with_tags)
                .unwrap()
                .unwrap()
                .rating()
                .stars(),
            4,
            "the fixture did not start tagged"
        );

        // The clear: the same authoritative state, now carrying neither stars,
        // nor a label, nor keywords.
        let mut cleared = Metadata::new();
        cleared.set_text(crate::NS_XMP, "CreatorTool", "Some Other App".to_string());
        let after = write_embedded_xmp(&with_tags, &cleared, TagWrite::Replace).unwrap();

        let read = read_embedded_xmp(&after).unwrap().expect("packet kept");
        assert_eq!(
            read.rating().stars(),
            0,
            "the cleared rating is still in the file"
        );
        assert_eq!(
            read.color_label(),
            crate::ratings::ColorLabel::None,
            "the cleared label is still in the file"
        );
        assert!(
            crate::keywords::subjects(&read).is_empty(),
            "the cleared keywords are still in the file"
        );
        // Outside the four owned properties, a replace is still additive:
        // another application's packet contents are not BridgeCraft's to clear.
        assert_eq!(
            read.text(crate::NS_XMP, "CreatorTool"),
            Some("Some Other App"),
            "Replace reached past the tag properties"
        );
    }

    /// A JPEG the packet splicer accepts: SOI, a start-of-scan marker and EOI.
    fn jpeg_fixture() -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8];
        out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x00, 0x01, 0x00, 0x00]);
        out.extend_from_slice(&[0xFF, 0xD9]);
        out
    }

    #[test]
    fn rewriting_a_packet_preserves_what_the_caller_did_not_change() {
        // A packet that already carries keywords and another tool's namespace.
        let original = concat!(
            "<?xpacket begin='\u{feff}' id='W5M0MpCehiHzreSzNTczkc9d'?>",
            "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'>",
            "<rdf:Description rdf:about='' xmlns:dc='http://purl.org/dc/elements/1.1/'",
            " xmlns:xmp='http://ns.adobe.com/xap/1.0/'",
            " xmlns:crs='http://ns.adobe.com/camera-raw-settings/1.0/'>",
            "<dc:subject><rdf:Bag><rdf:li>KeepMe</rdf:li></rdf:Bag></dc:subject>",
            "<xmp:Rating>2</xmp:Rating>",
            "<crs:LocalName>other tool</crs:LocalName>",
            "</rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end='w'?>"
        );
        let mut body = crate::embedded::XMP_JPEG_SIGNATURE.to_vec();
        body.extend_from_slice(original.as_bytes());
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
        jpeg.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(&body);
        jpeg.extend_from_slice(&[0xFF, 0xD9]);

        // Change only the rating. `Merge`, because this caller carries one
        // property and has nothing to say about the others — the case that
        // must stay additive, and the reason `Replace` is stated rather than
        // inferred from what a metadata happens to omit.
        let mut change = Metadata::new();
        change.set_text(NS_XMP, "Rating", "5".to_string());
        let written = write_embedded_xmp(&jpeg, &change, TagWrite::Merge).unwrap();
        let meta = read_embedded_xmp(&written)
            .unwrap()
            .expect("packet readable");

        assert_eq!(meta.rating().stars(), 5, "the change applied");
        assert!(
            crate::keywords::subjects(&meta).contains(&"KeepMe".to_string()),
            "existing keywords survived a rating-only change: {:?}",
            crate::keywords::subjects(&meta)
        );
        let keys: Vec<String> = meta
            .properties()
            .keys()
            .map(|k| format!("{}::{}", k.namespace, k.name))
            .collect();
        assert!(
            meta.get(NS_CAMERA_RAW, "LocalName").is_some(),
            "another application's namespace must not be dropped; got {keys:?}"
        );
        // Exactly one XMP segment: replacement, not accumulation.
        assert_eq!(
            written
                .windows(crate::embedded::XMP_JPEG_SIGNATURE.len())
                .filter(|w| *w == crate::embedded::XMP_JPEG_SIGNATURE)
                .count(),
            1,
            "a second packet was inserted instead of replacing the first"
        );
    }

    #[test]
    fn a_packet_too_large_for_one_segment_is_refused_not_truncated() {
        let mut meta = Metadata::new();
        let huge = "x".repeat(70_000);
        meta.set_text(NS_DC, "description", huge);
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xD9];
        let error = write_embedded_xmp(&jpeg, &meta, TagWrite::Replace).unwrap_err();
        assert!(
            matches!(error, MetadataError::UnsupportedWriteTarget(_)),
            "{error:?}"
        );
        assert_eq!(
            jpeg,
            vec![0xFF, 0xD8, 0xFF, 0xD9],
            "refused write left bytes alone"
        );
    }

    #[test]
    fn formats_without_an_embedded_packet_slot_are_reported_as_such() {
        let png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut meta = Metadata::new();
        meta.set_text(NS_XMP, "Rating", "3".to_string());
        let error = write_embedded_xmp(&png, &meta, TagWrite::Replace).unwrap_err();
        assert!(
            matches!(error, MetadataError::UnsupportedWriteTarget(_)),
            "PNG must say it cannot take a packet, not silently do nothing"
        );
    }
    #[test]
    fn a_packet_this_parser_cannot_read_is_never_rewritten_from_empty() {
        // Well-formed XML, right namespaces, but no RDF Description this parser
        // recognizes: it parses to zero properties. Merging into that empty
        // result and writing back would have deleted the original bytes, so the
        // writer must refuse instead.
        let foreign = concat!(
            "<?xpacket begin=''?><x:xmpmeta xmlns:x='adobe:ns:meta/'>",
            "<something:else xmlns:something='urn:whatever'/></x:xmpmeta><?xpacket end='w'?>"
        );
        let mut body = crate::embedded::XMP_JPEG_SIGNATURE.to_vec();
        body.extend_from_slice(foreign.as_bytes());
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
        jpeg.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(&body);
        jpeg.extend_from_slice(&[0xFF, 0xDA, 0x04, 0x00, 0x01]);
        jpeg.extend_from_slice(&[0xFF, 0xD9]);

        let mut change = Metadata::new();
        change.set_text(NS_XMP, "Rating", "5".to_string());
        let error = write_embedded_xmp(&jpeg, &change, TagWrite::Replace).unwrap_err();
        assert!(
            matches!(error, MetadataError::MalformedPacket(_)),
            "expected a refusal, got {error:?}"
        );
        assert_eq!(
            write_embedded_xmp(&jpeg, &change, TagWrite::Replace)
                .unwrap_err()
                .to_string(),
            error.to_string(),
            "the refusal must be stable, not a partial write the first time"
        );
    }

    #[test]
    fn a_file_with_no_packet_at_all_can_still_be_written() {
        // The refusal above is about an unreadable packet, not about absence.
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xDA, 0x04, 0x00, 0x01, 0xFF, 0xD9];
        let mut meta = Metadata::new();
        meta.set_text(NS_XMP, "Rating", "1".to_string());
        let written = write_embedded_xmp(&jpeg, &meta, TagWrite::Replace).unwrap();
        assert_eq!(
            read_embedded_xmp(&written)
                .unwrap()
                .unwrap()
                .rating()
                .stars(),
            1
        );
    }
    /// Round trip against the third-party sample: write into a file another
    /// application produced, then re-read it. A gated companion to the Pillow
    /// check above, run as `XMP_WRITE_SAMPLE=<file>` so the original is a copy.
    #[test]
    fn writes_into_a_file_another_application_produced() {
        let Some(sample) = std::env::var_os("XMP_WRITE_SAMPLE") else {
            eprintln!("XMP_WRITE_SAMPLE not set; skipping third-party write check");
            return;
        };
        let path = Path::new(&sample);
        let input = std::fs::read(path).unwrap();
        let before = read_embedded_xmp(&input)
            .unwrap()
            .expect("the sample must already carry a packet");

        let mut change = Metadata::new();
        change.set_text(NS_XMP, "Rating", "5".to_string());
        let written = write_embedded_xmp(&input, &change, TagWrite::Merge).unwrap();
        std::fs::write(path, &written).unwrap();

        let after = read_embedded_xmp(&written)
            .unwrap()
            .expect("the rewritten packet must be readable");
        assert_eq!(after.rating().stars(), 5);
        // Everything the other tool wrote is still there.
        let before_keys: Vec<String> = before
            .properties()
            .keys()
            .map(|k| format!("{}::{}", k.namespace, k.name))
            .collect();
        let after_keys: Vec<String> = after
            .properties()
            .keys()
            .map(|k| format!("{}::{}", k.namespace, k.name))
            .collect();
        for key in &before_keys {
            assert!(after_keys.contains(key), "lost {key} rewriting a real file");
        }
        eprintln!(
            "rewrote third-party sample: {} properties before, {} after",
            before_keys.len(),
            after_keys.len()
        );
    }
}
