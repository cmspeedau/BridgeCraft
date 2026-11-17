//! Saved searches: named queries a user asked to keep.
//!
//! These used to live in a table inside `metadata.sqlite`. That was harmless
//! while nothing could write a row, and a real problem the moment a button
//! could: the metadata index is a cache. It is rebuilt freely, its rows are all
//! derived from files that are still on disk, and `EXTRACTOR_VERSION` exists
//! precisely so an outdated one can be thrown away. A saved search is the
//! opposite — nothing else holds it, so losing it is losing something the user
//! made. It now lives in its own JSON file beside favourites and recents, and
//! deleting or rebuilding the index cannot touch it.
//!
//! Two consequences of that framing, both deliberate:
//!
//! - Writes go through [`crate::paths::write_atomic`], so an interrupted save
//!   leaves the previous list intact instead of an empty file where the user's
//!   collections were.
//! - A file this module cannot read is reported and left alone. Nothing here
//!   overwrites an unparseable file: a malformed byte in the JSON would
//!   otherwise destroy every saved search on the next unrelated save, when the
//!   data is still in the file and one edit away from being recoverable.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::metadata_index::SavedSearch;
use crate::{CoreError, Result};

const FILE_NAME: &str = "saved-searches.json";

/// The shape of the file. Bumping it is a deliberate act: the reader below
/// refuses anything it does not understand rather than guessing, so a new
/// version needs a migration beside it, not just a bigger number.
const DOCUMENT_VERSION: u32 = 1;

/// One saved search with the name the user gave it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedSearch {
    pub name: String,
    #[serde(flatten)]
    pub search: SavedSearch,
}

/// The user's saved searches, persisted as JSON in the data directory.
#[derive(Debug, Clone)]
pub struct SavedSearches {
    file: PathBuf,
    entries: Vec<NamedSearch>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Document {
    version: u32,
    searches: Vec<NamedSearch>,
}

impl SavedSearches {
    /// Open the current user's saved searches. A missing file is an empty list,
    /// which is the normal first run and not an error.
    pub fn open_default() -> Result<Self> {
        Self::open(crate::paths::data_file(FILE_NAME)?)
    }

