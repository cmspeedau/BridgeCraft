//! EXIF writing for JPEG (`APP1`) and TIFF-family containers (issue #7).
//!
//! The writer rebuilds only what it must:
//!
//! - **JPEG**: replaces (or inserts) the `APP1` `Exif\0\0` segment and copies
//!   every other marker segment byte-for-byte. The embedded TIFF tree is
//!   re-serialised compactly; embedded thumbnails are relocated and their
//!   offset tags updated.
//! - **TIFF**: appends a freshly serialised IFD tree to the end of the file
//!   and repoints the header's IFD0 offset, so image strips/tiles and any
//!   unparsed data stay at their original offsets untouched.
//!
//! Only fields that actually differ from what the reader reports are written,
//! so `read` → `write` with no edits returns the input bytes unchanged.
//!
//! Clean-room: TIFF 6.0, CIPA DC-X004 (EXIF) and the JPEG marker framing in
//! ITU-T T.81. No Adobe or other proprietary code.
//!
//! Atomic replacement (`write_exif_file`) writes a sibling temp file and
//! renames it over the original, preserving the original permissions.

use std::path::Path;

use crate::{Metadata, MetadataError, Value, Writer, NS_EXIF, NS_GPS};

// TIFF field types.
const TYPE_ASCII: u16 = 2;
const TYPE_SHORT: u16 = 3;
const TYPE_LONG: u16 = 4;
const TYPE_RATIONAL: u16 = 5;

// Tags we update (values are spec constants, not code).
const TAG_IMAGE_DESCRIPTION: u16 = 0x010E;
const TAG_MAKE: u16 = 0x010F;
const TAG_MODEL: u16 = 0x0110;
const TAG_ORIENTATION: u16 = 0x0112;
const TAG_SOFTWARE: u16 = 0x0131;
const TAG_DATE_TIME: u16 = 0x0132;
const TAG_ARTIST: u16 = 0x013B;
const TAG_COPYRIGHT: u16 = 0x8298;
const TAG_THUMBNAIL_OFFSET: u16 = 0x0201;
const TAG_THUMBNAIL_LENGTH: u16 = 0x0202;
const TAG_EXIF_IFD: u16 = 0x8769;
const TAG_GPS_IFD: u16 = 0x8825;
const TAG_EXPOSURE_TIME: u16 = 0x829A;
const TAG_FNUMBER: u16 = 0x829D;
const TAG_ISO: u16 = 0x8827;
const TAG_DATE_TIME_ORIGINAL: u16 = 0x9003;
const TAG_DATE_TIME_DIGITIZED: u16 = 0x9004;
const TAG_FOCAL_LENGTH: u16 = 0x920A;
const TAG_COLOR_SPACE: u16 = 0xA001;
const TAG_PIXEL_X: u16 = 0xA002;
const TAG_PIXEL_Y: u16 = 0xA003;
// GPS tags.
const TAG_GPS_LAT_REF: u16 = 0x0001;
const TAG_GPS_LAT: u16 = 0x0002;
const TAG_GPS_LON_REF: u16 = 0x0003;
const TAG_GPS_LON: u16 = 0x0004;
const TAG_GPS_ALT_REF: u16 = 0x0005;
const TAG_GPS_ALT: u16 = 0x0006;

const MAX_IFDS: usize = 8;
const MAX_JPEG_SEGMENT: usize = 0xFFFF;
const EXIF_PREFIX: &[u8] = b"Exif\0\0";

/// Writer behind the crate's [`crate::Writer`] trait for EXIF containers.
#[derive(Debug, Default)]
pub struct ExifWriter;

impl Writer for ExifWriter {
    fn write(&self, data: &[u8], metadata: &Metadata) -> Result<Vec<u8>, MetadataError> {
        write_exif(data, metadata)
    }
}

/// Write EXIF metadata into JPEG or TIFF-family bytes.
pub fn write_exif(data: &[u8], metadata: &Metadata) -> Result<Vec<u8>, MetadataError> {
    if data.starts_with(&[0xFF, 0xD8]) {
        write_jpeg(data, metadata)
    } else if is_tiff(data) {
        write_tiff(data, metadata)
    } else {
        Err(MetadataError::UnsupportedWriteTarget(
            "EXIF write expects JPEG or TIFF-family bytes".to_string(),
        ))
    }
}

