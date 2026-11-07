//! PDF page thumbnails through a locally available Pdfium library.
//!
//! BridgeCraft does not link or bundle Pdfium. [`pdfium_render`] loads the
//! user's `libpdfium` at run time and renders the first page of a document to
//! RGBA pixels. When no library can be loaded, PDF files are simply treated
//! as unsupported by the thumbnail pipeline (see
//! [`crate::decoder::can_thumbnail`]) rather than failing for every document
//! in a folder.
//!
//! Library discovery order:
//! 1. `PDFIUM_PATH`: a library file, or a directory containing the
//!    platform's Pdfium library name.
//! 2. The directory containing the BridgeCraft executable.
//! 3. Common install directories (`/usr/local/lib`, Homebrew prefixes, ...).
//! 4. The platform loader's default search path.
//!
//! Pdfium's licence (BSD-3-Clause for upstream Chromium's build, with
//! third-party components) applies to the user's copy only; no Pdfium code
//! is redistributed with BridgeCraft.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use pdfium_render::prelude::{PdfRenderConfig, Pdfium};

use crate::{ThumbError, Thumbnail};

/// True when `path` looks like a PDF by extension.
pub fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
}

/// Whether a Pdfium library could be loaded. The probe runs once per process.
pub fn is_available() -> bool {
    pdfium().is_ok()
}

/// Render the first page of `path` as an RGBA thumbnail whose longest edge
/// fits within `max_edge`, preserving aspect ratio.
pub fn decode_thumbnail(path: &Path, max_edge: u32) -> Result<Thumbnail, ThumbError> {
    let display = path.display().to_string();
    let pdfium = pdfium().map_err(|message| ThumbError::Pdf {
        path: display.clone(),
        message: message.to_string(),
    })?;
    let pdfium = pdfium.lock().map_err(|_| ThumbError::Pdf {
        path: display.clone(),
        message: "pdfium lock was poisoned by an earlier panic".into(),
    })?;

    let document = pdfium
        .load_pdf_from_file(path, None)
        .map_err(|e| ThumbError::Pdf {
            path: display.clone(),
            message: format!("could not open document: {e}"),
        })?;
    let page = document.pages().get(0).map_err(|e| ThumbError::Pdf {
        path: display.clone(),
        message: format!("could not load first page: {e}"),
    })?;

    let longest = page.width().value.max(page.height().value);
    let edge = max_edge.max(1);
    let scale = if longest > 0.0 {
        edge as f32 / longest
    } else {
        1.0
    };
    let config = PdfRenderConfig::new()
        .scale_page_by_factor(scale)
        .set_maximum_width(edge as i32)
        .set_maximum_height(edge as i32)
        .set_image_smoothing(true)
        .render_annotations(false)
        .render_form_data(false);

    let bitmap = page
        .render_with_config(&config)
        .map_err(|e| ThumbError::Pdf {
            path: display.clone(),
            message: format!("could not render first page: {e}"),
        })?;

    let width = bitmap.width().max(0) as u32;
    let height = bitmap.height().max(0) as u32;
    let pixels = bitmap.as_rgba_bytes();

    if width == 0 || height == 0 || pixels.len() != (width as usize) * (height as usize) * 4 {
        return Err(ThumbError::Pdf {
            path: display,
            message: format!(
                "unexpected bitmap: {}x{}, {} bytes",
                width,
                height,
                pixels.len()
            ),
        });
    }

    Ok(Thumbnail {
        width,
        height,
        pixels,
    })
}

/// Process-wide Pdfium instance. Pdfium initialisation is expensive and its
/// bindings are global, so one instance is created lazily and shared; the
/// `thread_safe` feature makes concurrent renders safe.
static PDFIUM: OnceLock<Result<Mutex<Pdfium>, String>> = OnceLock::new();

fn pdfium() -> Result<&'static Mutex<Pdfium>, &'static str> {
    match PDFIUM.get_or_init(init_pdfium) {
        Ok(pdfium) => Ok(pdfium),
        Err(message) => Err(message.as_str()),
    }
}