    /// Open a saved-search store at `file`.
    pub fn open(file: impl Into<PathBuf>) -> Result<Self> {
        let file = file.into();
        let entries = match std::fs::read(&file) {
            Ok(bytes) => {
                let text = String::from_utf8(bytes).map_err(|error| {
                    CoreError::InvalidOperation(format!(
                        "{} is not valid text, so the saved searches in it cannot be read: {error}",
                        file.display()
                    ))
                })?;
                parse_document(&text, &file)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        let mut store = Self { file, entries };
        store.sort();
        Ok(store)
    }

    /// The file these searches live in, named because the only useful thing an
    /// interface can say about an unreadable store is where to look at it.
    pub fn path(&self) -> &Path {
        &self.file
    }

    pub fn list(&self) -> &[NamedSearch] {
        &self.entries
    }

    pub fn get(&self, name: &str) -> Option<&SavedSearch> {
        self.entries
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| &entry.search)
    }

    /// Whether a name is already taken. Reported separately from `save` so the
    /// interface can say "this will replace X" *before* the click, instead of
    /// the user finding out from a list that lost one of its entries.
    pub fn contains(&self, name: &str) -> bool {
        self.entries.iter().any(|e| e.name == name)
    }

    /// Store `search` under `name`, replacing anything already using it.
    /// Returns whether an existing entry was overwritten.
    ///
    /// The write happens before the in-memory change, so a failed persist
    /// cannot leave the interface showing a search that is not on disk.
    pub fn save(&mut self, name: &str, search: &SavedSearch) -> Result<bool> {
        let name = name.trim();
        if name.is_empty() {
            return Err(CoreError::InvalidOperation(
                "a saved search needs a name".to_string(),
            ));
        }
        let replaced = self.contains(name);
        let mut entries = self.entries.clone();
        entries.retain(|entry| entry.name != name);
        entries.push(NamedSearch {
            name: name.to_string(),
            search: search.clone(),
        });
        let mut next = entries;
        sort_by_name(&mut next);
        persist(&self.file, &next)?;
        self.entries = next;
        Ok(replaced)
    }

    /// Remove a saved search. Returns whether anything went, because a delete
    /// that matched nothing means the list the user was looking at had already
    /// lost that entry, and the interface needs to re-read rather than assume.
    pub fn remove(&mut self, name: &str) -> Result<bool> {
        let before = self.entries.len();
        let mut entries = self.entries.clone();
        entries.retain(|entry| entry.name != name);
        if entries.len() == before {
            return Ok(false);
        }
        persist(&self.file, &entries)?;
        self.entries = entries;
        Ok(true)
    }

    fn sort(&mut self) {
        sort_by_name(&mut self.entries);
    }
}

/// Name order, so the sidebar list does not depend on the order things happened
/// to be saved in. Saving again under the same name is a replace, which keeps
/// its place either way; without this the list could shuffle between two looks
/// at the panel.
fn sort_by_name(entries: &mut [NamedSearch]) {
    entries.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
}

fn parse_document(text: &str, file: &Path) -> Result<Vec<NamedSearch>> {
    let document: Document = serde_json::from_str(text).map_err(|error| {
        CoreError::InvalidOperation(format!(
            "{} could not be read ({error}); nothing will overwrite it until it is \
             fixed or removed",
            file.display()
        ))
    })?;
    if document.version != DOCUMENT_VERSION {
        return Err(CoreError::InvalidOperation(format!(
            "{} holds saved searches written by version {}, and this build reads version \
             {} only",
            file.display(),
            document.version,
            DOCUMENT_VERSION
        )));
    }
    Ok(document.searches)
}

fn persist(file: &Path, entries: &[NamedSearch]) -> Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let document = Document {
        version: DOCUMENT_VERSION,
        searches: entries.to_vec(),
    };
    let bytes = serde_json::to_vec_pretty(&document)
        .map_err(|error| CoreError::InvalidOperation(format!("could not encode: {error}")))?;
    crate::paths::write_atomic(file, &bytes).map_err(|error| {
        CoreError::InvalidOperation(format!("could not write {}: {error}", file.display()))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata_index::{Query, SearchScope};

    fn search(words: &str) -> SavedSearch {
        SavedSearch {
            query: Query::text(words),
            scope: SearchScope::Library,
        }
    }

    fn store(dir: &Path) -> SavedSearches {
        SavedSearches::open(dir.join(FILE_NAME)).unwrap()
    }

    #[test]
    fn a_missing_file_is_an_empty_list_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let saved = store(dir.path());
        assert!(saved.list().is_empty());
        assert!(
            !saved.path().exists(),
            "opening created a file it does not own"
        );
    }

    #[test]
    fn a_saved_search_survives_reopening_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let mut saved = store(dir.path());
        saved.save("shore things", &search("beach sunset")).unwrap();

        let reopened = store(dir.path());
        assert_eq!(reopened.list().len(), 1);
        let stored = reopened.get("shore things").expect("gone after reopen");
        assert_eq!(stored, &search("beach sunset"));
    }

    #[test]
    fn saving_the_same_name_replaces_and_says_so_afterwards() {
        let dir = tempfile::tempdir().unwrap();
        let mut saved = store(dir.path());
        assert!(
            !saved.save("trip", &search("pines")).unwrap(),
            "first save has nothing to replace"
        );
        assert!(
            saved.save("trip", &search("sunset")).unwrap(),
            "second save should report a replacement"
        );
        assert_eq!(saved.list().len(), 1, "the name made a second entry");
        assert_eq!(
            saved.get("trip").unwrap().summary(),
            search("sunset").summary()
        );
    }

    #[test]
    fn the_list_is_name_ordered_however_it_was_saved() {
        let dir = tempfile::tempdir().unwrap();
        let mut saved = store(dir.path());
        for name in ["zebra", "Alpha", "mango"] {
            saved.save(name, &search(name)).unwrap();
        }
        let listed: Vec<&str> = saved.list().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            listed,
            vec!["Alpha", "mango", "zebra"],
            "case-insensitive name order"
        );