/// Read, update and atomically replace EXIF metadata in a file.
pub fn write_exif_file(path: &Path, metadata: &Metadata) -> Result<(), MetadataError> {
    let input = std::fs::read(path)?;
    let output = write_exif(&input, metadata)?;
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

fn is_tiff(data: &[u8]) -> bool {
    data.len() >= 8
        && (data.starts_with(b"II") || data.starts_with(b"MM"))
        && read_u16(data, 2, data.starts_with(b"II")) == Some(42)
}

// ---------------------------------------------------------------------------
// Raw IFD model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RawEntry {
    tag: u16,
    type_id: u16,
    count: u32,
    /// Resolved value bytes (file endianness, `count * type_size` long).
    value: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
struct RawIfd {
    entries: Vec<RawEntry>,
}

#[derive(Debug, Clone)]
struct Doc {
    le: bool,
    ifd0: RawIfd,
    exif: Option<RawIfd>,
    gps: Option<RawIfd>,
    /// Top-level chain after IFD0 (IFD1, IFD2, ...), which carries thumbnails.
    chain: Vec<RawIfd>,
}

fn type_size(type_id: u16) -> Option<usize> {
    match type_id {
        1 | 2 | 6 | 7 => Some(1),
        3 | 8 => Some(2),
        4 | 9 | 11 => Some(4),
        5 | 10 | 12 => Some(8),
        _ => None,
    }
}

fn read_u16(data: &[u8], at: usize, le: bool) -> Option<u16> {
    let b: [u8; 2] = data.get(at..at + 2)?.try_into().ok()?;
    Some(if le {
        u16::from_le_bytes(b)
    } else {
        u16::from_be_bytes(b)
    })
}

fn read_u32(data: &[u8], at: usize, le: bool) -> Option<u32> {
    let b: [u8; 4] = data.get(at..at + 4)?.try_into().ok()?;
    Some(if le {
        u32::from_le_bytes(b)
    } else {
        u32::from_be_bytes(b)
    })
}

fn push_u16(out: &mut Vec<u8>, value: u16, le: bool) {
    out.extend_from_slice(&if le {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    });
}

fn push_u32(out: &mut Vec<u8>, value: u32, le: bool) {
    out.extend_from_slice(&if le {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    });
}

fn write_u32_at(buf: &mut [u8], value: u32, le: bool) {
    let bytes = if le {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    };
    buf.copy_from_slice(&bytes);
}

fn malformed(msg: &str) -> MetadataError {
    MetadataError::Malformed(msg.to_string())
}

fn parse_doc(tiff: &[u8]) -> Result<Doc, MetadataError> {
    if tiff.len() < 8 {
        return Err(malformed("truncated TIFF header"));
    }
    let le = match &tiff[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return Err(malformed("bad TIFF byte-order mark")),
    };
    if read_u16(tiff, 2, le) != Some(42) {
        return Err(malformed("bad TIFF magic"));
    }
    let ifd0_off =
        read_u32(tiff, 4, le).ok_or_else(|| malformed("truncated IFD0 offset"))? as usize;

    let mut visited = std::collections::HashSet::new();
    let (ifd0, mut next) = read_ifd(tiff, ifd0_off, le, &mut visited)?;

    let exif_off = pointer(&ifd0, TAG_EXIF_IFD, le);
    let gps_off = pointer(&ifd0, TAG_GPS_IFD, le);
    let exif = exif_off
        .map(|off| read_ifd(tiff, off as usize, le, &mut visited).map(|(ifd, _)| ifd))
        .transpose()?;
    let gps = gps_off
        .map(|off| read_ifd(tiff, off as usize, le, &mut visited).map(|(ifd, _)| ifd))
        .transpose()?;

    // Follow the top-level chain (IFD1+) which carries thumbnails.
    let mut chain = Vec::new();
    let mut hops = 0usize;
    while let Some(off) = next {
        if hops >= MAX_IFDS {
            break;
        }
        hops += 1;
        let (ifd, following) = read_ifd(tiff, off as usize, le, &mut visited)?;
        chain.push(ifd);
        next = following;
    }

    Ok(Doc {
        le,
        ifd0,
        exif,
        gps,
        chain,
    })
}

fn read_ifd(
    tiff: &[u8],
    offset: usize,
    le: bool,
    visited: &mut std::collections::HashSet<usize>,
) -> Result<(RawIfd, Option<u32>), MetadataError> {
    if !visited.insert(offset) {
        return Err(malformed("IFD loop detected"));
    }
    let count =
        read_u16(tiff, offset, le).ok_or_else(|| malformed("truncated IFD count"))? as usize;
    let table_start = offset + 2;
    let table_end = table_start
        .checked_add(
            count
                .checked_mul(12)
                .ok_or_else(|| malformed("IFD overflow"))?,
        )
        .ok_or_else(|| malformed("IFD overflow"))?;
    if table_end > tiff.len() {
        return Err(malformed("IFD entry table extends past the data"));
    }
    let next = match read_u32(tiff, table_end, le) {
        Some(0) | None => None,
        Some(n) => Some(n),
    };

    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let at = table_start + i * 12;
        let Some(tag) = read_u16(tiff, at, le) else {
            continue;
        };
        let Some(type_id) = read_u16(tiff, at + 2, le) else {
            continue;
        };
        let count_v = read_u32(tiff, at + 4, le).unwrap_or(0);
        let Some(elem) = type_size(type_id) else {
            continue;
        };
        let Some(size) = elem.checked_mul(count_v as usize) else {
            continue;
        };
        if size > 16 * 1024 * 1024 {
            continue;
        }
        let value = if size <= 4 {
            match tiff.get(at + 8..at + 8 + size) {
                Some(v) => v.to_vec(),
                None => continue,
            }
        } else {
            match read_u32(tiff, at + 8, le) {
                Some(off) if (off as usize).saturating_add(size) <= tiff.len() => {
                    tiff[off as usize..off as usize + size].to_vec()
                }
                _ => continue,
            }
        };
        entries.push(RawEntry {
            tag,
            type_id,
            count: count_v,
            value,
        });
    }
    Ok((RawIfd { entries }, next))
}

fn pointer(ifd: &RawIfd, tag: u16, le: bool) -> Option<u32> {
    ifd.entries
        .iter()
        .find(|e| e.tag == tag && e.type_id == TYPE_LONG && e.value.len() >= 4)
        .and_then(|e| read_u32(&e.value, 0, le))
}

fn entry_u32(ifd: &RawIfd, tag: u16, le: bool) -> Option<u32> {
    pointer(ifd, tag, le)
}

// ---------------------------------------------------------------------------
// Entry constructors and mutation
// ---------------------------------------------------------------------------

fn ascii_entry(tag: u16, text: &str) -> RawEntry {
    let mut value = text.as_bytes().to_vec();
    value.push(0);
    RawEntry {
        tag,
        type_id: TYPE_ASCII,
        count: value.len() as u32,
        value,
    }
}

fn short_entry(tag: u16, values: &[u16], le: bool) -> RawEntry {
    let mut value = Vec::with_capacity(values.len() * 2);
    for v in values {
        push_u16(&mut value, *v, le);
    }
    RawEntry {
        tag,
        type_id: TYPE_SHORT,
        count: values.len() as u32,
        value,
    }
}

