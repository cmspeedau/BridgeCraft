//! Preview/zoom viewer state: quality tiers, zoom & pan maths, and stepping
//! through the visible files. Everything here is pure (no egui drawing, no
//! file IO) so it can be unit-tested without a window.
//!
//! Quality tiers, in rising resolution:
//! 1. **Thumbnail** (256 px) — the grid already has it; kept in the SQLite cache.
//! 2. **Preview** (2048 px) — first thing the viewer shows; also cached, since
//!    the cache is keyed by `(path, max_edge)`.
//! 3. **Full / 1:1** — native resolution, decoded on demand when the user asks
//!    for 100% zoom and never cached (huge RGBA blobs would bloat the cache).
//!    Guarded by [`full_allowed`] so a pathological file cannot make the app
//!    decode hundreds of megapixels.

use std::path::PathBuf;

use eframe::egui;

/// Long edge of the cached preview tier.
pub const PREVIEW_EDGE: u32 = 2048;
/// Long edge requested for the uncached full-resolution tier. Decoders scale
/// internally to fit this, so larger files simply arrive at 8192 px.
pub const FULL_EDGE: u32 = 8192;
/// Native pixel counts beyond this never trigger the full tier: the `image`
/// crate's decode peak (RGB + RGBA ≈ 8·MP bytes) would exceed a reasonable
/// budget, so the preview tier stands in and the status line says so.
pub const MAX_FULL_PIXELS: u64 = 64_000_000;
/// Zoom bounds, as a scale factor where 1.0 means one texture pixel per point.
pub const MIN_ZOOM: f32 = 0.02;
pub const MAX_ZOOM: f32 = 16.0;

/// Zoom state. `Fit` recomputes on every resize; `Manual` keeps the user's
/// zoom until they return to fit.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ZoomMode {
    #[default]
    Fit,
    Manual(f32),
}

/// Pure state of the preview window.
#[derive(Debug, Default)]
pub struct ViewerState {
    /// File currently shown; `None` means the viewer is closed.
    pub path: Option<PathBuf>,
    /// Navigable file list (visible, non-directory entries) for ←/→.
    pub files: Vec<PathBuf>,
    /// Native dimensions from a header read, when the format exposes them.
    pub native: Option<(u32, u32)>,
    pub zoom: ZoomMode,
    /// Pan offset from centre, in points.
    pub offset: egui::Vec2,
    /// The user already asked for the full tier once (suppresses re-requests).
    pub full_requested: bool,
    /// Informational note for the status line ("full disabled: …").
    pub note: String,
    /// Scale actually rendered last frame; seeds manual zoom from fit mode.
    pub displayed: f32,
}

impl ViewerState {
    pub fn open(&mut self, path: PathBuf, files: Vec<PathBuf>) {
        self.path = Some(path);
        self.files = files;
        self.native = None;
        self.zoom = ZoomMode::Fit;
        self.offset = egui::Vec2::ZERO;
        self.full_requested = false;
        self.note.clear();
        self.displayed = 1.0;
    }

    pub fn close(&mut self) {
        self.path = None;
        self.files.clear();
        self.native = None;
        self.note.clear();
    }

    pub fn is_open(&self) -> bool {
        self.path.is_some()
    }

    /// Move along the file list, wrapping at both ends. Resets pan/zoom like a
    /// fresh open; returns the newly current path.
    pub fn step(&mut self, delta: isize) -> Option<PathBuf> {
        let current = self.path.as_ref()?;
        let position = self.files.iter().position(|p| p == current)?;
        let len = self.files.len() as isize;
        let next = (position as isize + delta).rem_euclid(len) as usize;
        let path = self.files.get(next)?.clone();
        self.path = Some(path.clone());
        self.native = None;
        self.zoom = ZoomMode::Fit;
        self.offset = egui::Vec2::ZERO;
        self.full_requested = false;
        self.note.clear();
        Some(path)
    }

    /// Multiply the current zoom, leaving fit mode at the scale last shown.
    pub fn zoom_step(&mut self, factor: f32) {
        let from = match self.zoom {
            ZoomMode::Manual(z) => z,
            ZoomMode::Fit => self.displayed,
        };
        self.zoom = ZoomMode::Manual(clamp_zoom(from * factor));
    }

    pub fn set_fit(&mut self) {
        self.zoom = ZoomMode::Fit;
        self.offset = egui::Vec2::ZERO;
    }

    /// 100% — one texture pixel per point; the app layers the full-tier
    /// request on top when the displayed texture is downscaled.
    pub fn set_actual(&mut self) {
        self.zoom = ZoomMode::Manual(1.0);
    }
}

/// Clamp a zoom factor into the supported range.
pub fn clamp_zoom(zoom: f32) -> f32 {
    if !zoom.is_finite() {
        return 1.0;
    }
    zoom.clamp(MIN_ZOOM, MAX_ZOOM)
}