        // And the order is a property of the file, not of the loaded object.
        let reopened = store(dir.path());
        let again: Vec<&str> = reopened.list().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(again, listed, "order came from insertion, not storage");
    }

    #[test]
    fn an_empty_name_is_refused_before_anything_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut saved = store(dir.path());
        for blank in ["", "   "] {
            let error = saved.save(blank, &search("x")).unwrap_err().to_string();
            assert!(error.contains("needs a name"), "{error} for {blank:?}");
        }
        assert!(
            !saved.path().exists(),
            "a refused save still created the file"
        );
    }

    #[test]
    fn removing_reports_whether_anything_went() {
        let dir = tempfile::tempdir().unwrap();
        let mut saved = store(dir.path());
        saved.save("keep", &search("a")).unwrap();
        assert!(saved.remove("keep").unwrap());
        assert!(saved.list().is_empty());
        assert!(
            !saved.remove("keep").unwrap(),
            "a second delete claimed to remove something"
        );
    }

    #[test]
    fn an_unreadable_store_is_reported_and_left_alone() {
        // The reason the file is its own and not a table in a rebuildable
        // cache: it holds work nobody else has. So a malformed byte has to be
        // announced, and the very next save must not silently replace it.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE_NAME);
        std::fs::write(
            &file,
            r#"{"version":1,"searches":[{"name":"one","query":{"predicates":[],"combinator":"And"},"scope":"Library"}]} leftover"#,
        )
        .unwrap();
        let before = std::fs::read(&file).unwrap();

        let error = SavedSearches::open(&file).unwrap_err().to_string();
        assert!(
            error.contains("could not be read"),
            "the failure must name the file and what went wrong: {error}"
        );
        assert!(
            error.contains(&file.display().to_string()),
            "the message has to say which file to look at: {error}"
        );
        assert_eq!(
            std::fs::read(&file).unwrap(),
            before,
            "a failed open still rewrote the user's file"
        );
    }

    #[test]
    fn a_document_from_a_future_version_is_refused_rather_than_guessed() {
        // Reading version 2 with version 1 rules could drop fields silently and
        // then write the loss back out. Refusing keeps the file intact for a
        // build that understands it.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE_NAME);
        std::fs::write(&file, r#"{"version":2,"searches":[]}"#).unwrap();
        let error = SavedSearches::open(&file).unwrap_err().to_string();
        assert!(error.contains("version 2"), "{error}");
        assert!(file.exists(), "the refused file was deleted");
    }

    #[test]
    fn a_folder_scope_is_kept_exactly_as_saved() {
        // The scope is part of what the user saved: a search of one folder must
        // not silently become a search of everything on the next visit.
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("photos");
        let file = dir.path().join(FILE_NAME);
        let mut saved = SavedSearches::open(&file).unwrap();
        saved
            .save(
                "here",
                &SavedSearch {
                    query: Query::text("beach"),
                    scope: SearchScope::Folder(folder.clone()),
                },
            )
            .unwrap();

        let reopened = SavedSearches::open(&file).unwrap();
        match &reopened.get("here").unwrap().scope {
            SearchScope::Folder(root) => assert_eq!(root, &folder),
            other => panic!("scope came back as {other:?}"),
        }
    }

    #[test]
    fn a_stored_entry_can_be_found_by_the_path_it_came_from() {
        // `SavedSearch` is flattened into the entry, so the name has to stay a
        // sibling of the rules rather than colliding with a rule field.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE_NAME);
        let mut saved = SavedSearches::open(&file).unwrap();
        saved.save("named", &search("anything")).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("\"name\""), "{text}");
        assert!(text.contains("\"query\""), "{text}");
        assert!(text.contains("\"scope\""), "{text}");
        assert!(
            text.contains(&format!("\"version\": {DOCUMENT_VERSION}")),
            "the document version is not written where a reader can see it: {text}"
        );
    }
}