fn init_pdfium() -> Result<Mutex<Pdfium>, String> {
    let mut problems = Vec::new();
    for candidate in candidates() {
        match Pdfium::bind_to_library(&candidate) {
            Ok(bindings) => {
                tracing::debug!("pdfium loaded from {}", candidate.display());
                return Ok(Mutex::new(Pdfium::new(bindings)));
            }
            Err(e) => problems.push(format!("{}: {e}", candidate.display())),
        }
    }
    match Pdfium::bind_to_system_library() {
        Ok(bindings) => Ok(Mutex::new(Pdfium::new(bindings))),
        Err(e) => {
            problems.push(format!("system library: {e}"));
            let message = format!(
                "no usable Pdfium library found; set PDFIUM_PATH or install Pdfium ({})",
                problems.join("; ")
            );
            tracing::debug!("{message}");
            Err(message)
        }
    }
}

fn candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Some(raw) = std::env::var_os("PDFIUM_PATH") {
        let path = PathBuf::from(raw);
        if path.is_dir() {
            paths.push(Pdfium::pdfium_platform_library_name_at_path(&path));
        } else {
            paths.push(path);
        }
    }

    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        paths.push(Pdfium::pdfium_platform_library_name_at_path(&dir));
    }

    for dir in platform_library_dirs() {
        paths.push(Pdfium::pdfium_platform_library_name_at_path(dir));
    }

    paths
}

fn platform_library_dirs() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        &["/opt/homebrew/lib", "/usr/local/lib"]
    }
    #[cfg(target_os = "linux")]
    {
        &["/usr/local/lib", "/usr/lib/x86_64-linux-gnu", "/usr/lib"]
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        &[]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_classification() {
        assert!(is_supported(Path::new("doc.pdf")));
        assert!(is_supported(Path::new("DOC.PDF")));
        assert!(is_supported(Path::new("dir/manual.Pdf")));
        assert!(!is_supported(Path::new("doc.txt")));
        assert!(!is_supported(Path::new("pdf")));
    }

    /// Build a one-page PDF with a red rectangle in the middle of the page,
    /// with a correct cross-reference table so Pdfium accepts it.
    fn sample_pdf() -> Vec<u8> {
        let content = b"1 0 0 rg 100 100 200 300 re f\n";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 600] /Resources << >> /Contents 4 0 R >>"
                .to_string(),
            format!(
                "<< /Length {} >>\nstream\n{}endstream",
                content.len(),
                String::from_utf8_lossy(content)
            ),
        ];

        let mut pdf = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.push_str(&format!("{} 0 obj\n{}\nendobj\n", index + 1, object));
        }

        let xref_offset = pdf.len();
        pdf.push_str(&format!("xref\n0 {}\n", objects.len() + 1));
        pdf.push_str("0000000000 65535 f \n");
        for offset in offsets {
            pdf.push_str(&format!("{offset:010} 00000 n \n"));
        }
        pdf.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            objects.len() + 1,
            xref_offset
        ));
        pdf.into_bytes()
    }

    #[test]
    fn renders_first_page_when_pdfium_is_available() {
        if !is_available() {
            eprintln!("skipping: pdfium is not available (set PDFIUM_PATH)");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.pdf");
        std::fs::write(&path, sample_pdf()).unwrap();

        let thumb = decode_thumbnail(&path, 100).expect("page should render");
        assert!(thumb.is_consistent());
        assert!(thumb.width <= 100 && thumb.height <= 100);
        // 400x600 portrait page scaled to a 100px long edge.
        assert_eq!(thumb.height, 100);
        assert!(
            (thumb.width as i32 * 3 - thumb.height as i32 * 2).abs() <= 2,
            "unexpected size {}x{}",
            thumb.width,
            thumb.height
        );

        let pixel = |x: u32, y: u32| {
            let i = ((y * thumb.width + x) * 4) as usize;
            &thumb.pixels[i..i + 3]
        };
        // White page margin, then the red rectangle in the middle.
        assert!(pixel(2, 2).iter().all(|c| *c > 200));
        let mid = pixel(thumb.width / 2, thumb.height / 2);
        assert!(mid[0] > 200 && mid[1] < 60 && mid[2] < 60, "mid = {mid:?}");
    }

    #[test]
    fn corrupt_pdf_reports_an_error() {
        if !is_available() {
            eprintln!("skipping: pdfium is not available (set PDFIUM_PATH)");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.pdf");
        std::fs::write(&path, b"not a pdf").unwrap();

        assert!(matches!(
            decode_thumbnail(&path, 64),
            Err(ThumbError::Pdf { .. })
        ));
    }
}
