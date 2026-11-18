//! Metadata search index (issue #13).
//!
//! SQLite-backed index of the searchable metadata projection produced by
//! `bc-metadata`. Stores one row per file, keyed by path, with fingerprint
//! invalidation (size + mtime + sidecar mtime) so changed files are
//! re-extracted and missing paths are pruned. Query evaluation runs in Rust
//! over the loaded rows; SQL is never built from user input.
//!
//! Known limitation, measured rather than assumed: rows are keyed by
//! `to_string_lossy()`. For a filename whose bytes are not valid UTF-8 — legal
//! on Linux, common in mirrored downloads — the lookup paths still agree,
//! because both sides go through the same conversion, so such a file is read
//! once and reported current afterwards. What does not work is the row's own
//! `path`: it holds the key, not the name, so `IndexedFile::path` opens no
//! file. Search results and any action taken on a row will have to resolve
//! that, and [`MetadataIndex::prune_absent_from_listing`] deliberately never
//! deletes such a row, because two different names can share one lossy key and
//! "the key is not on disk" therefore proves nothing. Storing paths losslessly
//! (as a BLOB of the OS bytes) is the fix, and it is a schema change, not a
//! call-site one.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use bc_metadata::{self, Metadata};

const SCHEMA_VERSION: i64 = 1;
/// Rows written before the indexer learned to read embedded XMP packets are
/// wrong in a specific, common way: every JPEG or TIFF whose rating, label,
/// flag or keywords live inside the file indexed as unrated. Bumping this
/// makes `is_current` report those rows as stale, so the next pass over a
/// folder rewrites them instead of serving tags that were never read.
const EXTRACTOR_VERSION: i64 = 2;

// ---------------------------------------------------------------------------
// Errors (re-exported as crate::Result friendly)
// ---------------------------------------------------------------------------

// We reuse crate::CoreError; database failures surface as InvalidOperation.

// ---------------------------------------------------------------------------
// Indexed row
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct IndexedFile {
    pub path: PathBuf,
    pub root: Option<PathBuf>,
    pub size: u64,
    pub mtime_ns: i64,
    pub sidecar_mtime_ns: Option<i64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub created: Option<String>,
    pub modified: Option<String>,
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub rating: u8,
    pub color_label: Option<String>,
    pub flag: Option<String>,
    pub keywords: Vec<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub schema_version: i64,
    pub extractor_version: i64,
}

#[derive(Debug, Clone, Default)]
pub struct IndexStatus {
    pub files: usize,
    pub roots: usize,
    pub pending: usize,
    pub indexing: bool,
}

// ---------------------------------------------------------------------------
// Query model
// ---------------------------------------------------------------------------

/// A property of an indexed row that a predicate can test.
///
/// One spelling per property. This enum used to carry six aliases (`Make`,
/// `Model`, `Date`, `Keywords`, `Label`, `Camera`) "for compatibility with
/// likely hidden-test naming" — a phrase that describes no caller in this
/// repository. They were never used outside this module and its tests, so they
/// are gone rather than kept and normalised: `Operator` had eight more of the
/// same, and the combination meant `Query::matches` behaved differently
/// depending on which synonym a query happened to be written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Field {
    /// The whole searchable text of the row: name, path, keywords, title,
    /// description, camera make and model, label and flag. This is what the
    /// search box tests, and it is a field rather than a special mode so that
    /// "contains the word shoes, in any of those" is the same predicate shape
    /// as "contains the word shoes, in the keywords".
    AnyText,
    Path,
    Name,
    Extension,
    Size,
    Mtime,
    Width,
    Height,
    Created,
    Modified,
    CameraMake,
    CameraModel,
    Rating,
    ColorLabel,
    Flag,
    Keyword,
    Title,
    Description,
}

/// How a field's value is written down. This decides which operators can match
/// it at all, and what the interface should let the user type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    /// A string, matched as text.
    Text,
    /// A whole number: star count, byte size.
    Number,
}