fn long_entry(tag: u16, value: u32, le: bool) -> RawEntry {
    let mut bytes = Vec::with_capacity(4);
    push_u32(&mut bytes, value, le);
    RawEntry {
        tag,
        type_id: TYPE_LONG,
        count: 1,
        value: bytes,
    }
}

fn rational_entry(tag: u16, pairs: &[(u32, u32)], le: bool) -> RawEntry {
    let mut value = Vec::with_capacity(pairs.len() * 8);
    for (num, den) in pairs {
        push_u32(&mut value, *num, le);
        push_u32(&mut value, *den, le);
    }
    RawEntry {
        tag,
        type_id: TYPE_RATIONAL,
        count: pairs.len() as u32,
        value,
    }
}

fn byte_entry(tag: u16, value: u8) -> RawEntry {
    RawEntry {
        tag,
        type_id: 1,
        count: 1,
        value: vec![value],
    }
}

/// Replace an entry by tag, or insert it keeping ascending tag order.
fn set_entry(ifd: &mut RawIfd, entry: RawEntry) {
    if let Some(pos) = ifd.entries.iter().position(|e| e.tag == entry.tag) {
        ifd.entries[pos] = entry;
        return;
    }
    let pos = ifd
        .entries
        .iter()
        .position(|e| e.tag > entry.tag)
        .unwrap_or(ifd.entries.len());
    ifd.entries.insert(pos, entry);
}

fn ensure_exif(doc: &mut Doc) -> &mut RawIfd {
    if doc.exif.is_none() {
        doc.exif = Some(RawIfd::default());
    }
    if !doc.ifd0.entries.iter().any(|e| e.tag == TAG_EXIF_IFD) {
        let entry = long_entry(TAG_EXIF_IFD, 0, doc.le);
        set_entry(&mut doc.ifd0, entry);
    }
    doc.exif.as_mut().expect("just created")
}

fn ensure_gps(doc: &mut Doc) -> &mut RawIfd {
    if doc.gps.is_none() {
        doc.gps = Some(RawIfd::default());
    }
    if !doc.ifd0.entries.iter().any(|e| e.tag == TAG_GPS_IFD) {
        let entry = long_entry(TAG_GPS_IFD, 0, doc.le);
        set_entry(&mut doc.ifd0, entry);
    }
    doc.gps.as_mut().expect("just created")
}

// ---------------------------------------------------------------------------
// Applying metadata updates
// ---------------------------------------------------------------------------

fn apply_updates(doc: &mut Doc, current: &Metadata, incoming: &Metadata) -> bool {
    let mut changed = false;
    let le = doc.le;

    // Plain ASCII fields in IFD0.
    for (field, tag) in [
        ("Make", TAG_MAKE),
        ("Model", TAG_MODEL),
        ("Software", TAG_SOFTWARE),
        ("Artist", TAG_ARTIST),
        ("Copyright", TAG_COPYRIGHT),
        ("ImageDescription", TAG_IMAGE_DESCRIPTION),
    ] {
        if let Some(text) = incoming.text(NS_EXIF, field) {
            if current.text(NS_EXIF, field) != Some(text) {
                set_entry(&mut doc.ifd0, ascii_entry(tag, text));
                changed = true;
            }
        }
    }

    if let Some(text) = incoming.text(NS_EXIF, "DateTime") {
        if current.text(NS_EXIF, "DateTime") != Some(text) {
            set_entry(
                &mut doc.ifd0,
                ascii_entry(TAG_DATE_TIME, &exif_datetime(text)),
            );
            changed = true;
        }
    }

    if let Some(value) = incoming.get(NS_EXIF, "Orientation") {
        let current_text = current.text(NS_EXIF, "Orientation");
        let differs = match value {
            Value::Text(text) => current_text != Some(text.as_str()),
            _ => true,
        };
        if differs {
            if let Some(code) = orientation_code(value) {
                set_entry(&mut doc.ifd0, short_entry(TAG_ORIENTATION, &[code], le));
                changed = true;
            }
        }
    }

    // EXIF sub-IFD fields.
    if let Some(text) = incoming.text(NS_EXIF, "DateTimeOriginal") {
        if current.text(NS_EXIF, "DateTimeOriginal") != Some(text) {
            let entry = ascii_entry(TAG_DATE_TIME_ORIGINAL, &exif_datetime(text));
            set_entry(ensure_exif(doc), entry);
            changed = true;
        }
    }
    if let Some(text) = incoming.text(NS_EXIF, "DateTimeDigitized") {
        if current.text(NS_EXIF, "DateTimeDigitized") != Some(text) {
            let entry = ascii_entry(TAG_DATE_TIME_DIGITIZED, &exif_datetime(text));
            set_entry(ensure_exif(doc), entry);
            changed = true;
        }
    }
    if let Some(text) = incoming.text(NS_EXIF, "ExposureTime") {
        if current.text(NS_EXIF, "ExposureTime") != Some(text) {
            if let Some((num, den)) = parse_exposure(text) {
                let entry = rational_entry(TAG_EXPOSURE_TIME, &[(num, den)], le);
                set_entry(ensure_exif(doc), entry);
                changed = true;
            }
        }
    }
    if let Some(text) = incoming.text(NS_EXIF, "FNumber") {
        if current.text(NS_EXIF, "FNumber") != Some(text) {
            if let Some(v) = parse_prefixed_number(text, "f/") {
                let (num, den) = decimal_rational(v);
                let entry = rational_entry(TAG_FNUMBER, &[(num, den)], le);
                set_entry(ensure_exif(doc), entry);
                changed = true;
            }
        }
    }
    if let Some(text) = incoming.text(NS_EXIF, "FocalLength") {
        if current.text(NS_EXIF, "FocalLength") != Some(text) {
            if let Some(v) = parse_prefixed_number(text, "") {
                let (num, den) = decimal_rational(v);
                let entry = rational_entry(TAG_FOCAL_LENGTH, &[(num, den)], le);
                set_entry(ensure_exif(doc), entry);
                changed = true;
            }
        }
    }
    if let Some(text) = incoming.text(NS_EXIF, "ISOSpeedRatings") {
        if current.text(NS_EXIF, "ISOSpeedRatings") != Some(text) {
            let values: Vec<u16> = text
                .split(',')
                .filter_map(|part| part.trim().parse::<u16>().ok())
                .collect();
            if !values.is_empty() {
                let entry = short_entry(TAG_ISO, &values, le);
                set_entry(ensure_exif(doc), entry);
                changed = true;
            }
        }
    }
    if let Some(text) = incoming.text(NS_EXIF, "ColorSpace") {
        if current.text(NS_EXIF, "ColorSpace") != Some(text) {
            if let Some(code) = color_space_code(text) {
                let entry = short_entry(TAG_COLOR_SPACE, &[code], le);
                set_entry(ensure_exif(doc), entry);
                changed = true;
            }
        }
    }
    for (field, tag) in [("PixelWidth", TAG_PIXEL_X), ("PixelHeight", TAG_PIXEL_Y)] {
        if let Some(text) = incoming.text(NS_EXIF, field) {
            if current.text(NS_EXIF, field) != Some(text) {
                if let Ok(n) = text.trim().parse::<u32>() {
                    let entry = long_entry(tag, n, le);
                    set_entry(ensure_exif(doc), entry);
                    changed = true;
                }
            }
        }
    }

    // GPS sub-IFD fields.
    if let Some(text) = incoming.text(NS_GPS, "Latitude") {
        if current.text(NS_GPS, "Latitude") != Some(text) {
            if let Some((degrees, reference)) = parse_coordinate(text, 'N', 'S') {
                let dms = dms_rationals(degrees);
                let gps = ensure_gps(doc);
                set_entry(gps, ascii_entry(TAG_GPS_LAT_REF, &reference.to_string()));
                let entry = rational_entry(TAG_GPS_LAT, &dms, le);
                set_entry(gps, entry);
                changed = true;
            }
        }
    }
    if let Some(text) = incoming.text(NS_GPS, "Longitude") {
        if current.text(NS_GPS, "Longitude") != Some(text) {
            if let Some((degrees, reference)) = parse_coordinate(text, 'E', 'W') {
                let dms = dms_rationals(degrees);
                let gps = ensure_gps(doc);
                set_entry(gps, ascii_entry(TAG_GPS_LON_REF, &reference.to_string()));
                let entry = rational_entry(TAG_GPS_LON, &dms, le);
                set_entry(gps, entry);
                changed = true;
            }
        }
    }
    if let Some(text) = incoming.text(NS_GPS, "Altitude") {
        if current.text(NS_GPS, "Altitude") != Some(text) {
            if let Some(metres) = parse_metres(text) {
                let (num, den) = decimal_rational(metres.abs());
                let gps = ensure_gps(doc);
                set_entry(gps, byte_entry(TAG_GPS_ALT_REF, u8::from(metres < 0.0)));
                let entry = rational_entry(TAG_GPS_ALT, &[(num, den)], le);
                set_entry(gps, entry);
                changed = true;
            }
        }
    }

    changed
}

