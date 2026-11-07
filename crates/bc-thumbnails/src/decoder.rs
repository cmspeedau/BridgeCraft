//! Image and video decoding plus downscaling for thumbnails.

use std::path::Path;

use crate::{pdf, raw, video, ThumbError, Thumbnail};

/// True when `path` has the extension of an image format this crate can
/// decode. Keep in sync with the `image` features in the workspace
/// `Cargo.toml` (png, jpeg, gif, webp, tiff, bmp).
pub fn is_supported(path: &Path) -> bool {
    const SUPPORTED: [&str; 8] = ["png", "jpg", "jpeg", "gif", "webp", "tif", "tiff", "bmp"];
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| SUPPORTED.contains(&ext.to_ascii_lowercase().as_str()))
}

/// True when a thumbnail can actually be produced for `path`: a supported
/// image, or a video while an FFmpeg binary is available.
///
/// Use this (rather than [`is_supported`]) to decide what enters the
/// thumbnail pipeline, so a machine without FFmpeg does not retry every
/// video file it browses.
pub fn can_thumbnail(path: &Path) -> bool {
    is_supported(path)
        || (video::is_supported(path) && video::is_available())
        || (pdf::is_supported(path) && pdf::is_available())
        || (raw::is_supported(path) && raw::is_available())
}

/// Decode an image, video, or RAW file and produce an RGBA thumbnail whose
/// longest edge fits within `max_edge`, preserving aspect ratio.
pub fn decode_thumbnail(path: &Path, max_edge: u32) -> Result<Thumbnail, ThumbError> {
    if video::is_supported(path) {
        return video::decode_thumbnail(path, max_edge);
    }
    if pdf::is_supported(path) {
        return pdf::decode_thumbnail(path, max_edge);
    }
    if raw::is_supported(path) {
        return raw::decode_thumbnail(path, max_edge);
    }

    let display = path.display().to_string();
    let img = image::open(path).map_err(|source| ThumbError::Decode {
        path: display,
        source,
    })?;

    let rgba = img.into_rgba8();
    Ok(thumbnail_from_rgba(rgba, max_edge))
}

/// Fit `(width, height)` inside `max_edge`, preserving aspect ratio.
/// Returns the scaled dimensions, or `None` when no scaling is needed.
fn scale_fit(width: u32, height: u32, max_edge: u32) -> Option<(u32, u32)> {
    let longest = width.max(height);
    if longest > max_edge && longest > 0 {
        let ratio = max_edge as f64 / longest as f64;
        let nw = ((width as f64) * ratio).round().max(1.0) as u32;
        let nh = ((height as f64) * ratio).round().max(1.0) as u32;
        Some((nw.max(1), nh.max(1)))
    } else {
        None
    }
}

/// Downscale an RGBA image so its longest edge fits within `max_edge`.
pub(crate) fn thumbnail_from_rgba(rgba: image::RgbaImage, max_edge: u32) -> Thumbnail {
    let (w, h) = rgba.dimensions();
    let scaled = match scale_fit(w, h, max_edge) {
        Some((nw, nh)) => image::imageops::thumbnail(&rgba, nw, nh),
        None => rgba,
    };
    let (tw, th) = scaled.dimensions();
    Thumbnail {
        width: tw,
        height: th,
        pixels: scaled.into_raw(),
    }
}

/// Downscale an RGB image so its longest edge fits within `max_edge`, adding
/// an opaque alpha channel. Scaling happens while the buffer is still RGB,
/// which halves peak memory for large camera-RAW decodes.
pub(crate) fn thumbnail_from_rgb(rgb: image::RgbImage, max_edge: u32) -> Thumbnail {
    let (w, h) = rgb.dimensions();
    let scaled = match scale_fit(w, h, max_edge) {
        Some((nw, nh)) => image::imageops::thumbnail(&rgb, nw, nh),
        None => rgb,
    };
    let (tw, th) = scaled.dimensions();
    let pixels = scaled
        .as_raw()
        .as_chunks::<3>()
        .0
        .iter()
        .flat_map(|p| [p[0], p[1], p[2], 255])
        .collect();
    Thumbnail {
        width: tw,
        height: th,
        pixels,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_supported_matches_enabled_formats() {
        assert!(is_supported(Path::new("a.png")));
        assert!(is_supported(Path::new("a.PNG")));
        assert!(is_supported(Path::new("dir/b.jpeg")));
        assert!(is_supported(Path::new("dir/b.JPG")));
        assert!(is_supported(Path::new("c.tiff")));
        assert!(is_supported(Path::new("d.TIF")));
        assert!(is_supported(Path::new("e.webp")));
        assert!(!is_supported(Path::new("notes.txt")));
        assert!(!is_supported(Path::new("film.mp4")));
        assert!(!is_supported(Path::new("noext")));
        assert!(!is_supported(Path::new(".hidden.png.bak")));
    }

    #[test]
    fn can_thumbnail_gates_optional_runtimes() {
        assert!(can_thumbnail(Path::new("a.png")));
        assert!(!can_thumbnail(Path::new("notes.txt")));
        assert_eq!(can_thumbnail(Path::new("film.mp4")), video::is_available());
        assert_eq!(can_thumbnail(Path::new("doc.pdf")), pdf::is_available());
        assert_eq!(can_thumbnail(Path::new("shot.CR2")), raw::is_available());
    }
}
