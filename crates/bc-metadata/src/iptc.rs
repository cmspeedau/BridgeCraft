//! IPTC-IIM read and write (issue #8).
//!
//! IIM datasets ride inside the Photoshop `8BIM` resource block (resource id
//! `0x021C`) of JPEG/TIFF files. This module parses the 8BIM framing (mirroring
//! `bc-thumbnails::psd`) to locate that resource, decodes the record-2 datasets,
//! and maps them onto the shared [`Metadata`] model. Writing rebuilds only the
//! `0x021C` resource, preserving every other 8BIM resource verbatim.
//!
//! JPEG (APP13 Photoshop 3.0) containers are supported. TIFF containers are
//! deferred (see roadmap).
//!
//! Clean-room: IPTC IIM4 specification + publicly documented Photoshop resource
//! framing.

use std::path::Path;

use crate::{Metadata, MetadataError, Value};

const IPTC_RESOURCE_ID: u16 = 0x021C;
const IPTC_NS: &str = "iptc";

pub struct IptcReader;
impl crate::Reader for IptcReader {
    fn read(&self, data: &[u8]) -> Result<Metadata, MetadataError> {
        read_iptc_jpeg(data)
    }
}

pub struct IptcWriter;
impl crate::Writer for IptcWriter {
    fn write(&self, data: &[u8], metadata: &Metadata) -> Result<Vec<u8>, MetadataError> {
        write_iptc_jpeg(data, metadata)
    }
}

// ---------------------------------------------------------------------------
// 8BIM resource framing (mirrors bc-thumbnails/src/psd.rs)
// ---------------------------------------------------------------------------

struct Resource {
    id: u16,
    /// Raw chunk bytes including header + data + padding (verbatim).
    raw: Vec<u8>,
}

fn parse_resources(bytes: &[u8]) -> Result<Vec<Resource>, MetadataError> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        if pos + 7 > bytes.len() {
            return Err(MetadataError::Malformed("truncated 8BIM header".into()));
        }
        if &bytes[pos..pos + 4] != b"8BIM" {
            return Err(MetadataError::Malformed("bad 8BIM signature".into()));
        }
        let id = u16::from_be_bytes([bytes[pos + 4], bytes[pos + 5]]);
        let name_len = bytes[pos + 6] as usize;
        let padded_name_len = 1usize
            .checked_add(name_len)
            .and_then(|n| n.checked_add((1 + name_len) % 2))
            .ok_or_else(|| MetadataError::Malformed("name length overflow".into()))?;
        let len_offset = pos + 6 + padded_name_len;
        if len_offset + 4 > bytes.len() {
            return Err(MetadataError::Malformed("truncated resource length".into()));
        }
        let data_len = u32::from_be_bytes([
            bytes[len_offset],
            bytes[len_offset + 1],
            bytes[len_offset + 2],
            bytes[len_offset + 3],
        ]) as usize;
        let data_start = len_offset + 4;
        let data_end = data_start
            .checked_add(data_len)
            .ok_or_else(|| MetadataError::Malformed("data length overflow".into()))?;
        let padded_end = data_end
            .checked_add(data_len % 2)
            .ok_or_else(|| MetadataError::Malformed("padding overflow".into()))?;
        if padded_end > bytes.len() {
            return Err(MetadataError::Malformed(
                "truncated resource payload".into(),
            ));
        }
        let chunk_end = padded_end;
        out.push(Resource {
            id,
            raw: bytes[pos..chunk_end].to_vec(),
        });
        pos = chunk_end;
    }
    Ok(out)
}