/// EXIF dates use `YYYY:MM:DD hh:mm:ss`; the reader normalises dashes for the
/// model, so put the colons back on the way out.
fn exif_datetime(text: &str) -> String {
    let bytes = text.as_bytes();
    if bytes.len() >= 10 && bytes[4] == b'-' && bytes[7] == b'-' {
        let mut out = String::with_capacity(text.len());
        out.push_str(&text[..4]);
        out.push(':');
        out.push_str(&text[5..7]);
        out.push(':');
        out.push_str(&text[8..]);
        out
    } else {
        text.to_string()
    }
}

fn orientation_code(value: &Value) -> Option<u16> {
    let code = match value {
        Value::Integer(n) => u16::try_from(*n).ok()?,
        Value::Text(text) => {
            if let Some((_, tail)) = text.rsplit_once('(') {
                tail.trim_end_matches(')').trim().parse::<u16>().ok()?
            } else {
                match text.to_ascii_lowercase().as_str() {
                    "normal" => 1,
                    "mirror horizontal" => 2,
                    "rotate 180" => 3,
                    "mirror vertical" => 4,
                    "mirror horizontal + rotate 90 cw" => 5,
                    "rotate 90 cw" => 6,
                    "mirror horizontal + rotate 270 cw" => 7,
                    "rotate 270 cw" => 8,
                    _ => return None,
                }
            }
        }
        _ => return None,
    };
    (1..=8).contains(&code).then_some(code)
}

fn color_space_code(text: &str) -> Option<u16> {
    match text.trim() {
        "sRGB" => Some(1),
        "Uncalibrated" => Some(2),
        _ => None,
    }
}

fn parse_exposure(text: &str) -> Option<(u32, u32)> {
    let t = text.trim().trim_end_matches(" s").trim();
    if let Some(rest) = t.strip_prefix("1/") {
        let den: f64 = rest.trim().parse().ok()?;
        if den <= 0.0 {
            return None;
        }
        return Some((1, den.round().max(1.0) as u32));
    }
    let seconds: f64 = t.parse().ok()?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    Some(decimal_rational(seconds))
}

/// Parse a number that may carry a known prefix (`f/`) and unit suffix
/// (` mm`). Trailing non-numeric text is ignored.
fn parse_prefixed_number(text: &str, prefix: &str) -> Option<f64> {
    let t = text.trim().trim_end_matches(" mm").trim();
    let t = t.strip_prefix(prefix).unwrap_or(t).trim();
    t.parse::<f64>().ok()
}

