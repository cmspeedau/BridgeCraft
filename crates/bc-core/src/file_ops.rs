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
            "refusing to copy symlink {}", source.display()
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
            "unsupported filesystem object: {}", source.display()
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
            "unsupported filesystem object: {}", path.display()
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
        assert_eq!(std::fs::read(copied.join("nested/file.txt")).unwrap(), b"asset");
        assert!(matches!(copy_path(&source, &copied), Err(CoreError::PathAlreadyExists(_))));
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
        assert!(matches!(delete_path(&file), Err(CoreError::PathNotFound(_))));
    }

    #[test]
    fn refuses_copy_into_own_subtree() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        assert!(matches!(copy_path(&source, &source.join("child")), Err(CoreError::InvalidOperation(_))));
    }
}
