//! Batch export pipeline (issue #15): decode, fit, orient, and encode
//! selected files as a new format. The queued background execution lives in
//! [`crate::jobs`]; everything here is synchronous and headless so it is
//! unit-testable without a UI.
//!
//! Decoding and downscaling reuse the thumbnail pipeline
//! ([`bc_thumbnails::decoder`]) — resize math is never duplicated. Formats
//! in are everything the thumbnail pipeline decodes; camera RAW, PDF, video
//! and AI/PSD previews therefore export at *thumbnail quality* (the decode
//! path is shared), which is a documented limitation until full-tier
//! decoders land. Formats out: JPEG (quality knob), PNG and WebP.
//!
//! Metadata policy: JPEG exports carry the asset's rating, colour label,
//! approve/reject flag and keywords as an embedded XMP packet ([`carry_tags`]).
//! Capture data (EXIF `Make`/`Model`/`Software`/dates) deliberately does not
//! travel: an export is a downscaled re-encode, so copying exposure tags would
//! claim a provenance the output does not have. PNG and WebP exports carry no
//! tags, because this build has no packet writer for those containers and the
//! policy forbids scattering surprise sidecars; the dialog says so before the
//! job starts.
//! The other piece of metadata that is honoured is EXIF orientation: it is
//! applied to the pixel data rather than copied as a tag, so exports look right.

use std::path::{Path, PathBuf};

use image::{imageops, ColorType, ImageEncoder, RgbaImage};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ExportError {
    #[error("invalid export request: {0}")]
    InvalidRequest(String),

    #[error("cannot export {0}: the file is missing, not a regular file, or in a format BridgeCraft cannot decode")]
    Unsupported(String),

    #[error("decode failed: {0}")]
    Decode(String),

    #[error("encode failed: {0}")]
    Encode(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, ExportError>;

/// Accepted long-edge range for exports: below 16 px is a mis-click, above
/// 16384 px decodes can exceed sane memory on a workstation.
pub const MIN_MAX_EDGE: u32 = 16;
pub const MAX_MAX_EDGE: u32 = 16_384;

/// Output encoding for an export job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// `quality` is the JPEG quality knob, clamped to 1..=100. JPEG has no
    /// alpha channel: transparency is flattened onto white.
    Jpeg {
        quality: u8,
    },
    Png,
    /// Lossless (VP8L) WebP. The `image` 0.24 WebP encoder's only
    /// non-deprecated mode is lossless, which keeps alpha but trades file
    /// size; documented in the README.
    WebP,
}

impl ExportFormat {
    pub fn extension(&self) -> &'static str {
        match self {
            // JPEG conventionally uses .jpg so exports sort next to shots.
            ExportFormat::Jpeg { .. } => "jpg",
            ExportFormat::Png => "png",
            ExportFormat::WebP => "webp",
        }
    }

    pub fn label(&self) -> String {
        match self {
            ExportFormat::Jpeg { quality } => format!("JPEG q{quality}"),
            ExportFormat::Png => "PNG".to_string(),
            ExportFormat::WebP => "WebP (lossless)".to_string(),
        }
    }

    pub fn has_quality_knob(&self) -> bool {
        matches!(self, ExportFormat::Jpeg { .. })
    }
}

/// A validated-ready-to-submit batch export request.
#[derive(Debug, Clone)]
pub struct ExportRequest {
    /// Files to export; folders are rejected per file, not per batch.
    pub sources: Vec<PathBuf>,
    /// `Some(dir)` writes every output into that existing folder; `None`
    /// writes each output beside its original.
    pub destination: Option<PathBuf>,
    pub format: ExportFormat,
    /// Longest output edge in pixels; sources smaller than this are not
    /// upscaled (the shared fit helper only downscales).
    pub max_edge: u32,
}

impl ExportRequest {
    /// Batch-level validation: things that mean "the job cannot run at all".
    /// Per-file problems (unreadable sources, name collisions) are handled
    /// by the queue as individual failures instead.
    pub fn validate(&self) -> Result<()> {
        if self.sources.is_empty() {
            return Err(ExportError::InvalidRequest("no files selected".to_string()));
        }
        if !(MIN_MAX_EDGE..=MAX_MAX_EDGE).contains(&self.max_edge) {
            return Err(ExportError::InvalidRequest(format!(
                "max edge must be {MIN_MAX_EDGE}..={MAX_MAX_EDGE} pixels"
            )));
        }
        if let Some(dir) = &self.destination {
            let meta = std::fs::metadata(dir).map_err(|_| {
                ExportError::InvalidRequest(format!("destination folder does not exist: {dir:?}"))
            })?;
            if !meta.is_dir() {
                return Err(ExportError::InvalidRequest(format!(
                    "destination is not a folder: {dir:?}"
                )));
            }
        }
        Ok(())
    }
}