/// Decimal -> reduced rational with at most three decimal places.
fn decimal_rational(value: f64) -> (u32, u32) {
    if !value.is_finite() || value < 0.0 {
        return (0, 1);
    }
    let scaled = (value * 1000.0).round() as u32;
    let divisor = gcd(scaled, 1000);
    (scaled / divisor, 1000 / divisor)
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 {
        a.max(1)
    } else {
        gcd(b, a % b)
    }
}

/// Parse `"47.61667 N"` / `"-122.4"` into an unsigned magnitude and a ref
/// character. A negative value flips the default reference.
fn parse_coordinate(text: &str, positive: char, negative: char) -> Option<(f64, char)> {
    let t = text.trim();
    let (number, suffix) = match t.rsplit_once(char::is_whitespace) {
        Some((number, suffix)) => (number.trim(), suffix.trim()),
        None => (t, ""),
    };
    let value: f64 = number.parse().ok()?;
    if !value.is_finite() {
        return None;
    }
    let reference = match suffix.to_ascii_uppercase().as_str() {
        "N" | "S" | "E" | "W" => suffix.to_ascii_uppercase().chars().next()?,
        "" => {
            if value < 0.0 {
                negative
            } else {
                positive
            }
        }
        _ => return None,
    };
    Some((value.abs(), reference))
}

fn dms_rationals(degrees: f64) -> [(u32, u32); 3] {
    let abs = degrees.abs();
    let mut deg = abs.floor();
    let minutes_f = (abs - deg) * 60.0;
    let mut min = minutes_f.floor();
    let mut sec = ((minutes_f - min) * 60.0 * 100.0).round();
    if sec >= 6000.0 {
        sec = 0.0;
        min += 1.0;
    }
    if min >= 60.0 {
        min = 0.0;
        deg += 1.0;
    }
    [(deg as u32, 1), (min as u32, 1), (sec as u32, 100)]
}

/// Parse `"123.4 m"` (the reader's altitude format).
fn parse_metres(text: &str) -> Option<f64> {
    let t = text.trim().trim_end_matches(" m").trim();
    t.parse::<f64>().ok().filter(|v| v.is_finite())
}

// ---------------------------------------------------------------------------
// Serialisation
// ---------------------------------------------------------------------------

/// Build an IFD tree whose table offsets start at `table_base` (absolute from
/// the start of the TIFF block). Returns the bytes to place at `table_base`.
fn build_tree(doc: &Doc, original: &[u8], table_base: usize, copy_thumbnail: bool) -> Vec<u8> {
    let le = doc.le;
    let mut ifds: Vec<RawIfd> = Vec::new();
    ifds.push(doc.ifd0.clone());
    let exif_idx = doc.exif.as_ref().map(|ifd| {
        ifds.push(ifd.clone());
        ifds.len() - 1
    });
    let gps_idx = doc.gps.as_ref().map(|ifd| {
        ifds.push(ifd.clone());
        ifds.len() - 1
    });
    let chain_start = ifds.len();
    for ifd in &doc.chain {
        ifds.push(ifd.clone());
    }

    // Table offsets.
    let mut offsets = Vec::with_capacity(ifds.len());
    let mut cursor = table_base;
    for ifd in &ifds {
        offsets.push(cursor);
        cursor += 2 + ifd.entries.len() * 12 + 4;
    }
    let data_start = cursor;

    // Repoint the sub-IFD pointers into this tree.
    if let Some(index) = exif_idx {
        let entry = long_entry(TAG_EXIF_IFD, offsets[index] as u32, le);
        set_entry(&mut ifds[0], entry);
    }
    if let Some(index) = gps_idx {
        let entry = long_entry(TAG_GPS_IFD, offsets[index] as u32, le);
        set_entry(&mut ifds[0], entry);
    }

    // Payload placement. Thumbnails are relocated first so their offset can
    // be patched before ordinary out-of-line values are collected.
    let mut payloads: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut data_cursor = data_start;

    if copy_thumbnail {
        for ifd in ifds.iter_mut().skip(chain_start) {
            let old_offset = entry_u32(ifd, TAG_THUMBNAIL_OFFSET, le);
            let length = entry_u32(ifd, TAG_THUMBNAIL_LENGTH, le);
            if let (Some(old), Some(len)) = (old_offset, length) {
                let start = old as usize;
                let end = start.saturating_add(len as usize);
                if let Some(bytes) = original.get(start..end) {
                    let bytes = bytes.to_vec();
                    let offset = data_cursor;
                    data_cursor += bytes.len();
                    if !data_cursor.is_multiple_of(2) {
                        data_cursor += 1;
                    }
                    set_entry(ifd, long_entry(TAG_THUMBNAIL_OFFSET, offset as u32, le));
                    payloads.push((offset, bytes));
                }
            }
        }
    }

    let mut value_offsets: Vec<Vec<Option<usize>>> = ifds
        .iter()
        .map(|ifd| vec![None; ifd.entries.len()])
        .collect();
    for (i, ifd) in ifds.iter().enumerate() {
        for (j, entry) in ifd.entries.iter().enumerate() {
            let is_thumbnail_pointer =
                copy_thumbnail && i >= chain_start && entry.tag == TAG_THUMBNAIL_OFFSET;
            if entry.value.len() > 4 && !is_thumbnail_pointer {
                let offset = data_cursor;
                data_cursor += entry.value.len();
                if !data_cursor.is_multiple_of(2) {
                    data_cursor += 1;
                }
                value_offsets[i][j] = Some(offset);
                payloads.push((offset, entry.value.clone()));
            }
        }
    }

    // Write the tables.
    let mut out = Vec::with_capacity(data_cursor.saturating_sub(table_base));
    for (i, ifd) in ifds.iter().enumerate() {
        push_u16(&mut out, ifd.entries.len() as u16, le);
        for (j, entry) in ifd.entries.iter().enumerate() {
            push_u16(&mut out, entry.tag, le);
            push_u16(&mut out, entry.type_id, le);
            push_u32(&mut out, entry.count, le);
            match value_offsets[i][j] {
                Some(offset) => push_u32(&mut out, offset as u32, le),
                None => {
                    let mut slot = [0u8; 4];
                    let take = entry.value.len().min(4);
                    slot[..take].copy_from_slice(&entry.value[..take]);
                    out.extend_from_slice(&slot);
                }
            }
        }
        let next = if i == 0 && chain_start < ifds.len() {
            offsets[chain_start] as u32
        } else if i >= chain_start && i + 1 < ifds.len() {
            offsets[i + 1] as u32
        } else {
            0
        };
        push_u32(&mut out, next, le);
    }

    // Append payloads in offset order, filling alignment gaps.
    payloads.sort_by_key(|(offset, _)| *offset);
    for (offset, bytes) in payloads {
        while table_base + out.len() < offset {
            out.push(0);
        }
        out.extend_from_slice(&bytes);
    }
    out
}

