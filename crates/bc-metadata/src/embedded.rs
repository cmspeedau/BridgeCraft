//! Embedded XMP and IPTC packets inside JPEG and TIFF containers.
//!
//! Roadmap Phase 3 ("embedded XMP", "TIFF IPTC container"). Reading is split
//! out from writing on purpose: `bc-metadata` already writes EXIF in place and
//! XMP to sidecars, and moving tag writes into the file is a storage-policy
//! decision (`docs/design/0005-structured-metadata-model.md` §4 is still
//! unsigned off). This module only *finds* what another application put in the
//! file, so the panel can show it.
//!
//! Clean-room: located from the public packing descriptions in XMP
//! Specification Part 3 (JPEG `APP1` carrying the `http://ns.adobe.com/xap/1.0/`
//! signature; TIFF/DNG tag 700), TIFF 6.0 (tag 33723, IPTC/NAA) and the
//! Photoshop image-resource block convention (TIFF tag 34377, JPEG `APP13` with
//! the `Photoshop 3.0` signature). No proprietary code was read or translated.

use crate::MetadataError;

/// Signature that opens an XMP packet inside a JPEG `APP1` segment.
pub const XMP_JPEG_SIGNATURE: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
/// Signature that opens a Photoshop resource block in JPEG `APP13` / TIFF 34377.
pub const PHOTOSHOP_SIGNATURE: &[u8] = b"Photoshop 3.0\0";
/// TIFF tag carrying an embedded XMP packet.
pub const TAG_XMP: u16 = 700;
/// TIFF 6.0 tag carrying a raw IPTC-IIM dataset ("IPTC/NAA").
pub const TAG_IPTC_NAA: u16 = 33723;
/// TIFF tag carrying the Photoshop image-resource block ("Photoshop 3.0").
pub const TAG_PHOTOSHOP: u16 = 34377;

/// Guards against a hostile IFD: entries, pages and offsets are all bounded.
const MAX_ENTRIES: usize = 65_535;
const MAX_PAGES: usize = 16;