/// Worker-pool default: half the logical cores, at least one.
pub fn default_worker_count() -> usize {
    std::thread::available_parallelism()
        .map(|cores| (cores.get() / 2).max(1))
        .unwrap_or(1)
}

/// Decode `path` for export: shared thumbnail decode fitted to `max_edge`,
/// with EXIF orientation baked into the pixels.
///
/// Orientation is only consulted for formats routed through the plain
/// `image` decoder (which ignores the tag). Camera RAW arrives pre-oriented
/// by LibRaw (see `bc-thumbnails::raw`), and applying the tag again would
/// double-rotate it.
pub fn decode_for_export(path: &Path, max_edge: u32) -> Result<RgbaImage> {
    if !bc_thumbnails::decoder::can_thumbnail(path) {
        return Err(ExportError::Unsupported(path.display().to_string()));
    }
    let thumb = bc_thumbnails::decoder::decode_thumbnail(path, max_edge)
        .map_err(|e| ExportError::Decode(format!("{}: {e}", path.display())))?;
    let (width, height, pixels) = (thumb.width, thumb.height, thumb.pixels);
    let image = RgbaImage::from_raw(width, height, pixels).ok_or_else(|| {
        ExportError::Decode(format!(
            "{}: decoder returned {width}×{height} pixel data of an unexpected size",
            path.display()
        ))
    })?;
    let oriented = if bc_thumbnails::decoder::is_supported(path) {
        match bc_metadata::exif::read_exif_file(path) {
            Ok(meta) => match bc_metadata::exif::orientation_code(&meta) {
                Some(code) if code != 1 => apply_orientation(image, code),
                _ => image,
            },
            // Metadata problems must not fail the pixel export.
            Err(e) => {
                tracing::debug!(
                    "export: orientation read failed for {}: {e}",
                    path.display()
                );
                image
            }
        }
    } else {
        image
    };
    Ok(oriented)
}

/// Rotate/flip pixel data so it displays upright for EXIF orientation
/// codes 1..=8 (TIFF/EP publication values).
pub fn apply_orientation(image: RgbaImage, code: u16) -> RgbaImage {
    match code {
        2 => imageops::flip_horizontal(&image),
        3 => imageops::rotate180(&image),
        4 => imageops::flip_vertical(&image),
        // 5: transpose = rotate 90 CW then mirror horizontally.
        5 => imageops::flip_horizontal(&imageops::rotate90(&image)),
        6 => imageops::rotate90(&image),
        // 7: transverse = rotate 90 CW then mirror vertically.
        7 => imageops::flip_vertical(&imageops::rotate90(&image)),
        8 => imageops::rotate270(&image),
        _ => image,
    }
}

/// Encode RGBA pixels into the container bytes for `format`.
/// The tag properties that travel with an export, and nothing else.
///
/// Rating, colour label, approve/reject flag and keywords are judgements the
/// user made *about the asset*, so an export that loses them is a worse file
/// than the one in the library. Capture data deliberately does not travel with
/// this rule: EXIF `Make`/`Model`/`Software`/`DateTimeOriginal` describe the
/// original exposure, and the exported pixels are a downscaled re-encode, so
/// copying those tags would imply a provenance that is not true of the output.
/// Copying them correctly means rewriting an EXIF tree and is its own change.
pub const EXPORTED_TAG_PROPERTIES: &[(&str, &str)] = &[
    (bc_metadata::NS_XMP, "Rating"),
    (bc_metadata::NS_PHOTOSHOP, "Label"),
    (bc_metadata::NS_DC, "subject"),
    (bc_metadata::NS_BRIDGECRAFT, "Flag"),
];