/// Scale that fits `(width, height)` into `available`, upsampling small
/// images like a typical photo viewer. Zero sizes are guarded.
pub fn fit_scale(size: (u32, u32), available: (f32, f32)) -> f32 {
    let w = size.0.max(1) as f32;
    let h = size.1.max(1) as f32;
    let avail_w = if available.0.is_finite() {
        available.0.max(1.0)
    } else {
        1.0
    };
    let avail_h = if available.1.is_finite() {
        available.1.max(1.0)
    } else {
        1.0
    };
    clamp_zoom((avail_w / w).min(avail_h / h))
}

/// Whether a 1:1 full-tier decode may be attempted for these native
/// dimensions. `None` means the format's header did not expose them (video,
/// PDF, RAW) — those decoders scale internally to `FULL_EDGE`, so they are
/// always allowed.
pub fn full_allowed(native: Option<(u32, u32)>) -> bool {
    match native {
        Some((width, height)) => {
            width.max(height) <= FULL_EDGE
                && u64::from(width) * u64::from(height) <= MAX_FULL_PIXELS
        }
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn open_and_close_track_current_file() {
        let mut v = ViewerState::default();
        assert!(!v.is_open());
        v.open(PathBuf::from("a.png"), paths(&["a.png", "b.png"]));
        assert!(v.is_open());
        assert_eq!(v.zoom, ZoomMode::Fit);
        v.close();
        assert!(!v.is_open());
        assert!(v.files.is_empty());
    }

    #[test]
    fn stepping_wraps_and_resets_view() {
        let mut v = ViewerState::default();
        v.open(PathBuf::from("b.png"), paths(&["a.png", "b.png", "c.png"]));
        v.zoom = ZoomMode::Manual(4.0);
        v.offset = egui::vec2(9.0, 9.0);

        assert_eq!(v.step(1).as_deref(), Some(PathBuf::from("c.png").as_path()));
        assert_eq!(v.zoom, ZoomMode::Fit, "stepping resets zoom");
        assert_eq!(
            v.step(1).as_deref(),
            Some(PathBuf::from("a.png").as_path()),
            "wrap past end"
        );
        assert_eq!(
            v.step(-1).as_deref(),
            Some(PathBuf::from("c.png").as_path()),
            "wrap back"
        );
    }

    #[test]
    fn stepping_without_a_list_is_inert() {
        let mut v = ViewerState::default();
        assert_eq!(v.step(1), None, "closed viewer");
        v.open(PathBuf::from("x.png"), Vec::new());
        assert_eq!(v.step(1), None, "empty list");

        let mut stale = ViewerState::default();
        stale.open(PathBuf::from("gone.png"), paths(&["a.png"]));
        assert_eq!(stale.step(1), None, "current path not in list");
    }

    #[test]
    fn zoom_step_from_fit_starts_at_displayed_scale() {
        let mut v = ViewerState {
            displayed: 0.5,
            ..ViewerState::default()
        };
        v.zoom_step(2.0);
        assert_eq!(v.zoom, ZoomMode::Manual(1.0));
        v.zoom_step(2.0);
        assert_eq!(v.zoom, ZoomMode::Manual(2.0));
    }

    #[test]
    fn zoom_clamps_at_bounds_and_survives_nan() {
        assert_eq!(clamp_zoom(0.0001), MIN_ZOOM);
        assert_eq!(clamp_zoom(1000.0), MAX_ZOOM);
        assert_eq!(clamp_zoom(f32::NAN), 1.0);
        let mut v = ViewerState {
            displayed: MAX_ZOOM,
            ..ViewerState::default()
        };
        v.zoom_step(10.0);
        assert_eq!(v.zoom, ZoomMode::Manual(MAX_ZOOM));
    }

    #[test]
    fn fit_scale_preserves_aspect_and_guards_zero() {
        // Landscape in a portrait-ish box: width-bound.
        assert_eq!(fit_scale((4000, 2000), (800.0, 900.0)), 0.2);
        // Height-bound.
        assert_eq!(fit_scale((100, 400), (800.0, 200.0)), 0.5);
        // Small images upsample to fill.
        assert_eq!(fit_scale((50, 50), (400.0, 400.0)), 8.0);
        // Degenerate sizes and infinities stay finite and positive.
        let ok = fit_scale((0, 0), (f32::INFINITY, f32::NAN));
        assert!(ok.is_finite() && ok > 0.0);
    }

    #[test]
    fn full_tier_allows_reasonable_and_blocks_huge() {
        assert!(full_allowed(None), "headerless decoders scale internally");
        assert!(full_allowed(Some((6000, 4000))), "24 MP camera file");
        assert!(full_allowed(Some((8000, 8000))), "exactly at both caps");
        assert!(!full_allowed(Some((8193, 100))), "edge above FULL_EDGE");
        assert!(
            !full_allowed(Some((9504, 6336))),
            "60+ MP over MAX_FULL_PIXELS"
        );
    }
}
