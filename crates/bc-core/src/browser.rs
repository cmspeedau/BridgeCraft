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
    /// Case-insensitive substring filter on file names; empty means no filter.
    /// Directories are always shown so navigation keeps working while filtered.
    pub filter: String,
}

impl BrowserState {
    /// Open a new root: scan its direct children and apply the current sort.
    /// The filter is kept (matching Bridge's persistent filter behavior).
    pub fn open(&mut self, root: &std::path::Path) -> Result<()> {
        if !root.exists() {
            return Err(CoreError::PathNotFound(root.display().to_string()));
        }
        self.root = Some(root.to_path_buf());
        self.refresh()
    }

    /// Re-list the current root, preserving sort and filter settings.
    /// Selection is preserved by path for entries that still exist.
    pub fn refresh(&mut self) -> Result<()> {
        let root = self
            .root
            .as_ref()
            .ok_or_else(|| CoreError::PathNotFound("<no root open>".to_string()))?;
        self.entries = crate::fs::list_dir(root)?;
        sort_entries(&mut self.entries, self.sort_key, self.sort_dir);
        Ok(())
    }

    /// Re-list the current root, restoring selection for paths that remain.
    pub fn refresh_preserving_selection(&mut self) -> Result<()> {
        let selected: Vec<std::path::PathBuf> = self
            .selection
            .indices()
            .filter_map(|i| self.entries.get(i).map(|e| e.path.clone()))
            .collect();
        self.refresh()?;
        self.selection.clear();
        for (idx, entry) in self.entries.iter().enumerate() {
            if selected.contains(&entry.path) {
                self.selection.toggle(idx);
            }
        }
        Ok(())
    }

    /// Set the filter and return the new visible count.
    /// Selection is preserved by path for still-visible entries.
    pub fn set_filter(&mut self, filter: impl Into<String>) -> usize {
        self.filter = filter.into();
        let visible = self.visible_indices();
        let still_selected: Vec<std::path::PathBuf> = self
            .selection
            .indices()
            .filter(|i| visible.contains(i))
            .filter_map(|i| self.entries.get(i).map(|e| e.path.clone()))
            .collect();
        self.selection.clear();
        for (idx, entry) in self.entries.iter().enumerate() {
            if still_selected.contains(&entry.path) {
                self.selection.toggle(idx);
            }
        }
        visible.len()
    }

    /// Indices into `entries` that pass the current filter.
    /// Matching is a case-insensitive substring test on the file name;
    /// directories are always included so navigation keeps working.
    pub fn visible_indices(&self) -> Vec<usize> {
        let needle = self.filter.trim().to_lowercase();
        if needle.is_empty() {
            return (0..self.entries.len()).collect();
        }
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                e.is_dir
                    || e.path
                        .file_name()
                        .map_or(false, |n| n.to_string_lossy().to_lowercase().contains(&needle))
            })
            .map(|(i, _)| i)
            .collect()
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

    /// Number of entries passing the current filter.
    pub fn visible_len(&self) -> usize {
        self.visible_indices().len()
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

    #[test]
    fn filter_is_case_insensitive_substring_and_keeps_dirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Report.PNG"), b"x").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"x").unwrap();
        std::fs::create_dir(dir.path().join("Archive")).unwrap();

        let mut st = BrowserState::default();
        st.open(dir.path()).unwrap();

        // Uppercase "NOT" matches notes.txt case-insensitively;
        // Archive (dir) is always shown regardless of the filter.
        assert_eq!(st.set_filter("NOT"), 2);
        let vis = st.visible_indices();
        let names: Vec<String> = vis
            .iter()
            .map(|&i| st.entry(i).unwrap().name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n == "notes.txt"));
        assert!(names.iter().any(|n| n == "Archive"));
        assert!(names.iter().all(|n| n != "Report.PNG"));

        // Empty/whitespace filter shows everything again.
        assert_eq!(st.set_filter("   "), 3);
    }

    #[test]
    fn filter_preserves_selection_of_still_visible_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.txt"), b"x").unwrap();
        std::fs::write(dir.path().join("drop.txt"), b"x").unwrap();

        let mut st = BrowserState::default();
        st.open(dir.path()).unwrap();
        // Look up keep.txt's index instead of hardcoding it, so the test
        // doesn't depend on sort order.
        let keep_idx = (0..st.len())
            .find(|&i| st.entry(i).unwrap().name() == "keep.txt")
            .unwrap();
        st.selection.select_only(keep_idx);
        st.set_filter("keep");
        assert_eq!(st.selection.indices().count(), 1);
        st.set_filter("");
        assert_eq!(st.selection.indices().count(), 1);
    }

    #[test]
    fn refresh_preserving_selection_restores_by_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();

        let mut st = BrowserState::default();
        st.open(dir.path()).unwrap();
        st.selection.select_only(0);

        // A new file appears; a plain refresh would clear selection.
        std::fs::write(dir.path().join("b.txt"), b"x").unwrap();
        st.refresh_preserving_selection().unwrap();
        assert_eq!(st.len(), 2);
        assert_eq!(st.selection.indices().collect::<Vec<_>>(), vec![0]);
    }
}
