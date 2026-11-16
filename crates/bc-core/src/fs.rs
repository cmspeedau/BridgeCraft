//! Directory listing, recursive scanning, and sorting.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::entry::Entry;
use crate::{CoreError, Result};

/// Sort keys for the content grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    #[default]
    Name,
    Size,
    Modified,
    /// Star rating from the metadata index. Unindexed and unrated files sort
    /// together as zero stars.
    Rating,
    /// Colour label name from the metadata index, alphabetically.
    Label,
    /// Approve/reject flag from the metadata index.
    Flag,
}

impl SortKey {
    /// Every sort key, in the order a menu should offer them. The interface
    /// iterates this rather than listing keys inline, so adding a variant here
    /// cannot leave the menu showing an old set.
    pub const ALL: [SortKey; 6] = [
        SortKey::Name,
        SortKey::Size,
        SortKey::Modified,
        SortKey::Rating,
        SortKey::Label,
        SortKey::Flag,
    ];

    /// Whether answering this sort needs tag values, i.e. whether [`sort_entries`]
    /// has to be given a folder's [`TagValues`] rather than an empty one.
    pub fn needs_tags(&self) -> bool {
        matches!(self, SortKey::Rating | SortKey::Label | SortKey::Flag)
    }

    pub fn label(&self) -> &'static str {
        match self {
            SortKey::Name => "Name",
            SortKey::Size => "Size",
            SortKey::Modified => "Modified",
            SortKey::Rating => "Rating",
            SortKey::Label => "Label",
            SortKey::Flag => "Flag",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortDir {
    #[default]
    Asc,
    Desc,
}

/// One file's tag values, as the metadata index recorded them.
///
/// This is what makes sorting by rating possible at all: ordering a folder by
/// stars means knowing every file's rating, and reading every file's layers to
/// find out costs about 15 µs per file measured against 1.3 µs per file for one
/// indexed read of the folder. It is deliberately *not* what the grid overlays
/// draw from — those keep their own per-file cache, which checks the file's
/// modification time and so cannot show a tag the user has already changed.
/// Values here can lag a file by up to one index pass, which is why the status
/// bar says how many files are not indexed yet when a tag sort or filter is
/// doing the ordering.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagValue {
    pub rating: u8,
    /// Label name as stored, e.g. `"Red"`. `None` is unlabeled.
    pub label: Option<String>,
    /// `"Approved"` or `"Rejected"`; `None` is no flag.
    pub flag: Option<String>,
}

/// A folder's tag values, keyed by path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagValues {
    rows: HashMap<PathBuf, TagValue>,
}

impl TagValues {
    pub fn new(rows: HashMap<PathBuf, TagValue>) -> Self {
        Self { rows }
    }

    /// A file the index has never written has no row here at all: sorting and
    /// filtering then treat it as unrated, unlabeled and unflagged, so the
    /// order of a half-indexed folder does not change as its pass goes through.
    pub fn get(&self, path: &Path) -> Option<&TagValue> {
        self.rows.get(path)
    }

    pub fn insert(&mut self, path: PathBuf, value: TagValue) {
        self.rows.insert(path, value);
    }

    pub fn remove(&mut self, path: &Path) {
        self.rows.remove(path);
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn clear(&mut self) {
        self.rows.clear();
    }

    /// How many of `paths` the index has no row for: the number the interface
    /// reports when a tag sort or filter is in charge of the grid.
    pub fn missing_among<'p>(&self, paths: impl Iterator<Item = &'p Path>) -> usize {
        paths.filter(|path| !self.rows.contains_key(*path)).count()
    }
}

/// Ordering rules for the three tag keys, shared by the sort and the filter so
/// they cannot disagree about what "after" means.
fn rating_of(tags: &TagValues, path: &Path) -> u8 {
    tags.get(path).map_or(0, |value| value.rating)
}