fn malformed(msg: &str) -> MetadataError {
    MetadataError::Malformed(msg.to_string())
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

fn size_of_type(type_id: u16) -> Option<usize> {
    match type_id {
        1 | 2 | 6 | 7 => Some(1),
        3 | 8 => Some(2),
        4 | 9 | 11 => Some(4),
        5 | 10 | 12 => Some(8),
        _ => None,
    }
}

/// True for JPEG (SOI) bytes.
pub fn is_jpeg(data: &[u8]) -> bool {
    data.starts_with(&[0xFF, 0xD8])
}

/// True for classic TIFF/EXIF bytes (little or big endian, version 42).
pub fn is_tiff(data: &[u8]) -> bool {
    data.len() >= 8
        && (data.starts_with(b"II") || data.starts_with(b"MM"))
        && read_u16(data, 2, data.starts_with(b"II")) == Some(42)
}

/// The bytes of one TIFF tag's value, resolved from an inline field or an
/// offset, with the file's bounds checked at every hop.
///
/// Returns `Ok(None)` when the tag is simply absent, and `Err(Malformed)` for
/// structurally broken input rather than panicking on it.
pub fn tiff_tag_bytes(data: &[u8], want: u16) -> Result<Option<Vec<u8>>, MetadataError> {
    if !is_tiff(data) {
        return Ok(None);
    }
    let le = data.starts_with(b"II");
    let mut page =
        read_u32(data, 4, le).ok_or_else(|| malformed("truncated TIFF header"))? as usize;
    let mut seen_pages = 0usize;

    while page != 0 {
        seen_pages += 1;
        if seen_pages > MAX_PAGES {
            return Err(malformed("too many TIFF pages"));
        }
        let count =
            read_u16(data, page, le).ok_or_else(|| malformed("truncated IFD count"))? as usize;
        if count > MAX_ENTRIES {
            return Err(malformed("absurd IFD entry count"));
        }
        let entries_start = page
            .checked_add(2)
            .ok_or_else(|| malformed("IFD offset overflow"))?;
        let entries_end = entries_start
            .checked_add(count * 12)
            .ok_or_else(|| malformed("IFD size overflow"))?;
        if entries_end > data.len() {
            return Err(malformed("IFD extends past the data"));
        }

        for index in 0..count {
            let at = entries_start + index * 12;
            let tag = read_u16(data, at, le).ok_or_else(|| malformed("truncated IFD entry"))?;
            let type_id =
                read_u16(data, at + 2, le).ok_or_else(|| malformed("truncated IFD entry"))?;
            let count_value = read_u32(data, at + 4, le)
                .ok_or_else(|| malformed("truncated IFD entry"))?
                as usize;
            let unit = size_of_type(type_id).ok_or_else(|| {
                MetadataError::UnsupportedType(format!("TIFF field type {type_id}"))
            })?;
            let total = count_value
                .checked_mul(unit)
                .ok_or_else(|| malformed("IFD value size overflow"))?;
            if total > data.len() {
                return Err(malformed("IFD value larger than the file"));
            }

            if tag != want {
                continue;
            }

            // Values of four bytes or fewer live in the offset field itself.
            let bytes = if total <= 4 {
                let start = at + 8;
                let end = start
                    .checked_add(total)
                    .ok_or_else(|| malformed("inline value overflow"))?;
                data.get(start..end)
                    .ok_or_else(|| malformed("truncated inline value"))?
                    .to_vec()
            } else {
                let offset = read_u32(data, at + 8, le)
                    .ok_or_else(|| malformed("truncated IFD value offset"))?
                    as usize;
                let end = offset
                    .checked_add(total)
                    .ok_or_else(|| malformed("IFD value offset overflow"))?;
                data.get(offset..end)
                    .ok_or_else(|| malformed("IFD value points past the data"))?
                    .to_vec()
            };
            return Ok(Some(bytes));
        }

        let next = entries_end
            .checked_add(4)
            .ok_or_else(|| malformed("next IFD offset overflow"))?;
        page = if next <= data.len() {
            read_u32(data, entries_end, le).unwrap_or(0) as usize
        } else {
            0
        };
    }

    Ok(None)
}

/// Walk the JPEG marker list, handing each segment body to `inspect`.
///
/// Segment framing follows the public JPEG marker rules: `FF` + marker byte, a
/// two-byte big-endian length that counts itself, and standalone markers that
/// carry no length. Scan data (SOS and beyond) is not walked: metadata after
/// the entropy-coded data is not something these readers claim to find.
fn for_each_jpeg_segment(
    data: &[u8],
    mut inspect: impl FnMut(u8, &[u8]) -> Result<Option<Vec<u8>>, MetadataError>,
) -> Result<Option<Vec<u8>>, MetadataError> {
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
            break;
        }
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            pos += 2;
            continue;
        }
        let size = read_u16(data, pos + 2, false)
            .ok_or_else(|| malformed("truncated segment size"))? as usize;
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
        if let Some(found) = inspect(marker, &data[body_start..body_end])? {
            return Ok(Some(found));
        }
        pos = body_end;
    }
    Ok(None)
}

/// The XMP packet bytes from a JPEG `APP1` or a TIFF tag 700, if the file has one.
pub fn find_xmp_packet(data: &[u8]) -> Result<Option<Vec<u8>>, MetadataError> {
    if is_jpeg(data) {
        return for_each_jpeg_segment(data, |marker, body| {
            if marker == 0xE1 && body.starts_with(XMP_JPEG_SIGNATURE) {
                return Ok(Some(trim_xmp_tail(&body[XMP_JPEG_SIGNATURE.len()..])));
            }
            Ok(None)
        });
    }
    if is_tiff(data) {
        if let Some(bytes) = tiff_tag_bytes(data, TAG_XMP)? {
            return Ok(Some(trim_xmp_tail(&bytes)));
        }
    }
    Ok(None)
}

/// The Photoshop image-resource block from a JPEG `APP13` or a TIFF tag 34377.
pub fn find_photoshop_resources(data: &[u8]) -> Result<Option<Vec<u8>>, MetadataError> {
    if is_jpeg(data) {
        return for_each_jpeg_segment(data, |marker, body| {
            if marker == 0xED && body.starts_with(PHOTOSHOP_SIGNATURE) {
                return Ok(Some(body[PHOTOSHOP_SIGNATURE.len()..].to_vec()));
            }
            Ok(None)
        });
    }
    if is_tiff(data) {
        if let Some(bytes) = tiff_tag_bytes(data, TAG_PHOTOSHOP)? {
            let payload = bytes
                .strip_prefix(PHOTOSHOP_SIGNATURE)
                .unwrap_or(&bytes[..]);
            return Ok(Some(payload.to_vec()));
        }
    }
    Ok(None)
}