/// Carry the source file's tags into `encoded`, when the output format can
/// hold them.
///
/// JPEG takes a real embedded XMP packet. PNG and WebP have no packet writer
/// in this build, so those bytes are returned unchanged rather than having a
/// surprise `.xmp` file created beside them, which is the "no auto-sidecar"
/// rule from `docs/design/0005-structured-metadata-model.md` section 4.
///
/// Failing to read the source never fails the export: an asset browser that
/// refuses to write a picture because one tag layer is corrupt would be worse
/// than one that writes a picture without tags. The reason is logged, and the
/// dialog states the rule up front so the outcome is not a surprise.
pub fn carry_tags(source: &Path, encoded: Vec<u8>, format: ExportFormat) -> Vec<u8> {
    if !matches!(format, ExportFormat::Jpeg { .. }) {
        return encoded;
    }
    let layers = match bc_metadata::layers::read_file(source) {
        Ok(layers) => layers,
        Err(error) => {
            tracing::warn!("export: reading tags from {}: {error}", source.display());
            return encoded;
        }
    };

    let mut carried = bc_metadata::Metadata::new();
    for (namespace, name) in EXPORTED_TAG_PROPERTIES {
        if let Some(value) = layers.get(namespace, name) {
            carried.set(*namespace, *name, value.clone());
        }
    }
    if carried.is_empty() {
        // Nothing to write, and writing an empty packet would add a segment
        // that says the file has no tags.
        return encoded;
    }

    match bc_metadata::xmp::write_embedded_xmp(&encoded, &carried) {
        Ok(with_packet) => with_packet,
        Err(error) => {
            tracing::warn!(
                "export: could not attach tags to {}: {error}",
                source.display()
            );
            encoded
        }
    }
}

/// Whether this output format carries the tag properties, for the dialog to
/// say plainly before the user commits to a job.
pub fn format_carries_tags(format: ExportFormat) -> bool {
    matches!(format, ExportFormat::Jpeg { .. })
}

pub fn encode_image(image: &RgbaImage, format: ExportFormat) -> Result<Vec<u8>> {
    let (width, height) = image.dimensions();
    let mut buffer: Vec<u8> = Vec::new();
    let encoded = {
        let writer = std::io::BufWriter::new(&mut buffer);
        match format {
            ExportFormat::Jpeg { quality } => {
                let flat = flatten_onto_white(image);
                let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
                    writer,
                    quality.clamp(1, 100),
                );
                encoder.write_image(flat.as_raw(), width, height, ColorType::Rgb8)
            }
            ExportFormat::Png => {
                let encoder = image::codecs::png::PngEncoder::new(writer);
                encoder.write_image(image.as_raw(), width, height, ColorType::Rgba8)
            }
            ExportFormat::WebP => {
                let encoder = image::codecs::webp::WebPEncoder::new_lossless(writer);
                encoder.write_image(image.as_raw(), width, height, ColorType::Rgba8)
            }
        }
        // The encoder owns the BufWriter and flushes it on drop at the end
        // of this match, which is also when `buffer` becomes movable again.
    };
    encoded
        .map_err(|e| ExportError::Encode(format!("{format:?} encode at {width}×{height}: {e}")))?;
    Ok(buffer)
}

/// Composite RGBA over white and drop alpha, for JPEG output.
fn flatten_onto_white(image: &RgbaImage) -> image::RgbImage {
    let mut out = image::RgbImage::new(image.width(), image.height());
    for (x, y, pixel) in image.enumerate_pixels() {
        let a = pixel[3] as u16;
        let blend = |c: u8| -> u8 {
            // c * a/255 + 255 * (1 - a/255), rounded, computed in u16.
            (((c as u16 * a) + (255 * (255 - a)) + 127) / 255) as u8
        };
        out.put_pixel(
            x,
            y,
            image::Rgb([blend(pixel[0]), blend(pixel[1]), blend(pixel[2])]),
        );
    }
    out
}

