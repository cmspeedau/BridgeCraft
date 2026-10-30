//! Basic filesystem operations used by the browser (copy, move, rename, delete).
//!
//! These helpers do not overwrite existing destinations. Directory copies are
//! recursive; symlinks are rejected rather than followed.

use std::path::{Component, Path, PathBuf};

use crate::{CoreError, Result};

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn ensure_source(path: &Path) -> Result<std::fs::Metadata> {
    std::fs::symlink_metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CoreError::PathNotFound(path.display().to_string())
        } else {
            CoreError::Io(e)
        }
    })
}

fn ensure_new_destination(path: &Path) -> Result<()> {
    if exists(path) {
        return Err(CoreError::PathAlreadyExists(path.display().to_string()));
    }
    Ok(())
}

/// Copy a regular file or directory tree without replacing any existing path.
pub fn copy_path(source: &Path, destination: &Path) -> Result<()> {
    let metadata = ensure_source(source)?;
    ensure_new_destination(destination)?;
    if metadata.file_type().is_symlink() {
        return Err(CoreError::InvalidOperation(format!(
            "refusing to copy symlink {}",
            source.display()
        )));
    }
    if metadata.is_dir() {
        if destination.starts_with(source) {
            return Err(CoreError::InvalidOperation(
                "cannot copy a directory into itself".to_string(),
            ));
        }
        copy_directory(source, destination)
    } else if metadata.is_file() {
        std::fs::copy(source, destination)?;
        Ok(())
    } else {
        Err(CoreError::InvalidOperation(format!(
            "unsupported filesystem object: {}",
            source.display()
        )))
    }
}

fn copy_directory(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir(destination)?;
    for item in std::fs::read_dir(source)? {
        let item = item?;
        copy_path(&item.path(), &destination.join(item.file_name()))?;
    }
    Ok(())
}

/// Move a file or directory to a new path. Existing destinations are never replaced.
pub fn move_path(source: &Path, destination: &Path) -> Result<()> {
    let metadata = ensure_source(source)?;
    ensure_new_destination(destination)?;
    if metadata.is_dir() && destination.starts_with(source) {
        return Err(CoreError::InvalidOperation(
            "cannot move a directory into itself".to_string(),
        ));
    }
    std::fs::rename(source, destination)?;
    Ok(())
}

/// How selected items are transferred into a destination folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferMode {
    Move,
    Copy,
}

/// Summary returned by [`transfer_into_folder`].
#[derive(Debug, Default)]
pub struct TransferReport {
    /// Items successfully moved or copied.
    pub transferred: usize,
    /// Items skipped because they already reside in the destination folder (moves only).
    pub skipped: usize,
    /// Per-item failures in input order; empty when every item succeeded.
    pub errors: Vec<(PathBuf, CoreError)>,
}

/// Move or copy selected paths into an existing folder, preserving each name.
///
/// Batch-level rejections — a missing or non-folder destination, or a folder
/// transferred into itself or its own descendant — are returned as `Err`
/// before any item is touched, so a rejected batch leaves the filesystem
/// unchanged. Per-item failures (name collisions, symlinked sources on copy,
/// I/O errors) do not stop the batch: the transfer continues and each failure
/// is collected into [`TransferReport::errors`].
pub fn transfer_into_folder(
    sources: &[PathBuf],
    destination_folder: &Path,
    mode: TransferMode,
) -> Result<TransferReport> {
    let destination_meta = ensure_source(destination_folder)?;
    if !destination_meta.is_dir() {
        return Err(CoreError::InvalidOperation(format!(
            "destination is not a folder: {}",
            destination_folder.display()
        )));
    }
    let destination_real = std::fs::canonicalize(destination_folder)?;

    // Validate every source before touching the filesystem so a rejected
    // batch transfers nothing, not even the items before the offending one.
    let mut plan = Vec::with_capacity(sources.len());
    for source in sources {
        let source_meta = ensure_source(source)?;
        let name = source.file_name().ok_or_else(|| {
            CoreError::InvalidOperation(format!("path has no file name: {}", source.display()))
        })?;
        if source_meta.is_dir() {
            let source_real = std::fs::canonicalize(source)?;
            if destination_real.starts_with(&source_real) {
                return Err(CoreError::InvalidOperation(format!(
                    "cannot transfer a folder into itself or its descendant: {}",
                    source.display()
                )));
            }
        }
        // Compare the canonicalised parent rather than the canonicalised
        // source: canonicalising the source itself resolves symlinks to their
        // target's folder, so a symlink already sitting in the destination
        // would not be recognised there and would collide with itself.
        let already_in_place = mode == TransferMode::Move
            && source
                .parent()
                .and_then(|parent| std::fs::canonicalize(parent).ok())
                .is_some_and(|parent_real| parent_real == destination_real);
        plan.push((name, already_in_place));
    }

    let mut report = TransferReport::default();
    for (source, (name, already_in_place)) in sources.iter().zip(plan) {
        if already_in_place {
            report.skipped += 1;
            continue;
        }
        let destination = destination_folder.join(name);
        let outcome = match mode {
            TransferMode::Move => move_path(source, &destination),
            TransferMode::Copy => copy_path(source, &destination),
        };
        match outcome {
            Ok(()) => report.transferred += 1,
            Err(error) => report.errors.push((source.clone(), error)),
        }
    }
    Ok(report)
}

