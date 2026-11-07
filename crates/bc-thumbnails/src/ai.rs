//! Adobe Illustrator files with PDF-compatible content use the existing PDF renderer.

use std::fs::File;
use std::io::Read;
use std::path::Path;

/// True for `.ai` paths that start with a PDF header rather than PostScript.
pub fn is_pdf_compatible(path: &Path) -> bool {
    if !path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("ai"))
    {
        return false;
    }
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut prefix = [0u8; 5];
    file.read_exact(&mut prefix).is_ok() && &prefix == b"%PDF-"
}

/// True when the path has an Illustrator extension.
pub fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("ai"))
}

/// Decode PDF-compatible Illustrator content; PostScript-only AI is unsupported.
pub fn decode_thumbnail(path: &Path, max_edge: u32) -> Result<crate::Thumbnail, crate::ThumbError> {
    let display = path.display().to_string();
    if !is_pdf_compatible(path) {
        return Err(crate::ThumbError::Ai {
            path: display,
            message:
                "PostScript-only Illustrator files are unsupported; PDF-compatible AI is required"
                    .into(),
        });
    }
    crate::pdf::decode_thumbnail(path, max_edge).map_err(|error| crate::ThumbError::Ai {
        path: display,
        message: format!("PDF-compatible AI rendering failed: {error}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguishes_pdf_compatible_ai_from_postscript() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("design.ai");
        std::fs::write(&path, b"%PDF-1.4\n").unwrap();
        assert!(is_supported(&path));
        assert!(is_pdf_compatible(&path));
        std::fs::write(&path, b"%!PS-Adobe-3.0\n").unwrap();
        assert!(!is_pdf_compatible(&path));
        assert!(matches!(
            decode_thumbnail(&path, 64),
            Err(crate::ThumbError::Ai { .. })
        ));
    }
}
