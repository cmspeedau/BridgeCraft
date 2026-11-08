//! Export pipeline: resize/convert chosen files into a target directory.
//!
//! Per-file engine for issue #15. Decode and fit math are shared with the
//! thumbnail pipeline ([`crate::decoder::decode_thumbnail`] /
//! [`thumbnail_from_rgba`]) — no resize logic is duplicated here. Batching,
//! progress and cancellation live in the caller: run one [`export_file`] per
//! file inside a `bc-core::job_queue` job, cancelling between files.
//!
//! Guarantees:
//! - Output is named `basename_MAXEDGE.ext` with `-N` collision suffixes; an
//!   existing file is never overwritten silently.
//! - EXIF orientation is applied to pixels, not just copied.
//! - Writes are atomic: encode to a temp file in the destination directory,
//!   then rename, so a crash never leaves a truncated export.
//! - Camera RAW inputs go through the optional LibRaw runtime at thumbnail
//!   quality (documented trade-off from the thumbnail pipeline).

use std::io::BufWriter;
use std::path::{Path, PathBuf};

use crate::decoder::thumbnail_from_rgba;
use crate::ThumbError;

/// Output format for an export, with format-specific knobs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExportFormat {
    /// JPEG with 1–100 quality (clamped).
    Jpeg { quality: u8 },
    Png,
    WebP,
}

impl ExportFormat {
    pub fn extension(&self) -> &'static str {
        match self {
            ExportFormat::Jpeg { .. } => "jpg",
            ExportFormat::Png => "png",
            ExportFormat::WebP => "webp",
        }
    }
}

/// One export request: source file, destination directory, fit edge, format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportRequest {
    pub source: PathBuf,
    pub destination_dir: PathBuf,
    /// Longest output edge; aspect ratio is preserved.
    pub max_edge: u32,
    pub format: ExportFormat,
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn io_error(kind: std::io::ErrorKind, message: String) -> ThumbError {
    ThumbError::Io(std::io::Error::new(kind, message))
}

/// Resolve `basename_MAXEDGE.ext` (plus `-N` collision suffixes) inside the
/// destination directory, never returning an existing path.
fn pick_destination(
    source: &Path,
    destination_dir: &Path,
    max_edge: u32,
    format: &ExportFormat,
) -> Result<PathBuf, ThumbError> {
    let stem = source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| {
            io_error(
                std::io::ErrorKind::InvalidInput,
                format!("source has no usable file name: {}", source.display()),
            )
        })?;
    let ext = format.extension();
    let mut candidate = destination_dir.join(format!("{stem}_{max_edge}.{ext}"));
    if !exists(&candidate) {
        return Ok(candidate);
    }
    for n in 1..=9999u32 {
        candidate = destination_dir.join(format!("{stem}_{max_edge}-{n}.{ext}"));
        if !exists(&candidate) {
            return Ok(candidate);
        }
    }
    Err(io_error(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "no free collision suffix under {}",
            destination_dir.display()
        ),
    ))
}

/// Decode with EXIF orientation applied to the pixels, not just carried
/// along. Non-UTF8-decodable garbage surfaces as [`ThumbError::Decode`],
/// never a panic.
fn decode_oriented(path: &Path) -> Result<image::DynamicImage, ThumbError> {
    let display = path.display().to_string();
    let mut reader = image::ImageReader::open(path).map_err(ThumbError::Io)?;
    reader
        .guess_format()
        .map_err(|source| ThumbError::Decode { path: display.clone(), source })?;
    let orientation = reader
        .get_orientation()
        .map_err(|source| ThumbError::Decode { path: display.clone(), source })?;
    let image = reader
        .decode()
        .map_err(|source| ThumbError::Decode { path: display.clone(), source })?;
    Ok(image.apply_orientation(orientation))
}

/// Encode `image` into `destination` atomically enough for the caller: this
/// writes a fresh file; the caller renames it into place.
fn encode(
    image: &image::DynamicImage,
    format: &ExportFormat,
    destination: &Path,
) -> Result<(), ThumbError> {
    let file = std::fs::File::create(destination).map_err(ThumbError::Io)?;
    let mut writer = BufWriter::new(file);
    let encoded = match format {
        ExportFormat::Jpeg { quality } => {
            let mut encoder =
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, quality.clamp(1, 100));
            encoder.encode_image(image)
        }
        ExportFormat::Png => image.write_to(&mut writer, image::ImageFormat::Png),
        // `image` writes WebP losslessly; acceptable for the export preset.
        ExportFormat::WebP => image.write_to(&mut writer, image::ImageFormat::WebP),
    };
    encoded.map_err(|source| {
        io_error(
            std::io::ErrorKind::Other,
            format!("encode failed for {}: {source}", destination.display()),
        )
    })?;
    writer.flush().map_err(ThumbError::Io)?;
    Ok(())
}