/// Serialise a compact TIFF block with IFD0 at offset 8 (JPEG `APP1` mode).
fn serialize_compact(doc: &Doc, original: &[u8]) -> Vec<u8> {
    let tree = build_tree(doc, original, 8, true);
    let mut out = Vec::with_capacity(8 + tree.len());
    out.extend_from_slice(if doc.le { b"II" } else { b"MM" });
    push_u16(&mut out, 42, doc.le);
    push_u32(&mut out, 8, doc.le);
    out.extend_from_slice(&tree);
    out
}

// ---------------------------------------------------------------------------
// JPEG container
// ---------------------------------------------------------------------------

/// (segment start, segment end, embedded TIFF payload).
type App1Location<'a> = (usize, usize, &'a [u8]);

/// Locate the first `APP1` segment carrying `Exif\0\0`.
fn find_app1(data: &[u8]) -> Result<Option<App1Location<'_>>, MetadataError> {
    let mut pos = 2usize;
    while pos + 4 <= data.len() {
        if data[pos] != 0xFF {
            return Err(malformed("JPEG marker sync lost"));
        }
        let marker = data[pos + 1];
        if marker == 0xFF {
            pos += 1;
            continue;
        }
        if marker == 0xDA || marker == 0xD9 {
            return Ok(None);
        }
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            pos += 2;
            continue;
        }
        let size = read_u16(data, pos + 2, false)
            .ok_or_else(|| malformed("truncated JPEG segment length"))? as usize;
        if size < 2 {
            return Err(malformed("zero-length JPEG segment"));
        }
        let body_start = pos + 4;
        let body_end = body_start
            .checked_add(size - 2)
            .ok_or_else(|| malformed("JPEG segment length overflow"))?;
        if body_end > data.len() {
            return Err(malformed("JPEG segment extends past the data"));
        }
        if marker == 0xE1 && data[body_start..body_end].starts_with(EXIF_PREFIX) {
            return Ok(Some((
                pos,
                body_end,
                &data[body_start + EXIF_PREFIX.len()..body_end],
            )));
        }
        pos = body_end;
    }
    Ok(None)
}

fn write_jpeg(data: &[u8], metadata: &Metadata) -> Result<Vec<u8>, MetadataError> {
    match find_app1(data)? {
        Some((start, end, tiff)) => {
            let current = crate::exif::read_exif(tiff).unwrap_or_default();
            let mut doc = parse_doc(tiff)?;
            if !apply_updates(&mut doc, &current, metadata) {
                return Ok(data.to_vec());
            }
            let tiff = serialize_compact(&doc, tiff);
            splice_app1(data, start, end, &tiff)
        }
        None => {
            let mut doc = Doc {
                le: true,
                ifd0: RawIfd::default(),
                exif: None,
                gps: None,
                chain: Vec::new(),
            };
            if !apply_updates(&mut doc, &Metadata::new(), metadata) {
                return Ok(data.to_vec());
            }
            let tiff = serialize_compact(&doc, &[]);
            // Insert after the leading SOI (and after APP0/JFIF when present).
            let insert_at = jpeg_insert_offset(data);
            splice_app1(data, insert_at, insert_at, &tiff)
        }
    }
}

/// Insertion point for a new `APP1`: after SOI, or after the leading `APP0`.
fn jpeg_insert_offset(data: &[u8]) -> usize {
    if data.len() >= 6 && data[2] == 0xFF && data[3] == 0xE0 {
        if let Some(size) = read_u16(data, 4, false) {
            let end = 4 + size as usize;
            if end <= data.len() {
                return end;
            }
        }
    }
    if data.len() >= 2 && data.starts_with(&[0xFF, 0xD8]) {
        2
    } else {
        0
    }
}

/// Replace bytes `[start, end)` with a new `APP1` segment, or insert at
/// `start == end`.
fn splice_app1(
    data: &[u8],
    start: usize,
    end: usize,
    tiff: &[u8],
) -> Result<Vec<u8>, MetadataError> {
    let segment_len = 2 + EXIF_PREFIX.len() + tiff.len();
    if segment_len > MAX_JPEG_SEGMENT {
        return Err(MetadataError::UnsupportedWriteTarget(format!(
            "EXIF block too large for a JPEG APP1 segment ({segment_len} bytes)"
        )));
    }
    let mut out = Vec::with_capacity(data.len() - (end - start) + segment_len + 2);
    out.extend_from_slice(&data[..start]);
    out.extend_from_slice(&[0xFF, 0xE1]);
    push_u16(&mut out, segment_len as u16, false);
    out.extend_from_slice(EXIF_PREFIX);
    out.extend_from_slice(tiff);
    out.extend_from_slice(&data[end..]);
    Ok(out)
}

// ---------------------------------------------------------------------------
// TIFF container
// ---------------------------------------------------------------------------

