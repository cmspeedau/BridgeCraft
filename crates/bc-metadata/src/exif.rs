//! Pure-Rust EXIF reader for JPEG (`APP1` + `Exif\0\0`) and TIFF-family
//! files (TIFF, DNG and similar EXIF-native containers).
//!
//! Clean-room: implemented from the openly published Adobe TIFF 6.0 and
//! CIPA DC-X004 (EXIF) specifications only — no third-party, copied or
//! decompiled code (see CLEANROOM.md and issue #6).
//!
//! Values are projected into the interim flat [`Metadata`] map (the
//! structured model is issue #5) under stable `exif.*` / `gps.*` keys the
//! UI can rely on. Structurally broken input yields
//! [`MetadataError::Malformed`]; individual unresolvable tag payloads are
//! skipped rather than failing the whole read, and parsing never panics.

use std::collections::{HashSet, VecDeque};
use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::{Metadata, MetadataError, Reader};

/// Upper bound on bytes [`read_exif_file`] buffers. EXIF structures live at
/// the head of the file; tag payloads beyond the cap are skipped.
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// Reject implausibly large IFDs instead of trusting attacker counts.
const MAX_ENTRIES: usize = 1024;
/// Top-level chain + sub-IFD nesting cap (thumbnails are not followed).
const MAX_IFDS: usize = 8;

// TIFF/EXIF tag ids we surface (values are spec constants, not code).
const TAG_MAKE: u16 = 0x010F;
const TAG_MODEL: u16 = 0x0110;
const TAG_ORIENTATION: u16 = 0x0112;
const TAG_SOFTWARE: u16 = 0x0131;
const TAG_DATE_TIME: u16 = 0x0132;
const TAG_IMAGE_WIDTH: u16 = 0x0100;
const TAG_IMAGE_HEIGHT: u16 = 0x0101;
const TAG_THUMBNAIL_OFFSET: u16 = 0x0201;
const TAG_THUMBNAIL_LENGTH: u16 = 0x0202;
const TAG_EXIF_IFD: u16 = 0x8769;
const TAG_GPS_IFD: u16 = 0x8825;
const TAG_EXPOSURE_TIME: u16 = 0x829A;
const TAG_FNUMBER: u16 = 0x829D;
const TAG_ISO: u16 = 0x8827;
const TAG_DATE_TIME_ORIGINAL: u16 = 0x9003;
const TAG_FOCAL_LENGTH: u16 = 0x920A;
const TAG_COLOR_SPACE: u16 = 0xA001;
const TAG_PIXEL_X: u16 = 0xA002;
const TAG_PIXEL_Y: u16 = 0xA003;
// GPS IFD tags.
const TAG_GPS_LAT_REF: u16 = 0x0001;
const TAG_GPS_LAT: u16 = 0x0002;
const TAG_GPS_LON_REF: u16 = 0x0003;
const TAG_GPS_LON: u16 = 0x0004;
const TAG_GPS_ALT_REF: u16 = 0x0005;
const TAG_GPS_ALT: u16 = 0x0006;

/// EXIF reader behind the crate's [`Reader`] trait.
#[derive(Debug, Default)]
pub struct ExifReader;

impl Reader for ExifReader {
    fn read(&self, data: &[u8]) -> Result<Metadata, MetadataError> {
        read_exif(data)
    }
}

/// Read EXIF from in-memory JPEG or TIFF-family bytes.
///
/// A well-formed image without an EXIF block returns an empty map; only
/// structurally broken containers error.
pub fn read_exif(data: &[u8]) -> Result<Metadata, MetadataError> {
    if data.starts_with(&[0xFF, 0xD8]) {
        let tiff = match find_app1_exif(data)? {
            Some(tiff) => tiff,
            None => return Ok(Metadata::new()),
        };
        parse_tiff(tiff)
    } else if has_tiff_header(data) {
        parse_tiff(data)
    } else {
        Err(MetadataError::Unsupported)
    }
}

/// Read EXIF from a file, buffering at most [`MAX_FILE_BYTES`] plus a
/// marker slack (EXIF lives at the file head).
pub fn read_exif_file(path: &Path) -> Result<Metadata, MetadataError> {
    let file = File::open(path)?;
    let mut buf = Vec::new();
    file.take(MAX_FILE_BYTES + 64)
        .read_to_end(&mut buf)
        .map_err(MetadataError::Io)?;
    read_exif(&buf)
}

fn has_tiff_header(data: &[u8]) -> bool {
    data.len() >= 8 && (data.starts_with(b"II*\0") || data.starts_with(b"MM\0*"))
}

