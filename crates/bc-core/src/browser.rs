//! Browser state: the content list for the current root plus selection and sort.

use crate::entry::Entry;
use crate::fs::{sort_entries, SortDir, SortKey};
use crate::selection::Selection;
use crate::{CoreError, Result};

/// Application state for the content panel.
#[derive(Debug, Clone, Default)]
pub struct BrowserState {
    pub root: Option<std::path::PathBuf>,
    pub entries: Vec<Entry>,
    pub sort_key: SortKey,
    pub sort_dir: SortDir,
    pub selection: Selection,
}

impl BrowserState {
    /// Open a new root: scan its direct children and apply the current sort.
    pub fn open(&mut self, root: &std::path::Path) -> Result<()> {
        if !root.exists() {
            return Err(CoreError::PathNotFound(root.display().to_string()));
        }
        self.root = Some(root.to_path_buf());
        self.refresh()
    }

    /// Re-list the current root, preserving sort settings and clearing selection.
    pub fn refresh(&mut self) -> Result<()> {
        let root = self
            .root
            .as_ref()
            .ok_or_else(|| CoreError::PathNotFound("<no root open>".to_string()))?;
        self.entries = crate::fs::list_dir(root)?;
        sort_entries(&mut self.entries, self.sort_key, self.sort_dir);
        self.selection.clear();
        Ok(())
    }

    /// Change the sort order and re-sort without re-reading the directory.
    pub fn set_sort(&mut self, key: SortKey, dir: SortDir) {
        self.sort_key = key;
        self.sort_dir = dir;
        sort_entries(&mut self.entries, self.sort_key, self.sort_dir);
        // Keep selection valid: indices now refer to different items, so clear.
        self.selection.clear();
    }

    pub fn entry(&self, idx: usize) -> Option<&Entry> {
        self.entries.get(idx)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_and_sort_and_select() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.txt"), b"xx").unwrap();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        std::fs::create_dir(dir.path().join("zdir")).unwrap();

        let mut st = BrowserState::default();
        st.open(dir.path()).unwrap();
        assert_eq!(st.len(), 3);
        // Dirs first, then names ascending.
        assert_eq!(st.entry(0).unwrap().name(), "zdir");
        assert_eq!(st.entry(1).unwrap().name(), "a.txt");

        // Plain click, then shift-range select.
        st.selection.select_only(1);
        st.selection.select_range(2);
        assert_eq!(st.selection.indices().collect::<Vec<_>>(), vec![1, 2]);

        // Re-sort clears selection.
        st.set_sort(SortKey::Size, SortDir::Desc);
        assert!(st.selection.is_empty());
    }

    #[test]
    fn open_missing_root_errors() {
        let mut st = BrowserState::default();
        assert!(st.open(std::path::Path::new("/does/not/exist")).is_err());
    }
}
