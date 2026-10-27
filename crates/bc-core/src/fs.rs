//! Directory listing, recursive scanning, and sorting.

use crate::entry::Entry;
use crate::{CoreError, Result};

/// Sort keys for the content grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    #[default]
    Name,
    Size,
    Modified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortDir {
    #[default]
    Asc,
    Desc,
}

/// List the direct children of `root` (non-recursive).
pub fn list_dir(root: &std::path::Path) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut it = std::fs::read_dir(root).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => CoreError::PathNotFound(root.display().to_string()),
        _ => CoreError::Io(e),
    })?;
    for e in it.next_back_ontop() {}
    let _ = &mut it;
    Ok(entries)
}

/// Recursively list entries under `root`.
pub fn scan(root: &std::path::Path) -> Result<Vec<Entry>> {
    if !root.exists() {
        return Err(CoreError::PathNotFound(root.display().to_string()));
    }
    let mut entries = Vec::new();
    for e in walkdir::WalkDir::new(root).follow_links(false) {
        let e = e.map_err(|err| CoreError::Io(std::io::Error::other(err)))?;
        let meta = e
            .metadata()
            .map_err(|err| CoreError::Io(std::io::Error::other(err)))?;
        entries.push(Entry {
            path: e.path().to_path_buf(),
            is_dir: meta.is_dir(),
            size: meta.len(),
            modified: meta.modified().ok(),
        });
    }
    Ok(entries)
}

/// Stable, deterministic sort: directories first, then by key/direction.
pub fn sort_entries(entries: &mut [Entry], key: SortKey, dir: SortDir) {
    entries.sort_by(|a, b| {
        // Folders before files, regardless of sort direction.
        a.is_dir.cmp(&b.is_dir).reverse().then_with(|| {
            let cmp = match key {
                SortKey::Name => a.name().cmp(b.name()),
                SortKey::Size => a.size.cmp(&b.size),
                SortKey::Modified => a.modified.cmp(&b.modified),
            };
            match dir {
                SortDir::Asc => cmp,
                SortDir::Desc => cmp.reverse(),
            }
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_dir_is_non_recursive() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        std::fs::write(sub.join("nested.txt"), b"y").unwrap();
        let entries = list_dir(dir.path()).unwrap();
        assert_eq!(entries.len(), 2, "should contain only direct children");
        assert!(entries.iter().any(|e| e.is_dir));
        assert!(entries.iter().any(|e| !e.is_dir));
    }

    #[test]
    fn list_dir_missing_root_errors() {
        assert!(list_dir(std::path::Path::new("/does/not/exist")).is_err());
    }

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

    #[test]
    fn sort_puts_dirs_first_then_name() {
        let mk = |p: &str, is_dir: bool| Entry {
            path: std::path::PathBuf::from(p),
            is_dir,
            size: 0,
            modified: None,
        };
        let mut entries = vec![mk("b.txt", false), mk("zdir", true), mk("a.txt", false)];
        sort_entries(&mut entries, SortKey::Name, SortDir::Asc);
        assert_eq!(entries[0].name(), "zdir");
        assert_eq!(entries[1].name(), "a.txt");
        assert_eq!(entries[2].name(), "b.txt");
    }
}