/// A raw IPTC-IIM dataset from TIFF tag 33723, for files that carry IIM
/// directly instead of inside a Photoshop resource block.
pub fn find_iptc_dataset(data: &[u8]) -> Result<Option<Vec<u8>>, MetadataError> {
    if is_tiff(data) {
        return tiff_tag_bytes(data, TAG_IPTC_NAA);
    }
    Ok(None)
}

/// XMP packets in a JPEG are padded out to a round size after the closing
/// `xpacket` instruction; drop that trailing padding so the parser sees XML.
fn trim_xmp_tail(bytes: &[u8]) -> Vec<u8> {
    let end = match memstr(bytes, b"<?xpacket end=") {
        Some(at) => match find_byte(&bytes[at..], b'>') {
            Some(inner) => at + inner + 1,
            None => bytes.len(),
        },
        None => bytes.len(),
    };
    bytes[..end].to_vec()
}

fn find_byte(haystack: &[u8], needle: u8) -> Option<usize> {
    haystack.iter().position(|byte| *byte == needle)
}

/// Subsequence search without pulling in a dependency.
fn memstr(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// An XMP packet with its BOM written as bytes, the way a real packet starts.
    fn packet() -> Vec<u8> {
        let mut bytes = b"<?xpacket begin='".to_vec();
        bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]); // UTF-8 BOM
        bytes.extend_from_slice(
            b"' id='W5M0MpCehiHzreSzNTczkc9d'><x:xmpmeta xmlns:x='adobe:ns:meta/'></x:xmpmeta><?xpacket end='w'?>",
        );
        bytes
    }

    fn jpeg_with(marker: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8];
        out.push(0xFF);
        out.push(marker);
        out.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x05, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xFF, 0xD9]);
        out
    }

    /// A one-page TIFF with a single tag whose value lives past the IFD.
    fn tiff_with(tag: u16, value: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"II");
        out.extend_from_slice(&42u16.to_le_bytes());
        out.extend_from_slice(&8u32.to_le_bytes()); // IFD0 right after the header
        out.extend_from_slice(&1u16.to_le_bytes()); // one entry
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // type BYTE
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        let value_offset = 8 + 2 + 12 + 4;
        out.extend_from_slice(&(value_offset as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // no next page
        out.extend_from_slice(value);
        out
    }

    #[test]
    fn finds_the_xmp_packet_in_a_jpeg_app1_and_ignores_the_exif_one() {
        let exif_app1 = jpeg_with(0xE1, b"Exif\0\0tiny-tiff-bytes");
        assert!(find_xmp_packet(&exif_app1).unwrap().is_none());

        let mut body = XMP_JPEG_SIGNATURE.to_vec();
        let packet = packet();
        body.extend_from_slice(&packet);
        body.extend_from_slice(&[b' '; 200]); // packets are padded in real files
        let found = find_xmp_packet(&jpeg_with(0xE1, &body)).unwrap().unwrap();
        assert!(found.starts_with(b"<?xpacket"));
        assert_eq!(
            found, packet,
            "trim stops at the closing instruction, not a byte later"
        );
    }

    #[test]
    fn finds_the_xmp_packet_in_tiff_tag_700() {
        let packet = packet();
        let file = tiff_with(TAG_XMP, &packet);
        let found = find_xmp_packet(&file).unwrap().unwrap();
        assert_eq!(found, packet);
    }

    #[test]
    fn finds_photoshop_resources_in_both_containers() {
        let mut body = PHOTOSHOP_SIGNATURE.to_vec();
        body.extend_from_slice(b"8BIMfake-resource-bytes");
        let jpeg = jpeg_with(0xED, &body);
        let found = find_photoshop_resources(&jpeg).unwrap().unwrap();
        assert_eq!(found, b"8BIMfake-resource-bytes".to_vec());

        let tiff = tiff_with(TAG_PHOTOSHOP, &body);
        let found = find_photoshop_resources(&tiff).unwrap().unwrap();
        assert_eq!(found, b"8BIMfake-resource-bytes".to_vec());
    }

    #[test]
    fn raw_iim_dataset_is_read_from_tiff_tag_33723() {
        // A record-2 dataset marker, big enough to live at an offset. A
        // four-byte value would be stored inline instead, and the reader must
        // follow the spec on that distinction.
        let dataset = vec![0x1c, 0x02, 0x00, 0x00, 0x1c, 0x02, 0x05, 0x00];
        let file = tiff_with(TAG_IPTC_NAA, &dataset);
        assert_eq!(find_iptc_dataset(&file).unwrap().unwrap(), dataset);
        // A JPEG has no such tag.
        assert!(find_iptc_dataset(&jpeg_with(0xE1, &packet()))
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_short_tiff_value_is_read_inline_not_as_an_offset() {
        // Two bytes of type SHORT stored in the value field itself.
        let mut file = Vec::new();
        file.extend_from_slice(b"II");
        file.extend_from_slice(&42u16.to_le_bytes());
        file.extend_from_slice(&8u32.to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&TAG_IPTC_NAA.to_le_bytes());
        file.extend_from_slice(&3u16.to_le_bytes()); // SHORT
        file.extend_from_slice(&2u32.to_le_bytes()); // two of them
        file.extend_from_slice(&[0xAA, 0xBB, 0x00, 0x00]); // inline, not an offset
        file.extend_from_slice(&0u32.to_le_bytes());
        let found = tiff_tag_bytes(&file, TAG_IPTC_NAA).unwrap().unwrap();
        assert_eq!(found, vec![0xAA, 0xBB, 0x00, 0x00]);
    }

    #[test]
    fn malformed_containers_are_rejected_not_panicked() {
        // IFD count claims more entries than the file holds.
        let mut broken = tiff_with(TAG_XMP, b"<x:xmpmeta/>");
        broken[8..10].copy_from_slice(&60_000u16.to_le_bytes());
        assert!(matches!(
            tiff_tag_bytes(&broken, TAG_XMP),
            Err(MetadataError::Malformed(_))
        ));

        // A segment whose declared length runs past the end of the file.
        let mut truncated = vec![0xFF, 0xD8, 0xFF, 0xE1, 0xFF, 0xFF];
        truncated.extend_from_slice(b"short");
        assert!(matches!(
            find_xmp_packet(&truncated),
            Err(MetadataError::Malformed(_))
        ));

        // Loss-of-sync marker stream.
        assert!(matches!(
            find_xmp_packet(&[0xFF, 0xD8, 0x00, 0x00, 0x00, 0x00]),
            Err(MetadataError::Malformed(_))
        ));
    }

    #[test]
    fn a_tag_value_that_claims_to_be_bigger_than_the_file_is_malformed() {
        let mut file = tiff_with(TAG_XMP, b"small");
        // Count field of the single entry: claim 4 GiB.
        file[8 + 2 + 4..8 + 2 + 8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            tiff_tag_bytes(&file, TAG_XMP),
            Err(MetadataError::Malformed(_))
        ));
    }

    /// Real-world check against a file written by another application.
    ///
    /// The fixtures above are built by this crate, so they can only prove the
    /// reader agrees with our own writer. Set `XMP_SAMPLE` to a JPEG or TIFF that
    /// some other tool saved with an XMP packet (Pillow, Darktable, ExifTool) and
    /// this asserts the packet is located and parsed anyway.
    #[test]
    fn reads_an_xmp_packet_written_by_another_application() {
        let Some(sample) = std::env::var_os("XMP_SAMPLE") else {
            eprintln!("XMP_SAMPLE not set; skipping third-party XMP check");
            return;
        };
        let path = Path::new(&sample);
        let bytes = std::fs::read(path).expect("sample must be readable");
        let packet = find_xmp_packet(&bytes)
            .unwrap()
            .unwrap_or_else(|| panic!("no embedded XMP packet found in {}", path.display()));
        let text = String::from_utf8_lossy(&packet);
        assert!(
            text.contains("x:xmpmeta"),
            "located bytes are not an XMP packet: {text}"
        );
        let meta = crate::xmp::parse_xmp(&packet).expect("packet must parse");
        eprintln!(
            "third-party XMP sample {} exposed {} properties",
            path.display(),
            meta.properties().len()
        );
        assert!(
            !meta.is_empty(),
            "the packet parsed to nothing: {}",
            path.display()
        );
    }
}
