//! bc-core: filesystem scanning, watchers, selection model, and app state.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("path not found: {0}")]
    PathNotFound(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CoreError>;

/// A single item (file or folder) shown in the browser grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: std::path::PathBuf,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<std::time::SystemTime>,
}

/// Recursively list entries under `root` (non-recursive by default later;
/// simple recursive walk for the Phase 0 skeleton).
pub fn scan(root: &std::path::Path) -> Result<Vec<Entry>> {
    if !root.exists() {
        return Err(CoreError::PathNotFound(root.display().to_string()));
    }
    let mut entries = Vec::new();
    for e in walkdir::WalkDir::new(root).follow_links(false) {
        let e = e.map_err(|err| CoreError::Io(std::io::Error::other(err)))?;
        let meta = e.metadata().map_err(|err| CoreError::Io(std::io::Error::other(err)))?;
        entries.push(Entry {
            path: e.path().to_path_buf(),
            is_dir: meta.is_dir(),
            size: meta.len(),
            modified: meta.modified().ok(),
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_lists_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let entries = scan(dir.path()).unwrap();
        assert!(entries.iter().any(|e| e.path.ends_with("a.txt")));
        assert!(entries.iter().any(|e| e.path == dir.path()));
    }

    #[test]
    fn scan_missing_root_errors() {
        assert!(scan(std::path::Path::new("/does/not/exist")).is_err());
    }
}