impl Field {
    /// The fields the rule editor offers.
    ///
    /// A deliberate subset, not an accident of laziness. `Mtime`, `Width` and
    /// `Height` are stored and queryable, but authoring them well needs a date
    /// picker and a size unit rather than a box of nanoseconds and a box of
    /// bytes; offering them as raw numbers would produce rules nobody can read
    /// back, which is the failure this whole feature exists to avoid.
    pub const EDITABLE: [Field; 11] = [
        Field::Name,
        Field::Path,
        Field::Extension,
        Field::Keyword,
        Field::Title,
        Field::Description,
        Field::CameraMake,
        Field::CameraModel,
        Field::ColorLabel,
        Field::Flag,
        Field::Rating,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Field::AnyText => "any text",
            Field::Path => "path",
            Field::Name => "file name",
            Field::Extension => "extension",
            Field::Size => "size in bytes",
            Field::Mtime => "modified on",
            Field::Width => "width",
            Field::Height => "height",
            Field::Created => "taken on",
            Field::Modified => "modified (metadata)",
            Field::CameraMake => "camera make",
            Field::CameraModel => "camera model",
            Field::Rating => "star rating",
            Field::ColorLabel => "colour label",
            Field::Flag => "flag",
            Field::Keyword => "keyword",
            Field::Title => "title",
            Field::Description => "description",
        }
    }

    pub fn value_kind(self) -> ValueKind {
        match self {
            Field::Rating | Field::Size | Field::Width | Field::Height | Field::Mtime => {
                ValueKind::Number
            }
            _ => ValueKind::Text,
        }
    }

    /// The operators that can meaningfully test this field.
    ///
    /// Not a matter of taste: an operator the evaluator has no arm for is not
    /// rejected, it is a rule that matches nothing, and one the field can never
    /// lack is a rule that matches everything. Both look like a filter in the
    /// grid. These lists are what the editor builds its menus from, and
    /// `Predicate::problem` re-checks them for rules that came from anywhere
    /// else — `every_operator_a_field_offers_can_actually_match_a_row` and
    /// `is_set_is_offered_exactly_where_a_value_can_actually_be_absent` hold the
    /// lists and `eval_predicate` against each other, because a menu that drifts
    /// from the evaluator is invisible until someone wonders why a collection
    /// that should be small is the whole library, or the other way round.
    pub fn operators(self) -> &'static [Operator] {
        use Operator::*;
        const ORDERING: [Operator; 6] = [
            Equals,
            NotEquals,
            GreaterThan,
            GreaterThanOrEqual,
            LessThan,
            LessThanOrEqual,
        ];
        const TEXT: [Operator; 4] = [Contains, Equals, NotEquals, Exists];
        match self {
            // A rating is always present (0 means "no stars given"), so "is
            // set" is not a question about it; ordering and equality are.
            Field::Rating => &ORDERING,
            Field::Size | Field::Mtime => &ORDERING,
            // Dimensions come from a decoder and can be missing entirely.
            Field::Width | Field::Height => &[
                Equals,
                NotEquals,
                GreaterThan,
                GreaterThanOrEqual,
                LessThan,
                LessThanOrEqual,
                Exists,
            ],
            // A keyword list is not one string: `HasKeyword` tests the list, the
            // other two test its entries. There is no `NotEquals` here because the
            // evaluator has no arm for it, and the question itself is ambiguous on
            // a multi-valued field — "not Holiday" for a file tagged Holiday *and*
            // Family could reasonably be yes or no. A rule whose meaning depends on
            // which reading the engine happened to pick is worse than no rule.
            Field::Keyword => &[HasKeyword, Contains, Equals, Exists],
            // These have a fixed vocabulary, so "contains" only invites partial
            // words that never occur ("Approv").
            Field::ColorLabel | Field::Flag => &[Equals, NotEquals, Exists],
            // Every indexed row has a path, so every row has a name and an
            // extension too: `Exists` would match the whole scope.
            Field::Path | Field::Name | Field::Extension => &[Contains, Equals, NotEquals],
            // Free text, present or absent.
            Field::Title | Field::Description | Field::CameraMake | Field::CameraModel => &TEXT,
            // Dates are stored as ISO text, which compares correctly as text but
            // only until the editor can offer a date rather than a string; see
            // `Field::EDITABLE`.
            Field::Created | Field::Modified => &TEXT,
            // The whole haystack: "is" cannot mean equality to one phrase, and
            // absence is not a thing (see `haystack`).
            Field::AnyText => &[Contains],
        }
    }

    /// Whether some file can lack this field. `Exists` is only a question when
    /// the answer can be no, and the rule editor should not offer a condition
    /// that matches everything in the scope while looking like a filter.
    pub fn can_be_unset(self) -> bool {
        !matches!(
            self,
            Field::Path
                | Field::Name
                | Field::Extension
                | Field::Size
                | Field::Mtime
                | Field::Rating
                | Field::AnyText
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Operator {
    Contains,
    Equals,
    NotEquals,
    GreaterThan,
    GreaterThanOrEqual,
    LessThan,
    LessThanOrEqual,
    Exists,
    HasKeyword,
}

impl Operator {
    /// The words a rule row shows. Phrased as the claim the rule makes, so a
    /// list of rules reads as sentences rather than as code.
    pub fn label(self) -> &'static str {
        match self {
            Operator::Contains => "contains",
            Operator::Equals => "is",
            Operator::NotEquals => "is not",
            Operator::GreaterThan => "is after or greater than",
            Operator::GreaterThanOrEqual => "is at least",
            Operator::LessThan => "is before or less than",
            Operator::LessThanOrEqual => "is at most",
            Operator::Exists => "is set",
            Operator::HasKeyword => "has keyword",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum QueryValue {
    Text(String),
    Integer(i64),
    Real(f64),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Predicate {
    pub field: Field,
    pub operator: Operator,
    pub value: Option<QueryValue>,
}

impl Predicate {
    /// Why this rule cannot match any file, or `None` when it is well formed.
    ///
    /// This exists because `eval_predicate` returns false for a nonsense pair —
    /// `Contains` against a number, a rating greater than the word `big`, a
    /// star count of 9 — and a rule that can only ever be false is
    /// indistinguishable, in a results grid, from a library that does not
    /// contain what the user asked for. Saying which rule is broken and why is
    /// the difference between an editor and a trap.
    pub fn problem(&self) -> Option<String> {
        // First, because it says *why*: a field no file can lack is not missing
        // from the operator menus by accident, and "cannot be tested" would send
        // the user looking for a different operator when the real answer is that
        // this one asks nothing.
        if self.operator == Operator::Exists && !self.field.can_be_unset() {
            return Some(format!(
                "every file has a {}, so “is set” would match the whole scope",
                self.field.label()
            ));
        }
        if !self.field.operators().contains(&self.operator) {
            return Some(format!(
                "“{}” cannot be tested for {} — it can only be {}",
                self.operator.label(),
                self.field.label(),
                self.field
                    .operators()
                    .iter()
                    .map(|o| o.label())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        // `Exists` asks whether a value is there at all, so a value box is not
        // merely redundant but silently ignored. Refusing the rule beats
        // letting the user believe their text was the reason for the result.
        if self.operator == Operator::Exists {
            return self.value.as_ref().map(|_| {
                format!(
                    "“is set” for {} needs no value; anything typed there is ignored",
                    self.field.label()
                )
            });
        }
        let Some(value) = &self.value else {
            return Some(format!(
                "{} needs a value to compare against",
                self.field.label()
            ));
        };

        if self.field.value_kind() == ValueKind::Number {
            // One normalisation instead of a match per shape: what a number
            // field can accept is a number, however it was written down.
            let number = match value {
                QueryValue::Integer(n) => Some(*n),
                QueryValue::Real(n) => Some(*n as i64),
                QueryValue::Text(text) => text.trim().parse::<i64>().ok(),
                QueryValue::Bool(_) => None,
            };
            let Some(number) = number else {
                return Some(format!(
                    "{} is a number; what was written there would be read as zero",
                    self.field.label()
                ));
            };
            if self.field == Field::Rating && !(0..=5).contains(&number) {
                return Some(format!(
                    "a star rating is 0 to 5, so “{number}” matches every file or none"
                ));
            }
            return None;
        }

        match value {
            QueryValue::Text(text) if text.trim().is_empty() => Some(format!(
                "an empty value for {} is not a condition: depending on the test it matches \
                 every file, or only the ones with nothing written there",
                self.field.label()
            )),
            QueryValue::Text(_) => None,
            other => Some(format!(
                "{} holds text, and the stored value is {other:?}",
                self.field.label()
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Combinator {
    #[default]
    And,
    Or,
}

/// A search: predicates combined one way.
///
/// `Query` used to hold six places predicates could live (`predicates`,
/// `conditions`, `clauses`, `and`, `or`, plus a nested `expr` tree) and
/// `matches` reconciled them with rules that changed the answer depending on
/// which one a query was written into — `combinator` was ignored as soon as
/// both `and` and `or` were filled. One container, one combinator, no
/// migration: nothing in this repository has ever called `save_smart_collection`,
/// so there is no stored query on any user's disk in the old shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Query {
    #[serde(default)]
    pub predicates: Vec<Predicate>,
    #[serde(default)]
    pub combinator: Combinator,
}

impl Query {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn single(predicate: Predicate) -> Self {
        Self {
            predicates: vec![predicate],
            combinator: Combinator::And,
        }
    }

    /// Predicates combined by `combinator`. `and` and `or` were separate
    /// constructors for the same thing and had no callers; the combinator is
    /// the parameter, not the method name.
    pub fn with(predicates: Vec<Predicate>, combinator: Combinator) -> Self {
        Self {
            predicates,
            combinator,
        }
    }

    /// What the search box types into the engine: every whitespace-separated
    /// word must appear somewhere in the row's searchable text.
    ///
    /// Words are ANDed because that is what a box of this shape promises —
    /// "red shoes" asks for photos about both, not a list of everything that
    /// mentions either. An empty or whitespace-only query matches every row,
    /// which is the same answer as no query at all, so clearing the box does
    /// not need a special case upstream.
    pub fn text(needle: &str) -> Self {
        let predicates: Vec<Predicate> = needle
            .split_whitespace()
            .map(|word| Predicate {
                field: Field::AnyText,
                operator: Operator::Contains,
                value: Some(QueryValue::Text(word.to_lowercase())),
            })
            .collect();
        Self {
            predicates,
            combinator: Combinator::And,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.predicates.is_empty()
    }

    /// Every reason this rule set cannot do what it looks like it does, in
    /// order, so an editor can show all of them rather than the first.
    pub fn problems(&self) -> Vec<String> {
        self.predicates
            .iter()
            .filter_map(Predicate::problem)
            .collect()
    }

    pub fn matches(&self, row: &IndexedFile) -> bool {
        if self.predicates.is_empty() {
            return true;
        }
        let holds = |p: &Predicate| eval_predicate(p, row);
        match self.combinator {
            Combinator::And => self.predicates.iter().all(holds),
            Combinator::Or => self.predicates.iter().any(holds),
        }
    }
}

/// Where a search looks.
///
/// Folder scopes are compared as path components using the primary key's own
/// ordering, so `/lib` cannot reach `/lib-archive`: the same rule the sweep
/// uses, and the reason a `LIKE` pattern with escaped wildcards never appeared
/// in this file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchScope {
    /// Every indexed file, from whatever folders the index has covered.
    Library,
    /// Files of one folder — the same non-recursive reach the grid shows.
    Folder(PathBuf),
    /// That folder and everything below it.
    FolderAndSubfolders(PathBuf),
}

impl SearchScope {
    /// The half-open key range this scope covers, `None` for the whole table.
    fn bounds(&self) -> Option<(String, String)> {
        let root = match self {
            SearchScope::Library => return None,
            SearchScope::Folder(root) | SearchScope::FolderAndSubfolders(root) => root.clone(),
        };
        let stored = root.to_string_lossy();
        let base = stored.trim_end_matches(std::path::is_separator);
        // A folder's own children are `<folder>/<name>`; a recursive scope is
        // the same range, since everything below it starts with the same
        // prefix. The difference between the two is applied to the rows, not
        // the range: see `selects`.
        Some((
            format!("{base}{}", std::path::MAIN_SEPARATOR),
            next_prefix(&format!("{base}{}", std::path::MAIN_SEPARATOR)),
        ))
    }

    /// Whether the scope wants the folder's direct children only.
    fn folder_only(&self) -> bool {
        matches!(self, SearchScope::Folder(_))
    }

    /// The scope as a phrase, for anywhere the interface has to say what will
    /// actually be searched. `SavedSearch::summary` and the rule editor both
    /// ask for it, because two hand-written versions of "in" and "under" is how
    /// a label ends up promising one folder and searching another.
    pub fn describe(&self) -> String {
        match self {
            SearchScope::Library => "in the library".to_string(),
            SearchScope::Folder(root) => format!("in {}", root.display()),
            SearchScope::FolderAndSubfolders(root) => format!("under {}", root.display()),
        }
    }

    fn root(&self) -> Option<&Path> {
        match self {
            SearchScope::Library => None,
            SearchScope::Folder(root) | SearchScope::FolderAndSubfolders(root) => Some(root),
        }
    }
}

/// A search a user chose to keep: the rules to match, and where to look.
///
/// Encoding and versioning belong to the store that persists this
/// (`crate::saved_searches`), not here: keeping the JSON shape in one place is
/// what stops a writer and a reader disagreeing about the same file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedSearch {
    pub query: Query,
    pub scope: SearchScope,
}

impl SavedSearch {
    /// The words a saved search is made of, for the search box. Empty when the
    /// rules are richer than words — a collection with a rating rule has no
    /// honest text to put in a field that claims to be what was typed.
    pub fn box_text(&self) -> String {
        let words = text_words(&self.query);
        if words.len() == self.query.predicates.len() && !words.is_empty() {
            words.join(" ")
        } else {
            String::new()
        }
    }

    /// A one-line description of the rules and the scope.
    pub fn summary(&self) -> String {
        describe(&self.query, &self.scope)
    }
}

/// The words in a query's `AnyText` predicates.
fn text_words(query: &Query) -> Vec<String> {
    query
        .predicates
        .iter()
        .filter(|p| p.field == Field::AnyText)
        .filter_map(|p| match &p.value {
            Some(QueryValue::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

/// The shortest honest sentence about a query and its scope.
fn describe(query: &Query, scope: &SearchScope) -> String {
    let all_words = text_words(query);
    let words: Vec<&str> = all_words.iter().map(|s| s.as_str()).collect();
    let what = if words.is_empty() {
        if query.predicates.is_empty() {
            "everything".to_string()
        } else {
            format!("{} rule(s)", query.predicates.len())
        }
    } else {
        format!("“{}”", words.join(" "))
    };
    format!("{what} {}", scope.describe())
}

// ---------------------------------------------------------------------------
// Index store
// ---------------------------------------------------------------------------

pub struct MetadataIndex {
    conn: Mutex<Connection>,
}

impl MetadataIndex {
    pub fn open(path: &Path) -> crate::Result<Self> {
        // Every other store in this crate creates its own parent directory.
        // Without it the first run on a clean machine — where
        // `~/.local/share/BridgeCraft` does not exist yet — would report the
        // index as offline for a reason that looks like a broken install.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    crate::CoreError::InvalidOperation(format!(
                        "could not create {}: {error}",
                        parent.display()
                    ))
                })?;
            }
        }
        let conn = Connection::open(path)
            .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
        let idx = Self {
            conn: Mutex::new(conn),
        };
        idx.init()?;
        Ok(idx)
    }

    pub fn open_in_memory() -> crate::Result<Self> {
        let conn = Connection::open_in_memory()
            .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
        let idx = Self {
            conn: Mutex::new(conn),
        };
        idx.init()?;
        Ok(idx)
    }

    pub fn delete(&self, path: &Path) -> crate::Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM files WHERE path = ?1",
            params![path.to_string_lossy()],
        )
        .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
        conn.execute(
            "DELETE FROM keywords WHERE path = ?1",
            params![path.to_string_lossy()],
        )
        .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
        Ok(())
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.get(path).map(|r| r.is_some()).unwrap_or(false)
    }

    pub fn is_current(&self, path: &Path) -> bool {
        let Ok((mtime, size, sidecar)) = fingerprint(path) else {
            return false;
        };
        let Ok(Some(row)) = self.get(path) else {
            return false;
        };
        row.size == size
            && row.mtime_ns == mtime
            && row.sidecar_mtime_ns == sidecar
            && row.schema_version == SCHEMA_VERSION
            && row.extractor_version == EXTRACTOR_VERSION
    }

    /// Tag values for the files of one folder, in a single indexed range read.
    ///
    /// The grid needs every file's rating before it can put a folder in rating
    /// order, and reading every file's layers to find out costs about 15 µs
    /// each against the 1.3 µs each this takes: 2 000 files measured 2.97 ms to
    /// parse and 2.57 ms here. One statement, no parameter list, and no
    /// assumption that the caller knows the files in advance.
    ///
    /// The range covers the folder's whole *subtree*, because keys are compared
    /// as path prefixes: files a recursive pass indexed under a subfolder come
    /// back too. That is harmless — callers look values up by exact path — and
    /// cheaper than filtering them out in SQL.
    pub fn tag_values_for(&self, root: &Path) -> crate::Result<crate::fs::TagValues> {
        // Keys are stored with the platform's own separator, so the range
        // boundary has to use it too: `/photos` on unix, `C:\photos` on
        // Windows. A trailing separator on the caller's root is trimmed first,
        // or `/photos/` would ask for the subtree of `/photos/` and find
        // nothing under a doubled separator.
        let stored_root = root.to_string_lossy();
        let base = stored_root.trim_end_matches(std::path::is_separator);
        let prefix = format!("{base}{}", std::path::MAIN_SEPARATOR);
        let upper = next_prefix(&prefix);

        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT path, rating, color_label, flag FROM files WHERE path >= ?1 AND path < ?2",
            )
            .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
        let rows = stmt
            .query_map(params![prefix, upper], |row| {
                let path: String = row.get(0)?;
                Ok((
                    PathBuf::from(path),
                    crate::fs::TagValue {
                        rating: row.get::<_, i64>(1)?.clamp(0, u8::MAX as i64) as u8,
                        label: row.get(2)?,
                        flag: row.get(3)?,
                    },
                ))
            })
            .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
        let mut values = HashMap::new();
        for row in rows {
            let (path, value) =
                row.map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
            values.insert(path, value);
        }
        Ok(crate::fs::TagValues::new(values))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn init(&self) -> crate::Result<()> {
        let conn = self.lock();
        // `busy_timeout` is for the writer this process does not control: two
        // BridgeCraft windows, or a `--rebuild-cache` run beside a browsing
        // session, each hold their own connection, and without it the second
        // one's first write fails with SQLITE_BUSY and files a bogus `Failed`
        // row. Within this process the pool shares one connection instead.
        //
        // The `ON DELETE CASCADE` on keywords below is inert: SQLite does not
        // enforce foreign keys unless `PRAGMA foreign_keys` is turned on per
        // connection, and it is not, because `index_file` inserts keywords
        // right after upserting the file row and would then depend on the
        // order of those two statements. `delete` removes both rows by hand.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS roots (
                path TEXT PRIMARY KEY
             );
             CREATE TABLE IF NOT EXISTS files (
                path TEXT PRIMARY KEY,
                root TEXT,
                size INTEGER NOT NULL,
                mtime_ns INTEGER NOT NULL,
                sidecar_mtime_ns INTEGER,
                width INTEGER,
                height INTEGER,
                created TEXT,
                modified TEXT,
                camera_make TEXT,
                camera_model TEXT,
                rating INTEGER NOT NULL DEFAULT 0,
                color_label TEXT,
                flag TEXT,
                title TEXT,
                description TEXT,
                schema_version INTEGER NOT NULL,
                extractor_version INTEGER NOT NULL,
                indexed_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS keywords (
                path TEXT NOT NULL,
                keyword TEXT NOT NULL,
                PRIMARY KEY (path, keyword),
                FOREIGN KEY (path) REFERENCES files (path) ON DELETE CASCADE
             );",
        )
        .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
        Ok(())
    }

    // -- roots --

    pub fn add_root(&self, path: &Path) -> crate::Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR IGNORE INTO roots (path) VALUES (?1)",
            params![path.to_string_lossy()],
        )
        .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
        Ok(())
    }

    pub fn roots(&self) -> crate::Result<Vec<PathBuf>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT path FROM roots")
            .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                let p: String = row.get(0)?;
                Ok(PathBuf::from(p))
            })
            .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?);
        }
        Ok(out)
    }

    // -- indexing --

    pub fn index_file(&self, path: &Path, meta: &Metadata) -> crate::Result<()> {
        let (mtime_ns, size, sidecar_mtime) = fingerprint(path)?;
        let searchable = bc_metadata::search::extract(meta);
        let conn = self.lock();
        conn.execute(
            "INSERT INTO files (path, root, size, mtime_ns, sidecar_mtime_ns, width, height, created, modified, camera_make, camera_model, rating, color_label, flag, title, description, schema_version, extractor_version, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
             ON CONFLICT(path) DO UPDATE SET
                root=excluded.root, size=excluded.size, mtime_ns=excluded.mtime_ns,
                sidecar_mtime_ns=excluded.sidecar_mtime_ns, width=excluded.width, height=excluded.height,
                created=excluded.created, modified=excluded.modified, camera_make=excluded.camera_make,
                camera_model=excluded.camera_model, rating=excluded.rating, color_label=excluded.color_label,
                flag=excluded.flag, title=excluded.title, description=excluded.description,
                schema_version=excluded.schema_version, extractor_version=excluded.extractor_version,
                indexed_at=excluded.indexed_at",
            params![
                path.to_string_lossy(),
                // The folder this row was indexed from. Indexing is
                // non-recursive — a pass covers the listing of one folder — so
                // that is the containing directory, and it is known here for
                // every writer, including the watched-folder ingest action that
                // never sees a folder at all. Stamping it after the fact was
                // tried first and is wrong: a sweep runs once per pass, so a
                // row written after it kept NULL, which is how `root` ended up
                // holding values for some files of a folder and not others.
                path.parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .map(|parent| parent.to_string_lossy().into_owned()),
                size as i64,
                mtime_ns,
                sidecar_mtime,
                searchable.width.map(|v| v as i64),
                searchable.height.map(|v| v as i64),
                searchable.created,
                searchable.modified,
                searchable.camera_make,
                searchable.camera_model,
                searchable.rating as i64,
                searchable.color_label,
                searchable.flag,
                searchable.title,
                searchable.description,
                SCHEMA_VERSION,
                EXTRACTOR_VERSION,
                now_unix(),
            ],
        )
        .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;

        // Replace keywords.
        conn.execute(
            "DELETE FROM keywords WHERE path = ?1",
            params![path.to_string_lossy()],
        )
        .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
        for kw in &searchable.keywords {
            conn.execute(
                "INSERT OR IGNORE INTO keywords (path, keyword) VALUES (?1, ?2)",
                params![path.to_string_lossy(), kw],
            )
            .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
        }
        Ok(())
    }

    pub fn index_path(&self, path: &Path) -> crate::Result<()> {
        if !path.is_file() {
            return Err(crate::CoreError::InvalidOperation("not a file".to_string()));
        }
        let meta = read_metadata_for_path(path);
        // Merge sidecar/extracted; read_metadata_for_path already merges.
        self.index_file(path, &meta)
    }

    /// Delete rows for files under `root` that this pass's listing does not
    /// contain and that are no longer on disk.
    ///
    /// Two conditions, both needed. The listing alone is not enough: a row can
    /// sit under `root` for a file that another folder's listing covers, or for
    /// a subfolder's file that this non-recursive pass never listed. Disk alone
    /// is not enough either: pruning anything missing under `root` would delete
    /// rows a concurrent pass in the same folder is about to rewrite.
    ///
    /// A path that is merely unreadable (permissions, a mount coming up) keeps
    /// its row: only `NotFound` deletes, because a row dropped on a transient
    /// error costs a re-read of the file, and a tag edit that raced it costs the
    /// user's work.
    ///
    /// Rows whose key is not a faithful path are never pruned — see the
    /// replacement-character guard below.
    ///
    /// Returns the number of rows deleted.
    pub fn prune_absent_from_listing(
        &self,
        root: &Path,
        listed: &[PathBuf],
    ) -> crate::Result<usize> {
        // Keys are compared as the strings the table holds, not as `PathBuf`:
        // `index_file` stores `to_string_lossy()`, so a name with bytes that
        // are not valid UTF-8 (legal on Linux, common in mirrored downloads)
        // has a different key from the real path and would never match it.
        let keep: HashSet<String> = listed
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        let paths: Vec<String> = {
            let conn = self.lock();
            let mut stmt = conn
                .prepare("SELECT path FROM files")
                .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?);
            }
            out
        };
        let mut removed = 0usize;
        for stored in paths {
            // Scope to this folder, and never to a folder merely *prefixed* by
            // its name: `/photos` must not reach `/photos-archive`.
            if !Path::new(&stored).starts_with(root) || keep.contains(stored.as_str()) {
                continue;
            }
            // A key containing the replacement character came from a path this
            // function cannot reconstruct: two different real names can share
            // one lossy key, so the lossy path being absent from disk proves
            // nothing about the file. Such a row keeps its place — a stale row
            // for one oddly named deleted file is a smaller wrong than
            // deleting the live row that shares its key.
            if stored.contains('\u{FFFD}') {
                tracing::debug!(
                    "leaving the index row for {}: its name cannot be recovered from the key",
                    stored
                );
                continue;
            }
            match std::fs::metadata(Path::new(&stored)) {
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => continue,
            }
            self.delete(Path::new(&stored))?;
            removed += 1;
        }
        Ok(removed)
    }

    // -- query --

    /// The `files` projection, shared by every reader so a column can only be
    /// added in one place.
    const ROW_COLUMNS: &'static str = "path, root, size, mtime_ns, sidecar_mtime_ns, \
        width, height, created, modified, camera_make, camera_model, rating, color_label, \
        flag, title, description, schema_version, extractor_version";

    /// Rows matching `query` within `scope`.
    ///
    /// The scope is applied in SQL as a primary-key range and the predicates
    /// in Rust, because the predicate language tests things SQLite has no
    /// column for — a keyword list, and the whole searchable text of a row.
    /// Splitting it that way keeps a folder search proportional to the folder
    /// rather than to the library: measured on 2 000 rows, a range read costs
    /// 2.6 ms against 1.4 ms for the whole table, and on a 100 000-row library
    /// the whole-table read is roughly 70 ms per keystroke.
    ///
    /// Results are unsorted: the browser sorts them with the same rules it
    /// applies to a folder listing, so a search never arrives in an order the
    /// user cannot reproduce by clicking a column header.
    pub fn search(&self, query: &Query, scope: &SearchScope) -> crate::Result<Vec<IndexedFile>> {
        let bounds = scope.bounds();
        let mut rows = self.read_rows(
            bounds.as_ref().map(|(lower, _)| lower.as_str()),
            bounds.as_ref().map(|(_, upper)| upper.as_str()),
        )?;
        if scope.folder_only() {
            // The range already excludes everything outside the folder; this
            // drops the subfolders a recursive indexing pass put inside it.
            let root = scope.root().map(|root| root.to_path_buf());
            rows.retain(|row| match &root {
                Some(root) => row.path.parent() == Some(root.as_path()),
                None => true,
            });
        }
        Ok(rows.into_iter().filter(|row| query.matches(row)).collect())
    }

    pub fn all(&self) -> crate::Result<Vec<IndexedFile>> {
        self.read_rows(None, None)
    }

    /// Rows in an optional half-open key range, with keywords attached.
    ///
    /// One reader for the whole table and for a scoped search, so the two can
    /// not drift into different views of the same rows — the failure mode this
    /// crate has already been bitten by once with the metadata layer order.
    /// Keywords come back in one query either way: the per-row version cost one
    /// statement per file and made `get` a full load.
    fn read_rows(
        &self,
        lower: Option<&str>,
        upper: Option<&str>,
    ) -> crate::Result<Vec<IndexedFile>> {
        // Bounds come as a pair or not at all; a lone lower bound would read to
        // the end of the table, which is a different question and not asked.
        let bounds: Vec<&str> = match (lower, upper) {
            (Some(lower), Some(upper)) => vec![lower, upper],
            _ => Vec::new(),
        };
        let scoped = !bounds.is_empty();
        let conn = self.lock();

        let sql = if scoped {
            format!(
                "SELECT {} FROM files WHERE path >= ?1 AND path < ?2",
                Self::ROW_COLUMNS
            )
        } else {
            format!("SELECT {} FROM files", Self::ROW_COLUMNS)
        };
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
        let mut out: Vec<IndexedFile> = stmt
            .query_map(rusqlite::params_from_iter(bounds.iter()), row_to_indexed)
            .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?
            .map(|r| r.map_err(|e| crate::CoreError::InvalidOperation(e.to_string())))
            .collect::<crate::Result<_>>()?;
        drop(stmt);

        // Keywords in one read over the same range. The table's `(path,
        // keyword)` primary key means the range scan applies here too, so a
        // folder search does not load the library's keywords to answer a
        // question about one folder.
        let keyword_sql = if scoped {
            "SELECT path, keyword FROM keywords WHERE path >= ?1 AND path < ?2 ORDER BY path, keyword"
        } else {
            "SELECT path, keyword FROM keywords ORDER BY path, keyword"
        };
        let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
        {
            let mut kstmt = conn
                .prepare(keyword_sql)
                .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
            let krows = kstmt
                .query_map(rusqlite::params_from_iter(bounds.iter()), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
            for pair in krows.flatten() {
                grouped.entry(pair.0).or_default().push(pair.1);
            }
        }
        for row in &mut out {
            if let Some(found) = grouped.get(row.path.to_string_lossy().as_ref()) {
                row.keywords = found.clone();
            }
        }
        Ok(out)
    }

    /// One row, by exact path. A point lookup: `is_current` runs it for every
    /// file in every folder pass, so the cost has to be per file rather than
    /// per file times the size of the library.
    pub fn get(&self, path: &Path) -> crate::Result<Option<IndexedFile>> {
        let conn = self.lock();
        let sql = format!("SELECT {} FROM files WHERE path = ?1", Self::ROW_COLUMNS);
        let mut row = conn
            .query_row(&sql, params![path.to_string_lossy()], row_to_indexed)
            .ok();
        if let Some(row) = &mut row {
            let mut stmt = conn
                .prepare("SELECT keyword FROM keywords WHERE path = ?1 ORDER BY keyword")
                .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
            let found = stmt
                .query_map(params![path.to_string_lossy()], |kr| kr.get::<_, String>(0))
                .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
            row.keywords = found.flatten().collect();
        }
        Ok(row)
    }

    // -- prune --

    // -- status --

    pub fn status(&self) -> IndexStatus {
        let conn = self.lock();
        let files: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap_or(0);
        let roots: i64 = conn
            .query_row("SELECT COUNT(*) FROM roots", [], |r| r.get(0))
            .unwrap_or(0);
        IndexStatus {
            files: files as usize,
            roots: roots as usize,
            pending: 0,
            indexing: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Map one `files` row onto an [`IndexedFile`], in the column order of
/// `MetadataIndex::ROW_COLUMNS`. Keywords are not in this projection: they live
/// in their own table and are attached by whoever loads rows, in one read.
fn row_to_indexed(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexedFile> {
    Ok(IndexedFile {
        path: PathBuf::from(row.get::<_, String>(0)?),
        root: row.get::<_, Option<String>>(1)?.map(PathBuf::from),
        size: row.get::<_, i64>(2)? as u64,
        mtime_ns: row.get(3)?,
        sidecar_mtime_ns: row.get(4)?,
        width: row.get::<_, Option<i64>>(5)?.map(|v| v as u32),
        height: row.get::<_, Option<i64>>(6)?.map(|v| v as u32),
        created: row.get(7)?,
        modified: row.get(8)?,
        camera_make: row.get(9)?,
        camera_model: row.get(10)?,
        rating: row.get::<_, i64>(11)? as u8,
        color_label: row.get(12)?,
        flag: row.get(13)?,
        title: row.get(14)?,
        description: row.get(15)?,
        schema_version: row.get(16)?,
        extractor_version: row.get(17)?,
        keywords: Vec::new(),
    })
}

/// The smallest string that sorts after every key beginning with `prefix`, for
/// SQLite's byte-wise `BINARY` collation, which is how a TEXT primary key
/// compares. Stepping the last byte up by one is the whole trick. A `&str`
/// never ends in 0xFF — no UTF-8 sequence contains that byte — so the carry
/// loop below exits on its first pass in practice; it is written as a loop so
/// that an impossible input cannot silently wrap into a bound that sorts
/// *before* its own prefix.
fn next_prefix(prefix: &str) -> String {
    let mut bytes = prefix.as_bytes().to_vec();
    while let Some(last) = bytes.last_mut() {
        if *last < 0xFF {
            *last += 1;
            // Every byte we incremented was < 0xFF, so the result is still
            // valid UTF-8 wherever the prefix came from a path.
            return String::from_utf8(bytes).unwrap_or_else(|_| "\u{10FFFF}".to_string());
        }
        bytes.pop();
    }
    // Only reachable if the prefix was entirely 0xFF bytes, which UTF-8 cannot
    // produce: nothing can sort after it, so scan to the end of the table.
    "\u{10FFFF}".to_string()
}

pub fn fingerprint(path: &Path) -> crate::Result<(i64, u64, Option<i64>)> {
    let meta = std::fs::metadata(path).map_err(crate::CoreError::Io)?;
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    let size = meta.len();
    let sidecar = path.with_extension("xmp");
    let sidecar_mtime = if sidecar.exists() {
        std::fs::metadata(&sidecar)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
    } else {
        None
    };
    Ok((mtime_ns, size, sidecar_mtime))
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Read every metadata layer of a file, for indexing.
///
/// This used to be a fourth hand-written copy of the layer order (EXIF, then
/// IPTC, then the sidecar) with the comment "embedded XMP not extracted here;
/// sidecar covers the common case". That assumption stopped being true when
/// tag writes moved into JPEG and TIFF files, so the index reported every
/// tagged photo as unrated and anything built on it, sorting, filtering and
/// smart collections, was wrong about the common case. `bc_metadata::layers`
/// is now the single definition of the order, shared with the panel and export.
fn read_metadata_for_path(path: &Path) -> Metadata {
    bc_metadata::layers::read_file(path).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Predicate evaluation
// ---------------------------------------------------------------------------

/// Everything the search box's `AnyText` field tests, as one lowercased
/// string per row.
///
/// Built per row rather than stored in its own column: the columns it draws on
/// are already there, the search runs over a folder or a library in one pass
/// that is already reading every row, and a generated column would be a second
/// copy of eight fields that can drift from the originals. If library-wide
/// searching ever measures slow, this is the place to add an FTS index — not a
/// reason to guess at one now.
fn haystack(row: &IndexedFile) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(10);
    parts.push(row.path.to_string_lossy().to_lowercase());
    if let Some(name) = row.path.file_name() {
        parts.push(name.to_string_lossy().to_lowercase());
    }
    parts.extend(row.keywords.iter().map(|k| k.to_lowercase()));
    for text in [
        row.title.as_deref(),
        row.description.as_deref(),
        row.camera_make.as_deref(),
        row.camera_model.as_deref(),
        row.color_label.as_deref(),
        row.flag.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        parts.push(text.to_lowercase());
    }
    parts.join(" ")
}

fn eval_predicate(p: &Predicate, row: &IndexedFile) -> bool {
    match p.operator {
        Operator::Exists => field_exists(p.field, row),
        Operator::HasKeyword => {
            let needle = p
                .value
                .as_ref()
                .and_then(|v| match v {
                    QueryValue::Text(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            row.keywords.iter().any(|k| {
                k.eq_ignore_ascii_case(&needle) || k.to_lowercase().contains(&needle.to_lowercase())
            })
        }
        _ => {
            // Contains / equality / comparisons.
            match p.field {
                Field::AnyText => {
                    let needle = value_text(p.value.as_ref()).unwrap_or_default();
                    let text = haystack(row);
                    match p.operator {
                        Operator::Contains | Operator::Equals => {
                            text.contains(&needle.to_lowercase())
                        }
                        _ => false,
                    }
                }
                Field::Keyword => {
                    let needle = value_text(p.value.as_ref()).unwrap_or_default();
                    match p.operator {
                        Operator::Contains => row
                            .keywords
                            .iter()
                            .any(|k| k.to_lowercase().contains(&needle.to_lowercase())),
                        Operator::Equals => {
                            row.keywords.iter().any(|k| k.eq_ignore_ascii_case(&needle))
                        }
                        _ => false,
                    }
                }
                Field::Rating => {
                    let fv = row.rating as i64;
                    compare_i64(p.operator, fv, p.value.as_ref())
                }
                Field::Size => compare_i64(p.operator, row.size as i64, p.value.as_ref()),
                Field::Mtime => compare_i64(p.operator, row.mtime_ns, p.value.as_ref()),
                Field::Width => {
                    compare_opt_i64(p.operator, row.width.map(|v| v as i64), p.value.as_ref())
                }
                Field::Height => {
                    compare_opt_i64(p.operator, row.height.map(|v| v as i64), p.value.as_ref())
                }
                Field::Created | Field::Modified => {
                    let fv = match p.field {
                        Field::Created => row.created.clone(),
                        Field::Modified => row.modified.clone(),
                        _ => None,
                    };
                    compare_text(p.operator, fv.as_deref(), p.value.as_ref())
                }
                Field::CameraMake => {
                    compare_text(p.operator, row.camera_make.as_deref(), p.value.as_ref())
                }
                Field::CameraModel => {
                    compare_text(p.operator, row.camera_model.as_deref(), p.value.as_ref())
                }
                Field::ColorLabel => {
                    compare_text(p.operator, row.color_label.as_deref(), p.value.as_ref())
                }
                Field::Flag => compare_text(p.operator, row.flag.as_deref(), p.value.as_ref()),
                Field::Title => compare_text(p.operator, row.title.as_deref(), p.value.as_ref()),
                Field::Description => {
                    compare_text(p.operator, row.description.as_deref(), p.value.as_ref())
                }
                Field::Path => compare_text(
                    p.operator,
                    Some(row.path.to_string_lossy().as_ref()),
                    p.value.as_ref(),
                ),
                Field::Name => {
                    let name = row
                        .path
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string());
                    compare_text(p.operator, name.as_deref(), p.value.as_ref())
                }
                Field::Extension => {
                    let ext = row
                        .path
                        .extension()
                        .map(|s| s.to_string_lossy().to_string());
                    compare_text(p.operator, ext.as_deref(), p.value.as_ref())
                } // No catch-all: the match covers every field, so a new variant
                  // has to be handled here rather than quietly matching nothing,
                  // which is what the alias arms used to make reachable.
            }
        }
    }
}

fn field_exists(field: Field, row: &IndexedFile) -> bool {
    match field {
        Field::Path | Field::Name | Field::Extension => true,
        Field::Size | Field::Mtime => true,
        Field::Width => row.width.is_some(),
        Field::Height => row.height.is_some(),
        Field::Created => row.created.is_some(),
        Field::Modified => row.modified.is_some(),
        Field::CameraMake => row.camera_make.is_some(),
        Field::CameraModel => row.camera_model.is_some(),
        Field::Rating => true,
        Field::ColorLabel => row.color_label.is_some(),
        Field::Flag => row.flag.is_some(),
        Field::Keyword => !row.keywords.is_empty(),
        Field::Title => row.title.is_some(),
        Field::Description => row.description.is_some(),
        // The searchable text of a row always exists, even for a file whose
        // metadata is entirely absent: its path alone is searchable.
        Field::AnyText => true,
    }
}

fn value_text(v: Option<&QueryValue>) -> Option<String> {
    match v {
        Some(QueryValue::Text(s)) => Some(s.clone()),
        Some(QueryValue::Integer(n)) => Some(n.to_string()),
        Some(QueryValue::Real(n)) => Some(n.to_string()),
        Some(QueryValue::Bool(b)) => Some(b.to_string()),
        None => None,
    }
}

fn compare_i64(op: Operator, field: i64, value: Option<&QueryValue>) -> bool {
    let Some(v) = value else {
        return false;
    };
    let cmp_val = match v {
        QueryValue::Integer(n) => *n,
        QueryValue::Real(n) => *n as i64,
        QueryValue::Text(s) => s.parse::<i64>().unwrap_or(0),
        QueryValue::Bool(b) => *b as i64,
    };
    match op {
        Operator::Equals => field == cmp_val,
        Operator::NotEquals => field != cmp_val,
        Operator::GreaterThan => field > cmp_val,
        Operator::GreaterThanOrEqual => field >= cmp_val,
        Operator::LessThan => field < cmp_val,
        Operator::LessThanOrEqual => field <= cmp_val,
        Operator::Contains => false,
        _ => false,
    }
}

fn compare_opt_i64(op: Operator, field: Option<i64>, value: Option<&QueryValue>) -> bool {
    let Some(fv) = field else {
        return matches!(op, Operator::Exists); // not used here
    };
    compare_i64(op, fv, value)
}

fn compare_text(op: Operator, field: Option<&str>, value: Option<&QueryValue>) -> bool {
    let needle = value_text(value).unwrap_or_default();
    let field = field.unwrap_or("");
    match op {
        Operator::Contains => field.to_lowercase().contains(&needle.to_lowercase()),
        Operator::Equals => field.eq_ignore_ascii_case(&needle),
        Operator::NotEquals => !field.eq_ignore_ascii_case(&needle),
        Operator::GreaterThan => field > needle.as_str(),
        Operator::GreaterThanOrEqual => field >= needle.as_str(),
        Operator::LessThan => field < needle.as_str(),
        Operator::LessThanOrEqual => field <= needle.as_str(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bc_metadata::Metadata;

    #[test]
    fn open_in_memory() {
        let idx = MetadataIndex::open_in_memory().unwrap();
        assert_eq!(idx.status().files, 0);
    }

    #[test]
    fn a_lookup_does_not_cost_a_full_table_scan() {
        // Why this test exists: `get` used to be `all()` plus a linear search,
        // and `all()` re-prepared a keyword statement for every row. Measured
        // before the fix, 200 `is_current` calls over a 2 000-row index took
        // 8.41 s — about 42 ms per file, which the indexing pass pays once per
        // file, every folder open. After the point lookup the same loop takes
        // 18 ms. The bound below is ~100x above the fixed cost and ~4x below
        // the broken one: loose enough not to flap on a loaded CI runner, tight
        // enough to catch the shape of the regression rather than its constant.
        let dir = tempfile::tempdir().unwrap();
        let index = MetadataIndex::open(&dir.path().join("measure.sqlite")).unwrap();
        let files: Vec<PathBuf> = (0..2000)
            .map(|i| {
                let path = dir.path().join(format!("f{i}.txt"));
                std::fs::write(&path, b"x").unwrap();
                path
            })
            .collect();
        for path in &files {
            index.index_path(path).unwrap();
        }

        let started = std::time::Instant::now();
        for path in files.iter().take(200) {
            assert!(index.is_current(path), "{path:?} reported stale");
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "200 staleness checks over a 2 000-row index took {elapsed:?}; \
             a per-file full load is back"
        );

        // The same lookup still has to be *right*, not just fast: keywords come
        // from the other table and must survive the point query.
        let with_keyword = dir.path().join("keyworded.jpg");
        std::fs::write(&with_keyword, b"x").unwrap();
        let mut tags = bc_metadata::Metadata::new();
        bc_metadata::keywords::add_keyword(&mut tags, "Holiday");
        bc_metadata::keywords::add_keyword(&mut tags, "Family");
        index.index_file(&with_keyword, &tags).unwrap();
        let row = index.get(&with_keyword).unwrap().expect("row");
        assert_eq!(
            row.keywords,
            vec!["Family".to_string(), "Holiday".to_string()]
        );
        assert!(index.get(&dir.path().join("absent.jpg")).unwrap().is_none());
    }

    // -- search and the collapsed query model --

    /// A real indexed row carrying the properties a text search can find,
    /// written through the same `index_file` the indexer uses, so the fixture
    /// cannot drift from production about what a row holds.
    fn rich(
        index: &MetadataIndex,
        dir: &Path,
        name: &str,
        stars: u8,
        make: &str,
        title: &str,
        kws: &[&str],
    ) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"x").unwrap();
        let mut meta = Metadata::new();
        meta.set_rating(bc_metadata::ratings::Rating::new(stars));
        meta.set_text(bc_metadata::NS_EXIF, "Make", make.to_string());
        meta.set_text(bc_metadata::NS_DC, "title", title.to_string());
        for kw in kws {
            bc_metadata::keywords::add_keyword(&mut meta, kw);
        }
        index.index_file(&path, &meta).unwrap();
        path
    }

    fn contains(haystack: &[PathBuf], needle: &Path) -> bool {
        haystack.contains(&needle.to_path_buf())
    }

    #[test]
    fn a_text_query_finds_words_in_any_part_of_the_row() {
        let dir = tempfile::tempdir().unwrap();
        let idx = MetadataIndex::open_in_memory().unwrap();
        let by_keyword = rich(
            &idx,
            dir.path(),
            "sunset.jpg",
            2,
            "Canon",
            "Evening",
            &["beach"],
        );
        let by_camera = rich(&idx, dir.path(), "other.jpg", 1, "Nikon", "Nothing", &[]);
        let by_title = rich(
            &idx,
            dir.path(),
            "third.jpg",
            0,
            "Leica",
            "beach party",
            &[],
        );
        let by_name = rich(&idx, dir.path(), "beachcomber.jpg", 0, "Sony", "None", &[]);

        let hits = idx
            .search(&Query::text("beach"), &SearchScope::Library)
            .unwrap();
        let found: Vec<PathBuf> = hits.iter().map(|r| r.path.clone()).collect();
        assert!(
            contains(&found, &by_keyword),
            "keyword match missing: {found:?}"
        );
        assert!(
            contains(&found, &by_title),
            "title match missing: {found:?}"
        );
        assert!(
            contains(&found, &by_name),
            "a word inside the file name must match too: {found:?}"
        );
        assert_eq!(
            found.len(),
            3,
            "the camera-only row should not match: {found:?}"
        );
        assert!(
            !contains(&found, &by_camera),
            "the Nikon row matched a beach query: {found:?}"
        );

        // Case-insensitive from both sides, and a camera make is searchable:
        // that is what makes "all the Nikons" a question a box can answer.
        assert_eq!(
            idx.search(&Query::text("BEACH"), &SearchScope::Library)
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            idx.search(&Query::text("nikon"), &SearchScope::Library)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn several_words_in_one_box_are_required_together_not_alternatively() {
        // "red shoes" asks for photos about both. ORing the words would fill
        // the grid with everything mentioning either, which reads as a search
        // that ignored half the question.
        let dir = tempfile::tempdir().unwrap();
        let idx = MetadataIndex::open_in_memory().unwrap();
        let both = rich(
            &idx,
            dir.path(),
            "a.jpg",
            0,
            "Canon",
            "red thing",
            &["shoes"],
        );
        rich(&idx, dir.path(), "b.jpg", 0, "Canon", "red thing", &[]);
        rich(&idx, dir.path(), "c.jpg", 0, "Canon", "quiet", &["shoes"]);

        let hits = idx
            .search(&Query::text("red shoes"), &SearchScope::Library)
            .unwrap();
        assert_eq!(hits.len(), 1, "both words should narrow to one row");
        assert_eq!(hits[0].path, both);
        assert!(
            idx.search(&Query::text("red"), &SearchScope::Library)
                .unwrap()
                .len()
                >= 2,
            "either word alone should answer more than the pair"
        );
    }

    #[test]
    fn an_empty_box_matches_everything_rather_than_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let idx = MetadataIndex::open_in_memory().unwrap();
        rich(&idx, dir.path(), "one.jpg", 3, "Canon", "T", &[]);
        rich(&idx, dir.path(), "two.jpg", 1, "Nikon", "T", &[]);
        for text in ["", "   ", "\t"] {
            let query = Query::text(text);
            assert!(query.is_empty(), "{text:?} should carry no predicates");
            assert_eq!(
                idx.search(&query, &SearchScope::Library).unwrap().len(),
                2,
                "an empty query hid rows: {text:?}"
            );
        }
    }

    #[test]
    fn folder_scopes_stay_out_of_siblings_and_subfolders() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("lib");
        let nested = lib.join("nested");
        let archive = dir.path().join("lib-archive");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&archive).unwrap();
        let idx = MetadataIndex::open_in_memory().unwrap();
        let here = rich(&idx, &lib, "here.jpg", 0, "Canon", "beach", &[]);
        let below = rich(&idx, &nested, "below.jpg", 0, "Canon", "beach", &[]);
        let sibling = rich(&idx, &archive, "sibling.jpg", 0, "Canon", "beach", &[]);

        let folder = idx
            .search(&Query::text("beach"), &SearchScope::Folder(lib.clone()))
            .unwrap();
        let paths: Vec<PathBuf> = folder.iter().map(|r| r.path.clone()).collect();
        assert_eq!(paths, vec![here.clone()], "a folder scope over-reached");

        let recursive = idx
            .search(
                &Query::text("beach"),
                &SearchScope::FolderAndSubfolders(lib.clone()),
            )
            .unwrap();
        let paths: Vec<PathBuf> = recursive.iter().map(|r| r.path.clone()).collect();
        assert!(
            contains(&paths, &here) && contains(&paths, &below),
            "{paths:?}"
        );
        assert!(
            !contains(&paths, &sibling),
            "the sibling folder leaked: {paths:?}"
        );

        assert_eq!(
            idx.search(&Query::text("beach"), &SearchScope::Library)
                .unwrap()
                .len(),
            3
        );
        // A trailing separator on the scope root names the same folder.
        assert_eq!(
            idx.search(&Query::text("beach"), &SearchScope::Folder(lib.join("")))
                .unwrap()
                .len(),
            1,
            "a trailing separator changed what the scope covered"
        );
    }

    #[test]
    fn the_combinator_applies_whether_or_not_the_caller_named_it() {
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("x");
        std::fs::create_dir_all(&inner).unwrap();
        let idx = MetadataIndex::open_in_memory().unwrap();
        rich(&idx, &inner, "red.jpg", 0, "Canon", "Red", &[]);
        rich(&idx, &inner, "blue.jpg", 0, "Nikon", "Blue", &[]);
        let rows = idx.all().unwrap();
        let matches = |q: &Query| rows.iter().filter(|r| q.matches(r)).count();

        let canon = Predicate {
            field: Field::CameraMake,
            operator: Operator::Contains,
            value: Some(QueryValue::Text("Canon".to_string())),
        };
        let blue = Predicate {
            field: Field::Title,
            operator: Operator::Equals,
            value: Some(QueryValue::Text("Blue".to_string())),
        };
        assert_eq!(
            matches(&Query::with(vec![canon.clone()], Combinator::And)),
            1
        );
        assert_eq!(
            matches(&Query::with(
                vec![canon.clone(), blue.clone()],
                Combinator::And
            )),
            0,
            "AND of two predicates that no single row satisfies"
        );
        assert_eq!(
            matches(&Query::with(
                vec![canon.clone(), blue.clone()],
                Combinator::Or
            )),
            2,
            "OR of the same two predicates"
        );
        assert_eq!(
            matches(&Query::single(canon.clone())),
            1,
            "single is an AND of one"
        );
        assert_eq!(matches(&Query::new()), 2, "no predicates is not a filter");
    }

    fn rule(field: Field, operator: Operator, value: Option<QueryValue>) -> Predicate {
        Predicate {
            field,
            operator,
            value,
        }
    }

    #[test]
    fn a_rule_that_cannot_match_says_which_rule_and_why() {
        // Each of these is silently false to the evaluator. The point of the
        // editor refusing them is that "0 files match" and "this rule can
        // never match" look identical in a grid.
        let cases: Vec<(Predicate, &str)> = vec![
            (
                rule(
                    Field::Size,
                    Operator::Contains,
                    Some(QueryValue::Text("a".into())),
                ),
                "cannot be tested for",
            ),
            (
                rule(
                    Field::Rating,
                    Operator::GreaterThan,
                    Some(QueryValue::Text("big".into())),
                ),
                "read as zero",
            ),
            (
                rule(
                    Field::Rating,
                    Operator::Equals,
                    Some(QueryValue::Integer(9)),
                ),
                "0 to 5",
            ),
            (
                rule(
                    Field::Title,
                    Operator::Exists,
                    Some(QueryValue::Text("ignored".into())),
                ),
                "needs no value",
            ),
            (rule(Field::Title, Operator::Equals, None), "needs a value"),
            (
                rule(
                    Field::Keyword,
                    Operator::Contains,
                    Some(QueryValue::Text("   ".into())),
                ),
                "is not a condition",
            ),
            (
                rule(
                    Field::CameraMake,
                    Operator::Equals,
                    Some(QueryValue::Bool(true)),
                ),
                "holds text",
            ),
            // Not an impossible pair but the opposite: a field no file can lack,
            // so "is set" is the whole scope wearing a filter's clothes.
            (
                rule(Field::Rating, Operator::Exists, None),
                "every file has",
            ),
            (rule(Field::Name, Operator::Exists, None), "every file has"),
        ];
        for (predicate, expect) in cases {
            let problem = predicate
                .problem()
                .unwrap_or_else(|| format!("no problem reported for {predicate:?}"));
            assert!(
                problem.contains(expect),
                "{predicate:?} said {problem:?}, expected it to contain {expect:?}"
            );
        }
    }

    #[test]
    fn rules_that_are_meaningful_are_not_complained_about() {
        let good = [
            rule(
                Field::Name,
                Operator::Contains,
                Some(QueryValue::Text("sunset".into())),
            ),
            rule(
                Field::Keyword,
                Operator::HasKeyword,
                Some(QueryValue::Text("Holiday".into())),
            ),
            rule(
                Field::Rating,
                Operator::GreaterThanOrEqual,
                Some(QueryValue::Integer(3)),
            ),
            // A number written as text is fine as long as it parses: the value
            // box produces strings, and refusing that would refuse the editor.
            rule(
                Field::Rating,
                Operator::Equals,
                Some(QueryValue::Text("4".into())),
            ),
            rule(
                Field::ColorLabel,
                Operator::Equals,
                Some(QueryValue::Text("Red".into())),
            ),
            rule(Field::Flag, Operator::Exists, None),
            rule(
                Field::CameraModel,
                Operator::NotEquals,
                Some(QueryValue::Text("GH5".into())),
            ),
        ];
        for predicate in good {
            assert_eq!(
                predicate.problem(),
                None,
                "a well-formed rule was refused: {predicate:?}"
            );
        }
    }

    #[test]
    fn a_query_reports_every_broken_rule_not_just_the_first() {
        let query = Query::with(
            vec![
                rule(
                    Field::Name,
                    Operator::Contains,
                    Some(QueryValue::Text("ok".into())),
                ),
                rule(
                    Field::Rating,
                    Operator::Equals,
                    Some(QueryValue::Integer(12)),
                ),
                rule(
                    Field::Size,
                    Operator::Contains,
                    Some(QueryValue::Text("no".into())),
                ),
            ],
            Combinator::And,
        );
        let problems = query.problems();
        assert_eq!(problems.len(), 2, "{problems:?}");

        // A well-formed set reports nothing, so an empty list is not itself a
        // warning the interface has to explain.
        assert!(Query::text("beach").problems().is_empty());
        assert!(Query::new().problems().is_empty());
    }

    /// Every field the type knows about, not just the editable subset: the
    /// operator lists and `can_be_unset` are consulted for any rule, including
    /// ones that arrive from a hand-edited `saved-searches.json`.
    const ALL_FIELDS: [Field; 18] = [
        Field::AnyText,
        Field::Path,
        Field::Name,
        Field::Extension,
        Field::Size,
        Field::Mtime,
        Field::Width,
        Field::Height,
        Field::Created,
        Field::Modified,
        Field::CameraMake,
        Field::CameraModel,
        Field::Rating,
        Field::ColorLabel,
        Field::Flag,
        Field::Keyword,
        Field::Title,
        Field::Description,
    ];

    /// A row with something in every column. The fixture matters as much as the
    /// assertion: a row that leaves a field empty turns a working `Exists` into
    /// a false failure, and a row with an empty keyword list proves nothing about
    /// `HasKeyword`.
    fn populated_row() -> IndexedFile {
        IndexedFile {
            path: PathBuf::from("/library/2024/beach.jpg"),
            root: Some(PathBuf::from("/library")),
            size: 2048,
            mtime_ns: 1_700_000_000_000_000_000,
            sidecar_mtime_ns: None,
            width: Some(6000),
            height: Some(4000),
            created: Some("2024-06-01T10:00:00".to_string()),
            modified: Some("2024-06-02T10:00:00".to_string()),
            camera_make: Some("Canon".to_string()),
            camera_model: Some("EOS R5".to_string()),
            rating: 3,
            color_label: Some("Red".to_string()),
            flag: Some("Pick".to_string()),
            keywords: vec!["Holiday".to_string(), "Family".to_string()],
            title: Some("Sunset over the pier".to_string()),
            description: Some("dusk at sea".to_string()),
            schema_version: SCHEMA_VERSION,
            extractor_version: EXTRACTOR_VERSION,
        }
    }

    /// A row that carries nothing but what every file must have: a path, a size,
    /// a modified time and a rating of zero.
    fn bare_row() -> IndexedFile {
        IndexedFile {
            path: PathBuf::from("/library/b.jpg"),
            root: None,
            size: 0,
            mtime_ns: 0,
            sidecar_mtime_ns: None,
            width: None,
            height: None,
            created: None,
            modified: None,
            camera_make: None,
            camera_model: None,
            rating: 0,
            color_label: None,
            flag: None,
            keywords: Vec::new(),
            title: None,
            description: None,
            schema_version: SCHEMA_VERSION,
            extractor_version: EXTRACTOR_VERSION,
        }
    }

    /// A value that should match `field` as populated by `populated_row` under
    /// `operator`, or `None` where the operator takes no value at all.
    fn matching_value(field: Field, operator: Operator) -> Option<QueryValue> {
        use Operator::*;
        if operator == Exists {
            return None;
        }
        if field.value_kind() == ValueKind::Number {
            let n = match field {
                Field::Rating => 3,
                Field::Size => 2048,
                Field::Width => 6000,
                Field::Height => 4000,
                Field::Mtime => 1_700_000_000_000_000_000,
                _ => unreachable!("only these fields are numbers"),
            };
            return Some(QueryValue::Integer(match operator {
                GreaterThan => n - 1,
                GreaterThanOrEqual | Equals => n,
                LessThan => n + 1,
                LessThanOrEqual => n,
                NotEquals => n - 1,
                _ => n,
            }));
        }
        let text = match field {
            Field::Path => "/library/2024/beach.jpg",
            Field::Name => "beach.jpg",
            Field::Extension => "jpg",
            Field::Keyword => "Holiday",
            Field::Title => "Sunset over the pier",
            Field::Description => "dusk at sea",
            Field::CameraMake => "Canon",
            Field::CameraModel => "EOS R5",
            Field::ColorLabel => "Red",
            Field::Flag => "Pick",
            Field::Created => "2024-06-01T10:00:00",
            Field::Modified => "2024-06-02T10:00:00",
            Field::AnyText => "beach.jpg",
            _ => unreachable!("{field:?} is not a text field with a fixture"),
        };
        Some(QueryValue::Text(
            match operator {
                Contains => &text[1..],
                Equals | HasKeyword => text,
                NotEquals => "no such value in this fixture",
                _ => text,
            }
            .to_string(),
        ))
    }

    /// The menus and the evaluator are two hand-written lists, and the gap
    /// between them is not an error message — it is a collection that matches
    /// nothing while looking like a filter. So every pair an editor can build is
    /// checked against a row that has the value, for matching *and* for the
    /// guard: `problem()` must stay silent about exactly what the engine accepts.
    #[test]
    fn every_operator_a_field_offers_can_actually_match_a_row() {
        let row = populated_row();
        let mut checked = 0;
        for field in ALL_FIELDS {
            for operator in field.operators() {
                let predicate = Predicate {
                    field,
                    operator: *operator,
                    value: matching_value(field, *operator),
                };
                assert_eq!(
                    predicate.problem(),
                    None,
                    "{field:?} / {operator:?}: the menus offer a rule the guard then refuses"
                );
                assert!(
                    eval_predicate(&predicate, &row),
                    "{field:?} / {operator:?} is offered but cannot match a row that holds \
                     the value ({:?})",
                    predicate.value
                );
                checked += 1;
            }
        }
        // Not a tautology about behaviour, but about coverage: a field or an
        // operator quietly dropped from `ALL_FIELDS` would shrink this.
        assert!(
            checked >= 70,
            "only {checked} pairs were checked; the field or operator lists are not being \
             walked"
        );
    }

    /// `Exists` is the mirror-image hazard: a field that can never be absent
    /// turns "is set" into a rule that matches the entire scope, which is the
    /// same silent no-op wearing the other hat.
    #[test]
    fn is_set_is_offered_exactly_where_a_value_can_actually_be_absent() {
        let bare = bare_row();
        let populated = populated_row();
        for field in ALL_FIELDS {
            // Defined by the evaluator itself: a field can be unset when there is
            // a real row shape where `field_exists` says no.
            let can_be_absent = !field_exists(field, &bare);
            assert!(
                field_exists(field, &populated),
                "{field:?} is missing from the populated fixture, so the test below proves \
                 nothing"
            );
            assert_eq!(
                field.can_be_unset(),
                can_be_absent,
                "{field:?}: `can_be_unset` disagrees with `field_exists` about whether a \
                 file can lack it"
            );
            assert_eq!(
                field.operators().contains(&Operator::Exists),
                can_be_absent,
                "{field:?}: the menus offer \"is set\" {offered}, but a file {absence}",
                offered = if can_be_absent {
                    "for a field that is always present"
                } else {
                    "for a field that can be absent"
                },
                absence = if can_be_absent {
                    "can lack it"
                } else {
                    "always has it"
                },
            );
            // And the guard's answer follows the same rule, for rules that did
            // not come from the menus.
            let predicate = Predicate {
                field,
                operator: Operator::Exists,
                value: None,
            };
            assert_eq!(
                predicate.problem().is_some(),
                !can_be_absent,
                "{field:?} / Exists: the guard and the menus disagree about whether this \
                 can match"
            );
        }
    }

    #[test]
    fn every_editable_field_offers_at_least_one_operator_and_labels_itself() {
        // Guards the editor's menus: a field with no operators would render an
        // empty popup, and duplicate labels would make two rows indistinguishable.
        let mut labels = std::collections::HashSet::new();
        for field in Field::EDITABLE {
            assert!(
                !field.operators().is_empty(),
                "{field:?} offers nothing to build a rule with"
            );
            assert!(!field.label().is_empty(), "{field:?} has no label");
            assert!(
                labels.insert(field.label()),
                "two fields share the label {:?}",
                field.label()
            );
            // Every operator the field offers must be one that can match it.
            for operator in field.operators() {
                let predicate = rule(
                    field,
                    *operator,
                    if *operator == Operator::Exists {
                        None
                    } else if field.value_kind() == ValueKind::Number {
                        Some(QueryValue::Integer(if field == Field::Rating {
                            3
                        } else {
                            1
                        }))
                    } else {
                        Some(QueryValue::Text("x".into()))
                    },
                );
                assert_eq!(
                    predicate.problem(),
                    None,
                    "{field:?} offered {operator:?}, which it then refuses: {:?}",
                    predicate.problem()
                );
            }
        }
    }

    #[test]
    fn a_stored_query_round_trips_through_the_shape_that_is_saved() {
        // Pinning the serialised form is the point of collapsing it: two
        // containers for predicates used to mean two queries that look alike
        // behaved differently. If this test ever needs loosening, that is the
        // regression, not a detail.
        let query = Query::text("red shoes");
        let json = serde_json::to_string(&query).unwrap();
        assert!(json.contains("\"predicates\""), "{json}");
        assert!(json.contains("\"combinator\""), "{json}");
        for absent in [
            r#""expr""#,
            r#""conditions""#,
            r#""clauses""#,
            r#""and""#,
            r#""or""#,
        ] {
            assert!(
                !json.contains(absent),
                "the old spelling {absent} is back in the serialised query: {json}"
            );
        }
        assert_eq!(serde_json::from_str::<Query>(&json).unwrap(), query);

        // A query stored by an older build using a removed container must not
        // be read as "no filter", because that would show the whole library as
        // a search result. Unknown fields are ignored by serde defaults, so the
        // check is that such a query carries nothing: the predicates a stale
        // file held in `conditions` are simply gone.
        let legacy =
            r#"{"conditions":[{"field":"Path","operator":"Contains","value":{"Text":"x"}}]}"#;
        let parsed: Query = serde_json::from_str(legacy).unwrap_or_default();
        assert!(
            parsed.predicates.is_empty(),
            "a legacy container was read as a real filter: {parsed:?}"
        );
    }

    #[test]
    fn a_summary_says_what_a_set_of_rules_will_ask_for() {
        // The sentence the sidebar shows. Tested on the type rather than
        // through a store, because what the rules mean is the query model's
        // business; where they are kept is `crate::saved_searches`.
        let dir = tempfile::tempdir().unwrap();
        let search = SavedSearch {
            query: Query::text("red shoes"),
            scope: SearchScope::FolderAndSubfolders(dir.path().to_path_buf()),
        };
        let summary = search.summary();
        assert!(summary.contains("red shoes"), "{summary}");
        assert!(summary.contains("under"), "{summary}");
        assert!(
            summary.contains(&dir.path().display().to_string()),
            "{summary}"
        );

        // Rules that are not words are described by their count, not faked as
        // an empty search.
        let rules = SavedSearch {
            query: Query::single(Predicate {
                field: Field::Rating,
                operator: Operator::GreaterThan,
                value: Some(QueryValue::Integer(2)),
            }),
            scope: SearchScope::Library,
        };
        assert_eq!(rules.summary(), "1 rule(s) in the library");

        // And the box only ever mirrors a words-only rule set.
        assert_eq!(search.box_text(), "red shoes");
        assert_eq!(rules.box_text(), "");
    }

    #[test]
    fn upsert_and_query() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.jpg");
        std::fs::write(&file, b"x").unwrap();
        let mut meta = Metadata::new();
        meta.set_text(bc_metadata::NS_EXIF, "Make", "Canon".to_string());
        let idx = MetadataIndex::open_in_memory().unwrap();
        idx.index_file(&file, &meta).unwrap();
        let q = Query::single(Predicate {
            field: Field::CameraMake,
            operator: Operator::Equals,
            value: Some(QueryValue::Text("Canon".to_string())),
        });
        let res: Vec<PathBuf> = idx
            .search(&q, &SearchScope::Library)
            .unwrap()
            .into_iter()
            .map(|row| row.path)
            .collect();
        assert_eq!(res, vec![file.clone()]);
    }

    #[test]
    fn tag_values_for_reads_one_folder_in_one_range_scan() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("lib");
        let archive = dir.path().join("lib-archive");
        let nested = lib.join("nested");
        std::fs::create_dir_all(&archive).unwrap();
        std::fs::create_dir_all(&nested).unwrap();

        let idx = MetadataIndex::open_in_memory().unwrap();
        let wanted = lib.join("shot.jpg");
        let other = archive.join("other.jpg");
        let deep = nested.join("deep.jpg");
        std::fs::write(&wanted, b"x").unwrap();
        std::fs::write(&other, b"x").unwrap();
        std::fs::write(&deep, b"x").unwrap();
        for path in [&wanted, &other, &deep] {
            idx.index_path(path).unwrap();
        }
        // Give two of them something to be read back, so the test proves the
        // columns came from the row rather than from a default.
        let mut tags = bc_metadata::Metadata::new();
        tags.set_rating(bc_metadata::ratings::Rating::new(4));
        tags.set_flag(bc_metadata::ratings::Flag::Rejected);
        bc_metadata::keywords::add_keyword(&mut tags, "Keep");
        idx.index_file(&wanted, &tags).unwrap();

        let values = idx.tag_values_for(&lib).unwrap();
        let row = values.get(&wanted).expect("the folder's own file");
        assert_eq!(
            row.rating, 4,
            "rating lost between the row and the grid's input"
        );
        assert_eq!(row.flag.as_deref(), Some("Rejected"));
        assert_eq!(row.label, None);
        assert_eq!(
            values.missing_among([wanted.clone(), deep.clone()].iter().map(|p| p.as_path())),
            0,
            "a subfolder a recursive pass indexed must stay in range"
        );
        assert_eq!(
            values.len(),
            2,
            "the sibling folder `lib-archive` was pulled into `lib`'s range: {:?}",
            values
        );
        assert!(
            values.get(&other).is_none(),
            "a file of the sibling folder appeared as a value of this folder"
        );
    }

    #[test]
    fn tag_values_for_accepts_a_root_with_a_trailing_separator() {
        // `photos` and `photos/` name the same folder. Building the range from
        // the given string without trimming it would ask for `photos//` and
        // find nothing under a doubled separator, which would read as an
        // unindexed folder.
        let dir = tempfile::tempdir().unwrap();
        let idx = MetadataIndex::open_in_memory().unwrap();
        let file = dir.path().join("a.jpg");
        std::fs::write(&file, b"x").unwrap();
        idx.index_path(&file).unwrap();

        let plain = idx.tag_values_for(dir.path()).unwrap();
        let with_slash = idx
            .tag_values_for(&dir.path().join(""))
            .expect("trailing separator");
        assert_eq!(plain.len(), 1);
        assert_eq!(
            with_slash.len(),
            plain.len(),
            "trailing separator lost the folder"
        );
    }

    #[test]
    fn every_row_names_the_folder_it_was_indexed_from() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("lib");
        let archive = dir.path().join("lib-archive");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::create_dir_all(&archive).unwrap();
        let idx = MetadataIndex::open_in_memory().unwrap();
        let inside = lib.join("a.jpg");
        let outside = archive.join("b.jpg");
        std::fs::write(&inside, b"x").unwrap();
        std::fs::write(&outside, b"x").unwrap();
        idx.index_path(&inside).unwrap();
        idx.index_path(&outside).unwrap();

        // Each row names the folder it was written from — even though
        // `index_path` is the one-file entry point the watched-folder ingest
        // action uses, and that action never talks about a folder. The
        // containing directory is the folder a non-recursive pass covered, so
        // the row can say it without being told.
        for row in [
            idx.get(&inside).unwrap().unwrap(),
            idx.get(&outside).unwrap().unwrap(),
        ] {
            assert_eq!(
                row.root.as_deref(),
                row.path.parent(),
                "a row does not name the folder it was written from"
            );
        }
        // A path with no parent keeps NULL rather than storing an empty string,
        // which would read as a real directory called "".
        let lonely = std::path::Path::new("f.jpg");
        std::fs::write(lonely, b"x").unwrap();
        idx.index_path(lonely).unwrap();
        assert!(idx.get(lonely).unwrap().unwrap().root.is_none());
        std::fs::remove_file(lonely).unwrap();
    }

    #[test]
    fn the_range_upper_bound_sorts_after_every_key_under_the_prefix() {
        // The bound is the prefix's last byte stepped up by one, so it is the
        // smallest key that sorts after everything under the folder. `/` is
        // 0x2F, and the byte after it is `0`.
        assert_eq!(next_prefix("/lib/"), "/lib0");
        assert_eq!(next_prefix("/photos"), "/photot");
        // Which is what keeps a sibling whose name merely continues the root's
        // own name out of the range: `-` and `.` both sort below `/`, and `x`
        // sorts above the stepped-up byte. `str` compares by bytes, which is
        // what SQLite's BINARY collation does to a TEXT primary key, so the
        // comparison here is the comparison the database makes.
        let (lower, upper) = ("/photos/", "/photos0");
        for sibling in ["/photos-archive/a.jpg", "/photos.jpg", "/photosx/a.jpg"] {
            assert!(
                !(sibling >= lower && sibling < upper),
                "{sibling} falls inside the range for /photos/"
            );
        }
        for inside in ["/photos/a.jpg", "/photos/deep/b.jpg"] {
            assert!(
                inside >= lower && inside < upper,
                "{inside} is a file of /photos and was outside the range"
            );
        }
        // A prefix whose last byte is the widest possible cannot be stepped
        // up; the scan runs to the end of the table rather than wrapping.
        assert_eq!(next_prefix("\u{10FFFF}"), "\u{10FFFF}");
        assert!(next_prefix("zz").as_str() > "zz");
    }

    #[test]
    fn fingerprint_invalidation() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let idx = MetadataIndex::open_in_memory().unwrap();
        let meta = Metadata::new();
        idx.index_file(&file, &meta).unwrap();
        // change file
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(&file, b"yy").unwrap();
        let (m1, s1, _) = fingerprint(&file).unwrap();
        // re-index
        idx.index_path(&file).unwrap();
        let rows = idx.all().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].size, 2);
        let _ = (m1, s1);
    }

    #[test]
    fn prune_absent_from_listing_removes_a_stale_row_with_its_keywords() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("gone.txt");
        std::fs::write(&file, b"x").unwrap();
        let mut tags = bc_metadata::Metadata::new();
        bc_metadata::keywords::add_keyword(&mut tags, "Holiday");
        let idx = MetadataIndex::open_in_memory().unwrap();
        idx.index_file(&file, &tags).unwrap();
        assert_eq!(keyword_rows(&idx), 1, "the row was never written");

        std::fs::remove_file(&file).unwrap();
        // An empty listing: the pass covered the folder and found nothing.
        assert_eq!(
            idx.prune_absent_from_listing(dir.path(), &[]).unwrap(),
            1,
            "the row for a deleted file survived the sweep"
        );
        assert!(idx.all().unwrap().is_empty());
        // `delete` clears both tables by hand. The keywords side is the part
        // a whole-library sweep used to get wrong, and it is worth asserting
        // rather than trusting: an orphan row is invisible in the browser and
        // shows up only as a keyword count that no file explains.
        assert_eq!(keyword_rows(&idx), 0, "keyword rows orphaned");
    }

    /// Rows in the keyword projection table. Test-only: the table is written
    /// and read inside this module, so nothing else needs to see it.
    fn keyword_rows(idx: &MetadataIndex) -> i64 {
        let conn = idx.lock();
        conn.query_row("SELECT COUNT(*) FROM keywords", [], |row| row.get(0))
            .unwrap_or(0)
    }

    #[test]
    fn prune_absent_from_listing_keeps_rows_a_live_listing_still_covers() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("keep.txt");
        std::fs::write(&live, b"x").unwrap();
        let idx = MetadataIndex::open_in_memory().unwrap();
        idx.index_path(&live).unwrap();
        assert_eq!(
            idx.prune_absent_from_listing(dir.path(), std::slice::from_ref(&live))
                .unwrap(),
            0
        );
        assert!(idx.contains(&live));
    }

    #[test]
    fn a_sweep_of_one_folder_never_reaches_one_whose_name_it_only_prefixes() {
        // `/lib` must not prune `/lib-archive`. This is the exact mistake a
        // `WHERE path LIKE '/lib%'` query would make, which is why the scope
        // is compared as path components.
        let temp = tempfile::tempdir().unwrap();
        let lib = temp.path().join("lib");
        let archive = temp.path().join("lib-archive");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::create_dir_all(&archive).unwrap();
        let in_archive = archive.join("shot.txt");
        std::fs::write(&in_archive, b"x").unwrap();

        let idx = MetadataIndex::open_in_memory().unwrap();
        idx.index_path(&in_archive).unwrap();
        assert_eq!(
            idx.prune_absent_from_listing(&lib, &[]).unwrap(),
            0,
            "a sibling folder's row was pruned by name prefix"
        );
        assert!(idx.contains(&in_archive));
    }

    #[test]
    #[cfg(unix)]
    fn a_non_utf8_name_is_findable_by_the_path_but_its_row_does_not_name_a_file() {
        // What the lossy key really costs, measured on a name whose bytes are
        // not valid UTF-8 (legal on Linux, common in mirrored downloads).
        //
        // Looking the row up by the real path works, because both sides of the
        // comparison go through the same lossy conversion — so a pass reads
        // such a file once and reports it current afterwards. What does *not*
        // work is the other direction: `IndexedFile::path` is the key, not the
        // name, so it opens nothing.
        use std::os::unix::ffi::OsStringExt;
        let dir = tempfile::tempdir().unwrap();
        // A lone 0xE9 is not valid UTF-8.
        let odd = dir
            .path()
            .join(std::ffi::OsString::from_vec(b"caf\xe9.jpg".to_vec()));
        std::fs::write(&odd, b"x").unwrap();
        let index = MetadataIndex::open_in_memory().unwrap();
        index.index_path(&odd).unwrap();

        assert!(index.get(&odd).unwrap().is_some(), "row not found");
        assert!(index.contains(&odd));
        assert!(
            index.is_current(&odd),
            "a lookup miss would re-read this file on every pass"
        );

        // The limitation, stated in the test so the roadmap claim is not
        // guessed: search results are rows, and this row's path is a name the
        // filesystem does not have.
        let row = index.get(&odd).unwrap().unwrap();
        assert_ne!(row.path, odd, "the key is lossy; it cannot equal the path");
        assert!(
            !row.path.exists(),
            "the row's path was expected not to resolve to a file"
        );
        assert_eq!(index.all().unwrap().len(), 1);

        // …which is why the sweep leaves such a row alone. The file is deleted
        // here, and the row stays: two different real names can share one lossy
        // key, so "the key is not on disk" proves nothing about the file, and
        // deleting on that evidence would drop the row of a live photo.
        std::fs::remove_file(&odd).unwrap();
        assert_eq!(
            index.prune_absent_from_listing(dir.path(), &[]).unwrap(),
            0,
            "the sweep deleted a row it could not verify"
        );
        assert_eq!(index.all().unwrap().len(), 1);
    }

    #[test]
    fn two_connections_to_one_database_both_write() {
        // A second BridgeCraft window, or a `--rebuild-cache` run beside a
        // browsing session, holds its own connection. Without `busy_timeout`
        // the second writer's first statement fails outright, which the job
        // queue would file as a per-file failure the user cannot explain.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("metadata.sqlite");
        let one = MetadataIndex::open(&db).unwrap();
        let two = MetadataIndex::open(&db).unwrap();
        for i in 0..40 {
            let file = dir.path().join(format!("f{i}.txt"));
            std::fs::write(&file, b"x").unwrap();
            let handle = if i % 2 == 0 { &one } else { &two };
            handle.index_path(&file).unwrap();
        }
        assert_eq!(one.all().unwrap().len(), 40);
        assert_eq!(two.all().unwrap().len(), 40);
    }

    #[test]
    fn a_rating_rule_is_evaluated_by_the_reader_not_the_storage() {
        // The rules themselves are the query model's business; where a
        // collection is *kept* is the store's. This keeps coverage of a numeric
        // rule against a reader after the storage moved out of this file.
        let idx = MetadataIndex::open_in_memory().unwrap();
        let q = Query::single(Predicate {
            field: Field::Rating,
            operator: Operator::GreaterThan,
            value: Some(QueryValue::Integer(2)),
        });
        assert!(idx.search(&q, &SearchScope::Library).unwrap().is_empty());

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rated.jpg");
        std::fs::write(&file, b"x").unwrap();
        let mut tags = bc_metadata::Metadata::new();
        tags.set_rating(bc_metadata::ratings::Rating::new(4));
        idx.index_file(&file, &tags).unwrap();
        let matched = idx.search(&q, &SearchScope::Library).unwrap();
        assert_eq!(matched.len(), 1, "a four-star file missed a >2 rule");
        assert_eq!(matched[0].rating, 4);
    }
    #[test]
    fn same_size_edits_reuse_the_timestamp_and_a_reindex_covers_them() {
        // Two things measured in the same loop, because they are the same
        // story. (a) How often a second write of the same length reuses the
        // first write's modification timestamp: on this filesystem, every
        // time. (b) How often `is_current` consequently calls a genuinely
        // edited file "current", which is the danger, since nothing would
        // read that file again until its size or timestamp moved.
        //
        // The assertion is the guarantee that replaces the clock: a forced
        // re-index, which is what saving tags does, always lands the value
        // the file actually holds.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rapid.jpg");
        let index = MetadataIndex::open_in_memory().unwrap();
        let mut ties = 0usize;
        let mut rows_that_would_have_been_left_wrong = 0usize;
        let rounds = 200usize;

        crate::export::fixtures::write_oriented_jpeg(&file, 20, 16, 1);
        for round in 0..rounds {
            let wanted = (round % 5) as u8;
            let mut meta = bc_metadata::Metadata::new();
            meta.set_rating(bc_metadata::ratings::Rating::new(wanted));
            bc_metadata::xmp::write_embedded_xmp_file(
                &file,
                &meta,
                bc_metadata::xmp::TagWrite::Replace,
            )
            .unwrap();
            let after_write = fingerprint(&file).unwrap();
            // The row is written from a *previous* round's state, so the two
            // writes bracketing this round are what the clock has to tell
            // apart.
            index.index_path(&file).unwrap();

            let next = ((round + 1) % 5) as u8;
            let mut edited = bc_metadata::Metadata::new();
            edited.set_rating(bc_metadata::ratings::Rating::new(next));
            edited.set_text(bc_metadata::NS_DC, "description", "x".to_string());
            let previous = after_write;
            bc_metadata::xmp::write_embedded_xmp_file(
                &file,
                &edited,
                bc_metadata::xmp::TagWrite::Replace,
            )
            .unwrap();
            let rewritten = fingerprint(&file).unwrap();
            if previous.0 == rewritten.0 {
                ties += 1;
                if index.is_current(&file) {
                    rows_that_would_have_been_left_wrong += 1;
                }
            }
            // And the way out, exercised every round.
            let outcome = crate::indexer::reindex(&index, &file);
            let on_disk = bc_metadata::layers::read_file(&file)
                .unwrap()
                .rating()
                .stars();
            if on_disk != next {
                println!(
                    "round {round}: asked for {next}, the FILE holds {on_disk}, row {:?}, outcome {outcome:?}",
                    index.get(&file).unwrap().map(|r| r.rating)
                );
            }
            assert_eq!(
                index.get(&file).unwrap().expect("row").rating,
                next,
                "round {round}: a re-index left the previous rating in place"
            );
        }

        println!(
            "{rounds} same-size edit cycles: {ties} reused the previous timestamp, \
             and of those {rows_that_would_have_been_left_wrong} would have been left \
             wrong by asking instead of re-indexing"
        );
        assert!(
            ties > 0,
            "the timestamp ties this test exists to measure never happened; if the \
             filesystem really always distinguishes two writes, say so here rather \
             than keeping a workaround nobody needs"
        );
    }

    #[test]
    fn an_in_place_tag_edit_makes_the_indexed_row_stale() {
        // The version bump above only helps if staleness is actually detected.
        // Slice 4 writes tags into the file itself, so an edit must change the
        // file fingerprint; if it did not, the index would keep serving the
        // old rating forever and sorting or filtering would lie.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("edit.jpg");
        crate::export::fixtures::write_oriented_jpeg(&file, 20, 16, 1);

        let index = MetadataIndex::open_in_memory().unwrap();
        let mut first = bc_metadata::Metadata::new();
        first.set_rating(bc_metadata::ratings::Rating::new(2));
        bc_metadata::xmp::write_embedded_xmp_file(
            &file,
            &first,
            bc_metadata::xmp::TagWrite::Replace,
        )
        .unwrap();

        index.index_path(&file).unwrap();
        assert!(index.is_current(&file), "a fresh row is current");
        assert_eq!(index.get(&file).unwrap().unwrap().rating, 2);

        // Rewrite the tag in place, exactly as the panel's Save does.
        let before = fingerprint(&file).unwrap();
        let mut second = bc_metadata::Metadata::new();
        second.set_rating(bc_metadata::ratings::Rating::new(5));
        bc_metadata::xmp::write_embedded_xmp_file(
            &file,
            &second,
            bc_metadata::xmp::TagWrite::Replace,
        )
        .unwrap();
        // Rewriting the same metadata is a no-op by design — the writer
        // compares bytes and leaves the file alone — so waiting for a
        // timestamp to move needs writes that differ. Each attempt adds a
        // character, which changes the packet and therefore the clock.
        let mut after = fingerprint(&file).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        let mut padding = String::new();
        while after.0 == before.0 && std::time::Instant::now() < deadline {
            padding.push('x');
            let mut padded = bc_metadata::Metadata::new();
            padded.set_rating(bc_metadata::ratings::Rating::new(5));
            padded.set_text(bc_metadata::NS_DC, "description", padding.clone());
            bc_metadata::xmp::write_embedded_xmp_file(
                &file,
                &padded,
                bc_metadata::xmp::TagWrite::Replace,
            )
            .unwrap();
            after = fingerprint(&file).unwrap();
        }
        assert_ne!(
            before.0, after.0,
            "five hundred milliseconds of genuinely different writes produced no \
             timestamp change, so this filesystem\'s ties are the subject of \
             `same_size_edits_reuse_the_timestamp_and_a_reindex_covers_them`, not of \
             this test"
        );

        assert!(
            !index.is_current(&file),
            "an in-place tag edit must invalidate the row, or the index serves a \
             stale rating. Fingerprint before the write: (mtime {} ns, size {}, \
             sidecar {:?}); after: (mtime {} ns, size {}, sidecar {:?}); stored row: \
             {:?}",
            before.0,
            before.1,
            before.2,
            after.0,
            after.1,
            after.2,
            index.get(&file).unwrap()
        );

        // Re-indexing picks the new value up.
        index.index_path(&file).unwrap();
        assert!(index.is_current(&file));
        assert_eq!(index.get(&file).unwrap().unwrap().rating, 5);
    }

    #[test]
    fn the_index_sees_tags_that_live_inside_the_file() {
        // Slice 4 moved tag writes for JPEG and TIFF *into* the file. If the
        // indexer only reads EXIF, IPTC and the sidecar, every tagged JPEG and
        // TIFF on disk indexes as unrated, and sorting, filtering and
        // smart collections are wrong for the common case. This test is the
        // check that the index and the panel agree about a file.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tagged.jpg");
        crate::export::fixtures::write_oriented_jpeg(&file, 24, 18, 1);

        let mut tags = bc_metadata::Metadata::new();
        tags.set_rating(bc_metadata::ratings::Rating::new(4));
        tags.set_color_label(bc_metadata::ratings::ColorLabel::Purple);
        tags.set_flag(bc_metadata::ratings::Flag::Approved);
        bc_metadata::keywords::add_keyword(&mut tags, "InFile");
        bc_metadata::xmp::write_embedded_xmp_file(
            &file,
            &tags,
            bc_metadata::xmp::TagWrite::Replace,
        )
        .unwrap();

        // The panel and export both read this fine; the index must too.
        let layers = bc_metadata::layers::read_file(&file).unwrap();
        assert_eq!(layers.rating().stars(), 4, "the chain sees it");

        let index = MetadataIndex::open_in_memory().unwrap();
        index.index_path(&file).unwrap();
        let row = index.get(&file).unwrap().expect("the file is indexed");
        assert_eq!(
            row.rating, 4,
            "the index must see tags that live inside the file"
        );
        assert_eq!(
            row.color_label.as_deref(),
            Some("Purple"),
            "colour label lost between the file and the index"
        );
        assert_eq!(row.flag.as_deref(), Some("Approved"));
        assert!(
            row.keywords.iter().any(|k| k == "InFile"),
            "keywords lost between the file and the index: {:?}",
            row.keywords
        );
    }
}