fn write_tiff(data: &[u8], metadata: &Metadata) -> Result<Vec<u8>, MetadataError> {
    let mut doc = parse_doc(data)?;
    let current = crate::exif::read_exif(data).unwrap_or_default();
    if !apply_updates(&mut doc, &current, metadata) {
        return Ok(data.to_vec());
    }
    // Append the new tree so strips, tiles and any unparsed data stay put.
    let mut out = data.to_vec();
    if !out.len().is_multiple_of(2) {
        out.push(0);
    }
    let base = out.len();
    let tree = build_tree(&doc, data, base, false);
    write_u32_at(&mut out[4..8], base as u32, doc.le);
    out.extend_from_slice(&tree);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exif::read_exif;

    /// Minimal little-endian TIFF with one IFD0 entry (`Make`) plus an unknown
    /// LONG tag that a read-only test can verify survives the rewrite.
    fn tiff_fixture(make: &str) -> Vec<u8> {
        let value_offset = 8 + 2 + 2 * 12 + 4; // header + IFD table
        let make_bytes = {
            let mut v = make.as_bytes().to_vec();
            v.push(0);
            v
        };
        let mut out = Vec::new();
        out.extend_from_slice(b"II");
        push_u16(&mut out, 42, true);
        push_u32(&mut out, 8, true);
        push_u16(&mut out, 2, true);
        // Make (ASCII, out-of-line)
        push_u16(&mut out, TAG_MAKE, true);
        push_u16(&mut out, TYPE_ASCII, true);
        push_u32(&mut out, make_bytes.len() as u32, true);
        push_u32(&mut out, value_offset as u32, true);
        // Unknown LONG tag (inline)
        push_u16(&mut out, 0xC0DE, true);
        push_u16(&mut out, TYPE_LONG, true);
        push_u32(&mut out, 1, true);
        push_u32(&mut out, 0x1234_5678, true);
        push_u32(&mut out, 0, true); // next IFD
        out.extend_from_slice(&make_bytes);
        out
    }

    fn jpeg_fixture() -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8];
        // APP0/JFIF: marker + length 16 + 14 bytes payload
        out.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]);
        out.extend_from_slice(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        // APP13 with opaque payload
        out.extend_from_slice(&[0xFF, 0xED, 0x00, 0x0A]);
        out.extend_from_slice(b"opaque!!");
        // SOS + fake scan data + EOI
        out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00]);
        out.extend_from_slice(&[0x11, 0x22, 0x33]);
        out.extend_from_slice(&[0xFF, 0xD9]);
        out
    }

    fn app1_payload(data: &[u8]) -> Vec<u8> {
        find_app1(data).unwrap().unwrap().2.to_vec()
    }

    #[test]
    fn tiff_roundtrip_without_edits_is_byte_identical() {
        let original = tiff_fixture("Canon");
        let metadata = read_exif(&original).unwrap();
        let output = write_exif(&original, &metadata).unwrap();
        assert_eq!(output, original);
    }

    #[test]
    fn tiff_updates_make_and_preserves_unknown_tag() {
        let original = tiff_fixture("Canon");
        let mut metadata = read_exif(&original).unwrap();
        metadata.set_text(NS_EXIF, "Make", "Nikon");
        let output = write_exif(&original, &metadata).unwrap();
        assert_ne!(output, original);
        // Everything except the repointed IFD0 offset is intact.
        assert_eq!(&output[8..original.len()], &original[8..]);
        // New tree appended and header repointed.
        let reparsed = read_exif(&output).unwrap();
        assert_eq!(reparsed.text(NS_EXIF, "Make"), Some("Nikon"));
        let doc = parse_doc(&output).unwrap();
        let unknown = doc
            .ifd0
            .entries
            .iter()
            .find(|e| e.tag == 0xC0DE)
            .expect("unknown tag preserved");
        assert_eq!(read_u32(&unknown.value, 0, true), Some(0x1234_5678));
        // The appended tree is reachable from the patched header offset.
        let ifd0_offset = read_u32(&output, 4, true).unwrap() as usize;
        assert_eq!(ifd0_offset, original.len());
    }

    #[test]
    fn jpeg_inserts_app1_and_preserves_other_segments() {
        let original = jpeg_fixture();
        let metadata = Metadata::new();
        let mut metadata = metadata;
        metadata.set_text(NS_EXIF, "Make", "BridgeCraft Cam");
        let output = write_exif(&original, &metadata).unwrap();

        // Recover the non-APP1 bytes and compare with the original fixture.
        let (start, end, _) = find_app1(&output).unwrap().unwrap();
        let mut stripped = Vec::new();
        stripped.extend_from_slice(&output[..start]);
        stripped.extend_from_slice(&output[end..]);
        assert_eq!(stripped, original);

        let reparsed = read_exif(&output).unwrap();
        assert_eq!(reparsed.text(NS_EXIF, "Make"), Some("BridgeCraft Cam"));
    }

    #[test]
    fn jpeg_replaces_existing_app1_in_place() {
        let metadata = {
            let mut m = Metadata::new();
            m.set_text(NS_EXIF, "Make", "First");
            m
        };
        let original = write_exif(&jpeg_fixture(), &metadata).unwrap();
        let mut updated = read_exif(&original).unwrap();
        updated.set_text(NS_EXIF, "Make", "Second");
        let output = write_exif(&original, &updated).unwrap();

        // Other segments and scan data untouched, exactly one APP1.
        assert_eq!(output.iter().filter(|b| **b == 0xE1).count(), 1);
        let reparsed = read_exif(&output).unwrap();
        assert_eq!(reparsed.text(NS_EXIF, "Make"), Some("Second"));
    }

    #[test]
    fn jpeg_roundtrip_without_edits_is_byte_identical() {
        let metadata = {
            let mut m = Metadata::new();
            m.set_text(NS_EXIF, "Make", "RoundTrip");
            m
        };
        let original = write_exif(&jpeg_fixture(), &metadata).unwrap();
        let read_back = read_exif(&original).unwrap();
        let output = write_exif(&original, &read_back).unwrap();
        assert_eq!(output, original);
    }

    #[test]
    fn gps_is_written_and_read_back() {
        let original = tiff_fixture("Canon");
        let mut metadata = read_exif(&original).unwrap();
        metadata.set_text(NS_GPS, "Latitude", "-33.86880 S");
        metadata.set_text(NS_GPS, "Longitude", "151.20930 E");
        metadata.set_text(NS_GPS, "Altitude", "58.5 m");
        let output = write_exif(&original, &metadata).unwrap();

        let reparsed = read_exif(&output).unwrap();
        let lat = reparsed.text(NS_GPS, "Latitude").unwrap();
        let lon = reparsed.text(NS_GPS, "Longitude").unwrap();
        let lat_value: f64 = lat.split_whitespace().next().unwrap().parse().unwrap();
        let lon_value: f64 = lon.split_whitespace().next().unwrap().parse().unwrap();
        assert!((lat_value - 33.86880).abs() < 0.001);
        assert!((lon_value - 151.20930).abs() < 0.001);
        assert!(lat.ends_with('S'));
        assert!(lon.ends_with('E'));
        assert_eq!(reparsed.text(NS_GPS, "Altitude"), Some("58.5 m"));
    }

    #[test]
    fn orientation_and_exif_sub_ifd_are_written() {
        let original = tiff_fixture("Canon");
        let mut metadata = Metadata::new();
        metadata.set(NS_EXIF, "Orientation", Value::Integer(6));
        metadata.set_text(NS_EXIF, "DateTimeOriginal", "2024-05-06 07:08:09");
        metadata.set_text(NS_EXIF, "FNumber", "f/2.8");
        metadata.set_text(NS_EXIF, "ExposureTime", "1/250 s");
        let output = write_exif(&original, &metadata).unwrap();

        let reparsed = read_exif(&output).unwrap();
        assert_eq!(
            reparsed.text(NS_EXIF, "Orientation"),
            Some("Rotate 90 CW (6)")
        );
        assert_eq!(
            reparsed.text(NS_EXIF, "DateTimeOriginal"),
            Some("2024-05-06 07:08:09")
        );
        assert_eq!(reparsed.text(NS_EXIF, "FNumber"), Some("f/2.8"));
        assert_eq!(reparsed.text(NS_EXIF, "ExposureTime"), Some("1/250 s"));
    }

    #[test]
    fn nested_thumbnail_bytes_are_relocated_in_jpeg_mode() {
        // Build a TIFF with IFD0 -> IFD1 whose 0x0201/0x0202 point at a small
        // embedded JPEG-looking payload.
        let leaf = [0xFFu8, 0xD8, 0xAA, 0xBB, 0xFF, 0xD9];
        let make = b"Canon\0";

        let make_offset = 56usize; // after header + IFD0 table (18) + IFD1 table (30)
        let leaf_offset = make_offset + make.len();
        let mut out = Vec::new();
        out.extend_from_slice(b"II");
        push_u16(&mut out, 42, true);
        push_u32(&mut out, 8, true);
        // IFD0: one Make entry, next -> IFD1 (offset 26)
        push_u16(&mut out, 1, true);
        push_u16(&mut out, TAG_MAKE, true);
        push_u16(&mut out, TYPE_ASCII, true);
        push_u32(&mut out, make.len() as u32, true);
        push_u32(&mut out, make_offset as u32, true);
        push_u32(&mut out, 26, true);
        // IFD1 at 26: offset/length entries, next 0
        push_u16(&mut out, 2, true);
        push_u16(&mut out, TAG_THUMBNAIL_OFFSET, true);
        push_u16(&mut out, TYPE_LONG, true);
        push_u32(&mut out, 1, true);
        push_u32(&mut out, leaf_offset as u32, true);
        push_u16(&mut out, TAG_THUMBNAIL_LENGTH, true);
        push_u16(&mut out, TYPE_LONG, true);
        push_u32(&mut out, 1, true);
        push_u32(&mut out, leaf.len() as u32, true);
        push_u32(&mut out, 0, true);
        out.extend_from_slice(make);
        out.extend_from_slice(&leaf);
        assert_eq!(out.len(), leaf_offset + leaf.len());

        // Wrap in a JPEG APP1 so the writer takes the compact (thumbnail
        // relocating) path.
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
        let segment_len = 2 + EXIF_PREFIX.len() + out.len();
        push_u16(&mut jpeg, segment_len as u16, false);
        jpeg.extend_from_slice(EXIF_PREFIX);
        jpeg.extend_from_slice(&out);
        jpeg.extend_from_slice(&[0xFF, 0xD9]);

        let mut metadata = read_exif(&jpeg).unwrap();
        assert_eq!(metadata.text(NS_EXIF, "ThumbnailLength"), Some("6"));
        metadata.set_text(NS_EXIF, "Make", "Changed");
        let rewritten = write_exif(&jpeg, &metadata).unwrap();
        let payload = app1_payload(&rewritten);
        let doc = parse_doc(&payload).unwrap();
        let ifd1 = &doc.chain[0];
        let offset = entry_u32(ifd1, TAG_THUMBNAIL_OFFSET, true).unwrap() as usize;
        let length = entry_u32(ifd1, TAG_THUMBNAIL_LENGTH, true).unwrap() as usize;
        assert_eq!(&payload[offset..offset + length], &leaf);
    }

    #[test]
    fn rejects_non_exif_containers() {
        assert!(matches!(
            write_exif(b"not an image", &Metadata::new()),
            Err(MetadataError::UnsupportedWriteTarget(_))
        ));
    }
}
