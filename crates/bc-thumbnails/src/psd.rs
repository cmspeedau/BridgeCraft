//! Embedded Photoshop thumbnail extraction from PSD image resources.
//!
//! This parser reads only the PSD header, color-mode length and bounded image
//! resources; it does not decode or allocate the full-resolution composite.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use image::{ImageFormat, RgbImage};

use crate::{decoder, ThumbError, Thumbnail};

const RESOURCE_THUMBNAIL: u16 = 1033;
const RESOURCE_THUMBNAIL_PS5: u16 = 1036;
const MAX_RESOURCE_BYTES: usize = 256 * 1024 * 1024;
const MAX_PIXELS: u64 = 16_000_000;

/// True when the filename has the PSD extension.
pub fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("psd"))
}

/// Extract a bounded embedded RGB/JPEG preview and scale it to `max_edge`.
pub fn decode_thumbnail(path: &Path, max_edge: u32) -> Result<Thumbnail, ThumbError> {
    let display = path.display().to_string();
    let result = extract(path);
    result
        .map(|img| decoder::thumbnail_from_rgb(img, max_edge.max(1)))
        .map_err(|message| ThumbError::Psd {
            path: display,
            message,
        })
}

fn extract(path: &Path) -> Result<RgbImage, String> {
    let mut file = File::open(path).map_err(|e| format!("could not open PSD: {e}"))?;
    let mut header = [0u8; 26];
    file.read_exact(&mut header)
        .map_err(|e| format!("truncated PSD header: {e}"))?;
    if &header[..4] != b"8BPS" {
        return Err("invalid PSD signature".into());
    }
    if u16::from_be_bytes([header[4], header[5]]) != 1 {
        return Err("only PSD version 1 is supported (PSB is not supported)".into());
    }
    let mut color_length = [0u8; 4];
    file.read_exact(&mut color_length)
        .map_err(|e| format!("missing color-mode section length: {e}"))?;
    let color_length = read_u32(&color_length) as u64;
    file.seek(SeekFrom::Current(
        i64::try_from(color_length).map_err(|_| "color-mode section is too large")?,
    ))
    .map_err(|e| format!("could not seek past color-mode section: {e}"))?;
    let mut length = [0u8; 4];
    file.read_exact(&mut length)
        .map_err(|e| format!("missing image resources section: {e}"))?;
    let resource_len = read_u32(&length) as usize;
    if resource_len > MAX_RESOURCE_BYTES {
        return Err(format!(
            "image resources section exceeds {MAX_RESOURCE_BYTES} byte limit"
        ));
    }
    let mut resources = vec![0; resource_len];
    file.read_exact(&mut resources)
        .map_err(|e| format!("truncated image resources section: {e}"))?;
    parse_resources(&resources)
}

fn parse_resources(mut bytes: &[u8]) -> Result<RgbImage, String> {
    let mut thumbnail_error = None;
    while !bytes.is_empty() {
        if bytes.len() < 7 {
            return Err("truncated image-resource block header".into());
        }
        if &bytes[..4] != b"8BIM" {
            return Err("invalid image-resource signature".into());
        }
        let id = u16::from_be_bytes([bytes[4], bytes[5]]);
        let name_len = bytes[6] as usize;
        let padded_name_len = 1usize
            .checked_add(name_len)
            .and_then(|n| n.checked_add(n % 2))
            .ok_or("resource name length overflow")?;
        let len_offset = 6usize
            .checked_add(padded_name_len)
            .ok_or("resource header overflow")?;
        if bytes.len() < len_offset + 4 {
            return Err("truncated image-resource name or length".into());
        }
        let data_len = read_u32(&bytes[len_offset..len_offset + 4]) as usize;
        let data_start = len_offset + 4;
        let data_end = data_start
            .checked_add(data_len)
            .ok_or("resource length overflow")?;
        let padded_end = data_end
            .checked_add(data_len % 2)
            .ok_or("resource padding overflow")?;
        if padded_end > bytes.len() {
            return Err("truncated image-resource payload".into());
        }
        if id == RESOURCE_THUMBNAIL || id == RESOURCE_THUMBNAIL_PS5 {
            match parse_thumbnail(&bytes[data_start..data_end]) {
                Ok(image) => return Ok(image),
                Err(error) if thumbnail_error.is_none() => thumbnail_error = Some(error),
                Err(_) => {}
            }
        }
        bytes = &bytes[padded_end..];
    }
    Err(thumbnail_error
        .unwrap_or_else(|| "no supported image-resource thumbnail (IDs 1033/1036) found".into()))
}