/// Pick `basename_MAXEDGE.ext` in `folder`, or the same with a `_1`, `_2`,
/// … collision suffix. `taken` answers whether a candidate path is already
/// spoken for (on disk and/or reserved by a running job); the export queue
/// folds both into one check so two concurrent jobs never plan the same
/// name. Returns `None` when the source has no usable stem or no free
/// suffix exists within 1000 tries.
pub fn unique_export_name(
    folder: &Path,
    source: &Path,
    max_edge: u32,
    format: ExportFormat,
    mut taken: impl FnMut(&Path) -> bool,
) -> Option<PathBuf> {
    let stem = source.file_stem()?.to_str()?;
    if stem.is_empty() || stem.starts_with('.') {
        return None;
    }
    let ext = format.extension();
    let base = folder.join(format!("{stem}_{max_edge}.{ext}"));
    if !taken(&base) {
        return Some(base);
    }
    for suffix in 1..=1000u32 {
        let candidate = folder.join(format!("{stem}_{max_edge}_{suffix}.{ext}"));
        if !taken(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
pub(crate) mod fixtures {
    use std::path::Path;

    pub(crate) fn write_test_png(path: &Path, w: u32, h: u32) {
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(w, h, |x, y| {
            image::Rgba([(x * 3 % 256) as u8, (y * 5 % 256) as u8, 90, 255])
        }));
        img.save(path).unwrap();
    }

    /// A real JPEG with an APP1 EXIF header declaring the given orientation,
    /// assembled from an encoder-written JPEG plus a hand-built minimal
    /// little-endian TIFF (same technique as the bc-metadata fixtures).
    pub(crate) fn write_oriented_jpeg(path: &Path, w: u32, h: u32, orientation: u16) {
        let raw = {
            let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(w, h, |x, y| {
                // Non-square, non-symmetric content so rotation is visible.
                let r = if x < w / 2 { 220 } else { 20 };
                let g = if y < h / 2 { 220 } else { 20 };
                image::Rgba([r, g, 40, 255])
            }));
            let mut buf = std::io::Cursor::new(Vec::new());
            img.write_to(&mut buf, image::ImageFormat::Jpeg).unwrap();
            buf.into_inner()
        };
        let mut tiff = Vec::new();
        tiff.extend(b"II*\0");
        tiff.extend(8u32.to_le_bytes()); // IFD0 at offset 8
        tiff.extend(1u16.to_le_bytes()); // one entry
        tiff.extend(0x0112u16.to_le_bytes()); // Orientation
        tiff.extend(3u16.to_le_bytes()); // SHORT
        tiff.extend(1u32.to_le_bytes()); // count
        tiff.extend(orientation.to_le_bytes());
        tiff.extend([0, 0]); // padding to 4-byte value
        tiff.extend(0u32.to_le_bytes()); // no next IFD, no EXIF sub-IFD
        let mut out = vec![0xFF, 0xD8];
        out.extend([0xFF, 0xE1]);
        out.extend(((tiff.len() + 6 + 2) as u16).to_be_bytes());
        out.extend(b"Exif\0\0");
        out.extend(&tiff);
        out.extend(&raw[2..]); // rest of the encoder-written JPEG
        std::fs::write(path, out).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::write_test_png;
    use super::*;

    #[test]
    fn validate_rejects_empty_bad_edge_and_missing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.png");
        write_test_png(&file, 8, 8);

        let base = ExportRequest {
            sources: vec![file.clone()],
            destination: None,
            format: ExportFormat::Png,
            max_edge: 1024,
        };
        base.validate().unwrap();

        assert!(matches!(
            ExportRequest {
                sources: vec![],
                ..base.clone()
            }
            .validate(),
            Err(ExportError::InvalidRequest(_))
        ));
        assert!(matches!(
            ExportRequest {
                max_edge: 4,
                ..base.clone()
            }
            .validate(),
            Err(ExportError::InvalidRequest(_))
        ));
        assert!(matches!(
            ExportRequest {
                destination: Some(dir.path().join("nope")),
                ..base.clone()
            }
            .validate(),
            Err(ExportError::InvalidRequest(_))
        ));
        assert!(matches!(
            ExportRequest {
                destination: Some(file.clone()),
                ..base
            }
            .validate(),
            Err(ExportError::InvalidRequest(_))
        ));
    }

    #[test]
    fn decode_fits_max_edge_without_upscaling() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.png");
        write_test_png(&big, 300, 150);
        let img = decode_for_export(&big, 100).unwrap();
        assert_eq!((img.width(), img.height()), (100, 50));

        let small = dir.path().join("small.png");
        write_test_png(&small, 30, 20);
        let img = decode_for_export(&small, 1000).unwrap();
        assert_eq!((img.width(), img.height()), (30, 20));
    }

    #[test]
    fn decode_rejects_unsupported_and_truncated_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let text = dir.path().join("notes.txt");
        std::fs::write(&text, b"not an image").unwrap();
        assert!(matches!(
            decode_for_export(&text, 100),
            Err(ExportError::Unsupported(_))
        ));

        let broken = dir.path().join("broken.png");
        std::fs::write(&broken, b"\x89PNG truncated").unwrap();
        assert!(decode_for_export(&broken, 100).is_err());
    }

    #[test]
    fn orientation_transforms_match_exif_geometry() {
        // 3×1 with three distinct pixels: mirrors and the 180 rotation keep
        // the shape; 5..=8 must swap to 1×3.
        let make = || {
            RgbaImage::from_raw(
                3,
                1,
                vec![10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120],
            )
            .unwrap()
        };
        let src = make();

        let identity = apply_orientation(src.clone(), 1);
        assert_eq!(identity, src);

        let mirrored = apply_orientation(make(), 2);
        assert_eq!((mirrored.width(), mirrored.height()), (3, 1));
        assert_eq!(mirrored.get_pixel(0, 0).0, src.get_pixel(2, 0).0);

        let rotated = apply_orientation(make(), 3);
        assert_eq!((rotated.width(), rotated.height()), (3, 1));
        assert_eq!(rotated.get_pixel(0, 0).0, src.get_pixel(2, 0).0);

        let flipped = apply_orientation(make(), 4);
        assert_eq!(flipped.get_pixel(0, 0).0, src.get_pixel(0, 0).0);

        for code in [5u16, 6, 7, 8] {
            let out = apply_orientation(make(), code);
            assert_eq!((out.width(), out.height()), (1, 3), "code {code}");
        }

        // 6 is clockwise: the top-left pixel of a landscape shot moves to
        // the top-right of the portrait result.
        let cw = apply_orientation(make(), 6);
        assert_eq!(cw.get_pixel(0, 0).0, src.get_pixel(0, 0).0);
        // 8 is its inverse (counter-clockwise).
        let ccw = apply_orientation(make(), 8);
        assert_eq!(ccw.get_pixel(0, 2).0, src.get_pixel(0, 0).0);
        // Unknown codes pass pixels through untouched.
        assert_eq!(apply_orientation(make(), 99), src);
    }

    #[test]
    fn encoders_roundtrip_per_format() {
        let img = RgbaImage::from_fn(37, 11, |x, y| {
            image::Rgba([
                (x * 7) as u8,
                (y * 13) as u8,
                200,
                if x % 2 == 0 { 255 } else { 90 },
            ])
        });
        for format in [
            ExportFormat::Jpeg { quality: 85 },
            ExportFormat::Png,
            ExportFormat::WebP,
        ] {
            let bytes = encode_image(&img, format).unwrap();
            let decoded = image::load_from_memory(&bytes)
                .unwrap_or_else(|e| panic!("{format:?} output unreadable: {e}"))
                .into_rgba8();
            assert_eq!((decoded.width(), decoded.height()), (37, 11), "{format:?}");
            if !matches!(format, ExportFormat::Jpeg { .. }) {
                assert_eq!(decoded, img, "{format:?} must be lossless");
            }
        }
    }

    #[test]
    fn jpeg_flattens_alpha_onto_white() {
        let mut img = RgbaImage::from_pixel(2, 1, image::Rgba([0, 0, 0, 0]));
        img.put_pixel(1, 0, image::Rgba([0, 0, 0, 255]));
        let bytes = encode_image(&img, ExportFormat::Jpeg { quality: 100 }).unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap().into_rgb8();
        // Fully transparent black becomes white; opaque black stays black
        // (give JPEG lossy coding a small allowance even at q100).
        assert!(decoded.get_pixel(0, 0)[0] > 240);
        assert!(decoded.get_pixel(1, 0)[0] < 15);
    }

    #[test]
    fn names_follow_max_edge_and_take_collision_suffixes() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Shot.CR2");
        let png = ExportFormat::Png;

        let name = unique_export_name(dir.path(), &source, 1024, png, |_| false).unwrap();
        assert_eq!(name.file_name().unwrap(), "Shot_1024.png");

        // Existing disk file and a reserved sibling both push the suffix on.
        std::fs::write(dir.path().join("Shot_1024.png"), b"x").unwrap();
        let reserved = dir.path().join("Shot_1024_1.png");
        let name = unique_export_name(dir.path(), &source, 1024, png, |path| {
            path.exists() || path == reserved
        })
        .unwrap();
        assert_eq!(name.file_name().unwrap(), "Shot_1024_2.png");

        // A dot-only stem ("…") or a suffix-free run gives up cleanly.
        assert!(unique_export_name(dir.path(), Path::new("..."), 512, png, |_| false).is_none());
        assert!(
            unique_export_name(dir.path(), &source, 1024, png, |_| true).is_none(),
            "suffix search must terminate"
        );
    }

    #[test]
    fn default_worker_count_is_sane() {
        assert!((1..=64).contains(&default_worker_count()));
    }

    /// A real JPEG with orientation 1, so the pixel path is a plain copy.
    fn jpeg(dir: &std::path::Path, name: &str, w: u32, h: u32) -> PathBuf {
        let path = dir.join(name);
        fixtures::write_oriented_jpeg(&path, w, h, 1);
        path
    }

    #[test]
    fn a_jpeg_export_carries_the_rating_label_flag_and_keywords() {
        let dir = tempfile::tempdir().unwrap();
        let source = jpeg(dir.path(), "shot.jpg", 40, 24);

        // Tag the source the way the panel does: into the file itself.
        let mut tags = bc_metadata::Metadata::new();
        tags.set_rating(bc_metadata::ratings::Rating::new(4));
        tags.set_color_label(bc_metadata::ratings::ColorLabel::Green);
        tags.set_flag(bc_metadata::ratings::Flag::Approved);
        bc_metadata::keywords::add_keyword(&mut tags, "Clients|ACME");
        bc_metadata::keywords::add_keyword(&mut tags, "Selected");
        bc_metadata::xmp::write_embedded_xmp_file(&source, &tags).unwrap();
        assert_eq!(
            bc_metadata::layers::read_file(&source)
                .unwrap()
                .rating()
                .stars(),
            4,
            "the fixture itself must be tagged before testing the copy"
        );

        let image = decode_for_export(&source, 20).unwrap();
        let encoded = encode_image(&image, ExportFormat::Jpeg { quality: 80 }).unwrap();
        let out = dir.path().join("shot_20.jpg");
        std::fs::write(
            &out,
            carry_tags(&source, encoded, ExportFormat::Jpeg { quality: 80 }),
        )
        .unwrap();

        // Read the export back through the layer chain, not through the writer.
        let read = bc_metadata::layers::read_file(&out).unwrap();
        assert_eq!(read.rating().stars(), 4, "rating survived the export");
        assert_eq!(read.color_label(), bc_metadata::ratings::ColorLabel::Green);
        assert_eq!(read.flag(), bc_metadata::ratings::Flag::Approved);
        let mut subjects = bc_metadata::keywords::subjects(&read);
        subjects.sort();
        assert_eq!(
            subjects,
            vec!["Clients|ACME".to_string(), "Selected".to_string()],
            "the keyword hierarchy separator survives an export"
        );

        // And it is still a decodable picture at the size that was asked for.
        let decoded = image::load_from_memory(&std::fs::read(&out).unwrap()).unwrap();
        assert_eq!(decoded.width(), 20, "the export kept its requested size");
    }

    #[test]
    fn tags_that_live_in_a_sidecar_reach_the_exported_file() {
        // The chain decides what travels, not one container: a sidecar source
        // must be able to produce an embedded packet in the output.
        let dir = tempfile::tempdir().unwrap();
        let source = jpeg(dir.path(), "rawish.jpg", 30, 30);
        let mut side = bc_metadata::Metadata::new();
        side.set_rating(bc_metadata::ratings::Rating::new(5));
        bc_metadata::xmp::write_xmp_file(&bc_metadata::xmp::sidecar_path(&source), &side).unwrap();

        let image = decode_for_export(&source, 15).unwrap();
        let bytes = encode_image(&image, ExportFormat::Jpeg { quality: 90 }).unwrap();
        let out = dir.path().join("out.jpg");
        std::fs::write(
            &out,
            carry_tags(&source, bytes, ExportFormat::Jpeg { quality: 90 }),
        )
        .unwrap();
        assert_eq!(
            bc_metadata::layers::read_file(&out)
                .unwrap()
                .rating()
                .stars(),
            5,
            "a sidecar tag must not be lost by an export"
        );
    }

    #[test]
    fn png_and_webp_exports_gain_no_packet_and_no_stray_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("flat.png");
        fixtures::write_test_png(&source, 24, 18);
        let mut tags = bc_metadata::Metadata::new();
        tags.set_rating(bc_metadata::ratings::Rating::new(3));
        bc_metadata::xmp::write_xmp_file(&bc_metadata::xmp::sidecar_path(&source), &tags).unwrap();

        let image = decode_for_export(&source, 12).unwrap();
        for format in [ExportFormat::Png, ExportFormat::WebP] {
            let encoded = encode_image(&image, format).unwrap();
            let carried = carry_tags(&source, encoded.clone(), format);
            assert_eq!(encoded, carried, "{format:?} must be returned untouched");
            let out = dir.path().join(format!("export.{}", format.extension()));
            std::fs::write(&out, &carried).unwrap();
            // No sidecar is created beside an export: that is exactly the
            // surprise-file behaviour design/0005 section 4 forbids.
            assert!(
                !bc_metadata::xmp::sidecar_path(&out).exists(),
                "{format:?} export grew an unexpected sidecar"
            );
            assert!(image::load_from_memory(&std::fs::read(&out).unwrap()).is_ok());
        }
        assert!(!format_carries_tags(ExportFormat::Png));
        assert!(!format_carries_tags(ExportFormat::WebP));
        assert!(format_carries_tags(ExportFormat::Jpeg { quality: 80 }));
    }

    #[test]
    fn an_untagged_source_produces_an_export_with_no_empty_packet() {
        let dir = tempfile::tempdir().unwrap();
        let source = jpeg(dir.path(), "plain.jpg", 20, 20);
        let image = decode_for_export(&source, 10).unwrap();
        let encoded = encode_image(&image, ExportFormat::Jpeg { quality: 80 }).unwrap();
        let carried = carry_tags(&source, encoded.clone(), ExportFormat::Jpeg { quality: 80 });
        assert_eq!(
            encoded, carried,
            "nothing to carry must not add a packet that says the file has no tags"
        );
    }

    #[test]
    fn unreadable_source_metadata_never_loses_the_picture() {
        // The export already committed to writing an image; a tag layer that
        // cannot be read must not cost the user their picture.
        let dir = tempfile::tempdir().unwrap();
        let source = jpeg(dir.path(), "good.jpg", 16, 16);
        let image = decode_for_export(&source, 8).unwrap();
        let encoded = encode_image(&image, ExportFormat::Jpeg { quality: 80 }).unwrap();

        // A source that is gone: reading its layers is an error.
        let gone = dir.path().join("deleted.jpg");
        assert!(bc_metadata::layers::read_file(&gone).is_err());
        let carried = carry_tags(&gone, encoded.clone(), ExportFormat::Jpeg { quality: 80 });
        assert_eq!(
            carried, encoded,
            "an unreadable source must return the encoded pixels unchanged"
        );
        assert!(image::load_from_memory(&carried).is_ok());
    }

    #[test]
    fn capture_data_is_not_claimed_as_the_export_s_provenance() {
        // The whitelist is a decision, so pin it: camera and tool tags from
        // the source must NOT travel, while the user's own rating does.
        let dir = tempfile::tempdir().unwrap();
        let source = jpeg(dir.path(), "camera.jpg", 32, 24);
        let mut tags = bc_metadata::Metadata::new();
        tags.set_text(bc_metadata::NS_EXIF, "Make", "Canon");
        tags.set_text(bc_metadata::NS_EXIF, "Model", "R5");
        tags.set_text(bc_metadata::NS_XMP, "CreatorTool", "Some Camera App");
        tags.set_rating(bc_metadata::ratings::Rating::new(2));
        bc_metadata::xmp::write_embedded_xmp_file(&source, &tags).unwrap();

        let image = decode_for_export(&source, 16).unwrap();
        let bytes = encode_image(&image, ExportFormat::Jpeg { quality: 80 }).unwrap();
        let out = dir.path().join("out.jpg");
        std::fs::write(
            &out,
            carry_tags(&source, bytes, ExportFormat::Jpeg { quality: 80 }),
        )
        .unwrap();

        let read = bc_metadata::layers::read_file(&out).unwrap();
        assert_eq!(read.rating().stars(), 2, "the asset judgement travels");
        assert!(
            read.text(bc_metadata::NS_EXIF, "Make").is_none(),
            "camera make must not be asserted about a re-encode"
        );
        assert!(read.text(bc_metadata::NS_EXIF, "Model").is_none());
        assert!(read.text(bc_metadata::NS_XMP, "CreatorTool").is_none());
    }
}