/// Resize/convert one file per `request` and return the written path.
///
/// The destination directory is created if missing. Naming is
/// `basename_MAXEDGE.ext` with `-1`, `-2`, … suffixes on collision; nothing
/// existing is ever overwritten. On any error no output file is left behind.
pub fn export_file(request: &ExportRequest) -> Result<PathBuf, ThumbError> {
    let source_meta = std::fs::metadata(&request.source).map_err(ThumbError::Io)?;
    if !source_meta.is_file() {
        return Err(io_error(
            std::io::ErrorKind::InvalidInput,
            format!("source is not a regular file: {}", request.source.display()),
        ));
    }
    std::fs::create_dir_all(&request.destination_dir).map_err(ThumbError::Io)?;
    let destination = pick_destination(
        &request.source,
        &request.destination_dir,
        request.max_edge,
        &request.format,
    )?;

    let oriented = decode_oriented(&request.source)?;
    let thumb = thumbnail_from_rgba(oriented.into_rgba8(), request.max_edge);
    if !thumb.is_consistent() {
        return Err(io_error(
            std::io::ErrorKind::InvalidData,
            format!(
                "decoder produced inconsistent pixel buffer for {}",
                request.source.display()
            ),
        ));
    }
    let image = image::DynamicImage::ImageRgba8(
        image::RgbaImage::from_raw(thumb.width, thumb.height, thumb.pixels).ok_or_else(|| {
            io_error(
                std::io::ErrorKind::InvalidData,
                format!(
                    "decoder pixel buffer size mismatch for {}",
                    request.source.display()
                ),
            )
        })?,
    );

    let temp = destination.with_extension(format!(
        "{}.tmp",
        destination
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("out")
    ));
    encode(&image, &request.format, &temp)?;
    // The picked name was free moments ago, but re-check immediately before
    // the rename: `std::fs::rename` overwrites on Unix.
    if exists(&destination) {
        let _ = std::fs::remove_file(&temp);
        return Err(io_error(
            std::io::ErrorKind::AlreadyExists,
            format!("export target appeared: {}", destination.display()),
        ));
    }
    std::fs::rename(&temp, &destination).map_err(ThumbError::Io)?;
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_test_png(path: &Path, w: u32, h: u32) {
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(w, h, |x, y| {
            image::Rgba([(x * 4 % 256) as u8, (y * 4 % 256) as u8, 128, 255])
        }));
        let file = std::fs::File::create(path).unwrap();
        let mut writer = std::io::BufWriter::new(file);
        img.write_to(&mut writer, image::ImageFormat::Png).unwrap();
    }

    fn jpeg_request(source: PathBuf, dir: &Path, max_edge: u32) -> ExportRequest {
        ExportRequest {
            source,
            destination_dir: dir.to_path_buf(),
            max_edge,
            format: ExportFormat::Jpeg { quality: 85 },
        }
    }

    #[test]
    fn exports_resized_jpeg_with_expected_name_and_dimensions() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let source = dir.path().join("holiday.png");
        write_test_png(&source, 300, 150);

        let written = export_file(&jpeg_request(source, &out, 100)).unwrap();
        assert!(written.ends_with("holiday_100.jpg"));
        assert_eq!(image::image_dimensions(&written).unwrap(), (100, 50));
        // No temp residue after success.
        assert!(std::fs::read_dir(&out)
            .unwrap()
            .filter_map(Result::ok)
            .all(|e| !e.file_name().to_string_lossy().ends_with(".tmp")));
    }

    #[test]
    fn colliding_exports_get_suffixes_never_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let source = dir.path().join("asset.png");
        write_test_png(&source, 40, 40);

        let first = export_file(&jpeg_request(source.clone(), &out, 40)).unwrap();
        let second = export_file(&jpeg_request(source.clone(), &out, 40)).unwrap();
        let third = export_file(&jpeg_request(source, &out, 40)).unwrap();
        assert!(first.ends_with("asset_40.jpg"));
        assert!(second.ends_with("asset_40-1.jpg"));
        assert!(third.ends_with("asset_40-2.jpg"));
        assert!(first.exists() && second.exists() && third.exists());
    }

    #[test]
    fn corrupt_source_is_an_error_and_leaves_no_output() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let garbage = dir.path().join("broken.png");
        std::fs::write(&garbage, b"definitely not a png").unwrap();

        let result = export_file(&jpeg_request(garbage, &out, 64));
        assert!(matches!(result, Err(ThumbError::Decode { .. })));
        // Destination directory is created, but no partial output remains.
        assert!(out.exists());
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0);
    }

    #[test]
    fn png_and_webp_outputs_decode_back() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let source = dir.path().join("pic.png");
        write_test_png(&source, 64, 32);

        let png = export_file(&ExportRequest {
            source: source.clone(),
            destination_dir: out.clone(),
            max_edge: 32,
            format: ExportFormat::Png,
        })
        .unwrap();
        assert!(png.ends_with("pic_32.png"));
        assert_eq!(image::open(&png).unwrap().dimensions(), (32, 16));

        let webp = export_file(&ExportRequest {
            source,
            destination_dir: out,
            max_edge: 32,
            format: ExportFormat::WebP,
        })
        .unwrap();
        assert!(webp.ends_with("pic_32.webp"));
        assert_eq!(image::open(&webp).unwrap().dimensions(), (32, 16));
    }

    #[test]
    fn missing_source_and_directory_source_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let missing = jpeg_request(dir.path().join("nope.png"), &out, 32);
        assert!(matches!(export_file(&missing), Err(ThumbError::Io(_))));

        let as_dir = jpeg_request(dir.path().to_path_buf(), &out, 32);
        assert!(matches!(export_file(&as_dir), Err(ThumbError::Io(_))));
    }
}