fn rebuild_resources(resources: &[Resource], new_iptc: Option<Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut replaced = false;
    for r in resources {
        if r.id == IPTC_RESOURCE_ID {
            if let Some(data) = &new_iptc {
                // Rebuild a single 8BIM resource with id 0x021C.
                out.extend_from_slice(b"8BIM");
                out.extend_from_slice(&IPTC_RESOURCE_ID.to_be_bytes());
                out.push(0); // empty pascal name
                             // pad name to even
                out.push(0);
                let len = data.len() as u32;
                out.extend_from_slice(&len.to_be_bytes());
                out.extend_from_slice(data);
                if !data.len().is_multiple_of(2) {
                    out.push(0);
                }
                replaced = true;
                continue;
            }
        }
        out.extend_from_slice(&r.raw);
    }
    if !replaced {
        if let Some(data) = &new_iptc {
            out.extend_from_slice(b"8BIM");
            out.extend_from_slice(&IPTC_RESOURCE_ID.to_be_bytes());
            out.push(0);
            out.push(0);
            let len = data.len() as u32;
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(data);
            if !data.len().is_multiple_of(2) {
                out.push(0);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// JPEG APP13 container
// ---------------------------------------------------------------------------

const PS_SIGNATURE: &[u8] = b"Photoshop 3.0\0";

fn find_photoshop_resources(data: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    // Returns (segment_start, segment_end, resources_bytes) for the first
    // APP13 segment carrying the Photoshop 3.0 signature.
    let mut pos = 2usize; // skip SOI
    while pos + 4 <= data.len() {
        if data[pos] != 0xFF {
            return None;
        }
        let marker = data[pos + 1];
        if marker == 0xDA || marker == 0xD9 {
            return None;
        }
        if marker == 0xFF {
            pos += 1;
            continue;
        }
        // stand-alone markers
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            pos += 2;
            continue;
        }
        if pos + 4 > data.len() {
            return None;
        }
        let seg_len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        let seg_start = pos;
        let seg_end = pos + 2 + seg_len;
        if seg_end > data.len() {
            return None;
        }
        if marker == 0xED {
            // APP13
            let body = &data[pos + 4..seg_end];
            if body.starts_with(PS_SIGNATURE) {
                let res_bytes = body[PS_SIGNATURE.len()..].to_vec();
                return Some((seg_start, seg_end, res_bytes));
            }
        }
        pos = seg_end;
    }
    None
}

pub fn read_iptc_jpeg(data: &[u8]) -> Result<Metadata, MetadataError> {
    let Some((_s, _e, res_bytes)) = find_photoshop_resources(data) else {
        return Ok(Metadata::new());
    };
    let resources = parse_resources(&res_bytes)?;
    let mut iptc_data = None;
    for r in &resources {
        if r.id == IPTC_RESOURCE_ID {
            // resource data begins after the 4-byte length in raw; extract from raw
            // raw layout: 8BIM(4) id(2) name... length(4) data...
            // Find data offset.
            iptc_data = Some(extract_resource_data(&r.raw));
        }
    }
    let Some(iim) = iptc_data else {
        return Ok(Metadata::new());
    };
    parse_iim(&iim)
}

fn extract_resource_data(raw: &[u8]) -> Vec<u8> {
    // mirror parse_resources offset logic
    if raw.len() < 7 {
        return Vec::new();
    }
    let name_len = raw[6] as usize;
    let padded_name_len = 1 + name_len + ((1 + name_len) % 2);
    let len_offset = 6 + padded_name_len;
    if len_offset + 4 > raw.len() {
        return Vec::new();
    }
    let data_len = u32::from_be_bytes([
        raw[len_offset],
        raw[len_offset + 1],
        raw[len_offset + 2],
        raw[len_offset + 3],
    ]) as usize;
    let data_start = len_offset + 4;
    let data_end = data_start + data_len;
    if data_end > raw.len() {
        return Vec::new();
    }
    raw[data_start..data_end].to_vec()
}

pub fn write_iptc_jpeg(data: &[u8], metadata: &Metadata) -> Result<Vec<u8>, MetadataError> {
    let new_iim = write_iim(metadata);
    let Some((seg_start, seg_end, res_bytes)) = find_photoshop_resources(data) else {
        // No existing APP13: append one before EOI.
        // Build resources with just iptc.
        let mut resources_bytes = Vec::new();
        // minimal: just the iptc resource
        resources_bytes.extend_from_slice(b"8BIM");
        resources_bytes.extend_from_slice(&IPTC_RESOURCE_ID.to_be_bytes());
        resources_bytes.push(0);
        resources_bytes.push(0);
        let len = new_iim.len() as u32;
        resources_bytes.extend_from_slice(&len.to_be_bytes());
        resources_bytes.extend_from_slice(&new_iim);
        if !new_iim.len().is_multiple_of(2) {
            resources_bytes.push(0);
        }
        // Build APP13 segment
        let mut body = Vec::new();
        body.extend_from_slice(PS_SIGNATURE);
        body.extend_from_slice(&resources_bytes);
        // Find EOI
        let mut out = data.to_vec();
        // insert before last 2 bytes (EOI) if JPEG
        if out.len() >= 2 && out[out.len() - 2] == 0xFF && out[out.len() - 1] == 0xD9 {
            let eoi = out.split_off(out.len() - 2);
            // segment
            let seg_len = (body.len() + 2) as u16;
            out.push(0xFF);
            out.push(0xED);
            out.extend_from_slice(&seg_len.to_be_bytes());
            out.extend_from_slice(&body);
            out.extend_from_slice(&eoi);
        } else {
            return Err(MetadataError::Malformed("not a JPEG".into()));
        }
        return Ok(out);
    };

    let resources = parse_resources(&res_bytes)?;
    let new_res_bytes = rebuild_resources(&resources, Some(new_iim));

    let mut body = Vec::new();
    body.extend_from_slice(PS_SIGNATURE);
    body.extend_from_slice(&new_res_bytes);

    let mut out = Vec::new();
    out.extend_from_slice(&data[..seg_start]);
    let seg_len = (body.len() + 2) as u16;
    out.push(0xFF);
    out.push(0xED);
    out.extend_from_slice(&seg_len.to_be_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(&data[seg_end..]);
    Ok(out)
}

// ---------------------------------------------------------------------------
// IIM parse / write
// ---------------------------------------------------------------------------

fn decode_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

fn is_utf8_escape(data: &[u8]) -> bool {
    // 1:90 payload 0x1B 0x25 0x47
    data == [0x1B, 0x25, 0x47]
}

pub fn parse_iim(data: &[u8]) -> Result<Metadata, MetadataError> {
    // Determine charset.
    let mut utf8 = false;
    let mut pos = 0usize;
    while pos + 5 <= data.len() {
        if data[pos] != 0x1C {
            pos += 1;
            continue;
        }
        let record = data[pos + 1];
        let dataset = data[pos + 2];
        let size = u16::from_be_bytes([data[pos + 3], data[pos + 4]]) as usize;
        if record == 1 && dataset == 90 {
            let payload = if pos + 5 + size <= data.len() {
                &data[pos + 5..pos + 5 + size]
            } else {
                &[]
            };
            if is_utf8_escape(payload) {
                utf8 = true;
            }
        }
        pos += 5 + size;
    }

    let mut meta = Metadata::new();
    let mut keywords: Vec<String> = Vec::new();
    let mut creators: Vec<String> = Vec::new();

    pos = 0;
    while pos < data.len() {
        if data[pos] != 0x1C {
            pos += 1;
            continue;
        }
        if pos + 5 > data.len() {
            break;
        }
        let record = data[pos + 1];
        let dataset = data[pos + 2];
        let size = u16::from_be_bytes([data[pos + 3], data[pos + 4]]) as usize;
        if pos + 5 + size > data.len() {
            break;
        }
        let payload = &data[pos + 5..pos + 5 + size];
        let text = if utf8 {
            String::from_utf8_lossy(payload).to_string()
        } else {
            decode_latin1(payload)
        };

        if record == 2 {
            match dataset {
                0 => {
                    meta.set_text(IPTC_NS, "ObjectName", text);
                }
                5 => {
                    if !text.is_empty() {
                        keywords.push(text);
                    }
                }
                10 | 12 | 15 => {
                    if !text.is_empty() {
                        creators.push(text);
                    }
                }
                22 => {
                    if !text.is_empty() {
                        meta.set_text(IPTC_NS, "Caption", text.clone());
                        // map to dc:description as AltText
                        let mut la = crate::LangAlt::new();
                        la.insert("x-default", text);
                        meta.set(
                            "http://purl.org/dc/elements/1.1/",
                            "description",
                            Value::AltText(la),
                        );
                    }
                }
                25 => {
                    if !text.is_empty() {
                        meta.set_text(IPTC_NS, "SpecialInstructions", text);
                    }
                }
                55 => {
                    if !text.is_empty() {
                        meta.set_text(IPTC_NS, "CopyrightNotice", text.clone());
                        meta.set_text("http://purl.org/dc/elements/1.1/", "rights", text);
                    }
                }
                80 => {
                    if !text.is_empty() {
                        meta.set_text(IPTC_NS, "BylineTitle", text);
                    }
                }
                105 => {
                    if !text.is_empty() {
                        meta.set_text(IPTC_NS, "Headline", text);
                    }
                }
                116 => {
                    if !text.is_empty() {
                        meta.set_text(IPTC_NS, "Credit", text);
                    }
                }
                _ => {
                    if !text.is_empty() {
                        meta.set_text(IPTC_NS, format!("Dataset{}", dataset), text);
                    }
                }
            }
        }
        pos += 5 + size;
    }

    if !keywords.is_empty() {
        meta.set_text(IPTC_NS, "Keywords", keywords.join(", "));
        let arr = Value::Array(
            crate::ArrayKind::Bag,
            keywords.into_iter().map(Value::Text).collect(),
        );
        meta.set("http://purl.org/dc/elements/1.1/", "subject", arr);
    }
    if !creators.is_empty() {
        meta.set_text(IPTC_NS, "Byline", creators.join(", "));
        let arr = Value::Array(
            crate::ArrayKind::Bag,
            creators.into_iter().map(Value::Text).collect(),
        );
        meta.set("http://purl.org/dc/elements/1.1/", "creator", arr);
    }

    Ok(meta)
}

pub fn write_iim(metadata: &Metadata) -> Vec<u8> {
    let mut out = Vec::new();
    // charset UTF-8
    write_dataset(&mut out, 1, 90, &[0x1B, 0x25, 0x47]);

    if let Some(name) = metadata.text(IPTC_NS, "ObjectName") {
        write_dataset(&mut out, 2, 0, name.as_bytes());
    }
    // keywords
    if let Some(Value::Array(_, items)) =
        metadata.get("http://purl.org/dc/elements/1.1/", "subject")
    {
        for item in items {
            if let Value::Text(kw) = item {
                write_dataset(&mut out, 2, 5, kw.as_bytes());
            }
        }
    } else if let Some(kw) = metadata.text(IPTC_NS, "Keywords") {
        for part in kw.split(',') {
            let t = part.trim();
            if !t.is_empty() {
                write_dataset(&mut out, 2, 5, t.as_bytes());
            }
        }
    }
    // creators
    if let Some(Value::Array(_, items)) =
        metadata.get("http://purl.org/dc/elements/1.1/", "creator")
    {
        for item in items {
            if let Value::Text(c) = item {
                write_dataset(&mut out, 2, 10, c.as_bytes());
            }
        }
    } else if let Some(c) = metadata.text(IPTC_NS, "Byline") {
        for part in c.split(',') {
            let t = part.trim();
            if !t.is_empty() {
                write_dataset(&mut out, 2, 10, t.as_bytes());
            }
        }
    }
    if let Some(cap) = metadata.text(IPTC_NS, "Caption") {
        write_dataset(&mut out, 2, 22, cap.as_bytes());
    }
    if let Some(si) = metadata.text(IPTC_NS, "SpecialInstructions") {
        write_dataset(&mut out, 2, 25, si.as_bytes());
    }
    if let Some(cn) = metadata.text(IPTC_NS, "CopyrightNotice") {
        write_dataset(&mut out, 2, 55, cn.as_bytes());
    }
    if let Some(bt) = metadata.text(IPTC_NS, "BylineTitle") {
        write_dataset(&mut out, 2, 80, bt.as_bytes());
    }
    if let Some(h) = metadata.text(IPTC_NS, "Headline") {
        write_dataset(&mut out, 2, 105, h.as_bytes());
    }
    if let Some(cr) = metadata.text(IPTC_NS, "Credit") {
        write_dataset(&mut out, 2, 116, cr.as_bytes());
    }
    // object name
    if let Some(on) = metadata.text(IPTC_NS, "ObjectName") {
        write_dataset(&mut out, 2, 0, on.as_bytes());
    }
    out
}

fn write_dataset(out: &mut Vec<u8>, record: u8, dataset: u8, payload: &[u8]) {
    out.push(0x1C);
    out.push(record);
    out.push(dataset);
    let size = payload.len() as u16;
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(payload);
}

// File helpers
pub fn read_iptc_file(path: &Path) -> Result<Metadata, MetadataError> {
    let bytes = std::fs::read(path)?;
    // Assume JPEG; otherwise unsupported.
    read_iptc_jpeg(&bytes)
}

pub fn write_iptc_file(path: &Path, metadata: &Metadata) -> Result<(), MetadataError> {
    let bytes = std::fs::read(path)?;
    let out = write_iptc_jpeg(&bytes, metadata)?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    std::fs::write(&tmp, &out)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_iim() -> Vec<u8> {
        let mut m = Metadata::new();
        m.set_text(IPTC_NS, "ObjectName", "Obj".to_string());
        m.set_text(IPTC_NS, "Caption", "A caption".to_string());
        m.set_text(IPTC_NS, "Keywords", "a, b".to_string());
        write_iim(&m)
    }

    #[test]
    fn parse_and_write_iim_roundtrip() {
        let data = build_iim();
        let parsed = parse_iim(&data).unwrap();
        assert_eq!(parsed.text(IPTC_NS, "ObjectName"), Some("Obj"));
        assert_eq!(parsed.text(IPTC_NS, "Caption"), Some("A caption"));
        // keywords split
        match parsed.get("http://purl.org/dc/elements/1.1/", "subject") {
            Some(Value::Array(_, items)) => assert_eq!(items.len(), 2),
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn parse_truncated_does_not_panic() {
        let data = build_iim();
        for cut in 0..data.len() {
            let _ = parse_iim(&data[..cut]);
        }
    }

    #[test]
    fn unknown_dataset_stored() {
        // Build a dataset 2:99
        let mut data = Vec::new();
        data.push(0x1C);
        data.push(2);
        data.push(99);
        let payload = b"val";
        data.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        data.extend_from_slice(payload);
        let m = parse_iim(&data).unwrap();
        assert_eq!(m.text(IPTC_NS, "Dataset99"), Some("val"));
    }

    #[test]
    fn charset_utf8() {
        // 1:90 ESC % G then 2:120? use caption 2:22 with UTF-8 text
        let mut data = Vec::new();
        // charset
        data.push(0x1C);
        data.push(1);
        data.push(90);
        data.extend_from_slice(&3u16.to_be_bytes());
        data.extend_from_slice(&[0x1B, 0x25, 0x47]);
        // caption with multibyte
        let text: &[u8] = &[0x63, 0x61, 0x66, 0xc3, 0xa9];
        data.push(0x1C);
        data.push(2);
        data.push(22);
        data.extend_from_slice(&(text.len() as u16).to_be_bytes());
        data.extend_from_slice(text);
        let m = parse_iim(&data).unwrap();
        assert_eq!(m.text(IPTC_NS, "Caption"), Some("café"));
    }

    #[test]
    fn jpeg_roundtrip_preserves_other_resources() {
        // Build a minimal JPEG with an APP13 containing a dummy non-IPTC resource.
        let mut jpeg = vec![0xFF, 0xD8];
        // APP13 with photoshop sig + one resource id 0x03E8 (resolution) verbatim
        let mut body = Vec::new();
        body.extend_from_slice(PS_SIGNATURE);
        // resource 0x03E8 with data [1,2,3]
        body.extend_from_slice(b"8BIM");
        body.extend_from_slice(&0x03E8u16.to_be_bytes());
        body.push(0);
        body.push(0);
        body.extend_from_slice(&3u32.to_be_bytes());
        body.extend_from_slice(&[1, 2, 3]);
        body.push(0); // pad
        let seg_len = (body.len() + 2) as u16;
        jpeg.push(0xFF);
        jpeg.push(0xED);
        jpeg.extend_from_slice(&seg_len.to_be_bytes());
        jpeg.extend_from_slice(&body);
        jpeg.extend_from_slice(&[0xFF, 0xD9]);

        // write iptc
        let mut meta = Metadata::new();
        meta.set_text(IPTC_NS, "Caption", "hi".to_string());
        let out = write_iptc_jpeg(&jpeg, &meta).unwrap();

        // parse back
        let parsed = read_iptc_jpeg(&out).unwrap();
        assert_eq!(parsed.text(IPTC_NS, "Caption"), Some("hi"));

        // other resource preserved
        assert!(out.windows(4).any(|w| w == b"8BIM"));
        // ensure the dummy resource id still present
        let (_, _, res) = find_photoshop_resources(&out).unwrap();
        let resources = parse_resources(&res).unwrap();
        assert!(resources.iter().any(|r| r.id == 0x03E8));
        assert!(resources.iter().any(|r| r.id == IPTC_RESOURCE_ID));
    }
}
