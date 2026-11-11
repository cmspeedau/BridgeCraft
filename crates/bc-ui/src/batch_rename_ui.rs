//! Batch-rename dialog state and preview planning (issue #14).
//!
//! The dialog delegates parsing, collision checks and filesystem changes to
//! `bc-core::batch_rename`; this module only prepares metadata-aware contexts
//! and keeps the editable pattern plus its preview.

use std::path::{Path, PathBuf};

use bc_core::batch_rename::{self, Pattern, RenameContext, RenamePlanItem};

/// User-editable batch rename, with a precomputed preview plan.
pub struct RenameDialog {
    pub sources: Vec<PathBuf>,
    pub pattern: String,
    contexts: Vec<RenameContext>,
    pub preview: Result<Vec<RenamePlanItem>, String>,
}

impl RenameDialog {
    pub fn new(sources: Vec<PathBuf>) -> Self {
        let contexts = rename_contexts(&sources);
        let mut dialog = Self {
            sources,
            pattern: "{name}_{seq:0>3}".to_string(),
            contexts,
            preview: Ok(Vec::new()),
        };
        dialog.rebuild_preview();
        dialog
    }

    /// Update the plan after pattern edits. Filesystem state is rechecked so
    /// newly occupied destinations cannot silently overwrite a file.
    pub fn rebuild_preview(&mut self) {
        self.preview = Pattern::parse(&self.pattern)
            .and_then(|pattern| batch_rename::plan(&pattern, &self.contexts))
            .map_err(|error| error.to_string());
    }
}

fn rename_contexts(sources: &[PathBuf]) -> Vec<RenameContext> {
    sources
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let mut context = RenameContext::from_path(path.clone(), index);
            if path.is_file() {
                let sidecar = bc_metadata::xmp::sidecar_path(path);
                if let Ok(metadata) = bc_metadata::xmp::read_xmp_file(&sidecar) {
                    context.keywords = bc_metadata::keywords::subjects(&metadata);
                }
            }
            context
        })
        .collect()
}

/// Persistent undo journal location, following the user's XDG data directory.
pub fn journal_path() -> Result<PathBuf, String> {
    let data_dir = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("share"))
        })
        .ok_or_else(|| "cannot locate a user data directory".to_string())?;
    Ok(batch_rename::journal_path(&data_dir.join("BridgeCraft")))
}

/// Human-readable relative name for the preview; paths themselves remain
/// available on the plan item for the apply operation.
pub fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_uses_sequential_names_and_keeps_extensions() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("one.jpg");
        let second = temp.path().join("two.png");
        std::fs::write(&first, b"1").unwrap();
        std::fs::write(&second, b"2").unwrap();

        let mut dialog = RenameDialog::new(vec![first.clone(), second.clone()]);
        let plan = dialog.preview.as_ref().unwrap();
        assert_eq!(plan[0].target.file_name().unwrap(), "one_000.jpg");
        assert_eq!(plan[1].target.file_name().unwrap(), "two_001.png");

        dialog.pattern = "asset_{seq:1>2}".to_string();
        dialog.rebuild_preview();
        let plan = dialog.preview.as_ref().unwrap();
        assert_eq!(plan[0].target.file_name().unwrap(), "asset_01.jpg");
        assert_eq!(plan[1].target.file_name().unwrap(), "asset_02.png");
    }

    #[test]
    fn preview_has_stable_source_order_and_sidecar_keywords() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("z-last.jpg");
        let second = temp.path().join("a-first.jpg");
        std::fs::write(&first, b"1").unwrap();
        std::fs::write(&second, b"2").unwrap();
        let sidecar = bc_metadata::xmp::sidecar_path(&second);
        let mut metadata = bc_metadata::Metadata::new();
        bc_metadata::keywords::set_subjects(&mut metadata, vec!["Studio".to_string()]);
        bc_metadata::xmp::write_xmp_file(&sidecar, &metadata).unwrap();

        let mut dialog = RenameDialog::new(vec![first.clone(), second.clone()]);
        dialog.pattern = "{keyword#1}_{seq:1>2}".into();
        dialog.rebuild_preview();
        let plan = dialog.preview.as_ref().unwrap();
        assert_eq!(plan[0].source, first);
        assert_eq!(plan[1].source, second);
        assert_eq!(plan[0].target.file_name().unwrap(), "_01.jpg");
        assert_eq!(plan[1].target.file_name().unwrap(), "Studio_02.jpg");
    }

    #[test]
    fn preview_reports_parse_and_collision_errors() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("one.jpg");
        let second = temp.path().join("two.jpg");
        std::fs::write(&first, b"1").unwrap();
        std::fs::write(&second, b"2").unwrap();

        let mut dialog = RenameDialog::new(vec![first, second]);
        dialog.pattern = "same".to_string();
        dialog.rebuild_preview();
        assert!(dialog.preview.is_err());

        dialog.pattern = "{unknown}".to_string();
        dialog.rebuild_preview();
        assert!(dialog.preview.is_err());
    }
}