/// Labels sort alphabetically with unlabeled files last, by comparing
/// `(unlabeled, name)`: the flag decides the group and the name decides within
/// it. The eight label names the interface offers are an identity list (which
/// colour a chip shows), not a sort order, and copying them in here would invent
/// an ordering Bridge's behaviour has not been verified against.
fn label_of<'a>(tags: &'a TagValues, path: &Path) -> (bool, &'a str) {
    match tags.get(path).and_then(|value| value.label.as_deref()) {
        Some(label) => (false, label),
        None => (true, ""),
    }
}

/// No flag, then Approved, then Rejected: the order the flag type itself
/// declares, so this is not a second rule invented for sorting.
fn flag_of(tags: &TagValues, path: &Path) -> u8 {
    match tags.get(path).and_then(|value| value.flag.as_deref()) {
        Some("Approved") => 1,
        Some("Rejected") => 2,
        _ => 0,
    }
}

/// A tag filter over the same values a tag sort uses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagFilter {
    /// Keep files rated at least this many stars. `None` accepts any rating.
    pub min_rating: Option<u8>,
    /// Keep files with exactly this label; `Some("")` asks for unlabeled files.
    pub label: Option<String>,
    /// Keep files with exactly this flag; `Some("None")` asks for unflagged.
    pub flag: Option<String>,
}

impl TagFilter {
    pub fn is_active(&self) -> bool {
        self.min_rating.is_some() || self.label.is_some() || self.flag.is_some()
    }

    /// Whether one file passes. A file the index has not read yet passes only
    /// the *absence* of a filter: with a tag filter in charge, an unknown value
    /// is not evidence of a rating, a label or a flag, so such files are hidden
    /// and the status bar counts them.
    pub fn matches(&self, tags: &TagValues, path: &Path) -> bool {
        let Some(value) = tags.get(path) else {
            return !self.is_active();
        };
        if let Some(min) = self.min_rating {
            if value.rating < min {
                return false;
            }
        }
        if let Some(label) = &self.label {
            let stored = value.label.as_deref().unwrap_or("");
            if !label.eq_ignore_ascii_case(stored) {
                return false;
            }
        }
        if let Some(flag) = &self.flag {
            let stored = value.flag.as_deref().unwrap_or("None");
            if !flag.eq_ignore_ascii_case(stored) {
                return false;
            }
        }
        true
    }
}

