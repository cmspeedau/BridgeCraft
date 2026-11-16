//! Browser state: the content list for the current root plus selection and sort.

use crate::entry::Entry;
use crate::fs::{sort_entries, SortDir, SortKey, TagFilter, TagValues};
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
    /// Tag values for the current folder, as the metadata index recorded them.
    /// Only consulted by the tag sort keys and [`tag_filter`](Self::tag_filter),
    /// so an unrated folder still lists, sorts and filters by name exactly as
    /// it did before the index existed.
    pub tags: TagValues,
    /// Rating, label and flag filter. Empty means every file passes.
    pub tag_filter: TagFilter,
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
        sort_entries(&mut self.entries, self.sort_key, self.sort_dir, &self.tags);
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
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                // A folder is never hidden by a tag filter: it has no rating of
                // its own, and the tree has to stay navigable while filtered.
                if e.is_dir {
                    return true;
                }
                if !needle.is_empty()
                    && !e
                        .path
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().to_lowercase().contains(&needle))
                {
                    return false;
                }
                self.tag_filter.matches(&self.tags, &e.path)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Change the sort order and re-sort without re-reading the directory.
    pub fn set_sort(&mut self, key: SortKey, dir: SortDir) {
        self.sort_key = key;
        self.sort_dir = dir;
        sort_entries(&mut self.entries, self.sort_key, self.sort_dir, &self.tags);
        // Keep selection valid: indices now refer to different items, so clear.
        self.selection.clear();
    }

    /// Replace the folder's tag values with ones read from the metadata index,
    /// and re-sort if the current sort key is a tag key. Without the re-sort a
    /// folder would keep showing the order computed from the *previous*
    /// folder's rows until the user touched the sort menu.
    pub fn set_tag_values(&mut self, tags: TagValues) {
        self.tags = tags;
        if self.sort_key.needs_tags() {
            sort_entries(&mut self.entries, self.sort_key, self.sort_dir, &self.tags);
        }
    }

    /// Replace one file's tag values after the index row was rewritten, and
    /// re-sort if a tag key is in charge.
    ///
    /// Without this, starring a file in a folder sorted by rating would leave
    /// the row where it was until the next watcher pass, which reads as the
    /// save having been ignored.
    pub fn update_tag(&mut self, path: std::path::PathBuf, value: crate::fs::TagValue) {
        self.tags.insert(path, value);
        if self.sort_key.needs_tags() {
            sort_entries(&mut self.entries, self.sort_key, self.sort_dir, &self.tags);
        }
    }

    /// Apply a rating/label/flag filter and report how many entries pass.
    /// Selection is preserved by path for the files that stay visible, the same
    /// way the name filter preserves it.
    pub fn set_tag_filter(&mut self, filter: TagFilter) -> usize {
        self.tag_filter = filter;
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

    /// Files the index has no row for, counted over the whole folder. A tag
    /// filter hides those files, and a tag sort puts them at one end of the
    /// order, so the interface has to say how many are guesses rather than let
    /// a filtered-to-empty grid look like an empty folder.
    pub fn unindexed_len(&self) -> usize {
        self.tags.missing_among(
            self.entries
                .iter()
                .filter(|e| !e.is_dir)
                .map(|e| e.path.as_path()),
        )
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

    fn tagged_state(dir: &std::path::Path) -> (BrowserState, Vec<std::path::PathBuf>) {
        let mut st = BrowserState::default();
        st.open(dir).unwrap();
        let paths: Vec<std::path::PathBuf> = st
            .entries
            .iter()
            .filter(|e| !e.is_dir)
            .map(|e| e.path.clone())
            .collect();
        (st, paths)
    }

    fn value(rating: u8, label: &str) -> crate::fs::TagValue {
        crate::fs::TagValue {
            rating,
            label: (!label.is_empty()).then(|| label.to_string()),
            flag: None,
        }
    }

    #[test]
    fn a_tag_sort_orders_the_folder_by_the_values_the_index_supplied() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("low.jpg"), b"x").unwrap();
        std::fs::write(dir.path().join("high.jpg"), b"x").unwrap();
        std::fs::write(dir.path().join("mid.jpg"), b"x").unwrap();
        let (mut st, paths) = tagged_state(dir.path());

        let mut tags = crate::fs::TagValues::default();
        // Ratings keyed by name, not by position: `entries` is already in name
        // order, and zipping ratings onto it would quietly test whatever order
        // the listing happened to have.
        for path in &paths {
            let rating = match path.file_name().unwrap().to_string_lossy().as_ref() {
                "high.jpg" => 5,
                "mid.jpg" => 3,
                "low.jpg" => 1,
                other => panic!("unexpected file {other}"),
            };
            tags.insert(path.clone(), value(rating, ""));
        }
        st.set_tag_values(tags);
        st.set_sort(SortKey::Rating, SortDir::Desc);
        let order: Vec<String> = st
            .entries
            .iter()
            .map(|e| e.name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(order, ["high.jpg", "mid.jpg", "low.jpg"]);

        // A refresh re-lists from disk, so the sort has to be re-applied with
        // the values still in hand rather than falling back to name order.
        st.refresh().unwrap();
        assert_eq!(st.entries[0].name(), "high.jpg");
    }

    #[test]
    fn new_tag_values_re_sort_a_tag_sort_and_leave_a_name_sort_alone() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jpg"), b"x").unwrap();
        std::fs::write(dir.path().join("b.jpg"), b"x").unwrap();
        let (mut st, paths) = tagged_state(dir.path());

        // Sorting by name must not shuffle when the index arrives: the user is
        // looking at a name-ordered grid and nothing about it changed.
        let mut tags = crate::fs::TagValues::default();
        tags.insert(paths[1].clone(), value(5, ""));
        tags.insert(paths[0].clone(), value(1, ""));
        st.set_tag_values(tags.clone());
        st.set_sort(SortKey::Name, SortDir::Asc);
        let before: Vec<String> = st
            .entries
            .iter()
            .map(|e| e.name().to_string_lossy().into_owned())
            .collect();
        let mut swapped = crate::fs::TagValues::default();
        swapped.insert(paths[0].clone(), value(5, ""));
        swapped.insert(paths[1].clone(), value(1, ""));
        st.set_tag_values(swapped);
        let after: Vec<String> = st
            .entries
            .iter()
            .map(|e| e.name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(before, after, "a name sort moved when tags changed");

        // Under a tag sort the same change must reorder immediately, or the
        // grid would show yesterday's ratings until the user touched the menu.
        st.set_sort(SortKey::Rating, SortDir::Desc);
        assert_eq!(st.entries[0].name(), "a.jpg");
        st.set_tag_values(tags);
        assert_eq!(
            st.entries[0].name(),
            "b.jpg",
            "the folder kept its order after the values behind it changed"
        );
    }

    #[test]
    fn a_tag_filter_hides_files_the_index_has_not_read_and_keeps_folders() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("rated.jpg"), b"x").unwrap();
        std::fs::write(dir.path().join("unknown.jpg"), b"x").unwrap();
        std::fs::write(dir.path().join("unrated.jpg"), b"x").unwrap();
        let (mut st, paths) = tagged_state(dir.path());

        let rated = paths.iter().find(|p| p.ends_with("rated.jpg")).unwrap();
        let unrated = paths.iter().find(|p| p.ends_with("unrated.jpg")).unwrap();
        let mut tags = crate::fs::TagValues::default();
        tags.insert(rated.clone(), value(4, "Red"));
        tags.insert(unrated.clone(), value(0, ""));
        st.set_tag_values(tags);

        let visible = st.set_tag_filter(crate::fs::TagFilter {
            min_rating: Some(3),
            label: None,
            flag: None,
        });
        // `unknown.jpg` has no row at all, so it is hidden with the unrated
        // file; the subfolder stays so the user can still navigate out.
        assert_eq!(visible, 2, "expected the folder plus the four-star file");
        assert_eq!(st.visible_len(), 2);
        assert_eq!(
            st.unindexed_len(),
            1,
            "the count of files the index has not read must be visible to the status bar"
        );

        // Clearing the filter brings everything back.
        assert_eq!(st.set_tag_filter(crate::fs::TagFilter::default()), 4);
    }

    #[test]
    fn a_tag_filter_keeps_the_selection_that_still_matches() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["red.jpg", "blue.jpg"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let (mut st, paths) = tagged_state(dir.path());
        let mut tags = crate::fs::TagValues::default();
        for path in &paths {
            let label = if path.ends_with("red.jpg") {
                "Red"
            } else {
                "Blue"
            };
            tags.insert(path.clone(), value(0, label));
        }
        st.set_tag_values(tags);
        let red = (0..st.len())
            .find(|&i| st.entry(i).unwrap().name() == "red.jpg")
            .unwrap();
        st.selection.select_only(red);

        st.set_tag_filter(crate::fs::TagFilter {
            min_rating: None,
            label: Some("Red".to_string()),
            flag: None,
        });
        assert_eq!(
            st.selection.indices().count(),
            1,
            "the file that still passes the filter must stay selected"
        );

        st.set_tag_filter(crate::fs::TagFilter {
            min_rating: None,
            label: Some("Blue".to_string()),
            flag: None,
        });
        assert_eq!(
            st.selection.indices().count(),
            0,
            "a selection filtered out of sight must not stay selected"
        );
    }

    #[test]
    fn a_tag_sort_and_filter_on_an_unread_folder_behaves_like_no_index_at_all() {
        // The default `TagValues` is empty, which is the state before the first
        // pass finishes or with indexing switched off. A name sort must be
        // untouched by it, and a tag filter must not empty the grid on its own.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jpg"), b"x").unwrap();
        std::fs::write(dir.path().join("b.jpg"), b"x").unwrap();
        let (mut st, _) = tagged_state(dir.path());
        assert!(st.tags.is_empty());
        assert_eq!(st.unindexed_len(), 2);
        st.set_sort(SortKey::Rating, SortDir::Asc);
        assert_eq!(st.len(), 2, "an unread folder must still list its files");
        assert_eq!(st.visible_len(), 2, "an inactive filter hid files");
    }
}
