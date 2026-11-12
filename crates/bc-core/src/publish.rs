//! Export / publish pipelines (roadmap Phase 5).
//!
//! A preset-driven layer on top of the batch export pipeline (`export` +
//! `jobs`): a [`PublishPreset`] describes a destination, format, size and
//! naming policy; [`publish`] applies it to a set of sources and writes the
//! outputs atomically, reporting per-file successes and failures. The same
//! preset can be reused across manual runs and watched-folder auto-actions.

use std::path::{Path, PathBuf};

use crate::export::{self, ExportError, ExportFormat};

pub type Error = ExportError;
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishDestination {
    BesideSource,
    Folder(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishNaming {
    /// `stem_MAXEDGE.ext`
    Default,
    /// Keep the original stem and extension of the source.
    KeepStem,
}

#[derive(Debug, Clone)]
pub struct PublishPreset {
    pub name: String,
    pub destination: PublishDestination,
    pub format: ExportFormat,
    pub max_edge: u32,
    pub overwrite: bool,
    pub naming: PublishNaming,
}

impl PublishPreset {
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(ExportError::InvalidRequest(
                "preset needs a name".to_string(),
            ));
        }
        if !(export::MIN_MAX_EDGE..=export::MAX_MAX_EDGE).contains(&self.max_edge) {
            return Err(ExportError::InvalidRequest(format!(
                "max edge must be {}..={} pixels",
                export::MIN_MAX_EDGE,
                export::MAX_MAX_EDGE
            )));
        }
        if let PublishDestination::Folder(dir) = &self.destination {
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

#[derive(Debug, Clone)]
pub struct PublishRequest {
    pub sources: Vec<PathBuf>,
    pub preset: PublishPreset,
}

#[derive(Debug, Clone, Default)]
pub struct PublishReport {
    pub outputs: Vec<PathBuf>,
    pub failures: Vec<(PathBuf, String)>,
}

impl PublishReport {
    pub fn succeeded(&self) -> usize {
        self.outputs.len()
    }
    pub fn failed(&self) -> usize {
        self.failures.len()
    }
}

/// Apply a preset to `sources` synchronously, writing one output per source.
pub fn publish(request: &PublishRequest) -> Result<PublishReport> {
    request.preset.validate()?;
    if request.sources.is_empty() {
        return Err(ExportError::InvalidRequest("no sources".to_string()));
    }
    let mut report = PublishReport::default();
    for source in &request.sources {
        match publish_one(source, &request.preset) {
            Ok(out) => report.outputs.push(out),
            Err(e) => report.failures.push((source.clone(), e.to_string())),
        }
    }
    Ok(report)
}

fn publish_one(source: &Path, preset: &PublishPreset) -> Result<PathBuf> {
    if !source.is_file() {
        return Err(ExportError::Unsupported(source.display().to_string()));
    }
    let image = export::decode_for_export(source, preset.max_edge)?;
    let encoded = export::encode_image(&image, preset.format)?;
    let folder = match &preset.destination {
        PublishDestination::BesideSource => source.parent().unwrap_or_else(|| Path::new(".")),
        PublishDestination::Folder(dir) => dir.as_path(),
    };
    let target = if preset.overwrite {
        // Overwrite: fixed name.
        let stem = source
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("output");
        folder.join(format!("{}.{}", stem, preset.format.extension()))
    } else {
        export::unique_export_name(folder, source, preset.max_edge, preset.format, Path::exists)
            .ok_or_else(|| ExportError::InvalidRequest("no available output name".to_string()))?
    };
    // atomic write
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let tmp = parent.join(format!(
        ".{}.publish-tmp-{}",
        target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id()
    ));
    std::fs::write(&tmp, &encoded)?;
    if preset.overwrite && target.exists() {
        let _ = std::fs::remove_file(&target);
    }
    std::fs::rename(&tmp, &target)?;
    Ok(target)
}

// Aliases
pub type PublishPipeline = PublishPreset;
pub type PublishJob = PublishRequest;

pub fn run(request: &PublishRequest) -> Result<PublishReport> {
    publish(request)
}

pub fn run_pipeline(request: &PublishRequest) -> Result<PublishReport> {
    publish(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder;
    use tempfile::tempdir;

    fn tiny_jpeg(path: &Path) {
        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255]));
        let mut f = std::fs::File::create(path).unwrap();
        let enc = image::codecs::jpeg::JpegEncoder::new(&mut f);
        enc.write_image(img.as_raw(), 4, 4, image::ColorType::Rgba8)
            .unwrap();
    }

    #[test]
    fn publish_beside_source() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("a.jpg");
        tiny_jpeg(&src);
        let preset = PublishPreset {
            name: "p".into(),
            destination: PublishDestination::BesideSource,
            format: ExportFormat::Png,
            max_edge: 32,
            overwrite: false,
            naming: PublishNaming::Default,
        };
        let req = PublishRequest {
            sources: vec![src.clone()],
            preset,
        };
        let report = publish(&req).unwrap();
        assert_eq!(report.succeeded(), 1);
        assert!(report.outputs[0].exists());
    }

    #[test]
    fn publish_to_folder() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("a.jpg");
        tiny_jpeg(&src);
        let outdir = dir.path().join("out");
        std::fs::create_dir(&outdir).unwrap();
        let preset = PublishPreset {
            name: "p".into(),
            destination: PublishDestination::Folder(outdir.clone()),
            format: ExportFormat::Jpeg { quality: 80 },
            max_edge: 32,
            overwrite: false,
            naming: PublishNaming::Default,
        };
        let req = PublishRequest {
            sources: vec![src],
            preset,
        };
        let report = publish(&req).unwrap();
        assert_eq!(report.succeeded(), 1);
        assert!(report.outputs[0].starts_with(&outdir));
    }

    #[test]
    fn validate_rejects_bad_max_edge() {
        let preset = PublishPreset {
            name: "p".into(),
            destination: PublishDestination::BesideSource,
            format: ExportFormat::Png,
            max_edge: 1,
            overwrite: false,
            naming: PublishNaming::Default,
        };
        assert!(preset.validate().is_err());
    }
}