/// Rename an item in place. `new_name` must be a single, non-empty path component.
pub fn rename_path(source: &Path, new_name: &str) -> Result<PathBuf> {
    ensure_source(source)?;
    let mut components = Path::new(new_name).components();
    if new_name.is_empty()
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(CoreError::InvalidOperation(
            "new name must be a single path component".to_string(),
        ));
    }
    let parent = source.parent().unwrap_or_else(|| Path::new("."));
    let destination = parent.join(new_name);
    ensure_new_destination(&destination)?;
    std::fs::rename(source, &destination)?;
    Ok(destination)
}

/// Delete a file or directory tree. Symlinks are removed as links, never traversed.
pub fn delete_path(path: &Path) -> Result<()> {
    let metadata = ensure_source(path)?;
    if metadata.file_type().is_symlink() || metadata.is_file() {
        std::fs::remove_file(path)?;
    } else if metadata.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else {
        return Err(CoreError::InvalidOperation(format!(
            "unsupported filesystem object: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_file_and_directory_tree_without_overwriting() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(source.join("nested")).unwrap();
        std::fs::write(source.join("nested/file.txt"), b"asset").unwrap();
        let copied = temp.path().join("copied");
        copy_path(&source, &copied).unwrap();
        assert_eq!(
            std::fs::read(copied.join("nested/file.txt")).unwrap(),
            b"asset"
        );
        assert!(matches!(
            copy_path(&source, &copied),
            Err(CoreError::PathAlreadyExists(_))
        ));
    }

    #[test]
    fn moves_then_renames_without_replacing() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.txt");
        let folder = temp.path().join("folder");
        std::fs::write(&source, b"x").unwrap();
        std::fs::create_dir(&folder).unwrap();
        let moved = folder.join("source.txt");
        move_path(&source, &moved).unwrap();
        assert!(!source.exists());
        let renamed = rename_path(&moved, "renamed.txt").unwrap();
        assert!(renamed.ends_with("renamed.txt"));
        assert!(renamed.exists());
        assert!(rename_path(&renamed, "../escape").is_err());
    }

    #[test]
    fn deletes_files_and_directories_and_rejects_missing_paths() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file.txt");
        std::fs::write(&file, b"x").unwrap();
        delete_path(&file).unwrap();
        assert!(!file.exists());
        let folder = temp.path().join("folder");
        std::fs::create_dir_all(folder.join("nested")).unwrap();
        delete_path(&folder).unwrap();
        assert!(!folder.exists());
        assert!(matches!(
            delete_path(&file),
            Err(CoreError::PathNotFound(_))
        ));
    }

    #[test]
    fn folder_transfer_skips_same_folder_and_reports_collisions() {
        let temp = tempfile::tempdir().unwrap();
        let source_folder = temp.path().join("source");
        let destination = temp.path().join("destination");
        std::fs::create_dir_all(&source_folder).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let source = source_folder.join("asset.txt");
        std::fs::write(&source, b"asset").unwrap();

        let moved_in_place = transfer_into_folder(
            std::slice::from_ref(&source),
            &source_folder,
            TransferMode::Move,
        )
        .unwrap();
        assert_eq!(
            (
                moved_in_place.transferred,
                moved_in_place.skipped,
                moved_in_place.errors.len()
            ),
            (0, 1, 0)
        );

        let copied = transfer_into_folder(
            std::slice::from_ref(&source),
            &destination,
            TransferMode::Copy,
        )
        .unwrap();
        assert_eq!(
            (copied.transferred, copied.skipped, copied.errors.len()),
            (1, 0, 0)
        );
        assert_eq!(
            std::fs::read(destination.join("asset.txt")).unwrap(),
            b"asset"
        );

        let collided = transfer_into_folder(
            std::slice::from_ref(&source),
            &destination,
            TransferMode::Copy,
        )
        .unwrap();
        assert_eq!((collided.transferred, collided.skipped), (0, 0));
        assert_eq!(collided.errors.len(), 1);
        assert_eq!(collided.errors[0].0, source);
        assert!(matches!(
            collided.errors[0].1,
            CoreError::PathAlreadyExists(_)
        ));
    }

    #[test]
    fn continues_batch_after_per_item_failures() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("destination");
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(destination.join("b.txt"), b"existing").unwrap();
        let a = temp.path().join("a.txt");
        let b = temp.path().join("b.txt");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();

        let report =
            transfer_into_folder(&[a.clone(), b.clone()], &destination, TransferMode::Move)
                .unwrap();
        assert_eq!(
            (report.transferred, report.skipped, report.errors.len()),
            (1, 0, 1)
        );
        assert_eq!(report.errors[0].0, b);
        assert!(matches!(
            report.errors[0].1,
            CoreError::PathAlreadyExists(_)
        ));
        assert!(!a.exists());
        assert!(b.exists());
    }

    #[test]
    fn rejects_transfer_into_self_or_descendant_without_moving_anything() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("folder");
        let child = folder.join("child");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(child.join("keep.txt"), b"keep").unwrap();
        let healthy = temp.path().join("healthy.txt");
        std::fs::write(&healthy, b"healthy").unwrap();

        // A folder transferred into itself or an existing child is rejected
        // in both modes.
        for mode in [TransferMode::Move, TransferMode::Copy] {
            assert!(matches!(
                transfer_into_folder(std::slice::from_ref(&folder), &folder, mode),
                Err(CoreError::InvalidOperation(_))
            ));
            assert!(matches!(
                transfer_into_folder(std::slice::from_ref(&folder), &child, mode),
                Err(CoreError::InvalidOperation(_))
            ));
        }

        // A rejected batch validates up front: nothing in it is transferred.
        assert!(matches!(
            transfer_into_folder(
                &[healthy.clone(), folder.clone()],
                &child,
                TransferMode::Move
            ),
            Err(CoreError::InvalidOperation(_))
        ));
        assert!(healthy.exists());
        assert_eq!(std::fs::read(child.join("keep.txt")).unwrap(), b"keep");

        // Destination must be a folder, and a missing destination stays missing.
        assert!(matches!(
            transfer_into_folder(std::slice::from_ref(&healthy), &healthy, TransferMode::Move),
            Err(CoreError::InvalidOperation(_))
        ));
        assert!(matches!(
            transfer_into_folder(
                std::slice::from_ref(&folder),
                &folder.join("nope"),
                TransferMode::Move
            ),
            Err(CoreError::PathNotFound(_))
        ));
    }

    #[test]
    #[cfg(unix)]
    fn move_skips_symlink_already_in_destination() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("destination");
        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir(&destination).unwrap();
        std::fs::create_dir(&elsewhere).unwrap();
        let target = elsewhere.join("target.txt");
        std::fs::write(&target, b"x").unwrap();
        let link = destination.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let report = transfer_into_folder(
            std::slice::from_ref(&link),
            &destination,
            TransferMode::Move,
        )
        .unwrap();
        assert_eq!(
            (report.transferred, report.skipped, report.errors.len()),
            (0, 1, 0)
        );
        assert!(std::fs::symlink_metadata(&link).is_ok());
    }

    #[test]
    fn refuses_copy_into_own_subtree() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        assert!(matches!(
            copy_path(&source, &source.join("child")),
            Err(CoreError::InvalidOperation(_))
        ));
    }
}