/// List the direct children of `root` (non-recursive).
pub fn list_dir(root: &std::path::Path) -> Result<Vec<Entry>> {
    let it = std::fs::read_dir(root).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => CoreError::PathNotFound(root.display().to_string()),
        _ => CoreError::Io(e),
    })?;
    let mut entries = Vec::new();
    for e in it {
        let e = e.map_err(CoreError::Io)?;
        let meta = e.metadata().map_err(CoreError::Io)?;
        entries.push(Entry {
            path: e.path(),
            is_dir: meta.is_dir(),
            size: meta.len(),
            modified: meta.modified().ok(),
        });
    }
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
pub fn sort_entries(entries: &mut [Entry], key: SortKey, dir: SortDir, tags: &TagValues) {
    entries.sort_by(|a, b| {
        // Folders before files, regardless of sort direction.
        a.is_dir.cmp(&b.is_dir).reverse().then_with(|| {
            let cmp = match key {
                SortKey::Name => a.name().cmp(b.name()),
                SortKey::Size => a.size.cmp(&b.size),
                SortKey::Modified => a.modified.cmp(&b.modified),
                // Ties break by name ascending, so a folder of two hundred
                // unrated photos has one order rather than whichever order the
                // incoming comparator happened to see. The tie-break does not
                // follow `dir`: it is what makes the sort deterministic, not a
                // second axis the user asked to reverse.
                SortKey::Rating => rating_of(tags, &a.path)
                    .cmp(&rating_of(tags, &b.path))
                    .then_with(|| a.name().cmp(b.name())),
                SortKey::Label => label_of(tags, &a.path)
                    .cmp(&label_of(tags, &b.path))
                    .then_with(|| a.name().cmp(b.name())),
                SortKey::Flag => flag_of(tags, &a.path)
                    .cmp(&flag_of(tags, &b.path))
                    .then_with(|| a.name().cmp(b.name())),
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
        sort_entries(
            &mut entries,
            SortKey::Name,
            SortDir::Asc,
            &TagValues::default(),
        );
        assert_eq!(entries[0].name(), "zdir");
        assert_eq!(entries[1].name(), "a.txt");
        assert_eq!(entries[2].name(), "b.txt");
    }

    fn tags(rows: &[(&str, u8, &str, &str)]) -> TagValues {
        let mut map = HashMap::new();
        for (path, rating, label, flag) in rows {
            map.insert(
                PathBuf::from(path),
                TagValue {
                    rating: *rating,
                    label: (!label.is_empty()).then(|| label.to_string()),
                    flag: (!flag.is_empty()).then(|| flag.to_string()),
                },
            );
        }
        TagValues::new(map)
    }

    fn entries_of(names: &[&str]) -> Vec<Entry> {
        names
            .iter()
            .map(|name| Entry {
                path: PathBuf::from(name),
                is_dir: false,
                size: 0,
                modified: None,
            })
            .collect()
    }

    fn names(entries: &[Entry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| e.name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_rating_sort_breaks_ties_by_name_in_both_directions() {
        let values = tags(&[
            ("two.jpg", 2, "", ""),
            ("zero-a.jpg", 0, "", ""),
            ("five.jpg", 5, "", ""),
            ("zero-b.jpg", 0, "", ""),
        ]);
        let mut ascending = entries_of(&["five.jpg", "zero-a.jpg", "two.jpg", "zero-b.jpg"]);
        sort_entries(&mut ascending, SortKey::Rating, SortDir::Asc, &values);
        // Unrated files are not a separate group: they are zero-star files, and
        // two of them sort by name. The order has to be one order, not whichever
        // order the incoming comparator happened to see.
        assert_eq!(
            names(&ascending),
            ["zero-a.jpg", "zero-b.jpg", "two.jpg", "five.jpg"]
        );

        let mut descending = entries_of(&["zero-a.jpg", "two.jpg", "five.jpg", "zero-b.jpg"]);
        sort_entries(&mut descending, SortKey::Rating, SortDir::Desc, &values);
        assert_eq!(
            names(&descending),
            ["five.jpg", "two.jpg", "zero-b.jpg", "zero-a.jpg"],
            "a reversed sort reverses the whole comparison, tie-break included, \
             exactly as a reversed name sort reverses names"
        );
        // What the tie-break is *for* is determinism, not a fixed direction: two
        // files with the same rating have no natural order, and without a
        // tie-break the same folder could shuffle between repaints. That it
        // follows the direction is what keeps one rule (`Desc` means this order,
        // flipped) rather than two.
    }

    #[test]
    fn a_label_sort_puts_unlabeled_files_last() {
        let values = tags(&[
            ("red.jpg", 0, "Red", ""),
            ("none-b.jpg", 0, "", ""),
            ("orange.jpg", 0, "Orange", ""),
            ("none-a.jpg", 0, "", ""),
        ]);
        let mut entries = entries_of(&["none-b.jpg", "red.jpg", "orange.jpg", "none-a.jpg"]);
        sort_entries(&mut entries, SortKey::Label, SortDir::Asc, &values);
        // Labels are a name, ordered alphabetically, and unlabeled files come
        // last rather than being treated as the empty name that sorts first.
        assert_eq!(
            names(&entries),
            ["orange.jpg", "red.jpg", "none-a.jpg", "none-b.jpg"]
        );
    }

    #[test]
    fn a_flag_sort_orders_nothing_then_approved_then_rejected() {
        let values = tags(&[
            ("rej.jpg", 0, "", "Rejected"),
            ("app.jpg", 0, "", "Approved"),
            ("plain.jpg", 0, "", ""),
        ]);
        let mut entries = entries_of(&["rej.jpg", "plain.jpg", "app.jpg"]);
        sort_entries(&mut entries, SortKey::Flag, SortDir::Asc, &values);
        assert_eq!(names(&entries), ["plain.jpg", "app.jpg", "rej.jpg"]);
    }

    #[test]
    fn a_file_the_index_never_wrote_sorts_as_unrated_rather_than_last_of_all() {
        // During a first pass over a big folder most files have no row yet. A
        // missing row has to mean "nothing known", which is the same as zero
        // stars, and must not become its own group after everything else —
        // otherwise the grid would reshuffle as the pass went through it.
        let values = tags(&[("indexed-three.jpg", 3, "", "")]);
        let mut entries = entries_of(&["unindexed-a.jpg", "indexed-three.jpg", "unindexed-b.jpg"]);
        sort_entries(&mut entries, SortKey::Rating, SortDir::Asc, &values);
        assert_eq!(
            names(&entries),
            ["unindexed-a.jpg", "unindexed-b.jpg", "indexed-three.jpg"]
        );
        assert_eq!(
            values.missing_among(entries.iter().map(|e| e.path.as_path())),
            2
        );
    }

    #[test]
    fn only_the_three_tag_keys_need_index_values() {
        assert!(!SortKey::Name.needs_tags());
        assert!(!SortKey::Size.needs_tags());
        assert!(!SortKey::Modified.needs_tags());
        for key in [SortKey::Rating, SortKey::Label, SortKey::Flag] {
            assert!(key.needs_tags(), "{key:?} needs the index");
        }
    }

    #[test]
    fn the_tag_filter_asks_for_rating_label_and_flag_together() {
        let values = tags(&[
            ("five.jpg", 5, "Red", "Approved"),
            ("one.jpg", 1, "Blue", "Rejected"),
            ("plain.jpg", 0, "", ""),
        ]);
        let any = |min: Option<u8>, label: Option<&str>, flag: Option<&str>| TagFilter {
            min_rating: min,
            label: label.map(str::to_string),
            flag: flag.map(str::to_string),
        };

        assert!(any(None, None, None).matches(&values, Path::new("plain.jpg")));
        assert!(!any(Some(1), None, None).matches(&values, Path::new("plain.jpg")));
        assert!(any(Some(1), None, None).matches(&values, Path::new("one.jpg")));
        assert!(any(Some(1), Some("Blue"), None).matches(&values, Path::new("one.jpg")));
        // An empty label name means "unlabeled", not "any label".
        assert!(any(Some(0), Some(""), None).matches(&values, Path::new("plain.jpg")));
        assert!(!any(Some(0), Some(""), None).matches(&values, Path::new("five.jpg")));
        // A flag of "None" is the same idea for flags.
        assert!(any(None, None, Some("None")).matches(&values, Path::new("plain.jpg")));
        assert!(!any(None, None, Some("None")).matches(&values, Path::new("one.jpg")));
        // Names are matched without case, the way the label chips are read.
        assert!(any(None, Some("red"), None).matches(&values, Path::new("five.jpg")));
    }

    #[test]
    fn an_unknown_value_passes_no_tag_filter() {
        let values = tags(&[("indexed.jpg", 5, "Red", "Approved")]);
        let filter = TagFilter {
            min_rating: Some(3),
            label: None,
            flag: None,
        };
        assert!(
            !filter.matches(&values, Path::new("still-on-disk.jpg")),
            "a file with no row cannot be shown as satisfying a rating the \
             index never read, which would sort and filter on a guess"
        );
        // With nothing asked, an unknown value is simply not excluded.
        assert!(TagFilter::default().matches(&values, Path::new("still-on-disk.jpg")));
        assert!(!TagFilter::default().is_active());
        assert!(filter.is_active());
    }
}