/// Locate the TIFF block inside a JPEG's first EXIF `APP1` segment.
///
/// `Ok(None)` means "valid JPEG, no EXIF". Marker-structure violations are
/// `Malformed`.
fn find_app1_exif(data: &[u8]) -> Result<Option<&[u8]>, MetadataError> {
    let mut pos = 2usize; // skip SOI
    while pos + 4 <= data.len() {
        if data[pos] != 0xFF {
            return Err(MetadataError::Malformed(
                "JPEG marker sync lost".to_string(),
            ));
        }
        let marker = data[pos + 1];
        if marker == 0xFF {
            pos += 1; // fill byte
            continue;
        }
        if marker == 0xDA || marker == 0xD9 {
            // Start of scan / EOI: no EXIF found.
            return Ok(None);
        }
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            pos += 2; // standalone markers carry no length
            continue;
        }
        let size = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        if size < 2 {
            return Err(MetadataError::Malformed(
                "zero-length JPEG segment".to_string(),
            ));
        }
        let body_start = pos + 4;
        let body_len = size - 2;
        let body_end = body_start
            .checked_add(body_len)
            .ok_or_else(|| MetadataError::Malformed("JPEG segment length overflow".to_string()))?;
        if body_end > data.len() {
            return Err(MetadataError::Malformed(
                "JPEG segment extends past the data".to_string(),
            ));
        }
        let body = &data[body_start..body_end];
        if marker == 0xE1 && body.starts_with(b"Exif\0\0") {
            let tiff = &body[b"Exif\0\0".len()..];
            if tiff.is_empty() {
                return Err(MetadataError::Malformed("empty EXIF payload".to_string()));
            }
            return Ok(Some(tiff));
        }
        pos = body_end;
    }
    Ok(None)
}

/// One IFD entry with its value bytes already located (bounds-checked).
struct Entry<'a> {
    tag: u16,
    type_id: u16,
    value: &'a [u8],
}

struct Tiff<'a> {
    data: &'a [u8],
    le: bool,
    out: Metadata,
    visited: HashSet<usize>,
}

fn type_size(type_id: u16) -> Option<usize> {
    match type_id {
        1 | 2 | 6 | 7 => Some(1), // BYTE, ASCII, SBYTE, UNDEFINED
        3 | 8 => Some(2),         // SHORT, SSHORT
        4 | 9 | 11 => Some(4),    // LONG, SLONG, FLOAT
        5 | 10 | 12 => Some(8),   // RATIONAL, SRATIONAL, DOUBLE
        _ => None,
    }
}

impl<'a> Tiff<'a> {
    fn new(data: &'a [u8]) -> Option<Self> {
        if data.len() < 8 {
            return None;
        }
        let le = data.starts_with(b"II");
        let magic = if le {
            u16::from_le_bytes([data[2], data[3]])
        } else {
            u16::from_be_bytes([data[2], data[3]])
        };
        if magic != 42 {
            return None;
        }
        Some(Self {
            data,
            le,
            out: Metadata::new(),
            visited: HashSet::new(),
        })
    }

