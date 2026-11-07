//! Video thumbnails through a locally installed FFmpeg binary.
//!
//! BridgeCraft does not link or bundle FFmpeg. [`ffmpeg_sidecar`] spawns the
//! user's `ffmpeg` executable, decodes exactly one frame scaled to the
//! requested edge, and hands the raw RGBA pixels back over stdout. When no
//! binary is available the thumbnail pipeline simply treats video files as
//! unsupported (see [`crate::decoder::can_thumbnail`]) instead of failing
//! loudly for every clip in a folder.
//!
//! The binary is user-provided, so its licence (LGPL/GPL depending on the
//! build) does not affect BridgeCraft's own licensing.

use std::path::Path;
use std::sync::OnceLock;

use ffmpeg_sidecar::command::FfmpegCommand;

use crate::{ThumbError, Thumbnail};

/// Seek position for the first extraction attempt. Starting a second or so
/// in skips title cards and fade-ins; clips shorter than this fall back to
/// their first frame.
const FIRST_SEEK: &str = "1";

/// Extensions handled by FFmpeg rather than the `image` crate.
const VIDEO_EXTENSIONS: [&str; 17] = [
    "mp4", "m4v", "mov", "mkv", "webm", "avi", "wmv", "flv", "mpg", "mpeg", "m2ts", "mts", "3gp",
    "3g2", "ogv", "ts", "vob",
];

/// True when `path` looks like a video file by extension.
pub fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| VIDEO_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// Whether an `ffmpeg` executable is available on this machine.
///
/// The probe runs once per process; callers may use it freely from hot paths.
pub fn is_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let available = ffmpeg_sidecar::command::ffmpeg_is_installed();
        if !available {
            tracing::debug!("ffmpeg not found in PATH: video thumbnails are disabled");
        }
        available
    })
}

/// Extract one representative frame as an RGBA thumbnail whose longest edge
/// fits within `max_edge`, preserving aspect ratio.
pub fn decode_thumbnail(path: &Path, max_edge: u32) -> Result<Thumbnail, ThumbError> {
    let display = path.display().to_string();
    if !is_available() {
        return Err(ThumbError::Video {
            path: display,
            message: "ffmpeg was not found in PATH".into(),
        });
    }

    let mut problems = Vec::new();
    for seek in [FIRST_SEEK, "0"] {
        match extract_frame(path, max_edge.max(1), seek) {
            Ok(Some(thumb)) => return Ok(thumb),
            Ok(None) => problems.push(format!("no frame found at {seek}s")),
            Err(message) => problems.push(message),
        }
    }

    Err(ThumbError::Video {
        path: display,
        message: problems.join("; "),
    })
}

/// Run FFmpeg once, seeking to `seek` seconds, and return the first decoded
/// frame as RGBA pixels. `Ok(None)` means FFmpeg ran but produced no frame
/// (for example, the seek landed past the end of a short clip).
fn extract_frame(path: &Path, max_edge: u32, seek: &str) -> Result<Option<Thumbnail>, String> {
    let scale = format!("scale={max_edge}:{max_edge}:force_original_aspect_ratio=decrease");

    let mut child = FfmpegCommand::new()
        .hide_banner()
        .seek(seek)
        .input(path)
        .no_audio()
        .frames(1)
        .filter(scale)
        .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .spawn()
        .map_err(|e| format!("could not start ffmpeg: {e}"))?;

    let mut frames = match child.iter() {
        Ok(iter) => iter.filter_frames(),
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("ffmpeg output unavailable: {e}"));
        }
    };

    // Exactly one frame is requested, and the iterator must be dropped before
    // waiting so its stdout thread stops and the process can be reaped.
    let frame = frames.next();
    drop(frames);
    let _ = child.wait();

    let Some(frame) = frame else {
        return Ok(None);
    };

    if frame.width == 0 || frame.height == 0 {
        return Err("ffmpeg returned an empty frame".into());
    }
    let expected = (frame.width as usize) * (frame.height as usize) * 4;
    if frame.data.len() != expected {
        return Err(format!(
            "unexpected frame size: {} bytes for {}x{} RGBA",
            frame.data.len(),
            frame.width,
            frame.height
        ));
    }

    let thumb = Thumbnail {
        width: frame.width,
        height: frame.height,
        pixels: frame.data,
    };
    if !thumb.is_consistent() {
        return Err("ffmpeg returned inconsistent frame data".into());
    }
    Ok(Some(thumb))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn extension_classification() {
        assert!(is_supported(Path::new("a.mp4")));
        assert!(is_supported(Path::new("a.MP4")));
        assert!(is_supported(Path::new("dir/b.mkv")));
        assert!(is_supported(Path::new("c.WEBM")));
        assert!(!is_supported(Path::new("photo.jpg")));
        assert!(!is_supported(Path::new("notes.txt")));
        assert!(!is_supported(Path::new("noext")));
    }

    /// Generate a short synthetic clip, then decode a frame from it. Skipped
    /// (with a log) when FFmpeg is not installed, so the suite stays green on
    /// machines without it.
    #[test]
    fn decode_frame_from_generated_clip() {
        if !is_available() {
            eprintln!("skipping: ffmpeg is not installed");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("clip.mp4");
        let status = FfmpegCommand::new()
            .hide_banner()
            .args([
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=5",
                "-t",
                "2",
                "-c:v",
                "mpeg4",
            ])
            .output(&clip)
            .spawn()
            .expect("ffmpeg should start")
            .wait()
            .expect("ffmpeg should finish");
        assert!(status.success(), "could not generate test clip");

        let thumb = decode_thumbnail(&clip, 100).expect("frame should decode");
        assert!(thumb.is_consistent());
        assert!(thumb.width <= 100 && thumb.height <= 100);
        // 4:3 source scaled to a 100px long edge.
        assert_eq!((thumb.width, thumb.height), (100, 75));
        // testsrc is never entirely black.
        assert!(thumb
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .any(|p| (p[0] | p[1] | p[2]) != 0));
    }

    #[test]
    fn shorter_than_seek_falls_back_to_first_frame() {
        if !is_available() {
            eprintln!("skipping: ffmpeg is not installed");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("short.mp4");
        let status = FfmpegCommand::new()
            .hide_banner()
            .args([
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=160x120:rate=5",
                "-t",
                "0.4",
                "-c:v",
                "mpeg4",
            ])
            .output(&clip)
            .spawn()
            .expect("ffmpeg should start")
            .wait()
            .expect("ffmpeg should finish");
        assert!(status.success(), "could not generate short clip");

        let thumb = decode_thumbnail(&clip, 64).expect("first-frame fallback should work");
        assert!(thumb.is_consistent());
        assert_eq!((thumb.width, thumb.height), (64, 48));
    }

    #[test]
    fn corrupt_video_reports_an_error() {
        if !is_available() {
            eprintln!("skipping: ffmpeg is not installed");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("broken.mp4");
        std::fs::write(&bogus, b"not a video").unwrap();

        // Give a hung/corrupt decode a generous bound in the test itself.
        let start = std::time::Instant::now();
        let err = decode_thumbnail(&bogus, 64).unwrap_err();
        assert!(matches!(err, ThumbError::Video { .. }));
        assert!(start.elapsed() < Duration::from_secs(30));
    }
}
