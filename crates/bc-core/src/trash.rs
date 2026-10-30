//! A BridgeCraft-managed per-user bin. Items are moved (never permanently
//! deleted) and can be listed or restored to their original paths.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{recents::user_data_dir, CoreError, Result};

const BIN_DIR: &str = "bin";
const INDEX_FILE: &str = "index.json";
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashEntry {
    pub id: String,
    pub original_path: PathBuf,
    pub deleted_at_unix_secs: u64,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoredEntry {
    id: String,
    original_path: String,
    stored_path: String,
    deleted_at_unix_secs: u64,
}

/// File operations against BridgeCraft's managed, restorable bin.
#[derive(Debug)]
pub struct TrashBin {
    root: PathBuf,
    entries: Vec<StoredEntry>,
}

impl TrashBin {
    /// Open the current user's BridgeCraft bin.
    pub fn open_default() -> Result<Self> {
        Self::open(user_data_dir()?.join("BridgeCraft").join(BIN_DIR))
    }

    /// Open a bin rooted at `root`; primarily useful for embedding and tests.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("items"))?;
        let index = root.join(INDEX_FILE);
        let entries = match fs::read(index) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                CoreError::InvalidOperation(format!("invalid trash index: {error}"))
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self { root, entries })
    }

    /// Move a path into the bin. The source remains untouched if the move fails.
    pub fn trash(&mut self, source: impl AsRef<Path>) -> Result<TrashEntry> {
        let source = source.as_ref();
        let original_path = if source.is_absolute() {
            source.to_path_buf()
        } else {
            std::env::current_dir()?.join(source)
        };
        let metadata = fs::symlink_metadata(source).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                CoreError::PathNotFound(source.display().to_string())
            } else {
                CoreError::Io(error)
            }
        })?;
        if !metadata.file_type().is_symlink() && !metadata.is_file() && !metadata.is_dir() {
            return Err(CoreError::InvalidOperation(format!(
                "unsupported filesystem object: {}",
                source.display()
            )));
        }
        let canonical_root = fs::canonicalize(&self.root)?;
        let absolute_source = fs::canonicalize(source)?;
        if absolute_source == canonical_root || absolute_source.starts_with(&canonical_root) {
            return Err(CoreError::InvalidOperation(
                "cannot move the managed bin into itself".into(),
            ));
        }
        let name = source
            .file_name()
            .ok_or_else(|| CoreError::InvalidOperation("source path has no file name".into()))?;
        let deleted_at_unix_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| CoreError::InvalidOperation(format!("system clock error: {error}")))?
            .as_secs();
        let id = loop {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| {
                    CoreError::InvalidOperation(format!("system clock error: {error}"))
                })?
                .as_nanos();
            let id = format!("{nanos}-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed));
            if !self.entries.iter().any(|entry| entry.id == id)
                && !self.root.join("items").join(&id).exists()
            {
                break id;
            }
        };
        let item_dir = self.root.join("items").join(&id);
        fs::create_dir(&item_dir)?;
        let stored_path = item_dir.join(name);
        if let Err(error) = fs::rename(&original_path, &stored_path) {
            let _ = fs::remove_dir(&item_dir);
            return Err(error.into());
        }
        let record = StoredEntry {
            id: id.clone(),
            original_path: original_path.to_string_lossy().into_owned(),
            stored_path: stored_path.to_string_lossy().into_owned(),
            deleted_at_unix_secs,
        };
        self.entries.push(record);
        if let Err(error) = self.persist() {
            let record = self
                .entries
                .pop()
                .ok_or_else(|| CoreError::InvalidOperation("trash index update failed".into()))?;
            let _ = fs::rename(&record.stored_path, &original_path);
            let _ = fs::remove_dir(item_dir);
            return Err(error);
        }
        Ok(TrashEntry {
            id,
            original_path,
            deleted_at_unix_secs,
        })
    }

    /// List currently indexed bin entries, oldest first.
    pub fn list(&self) -> Vec<TrashEntry> {
        self.entries
            .iter()
            .map(|entry| TrashEntry {
                id: entry.id.clone(),
                original_path: PathBuf::from(&entry.original_path),
                deleted_at_unix_secs: entry.deleted_at_unix_secs,
            })
            .collect()
    }

    /// Restore an item to its original path without replacing an existing item.
    pub fn restore(&mut self, id: &str) -> Result<PathBuf> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.id == id)
            .ok_or_else(|| CoreError::PathNotFound(format!("trash entry {id}")))?;
        let record = &self.entries[index];
        let original = PathBuf::from(&record.original_path);
        let stored = PathBuf::from(&record.stored_path);
        if fs::symlink_metadata(&original).is_ok() {
            return Err(CoreError::PathAlreadyExists(original.display().to_string()));
        }
        if let Some(parent) = original.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&stored, &original)?;
        let removed = self.entries.remove(index);
        if let Err(error) = self.persist() {
            let _ = fs::rename(&original, &stored);
            self.entries.insert(index, removed);
            return Err(error);
        }
        if let Some(parent) = stored.parent() {
            let _ = fs::remove_dir(parent);
        }
        Ok(original)
    }

    fn persist(&self) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&self.entries).map_err(|error| {
            CoreError::InvalidOperation(format!("could not encode trash index: {error}"))
        })?;
        fs::write(self.root.join(INDEX_FILE), bytes)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trashes_lists_persists_and_restores_without_overwriting() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("bin");
        let source = temp.path().join("assets/file.txt");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, b"keep me").unwrap();
        let mut bin = TrashBin::open(&root).unwrap();
        let entry = bin.trash(&source).unwrap();
        assert!(!source.exists());
        assert_eq!(bin.list(), vec![entry.clone()]);
        drop(bin);
        let mut bin = TrashBin::open(&root).unwrap();
        assert_eq!(bin.list(), vec![entry.clone()]);
        fs::write(&source, b"do not replace").unwrap();
        assert!(matches!(
            bin.restore(&entry.id),
            Err(CoreError::PathAlreadyExists(_))
        ));
        fs::remove_file(&source).unwrap();
        assert_eq!(bin.restore(&entry.id).unwrap(), source);
        assert_eq!(fs::read(&source).unwrap(), b"keep me");
        assert!(bin.list().is_empty());
    }

    #[test]
    fn trashes_directories_and_rejects_missing_and_bin_source() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("bin");
        let dir = temp.path().join("folder");
        fs::create_dir_all(dir.join("nested")).unwrap();
        fs::write(dir.join("nested/file"), b"data").unwrap();
        let mut bin = TrashBin::open(&root).unwrap();
        let entry = bin.trash(&dir).unwrap();
        assert!(matches!(
            bin.trash(temp.path().join("missing")),
            Err(CoreError::PathNotFound(_))
        ));
        assert!(matches!(
            bin.trash(&root),
            Err(CoreError::InvalidOperation(_))
        ));
        bin.restore(&entry.id).unwrap();
        assert_eq!(fs::read(dir.join("nested/file")).unwrap(), b"data");
    }
}