    fn u16(&self, at: usize) -> Option<u16> {
        let b: [u8; 2] = self.data.get(at..at + 2)?.try_into().ok()?;
        Some(if self.le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    }

    fn u32(&self, at: usize) -> Option<u32> {
        let b: [u8; 4] = self.data.get(at..at + 4)?.try_into().ok()?;
        Some(if self.le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    /// Parse one IFD into entries; structural violations are `Malformed`.
    fn read_ifd(&self, offset: usize) -> Result<(Vec<Entry<'a>>, Option<usize>), MetadataError> {
        if offset + 2 > self.data.len() {
            return Err(MetadataError::Malformed(
                "IFD offset past the end of the data".to_string(),
            ));
        }
        let count = self
            .u16(offset)
            .ok_or_else(|| MetadataError::Malformed("truncated IFD count".to_string()))?
            as usize;
        if count > MAX_ENTRIES {
            return Err(MetadataError::Malformed(format!(
                "IFD claims {count} entries (cap {MAX_ENTRIES})"
            )));
        }
        let table_start = offset + 2;
        let table_end = table_start + count * 12;
        if table_end > self.data.len() {
            return Err(MetadataError::Malformed(
                "IFD entry table extends past the data".to_string(),
            ));
        }
        let next = if table_end + 4 <= self.data.len() {
            match self.u32(table_end) {
                Some(n) if n != 0 => Some(n as usize),
                _ => None,
            }
        } else {
            None
        };

        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            let at = table_start + i * 12;
            let tag = match self.u16(at) {
                Some(t) => t,
                None => continue,
            };
            let type_id = match self.u16(at + 2) {
                Some(t) => t,
                None => continue,
            };
            let count_v = self.u32(at + 4).unwrap_or(0);
            // One unreadable tag should not cost the whole file: skip the
            // entry rather than failing the read.
            let Some(elem) = type_size(type_id) else {
                continue;
            };
            let Some(size) = elem.checked_mul(count_v as usize) else {
                continue;
            };
            if size > 16 * 1024 * 1024 {
                continue; // absurd count: skip
            }
            // Values up to 4 bytes live inline; longer ones are at an offset.
            let value: &[u8] = if size <= 4 {
                match self.data.get(at + 8..at + 8 + size) {
                    Some(v) => v,
                    None => continue,
                }
            } else {
                match self.u32(at + 8) {
                    Some(off) => match off as usize..(off as usize + size) {
                        r if r.end <= self.data.len() => &self.data[r],
                        _ => continue, // payload past the buffer: skip the tag
                    },
                    None => continue,
                }
            };
            entries.push(Entry {
                tag,
                type_id,
                value,
            });
        }
        Ok((entries, next))
    }

    fn ascii(entry: &Entry<'_>) -> Option<String> {
        if entry.type_id != 2 {
            return None;
        }
        let text = String::from_utf8_lossy(entry.value);
        let text = text.trim_matches('\0').trim().to_string();
        (!text.is_empty()).then_some(text)
    }

    fn shorts(&self, entry: &Entry<'_>) -> Vec<u16> {
        if entry.type_id != 3 {
            return Vec::new();
        }
        entry
            .value
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| {
                if self.le {
                    u16::from_le_bytes(*b)
                } else {
                    u16::from_be_bytes(*b)
                }
            })
            .collect()
    }

    fn longs(&self, entry: &Entry<'_>) -> Vec<u32> {
        if entry.type_id != 4 {
            return Vec::new();
        }
        entry
            .value
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| {
                if self.le {
                    u32::from_le_bytes(*b)
                } else {
                    u32::from_be_bytes(*b)
                }
            })
            .collect()
    }

    /// RATIONAL values as (numerator, denominator) pairs, zero denominators
    /// dropped so formatting cannot divide by zero.
    fn rationals(&self, entry: &Entry<'_>) -> Vec<(u32, u32)> {
        if entry.type_id != 5 {
            return Vec::new();
        }
        entry
            .value
            .as_chunks::<8>()
            .0
            .iter()
            .filter_map(|b| {
                let num = if self.le {
                    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
                } else {
                    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
                };
                let den = if self.le {
                    u32::from_le_bytes([b[4], b[5], b[6], b[7]])
                } else {
                    u32::from_be_bytes([b[4], b[5], b[6], b[7]])
                };
                (den != 0).then_some((num, den))
            })
            .collect()
    }

    /// Walk IFD0, the top-level chain (thumbnail IFDs), the Exif sub-IFD and
    /// the GPS sub-IFD. Loops or over-deep nesting anywhere are `Malformed`.
    fn parse(mut self) -> Result<Metadata, MetadataError> {
        let ifd0_off = self
            .u32(4)
            .ok_or_else(|| MetadataError::Malformed("truncated TIFF header".to_string()))?
            as usize;
        if !self.visited.insert(ifd0_off) {
            return Err(MetadataError::Malformed("IFD loop detected".to_string()));
        }
        let (ifd0, next) = self.read_ifd(ifd0_off)?;
        self.merge_ifd0(&ifd0);

        // Follow the top-level chain: IFD1+ carry the embedded thumbnail in
        // JPEG/TIFF files. Its location is surfaced, never decoded (the UI
        // keeps its own thumbnail cache). Loops are Malformed; depth capped.
        let mut next = next;
        let mut chain_hops = 0;
        while let Some(off) = next {
            if chain_hops >= MAX_IFDS {
                break;
            }
            chain_hops += 1;
            if !self.visited.insert(off) {
                return Err(MetadataError::Malformed("IFD loop detected".to_string()));
            }
            let (entries, following) = self.read_ifd(off)?;
            self.merge_chain(&entries);
            next = following;
        }

        let mut queue: VecDeque<(usize, bool)> = VecDeque::new();
        for entry in &ifd0 {
            match entry.tag {
                TAG_EXIF_IFD => {
                    if let Some(off) = self.longs(entry).first().copied() {
                        queue.push_back((off as usize, true));
                    }
                }
                TAG_GPS_IFD => {
                    if let Some(off) = self.longs(entry).first().copied() {
                        queue.push_back((off as usize, false));
                    }
                }
                _ => {}
            }
        }
        let mut seen = 1usize;
        while let Some((off, is_exif)) = queue.pop_front() {
            if seen >= MAX_IFDS {
                break;
            }
            seen += 1;
            if !self.visited.insert(off) {
                return Err(MetadataError::Malformed("IFD loop detected".to_string()));
            }
            let (entries, _) = self.read_ifd(off)?;
            if is_exif {
                self.merge_exif(&entries);
            } else {
                self.merge_gps(&entries);
            }
        }
        Ok(self.out)
    }