fn parse_thumbnail(data: &[u8]) -> Result<RgbImage, String> {
    if data.len() < 28 {
        return Err("thumbnail resource header is truncated".into());
    }
    let format = read_u32(&data[0..4]);
    let width = read_u32(&data[4..8]);
    let height = read_u32(&data[8..12]);
    let row_bytes = read_u32(&data[12..16]) as usize;
    let total_size = read_u32(&data[16..20]) as usize;
    let compressed_size = read_u32(&data[20..24]) as usize;
    let bits = u16::from_be_bytes([data[24], data[25]]);
    let planes = u16::from_be_bytes([data[26], data[27]]);
    validate_dimensions(width, height)?;
    if planes != 1 {
        return Err(format!("thumbnail has unsupported plane count {planes}"));
    }
    let payload = &data[28..];
    match format {
        1 => {
            if compressed_size > payload.len() || compressed_size > MAX_RESOURCE_BYTES {
                return Err("JPEG preview length is invalid".into());
            }
            let mut reader = image::io::Reader::with_format(
                std::io::Cursor::new(&payload[..compressed_size]),
                ImageFormat::Jpeg,
            );
            let mut limits = image::io::Limits::default();
            limits.max_image_width = Some(width);
            limits.max_image_height = Some(height);
            limits.max_alloc = Some(64 * 1024 * 1024);
            reader.limits(limits);
            let decoded = reader
                .decode()
                .map_err(|e| format!("could not decode embedded JPEG preview: {e}"))?;
            let rgb = decoded.into_rgb8();
            if rgb.width() != width || rgb.height() != height {
                return Err("embedded JPEG dimensions do not match resource header".into());
            }
            Ok(rgb)
        }
        0 => {
            if bits != 24 {
                return Err(format!(
                    "uncompressed preview bit depth {bits} is unsupported (expected 24)"
                ));
            }
            let min_row = (width as usize)
                .checked_mul(3)
                .ok_or("preview row size overflow")?;
            if row_bytes < min_row {
                return Err("uncompressed preview row stride is too small".into());
            }
            let needed = row_bytes
                .checked_mul(height as usize)
                .ok_or("preview size overflow")?;
            if needed > MAX_RESOURCE_BYTES || total_size < needed || payload.len() < needed {
                return Err("uncompressed preview payload is truncated or oversized".into());
            }
            let mut rgb = Vec::with_capacity(
                min_row
                    .checked_mul(height as usize)
                    .ok_or("preview size overflow")?,
            );
            for row in payload[..needed].chunks_exact(row_bytes) {
                rgb.extend_from_slice(&row[..min_row]);
            }
            RgbImage::from_raw(width, height, rgb)
                .ok_or_else(|| "invalid uncompressed RGB preview buffer".into())
        }
        other => Err(format!(
            "thumbnail format {other} is unsupported (expected RGB or JPEG)"
        )),
    }
}

fn validate_dimensions(width: u32, height: u32) -> Result<(), String> {
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or("thumbnail dimensions overflow")?;
    if width == 0 || height == 0 || pixels > MAX_PIXELS {
        return Err(format!(
            "thumbnail dimensions {width}x{height} exceed limits"
        ));
    }
    Ok(())
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(id: u16, data: &[u8]) -> Vec<u8> {
        let mut out = b"8BIM".to_vec();
        out.extend_from_slice(&id.to_be_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(data);
        if !data.len().is_multiple_of(2) {
            out.push(0);
        }
        out
    }
    fn thumb_header(
        format: u32,
        w: u32,
        h: u32,
        row: u32,
        size: u32,
        comp: u32,
        bits: u16,
    ) -> Vec<u8> {
        let mut d = Vec::new();
        for n in [format, w, h, row, size, comp] {
            d.extend_from_slice(&n.to_be_bytes());
        }
        d.extend_from_slice(&bits.to_be_bytes());
        d.extend_from_slice(&1u16.to_be_bytes());
        d
    }
    fn psd(resources: &[u8]) -> Vec<u8> {
        let mut out = b"8BPS\0\x01".to_vec();
        out.extend_from_slice(&[0; 6]);
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&2u32.to_be_bytes());
        out.extend_from_slice(&2u32.to_be_bytes());
        out.extend_from_slice(&8u16.to_be_bytes());
        out.extend_from_slice(&3u16.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&(resources.len() as u32).to_be_bytes());
        out.extend_from_slice(resources);
        out
    }
    #[test]
    fn extracts_24_bit_rgb_resource_and_strips_row_padding() {
        let mut data = thumb_header(0, 2, 1, 8, 8, 0, 24);
        data.extend_from_slice(&[10, 20, 30, 40, 50, 60, 0, 0]);
        let rgb = parse_resources(&resource(1036, &data)).unwrap();
        assert_eq!(rgb.as_raw(), &[10, 20, 30, 40, 50, 60]);
    }
    #[test]
    fn extracts_embedded_jpeg_resource() {
        let source = RgbImage::from_pixel(2, 1, image::Rgb([70, 80, 90]));
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(source)
            .write_to(&mut jpeg, ImageFormat::Jpeg)
            .unwrap();
        let jpeg = jpeg.into_inner();
        let mut data = thumb_header(1, 2, 1, 6, jpeg.len() as u32, jpeg.len() as u32, 24);
        data.extend_from_slice(&jpeg);
        assert_eq!(
            parse_resources(&resource(1033, &data))
                .unwrap()
                .dimensions(),
            (2, 1)
        );
    }
    #[test]
    fn rejects_missing_malformed_and_oversized_preview() {
        assert!(parse_resources(&[]).unwrap_err().contains("no supported"));
        assert!(parse_resources(b"8BIM").is_err());
        assert!(validate_dimensions(20_000, 20_000).is_err());
        let mut data = thumb_header(0, 1, 1, 2, 2, 0, 24);
        data.extend_from_slice(&[0, 0]);
        assert!(parse_resources(&resource(1033, &data)).is_err());
    }
    #[test]
    fn reads_psd_file_resource_and_downscales() {
        let data = thumb_header(0, 2, 1, 6, 6, 0, 24);
        let mut data = data;
        data.extend_from_slice(&[255, 0, 0, 0, 255, 0]);
        let bytes = psd(&resource(1033, &data));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.psd");
        std::fs::write(&path, bytes).unwrap();
        let thumb = decode_thumbnail(&path, 1).unwrap();
        assert_eq!((thumb.width, thumb.height), (1, 1));
        assert!(thumb.is_consistent());
    }
}
