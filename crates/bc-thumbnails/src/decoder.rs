//! Image decoding and downscaling for thumbnails.

use std::path::Path;

use crate::{Thumbnail, ThumbError};

/// Decode an image file and produce an RGBA thumbnail whose longest
/// edge fits within `max_edge`, preserving aspect ratio.
pub fn decode_thumbnail(path: &Path, max_edge: u32) -> Result<Thumbnail, ThumbError> {
    let display = path.display().to_string();
    let img = image::open(path).map_err(|source| ThumbError::Decode {
        path: display,
        source,
    })?;

    let rgba = img.into_rgba8();
    let (w, h) = rgba.dimensions();

    let longest = w.max(h);
    let scaled = if longest > max_edge && longest > 0 {
        let ratio = max_edge as f64 / longest as f64;
        let nw = ((w as f64) * ratio).round().max(1.0) as u32;
        let nh = ((h as f64) * ratio).round().max(1.0) as u32;
        image::imageops::thumbnail(&rgba, nw.max(1), nh.max(1))
    } else {
        rgba
    };

    let (tw, th) = scaled.dimensions();
    Ok(Thumbnail {
        width: tw,
        height: th,
        pixels: scaled.into_raw(),
    })
}