    fn merge_ifd0(&mut self, entries: &[Entry<'_>]) {
        for entry in entries {
            match entry.tag {
                TAG_MAKE => {
                    if let Some(v) = Self::ascii(entry) {
                        self.out.insert("exif.Make".into(), v);
                    }
                }
                TAG_MODEL => {
                    if let Some(v) = Self::ascii(entry) {
                        self.out.insert("exif.Model".into(), v);
                    }
                }
                TAG_SOFTWARE => {
                    if let Some(v) = Self::ascii(entry) {
                        self.out.insert("exif.Software".into(), v);
                    }
                }
                TAG_DATE_TIME => {
                    if let Some(v) = Self::ascii(entry) {
                        self.out
                            .insert("exif.DateTime".into(), normalize_exif_date(&v));
                    }
                }
                TAG_ORIENTATION => {
                    if let Some(&n) = self.shorts(entry).first() {
                        self.out
                            .insert("exif.Orientation".into(), orientation_text(n));
                    }
                }
                _ => {}
            }
        }
    }

    /// IFD1+ from the top-level chain: surface the embedded thumbnail's
    /// location (per TIFF 6.0 these are LONGs); nothing else is read.
    fn merge_chain(&mut self, entries: &[Entry<'_>]) {
        for entry in entries {
            let key = match entry.tag {
                TAG_IMAGE_WIDTH => "exif.ThumbnailWidth",
                TAG_IMAGE_HEIGHT => "exif.ThumbnailHeight",
                TAG_THUMBNAIL_OFFSET => "exif.ThumbnailOffset",
                TAG_THUMBNAIL_LENGTH => "exif.ThumbnailLength",
                _ => continue,
            };
            if let Some(&n) = self.longs(entry).first() {
                self.out.insert(key.to_string(), n.to_string());
            }
        }
    }

    fn merge_exif(&mut self, entries: &[Entry<'_>]) {
        for entry in entries {
            match entry.tag {
                TAG_EXPOSURE_TIME => {
                    if let Some(&(n, d)) = self.rationals(entry).first() {
                        self.out
                            .insert("exif.ExposureTime".into(), format_exposure(n, d));
                    }
                }
                TAG_FNUMBER => {
                    if let Some(&(n, d)) = self.rationals(entry).first() {
                        self.out.insert(
                            "exif.FNumber".into(),
                            format!("f/{:.1}", n as f64 / d as f64),
                        );
                    }
                }
                TAG_ISO => {
                    let values = self.shorts(entry);
                    if !values.is_empty() {
                        self.out.insert(
                            "exif.ISOSpeedRatings".into(),
                            values
                                .iter()
                                .map(u16::to_string)
                                .collect::<Vec<_>>()
                                .join(", "),
                        );
                    }
                }
                TAG_DATE_TIME_ORIGINAL => {
                    if let Some(v) = Self::ascii(entry) {
                        self.out
                            .insert("exif.DateTimeOriginal".into(), normalize_exif_date(&v));
                    }
                }
                TAG_FOCAL_LENGTH => {
                    if let Some(&(n, d)) = self.rationals(entry).first() {
                        self.out.insert(
                            "exif.FocalLength".into(),
                            format!("{:.1} mm", n as f64 / d as f64),
                        );
                    }
                }
                TAG_COLOR_SPACE => {
                    if let Some(&n) = self.shorts(entry).first() {
                        self.out
                            .insert("exif.ColorSpace".into(), color_space_text(n));
                    }
                }
                TAG_PIXEL_X => {
                    if let Some(&n) = self.longs(entry).first() {
                        self.out.insert("exif.PixelWidth".into(), n.to_string());
                    }
                }
                TAG_PIXEL_Y => {
                    if let Some(&n) = self.longs(entry).first() {
                        self.out.insert("exif.PixelHeight".into(), n.to_string());
                    }
                }
                _ => {}
            }
        }
    }

    fn merge_gps(&mut self, entries: &[Entry<'_>]) {
        let mut lat_ref = None;
        let mut lat = None;
        let mut lon_ref = None;
        let mut lon = None;
        let mut alt_ref = 0u16;
        let mut alt = None;
        for entry in entries {
            match entry.tag {
                TAG_GPS_LAT_REF => lat_ref = Self::ascii(entry),
                TAG_GPS_LAT => lat = dms_degrees(&self.rationals(entry)),
                TAG_GPS_LON_REF => lon_ref = Self::ascii(entry),
                TAG_GPS_LON => lon = dms_degrees(&self.rationals(entry)),
                TAG_GPS_ALT_REF => alt_ref = self.shorts(entry).first().copied().unwrap_or(0),
                // Altitude is a single RATIONAL of metres, not a DMS triple.
                TAG_GPS_ALT => {
                    alt = self
                        .rationals(entry)
                        .first()
                        .map(|&(n, d)| n as f64 / d as f64)
                }
                _ => {}
            }
        }
        if let (Some(v), Some(r)) = (lat, lat_ref) {
            self.out
                .insert("gps.Latitude".into(), format!("{v:.5} {r}"));
        }
        if let (Some(v), Some(r)) = (lon, lon_ref) {
            self.out
                .insert("gps.Longitude".into(), format!("{v:.5} {r}"));
        }
        if let Some(v) = alt {
            let sign = if alt_ref == 1 { "-" } else { "" };
            self.out
                .insert("gps.Altitude".into(), format!("{sign}{v:.1} m"));
        }
    }
}

fn parse_tiff(data: &[u8]) -> Result<Metadata, MetadataError> {
    match Tiff::new(data) {
        Some(tiff) => tiff.parse(),
        None => Err(MetadataError::Malformed(
            "missing or bad TIFF header".to_string(),
        )),
    }
}

/// EXIF stamps dates as `YYYY:MM:DD hh:mm:ss`; panels prefer ISO-ish dashes.
fn normalize_exif_date(raw: &str) -> String {
    let bytes = raw.as_bytes();
    if bytes.len() >= 10 && bytes[4] == b':' && bytes[7] == b':' {
        let mut out = String::with_capacity(raw.len());
        out.push_str(&raw[..4]);
        out.push('-');
        out.push_str(&raw[5..7]);
        out.push('-');
        out.push_str(&raw[8..]);
        return out;
    }
    raw.to_string()
}

fn orientation_text(n: u16) -> String {
    let label = match n {
        1 => "Normal",
        2 => "Mirror horizontal",
        3 => "Rotate 180",
        4 => "Mirror vertical",
        5 => "Mirror horizontal + rotate 90 CW",
        6 => "Rotate 90 CW",
        7 => "Mirror horizontal + rotate 270 CW",
        8 => "Rotate 270 CW",
        _ => "Unknown",
    };
    format!("{label} ({n})")
}

fn color_space_text(n: u16) -> String {
    match n {
        1 => "sRGB",
        2 => "Uncalibrated",
        _ => "Reserved",
    }
    .to_string()
}

/// Shutter speed as a human string: `1/400 s`, or seconds when >= 1 s.
fn format_exposure(num: u32, den: u32) -> String {
    let seconds = num as f64 / den as f64;
    if seconds <= 0.0 {
        return "0 s".to_string();
    }
    if seconds >= 1.0 {
        format!("{:.2} s", seconds)
    } else {
        format!("1/{} s", (1.0 / seconds).round().max(1.0) as i64)
    }
}

/// Three RATIONAL parts of degrees/minutes/seconds as decimal degrees.
fn dms_degrees(parts: &[(u32, u32)]) -> Option<f64> {
    if parts.len() != 3 {
        return None;
    }
    let deg = parts[0].0 as f64 / parts[0].1 as f64;
    let min = parts[1].0 as f64 / parts[1].1 as f64;
    let sec = parts[2].0 as f64 / parts[2].1 as f64;
    if !deg.is_finite() || !min.is_finite() || !sec.is_finite() || min >= 60.0 || sec >= 60.0 {
        return None;
    }
    Some(deg + min / 60.0 + sec / 3600.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------- fixture builder --

    /// Build the file with exact offsets: heap region laid out first, then
    /// GPS IFD, Exif IFD, IFD0, each pointing into known offsets.
    fn layout_and_parse(le: bool) -> Vec<u8> {
        let heap_at = 8usize;

        // Heap payloads (offsets assigned in this order).
        let make = b"Bridge Test Cam\0";
        let model = b"BTC Z1\0";
        let software = b"bc-metadata-tests\0";
        let dt_file = b"2014:10:18 12:20:00\0";
        let dt_original = b"2014:10:18 12:23:43\0";
        let exposure = vec![(1u32, 400u32)];
        let fnumber = vec![(71u32, 10u32)];
        let focal = vec![(105u32, 1u32)];
        let lat = vec![(47u32, 1u32), (36u32, 1u32), (2236u32, 100u32)];
        let lon = vec![(122u32, 1u32), (19u32, 1u32), (1200u32, 100u32)];

        let mut heap_bytes = Vec::new();
        let mut offsets = Vec::new();
        for payload in [
            heap_payload_str(make),
            heap_payload_str(model),
            heap_payload_str(software),
            heap_payload_str(dt_file),
            heap_payload_str(dt_original),
            heap_payload_rats(&exposure, le),
            heap_payload_rats(&fnumber, le),
            heap_payload_rats(&focal, le),
            heap_payload_rats(&lat, le),
            heap_payload_rats(&lon, le),
        ] {
            offsets.push(heap_at + heap_bytes.len());
            heap_bytes.extend(payload);
        }
        let [make_off, model_off, software_off, dt_file_off, dt_orig_off, exposure_off, fnumber_off, focal_off, lat_off, lon_off] =
            <[usize; 10]>::try_from(offsets).unwrap();

        let gps_off = heap_at + heap_bytes.len();
        let gps_entries: Vec<(u16, u16, u32, Vec<u8>)> = vec![
            (TAG_GPS_LAT_REF, 2, 2, b"N\0".to_vec()),
            (TAG_GPS_LAT, 5, 3, u32_le(lat_off as u32, le)),
            (TAG_GPS_LON_REF, 2, 2, b"W\0".to_vec()),
            (TAG_GPS_LON, 5, 3, u32_le(lon_off as u32, le)),
        ];
        let gps_bytes = build_ifd(&gps_entries, le, None, gps_off);

        let exif_off = gps_off + gps_bytes.len();
        let exif_entries: Vec<(u16, u16, u32, Vec<u8>)> = vec![
            (TAG_EXPOSURE_TIME, 5, 1, u32_le(exposure_off as u32, le)),
            (TAG_FNUMBER, 5, 1, u32_le(fnumber_off as u32, le)),
            (TAG_ISO, 3, 1, u16_le(2000, le)),
            (
                TAG_DATE_TIME_ORIGINAL,
                2,
                dt_original.len() as u32,
                u32_le(dt_orig_off as u32, le),
            ),
            (TAG_FOCAL_LENGTH, 5, 1, u32_le(focal_off as u32, le)),
            (TAG_COLOR_SPACE, 3, 1, u16_le(1, le)),
            (TAG_PIXEL_X, 4, 1, u32_le(5184, le)),
            (TAG_PIXEL_Y, 4, 1, u32_le(3456, le)),
        ];
        let exif_bytes = build_ifd(&exif_entries, le, None, exif_off);

        let ifd0_off = exif_off + exif_bytes.len();
        let ifd0_entries: Vec<(u16, u16, u32, Vec<u8>)> = vec![
            (TAG_MAKE, 2, make.len() as u32, u32_le(make_off as u32, le)),
            (
                TAG_MODEL,
                2,
                model.len() as u32,
                u32_le(model_off as u32, le),
            ),
            (TAG_ORIENTATION, 3, 1, u16_le(6, le)),
            (
                TAG_SOFTWARE,
                2,
                software.len() as u32,
                u32_le(software_off as u32, le),
            ),
            (
                TAG_DATE_TIME,
                2,
                dt_file.len() as u32,
                u32_le(dt_file_off as u32, le),
            ),
            (TAG_EXIF_IFD, 4, 1, u32_le(exif_off as u32, le)),
            (TAG_GPS_IFD, 4, 1, u32_le(gps_off as u32, le)),
        ];
        // IFD1 hangs off IFD0's `next` and points at a thumbnail. Its offset
        // is known upfront because every entry is exactly 12 bytes.
        let ifd0_len = 2 + ifd0_entries.len() * 12 + 4;
        let ifd1_off = ifd0_off + ifd0_len;
        let ifd1_entries: Vec<(u16, u16, u32, Vec<u8>)> = vec![
            (TAG_IMAGE_WIDTH, 4, 1, u32_le(320, le)),
            (TAG_IMAGE_HEIGHT, 4, 1, u32_le(240, le)),
            (TAG_THUMBNAIL_OFFSET, 4, 1, u32_le(4096, le)),
            (TAG_THUMBNAIL_LENGTH, 4, 1, u32_le(8192, le)),
        ];
        let ifd1_bytes = build_ifd(&ifd1_entries, le, None, ifd1_off);
        let ifd0_bytes = build_ifd(&ifd0_entries, le, Some(ifd1_off as u32), ifd0_off);

        let mut file = Vec::new();
        file.extend(if le { b"II*\0" } else { b"MM\0*" });
        file.extend(u32_le(ifd0_off as u32, le));
        file.extend(heap_bytes);
        file.extend(gps_bytes);
        file.extend(exif_bytes);
        file.extend(ifd0_bytes);
        file.extend(ifd1_bytes);
        file
    }

    fn heap_payload_str(s: &[u8]) -> Vec<u8> {
        s.to_vec()
    }

    fn heap_payload_rats(rats: &[(u32, u32)], le: bool) -> Vec<u8> {
        let mut v = Vec::new();
        for (n, d) in rats {
            v.extend(u32_le(*n, le));
            v.extend(u32_le(*d, le));
        }
        v
    }

    fn u16_le(v: u16, le: bool) -> Vec<u8> {
        if le {
            v.to_le_bytes().to_vec()
        } else {
            v.to_be_bytes().to_vec()
        }
    }

    fn u32_le(v: u32, le: bool) -> Vec<u8> {
        if le {
            v.to_le_bytes().to_vec()
        } else {
            v.to_be_bytes().to_vec()
        }
    }

    /// Entries must be sorted by tag; the 4-byte `value_or_offset` is the
    /// caller-encoded payload; `self_off` positions nothing (entries hold
    /// offsets only, never self references).
    fn build_ifd(
        entries: &[(u16, u16, u32, Vec<u8>)],
        le: bool,
        next: Option<u32>,
        _self_off: usize,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(u16_le(entries.len() as u16, le));
        for (tag, type_id, count, value) in entries {
            debug_assert!(value.len() <= 4);
            out.extend(u16_le(*tag, le));
            out.extend(u16_le(*type_id, le));
            out.extend(u32_le(*count, le));
            let mut field = [0u8; 4];
            field[..value.len()].copy_from_slice(value);
            out.extend(field);
        }
        out.extend(u32_le(next.unwrap_or(0), le));
        out
    }

    // ------------------------------------------------------------ tests ---

    #[test]
    fn reads_a_hand_built_little_endian_tiff() {
        let file = layout_and_parse(true);
        let meta = read_exif(&file).unwrap();
        assert_eq!(
            meta.get("exif.Make").map(String::as_str),
            Some("Bridge Test Cam")
        );
        assert_eq!(meta.get("exif.Model").map(String::as_str), Some("BTC Z1"));
        assert_eq!(
            meta.get("exif.Orientation").map(String::as_str),
            Some("Rotate 90 CW (6)")
        );
        assert_eq!(
            meta.get("exif.ExposureTime").map(String::as_str),
            Some("1/400 s")
        );
        assert_eq!(meta.get("exif.FNumber").map(String::as_str), Some("f/7.1"));
        assert_eq!(
            meta.get("exif.ISOSpeedRatings").map(String::as_str),
            Some("2000")
        );
        assert_eq!(
            meta.get("exif.DateTimeOriginal").map(String::as_str),
            Some("2014-10-18 12:23:43")
        );
        assert_eq!(
            meta.get("exif.PixelWidth").map(String::as_str),
            Some("5184")
        );
        assert_eq!(
            meta.get("exif.ColorSpace").map(String::as_str),
            Some("sRGB")
        );
        assert_eq!(
            meta.get("exif.ThumbnailWidth").map(String::as_str),
            Some("320")
        );
        assert_eq!(
            meta.get("exif.ThumbnailOffset").map(String::as_str),
            Some("4096")
        );
        assert_eq!(
            meta.get("gps.Latitude").map(String::as_str),
            Some("47.60621 N")
        );
        assert_eq!(
            meta.get("gps.Longitude").map(String::as_str),
            Some("122.32000 W")
        );
    }

    #[test]
    fn reads_big_endian_tiff() {
        let file = layout_and_parse(false);
        let meta = read_exif(&file).unwrap();
        assert_eq!(
            meta.get("exif.Make").map(String::as_str),
            Some("Bridge Test Cam")
        );
        assert_eq!(
            meta.get("exif.DateTime").map(String::as_str),
            Some("2014-10-18 12:20:00")
        );
        assert_eq!(
            meta.get("exif.FocalLength").map(String::as_str),
            Some("105.0 mm")
        );
    }

    #[test]
    fn reads_exif_wrapped_in_a_jpeg_app1() {
        let tiff = layout_and_parse(true);
        let mut jpeg = vec![0xFF, 0xD8]; // SOI
        jpeg.push(0xFF);
        jpeg.push(0xE1); // APP1
        let body_len = (tiff.len() + 6 + 2) as u16;
        jpeg.extend(body_len.to_be_bytes());
        jpeg.extend(b"Exif\0\0");
        jpeg.extend(&tiff);
        jpeg.extend([0xFF, 0xD9]); // EOI
        let meta = read_exif(&jpeg).unwrap();
        assert_eq!(meta.get("exif.Model").map(String::as_str), Some("BTC Z1"));
    }

    #[test]
    fn plain_jpeg_without_exif_is_empty_not_an_error() {
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xDA, 0, 0, 0, 0];
        assert!(read_exif(&jpeg).unwrap().is_empty());
    }

    #[test]
    fn truncated_and_malformed_inputs_error_without_panicking() {
        let tiff = layout_and_parse(true);
        for cut in [0usize, 2, 4, 6, 9, 16, 24, 40, 64, tiff.len() - 1] {
            // Whatever the verdict, the parser must survive every truncation.
            let verdict = std::panic::catch_unwind(|| read_exif(&tiff[..cut]));
            assert!(verdict.is_ok(), "parser panicked on {cut}-byte input");
        }
        assert!(matches!(
            read_exif(b"hello, definitely not an image"),
            Err(MetadataError::Unsupported)
        ));
        // IFD offset pointing past the buffer.
        let mut bogus = tiff.clone();
        bogus[4..8].copy_from_slice(&(tiff.len() as u32 + 1000).to_le_bytes());
        assert!(matches!(
            read_exif(&bogus),
            Err(MetadataError::Malformed(_))
        ));
        // Bad TIFF magic: no longer a TIFF-family container at all.
        let mut magic = tiff.clone();
        magic[2..4].copy_from_slice(&99u16.to_le_bytes());
        assert!(matches!(read_exif(&magic), Err(MetadataError::Unsupported)));
    }

    #[test]
    fn cyclic_exif_sub_ifd_is_malformed() {
        let tiff = layout_and_parse(true);
        // Find the ExifIFD pointer entry (tag 0x8769 LE) and repoint it at
        // IFD0's own offset, creating a cycle.
        let pos = tiff
            .windows(2)
            .rposition(|w| w == 0x8769u16.to_le_bytes())
            .expect("ExifIFD entry present");
        let mut cyclic = tiff.clone();
        let value_field = pos + 8; // entry: tag(2) type(2) count(4) value(4)
        let ifd0_off = u32::from_le_bytes([tiff[4], tiff[5], tiff[6], tiff[7]]);
        cyclic[value_field..value_field + 4].copy_from_slice(&ifd0_off.to_le_bytes());
        assert!(matches!(
            read_exif(&cyclic),
            Err(MetadataError::Malformed(_))
        ));
    }

    #[test]
    fn absurd_entry_counts_are_capped() {
        let mut tiff = layout_and_parse(true);
        let ifd0_off = u32::from_le_bytes([tiff[4], tiff[5], tiff[6], tiff[7]]) as usize;
        tiff[ifd0_off..ifd0_off + 2].copy_from_slice(&0xFFFFu16.to_le_bytes());
        assert!(matches!(read_exif(&tiff), Err(MetadataError::Malformed(_))));
    }

    /// Real-world check against a camera file when one is provided.
    #[test]
    fn reads_exif_from_a_real_camera_file_when_available() {
        let Some(sample) = std::env::var_os("RAW_SAMPLE") else {
            eprintln!("RAW_SAMPLE not set; skipping real-camera EXIF check");
            return;
        };
        let path = Path::new(&sample);
        let meta = read_exif_file(path).expect("camera RAW must expose EXIF");
        assert!(
            meta.iter()
                .any(|(k, _)| k == "exif.Make" || k == "exif.Model"),
            "expected camera make/model EXIF in {}: {meta:?}",
            path.display()
        );
    }
}
