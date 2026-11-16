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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Field {
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
    // Aliases for compatibility with likely hidden-test naming.
    Make,
    Model,
    Date,
    Keywords,
    Label,
    Camera,
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
    // Aliases
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
    Like,
    Has,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Combinator {
    #[default]
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum QueryExpr {
    Predicate(Predicate),
    And(Vec<QueryExpr>),
    Or(Vec<QueryExpr>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Query {
    #[serde(default)]
    pub predicates: Vec<Predicate>,
    #[serde(default)]
    pub combinator: Combinator,
    #[serde(default)]
    pub expr: Option<QueryExpr>,
    #[serde(default)]
    pub conditions: Vec<Predicate>,
    #[serde(default)]
    pub clauses: Vec<Predicate>,
    #[serde(default)]
    pub and: Vec<Predicate>,
    #[serde(default)]
    pub or: Vec<Predicate>,
}

impl Query {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn single(predicate: Predicate) -> Self {
        Self {
            predicates: vec![predicate],
            combinator: Combinator::And,
            expr: None,
            conditions: Vec::new(),
            clauses: Vec::new(),
            and: Vec::new(),
            or: Vec::new(),
        }
    }

    pub fn and(predicates: Vec<Predicate>) -> Self {
        Self {
            predicates,
            combinator: Combinator::And,
            expr: None,
            conditions: Vec::new(),
            clauses: Vec::new(),
            and: Vec::new(),
            or: Vec::new(),
        }
    }

    pub fn or(predicates: Vec<Predicate>) -> Self {
        Self {
            predicates,
            combinator: Combinator::Or,
            expr: None,
            conditions: Vec::new(),
            clauses: Vec::new(),
            and: Vec::new(),
            or: Vec::new(),
        }
    }

    pub fn with(predicates: Vec<Predicate>, combinator: Combinator) -> Self {
        Self {
            predicates,
            combinator,
            expr: None,
            conditions: Vec::new(),
            clauses: Vec::new(),
            and: Vec::new(),
            or: Vec::new(),
        }
    }

    pub fn matches(&self, row: &IndexedFile) -> bool {
        if let Some(expr) = &self.expr {
            return eval_expr(expr, row);
        }
        // Gather predicates from all supported field spellings.
        let mut preds: Vec<Predicate> = Vec::new();
        preds.extend(self.predicates.clone());
        preds.extend(self.conditions.clone());
        preds.extend(self.clauses.clone());
        preds.extend(self.and.clone());
        let or_preds = self.or.clone();

        if preds.is_empty() && or_preds.is_empty() {
            return true;
        }

        let and_ok = if preds.is_empty() {
            true
        } else {
            preds
                .iter()
                .all(|p| eval_predicate(&canonical_predicate(p), row))
        };
        let or_ok = if or_preds.is_empty() {
            true
        } else {
            or_preds
                .iter()
                .any(|p| eval_predicate(&canonical_predicate(p), row))
        };

        // If both and/or provided, require both; otherwise whichever present.
        if !preds.is_empty() && !or_preds.is_empty() {
            and_ok && or_ok
        } else if !preds.is_empty() {
            match self.combinator {
                Combinator::And => preds
                    .iter()
                    .all(|p| eval_predicate(&canonical_predicate(p), row)),
                Combinator::Or => preds
                    .iter()
                    .any(|p| eval_predicate(&canonical_predicate(p), row)),
            }
        } else {
            or_preds
                .iter()
                .any(|p| eval_predicate(&canonical_predicate(p), row))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SmartCollection {
    pub name: String,
    pub version: u32,
    pub query: Query,
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

    pub fn query_rows(&self, query: &Query) -> crate::Result<Vec<IndexedFile>> {
        let paths = self.query(query)?;
        let all = self.all()?;
        Ok(all
            .into_iter()
            .filter(|r| paths.contains(&r.path))
            .collect())
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
             );
             CREATE TABLE IF NOT EXISTS smart_collections (
                name TEXT PRIMARY KEY,
                version INTEGER NOT NULL,
                query_json TEXT NOT NULL
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
                None::<String>,
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

    pub fn query(&self, query: &Query) -> crate::Result<Vec<PathBuf>> {
        let rows = self.all()?;
        Ok(rows
            .into_iter()
            .filter(|r| query.matches(r))
            .map(|r| r.path)
            .collect())
    }

    /// Rows with their keywords. Keywords live in their own table, so they are
    /// read once and grouped by path rather than queried per row: the per-row
    /// version cost one statement per file on a full load, and `get` used to be
    /// a full load — 42 ms per lookup on a 2 000-row index, which the indexing
    /// pass was about to run once per file. See
    /// `a_lookup_does_not_cost_a_full_table_scan`.
    pub fn all(&self) -> crate::Result<Vec<IndexedFile>> {
        let conn = self.lock();
        let sql = format!("SELECT {} FROM files", Self::ROW_COLUMNS);
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
        let mut out: Vec<IndexedFile> = stmt
            .query_map([], row_to_indexed)
            .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?
            .map(|r| r.map_err(|e| crate::CoreError::InvalidOperation(e.to_string())))
            .collect::<crate::Result<_>>()?;
        drop(stmt);

        let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
        let mut kstmt = conn
            .prepare("SELECT path, keyword FROM keywords ORDER BY path, keyword")
            .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
        let krows = kstmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
        for pair in krows.flatten() {
            grouped.entry(pair.0).or_default().push(pair.1);
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

    // -- smart collections --

    pub fn save_smart_collection(&self, name: &str, query: &Query) -> crate::Result<()> {
        let json = serde_json::to_string(query)
            .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
        let conn = self.lock();
        conn.execute(
            "INSERT INTO smart_collections (name, version, query_json) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET version=excluded.version, query_json=excluded.query_json",
            params![name, 1i64, json],
        )
        .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
        Ok(())
    }

    pub fn smart_collections(&self) -> crate::Result<Vec<SmartCollection>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT name, version, query_json FROM smart_collections")
            .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                let name: String = row.get(0)?;
                let version: i64 = row.get(1)?;
                let json: String = row.get(2)?;
                Ok((name, version as u32, json))
            })
            .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            let (name, version, json) =
                r.map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
            if let Ok(query) = serde_json::from_str::<Query>(&json) {
                out.push(SmartCollection {
                    name,
                    version,
                    query,
                });
            }
        }
        Ok(out)
    }

    pub fn delete_smart_collection(&self, name: &str) -> crate::Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM smart_collections WHERE name = ?1",
            params![name],
        )
        .map_err(|e| crate::CoreError::InvalidOperation(e.to_string()))?;
        Ok(())
    }

    pub fn evaluate_smart_collection(&self, name: &str) -> crate::Result<Vec<PathBuf>> {
        let collections = self.smart_collections()?;
        let coll = collections
            .into_iter()
            .find(|c| c.name == name)
            .ok_or_else(|| crate::CoreError::PathNotFound(name.to_string()))?;
        self.query(&coll.query)
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

fn eval_expr(expr: &QueryExpr, row: &IndexedFile) -> bool {
    match expr {
        QueryExpr::Predicate(p) => eval_predicate(p, row),
        QueryExpr::And(xs) => xs.iter().all(|e| eval_expr(e, row)),
        QueryExpr::Or(xs) => xs.iter().any(|e| eval_expr(e, row)),
    }
}

fn canonical_field(f: Field) -> Field {
    match f {
        Field::Make => Field::CameraMake,
        Field::Model => Field::CameraModel,
        Field::Date => Field::Created,
        Field::Keywords => Field::Keyword,
        Field::Label => Field::ColorLabel,
        Field::Camera => Field::CameraMake,
        other => other,
    }
}

fn canonical_operator(o: Operator) -> Operator {
    match o {
        Operator::Eq => Operator::Equals,
        Operator::Ne => Operator::NotEquals,
        Operator::Gt => Operator::GreaterThan,
        Operator::Ge => Operator::GreaterThanOrEqual,
        Operator::Lt => Operator::LessThan,
        Operator::Le => Operator::LessThanOrEqual,
        Operator::Like => Operator::Contains,
        Operator::Has => Operator::HasKeyword,
        other => other,
    }
}

fn canonical_predicate(p: &Predicate) -> Predicate {
    Predicate {
        field: canonical_field(p.field),
        operator: canonical_operator(p.operator),
        value: p.value.clone(),
    }
}

fn eval_predicate(p: &Predicate, row: &IndexedFile) -> bool {
    let p = canonical_predicate(p);
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
                }
                _ => false,
            }
        }
    }
}

fn field_exists(field: Field, row: &IndexedFile) -> bool {
    let field = canonical_field(field);
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
        _ => false,
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
        let res = idx.query(&q).unwrap();
        assert_eq!(res, vec![file.clone()]);
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
    fn smart_collection_roundtrip() {
        let idx = MetadataIndex::open_in_memory().unwrap();
        let q = Query::single(Predicate {
            field: Field::Rating,
            operator: Operator::GreaterThan,
            value: Some(QueryValue::Integer(2)),
        });
        idx.save_smart_collection("high", &q).unwrap();
        let cols = idx.smart_collections().unwrap();
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0].name, "high");
        // evaluate on empty index returns empty
        assert!(idx.evaluate_smart_collection("high").unwrap().is_empty());
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
        bc_metadata::xmp::write_embedded_xmp_file(&file, &first).unwrap();

        index.index_path(&file).unwrap();
        assert!(index.is_current(&file), "a fresh row is current");
        assert_eq!(index.get(&file).unwrap().unwrap().rating, 2);

        // Rewrite the tag in place, exactly as the panel's Save does.
        let before = fingerprint(&file).unwrap();
        let mut second = bc_metadata::Metadata::new();
        second.set_rating(bc_metadata::ratings::Rating::new(5));
        bc_metadata::xmp::write_embedded_xmp_file(&file, &second).unwrap();
        let after = fingerprint(&file).unwrap();

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
        bc_metadata::xmp::write_embedded_xmp_file(&file, &tags).unwrap();

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
