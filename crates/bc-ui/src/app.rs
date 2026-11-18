//! Main application: favorites/folders sidebar, thumbnail grid, folder
//! navigation, selection, thumbnails, live directory watching, name
//! filtering, metadata panel, and the preview/zoom viewer.
//!
//! The layout mirrors the public Adobe Bridge "Essentials" workspace
//! (left favorites sidebar, central content grid, right Preview/Metadata
//! panel, breadcrumb path bar, bottom status bar with thumbnail zoom) —
//! a visual target only: all code here is BridgeCraft's own (see
//! CLEANROOM.md).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bc_core::auto_process::{AutoAction, AutoProcessor, AutoRule};
use bc_core::browser::BrowserState;
use bc_core::export::{ExportFormat, ExportRequest};
use bc_core::favorites::Favorites;
use bc_core::file_ops::{
    copy_path, delete_path, move_path, rename_path, transfer_into_folder, TransferMode,
};
use bc_core::fs::{SortDir, SortKey};
use bc_core::jobs::{JobKind, JobQueue};
use bc_core::plugins::{Capability as PluginCapability, PluginHost, PluginManager};
use bc_core::recents::Recents;
use bc_core::selection::Selection;
use bc_core::trash::TrashBin;
use bc_core::watcher::Watcher;
use bc_core::{lightcraft, photocraft};
use bc_thumbnails::{CacheStore, Thumbnail};
use eframe::egui;

use crate::batch_rename_ui;
use crate::metadata_edit::{self, BulkReport, MetadataForm};
use crate::tag_widgets;
use crate::viewer::{self, ViewerState, ZoomMode, FULL_EDGE, PREVIEW_EDGE};

/// Default grid cell width; the status-bar zoom slider ranges from this.
const DEFAULT_CELL: f32 = 150.0;
const MIN_CELL: f32 = 96.0;
const MAX_CELL: f32 = 320.0;
const GRID_SPACING: f32 = 8.0;
const THUMB_MAX_EDGE: u32 = 256;
/// Coalesce watcher event bursts into a single refresh.
const WATCHER_DEBOUNCE: Duration = Duration::from_millis(300);
/// How long the search box waits after the last keystroke. Longer than the
/// watcher debounce on purpose: the watcher re-reads a directory, a search
/// scans the whole index.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(450);

/// Type-ahead settles faster than a search has to. The suggestion query is a
/// range over an index — measured at 2 to 25 ms on a 600 000-row keyword table —
/// against a whole-library search at roughly 70 ms per 100 000 rows, so a shorter
/// wait costs a scan nobody asked for twice in a row.
const SUGGEST_DEBOUNCE: Duration = Duration::from_millis(150);

/// How many candidates the keyword box shows. More than this is a list to read
/// rather than a completion to glance at.
const SUGGEST_LIMIT: usize = 8;
const LIST_ROW_HEIGHT: f32 = 28.0;
/// Sidebar "Recent" section cap.
const SIDEBAR_RECENTS: usize = 6;
/// The one accent color in the shell: it marks the current selection and,
/// during a drag, the folder a drop will land in. The two states are told
/// apart by ring weight and offset, never by a second color.
const ACCENT: egui::Color32 = egui::Color32::from_rgb(0, 160, 255);

/// Texture cache key: a file can have several resolutions in flight (grid
/// thumbnail, viewer preview, viewer full-resolution).
type TexKey = (PathBuf, u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ViewMode {
    #[default]
    Thumbnails,
    List,
    Details,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SidebarTab {
    #[default]
    Favorites,
    Folders,
    /// Saved searches. A third tab rather than a section under the other two,
    /// because clicking one of these replaces the grid the way a folder does,
    /// and the panel that lists folders is not where that belongs.
    Collections,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum RightTab {
    #[default]
    Preview,
    Metadata,
}

/// Pending parameters for the Export dialog (issue #15). The dialog is a
/// thin form over `bc_core::export`; all execution logic lives in the
/// export queue.
struct ExportDialog {
    sources: Vec<PathBuf>,
    /// Empty means "beside originals".
    destination: String,
    format_choice: usize,
    quality: i32,
    max_edge: i32,
    /// True when some source is only decodable through the runtime-backed
    /// thumbnail paths (RAW, PDF, video, PSD previews): worth a warning.
    low_tier: bool,
}

impl ExportDialog {
    fn new(sources: Vec<PathBuf>) -> Self {
        // Warn honestly when sources only the thumbnail pipeline can read
        // (RAW/PDF/video/PSD) ride along: they export at thumbnail quality.
        let low_tier = sources
            .iter()
            .any(|path| !bc_thumbnails::decoder::is_supported(path));
        Self {
            sources,
            destination: String::new(),
            format_choice: 0,
            quality: 85,
            max_edge: 1024,
            low_tier,
        }
    }
}

/// One rule being written.
///
/// Compared by value so the live count can tell whether the rule set still
/// means the same thing as the number on screen.
#[derive(Clone, PartialEq)]
struct RuleRow {
    field: bc_core::Field,
    operator: bc_core::Operator,
    /// The value exactly as typed. Kept as text rather than coerced, because
    /// the editor has to be able to show "big" in a number field and say why
    /// that matches nothing -- silently turning it into 0 is the failure this
    /// dialog exists to prevent.
    value: String,
}

/// The collection rule editor, open while `BridgeApp::editor` is `Some`.
///
/// The dialog is state that the frame reads and writes; asking the worker for a
/// match count is the only thing in it that leaves the thread. Counts are asked
/// for, never computed here: a rule over a large library is a table scan, and a
/// scan in the paint path is a frozen window for whoever is typing.
struct CollectionEditor {
    /// Name being saved under.
    name: String,
    /// The collection this dialog opened from, when it opened from one. Saving
    /// under a different name then *creates*, and the dialog says so rather than
    /// letting a rename look like it happened.
    editing: Option<String>,
    /// The words half of the rule set — the same thing the search box holds, and
    /// saved as the same `AnyText` predicates so a collection means one thing
    /// whoever wrote it.
    words: String,
    rows: Vec<RuleRow>,
    combinator: bc_core::Combinator,
    /// Resolved, not a menu choice: a collection can carry a folder that is not
    /// the one on screen, and "this folder" would then silently mean somewhere
    /// else. The dialog shows this value's own sentence, so what is written is
    /// what runs.
    scope: bc_core::SearchScope,
    /// Live count plumbing. Both ends of the channel live here: the app puts the
    /// sender into each job it hands the shared worker and reads the receiver
    /// back, so nothing has to borrow the editor and the request channel out of
    /// `BridgeApp` in the same statement.
    count_tx: Sender<CountAnswer>,
    count_rx: Receiver<CountAnswer>,
    count_seq: u64,
    /// The request in flight and the rules it measures. One field rather than a
    /// sequence and a key separately, because those two agreeing is the whole
    /// content of "this number is about these boxes".
    outstanding: Option<(u64, CountKey)>,
    /// What `count` and `count_error` describe. `None` means nothing on screen
    /// has been counted, which the dialog says in words instead of showing a
    /// stale number or a zero.
    counted: Option<CountKey>,
    count: Option<usize>,
    count_error: Option<String>,
    count_due: Option<Instant>,
}

/// Everything that decides *how many* files a draft collection matches, and
/// nothing else. The name is deliberately absent: renaming a collection does not
/// change its answer, and recounting for a letter typed into the name box would
/// be a table scan for a label.
///
/// Compared by value once per frame instead of marking the editor dirty from
/// each widget. A `schedule_count` at every control is nine places where a
/// future tenth can forget to add one and leave a confident, wrong number on
/// screen — the exact failure this dialog exists to prevent. So the check comes
/// from the rules themselves, not from whoever last touched a box.
#[derive(Clone, PartialEq)]
struct CountKey {
    words: String,
    rows: Vec<RuleRow>,
    combinator: bc_core::Combinator,
    scope: bc_core::SearchScope,
}

impl CollectionEditor {
    /// Blank, or loaded from a saved search. `scope` is resolved by the caller
    /// against the folder on screen, because only the app knows that.
    fn new(
        name: String,
        editing: Option<String>,
        search: Option<&bc_core::SavedSearch>,
        scope: bc_core::SearchScope,
    ) -> Self {
        let (count_tx, count_rx) = channel::<CountAnswer>();
        let (words, rows) = match search.map(split_saved) {
            Some((words, rows)) => (words, rows),
            None => (String::new(), Vec::new()),
        };
        Self {
            name,
            editing,
            words,
            rows,
            combinator: search
                .map(|saved| saved.query.combinator)
                .unwrap_or(bc_core::Combinator::And),
            scope,
            count_tx,
            count_rx,
            count_seq: 0,
            outstanding: None,
            counted: None,
            count: None,
            count_error: None,
            count_due: None,
        }
    }

    /// The ruleset as the engine sees it: words first, then the rows, under one
    /// combinator. A collection saved from here is the same shape the search box
    /// produces, plus whatever the boxes could not type.
    fn query(&self) -> bc_core::Query {
        let mut predicates: Vec<bc_core::Predicate> = self
            .words
            .split_whitespace()
            .map(|word| bc_core::Predicate {
                field: bc_core::Field::AnyText,
                operator: bc_core::Operator::Contains,
                // As typed. The haystack is lowercased inside the evaluator, so
                // folding the case here would buy nothing and cost the user the
                // letters they wrote: a collection reopened as "beach" after it
                // was typed as "Beach" reads as a corrupted file.
                value: Some(bc_core::QueryValue::Text(word.to_string())),
            })
            .collect();
        predicates.extend(self.rows.iter().map(|row| bc_core::Predicate {
            field: row.field,
            operator: row.operator,
            value: row.query_value(),
        }));
        bc_core::Query {
            predicates,
            combinator: self.combinator,
        }
    }

    fn as_saved(&self) -> bc_core::SavedSearch {
        bc_core::SavedSearch {
            query: self.query(),
            scope: self.scope.clone(),
        }
    }

    /// What would be counted if a request went out now.
    fn count_key(&self) -> CountKey {
        CountKey {
            words: self.words.clone(),
            rows: self.rows.clone(),
            combinator: self.combinator,
            scope: self.scope.clone(),
        }
    }

    /// Why the rules on screen cannot be counted yet, in row order.
    ///
    /// While any of these is present the dialog refuses to ask for a number: a
    /// count from a rule set containing an impossible predicate is not a low
    /// result, it is a meaningless one.
    fn problems(&self) -> Vec<String> {
        self.query().problems()
    }

    /// Whether the number on screen still describes the boxes. False while a
    /// request is in flight, which is what makes "counting…" the honest label.
    fn count_is_current(&self, key: &CountKey) -> bool {
        self.counted.as_ref() == Some(key)
    }

    /// Queue a recount once the typing settles. Called every frame; the only
    /// cost when nothing changed is one comparison of small values.
    fn schedule_count(&mut self, key: &CountKey) {
        if self.count_is_current(key) || self.count_due.is_some() {
            return;
        }
        self.count_due = Some(Instant::now() + SEARCH_DEBOUNCE);
    }

    /// Take any count that has landed. An answer that is not the outstanding
    /// one is dropped: it describes rules the user has edited since, and
    /// putting it on screen would be worse than showing nothing.
    fn poll_count(&mut self) {
        let Some((seq, key)) = self.outstanding.clone() else {
            while self.count_rx.try_recv().is_ok() {}
            return;
        };
        while let Ok(answer) = self.count_rx.try_recv() {
            if answer.seq != seq {
                continue;
            }
            self.outstanding = None;
            // Stamped with the rules that were on screen when the request went
            // out, not with the rules on screen now: if the boxes changed during
            // the scan, `count_is_current` is what keeps the number off the
            // screen until a newer request answers.
            self.counted = Some(key);
            self.count_due = None;
            match answer.count {
                Ok(count) => {
                    self.count = Some(count);
                    self.count_error = None;
                }
                Err(error) => {
                    self.count = None;
                    self.count_error = Some(error);
                }
            }
            return;
        }
    }
}

impl RuleRow {
    /// How the typed value reaches the engine: a number field gets a number
    /// when it parses, and anything else stays text so `Predicate::problem`
    /// can explain it rather than a silent zero being compared.
    fn query_value(&self) -> Option<bc_core::QueryValue> {
        if self.operator == bc_core::Operator::Exists {
            return None;
        }
        let trimmed = self.value.trim();
        if trimmed.is_empty() {
            return Some(bc_core::QueryValue::Text(String::new()));
        }
        match self.field.value_kind() {
            // A number field keeps a number; anything that does not parse
            // stays text so `Predicate::problem` can name it. Coercing it to
            // zero here would be the silent failure this dialog exists to
            // prevent, because `compare_i64` reads unparsable text as zero.
            bc_core::ValueKind::Number => {
                let value = match trimmed.parse::<i64>() {
                    Ok(n) => bc_core::QueryValue::Integer(n),
                    Err(_) => bc_core::QueryValue::Text(trimmed.to_string()),
                };
                Some(value)
            }
            // Kept exactly as written, for the same reason as the words: every
            // text comparison in the evaluator folds case itself, so lowering it
            // here only rewrites what the user typed.
            bc_core::ValueKind::Text => Some(bc_core::QueryValue::Text(trimmed.to_string())),
        }
    }

    fn problem(&self) -> Option<String> {
        bc_core::Predicate {
            field: self.field,
            operator: self.operator,
            value: self.query_value(),
        }
        .problem()
    }

    /// A new rule, defaulted so the first thing the popup shows is a pair that
    /// can actually match.
    fn new() -> Self {
        let field = bc_core::Field::Keyword;
        Self {
            field,
            operator: *field
                .operators()
                .first()
                .expect("an editable field offers an operator"),
            value: String::new(),
        }
    }
}

/// The scope rows the editor offers, each with what choosing it would actually
/// search.
///
/// The two folder scopes are absent with no folder open, rather than resolving to
/// the library as `SearchScopeChoice::scope` does: a row that says "this folder"
/// and then searches everything is the same lie the search box tells, and an
/// editor with room for a proper control should not repeat it.
fn scope_options(root: Option<&Path>) -> Vec<(SearchScopeChoice, bc_core::SearchScope)> {
    SearchScopeChoice::ALL
        .into_iter()
        .filter(|choice| root.is_some() || *choice == SearchScopeChoice::Library)
        .map(|choice| (choice, choice.scope(root)))
        .collect()
}

/// Split a stored rule set back into words and rules for editing. A rule this
/// dialog cannot represent keeps its words and drops the rest, which is
/// reported in the dialog rather than kept quiet.
fn split_saved(saved: &bc_core::SavedSearch) -> (String, Vec<RuleRow>) {
    let mut words: Vec<String> = Vec::new();
    let mut rows: Vec<RuleRow> = Vec::new();
    for predicate in &saved.query.predicates {
        if predicate.field == bc_core::Field::AnyText {
            if let Some(bc_core::QueryValue::Text(text)) = &predicate.value {
                words.push(text.clone());
            }
            continue;
        }
        let value = match &predicate.value {
            Some(bc_core::QueryValue::Text(text)) => text.clone(),
            Some(bc_core::QueryValue::Integer(n)) => n.to_string(),
            Some(bc_core::QueryValue::Real(n)) => n.to_string(),
            Some(bc_core::QueryValue::Bool(b)) => b.to_string(),
            None => String::new(),
        };
        rows.push(RuleRow {
            field: predicate.field,
            operator: predicate.operator,
            value,
        });
    }
    (words.join(" "), rows)
}

/// A keyword vocabulary question, asked of the index by the keyword box.
struct SuggestQuery {
    seq: u64,
    prefix: String,
    index: Arc<bc_core::MetadataIndex>,
}

struct SuggestAnswer {
    seq: u64,
    results: Result<Vec<bc_core::KeywordSuggestion>, String>,
}

struct ThumbJob {
    path: PathBuf,
    mtime_ns: i64,
    size: u64,
    max_edge: u32,
    /// Decode through the SQLite cache (grid/preview tiers) or always decode
    /// fresh (the uncached full-resolution tier).
    cached: bool,
}

/// What the search box searches. Two of the three need the folder that is
/// open, so this is an interface choice resolved against the browser rather
/// than the `bc_core::SearchScope` itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SearchScopeChoice {
    /// The folder on screen. The default, because it is what the user is
    /// looking at when they start typing.
    #[default]
    Folder,
    FolderAndSubfolders,
    Library,
}

impl SearchScopeChoice {
    const ALL: [SearchScopeChoice; 3] = [
        SearchScopeChoice::Folder,
        SearchScopeChoice::FolderAndSubfolders,
        SearchScopeChoice::Library,
    ];

    fn label(self) -> &'static str {
        match self {
            SearchScopeChoice::Folder => "this folder",
            SearchScopeChoice::FolderAndSubfolders => "this folder and subfolders",
            SearchScopeChoice::Library => "the whole library",
        }
    }

    /// Resolve against the folder actually open. With nothing open, a folder
    /// scope means the library — searching "nothing" and returning an empty
    /// grid would read as a broken search.
    fn scope(self, root: Option<&Path>) -> bc_core::SearchScope {
        match (self, root) {
            (SearchScopeChoice::Library, _) | (_, None) => bc_core::SearchScope::Library,
            (SearchScopeChoice::Folder, Some(root)) => {
                bc_core::SearchScope::Folder(root.to_path_buf())
            }
            (SearchScopeChoice::FolderAndSubfolders, Some(root)) => {
                bc_core::SearchScope::FolderAndSubfolders(root.to_path_buf())
            }
        }
    }
}

struct ThumbResult {
    path: PathBuf,
    max_edge: u32,
    thumb: Option<Thumbnail>,
}

/// One library-search request. The database handle travels with it rather
/// than living in the worker: the app opens the index lazily on the first
/// folder that needs it, and a worker spawned at startup would have to be
/// re-spawned or polled for that handle otherwise.
/// One rule set to run against the index, and the index to run it against.
/// Shared by the search box (which wants the rows) and the collection editor
/// (which wants to know how many there are).
struct SearchQuery {
    seq: u64,
    /// Rules to run, already built: from the search box, or from a saved
    /// collection. The worker does not know how to read the box, and should
    /// not have to — one place turns intent into a `Query`.
    query: bc_core::Query,
    scope: bc_core::SearchScope,
    index: Arc<bc_core::MetadataIndex>,
}

struct SearchAnswer {
    seq: u64,
    rows: Result<Vec<bc_core::IndexedFile>, String>,
}

/// How many files a rule set matches, as answered by the shared worker. The
/// number is counted where the rows already are and only the count crosses the
/// channel: a rule over a large library answers with hundreds of thousands of
/// `IndexedFile`s, and shipping them to call `.len()` in the paint thread would
/// make the editor's live number a memory and CPU cost per keystroke.
struct CountAnswer {
    seq: u64,
    count: Result<usize, String>,
}

/// What the search worker is asked to do. One thread serves both the box and
/// the editor: they are the same SQLite reader, and two threads would compete
/// for the same connection while doing the same kind of scan.
enum SearchJob {
    Run {
        request: SearchQuery,
        reply: Sender<SearchAnswer>,
    },
    Count {
        /// The same request shape as `Run`: what differs is the answer, not the
        /// question.
        request: SearchQuery,
        reply: Sender<CountAnswer>,
    },
    Suggest {
        request: SuggestQuery,
        reply: Sender<SuggestAnswer>,
    },
}

/// The library search runs on its own thread, like the thumbnails.
///
/// A whole-library query is a table scan: measured at about 1.4 ms per 2 000
/// rows, which is ~70 ms per keystroke on a 100 000-row index. That belongs
/// off the paint thread, and answers carry the sequence number of the request
/// they came from so a reply to a word the user has already finished typing is
/// discarded instead of replacing the grid with an older answer.
fn spawn_search_worker(rx: Receiver<SearchJob>) {
    let handle = std::thread::Builder::new()
        .name("search".into())
        .spawn(move || {
            for job in rx {
                match job {
                    SearchJob::Run { request, reply } => {
                        let SearchQuery {
                            seq,
                            query,
                            scope,
                            index,
                        } = request;
                        let rows = index
                            .search(&query, &scope)
                            .map_err(|error| error.to_string());
                        if reply.send(SearchAnswer { seq, rows }).is_err() {
                            tracing::debug!("search answer undelivered (frame gone)");
                        }
                    }
                    SearchJob::Suggest { request, reply } => {
                        // One deliberate exception to "the worker answers and
                        // moves on": the first call builds an index over the
                        // keyword table, measured at 0.7 s on 600 000 rows. That
                        // is why this query belongs on this thread and not where
                        // the box is drawn, and why a slow first answer is not a
                        // defect to fix by moving it back to the paint path.
                        let SuggestQuery { seq, prefix, index } = request;
                        let results = index
                            .keyword_suggestions(&prefix, SUGGEST_LIMIT)
                            .map_err(|error| error.to_string());
                        if reply.send(SuggestAnswer { seq, results }).is_err() {
                            tracing::debug!("keyword suggestions undelivered (panel gone)");
                        }
                    }
                    SearchJob::Count { request, reply } => {
                        let SearchQuery {
                            seq,
                            query,
                            scope,
                            index,
                        } = request;
                        // `search` then `len`, not a counting query: the cost
                        // of a search here is reading the rows, which happens
                        // either way, so a `MetadataIndex::count` would be a
                        // second name for the same scan. What this does save is
                        // the answer crossing a channel as a Vec of clones.
                        let count = index
                            .search(&query, &scope)
                            .map(|rows| rows.len())
                            .map_err(|error| error.to_string());
                        if reply.send(CountAnswer { seq, count }).is_err() {
                            tracing::debug!("count answer undelivered (dialog closed)");
                        }
                    }
                }
            }
        });
    match handle {
        Ok(_) => {}
        Err(error) => {
            // No worker means no answers ever arrive, so the box has to say so
            // rather than sit on "searching…". The app notices through the
            // send error below and reports it as a search failure.
            tracing::warn!("search worker unavailable: {error}");
        }
    }
}

fn mtime_ns(modified: Option<std::time::SystemTime>) -> i64 {
    modified
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Dedicated worker thread that owns the SQLite cache (rusqlite's
/// `Connection` is `Send` but not `Sync`, so single-owner fits perfectly).
fn spawn_thumb_worker(
    cache_path: Option<PathBuf>,
    rx: Receiver<ThumbJob>,
    tx: Sender<ThumbResult>,
) {
    std::thread::Builder::new()
        .name("thumbs".into())
        .spawn(move || {
            let cache = cache_path.and_then(|p| match CacheStore::open(&p) {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!("thumbnail cache unavailable: {e}");
                    None
                }
            });
            for job in rx {
                let thumb = bc_thumbnails::load_or_generate(
                    if job.cached { cache.as_ref() } else { None },
                    &job.path,
                    job.mtime_ns,
                    job.size,
                    job.max_edge,
                );
                if let Err(e) = &thumb {
                    tracing::debug!("thumb decode failed for {}: {e}", job.path.display());
                }
                let max_edge = job.max_edge;
                if tx
                    .send(ThumbResult {
                        path: job.path,
                        max_edge,
                        thumb: thumb.ok(),
                    })
                    .is_err()
                {
                    break; // UI gone
                }
            }
        })
        .expect("failed to spawn thumbnail worker");
}

/// Label for a breadcrumb/tree node: file name, or an anchored icon for the
/// filesystem root ("/" or a drive).
fn node_label(path: &Path) -> String {
    match path.parent() {
        None => format!("⌂ {}", path.display()),
        Some(_) => path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
    }
}

pub struct BridgeApp {
    browser: BrowserState,
    path_input: String,
    status: String,
    copy_destination: String,
    move_destination: String,
    rename_name: String,
    pending_delete: Option<Vec<PathBuf>>,
    view_mode: ViewMode,
    list_focused: bool,

    thumb_tx: Sender<ThumbJob>,
    thumb_rx: Receiver<ThumbResult>,
    /// Requests already sent to the worker, awaiting a result.
    pending: HashSet<TexKey>,
    /// Decoded images uploaded to the GPU, keyed by path and long edge.
    textures: HashMap<TexKey, egui::TextureHandle>,
    /// Paths that failed to decode at a given tier; not retried until refresh.
    failed: HashSet<TexKey>,
    /// Pure state of the preview/zoom viewer (drawn over the content panel).
    viewer: ViewerState,

    /// Watcher for the open root; recreated on navigation.
    watcher: Option<Watcher>,
    /// When a watcher event was seen; refresh fires after the debounce elapses.
    refresh_due: Option<Instant>,
    /// Folder under the pointer last frame; OS file drops land here when set.
    hovered_folder: Option<PathBuf>,

    // ------------------------------------------------------------- shell --
    /// Persistent favorite folders; `None` when the store cannot be opened.
    favorites: Option<Favorites>,
    /// Load failure surfaced once so the sidebar can say why it is inert.
    favorites_error: Option<String>,
    sidebar_tab: SidebarTab,
    right_tab: RightTab,
    /// Folder-tree nodes the user has expanded (persisted across navigation).
    expanded: HashSet<PathBuf>,
    /// Lazy directory listings for the tree; `None` value = read failed.
    tree_children: HashMap<PathBuf, Option<Vec<PathBuf>>>,
    /// Cached recent-folder list (reloaded on navigation).
    recent_paths: Vec<PathBuf>,
    /// Grid cell width, driven by the status-bar zoom slider.
    cell_size: f32,
    /// Hide files nothing can thumbnail from the grid/list.
    thumbs_only: bool,
    /// Open-folder path field (toggled by the Open button / Ctrl+O).
    open_field: bool,
    trash_window: bool,
    /// Pattern preview/apply dialog for batch rename.
    rename_dialog: Option<batch_rename_ui::RenameDialog>,
    /// Watched-folder auto-action engine and quiet status/history view.
    auto_processor: Option<AutoProcessor>,
    auto_window: bool,
    auto_watch_root: String,
    auto_extensions: String,
    auto_pattern: String,
    auto_min_size: String,
    auto_settle_seconds: String,
    auto_action_thumbnail: bool,
    auto_action_rename: bool,
    auto_action_export: bool,
    auto_action_dated_move: bool,
    auto_started: Instant,
    /// Header-only dimensions, cached per path for the Metadata panel.
    dims_cache: HashMap<PathBuf, Option<(u32, u32)>>,
    /// EXIF fields cached per path, keyed with the mtime they were read at
    /// (including the sidecar's, when one exists).
    exif_cache: HashMap<PathBuf, (i64, bc_metadata::Metadata)>,
    /// Editable metadata form for the single selection.
    metadata_form: MetadataForm,
    metadata_form_path: Option<PathBuf>,
    metadata_form_mtime: i64,
    metadata_status: Option<String>,
    /// Rating, label, flag and keywords for the same selection as above.
    tags_form: metadata_edit::TagsForm,
    /// Text in the keyword add field.
    keyword_draft: String,
    /// Multi-selection bulk editor buffers.
    bulk_description: String,
    bulk_artist: String,
    bulk_copyright: String,
    bulk_report: Option<BulkReport>,
    /// Bulk tag stamping: each property is applied only when its own box is
    /// checked, so "set the copyright and leave the ratings alone" is one
    /// explicit statement rather than a guess from which fields are empty.
    bulk_tags: metadata_edit::TagStamp,
    /// Keyword typed into the bulk add field.
    bulk_keyword: String,
    /// Keyword typed into the bulk remove field.
    bulk_keyword_remove: String,

    /// The library search box. Separate from `browser.filter`, which narrows
    /// the listing already on screen: this asks the index for rows, which may
    /// come from anywhere in scope.
    search_text: String,
    search_scope: SearchScopeChoice,
    search_tx: Sender<SearchJob>,
    search_rx: Receiver<SearchAnswer>,
    /// The sender this app reads the matching receiver from; handed to each
    /// job so one worker thread can answer several consumers.
    search_reply_tx: Sender<SearchAnswer>,
    /// The collection rule editor, open when `Some`. Count requests go out on
    /// `search_tx` like every other query, so a stale answer cannot race a
    /// newer one across two threads reading one SQLite connection.
    editor: Option<CollectionEditor>,
    /// Library-wide keyword type-ahead for the box in the tags panel.
    /// `suggestions` describes `suggestions_prefix`, and the panel shows nothing
    /// unless the two still agree: a candidate list for a word the user has
    /// already changed is how a typo becomes a permanent keyword.
    suggest_reply_tx: Sender<SuggestAnswer>,
    suggest_rx: Receiver<SuggestAnswer>,
    suggest_seq: u64,
    /// The request in flight and the prefix it asks about.
    suggest_waiting: Option<(u64, String)>,
    suggestions: Vec<bc_core::KeywordSuggestion>,
    /// The prefix the panel drew last frame and wants an answer for. Set by the
    /// row helper rather than read off a particular box, because there are two
    /// keyword boxes — a single file's and a bulk stamp's — and one list.
    suggest_query: String,
    suggestions_prefix: String,
    suggest_error: Option<String>,
    suggest_due: Option<Instant>,
    /// Counters so an answer can be matched to the request it answers, and a
    /// superseded one thrown away.
    search_seq: u64,
    search_waiting: Option<u64>,
    /// When to fire the query after the last keystroke.
    search_due: Option<Instant>,
    /// Why the last search could not run, if it could not.
    search_error: Option<String>,
    /// The folder's own file count when a folder-scoped search was asked for,
    /// and how many of those the index has never read. Nothing else can answer
    /// that, and an empty result grid needs the difference stated.
    search_scope_total: usize,
    search_scope_unindexed: usize,
    /// Saved searches, as last read from the index. Refreshed when the tab is
    /// opened and after every save or delete, so the list cannot show a
    /// collection that no longer exists — or hide one that does.
    /// The user's saved searches, as read from their own file. Held as a
    /// `Vec` copy for drawing and owned by `saved_store` for writing, so the
    /// panel never reads the file per frame and never writes blind.
    collections: Vec<bc_core::NamedSearch>,
    /// The store itself, opened on first use. `None` with
    /// `saved_store_error` set is the state where the file exists but cannot
    /// be understood: the panel says so, and nothing overwrites it.
    saved_store: Option<bc_core::SavedSearches>,
    saved_store_error: Option<String>,
    /// Which tab the last drawn sidebar showed, so entering Collections is an
    /// event that can be detected instead of a query to repeat every frame.
    sidebar_tab_shown: SidebarTab,
    /// The name being typed in the Collections panel.
    collection_name: String,
    bulk_rating_enabled: bool,
    bulk_label_enabled: bool,
    bulk_flag_enabled: bool,

    // ------------------------------------------------------------- jobs --
    /// Session-lifetime worker pool: batch exports and metadata index passes
    /// alike (all the logic lives in `bc-core`).
    job_queue: JobQueue,
    /// Shared metadata index database, opened on the first folder that needs
    /// it. `None` with `index_error` set is a *reported* state, not a silent
    /// one: the status bar says the index is unavailable rather than letting a
    /// later search look like an empty library.
    index: Option<Arc<bc_core::MetadataIndex>>,
    index_error: Option<String>,
    /// The folder pass in the queue right now, so opening another folder can
    /// cancel it instead of leaving two writers on the same rows.
    index_job: Option<u64>,
    /// Files settled out of the running pass, for the status bar. Refreshed by
    /// the job poll rather than read in the paint pass, which would take the
    /// registry lock once per frame per widget.
    index_progress: Option<(usize, usize)>,
    /// Open Export dialog parameters, if any.
    export_dialog: Option<ExportDialog>,
    /// Jobs list window visibility.
    jobs_window: bool,
    /// Count of jobs still queued/running, refreshed by the poll.
    jobs_active: usize,
    /// Jobs whose completion summary has already been put in the status bar.
    jobs_notified: HashSet<u64>,
    /// Local WASM plugin registry; discovery is explicit from the manager dialog.
    plugin_manager: PluginManager,
    plugin_manager_error: Option<String>,
    plugin_manager_window: bool,
    plugin_panel_open: HashSet<String>,
    photocraft_bridge: photocraft::PhotoCraftBridge,
    photocraft_executable_input: String,
    photocraft_settings_window: bool,
    lightcraft_bridge: lightcraft::LightCraftBridge,
    lightcraft_executable_input: String,
    lightcraft_settings_window: bool,
}

impl BridgeApp {
    pub fn new(ctx: &egui::Context, root: Option<String>) -> Self {
        // The cache path is resolved here, from the one definition in
        // `bc_core::paths`, rather than handed in. A caller that derived it
        // from the working directory was the bug this closes: a bundled `.app`
        // or a read-only AppImage has no writable directory to put a
        // gigabyte-scale cache in. A failure to resolve the data directory
        // leaves the app running without a cache, which costs regeneration and
        // nothing else.
        let cache_path = match bc_core::paths::thumbnail_cache_path() {
            Ok(path) => Some(path),
            Err(error) => {
                tracing::warn!("thumbnail cache disabled: {error}");
                None
            }
        };
        let (job_tx, job_rx) = channel::<ThumbJob>();
        let (res_tx, res_rx) = channel::<ThumbResult>();
        spawn_thumb_worker(cache_path.clone(), job_rx, res_tx);
        // Requests go out on `query_tx`; answers come back on `answer_rx`.
        // The field holding the receiver is `search_rx` because that is what
        // the frame drains, but the two channels are deliberately not the same.
        let (query_tx, query_rx) = channel::<SearchJob>();
        let (answer_tx, answer_rx) = channel::<SearchAnswer>();
        let (suggest_reply_tx, suggest_rx) = channel::<SuggestAnswer>();
        spawn_search_worker(query_rx);

        let (favorites, favorites_error) = match Favorites::open_default() {
            Ok(store) => (Some(store), None),
            Err(e) => (None, Some(e.to_string())),
        };

        let mut app = Self {
            browser: BrowserState::default(),
            path_input: String::new(),
            status: "Ready.".into(),
            copy_destination: String::new(),
            move_destination: String::new(),
            rename_name: String::new(),
            pending_delete: None,
            view_mode: ViewMode::Thumbnails,
            list_focused: false,
            thumb_tx: job_tx,
            thumb_rx: res_rx,
            pending: HashSet::new(),
            textures: HashMap::new(),
            failed: HashSet::new(),
            viewer: ViewerState::default(),
            watcher: None,
            refresh_due: None,
            hovered_folder: None,
            favorites,
            favorites_error,
            sidebar_tab: SidebarTab::default(),
            right_tab: RightTab::default(),
            expanded: HashSet::new(),
            tree_children: HashMap::new(),
            recent_paths: Vec::new(),
            cell_size: DEFAULT_CELL,
            thumbs_only: false,
            open_field: false,
            trash_window: false,
            rename_dialog: None,
            auto_processor: None,
            auto_window: false,
            auto_watch_root: String::new(),
            auto_extensions: "jpg,dng,nef,arw".into(),
            auto_pattern: "{name}_{seq:0>3}".into(),
            auto_min_size: "0".into(),
            auto_settle_seconds: "2".into(),
            auto_action_thumbnail: true,
            auto_action_rename: false,
            auto_action_export: false,
            auto_action_dated_move: false,
            auto_started: Instant::now(),
            dims_cache: HashMap::new(),
            exif_cache: HashMap::new(),
            metadata_form: MetadataForm::default(),
            metadata_form_path: None,
            metadata_form_mtime: 0,
            metadata_status: None,
            tags_form: metadata_edit::TagsForm::default(),
            keyword_draft: String::new(),
            bulk_description: String::new(),
            bulk_artist: String::new(),
            bulk_copyright: String::new(),
            bulk_report: None,
            bulk_tags: metadata_edit::TagStamp::default(),
            bulk_keyword: String::new(),
            bulk_keyword_remove: String::new(),
            search_text: String::new(),
            search_scope: SearchScopeChoice::default(),
            search_tx: query_tx.clone(),
            search_rx: answer_rx,
            search_reply_tx: answer_tx,
            editor: None,
            suggest_reply_tx,
            suggest_rx,
            suggest_seq: 0,
            suggest_waiting: None,
            suggestions: Vec::new(),
            suggest_query: String::new(),
            suggestions_prefix: String::new(),
            suggest_error: None,
            suggest_due: None,
            search_seq: 0,
            search_waiting: None,
            search_due: None,
            search_error: None,
            search_scope_total: 0,
            search_scope_unindexed: 0,
            collections: Vec::new(),
            saved_store: None,
            saved_store_error: None,
            sidebar_tab_shown: SidebarTab::default(),
            collection_name: String::new(),
            bulk_rating_enabled: false,
            bulk_label_enabled: false,
            bulk_flag_enabled: false,
            job_queue: JobQueue::new(bc_core::export::default_worker_count()),
            index: None,
            index_error: None,
            index_job: None,
            index_progress: None,
            export_dialog: None,
            jobs_window: false,
            jobs_active: 0,
            jobs_notified: HashSet::new(),
            plugin_manager: PluginManager::open_default().unwrap_or_else(|error| {
                tracing::warn!("plugin manager unavailable: {error}");
                PluginManager::new()
            }),
            plugin_manager_error: None,
            plugin_manager_window: false,
            plugin_panel_open: HashSet::new(),
            photocraft_bridge: photocraft::PhotoCraftBridge::new(),
            photocraft_executable_input: String::new(),
            photocraft_settings_window: false,
            lightcraft_bridge: lightcraft::LightCraftBridge::new(),
            lightcraft_executable_input: String::new(),
            lightcraft_settings_window: false,
        };

        match AutoProcessor::open_default(cache_path.clone()) {
            Ok(processor) => {
                app.auto_processor = Some(processor);
            }
            Err(error) => tracing::warn!("auto-processing unavailable: {error}"),
        }

        if let Some(root) = root {
            app.open_root(Path::new(&root));
        } else if let Ok(cwd) = std::env::current_dir() {
            app.open_root(&cwd);
        }

        ctx.set_visuals(egui::Visuals::dark());
        app
    }

    fn open_root(&mut self, root: &Path) {
        match self.browser.open(root) {
            Ok(()) => {
                self.path_input = root.display().to_string();
                self.status = format!("Opened {} ({} items)", root.display(), self.browser.len());
                match Recents::open_default() {
                    Ok(mut recents) => match recents.record_access(root) {
                        Ok(()) => {
                            self.recent_paths = recents.paths().iter().take(16).cloned().collect();
                        }
                        Err(e) => {
                            self.status =
                                format!("Opened {}; recents unavailable: {e}", root.display())
                        }
                    },
                    Err(e) => {
                        self.status = format!("Opened {}; recents unavailable: {e}", root.display())
                    }
                }
                self.pending.clear();
                self.failed.clear();
                // Folder changes end the preview: the file list it navigated is gone.
                self.viewer.close();
                // Drop thumbnails unrelated to the new folder to bound memory use.
                self.textures.retain(|(path, _), _| path.starts_with(root));
                // Keep the tree anchored on the folder the user is in.
                self.expanded.insert(root.to_path_buf());
                self.attach_watcher(root);
                // Navigation ends a search, so the box has to empty with it:
                // a field showing last folder's query above this folder's
                // listing would look like a filter that silently did nothing.
                self.forget_search();
                // Values already in the index are available before the pass
                // starts, so a folder visited once sorts by rating immediately
                // rather than after this visit's pass has finished.
                self.load_folder_tags(root);
                self.start_indexing(root);
            }
            Err(e) => {
                self.status = format!("Error: {e}");
            }
        }
    }

    /// (Re)create the directory watcher for the current root.
    fn attach_watcher(&mut self, root: &Path) {
        self.watcher = match Watcher::new(root) {
            Ok(w) => {
                tracing::debug!("watching {}", root.display());
                Some(w)
            }
            Err(e) => {
                tracing::warn!("could not watch {}: {e}", root.display());
                self.refresh_due = None;
                None
            }
        };
    }

    /// Open the metadata index once and keep the handle.
    ///
    /// Shared rather than opened per pass because the queue hands the same
    /// `Arc` to every worker: one SQLite connection means one writer, which is
    /// what a store whose rows are keyed by path wants. A failure is remembered
    /// in `index_error` and reported, not retried every frame — a missing data
    /// directory does not become a read-only disk by clicking twice.
    fn open_index(&mut self) -> Option<Arc<bc_core::MetadataIndex>> {
        if self.index.is_some() {
            return self.index.clone();
        }
        let path = match bc_core::paths::metadata_index_path() {
            Ok(path) => path,
            Err(error) => {
                self.index_error = Some(error.to_string());
                return None;
            }
        };
        match bc_core::MetadataIndex::open(&path) {
            Ok(index) => {
                self.index_error = None;
                self.index = Some(Arc::new(index));
                self.index.clone()
            }
            Err(error) => {
                self.index_error = Some(format!("{}: {error}", path.display()));
                None
            }
        }
    }

    /// Queue a metadata-index pass over the folder just opened.
    ///
    /// Opening a folder while an earlier pass runs cancels that pass first: the
    /// rows it would still write belong to a folder nobody is looking at, and
    /// two passes on the same connection would fight over the same rows. The
    /// cancelled pass's sweep still runs, so nothing already true on disk gets
    /// left behind.
    fn start_indexing(&mut self, root: &Path) {
        let files: Vec<PathBuf> = self
            .browser
            .entries
            .iter()
            .filter(|entry| !entry.is_dir)
            .map(|entry| entry.path.clone())
            .collect();
        let total = files.len();
        let Some(index) = self.open_index() else {
            // Reachable state, and the user has to be told: without a database
            // there is no search and no index-backed sorting, so saying nothing
            // would make an empty result look like an empty library.
            self.status = format!(
                "Search index unavailable ({}); search and sorting by tag will be incomplete",
                self.index_error.as_deref().unwrap_or("no reason recorded")
            );
            tracing::warn!("{}", self.status);
            return;
        };
        if let Some(previous) = self.index_job.take() {
            self.job_queue.cancel(previous);
        }
        let job = self.job_queue.submit_index(root, files, index);
        self.index_job = Some(job.id);
        tracing::debug!("indexing {} ({total} files)", root.display());
    }

    /// Read the folder's tag values out of the metadata index and hand them to
    /// the browser, so a rating sort or tag filter has something to read.
    ///
    /// This is a single indexed range query, not a query per file: measured on
    /// a 2 000-file folder it costs 2.57 ms against 2.97 ms to parse those
    /// files' metadata layers once each, and against 19.6 ms to ask the index
    /// about them one row at a time. On the UI thread, therefore, at a cost
    /// comparable to the thumbnail work already done when a folder opens.
    fn load_folder_tags(&mut self, root: &Path) {
        // `open_index`, not `self.index.clone()`: this runs before the pass is
        // queued, and reading a field that only the other call sets would make
        // the very first folder open of a session sort as though the library
        // were empty even when every row was already there.
        let Some(index) = self.open_index() else {
            return;
        };
        match index.tag_values_for(root) {
            Ok(values) => self.browser.set_tag_values(values),
            Err(error) => {
                tracing::warn!("could not read the index for {}: {error}", root.display());
                self.browser
                    .set_tag_values(bc_core::fs::TagValues::default());
            }
        }
    }

    /// Write one saved file back into the index and update the values the grid
    /// sorts on, so starring a photo in a folder sorted by rating moves it
    /// now. Waiting for the watcher's next pass would leave the row where it
    /// was, which reads as the save having been ignored.
    fn index_saved_tags(&mut self, path: &Path) {
        let Some(index) = self.index.clone() else {
            return;
        };
        // `reindex`, not `index_one`: this process has just written the file,
        // so asking the index whether the row still matches it can answer
        // "yes" to a file whose rating just changed. On this filesystem that
        // tie is common, not exotic, and the row would then stay wrong forever.
        if let bc_core::indexer::IndexOutcome::Failed(message) =
            bc_core::indexer::reindex(&index, path)
        {
            // The file itself is saved; only the index row is behind, and the
            // next pass will pick it up. Say so rather than stay silent,
            // because the sort order is what will look wrong.
            tracing::warn!("indexing {} after a save: {message}", path.display());
            self.status = format!(
                "Saved, but the search index could not be updated ({message}); \
                 a tag sort may be out of date"
            );
            return;
        }
        match index.get(path) {
            Ok(Some(row)) => self.browser.update_tag(
                path.to_path_buf(),
                bc_core::fs::TagValue {
                    rating: row.rating,
                    label: row.color_label,
                    flag: row.flag,
                },
            ),
            Ok(None) => self.browser.tags.remove(path),
            Err(error) => tracing::warn!("reading back the row for {}: {error}", path.display()),
        }
    }

    /// Ask the search worker for the current box contents.
    ///
    /// Every call supersedes the one before it by sequence number rather than
    /// by cancelling: the worker is single-threaded and one query is a table
    /// scan, so the useful thing to drop is the *answer* to a question nobody
    /// asked any more, not the work already underway.
    fn request_search(&mut self) {
        let text = self.search_text.trim().to_string();
        if text.is_empty() {
            self.clear_search();
            return;
        }
        let Some(index) = self.open_index() else {
            // Reachable, and it needs saying here rather than leaving a grid
            // that quietly shows nothing.
            self.search_error = self.index_error.clone();
            self.status = format!(
                "Search unavailable: {}",
                self.index_error.as_deref().unwrap_or("no reason recorded")
            );
            return;
        };
        let scope_choice = self.search_scope;
        let scope = scope_choice.scope(self.browser.root.as_deref());
        let (total, unindexed) = self.scope_coverage(&scope);
        self.search_scope_total = total;
        self.search_scope_unindexed = unindexed;

        self.search_seq += 1;
        let seq = self.search_seq;
        self.browser.enter_search(&text, scope.clone());
        self.search_waiting = Some(seq);
        self.search_error = None;
        self.status = format!("Searching for “{text}”…");
        let job = SearchQuery {
            seq,
            query: bc_core::Query::text(&text),
            scope,
            index,
        };
        if self
            .search_tx
            .send(SearchJob::Run {
                request: job,
                reply: self.search_reply_tx.clone(),
            })
            .is_err()
        {
            self.search_waiting = None;
            self.search_error = Some("the search worker is not running".to_string());
            self.status = "Search unavailable: the background search thread is gone".to_string();
        }
    }

    /// Take answers that have arrived. Only the newest request is answered to;
    /// anything older is dropped on the floor.
    fn poll_search(&mut self, ctx: &egui::Context) {
        let mut latest: Option<SearchAnswer> = None;
        while let Ok(answer) = self.search_rx.try_recv() {
            if Some(answer.seq) == self.search_waiting {
                latest = Some(answer);
            }
        }
        let Some(answer) = latest else {
            if self.search_waiting.is_some() {
                // Keep the frame clock running so the answer is taken as soon
                // as it lands, without repainting at 60 Hz while idle.
                ctx.request_repaint_after(Duration::from_millis(60));
            }
            return;
        };
        self.search_waiting = None;
        match answer.rows {
            Ok(rows) => {
                let count = rows.len();
                self.browser.set_search_results(rows);
                self.status = self.search_status(count);
            }
            Err(error) => {
                self.search_error = Some(error.clone());
                self.status = format!("Search failed: {error}");
                // The grid is empty because the query errored, not because the
                // library is empty: leave the search state rather than showing
                // the folder as if nothing had been asked.
            }
        }
    }

    /// Saved searches: list, run, delete, and save what is on screen.
    fn collections_panel(&mut self, ui: &mut egui::Ui) {
        // Nothing here needs the metadata index any more. The definitions are
        // their own file precisely because the index is a cache, so an offline
        // index costs *running* a search and not keeping one; the list and the
        // save control stay available and say so where it matters.
        let store_unreadable = self.saved_store_error.is_some();

        // Save what the grid is currently showing.
        let showing = self.browser.search().is_some();
        ui.group(|ui| {
            ui.add(
                egui::Label::new(egui::RichText::new("Save this search").weak().small())
                    .selectable(false),
            );
            let name_field = ui.add_sized(
                [ui.available_width().max(80.0), 20.0],
                egui::TextEdit::singleline(&mut self.collection_name).hint_text("name…"),
            );
            let typed = self.collection_name.trim();
            let replaces = self
                .collections
                .iter()
                .any(|c| c.name.eq_ignore_ascii_case(typed) && !typed.is_empty());
            let button_label = if replaces {
                format!("Replace “{typed}”")
            } else {
                "Save".to_string()
            };
            // A disabled control still has to say why, or the only hint that a
            // collection cannot be saved right now is a greyed-out button.
            let hover = if replaces {
                "This overwrites the existing collection's rules rather than adding a second \
                 one with the same name."
            } else if !showing {
                "Run a search or open a collection first — there is nothing to save yet."
            } else {
                "Store these rules and this scope under the name above."
            };
            let entered = name_field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            // Disabled rather than silently refused: pressing "Save" into an
            // unreadable file must never look like it worked.
            let can_save = showing && !typed.is_empty() && !store_unreadable;
            let clicked = ui
                .add_enabled_ui(can_save, |ui| {
                    ui.button(&button_label)
                        .on_hover_text(if store_unreadable {
                            "The saved-search file cannot be read, so nothing will be written \
                             over it."
                        } else {
                            hover
                        })
                        .clicked()
                })
                .inner;
            if clicked || (entered && can_save) {
                self.save_current_collection();
            }
            if !showing {
                ui.weak("Nothing to save: the grid is showing a folder.");
            }
        });

        // The box above can only ever save what the search box produced, which is
        // words and a scope. This is the other half of the feature: conditions on
        // the fields a sentence cannot ask for, and the way back into a collection
        // that is already saved.
        ui.add_enabled_ui(!store_unreadable, |ui| {
            if ui
                .button("Edit rules…")
                .on_hover_text(
                    "Build a collection from star rating, keyword, colour label, flag, camera, \
                     name and the other indexed fields, and see how many files match while you \
                     write it.",
                )
                .clicked()
            {
                self.open_collection_editor(String::new(), None, None);
            }
        });
        ui.separator();

        if let Some(error) = &self.saved_store_error {
            // The wording a user can act on: the file is named, and the reason
            // is the parser's own, not a shrug.
            ui.colored_label(
                egui::Color32::from_rgb(220, 120, 120),
                format!("Saved searches unavailable: {error}"),
            );
            return;
        }
        if self.collections.is_empty() {
            ui.weak("No saved searches yet. Run one, then name it above.");
            return;
        }

        let entries = self.collections.clone();
        egui::ScrollArea::vertical().show(ui, |ui| {
            for entry in entries {
                let running = self.browser.collection() == Some(entry.name.as_str());
                let name = entry.name.clone();
                ui.horizontal(|ui| {
                    let hover = if running {
                        "This is the collection on screen.".to_string()
                    } else {
                        format!("Run: {}", entry.search.summary())
                    };
                    if ui
                        .selectable_label(running, &entry.name)
                        .on_hover_text(hover)
                        .clicked()
                    {
                        self.run_collection(&name);
                    }
                    if ui
                        .small_button("✎")
                        .on_hover_text(
                            "Edit these rules. The collection stays as it is until \
                         the editor saves.",
                        )
                        .clicked()
                    {
                        let search = entry.search.clone();
                        // `name` is cloned twice rather than moved: the delete
                        // button below asks for the same one.
                        self.open_collection_editor(
                            name.clone(),
                            Some(name.clone()),
                            Some(&search),
                        );
                    }
                    if ui
                        .small_button("✕")
                        .on_hover_text("Delete this saved search. No files are touched.")
                        .clicked()
                    {
                        self.delete_collection(&name);
                    }
                });
                ui.add(
                    egui::Label::new(egui::RichText::new(entry.search.summary()).weak().small())
                        .selectable(false)
                        .truncate(true),
                );
            }
        });
    }

    /// Open the saved-search store, or explain why it cannot be opened.
    ///
    /// A file that will not parse is left alone and reported. That is the whole
    /// reason these moved out of the metadata index: the index is a cache that
    /// may be rebuilt at any time, while this file holds work nothing else has.
    /// Overwriting an unreadable version of it on the next unrelated save would
    /// turn one bad byte into every collection the user ever named.
    fn saved_store(&mut self) -> Option<&mut bc_core::SavedSearches> {
        if self.saved_store.is_some() {
            return self.saved_store.as_mut();
        }
        match bc_core::SavedSearches::open_default() {
            Ok(store) => {
                self.saved_store_error = None;
                self.saved_store = Some(store);
                self.saved_store.as_mut()
            }
            Err(error) => {
                self.saved_store_error = Some(error.to_string());
                self.saved_store = None;
                None
            }
        }
    }

    /// Re-read the saved searches. Failures are shown, never swallowed: a panel
    /// that silently listed nothing would look like the user's collections had
    /// gone.
    fn refresh_collections(&mut self) {
        let Some(store) = self.saved_store() else {
            self.collections.clear();
            return;
        };
        self.collections = store.list().to_vec();
    }

    /// Store the rules and scope the grid is showing under a name.
    fn save_current_collection(&mut self) {
        // Taken as owned values up front: the rest of this function mutates
        // the browser, and `search()` is a view into it.
        // Only the rules and the scope are needed; the box text stays where it
        // is, because saving does not re-run anything.
        let Some((query, scope)) = self
            .browser
            .search()
            .map(|(_, query, scope)| (query.clone(), scope.clone()))
        else {
            self.status = "Nothing to save: the grid is showing a folder.".to_string();
            return;
        };
        let name = self.collection_name.trim().to_string();
        if name.is_empty() {
            self.status = "A collection needs a name.".to_string();
            return;
        }
        let search = bc_core::SavedSearch { query, scope };
        // The store borrow ends here: everything below writes through `self`,
        // and a held `&mut` store would make that a borrow conflict on the
        // happy path.
        let outcome = match self.saved_store() {
            Some(store) => store
                .save(&name, &search)
                .map_err(|error| error.to_string()),
            None => Err(self
                .saved_store_error
                .clone()
                .unwrap_or_else(|| "the saved-search file could not be opened".to_string())),
        };
        match outcome {
            Ok(replaced) => {
                self.refresh_collections();
                self.collection_name.clear();
                // Nothing is re-run and no rows are dropped: the grid already
                // shows this collection's answer, so the listing is only
                // labelled. A second save then offers to replace it, and the
                // status line can name what the user is looking at.
                self.browser.name_collection(&name);
                // Naming a replacement matters: saving over "trip" quietly is
                // how a user loses the rules they meant to keep.
                let verb = if replaced { "Replaced" } else { "Saved" };
                self.status = format!("{verb} collection “{name}” ({})", search.summary());
            }
            Err(reason) => self.status = format!("Could not save “{name}”: {reason}"),
        }
    }

    /// Run a saved collection through the same worker a typed search uses.
    fn run_collection(&mut self, name: &str) {
        // The definition comes from the list the user clicked, not from the
        // store again: if the two ever disagreed, running what is on screen is
        // the behaviour the click promises.
        let Some(entry) = self.collections.iter().find(|c| c.name == name) else {
            self.status = format!("“{name}” is no longer in the list.");
            self.refresh_collections();
            return;
        };
        let search = entry.search.clone();
        let Some(index) = self.index.clone() else {
            // Searching needs the index; the saved definition does not live
            // there any more, so this is a different failure from the store
            // being unreadable and gets its own words.
            self.status = format!(
                "Cannot run “{name}”: the metadata index is offline. {}",
                self.index_error.as_deref().unwrap_or("")
            );
            return;
        };
        // The box mirrors the collection's words when it can, so what the user
        // reads in the field matches what produced the grid.
        self.search_text = search.box_text();
        // Coverage is re-asked rather than left over from the last typed
        // search: it is a fact about the scope being run, not about whatever
        // the box did last.
        let (total, unindexed) = self.scope_coverage(&search.scope);
        self.search_scope_total = total;
        self.search_scope_unindexed = unindexed;
        self.search_seq += 1;
        let seq = self.search_seq;
        self.browser.enter_collection(name, &search);
        self.search_waiting = Some(seq);
        self.search_error = None;
        self.status = format!("Running collection “{name}”…");
        let job = SearchQuery {
            seq,
            query: search.query.clone(),
            scope: search.scope.clone(),
            index,
        };
        if self
            .search_tx
            .send(SearchJob::Run {
                request: job,
                reply: self.search_reply_tx.clone(),
            })
            .is_err()
        {
            self.search_waiting = None;
            self.status = "Search unavailable: the background search thread is gone".to_string();
        }
    }

    /// Open the rule editor. A saved collection arrives with its own rules and
    /// its own scope; a new one starts where the user is looking, resolved now
    /// rather than kept as a promise about "this folder" that a navigation could
    /// quietly change.
    fn open_collection_editor(
        &mut self,
        name: String,
        editing: Option<String>,
        search: Option<&bc_core::SavedSearch>,
    ) {
        let scope = match search {
            Some(saved) => saved.scope.clone(),
            None => self.search_scope.scope(self.browser.root.as_deref()),
        };
        self.editor = Some(CollectionEditor::new(name, editing, search, scope));
    }

    /// The editor's share of the search worker: take any count that has landed,
    /// and ask for one when the boxes have stopped changing.
    fn tick_editor_count(&mut self, ctx: &egui::Context) {
        let Some(mut editor) = self.editor.take() else {
            return;
        };
        // Drained before anything else, every frame: an answer that has landed is
        // never left sitting in the channel, so `outstanding` cannot outlive its
        // own reply and leave the dialog saying "counting…" forever.
        editor.poll_count();
        let key = editor.count_key();

        // Rules that cannot match have no number worth scanning for, and showing
        // the zero they would return is how an impossible collection comes to
        // look like an empty library. Marking them counted is what stops the
        // dialog re-asking every frame; each row says its own piece underneath.
        if !editor.problems().is_empty() {
            editor.counted = Some(key);
            editor.count = None;
            editor.count_error = None;
            editor.outstanding = None;
            editor.count_due = None;
            self.editor = Some(editor);
            return;
        }

        if editor.count_is_current(&key) {
            self.editor = Some(editor);
            return;
        }
        if let Some(due) = editor.count_due {
            if Instant::now() >= due {
                editor.count_due = None;
                self.request_count(&mut editor, key);
            } else {
                ctx.request_repaint_after(due.saturating_duration_since(Instant::now()));
            }
        } else {
            editor.schedule_count(&key);
            if editor.count_due.is_some() {
                ctx.request_repaint_after(SEARCH_DEBOUNCE);
            }
        }
        self.editor = Some(editor);
    }

    /// The frame's half of keyword suggestions: take any answer that landed, and
    /// fire the deadline the completion row set. Scheduling is the row's job — it
    /// is the only thing that knows which of the two keyword boxes is on screen
    /// and what is typed in it — so this is a clock and nothing else.
    fn tick_suggestions(&mut self, ctx: &egui::Context) {
        self.poll_suggestions();
        let Some(due) = self.suggest_due else {
            if self.suggest_waiting.is_some() {
                // Outstanding: keep the frame clock awake so the answer is taken
                // as soon as it lands, without repainting at 60 Hz while idle.
                ctx.request_repaint_after(Duration::from_millis(60));
            }
            return;
        };
        if Instant::now() < due {
            ctx.request_repaint_after(due.saturating_duration_since(Instant::now()));
            return;
        }
        self.suggest_due = None;
        let typed = self.typed_prefix();
        if !typed.is_empty() && self.suggestions_prefix != typed {
            self.request_suggestions(typed);
        }
    }

    /// Queue a suggestion query for what a keyword box currently holds. Called
    /// from the row on every frame it draws, so the cost of being called again
    /// with the same text is one comparison.
    ///
    /// The deadline is what keeps a fast typist from queueing a scan per
    /// keystroke, and asking for whatever is typed *when the deadline fires* —
    /// rather than what was typed when it was set — is what keeps an abandoned
    /// word from being asked about at all.
    fn schedule_suggestions(&mut self, ui: &egui::Ui, typed: &str) {
        // Nothing typed is not a slow query, it is no question at all: the list
        // goes, and an answer already travelling is dropped by the receiver
        // nobody is waiting for.
        if typed.is_empty() {
            self.suggestions.clear();
            self.suggestions_prefix.clear();
            self.suggest_error = None;
            self.suggest_due = None;
            self.suggest_waiting = None;
            return;
        }
        if self.suggestions_prefix == typed || self.suggest_due.is_some() {
            // Answered, or already on its way.
            return;
        }
        self.suggest_due = Some(Instant::now() + SUGGEST_DEBOUNCE);
        ui.ctx().request_repaint_after(SUGGEST_DEBOUNCE);
    }

    /// The prefix the last drawn keyword row was asking about, already in the
    /// form the index stores keywords in. Normalised at the row rather than
    /// here, because the row is the only place that knows which box is on
    /// screen: two spaces typed by hand are the same word to `add_keyword`, and
    /// asking the library with the un-normalised text would answer "new
    /// keyword" about a word the library already has.
    fn typed_prefix(&self) -> String {
        self.suggest_query.clone()
    }

    fn poll_suggestions(&mut self) {
        let Some((seq, asked)) = self.suggest_waiting.clone() else {
            while self.suggest_rx.try_recv().is_ok() {}
            return;
        };
        while let Ok(answer) = self.suggest_rx.try_recv() {
            if answer.seq != seq {
                // A reply to a word that is no longer outstanding. The list it
                // carries belongs to a prefix nobody is asking about.
                continue;
            }
            self.suggest_waiting = None;
            self.suggestions_prefix = asked;
            self.suggest_due = None;
            match answer.results {
                Ok(list) => {
                    self.suggestions = list;
                    self.suggest_error = None;
                }
                Err(error) => {
                    self.suggestions = Vec::new();
                    self.suggest_error = Some(error);
                }
            }
            return;
        }
    }

    /// The keyword completion row: what the library already calls things that
    /// begin with what is typed, plus one line saying which kind of absence the
    /// box is looking at. Shared by both keyword boxes, because the question is
    /// the same and the bulk one is where a wrong keyword multiplies by the
    /// size of the selection.
    ///
    /// `carried` names keywords the subject already holds, which are not offered
    /// back as additions. The bulk panel passes nothing: "already carried" means
    /// something different for every file in a selection, and the stamp path
    /// refuses the duplicates it meets.
    fn keyword_completions(
        &mut self,
        ui: &mut egui::Ui,
        draft: String,
        carried: &[String],
    ) -> Option<String> {
        let typed = bc_metadata::keywords::normalize_prefix(&draft);
        self.suggest_query = typed.clone();
        self.schedule_suggestions(ui, &typed);
        let answered = !typed.is_empty() && self.suggestions_prefix == typed;

        // Only a list answered *for this prefix* may be shown: `suggestions`
        // holds the last one that landed, which after an edit is a list for a
        // word nobody is typing.
        let candidates: Vec<(String, usize)> = if answered {
            self.suggestions
                .iter()
                .filter(|s| !carried.contains(&s.keyword))
                .map(|s| (s.keyword.clone(), s.files))
                .collect()
        } else {
            Vec::new()
        };

        let mut chosen = None;
        if !candidates.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new("In use:").weak().small())
                        .selectable(false),
                );
                for (keyword, files) in &candidates {
                    // The count is the point of the row: a word used by 5 000
                    // files is the library's vocabulary, and a word used by one
                    // is worth a look before it is copied.
                    let label = format!("{keyword} ({files})");
                    if ui
                        .small_button(egui::RichText::new(label).small())
                        .on_hover_text(format!(
                            "Used by {files} indexed file(s). Clicking adds this spelling, \
                             exactly as those files spell it."
                        ))
                        .clicked()
                    {
                        chosen = Some(keyword.clone());
                    }
                }
            });
        }

        // One status line, distinguishing the three absences a keyword box can
        // mean: not asked yet, cannot ask, and asked with nothing to show.
        // Collapsing them is how "no suggestions" comes to read as "this word is
        // new" when the index was simply offline.
        if !typed.is_empty() {
            let line =
                if let Some(error) = answered.then_some(self.suggest_error.as_ref()).flatten() {
                    Some(format!("Library suggestions unavailable: {error}"))
                } else if !answered {
                    Some("Checking the library’s keywords…".to_string())
                } else if !candidates.is_empty() {
                    // The row above already said what is true of this word: the
                    // library uses it. Nothing to add, and anything added here
                    // would contradict the buttons a line above.
                    None
                } else if !self.suggestions.is_empty() {
                    // Every spelling the library holds is already on this file.
                    Some("Already on this file.".to_string())
                } else if typed.ends_with('|') {
                    // A trailing separator is not a keyword: it is the question
                    // "what hangs under this", and answering it with "this would
                    // be a new keyword" would offer to create a word nobody
                    // typed. Say what is empty instead.
                    let parent = typed.trim_end_matches('|');
                    Some(format!("Nothing is filed under “{parent}” yet."))
                } else {
                    Some(format!(
                        "“{typed}” is not used by any indexed file — this would be a new \
                         keyword."
                    ))
                };
            if let Some(line) = line {
                ui.add(
                    egui::Label::new(egui::RichText::new(line).weak().small()).selectable(false),
                );
            }
        }
        chosen
    }

    /// Ask the worker what the library calls things beginning with `prefix`.
    fn request_suggestions(&mut self, prefix: String) {
        let Some(index) = self.open_index() else {
            // Say so rather than showing nothing: an empty list and an offline
            // index look identical in a keyword box, and one of them means the
            // word the user is typing may already exist somewhere.
            self.suggestions_prefix = prefix;
            self.suggestions = Vec::new();
            self.suggest_error = Some(
                self.index_error
                    .clone()
                    .unwrap_or_else(|| "the metadata index is offline".to_string()),
            );
            return;
        };
        self.suggest_seq += 1;
        let seq = self.suggest_seq;
        let request = SuggestQuery {
            seq,
            prefix: prefix.clone(),
            index,
        };
        let reply = self.suggest_reply_tx.clone();
        if self
            .search_tx
            .send(SearchJob::Suggest { request, reply })
            .is_err()
        {
            self.suggestions_prefix = prefix;
            self.suggestions = Vec::new();
            self.suggest_error = Some("the background search thread is not running".to_string());
        } else {
            self.suggest_waiting = Some((seq, prefix));
        }
    }

    fn request_count(&mut self, editor: &mut CollectionEditor, key: CountKey) {
        let Some(index) = self.open_index() else {
            // The only part of this dialog that needs the index. The rules are
            // still savable, so the message has to be about the count and not
            // about the collection — the two failures read differently, and the
            // old build used one sentence for both.
            editor.counted = Some(key);
            editor.count = None;
            editor.count_error = Some(
                self.index_error
                    .clone()
                    .unwrap_or_else(|| "the metadata index is offline".to_string()),
            );
            return;
        };
        editor.count_seq += 1;
        let seq = editor.count_seq;
        let request = SearchQuery {
            seq,
            query: editor.query(),
            scope: key.scope.clone(),
            index,
        };
        let reply = editor.count_tx.clone();
        if self
            .search_tx
            .send(SearchJob::Count { request, reply })
            .is_err()
        {
            editor.counted = Some(key);
            editor.count = None;
            editor.count_error = Some("the background search thread is not running".to_string());
        } else {
            editor.outstanding = Some((seq, key));
        }
    }

    /// Write a draft to the store. Answers whether it landed, because the dialog
    /// must stay open when it did not: the rules the user just typed exist
    /// nowhere else, and closing the window would discard them.
    fn save_collection_draft(
        &mut self,
        name: &str,
        search: &bc_core::SavedSearch,
        editing: Option<&str>,
    ) -> bool {
        let outcome = match self.saved_store() {
            Some(store) => store.save(name, search).map_err(|error| error.to_string()),
            None => Err(self
                .saved_store_error
                .clone()
                .unwrap_or_else(|| "the saved-search file could not be opened".to_string())),
        };
        match outcome {
            Ok(replaced) => {
                self.refresh_collections();
                let verb = if replaced { "Replaced" } else { "Saved" };
                let note = match editing {
                    // Saving under a different name creates a second collection;
                    // the original is still in the list. Saying nothing would let
                    // this read as a rename that lost the old one.
                    Some(old) if old != name => {
                        format!(" “{name}”; “{old}” is still saved under its own name.")
                    }
                    _ => format!(" ({})", search.summary()),
                };
                self.status = format!("{verb} collection{note}");
                true
            }
            Err(reason) => {
                self.status = format!("Could not save “{name}”: {reason}");
                false
            }
        }
    }

    /// The rule editor. Its live number comes from the worker thread: a rule over
    /// a large library is a table scan, and a scan in the paint path is a window
    /// that stops answering while someone types.
    fn editor_window(&mut self, ctx: &egui::Context) {
        let Some(mut editor) = self.editor.take() else {
            return;
        };
        let root = self.browser.root.clone();
        let store_unreadable = self.saved_store_error.is_some();
        let saved_names: Vec<String> = self
            .collections
            .iter()
            .map(|entry| entry.name.clone())
            .collect();
        // `open` is borrowed by the window's own close box, so the dialog's
        // buttons set a separate flag instead of writing it — the same split the
        // export window uses, and the reason it has one.
        let mut open = true;
        let mut save = false;
        let mut dismiss = false;
        let title = if editor.editing.is_some() {
            "Edit collection"
        } else {
            "New collection"
        };

        egui::Window::new(title)
            .id(egui::Id::new("collection_editor"))
            .collapsible(false)
            .resizable(true)
            .default_width(560.0)
            .open(&mut open)
            .show(ctx, |ui| {
                let problems = editor.problems();
                let key = editor.count_key();

                ui.horizontal(|ui| {
                    ui.label("Name");
                    ui.add_sized(
                        [260.0, 20.0],
                        egui::TextEdit::singleline(&mut editor.name).hint_text("collection name"),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("Words");
                    ui.add_sized(
                        [260.0, 20.0],
                        egui::TextEdit::singleline(&mut editor.words).hint_text("beach sunset"),
                    );
                    // Said as what the engine does, not as "the fields below":
                    // words are matched against the whole projected text of a
                    // row, which is wider than the list under this box, and a
                    // label that underscopes its own rule teaches the user the
                    // wrong thing about how their collection works.
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new("each word, anywhere in the file's indexed text")
                                .weak()
                                .small(),
                        )
                        .selectable(false),
                    );
                });

                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Match");
                    ui.selectable_value(
                        &mut editor.combinator,
                        bc_core::Combinator::And,
                        "every",
                    );
                    ui.label("of");
                    ui.selectable_value(
                        &mut editor.combinator,
                        bc_core::Combinator::Or,
                        "any",
                    );
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new("— words count as conditions too").weak().small(),
                        )
                        .selectable(false),
                    );
                });

                let mut remove: Option<usize> = None;
                // Both halves have to be empty for that to be true: with words
                // typed and no conditions, the collection matches files holding
                // those words and saying "every file" would be a lie about the
                // very rule set the count line is about to confirm.
                if editor.rows.is_empty() && editor.words.trim().is_empty() {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new("No conditions yet: this collection matches every file in its scope.")
                                .weak()
                                .small(),
                        )
                        .selectable(false),
                    );
                }
                for (ix, row) in editor.rows.iter_mut().enumerate() {
                    ui.push_id(ix, |ui| {
                        ui.horizontal(|ui| {
                            egui::ComboBox::from_id_source("rule_field")
                                .selected_text(row.field.label())
                                .show_ui(ui, |ui| {
                                    for field in bc_core::Field::EDITABLE {
                                        if ui
                                            .selectable_value(&mut row.field, field, field.label())
                                            .clicked()
                                        {
                                            // Keep the operator when the new field
                                            // can use it, so switching name→keyword
                                            // does not silently change the question.
                                            // A field that offers nothing is a bug
                                            // in `EDITABLE`, not a reason to panic.
                                            if !field.operators().contains(&row.operator) {
                                                if let Some(first) = field.operators().first() {
                                                    row.operator = *first;
                                                }
                                            }
                                        }
                                    }
                                });
                            egui::ComboBox::from_id_source("rule_operator")
                                .selected_text(row.operator.label())
                                .show_ui(ui, |ui| {
                                    // Only the operators that can test this field,
                                    // which is why a row cannot be built that
                                    // matches nothing.
                                    for operator in row.field.operators() {
                                        ui.selectable_value(
                                            &mut row.operator,
                                            *operator,
                                            operator.label(),
                                        );
                                    }
                                });
                            let value_box = egui::TextEdit::singleline(&mut row.value)
                                .hint_text(if row.field.value_kind() == bc_core::ValueKind::Number {
                                    "number"
                                } else {
                                    "text"
                                });
                            // No popup of known values for colour label or flag,
                            // even though `bc_metadata::ratings` has an enum for
                            // each: labels also carry whatever another application
                            // wrote (`ColorLabel::Custom`), so a closed list would
                            // hide half of what the index can hold. The count line
                            // is the check on a misspelling instead.
                            //
                            // Left editable even for "is set", which reads no
                            // value: a rule loaded from a hand-edited
                            // `saved-searches.json` can arrive with text in this
                            // box, and `problem()` refuses to save it. Disabling
                            // the box would leave the dialog unable to save and
                            // the user unable to clear the thing stopping it.
                            ui.add(value_box).on_hover_text(if row.operator
                                == bc_core::Operator::Exists
                            {
                                "“is set” reads no value, so what is typed here is ignored"
                            } else {
                                "Matched against the values the index holds"
                            });
                            if ui
                                .small_button("✕")
                                .on_hover_text("Remove this condition")
                                .clicked()
                            {
                                remove = Some(ix);
                            }
                        });
                        if let Some(problem) = row.problem() {
                            ui.horizontal(|ui| {
                                ui.label("  ");
                                ui.colored_label(ui.visuals().warn_fg_color, problem);
                            });
                        }
                    });
                }

                ui.horizontal(|ui| {
                    if ui
                        .small_button("+ condition")
                        .on_hover_text("Add a condition on any indexed field")
                        .clicked()
                    {
                        editor.rows.push(RuleRow::new());
                    }
                    // Two of the questions the box makes slow to type by hand, and
                    // both are the shape of rule a folder listing cannot express.
                    if ui
                        .small_button("keyword…")
                        .on_hover_text("Files carrying a keyword")
                        .clicked()
                    {
                        editor.rows.push(RuleRow {
                            field: bc_core::Field::Keyword,
                            operator: bc_core::Operator::HasKeyword,
                            value: String::new(),
                        });
                    }
                    if ui
                        .small_button("starred")
                        .on_hover_text("Files with at least one star")
                        .clicked()
                    {
                        // "is set" is not this: every indexed file has a rating,
                        // zero included, so that rule would match the whole
                        // scope. One star is the floor the label promises.
                        editor.rows.push(RuleRow {
                            field: bc_core::Field::Rating,
                            operator: bc_core::Operator::GreaterThanOrEqual,
                            value: "1".to_string(),
                        });
                    }
                });

                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Look");
                    egui::ComboBox::from_id_source("editor_scope")
                        .selected_text(editor.scope.describe())
                        .show_ui(ui, |ui| {
                            for (choice, scope) in scope_options(root.as_deref()) {
                                ui.selectable_value(&mut editor.scope, scope, choice.label());
                            }
                        });
                });

                ui.separator();
                // One line, four states, and never a number that belongs to other
                // rules: an answer is shown only while the boxes still say what it
                // was counted for.
                if !problems.is_empty() {
                    let count = problems.len();
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        format!(
                            "{count} condition(s) above cannot match anything, so nothing was                              counted. Fix them or remove them."
                        ),
                    );
                } else if editor.outstanding.is_some() {
                    ui.weak("Counting…");
                } else if !editor.count_is_current(&key) {
                    ui.weak("Not counted for these rules yet.");
                } else if let Some(error) = &editor.count_error {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        format!("Could not count: {error}"),
                    );
                } else if let Some(count) = editor.count {
                    ui.label(format!("{count} file(s) match"));
                }

                ui.separator();
                ui.horizontal(|ui| {
                    let typed = editor.name.trim();
                    let replaces = !typed.is_empty()
                        && saved_names
                            .iter()
                            .any(|name| name.as_str() == typed);
                    let renamed = editor
                        .editing
                        .as_deref()
                        .is_some_and(|old| old != typed);
                    let can_save =
                        !typed.is_empty() && problems.is_empty() && !store_unreadable;
                    let reason = if store_unreadable {
                        "The saved-search file cannot be read, so nothing will be written over                          it."
                    } else if typed.is_empty() {
                        "A collection needs a name."
                    } else if !problems.is_empty() {
                        "Every condition has to be able to match before this can be saved."
                    } else if renamed {
                        "This name is new: the collection you opened stays in the list as it is."
                    } else if replaces {
                        "This replaces the rules the collection already holds."
                    } else {
                        "Store these conditions and this scope under the name above."
                    };
                    if ui
                        .add_enabled(can_save, egui::Button::new("Save").small())
                        .on_hover_text(reason)
                        .clicked()
                    {
                        save = true;
                    }
                    if ui
                        .add(egui::Button::new("Cancel").small())
                        .on_hover_text("Close without saving. Nothing has been written.")
                        .clicked()
                    {
                        dismiss = true;
                    }
                    if renamed {
                        ui.weak(format!("keeps “{}”", editor.editing.as_deref().unwrap_or("")));
                    }
                });
            });

        if save {
            let name = editor.name.trim().to_string();
            let search = editor.as_saved();
            let editing = editor.editing.clone();
            // Close only on a write that landed. A failure keeps the rules on
            // screen, because they exist nowhere else until it succeeds.
            dismiss = self.save_collection_draft(&name, &search, editing.as_deref());
        }
        if open && !dismiss {
            self.editor = Some(editor);
        }
    }

    fn delete_collection(&mut self, name: &str) {
        // Nothing else may hold the store while these lines run, so the
        // outcome is decided first and reported after.
        let outcome = match self.saved_store() {
            Some(store) => store.remove(name).map_err(|error| error.to_string()),
            None => Err(self
                .saved_store_error
                .clone()
                .unwrap_or_else(|| "the saved-search file could not be opened".to_string())),
        };
        match outcome {
            Ok(true) => {
                // If the deleted one is on screen, its rows stay until the user
                // moves on: the grid is the answer to a query that has already
                // run, and pulling the floor out from under a selection would
                // be worse than a title that no longer matches a list entry.
                self.refresh_collections();
                self.status = format!("Deleted collection “{name}”. No files were touched.");
            }
            Ok(false) => {
                // The list the user clicked was stale, so re-read it rather
                // than let them click a name that no longer exists again.
                self.refresh_collections();
                self.status = format!("“{name}” was already gone; the list has been re-read.");
            }
            Err(reason) => {
                self.status = format!("Could not delete “{name}”: {reason}");
                self.refresh_collections();
            }
        }
    }

    /// Drop every trace of a search from the interface state.
    ///
    /// Separate from `clear_search`, which *re-lists* the folder: navigation
    /// has already replaced the listing, and refreshing again would throw away
    /// the selection the new folder is about to make.
    fn forget_search(&mut self) {
        self.search_text.clear();
        self.search_due = None;
        self.search_waiting = None;
        self.search_error = None;
        self.search_scope_total = 0;
        self.search_scope_unindexed = 0;
    }

    /// Leave the search and show the folder again.
    fn clear_search(&mut self) {
        if !self.browser.is_searching() {
            self.search_error = None;
            return;
        }
        self.search_waiting = None;
        self.search_error = None;
        let root = self.browser.root.clone();
        match self.browser.refresh() {
            Ok(()) => {
                self.status = match root.as_deref() {
                    Some(root) => {
                        format!("Back to {} ({} items)", root.display(), self.browser.len())
                    }
                    None => "Back to the folder".to_string(),
                };
            }
            Err(error) => self.status = format!("Could not re-list the folder: {error}"),
        }
    }

    /// The line that explains a result grid. The count is not the whole story:
    /// a folder-scope search has to say how many of its files the index has
    /// never read, because those cannot appear however the box is worded.
    /// How much of the folder on screen the index has actually seen, for a
    /// scope that *is* that folder.
    ///
    /// Anything else answers zero. The numbers describe the listing, so
    /// attaching them to a search of somewhere else is a claim about files
    /// nobody is looking at — which is how a stale pair from the search box used
    /// to follow a collection run into the status line and describe a folder the
    /// collection had never mentioned.
    ///
    /// Taken from the folder listing *before* it is replaced: once the grid
    /// holds results, the files that are not in the index are exactly the ones
    /// that are not on screen.
    fn scope_coverage(&self, scope: &bc_core::SearchScope) -> (usize, usize) {
        let bc_core::SearchScope::Folder(root) = scope else {
            return (0, 0);
        };
        if self.browser.root.as_deref() != Some(root.as_path()) {
            return (0, 0);
        }
        let files: Vec<&Path> = self
            .browser
            .entries
            .iter()
            .filter(|e| !e.is_dir)
            .map(|e| e.path.as_path())
            .collect();
        (
            files.len(),
            self.browser.tags.missing_among(files.into_iter()),
        )
    }

    fn search_status(&self, count: usize) -> String {
        let Some((text, _query, scope)) = self.browser.search() else {
            return format!("{} items", self.browser.len());
        };
        if let Some(name) = self.browser.collection() {
            // A collection is not a typed search: the line has to say which of
            // the two the grid is, or "Search “”" would be the honest reading
            // of a saved rule set and that reads like a bug.
            let mut line = format!("Collection “{name}”: {count} file(s)");
            if self.search_scope_unindexed > 0 {
                line.push_str(&format!(
                    " · {} of {} files here are not indexed yet and cannot match",
                    self.search_scope_unindexed, self.search_scope_total
                ));
            }
            return line;
        }
        let _ = text;
        let scope_words = match scope {
            bc_core::SearchScope::Library => "the whole library".to_string(),
            bc_core::SearchScope::Folder(root) => format!("this folder ({})", root.display()),
            bc_core::SearchScope::FolderAndSubfolders(root) => {
                format!("this folder and subfolders ({})", root.display())
            }
        };
        let mut line = format!("Search “{text}”: {count} file(s) in {scope_words}");
        if self.search_scope_unindexed > 0 {
            line.push_str(&format!(
                " · {} of {} files here are not indexed yet and cannot match",
                self.search_scope_unindexed, self.search_scope_total
            ));
        }
        line
    }

    /// Which end of the sort comes first, in words that fit the button, plus
    /// the hover text that says what clicking does.
    fn sort_direction_label(&self) -> (String, String) {
        sort_direction_label(self.browser.sort_key, self.browser.sort_dir)
    }

    /// The rating/label/flag filter, in one menu next to the name filter.
    /// Returns true when the user changed it, so the caller can report the
    /// new count once instead of every frame.
    fn tag_filter_button(&mut self, ui: &mut egui::Ui) -> bool {
        let before = self.browser.tag_filter.clone();
        let active = self.browser.tag_filter.is_active();
        let summary = if active {
            "Tags ●".to_string()
        } else {
            "Tags".to_string()
        };
        ui.menu_button(summary, |ui| {
            ui.horizontal(|ui| {
                ui.label("Rating:");
                egui::ComboBox::from_id_source("tag_min_rating")
                    .selected_text(match self.browser.tag_filter.min_rating {
                        None => "any rating".to_string(),
                        Some(0) => "unrated only".to_string(),
                        Some(min) => format!("{min}★ or more"),
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.browser.tag_filter.min_rating,
                            None,
                            "any rating",
                        );
                        for min in 0..=5u8 {
                            let label = if min == 0 {
                                "unrated only".to_string()
                            } else {
                                format!("{min}★ or more")
                            };
                            ui.selectable_value(
                                &mut self.browser.tag_filter.min_rating,
                                Some(min),
                                &label,
                            );
                        }
                    });
            });
            ui.horizontal(|ui| {
                ui.label("Label:");
                egui::ComboBox::from_id_source("tag_label")
                    .selected_text(
                        self.browser
                            .tag_filter
                            .label
                            .as_deref()
                            .unwrap_or("any label"),
                    )
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.browser.tag_filter.label, None, "any label");
                        // The empty name is how "no label at all" is stored, so
                        // it is offered as the chip the panel already shows.
                        ui.selectable_value(
                            &mut self.browser.tag_filter.label,
                            Some(String::new()),
                            "none",
                        );
                        for (name, _) in metadata_edit::LABEL_CHOICES {
                            if name.is_empty() {
                                continue;
                            }
                            ui.selectable_value(
                                &mut self.browser.tag_filter.label,
                                Some((*name).to_string()),
                                *name,
                            );
                        }
                    });
            });
            ui.horizontal(|ui| {
                ui.label("Flag:");
                egui::ComboBox::from_id_source("tag_flag")
                    .selected_text(
                        self.browser
                            .tag_filter
                            .flag
                            .as_deref()
                            .unwrap_or("any flag"),
                    )
                    .show_ui(ui, |ui| {
                        for option in ["any flag", "None", "Approved", "Rejected"] {
                            ui.selectable_value(
                                &mut self.browser.tag_filter.flag,
                                (option != "any flag").then(|| option.to_string()),
                                option,
                            );
                        }
                    });
            });
            ui.separator();
            // The honesty note, in the place the filter is set: files the index
            // has not read cannot be shown as having a rating they may have.
            let missing = self.browser.unindexed_len();
            if missing > 0 {
                // Two different states, and saying the wrong one here would be
                // a small lie with consequences: with no filter in force,
                // nothing is hidden, and the number is a heads-up rather than
                // an explanation of a shortened grid.
                let text = if self.browser.tag_filter.is_active() {
                    format!(
                        "{missing} of them have no index row, so a tag filter hides them \
                         whatever their real rating."
                    )
                } else {
                    format!(
                        "{missing} of them have no index row yet; a tag filter would hide \
                         them until the pass reads them."
                    )
                };
                ui.weak(text);
            }
            if self.browser.tag_filter.is_active() && ui.button("Clear tag filter").clicked() {
                self.browser.tag_filter = bc_core::fs::TagFilter::default();
            }
        });
        self.browser.tag_filter != before
    }

    /// What a tag sort needs the user to know: how much of the folder the index
    /// has actually read, because files it has not read sort as unrated.
    fn tag_sort_status(&self) -> String {
        let files = self.browser.entries.iter().filter(|e| !e.is_dir).count();
        let missing = self.browser.unindexed_len();
        if self.index_error.is_some() {
            return format!(
                "Sorting by {}: the search index is offline, so nothing has a value to sort on",
                self.browser.sort_key.label()
            );
        }
        if missing == 0 {
            return format!(
                "Sorting by {} ({} files, all indexed)",
                self.browser.sort_key.label(),
                files
            );
        }
        format!(
            "Sorting by {} · {missing} of {files} files not indexed yet, shown as {note}",
            self.browser.sort_key.label(),
            note = match self.browser.sort_key {
                SortKey::Rating => "unrated",
                SortKey::Label => "unlabeled",
                SortKey::Flag => "unflagged",
                _ => "unordered",
            }
        )
    }

    fn tag_filter_status(&self, shown: usize) -> String {
        let hidden_unindexed = self
            .browser
            .entries
            .iter()
            .filter(|e| !e.is_dir && self.browser.tags.get(&e.path).is_none())
            .count();
        if !self.browser.tag_filter.is_active() {
            return format!(
                "Tag filter cleared: {} of {} items",
                shown,
                self.browser.len()
            );
        }
        let mut text = format!("Tag filter: {} of {} items", shown, self.browser.len());
        if hidden_unindexed > 0 {
            text.push_str(&format!(" · {hidden_unindexed} not indexed and hidden"));
        }
        text
    }

    /// Reload the listing after a drop so the results are visible immediately
    /// rather than waiting for the debounced watcher refresh.
    fn refresh_after_drop(&mut self) {
        let _ = self.browser.refresh_preserving_selection();
    }

    /// Drain watcher events; schedule a debounced refresh when any arrive.
    fn poll_watcher(&mut self) {
        if self.refresh_due.is_none() {
            if let Some(w) = &self.watcher {
                if !w.drain().is_empty() {
                    self.refresh_due = Some(Instant::now() + WATCHER_DEBOUNCE);
                }
            }
        } else {
            // Keep draining while a refresh is pending so the queue never backs up.
            if let Some(w) = &self.watcher {
                let _ = w.drain();
            }
            if Instant::now() >= self.refresh_due.unwrap() {
                self.refresh_due = None;
                match self.browser.refresh_preserving_selection() {
                    Ok(()) => {
                        // Files may have been fixed or replaced on disk.
                        self.failed.clear();
                        self.tree_children.clear();
                        self.status = format!("Refreshed ({} items)", self.browser.len());
                        // …and a grid showing an answer needs that answer
                        // re-derived, not the folder listed underneath it.
                        if self.browser.is_searching() {
                            self.request_search();
                        }
                        // …and their index rows may be stale. A settled watcher
                        // event is the only signal that something changed
                        // outside BridgeCraft, so re-run the pass over the
                        // folder: rows that are still current cost two stats
                        // and one indexed lookup each, and are reported as
                        // skipped rather than re-read.
                        if let Some(root) = self.browser.root.clone() {
                            self.start_indexing(&root);
                        }
                    }
                    Err(e) => tracing::warn!("live refresh failed: {e}"),
                }
            }
        }
    }

    /// Collect finished thumbnail results and upload them as GPU textures.
    fn poll_thumbnails(&mut self, ctx: &egui::Context) {
        for result in self.thumb_rx.try_iter() {
            self.pending.remove(&(result.path.clone(), result.max_edge));
            if let Some(thumb) = result.thumb {
                if !thumb.is_consistent() {
                    continue;
                }
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [thumb.width as usize, thumb.height as usize],
                    &thumb.pixels,
                );
                let handle = ctx.load_texture(
                    format!("{}@{}", result.path.display(), result.max_edge),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                self.textures.insert((result.path, result.max_edge), handle);
            } else {
                self.failed.insert((result.path, result.max_edge));
            }
        }
    }

    fn request_thumbnail(&mut self, path: &Path, mtime: i64, size: u64) {
        self.request_image(path, mtime, size, THUMB_MAX_EDGE, true);
    }

    fn request_image(&mut self, path: &Path, mtime: i64, size: u64, max_edge: u32, cached: bool) {
        let key = (path.to_path_buf(), max_edge);
        if self.textures.contains_key(&key)
            || self.pending.contains(&key)
            || self.failed.contains(&key)
        {
            return;
        }
        if self
            .thumb_tx
            .send(ThumbJob {
                path: path.to_path_buf(),
                mtime_ns: mtime,
                size,
                max_edge,
                cached,
            })
            .is_ok()
        {
            self.pending.insert(key);
        }
    }

    // -------------------------------------------------------------- shell --

    /// Entries the grid/list/status actually show: browser visibility plus
    /// the optional "thumbnails only" filter.
    fn display_indices(&self) -> Vec<usize> {
        let visible = self.browser.visible_indices();
        if !self.thumbs_only {
            return visible;
        }
        visible
            .into_iter()
            .filter(|&idx| {
                self.browser
                    .entries
                    .get(idx)
                    .is_some_and(|e| e.is_dir || bc_thumbnails::decoder::can_thumbnail(&e.path))
            })
            .collect()
    }

    /// The single selected entry, when exactly one file/folder is selected.
    fn single_selection(&self) -> Option<usize> {
        let mut indices = self.browser.selection.indices();
        match (indices.next(), indices.next()) {
            (Some(idx), None) if self.browser.entries.get(idx).is_some() => Some(idx),
            _ => None,
        }
    }

    fn selected_total_size(&self) -> u64 {
        self.browser
            .selection
            .indices()
            .filter_map(|idx| self.browser.entries.get(idx))
            .map(|entry| entry.size)
            .sum()
    }

    fn refresh_now(&mut self) {
        match self.browser.refresh_preserving_selection() {
            Ok(()) => {
                self.failed.clear();
                self.tree_children.clear();
                self.status = format!("Refreshed ({} items)", self.browser.len());
            }
            Err(e) => self.status = format!("Refresh failed: {e}"),
        }
    }

    /// Subdirectories of `dir` for the tree, cached after first read.
    fn tree_children_of(&mut self, dir: &Path) -> Vec<PathBuf> {
        if let Some(cached) = self.tree_children.get(dir) {
            return cached.clone().unwrap_or_default();
        }
        let mut children: Vec<PathBuf> = std::fs::read_dir(dir)
            .ok()
            .map(|read| {
                read.filter_map(Result::ok)
                    .filter_map(|item| {
                        let path = item.path();
                        let is_dir = item.file_type().ok().is_some_and(|ft| ft.is_dir());
                        // Hidden dot-folders are noise in a favorites tree.
                        let hidden = path
                            .file_name()
                            .is_some_and(|n| n.to_string_lossy().starts_with('.'));
                        (is_dir && !hidden).then_some(path)
                    })
                    .collect()
            })
            .unwrap_or_default();
        children.sort();
        self.tree_children
            .insert(dir.to_path_buf(), Some(children.clone()));
        children
    }

    /// One folder-tree row plus its expanded children.
    fn tree_node(&mut self, ui: &mut egui::Ui, dir: &Path, depth: usize) {
        let children = self.tree_children_of(dir);
        let is_open = self.expanded.contains(dir);
        let at = self.browser.root.as_deref() == Some(dir);

        ui.push_id(dir, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(depth as f32 * 12.0);
                let twist = if children.is_empty() {
                    "·"
                } else if is_open {
                    "-"
                } else {
                    "+"
                };
                if ui
                    .add(egui::Label::new(twist).selectable(false))
                    .on_hover_text("Expand/collapse")
                    .interact(egui::Sense::click())
                    .clicked()
                    && !children.is_empty()
                {
                    if is_open {
                        self.expanded.remove(dir);
                    } else {
                        self.expanded.insert(dir.to_path_buf());
                    }
                }
                if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new(format!("📁 {}", node_label(dir))).small(),
                        )
                        .selected(at),
                    )
                    .clicked()
                {
                    self.open_root(dir);
                }
            });
        });

        if is_open {
            for child in &children {
                self.tree_node(ui, child, depth + 1);
            }
        }
    }

    /// Left sidebar: Favorites/Folders tabs, Recent shortcuts, Trash entry.
    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.sidebar_tab, SidebarTab::Favorites, "★ Favorites");
            ui.selectable_value(&mut self.sidebar_tab, SidebarTab::Folders, "📁 Folders");
            ui.selectable_value(
                &mut self.sidebar_tab,
                SidebarTab::Collections,
                "◎ Collections",
            );
        });
        ui.separator();

        // Bottom dock inside the sidebar: recents + trash.
        egui::TopBottomPanel::bottom("sidebar_dock").show_inside(ui, |ui| {
            ui.separator();
            ui.add(egui::Label::new(egui::RichText::new("Recent").weak()).selectable(false));
            if self.recent_paths.is_empty() {
                ui.weak("No recent folders.");
            } else {
                let recent = self.recent_paths.clone();
                egui::ScrollArea::vertical()
                    .max_height(SIDEBAR_RECENTS as f32 * 24.0)
                    .auto_shrink([true, false])
                    .show(ui, |ui| {
                        for path in recent.iter().take(SIDEBAR_RECENTS) {
                            let short = node_label(path);
                            let width = ui.available_width().max(60.0);
                            let clicked = ui
                                .add_sized(
                                    [width, 18.0],
                                    egui::Button::new(egui::RichText::new(&short).small()),
                                )
                                .on_hover_text(path.display().to_string())
                                .clicked();
                            if clicked {
                                self.open_root(path);
                            }
                        }
                    });
            }
            ui.separator();
            if ui.button("Trash…").clicked() {
                self.trash_window = true;
            }
        });

        // Read the collections when the tab is *entered*, not on every frame:
        // the panel redraws constantly, and re-reading per redraw would put a
        // database query in the paint path. Leaving and coming back is the
        // refresh, alongside the explicit ones after a save or a delete.
        if self.sidebar_tab != self.sidebar_tab_shown {
            self.sidebar_tab_shown = self.sidebar_tab;
            if self.sidebar_tab == SidebarTab::Collections {
                self.refresh_collections();
            }
        }

        match self.sidebar_tab {
            SidebarTab::Collections => {
                self.collections_panel(ui);
            }
            SidebarTab::Favorites => {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if let Some(error) = &self.favorites_error {
                        ui.weak(format!("Favorites unavailable: {error}"));
                        return;
                    }
                    let favorites = match &self.favorites {
                        Some(store) => store.paths().to_vec(),
                        None => Vec::new(),
                    };
                    if favorites.is_empty() {
                        ui.weak(
                            "No favorites yet.\nUse “+ Add current folder” to star this folder.",
                        );
                    }
                    for fav in favorites {
                        let mut remove = false;
                        ui.horizontal(|ui| {
                            let label = node_label(&fav);
                            let at = self.browser.root.as_deref() == Some(fav.as_path());
                            let width = (ui.available_width() - 32.0).max(60.0);
                            let clicked = ui
                                .add_sized(
                                    [width, 18.0],
                                    egui::Button::new(
                                        egui::RichText::new(format!("⭐ {label}")).small(),
                                    )
                                    .selected(at),
                                )
                                .on_hover_text(fav.display().to_string())
                                .clicked();
                            if ui
                                .small_button("×")
                                .on_hover_text("Remove from favorites")
                                .clicked()
                            {
                                remove = true;
                            }
                            if clicked {
                                self.open_root(&fav);
                            }
                        });
                        if remove {
                            if let Some(store) = &mut self.favorites {
                                match store.remove(&fav) {
                                    Ok(_) => {
                                        self.status =
                                            format!("Removed {} from favorites", fav.display())
                                    }
                                    Err(e) => {
                                        self.status = format!("Could not update favorites: {e}")
                                    }
                                }
                            }
                        }
                    }
                    if let Some(root) = self.browser.root.clone() {
                        let already = self
                            .favorites
                            .as_ref()
                            .is_some_and(|store| store.contains(&root));
                        if !already && ui.button("+ Add current folder").clicked() {
                            if let Some(store) = &mut self.favorites {
                                match store.add(&root) {
                                    Ok(true) => {
                                        self.status =
                                            format!("Added {} to favorites", root.display())
                                    }
                                    Ok(false) => {}
                                    Err(e) => {
                                        self.status = format!("Could not favorite folder: {e}")
                                    }
                                }
                            }
                        }
                    }
                });
            }
            SidebarTab::Folders => {
                let root = self.browser.root.clone();
                egui::ScrollArea::vertical().show(ui, |ui| match root {
                    Some(root) => {
                        // The tree is anchored at the open folder; breadcrumb
                        // chips and Recents handle upward navigation.
                        self.tree_node(ui, &root, 0);
                    }
                    None => {
                        ui.weak("Open a folder to browse the tree.");
                    }
                });
            }
        }
    }

    /// Cached header-only dimensions for the metadata panel.
    fn dims_of(&mut self, path: &Path) -> Option<(u32, u32)> {
        if !self.dims_cache.contains_key(path) {
            let dims = bc_thumbnails::decoder::image_dimensions(path);
            self.dims_cache.insert(path.to_path_buf(), dims);
        }
        self.dims_cache.get(path).copied().flatten()
    }

    /// Metadata for a file, cached until the file or its sidecar changes.
    /// Failed reads cache an empty map so the panel does not retry per frame.
    fn exif_for(&mut self, path: &Path, mtime: i64) -> Option<bc_metadata::Metadata> {
        let sidecar_mtime = std::fs::metadata(bc_metadata::xmp::sidecar_path(path))
            .and_then(|m| m.modified())
            .ok();
        let stamp = mtime.max(mtime_ns(sidecar_mtime));
        if let Some((cached_mtime, meta)) = self.exif_cache.get(path) {
            if *cached_mtime == stamp {
                return Some(meta.clone());
            }
        }
        let meta = metadata_edit::read_panel_metadata(path);
        self.exif_cache
            .insert(path.to_path_buf(), (stamp, meta.clone()));
        Some(meta)
    }

    /// The four tag properties for a file, read through the same cache as the
    /// panel (which already folds the sidecar's mtime into its key). Cell and
    /// row overlays call this only for what is on screen.
    fn tags_for(&mut self, path: &Path, mtime: i64) -> metadata_edit::TagState {
        self.exif_for(path, mtime)
            .map(|meta| metadata_edit::TagState::from_metadata(&meta))
            .unwrap_or_default()
    }

    fn prop_row(ui: &mut egui::Ui, label: &str, value: String) {
        ui.horizontal(|ui| {
            ui.add_sized(
                [76.0, 16.0],
                egui::Label::new(egui::RichText::new(label).weak().small()).selectable(false),
            );
            ui.add(
                egui::Label::new(egui::RichText::new(value).small())
                    .truncate(true)
                    .selectable(true),
            );
        });
    }

    /// Right panel: Preview tab (image) and Metadata tab (properties).
    fn metadata_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.right_tab, RightTab::Preview, "Preview");
            ui.selectable_value(&mut self.right_tab, RightTab::Metadata, "Metadata");
        });
        ui.separator();

        let count = self.browser.selection.len();
        let single = self.single_selection();
        let Some(idx) = single else {
            if count > 0 && self.right_tab == RightTab::Metadata {
                self.bulk_metadata_panel(ui, count);
            } else {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if count == 0 {
                        ui.weak("Nothing selected.");
                    } else {
                        ui.label(format!("{count} selected"));
                        let total = self.selected_total_size();
                        ui.weak(format!("{} total", human_size(total)));
                        for path in self.selected_paths(Path::new("")).iter().take(200) {
                            ui.weak(
                                path.file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| path.display().to_string()),
                            );
                        }
                    }
                });
            }
            return;
        };

        let Some(entry) = self.browser.entries.get(idx).cloned() else {
            ui.weak("Selection is no longer visible.");
            return;
        };
        let name = entry.name().to_string_lossy().into_owned();
        ui.add(
            egui::Label::new(egui::RichText::new(&name).strong().small())
                .truncate(true)
                .selectable(false),
        )
        .on_hover_text(entry.path.display().to_string());
        ui.separator();

        match self.right_tab {
            RightTab::Preview => {
                let path = entry.path.clone();
                if entry.is_dir {
                    ui.centered_and_justified(|ui| {
                        ui.label(egui::RichText::new("📁").size(48.0));
                    });
                    ui.centered_and_justified(|ui| ui.weak("Folder"));
                } else if bc_thumbnails::decoder::can_thumbnail(&path) {
                    // Ask the cache for the big tier once; the grid tier fills
                    // in while it decodes.
                    if let Some(meta) = self.entry_meta(&path) {
                        self.request_image(&path, meta.0, meta.1, PREVIEW_EDGE, true);
                    }
                    let texture = self
                        .textures
                        .get(&(path.clone(), PREVIEW_EDGE))
                        .or_else(|| self.textures.get(&(path.clone(), THUMB_MAX_EDGE)))
                        .cloned();
                    match texture {
                        Some(tex) => {
                            let (rect, _) =
                                ui.allocate_exact_size(ui.available_size(), egui::Sense::hover());
                            let size = tex.size_vec2();
                            let scale = ((rect.width() / size.x.max(1.0))
                                .min(rect.height() / size.y.max(1.0)))
                            .clamp(0.02, 8.0);
                            let draw_rect =
                                egui::Rect::from_center_size(rect.center(), size * scale);
                            ui.painter().image(
                                tex.id(),
                                draw_rect,
                                egui::Rect::from_min_max(
                                    egui::pos2(0.0, 0.0),
                                    egui::pos2(1.0, 1.0),
                                ),
                                egui::Color32::WHITE,
                            );
                        }
                        None => {
                            let key = (path.clone(), PREVIEW_EDGE);
                            let message = if self.failed.contains(&key) {
                                "Preview failed to decode"
                            } else {
                                "Loading preview…"
                            };
                            ui.centered_and_justified(|ui| ui.weak(message));
                        }
                    }
                } else {
                    ui.weak("No preview for this file type.");
                }
            }
            RightTab::Metadata => {
                let path = entry.path.clone();
                let dims = if entry.is_dir {
                    None
                } else {
                    self.dims_of(&path)
                };
                let file_type = if entry.is_dir {
                    "Folder".to_owned()
                } else {
                    entry
                        .path
                        .extension()
                        .map(|ext| format!("{} file", ext.to_string_lossy().to_uppercase()))
                        .unwrap_or_else(|| "File".to_owned())
                };
                let mtime = mtime_ns(entry.modified);
                let exif = if entry.is_dir || !bc_thumbnails::decoder::can_thumbnail(&path) {
                    None
                } else {
                    self.exif_for(&path, mtime)
                };
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if let Some(meta) = exif.as_ref().filter(|meta| !meta.is_empty()) {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new("Camera Data (EXIF)").weak().small(),
                            )
                            .selectable(false),
                        );
                        if let Some(v) = format_camera(meta) {
                            Self::prop_row(ui, "Camera", v);
                        }
                        if let Some(v) = meta
                            .text(bc_metadata::NS_EXIF, "DateTimeOriginal")
                            .or_else(|| meta.text(bc_metadata::NS_EXIF, "DateTime"))
                        {
                            Self::prop_row(ui, "Taken", v.to_string());
                        }
                        for (label, local) in [
                            ("Exposure", "ExposureTime"),
                            ("Aperture", "FNumber"),
                            ("ISO", "ISOSpeedRatings"),
                            ("Focal Length", "FocalLength"),
                            ("Orientation", "Orientation"),
                            ("Color Space", "ColorSpace"),
                        ] {
                            if let Some(v) = meta.text(bc_metadata::NS_EXIF, local) {
                                Self::prop_row(ui, label, v.to_string());
                            }
                        }
                        if let Some(v) = exif_image_size(meta) {
                            Self::prop_row(ui, "EXIF Size", v);
                        }
                        if let Some(v) = exif_gps(meta) {
                            Self::prop_row(ui, "GPS", v);
                        }
                        ui.separator();
                    }
                    self.metadata_form_ui(ui, &path, mtime);
                    ui.separator();
                    ui.add(
                        egui::Label::new(egui::RichText::new("File Properties").weak().small())
                            .selectable(false),
                    );
                    Self::prop_row(ui, "Name", name.clone());
                    Self::prop_row(ui, "Kind", file_type.clone());
                    if let Some((w, h)) = dims {
                        Self::prop_row(ui, "Dimensions", format!("{w} × {h}"));
                    }
                    Self::prop_row(
                        ui,
                        "Size",
                        if entry.is_dir {
                            "—".to_owned()
                        } else {
                            human_size(entry.size)
                        },
                    );
                    Self::prop_row(ui, "Modified", format_modified(entry.modified));
                    Self::prop_row(ui, "Path", path.display().to_string());
                });
            }
        }
    }

    /// Rating, color label, flag and keywords for the current selection.
    ///
    /// These four go to the XMP sidecar rather than into the image, because
    /// they have no EXIF equivalent and embedded XMP writing is still Phase 3
    /// work. The section says where they land, so nobody has to guess why the
    /// JPEG itself did not change.
    fn tags_ui(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.add(
            egui::Label::new(egui::RichText::new("Rating & labels").strong().small())
                .selectable(false),
        );

        if let Some(stars) = tag_widgets::rating_row(ui, self.tags_form.current().stars) {
            self.tags_form.set_stars(stars);
        }
        if let Some(label) = tag_widgets::label_chips(ui, &self.tags_form.current().label) {
            self.tags_form.set_label(&label);
        }
        if let Some(flag) = tag_widgets::flag_row(ui, self.tags_form.current().flag) {
            self.tags_form.set_flag(flag);
        }

        ui.add(
            egui::Label::new(
                egui::RichText::new("Keywords (stored in the XMP sidecar; click one to remove)")
                    .weak()
                    .small(),
            )
            .selectable(false),
        );
        if let Some(removed) = tag_widgets::keyword_chips(ui, &self.tags_form.current().keywords) {
            self.tags_form.remove_keyword(&removed);
        }

        let mut request_add = false;
        ui.horizontal(|ui| {
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.keyword_draft)
                    .hint_text("new keyword")
                    .font(egui::TextStyle::Small)
                    .desired_width(ui.available_width() - 42.0),
            );
            if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                request_add = true;
            }
            if ui.small_button("Add").clicked() {
                request_add = true;
            }
        });

        // Completions used to come only from the hierarchy this file carries,
        // because there was no pool to ask. The indexer has since filled the
        // index's keyword table from every folder it has read, so the box can
        // answer the question a keyword actually raises: not "what is this word
        // called" but "does this library already have a word for it, and how is
        // it spelled". That is the one place a near-duplicate can still be
        // stopped, because after the save there are two keywords and no
        // interface that will merge them.
        let carried: Vec<String> = self
            .tags_form
            .current()
            .keywords
            .iter()
            .map(|keyword| bc_metadata::keywords::normalize_keyword(keyword))
            .collect();
        let chosen = self.keyword_completions(ui, self.keyword_draft.clone(), &carried);
        if let Some(suggestion) = chosen {
            self.keyword_draft = suggestion;
            request_add = true;
        }

        if request_add {
            let text = std::mem::take(&mut self.keyword_draft);
            let trimmed = text.trim().to_string();
            if trimmed.is_empty() {
                self.metadata_status = Some("A keyword needs some text.".to_string());
            } else if !self.tags_form.add_keyword(&trimmed) {
                self.metadata_status = Some(format!("\"{trimmed}\" is already on this file."));
            }
        }
    }

    /// Editable metadata form for the single selection: dirty rows are
    /// tinted, Save writes through the #5 policy and Revert restores the
    /// values loaded from disk.
    fn metadata_form_ui(&mut self, ui: &mut egui::Ui, path: &Path, mtime: i64) {
        let needs_load =
            self.metadata_form_path.as_deref() != Some(path) || self.metadata_form_mtime != mtime;
        if needs_load {
            let meta = self.exif_for(path, mtime).unwrap_or_default();
            self.metadata_form.load(&meta);
            self.tags_form.load(&meta);
            self.keyword_draft.clear();
            self.metadata_form_path = Some(path.to_path_buf());
            self.metadata_form_mtime = mtime;
            self.metadata_status = None;
        }

        ui.add(egui::Label::new(egui::RichText::new("Metadata").weak().small()).selectable(false));
        self.tags_ui(ui);
        for value in self.metadata_form.values_mut() {
            let dirty = value.is_dirty();
            let label = if dirty {
                format!("• {}", value.spec.label)
            } else {
                value.spec.label.to_string()
            };
            let rich = if dirty {
                egui::RichText::new(label)
                    .small()
                    .color(ui.visuals().warn_fg_color)
            } else {
                egui::RichText::new(label).weak().small()
            };
            if value.spec.multiline {
                ui.add(egui::Label::new(rich).selectable(false));
                ui.add(
                    egui::TextEdit::multiline(&mut value.current)
                        .font(egui::TextStyle::Small)
                        .desired_rows(2)
                        .desired_width(f32::INFINITY),
                );
            } else {
                ui.horizontal(|ui| {
                    ui.add_sized([76.0, 16.0], egui::Label::new(rich).selectable(false));
                    ui.add(
                        egui::TextEdit::singleline(&mut value.current)
                            .font(egui::TextStyle::Small)
                            .desired_width(f32::INFINITY),
                    );
                });
            }
        }

        let text_dirty = self.metadata_form.is_dirty();
        let tags_dirty = self.tags_form.is_dirty();
        let dirty = text_dirty || tags_dirty;
        let text_count = self.metadata_form.dirty_count();
        // The count says what is unsaved rather than implying only fields:
        // saving is enabled for tags too, and "0 unsaved" next to a live Save
        // button would read as a bug.
        let summary = match (text_count, tags_dirty) {
            (0, false) => None,
            (0, true) => Some("tags unsaved".to_string()),
            (n, false) => Some(format!("{n} unsaved")),
            (n, true) => Some(format!("{n} fields + tags unsaved")),
        };
        ui.horizontal(|ui| {
            if ui
                .add_enabled(dirty, egui::Button::new("Save").small())
                .clicked()
            {
                let mut problems: Vec<String> = Vec::new();
                if text_dirty {
                    let changes = self.metadata_form.dirty_changes();
                    let report = metadata_edit::apply_changes(
                        std::slice::from_ref(&path.to_path_buf()),
                        &changes,
                        bc_metadata::WritePolicy::Auto,
                    );
                    problems.extend(report.failures.into_iter().map(|f| f.error));
                }
                let mut wrote_to: Option<&'static str> = None;
                if tags_dirty {
                    let tags = self.tags_form.current().clone();
                    match metadata_edit::save_tags(path, &tags) {
                        Ok(target) => wrote_to = Some(target.describe()),
                        Err(error) => problems.push(error),
                    }
                }
                match problems.first() {
                    Some(error) => self.metadata_status = Some(format!("Save failed: {error}")),
                    None => {
                        self.exif_cache.remove(path);
                        self.index_saved_tags(path);
                        let saved = metadata_edit::read_panel_metadata(path);
                        self.metadata_form.load(&saved);
                        self.tags_form.load(&saved);
                        self.metadata_form_mtime =
                            mtime_ns(std::fs::metadata(path).and_then(|m| m.modified()).ok());
                        self.metadata_status = Some(match wrote_to {
                            // Saying where tags landed is the point: "Saved."
                            // alone hides that a JPEG keeps them in the file
                            // while a RAW keeps them beside it.
                            Some(location) => format!("Saved: {location}."),
                            None => "Saved.".to_string(),
                        });
                        self.status = format!("Saved metadata for {}", path.display());
                    }
                }
            }
            if ui
                .add_enabled(dirty, egui::Button::new("Revert").small())
                .clicked()
            {
                self.metadata_form.revert();
                self.tags_form.revert();
                self.keyword_draft.clear();
                self.metadata_status = None;
            }
            if let Some(summary) = summary {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(summary)
                            .small()
                            .color(ui.visuals().warn_fg_color),
                    )
                    .selectable(false),
                );
            }
        });
        if let Some(status) = &self.metadata_status {
            ui.add(egui::Label::new(egui::RichText::new(status).small()).selectable(false));
        }
    }

    /// Multi-selection bulk editor: the fields that make sense to stamp
    /// across many files, with a per-file result summary.
    fn bulk_metadata_panel(&mut self, ui: &mut egui::Ui, count: usize) {
        let paths: Vec<PathBuf> = self
            .browser
            .entries
            .iter()
            .enumerate()
            .filter(|(idx, _)| self.browser.selection.contains(*idx))
            .map(|(_, entry)| entry.path.clone())
            .collect();
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!("Bulk edit — {count} selected"))
                        .strong()
                        .small(),
                )
                .selectable(false),
            );
            ui.weak(format!(
                "{} total. Empty fields are left unchanged.",
                human_size(self.selected_total_size())
            ));
            ui.separator();

            for spec in metadata_edit::EDITABLE_FIELDS
                .iter()
                .filter(|spec| spec.bulk)
            {
                ui.add(egui::Label::new(
                    egui::RichText::new(spec.label).weak().small(),
                ));
                let buffer = match spec.id.name {
                    "ImageDescription" => &mut self.bulk_description,
                    "Artist" => &mut self.bulk_artist,
                    "Copyright" => &mut self.bulk_copyright,
                    _ => continue,
                };
                if spec.multiline {
                    ui.add(
                        egui::TextEdit::multiline(buffer)
                            .font(egui::TextStyle::Small)
                            .desired_rows(2)
                            .desired_width(f32::INFINITY),
                    );
                } else {
                    ui.add(
                        egui::TextEdit::singleline(buffer)
                            .font(egui::TextStyle::Small)
                            .desired_width(f32::INFINITY),
                    );
                }
            }

            ui.separator();
            ui.add(
                egui::Label::new(egui::RichText::new("Rating & labels").strong().small())
                    .selectable(false),
            );
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.bulk_rating_enabled, "Set rating");
                if self.bulk_rating_enabled {
                    if let Some(stars) =
                        tag_widgets::rating_row(ui, self.bulk_tags.stars.unwrap_or(0))
                    {
                        self.bulk_tags.stars = Some(stars);
                    }
                } else {
                    self.bulk_tags.stars = None;
                }
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.bulk_label_enabled, "Set label");
                if self.bulk_label_enabled {
                    if let Some(label) =
                        tag_widgets::label_chips(ui, self.bulk_tags.label.as_deref().unwrap_or(""))
                    {
                        self.bulk_tags.label = if label.is_empty() { None } else { Some(label) };
                    }
                } else {
                    self.bulk_tags.label = None;
                }
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.bulk_flag_enabled, "Set flag");
                if self.bulk_flag_enabled {
                    if let Some(flag) =
                        tag_widgets::flag_row(ui, self.bulk_tags.flag.unwrap_or_default())
                    {
                        self.bulk_tags.flag = Some(flag);
                    }
                } else {
                    self.bulk_tags.flag = None;
                }
            });
            ui.horizontal(|ui| {
                ui.label("Add keyword:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.bulk_keyword)
                        .font(egui::TextStyle::Small)
                        .desired_width(110.0),
                );
                ui.label("Remove:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.bulk_keyword_remove)
                        .font(egui::TextStyle::Small)
                        .desired_width(110.0),
                );
            });
            // The same vocabulary question the single-file box asks, in the
            // place where a wrong answer is worst: a misspelling stamped on
            // two hundred files is two hundred copies of a keyword no
            // search will find.
            let chosen = self.keyword_completions(ui, self.bulk_keyword.clone(), &[]);
            if let Some(keyword) = chosen {
                self.bulk_keyword = keyword;
            }

            let has_value = !self.bulk_description.trim().is_empty()
                || !self.bulk_artist.trim().is_empty()
                || !self.bulk_copyright.trim().is_empty()
                || !self.bulk_tags.is_empty();
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(has_value, egui::Button::new("Apply to selection").small())
                    .clicked()
                {
                    let mut changes = Vec::new();
                    for (name, value) in [
                        ("ImageDescription", &self.bulk_description),
                        ("Artist", &self.bulk_artist),
                        ("Copyright", &self.bulk_copyright),
                    ] {
                        if !value.trim().is_empty() {
                            changes.push(metadata_edit::FieldChange {
                                id: metadata_edit::FieldId {
                                    namespace: bc_metadata::NS_EXIF,
                                    name,
                                },
                                value: value.clone(),
                            });
                        }
                    }
                    let mut stamp = self.bulk_tags.clone();
                    let add = self.bulk_keyword.trim().to_string();
                    let remove = self.bulk_keyword_remove.trim().to_string();
                    if !add.is_empty() {
                        stamp.add_keywords = vec![add];
                    }
                    if !remove.is_empty() {
                        stamp.remove_keywords = vec![remove];
                    }
                    let stamp_is_empty = stamp.is_empty();
                    let tags = metadata_edit::apply_stamp_to_many(&paths, &stamp);
                    let report = metadata_edit::apply_changes(
                        &paths,
                        &changes,
                        bc_metadata::WritePolicy::Auto,
                    );
                    // One failure list for both writes: a file whose tags
                    // failed is as real to the user as one whose fields did.
                    let mut failures: Vec<(PathBuf, String)> = report
                        .failures
                        .iter()
                        .map(|failure| (failure.path.clone(), failure.error.clone()))
                        .collect();
                    failures.extend(tags.failures.clone());
                    let mut parts = Vec::new();
                    if !changes.is_empty() {
                        parts.push(format!("{} files with fields written", report.succeeded));
                    }
                    if !stamp_is_empty {
                        parts.push(format!("{} files with tags written", tags.succeeded));
                    }
                    let mut combined = report;
                    combined.failures.extend(
                        tags.failures
                            .into_iter()
                            .map(|(path, error)| metadata_edit::BulkFailure { path, error }),
                    );
                    self.status = if parts.is_empty() {
                        "Bulk edit: nothing to write.".to_string()
                    } else {
                        format!(
                            "Bulk edit: {}; {} file(s) failed",
                            parts.join(", "),
                            failures.len()
                        )
                    };
                    self.exif_cache.clear();
                    self.metadata_form_path = None;
                    // Re-index the files that were actually written. The tag
                    // sort and the tag filter read the index, so a bulk stamp
                    // that left the rows behind would move nothing.
                    let failed: HashSet<PathBuf> =
                        failures.iter().map(|(path, _)| path.clone()).collect();
                    for path in paths.iter().filter(|path| !failed.contains(*path)) {
                        self.index_saved_tags(path);
                    }
                    self.bulk_report = Some(combined);
                }
                if ui.add(egui::Button::new("Clear").small()).clicked() {
                    self.bulk_description.clear();
                    self.bulk_artist.clear();
                    self.bulk_copyright.clear();
                    self.bulk_tags = metadata_edit::TagStamp::default();
                    self.bulk_keyword.clear();
                    self.bulk_keyword_remove.clear();
                    self.bulk_rating_enabled = false;
                    self.bulk_label_enabled = false;
                    self.bulk_flag_enabled = false;
                    self.bulk_report = None;
                }
            });

            if let Some(report) = &self.bulk_report {
                if report.failures.is_empty() {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!("{} file(s) saved.", report.succeeded))
                                .small(),
                        )
                        .selectable(false),
                    );
                } else {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!(
                                "{} saved, {} failed",
                                report.succeeded,
                                report.failures.len()
                            ))
                            .small()
                            .color(ui.visuals().error_fg_color),
                        )
                        .selectable(false),
                    );
                    egui::CollapsingHeader::new("Failed files").show(ui, |ui| {
                        for failure in &report.failures {
                            ui.weak(format!("{}: {}", failure.path.display(), failure.error));
                        }
                    });
                }
            }

            ui.separator();
            for path in paths.iter().take(200) {
                ui.weak(
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string()),
                );
            }
        });
    }

    /// Preview-tier textures belong to the viewer and the metadata panel
    /// only; anything else (files the user moved past) is dropped so huge
    /// 2048 px RGBA images cannot pile up in GPU memory.
    fn prune_preview_textures(&mut self) {
        let viewer_path = self.viewer.path.clone();
        let panel_path = self.single_selection().and_then(|idx| {
            self.browser
                .entries
                .get(idx)
                .filter(|e| !e.is_dir)
                .map(|e| e.path.clone())
        });
        self.textures.retain(|(path, edge), _| {
            *edge != PREVIEW_EDGE
                || viewer_path.as_deref() == Some(path.as_path())
                || panel_path.as_deref() == Some(path.as_path())
        });
    }

    // ------------------------------------------------------------- viewer --

    /// (mtime_ns, size) for a browsed entry, used to request viewer tiers.
    fn entry_meta(&self, path: &Path) -> Option<(i64, u64)> {
        self.browser
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .map(|entry| (mtime_ns(entry.modified), entry.size))
    }

    fn open_viewer(&mut self, path: &Path) {
        if path.is_dir() || !bc_thumbnails::decoder::can_thumbnail(path) {
            self.status = format!("No preview available for {}", path.display());
            return;
        }
        let files: Vec<PathBuf> = self
            .display_indices()
            .iter()
            .filter_map(|&idx| self.browser.entries.get(idx))
            .filter(|entry| !entry.is_dir)
            .map(|entry| entry.path.clone())
            .collect();
        self.viewer.open(path.to_path_buf(), files);
        self.viewer.native = bc_thumbnails::decoder::image_dimensions(path);
        self.request_preview(path);
        self.status =
            "Preview · Left/Right browse · +/- or Ctrl+scroll zoom · 0 fit · 1 100% · drag pan · Esc close"
                .into();
    }

    fn close_viewer(&mut self) {
        if let Some(path) = self.viewer.path.take() {
            self.forget_viewer_textures(&path);
        }
        self.viewer.close();
    }

    /// Drop the uncached/preview-tier textures of a file the viewer left, so
    /// multi-hundred-megabyte full decodes do not pile up in GPU memory.
    fn forget_viewer_textures(&mut self, path: &Path) {
        let keep = |key: &(PathBuf, u32)| key.0 != path || key.1 == THUMB_MAX_EDGE;
        self.textures.retain(|key, _| keep(key));
        self.failed.retain(|key| keep(key));
    }

    fn viewer_step(&mut self, delta: isize) {
        let previous = self.viewer.path.clone();
        let Some(next) = self.viewer.step(delta) else {
            return;
        };
        if let Some(previous) = &previous {
            self.forget_viewer_textures(previous);
        }
        self.viewer.native = bc_thumbnails::decoder::image_dimensions(&next);
        if let Some(idx) = self
            .browser
            .entries
            .iter()
            .position(|entry| entry.path == next)
        {
            self.browser.selection.select_only(idx);
        }
        self.request_preview(&next);
        self.status = format!("Previewing {}", next.display());
    }

    fn request_preview(&mut self, path: &Path) {
        if let Some((mtime, size)) = self.entry_meta(path) {
            self.request_image(path, mtime, size, PREVIEW_EDGE, true);
        }
    }

    /// 100% zoom: show at actual size, upgrading to the uncached full tier
    /// when the preview is a downscale and the file is not pathologically big.
    fn go_actual_size(&mut self) {
        self.viewer.set_actual();
        let Some(path) = self.viewer.path.clone() else {
            return;
        };
        if self.viewer.full_requested {
            return;
        }
        self.viewer.full_requested = true;
        let native = self.viewer.native;
        if let Some((width, height)) = native {
            if width.max(height) <= PREVIEW_EDGE {
                // The preview tier already holds native resolution.
                return;
            }
        }
        if !viewer::full_allowed(native) {
            let dims = native.map_or_else(|| "dimensions".to_owned(), |(w, h)| format!("{w}×{h}"));
            self.viewer.note = format!(
                "Full resolution skipped for {dims}; preview shown at 100% ({:.0} MP cap)",
                viewer::MAX_FULL_PIXELS as f64 / 1e6
            );
            return;
        }
        if let Some((mtime, size)) = self.entry_meta(&path) {
            self.request_image(&path, mtime, size, FULL_EDGE, false);
        }
    }

    /// Draw the preview/zoom viewer in place of the content panel.
    fn viewer_view(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(path) = self.viewer.path.clone() else {
            return;
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());

        ui.horizontal(|ui| {
            if ui
                .button("<")
                .on_hover_text("Previous file (Left arrow)")
                .clicked()
            {
                self.viewer_step(-1);
            }
            if ui
                .button(">")
                .on_hover_text("Next file (Right arrow)")
                .clicked()
            {
                self.viewer_step(1);
            }
            ui.separator();
            if ui.button("-").on_hover_text("Zoom out (Minus)").clicked() {
                self.viewer.zoom_step(0.8);
            }
            if ui.button("+").on_hover_text("Zoom in (Plus)").clicked() {
                self.viewer.zoom_step(1.25);
            }
            ui.label(format!("{:.0}%", self.viewer.displayed * 100.0));
            if ui.button("Fit").on_hover_text("Fit window (0)").clicked() {
                self.viewer.set_fit();
            }
            if ui
                .button("100%")
                .on_hover_text("Actual pixels, full-resolution decode (1)")
                .clicked()
            {
                self.go_actual_size();
            }
            ui.separator();
            ui.label(egui::RichText::new(&name).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Close").on_hover_text("Esc").clicked() {
                    self.close_viewer();
                }
            });
        });

        // Info row: shown resolution, native size, full-tier progress/notes.
        let prev_key = (path.clone(), PREVIEW_EDGE);
        let full_key = (path.clone(), FULL_EDGE);
        ui.horizontal(|ui| {
            if let Some((w, h)) = self.viewer.native {
                ui.weak(format!("native {w}×{h}"));
            }
            if self.pending.contains(&full_key) {
                ui.weak("decoding full resolution…");
            } else if self.failed.contains(&full_key) {
                ui.weak("full-resolution decode failed");
            }
            if !self.viewer.note.is_empty() {
                ui.weak(self.viewer.note.clone());
            }
        });
        ui.separator();

        // Re-request the preview if its texture vanished (folder refresh,
        // earlier failure cleared) so the viewer never sticks on "loading".
        if !self.textures.contains_key(&prev_key)
            && !self.pending.contains(&prev_key)
            && !self.failed.contains(&prev_key)
        {
            self.request_preview(&path);
        }

        let (rect, response) =
            ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
        let texture = self
            .textures
            .get(&full_key)
            .or_else(|| self.textures.get(&prev_key))
            .cloned();

        match &texture {
            Some(tex) => {
                let size = tex.size_vec2();
                let fit = viewer::fit_scale(
                    (size.x.max(1.0) as u32, size.y.max(1.0) as u32),
                    (rect.width(), rect.height()),
                );
                let scale = match self.viewer.zoom {
                    ZoomMode::Fit => fit,
                    ZoomMode::Manual(zoom) => viewer::clamp_zoom(zoom),
                };
                self.viewer.displayed = scale;
                if response.dragged() {
                    self.viewer.offset += response.drag_delta();
                }
                if response.double_clicked() {
                    self.viewer.set_fit();
                }
                let draw_rect =
                    egui::Rect::from_center_size(rect.center() + self.viewer.offset, size * scale);
                ui.painter().image(
                    tex.id(),
                    draw_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            None => {
                let message = if self.failed.contains(&prev_key) {
                    "Preview failed to decode"
                } else {
                    "Loading preview…"
                };
                ui.allocate_ui_at_rect(rect, |ui| {
                    ui.centered_and_justified(|ui| ui.weak(message));
                });
            }
        }

        // Ctrl+scroll / pinch to zoom while the pointer is over the image.
        if response.hovered() {
            let delta = ctx.input(|input| input.zoom_delta());
            if (delta - 1.0).abs() > f32::EPSILON {
                self.viewer.zoom_step(delta);
            }
        }
    }

    // ------------------------------------------------------------ content --

    fn content_grid(&mut self, ui: &mut egui::Ui) {
        // Keep all layout dimensions positive: a collapsed/zero-width UI should
        // still show one column rather than divide by zero or skip the grid.
        let cell_width = if self.cell_size.is_finite() && self.cell_size > 0.0 {
            self.cell_size
        } else {
            DEFAULT_CELL
        };
        let row_height = if cell_width.is_finite() && cell_width >= MIN_CELL {
            cell_width + 44.0
        } else {
            cell_width + 32.0
        };
        let spacing = if GRID_SPACING.is_finite() && GRID_SPACING > 0.0 {
            GRID_SPACING
        } else {
            0.0
        };
        let available_width = ui.available_width().max(0.0);
        let cols = (((available_width + spacing) / (cell_width + spacing)).floor() as usize).max(1);
        let indices = self.display_indices();
        let row_step = row_height + spacing;
        let row_count = indices.len().div_ceil(cols);

        egui::ScrollArea::vertical().show_viewport(ui, |ui, viewport| {
            // ScrollArea reports the viewport in content coordinates. Use its
            // vertical bounds to lay out only rows that can be seen; spacers
            // retain the full scroll range without creating off-screen cells.
            let first_row = ((viewport.min.y.max(0.0) / row_step).floor() as usize).min(row_count);
            let end_row = ((viewport.max.y.max(0.0) / row_step).ceil() as usize).min(row_count);
            let end_row = end_row.max(first_row);
            ui.add_space(first_row as f32 * row_step);

            for row in first_row..end_row {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = spacing;
                    let row_start = row * cols;
                    let row_end = (row_start + cols).min(indices.len());
                    for &idx in &indices[row_start..row_end] {
                        let Some(entry) = self.browser.entries.get(idx) else {
                            continue;
                        };
                        let path = entry.path.clone();
                        let is_dir = entry.is_dir;
                        let size = entry.size;
                        let modified = entry.modified;
                        let selected = self.browser.selection.contains(idx);
                        // Tags are read for the cells on screen only, through
                        // the panel's cache, so a rating set in the panel shows
                        // up in the grid on the next frame.
                        let tags = if is_dir {
                            metadata_edit::TagState::default()
                        } else {
                            self.tags_for(&path, mtime_ns(modified))
                        };
                        let cell = ui.allocate_ui_with_layout(
                            egui::vec2(cell_width, row_height),
                            egui::Layout::top_down(egui::Align::Center),
                            |cell_ui| {
                                self.cell(
                                    cell_ui, idx, &path, is_dir, size, selected, cell_width, &tags,
                                )
                            },
                        );
                        // One interaction widget per cell: `cell()` allocates the
                        // thumbnail box with click-and-drag sense, and everything
                        // below (select, open, drag, drop) happens on it, exactly
                        // as the list rows do. Two alternatives were tried and
                        // rejected on evidence: the outer response from
                        // `allocate_ui_with_layout` never reported the pointer for
                        // the first cell of a row, so that cell could not be a drop
                        // target, and a second overlapping surface with drag sense
                        // took the presses away from the cell, turning a plain click
                        // on a folder into a drop onto itself.
                        let response = cell
                            .inner
                            // Every cell is draggable, so say so under the
                            // pointer: egui sets the grabbing cursor once a drag
                            // is in flight and this covers the press.
                            .on_hover_cursor(egui::CursorIcon::Grab);

                        if response.secondary_clicked() && !self.browser.selection.contains(idx) {
                            self.browser.selection.select_only(idx);
                        }
                        response.context_menu(|ui| self.selection_context_menu(ui, &path));
                        if response.clicked() {
                            self.list_focused = true;
                            let mods = ui.input(|i| i.modifiers);
                            if mods.shift {
                                self.browser.selection.select_range(idx);
                            } else if mods.ctrl || mods.command {
                                self.browser.selection.toggle(idx);
                            } else {
                                self.browser.selection.select_only(idx);
                            }
                        }
                        if response.double_clicked() {
                            if is_dir {
                                self.open_root(&path);
                            } else {
                                self.open_viewer(&path);
                            }
                        }
                        // Dragging an item inside the selection drags the whole
                        // selection; dragging an unselected item selects and
                        // drags just that item.
                        if response.drag_started() {
                            let paths = if self.browser.selection.contains(idx) {
                                self.selected_paths(&path)
                            } else {
                                self.browser.selection.select_only(idx);
                                vec![path.clone()]
                            };
                            response.dnd_set_drag_payload(paths);
                        }
                        // Drop target: folders accept the payload, and a folder
                        // under the pointer is the landing spot for files
                        // dropped in from the desktop. `contains_pointer` rather
                        // than `hovered`, because egui reports no widget as
                        // hovered while another one is being dragged.
                        if is_dir {
                            if response.contains_pointer() {
                                self.hovered_folder = Some(path.clone());
                            }
                            if response.dnd_hover_payload::<Vec<PathBuf>>().is_some() {
                                // Selection draws a 2px ring 3px outside this
                                // rect; the target ring is 3px and 6px out, so
                                // a folder that is both selected and being
                                // dropped onto cannot be mistaken for either.
                                ui.painter().rect_stroke(
                                    response.rect.expand(6.0),
                                    8.0,
                                    egui::Stroke::new(3.0_f32, ACCENT),
                                );
                            }
                            if let Some(payload) = response.dnd_release_payload::<Vec<PathBuf>>() {
                                let mode = drop_mode(ui.input(|i| i.modifiers.alt));
                                self.status = apply_transfer(&payload, &path, mode);
                                self.refresh_after_drop();
                            }
                        }
                        if !is_dir && bc_thumbnails::decoder::can_thumbnail(&path) {
                            self.request_thumbnail(&path, mtime_ns(modified), size);
                        }
                    }
                });
                // Include row spacing in the virtual extent so the next row's
                // origin matches the row-index calculation above.
                ui.add_space(spacing);
            }

            ui.add_space((row_count - end_row) as f32 * row_step);
        });
    }

    fn content_list(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, details: bool) {
        let visible = self.display_indices();
        if details {
            // The header shares `details_column_x` with the rows, so a column
            // title cannot drift away from its values.
            ui.horizontal(|ui| {
                let left = ui.cursor().left();
                // Rows are laid out inside the scroll area, which reserves the
                // scrollbar's width; measure the header the same way.
                let width = ui.available_width() - ui.spacing().scroll.bar_width;
                let y = ui.cursor().center().y;
                for (column, (title, _)) in details_columns().iter().enumerate() {
                    ui.painter().text(
                        egui::pos2(details_column_x(left, width, column), y),
                        egui::Align2::LEFT_CENTER,
                        *title,
                        egui::FontId::proportional(14.0),
                        ui.visuals().strong_text_color(),
                    );
                }
                ui.add_space(width);
            });
            ui.separator();
        }

        if self.list_focused && !ctx.wants_keyboard_input() {
            let (up, down, enter, shift) = ui.input(|input| {
                (
                    input.key_pressed(egui::Key::ArrowUp),
                    input.key_pressed(egui::Key::ArrowDown),
                    input.key_pressed(egui::Key::Enter),
                    input.modifiers.shift,
                )
            });
            let delta = if up {
                Some(-1)
            } else if down {
                Some(1)
            } else {
                None
            };
            if let Some(delta) = delta {
                if let Some(target) = navigation_target(&visible, &self.browser.selection, delta) {
                    if shift {
                        self.browser.selection.select_range(target);
                    } else {
                        self.browser.selection.select_only(target);
                    }
                }
            }
            if enter {
                let focused = self
                    .browser
                    .selection
                    .anchor()
                    .or_else(|| self.browser.selection.indices().next());
                if let Some(idx) = focused {
                    if let Some(entry) = self.browser.entries.get(idx) {
                        if !entry.is_dir {
                            let path = entry.path.clone();
                            self.open_viewer(&path);
                        }
                    }
                }
            }
        }

        egui::ScrollArea::vertical().show_rows(ui, LIST_ROW_HEIGHT, visible.len(), |ui, rows| {
            for row in rows {
                let idx = visible[row];
                let Some(entry) = self.browser.entries.get(idx) else {
                    continue;
                };
                let name = entry.name().to_string_lossy().into_owned();
                let is_dir = entry.is_dir;
                let size = entry.size;
                // Cloned up front: the tag read below needs `&mut self` for its
                // cache, and `entry` borrows the browser.
                let entry_path = entry.path.clone();
                let entry_modified = entry.modified;
                let modified = format_modified(entry.modified);
                let file_type = if is_dir {
                    "Folder".to_owned()
                } else {
                    entry
                        .path
                        .extension()
                        .map(|ext| ext.to_string_lossy().to_uppercase())
                        .filter(|ext| !ext.is_empty())
                        .unwrap_or_else(|| "File".to_owned())
                };
                let selected = self.browser.selection.contains(idx);
                let (rect, response) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), LIST_ROW_HEIGHT),
                    egui::Sense::click_and_drag(),
                );
                // Rows drag the selection too, so the pointer says so.
                let response = response.on_hover_cursor(egui::CursorIcon::Grab);
                if selected {
                    ui.painter()
                        .rect_filled(rect, 2.0, ui.visuals().selection.bg_fill);
                } else if row % 2 == 1 {
                    ui.painter()
                        .rect_filled(rect, 0.0, ui.visuals().faint_bg_color);
                }
                let color = if selected {
                    ui.visuals().selection.stroke.color
                } else {
                    ui.visuals().text_color()
                };
                if details {
                    let tags = if is_dir {
                        metadata_edit::TagState::default()
                    } else {
                        self.tags_for(&entry_path, mtime_ns(entry_modified))
                    };
                    // Six columns, ordered by what decides a photographer's
                    // next move: what it is, how big, when, then the two
                    // judgements the user made about it.
                    let values = [
                        format!("{}  {name}", if is_dir { "📁" } else { "·" }),
                        file_type,
                        if is_dir {
                            String::new()
                        } else {
                            human_size(size)
                        },
                        modified,
                        if tags.stars > 0 {
                            format!("{}", tags.stars)
                        } else {
                            String::new()
                        },
                        tags.label.clone(),
                    ];
                    for (column, value) in values.iter().enumerate() {
                        let column_x = details_column_x(rect.left(), rect.width(), column);
                        if column == 5 && !value.is_empty() {
                            // The label column carries its chip color, so the
                            // name is not the only thing telling you which
                            // label is on the file.
                            ui.painter().circle_filled(
                                egui::pos2(column_x + 4.0, rect.center().y),
                                3.6,
                                metadata_edit::label_color(value),
                            );
                        }
                        ui.painter().text(
                            egui::pos2(
                                if column == 5 {
                                    column_x + 11.0
                                } else {
                                    column_x
                                },
                                rect.center().y,
                            ),
                            egui::Align2::LEFT_CENTER,
                            value,
                            egui::FontId::proportional(13.0),
                            color,
                        );
                    }
                } else {
                    // Plain list: no room for columns, so the label rides on
                    // the right edge as its chip color and the rating is left
                    // to the details view rather than competing with the name.
                    let tags = if is_dir {
                        metadata_edit::TagState::default()
                    } else {
                        self.tags_for(&entry_path, mtime_ns(entry_modified))
                    };
                    ui.painter().text(
                        egui::pos2(rect.left() + 8.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        format!("{}  {name}", if is_dir { "📁" } else { "·" }),
                        egui::FontId::proportional(14.0),
                        color,
                    );
                    if !tags.label.is_empty() {
                        let center = egui::pos2(rect.right() - 12.0, rect.center().y);
                        ui.painter().circle_filled(
                            center,
                            4.6,
                            egui::Color32::from_black_alpha(190),
                        );
                        ui.painter().circle_filled(
                            center,
                            3.6,
                            metadata_edit::label_color(&tags.label),
                        );
                    }
                }
                if response.secondary_clicked() && !self.browser.selection.contains(idx) {
                    self.browser.selection.select_only(idx);
                }
                let context_path = self.browser.entries[idx].path.clone();
                response.context_menu(|ui| self.selection_context_menu(ui, &context_path));
                if response.clicked() {
                    self.list_focused = true;
                    let mods = ui.input(|input| input.modifiers);
                    if mods.shift {
                        self.browser.selection.select_range(idx);
                    } else if mods.ctrl || mods.command {
                        self.browser.selection.toggle(idx);
                    } else {
                        self.browser.selection.select_only(idx);
                    }
                }
                if response.double_clicked() {
                    let path = self.browser.entries[idx].path.clone();
                    if is_dir {
                        self.open_root(&path);
                    } else {
                        self.open_viewer(&path);
                    }
                }
                // Drag source and folder target, mirroring the thumbnail grid.
                if response.drag_started() {
                    let paths = if self.browser.selection.contains(idx) {
                        self.selected_paths(&context_path)
                    } else {
                        self.browser.selection.select_only(idx);
                        vec![context_path.clone()]
                    };
                    response.dnd_set_drag_payload(paths);
                }
                if is_dir {
                    if response.contains_pointer() {
                        self.hovered_folder = Some(context_path.clone());
                    }
                    if response.dnd_hover_payload::<Vec<PathBuf>>().is_some() {
                        ui.painter()
                            .rect_stroke(rect, 2.0, egui::Stroke::new(3.0_f32, ACCENT));
                    }
                    if let Some(payload) = response.dnd_release_payload::<Vec<PathBuf>>() {
                        let mode = drop_mode(ui.input(|i| i.modifiers.alt));
                        self.status = apply_transfer(&payload, &context_path, mode);
                        self.refresh_after_drop();
                    }
                }
            }
        });
    }

    fn selected_paths(&self, clicked_path: &Path) -> Vec<PathBuf> {
        let paths: Vec<_> = self
            .browser
            .entries
            .iter()
            .enumerate()
            .filter(|(idx, _)| self.browser.selection.contains(*idx))
            .map(|(_, entry)| entry.path.clone())
            .collect();
        if paths.is_empty() {
            vec![clicked_path.to_path_buf()]
        } else {
            paths
        }
    }

    fn select_all_visible(&mut self) {
        // Select only currently displayed entries so Select All respects
        // the active filter and the thumbnails-only toggle.
        let visible = self.display_indices();
        self.browser.selection.clear();
        for idx in visible {
            self.browser.selection.toggle(idx);
        }
    }

    fn confirm_delete_window(&mut self, ctx: &egui::Context) {
        let Some(paths) = self.pending_delete.clone() else {
            return;
        };
        let mut confirm = false;
        let mut cancel = false;
        egui::Window::new("Confirm permanent deletion")
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(format!(
                    "Permanently delete {} item(s)? This cannot be undone.",
                    paths.len()
                ));
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                    if ui.button("Delete permanently").clicked() {
                        confirm = true;
                    }
                });
            });
        if confirm {
            let mut done = 0;
            for path in &paths {
                match delete_path(path) {
                    Ok(()) => done += 1,
                    Err(e) => self.status = format!("Could not delete {}: {e}", path.display()),
                }
            }
            if done > 0 {
                self.status = format!("Permanently deleted {done} item(s)");
            }
            self.pending_delete = None;
        } else if cancel {
            self.pending_delete = None;
        }
    }

    fn selection_context_menu(&mut self, ui: &mut egui::Ui, clicked_path: &Path) {
        let paths = self.selected_paths(clicked_path);
        let context_actions: Vec<_> = self
            .plugin_manager
            .plugins_with_capability(PluginCapability::ContextMenu)
            .into_iter()
            .flat_map(|plugin| {
                plugin
                    .manifest
                    .entry_points
                    .context_menu
                    .iter()
                    .map(move |action| {
                        (
                            plugin.id.clone(),
                            action.id.clone(),
                            action.label.clone(),
                            action.function.clone(),
                        )
                    })
            })
            .collect();
        ui.label(format!("{} selected", paths.len()));
        for (plugin_id, action_id, label, function) in context_actions {
            if ui.button(label).clicked() {
                let host = UiPluginHost::from_app(self);
                self.status = match self.plugin_manager.run_wasm(
                    &plugin_id,
                    function.as_deref().or(Some(action_id.as_str())),
                    &host,
                ) {
                    Ok(run) => format!(
                        "Plugin completed in {:?}: {}",
                        run.elapsed,
                        run.output.join(" · ")
                    ),
                    Err(error) => format!("Plugin failed: {error}"),
                };
                ui.close_menu();
            }
        }
        let batch_hooks: Vec<_> = self
            .plugin_manager
            .plugins_with_capability(PluginCapability::BatchHook)
            .into_iter()
            .flat_map(|plugin| {
                plugin
                    .manifest
                    .entry_points
                    .batch_hooks
                    .iter()
                    .map(move |action| {
                        (
                            plugin.id.clone(),
                            action.id.clone(),
                            action.label.clone(),
                            action.function.clone(),
                        )
                    })
            })
            .collect();
        if paths.len() > 1 {
            for (plugin_id, action_id, label, function) in batch_hooks {
                if ui.button(format!("Batch: {label}")).clicked() {
                    let host = UiPluginHost::from_app(self);
                    self.status = match self.plugin_manager.run_wasm(
                        &plugin_id,
                        function.as_deref().or(Some(action_id.as_str())),
                        &host,
                    ) {
                        Ok(run) => format!(
                            "Batch plugin completed in {:?}: {}",
                            run.elapsed,
                            run.output.join(" · ")
                        ),
                        Err(error) => format!("Batch plugin failed: {error}"),
                    };
                    ui.close_menu();
                }
            }
        }
        ui.separator();
        if paths.len() == 1 {
            if ui.button("Preview").clicked() {
                self.open_viewer(&paths[0]);
                ui.close_menu();
            }
            ui.separator();
        }
        ui.menu_button("Copy to folder…", |ui| {
            ui.text_edit_singleline(&mut self.copy_destination);
            if ui.button("Copy").clicked() {
                let folder = PathBuf::from(self.copy_destination.trim());
                if self.copy_destination.trim().is_empty() {
                    self.status = "Enter a destination folder for copy.".into();
                } else {
                    let mut done = 0;
                    for source in &paths {
                        let Some(name) = source.file_name() else {
                            self.status = format!(
                                "Cannot copy path without a file name: {}",
                                source.display()
                            );
                            continue;
                        };
                        let destination = folder.join(name);
                        match copy_path(source, &destination) {
                            Ok(()) => done += 1,
                            Err(e) => {
                                self.status = format!("Copy failed for {}: {e}", source.display())
                            }
                        }
                    }
                    if done > 0 {
                        self.status = format!("Copied {done} item(s) to {}", folder.display());
                    }
                }
                ui.close_menu();
            }
        });
        ui.menu_button("Move to folder…", |ui| {
            ui.text_edit_singleline(&mut self.move_destination);
            if ui.button("Move").clicked() {
                let folder = PathBuf::from(self.move_destination.trim());
                if self.move_destination.trim().is_empty() {
                    self.status = "Enter a destination folder for move.".into();
                } else {
                    let mut done = 0;
                    for source in &paths {
                        let Some(name) = source.file_name() else {
                            self.status = format!(
                                "Cannot move path without a file name: {}",
                                source.display()
                            );
                            continue;
                        };
                        let destination = folder.join(name);
                        match move_path(source, &destination) {
                            Ok(()) => done += 1,
                            Err(e) => {
                                self.status = format!("Move failed for {}: {e}", source.display())
                            }
                        }
                    }
                    if done > 0 {
                        self.status = format!("Moved {done} item(s) to {}", folder.display());
                    }
                }
                ui.close_menu();
            }
        });
        if !paths.is_empty() && ui.button("Batch rename…").clicked() {
            self.rename_dialog = Some(batch_rename_ui::RenameDialog::new(paths.clone()));
            ui.close_menu();
        }
        if let Ok(journal) = batch_rename_ui::journal_path() {
            let can_undo = journal.exists()
                && paths.iter().any(|selected| {
                    let Ok(contents) = std::fs::read_to_string(&journal) else {
                        return false;
                    };
                    let Ok(record) =
                        serde_json::from_str::<bc_core::batch_rename::UndoJournal>(&contents)
                    else {
                        return false;
                    };
                    record
                        .operations
                        .iter()
                        .any(|op| selected == Path::new(&op.from) || selected == Path::new(&op.to))
                });
            if ui
                .add_enabled(can_undo, egui::Button::new("Undo last batch rename"))
                .clicked()
            {
                match bc_core::batch_rename::undo(&journal) {
                    Ok(count) => {
                        self.status = format!("Restored {count} item(s) from the last rename.");
                        self.refresh_now();
                    }
                    Err(error) => self.status = format!("Rename undo failed: {error}"),
                }
                ui.close_menu();
            }
        }
        if paths.len() == 1 {
            ui.horizontal(|ui| {
                ui.label("Rename:");
                ui.text_edit_singleline(&mut self.rename_name);
                if ui.button("Apply").clicked() {
                    let name = self.rename_name.trim();
                    if name.is_empty() {
                        self.status = "Enter a new name.".into();
                    } else {
                        match rename_path(&paths[0], name) {
                            Ok(path) => self.status = format!("Renamed to {}", path.display()),
                            Err(e) => self.status = format!("Rename failed: {e}"),
                        }
                    }
                    ui.close_menu();
                }
            });
        }
        if ui
            .add_enabled(
                self.photocraft_bridge.executable().is_some(),
                egui::Button::new("Edit in PhotoCraft"),
            )
            .on_hover_text("Open the selected files directly in PhotoCraft")
            .clicked()
        {
            let files: Vec<PathBuf> = paths
                .iter()
                .filter(|path| !path.is_dir())
                .cloned()
                .collect();
            if files.is_empty() {
                self.status = "Select files to edit in PhotoCraft.".into();
            } else {
                match self.photocraft_bridge.edit(&files) {
                    Ok(()) => {
                        self.status = format!("Opened {} item(s) in PhotoCraft", files.len());
                    }
                    Err(error) => self.status = format!("Could not open PhotoCraft: {error}"),
                }
            }
            ui.close_menu();
        }
        if ui
            .add_enabled(
                self.lightcraft_bridge.executable().is_some(),
                egui::Button::new("Edit in LightCraft"),
            )
            .on_hover_text("Import the selected files into LightCraft's photo library")
            .clicked()
        {
            let files: Vec<PathBuf> = paths
                .iter()
                .filter(|path| !path.is_dir())
                .cloned()
                .collect();
            if files.is_empty() {
                self.status = "Select files to open in LightCraft.".into();
            } else {
                match self.lightcraft_bridge.edit(&files) {
                    Ok(()) => self.status = format!("Opened {} item(s) in LightCraft", files.len()),
                    Err(error) => self.status = format!("Could not open LightCraft: {error}"),
                }
            }
            ui.close_menu();
        }
        if ui
            .button("Export…")
            .on_hover_text("Batch resize/convert to JPEG, PNG or WebP")
            .clicked()
        {
            let files: Vec<PathBuf> = paths
                .iter()
                .filter(|path| !path.is_dir())
                .cloned()
                .collect();
            if files.is_empty() {
                self.status = "Select files to export.".into();
            } else {
                self.export_dialog = Some(ExportDialog::new(files));
            }
            ui.close_menu();
        }
        if ui.button("Move to BridgeCraft Trash").clicked() {
            match TrashBin::open_default() {
                Ok(mut bin) => {
                    let mut done = 0;
                    for path in &paths {
                        match bin.trash(path) {
                            Ok(_) => done += 1,
                            Err(e) => {
                                self.status = format!("Could not trash {}: {e}", path.display())
                            }
                        }
                    }
                    if done > 0 {
                        self.status = format!("Moved {done} item(s) to BridgeCraft Trash");
                    }
                }
                Err(e) => self.status = format!("Could not open BridgeCraft Trash: {e}"),
            }
            ui.close_menu();
        }
        if ui.button("Delete permanently…").clicked() {
            self.pending_delete = Some(paths.clone());
            ui.close_menu();
        }
    }

    fn trash_window(&mut self, ctx: &egui::Context) {
        if !self.trash_window {
            return;
        }
        // Local flag so the window's close button can clear the shared field
        // without borrowing `self` through the builder.
        let mut open = true;
        egui::Window::new("BridgeCraft Trash")
            .resizable(true)
            .default_size([460.0, 300.0])
            .open(&mut open)
            .show(ctx, |ui| match TrashBin::open_default() {
                Ok(mut bin) => {
                    let entries = bin.list();
                    if entries.is_empty() {
                        ui.weak("Trash is empty.");
                    } else {
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            for entry in entries {
                                ui.horizontal(|ui| {
                                    ui.label(entry.original_path.display().to_string());
                                    if ui.button("Restore").clicked() {
                                        match bin.restore(&entry.id) {
                                            Ok(path) => {
                                                self.status = format!("Restored {}", path.display())
                                            }
                                            Err(e) => {
                                                self.status = format!("Could not restore item: {e}")
                                            }
                                        }
                                    }
                                });
                            }
                        });
                    }
                }
                Err(e) => {
                    ui.label(format!("Trash unavailable: {e}"));
                }
            });
        self.trash_window = open;
    }

    // ------------------------------------------------------------- export --

    /// Refresh job counters, announce freshly finished jobs once, and keep
    /// the frame clock running while work is in flight.
    fn poll_jobs(&mut self, ctx: &egui::Context) {
        let snapshots = self.job_queue.snapshot();
        self.jobs_active = snapshots.iter().filter(|job| !job.finished).count();
        for job in snapshots.iter().filter(|job| job.finished) {
            if self.jobs_notified.insert(job.id) {
                self.status = match &job.kind {
                    JobKind::Index { root } => format!(
                        "Indexed {}: {} read, {} already current, {} stale rows removed, {} failed",
                        root.display(),
                        job.completed,
                        job.skipped,
                        job.pruned,
                        job.failed
                    ),
                    JobKind::Export { .. } => format!(
                        "Export job #{} finished: {} done, {} failed, {} cancelled",
                        job.id, job.completed, job.failed, job.cancelled
                    ),
                };
            }
        }
        // The folder pass is the one job the status bar follows by id, so that
        // its row can clear itself when it ends rather than linger as a stale
        // count from a cancelled pass.
        self.index_progress = self.index_job.and_then(|id| {
            snapshots
                .iter()
                .find(|job| job.id == id)
                .filter(|job| !job.finished)
                .map(|job| {
                    (
                        job.completed + job.skipped + job.failed + job.cancelled,
                        job.total,
                    )
                })
        });
        if let Some(id) = self.index_job {
            if snapshots.iter().any(|job| job.id == id && job.finished) {
                self.index_job = None;
                // The pass that just ended wrote rows nobody had read into the
                // grid yet. Re-reading them is one query, and without it a
                // first visit to a folder would keep sorting as unrated until
                // the user re-opened it.
                if let Some(root) = self.browser.root.clone() {
                    self.load_folder_tags(&root);
                }
            }
        }
        if self.jobs_active > 0 {
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
        }
    }

    fn export_window(&mut self, ctx: &egui::Context) {
        // Move the dialog out so the window closure can mutably borrow it
        // without aliasing `self` (same pattern as the trash window).
        let Some(mut dialog) = self.export_dialog.take() else {
            return;
        };
        let mut open = true;
        let mut start = false;
        let mut dismissed = false;
        egui::Window::new("Export selection")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(format!(
                    "Export {} file(s) as basename_<max edge>.<ext>; existing files are never overwritten.",
                    dialog.sources.len()
                ));
                ui.horizontal(|ui| {
                    ui.label("Into folder:");
                    ui.add_sized(
                        [280.0, 20.0],
                        egui::TextEdit::singleline(&mut dialog.destination)
                            .hint_text("empty = beside originals"),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("Format:");
                    egui::ComboBox::from_id_source("export_format")
                        .selected_text(match dialog.format_choice {
                            0 => "JPEG",
                            1 => "PNG",
                            _ => "WebP",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut dialog.format_choice, 0, "JPEG");
                            ui.selectable_value(&mut dialog.format_choice, 1, "PNG");
                            ui.selectable_value(&mut dialog.format_choice, 2, "WebP");
                        });
                    if dialog.format_choice == 0 {
                        ui.add(egui::Slider::new(&mut dialog.quality, 30..=100).text("Quality"));
                    }
                    ui.add(
                        egui::Slider::new(&mut dialog.max_edge, 64..=4096)
                            .step_by(16.0)
                            .text("Max edge px"),
                    );
                });
                if dialog.low_tier {
                    ui.weak(
                        "RAW, PDF, video and Illustrator/PSD sources export at thumbnail quality.",
                    );
                }
                ui.separator();
                ui.weak(if dialog.format_choice == 0 {
                    "Metadata policy: the rating, colour label, flag and keywords are copied \
                     into each exported JPEG as an XMP packet. Camera data (make, model, \
                     software, date taken) is not: an export is a re-encode, not the exposure. \
                     EXIF orientation is applied to the exported pixels."
                        .to_string()
                } else {
                    "Metadata policy: PNG and WebP have no XMP slot in BridgeCraft yet, so \
                     these exports carry no tags. Choose JPEG to keep rating, label, flag and \
                     keywords. EXIF orientation is still applied to the exported pixels."
                        .to_string()
                });
                ui.horizontal(|ui| {
                    if ui.button("Start job").clicked() {
                        start = true;
                    }
                    if ui.button("Cancel").clicked() {
                        dismissed = true;
                    }
                });
            });

        if start {
            let format = match dialog.format_choice {
                0 => ExportFormat::Jpeg {
                    quality: dialog.quality as u8,
                },
                1 => ExportFormat::Png,
                _ => ExportFormat::WebP,
            };
            let destination = dialog.destination.trim().to_string();
            let file_count = dialog.sources.len();
            let request = ExportRequest {
                sources: std::mem::take(&mut dialog.sources),
                destination: (!destination.is_empty()).then(|| PathBuf::from(destination)),
                format,
                max_edge: dialog.max_edge.max(0) as u32,
            };
            match self.job_queue.submit(request) {
                Ok(handle) => {
                    self.status =
                        format!("Export job #{} started ({file_count} file(s)).", handle.id);
                    self.jobs_window = true;
                }
                Err(e) => self.status = format!("Cannot start export: {e}"),
            }
        } else if open && !dismissed {
            self.export_dialog = Some(dialog);
        }
    }

    /// Preview, validate and apply a token-based rename batch. The dialog
    /// keeps preview and apply separate so a malformed/colliding plan cannot
    /// change the filesystem.
    fn rename_window(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.rename_dialog.take() else {
            return;
        };
        let mut open = true;
        let mut dismissed = false;
        let mut apply = false;
        egui::Window::new("Batch rename")
            .default_size([620.0, 460.0])
            .collapsible(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(format!("Rename {} selected item(s) with a pattern:", dialog.sources.len()));
                let response = ui.add_sized(
                    [ui.available_width(), 24.0],
                    egui::TextEdit::singleline(&mut dialog.pattern)
                        .hint_text("e.g. {date:%Y-%m-%d}_{name}_{seq:0>3}"),
                );
                if response.changed() {
                    dialog.rebuild_preview();
                }
                ui.weak("Tokens: {name}, {name:upper}, {name:lower}, {name:title}, {ext}, {date:%Y-%m-%d}, {seq:WIDTH} or {seq:START>WIDTH}, {keyword#N}. The original extension is kept automatically unless {ext} is used.");
                ui.separator();

                match &dialog.preview {
                    Ok(plan) => {
                        ui.horizontal(|ui| {
                            ui.strong(format!("Preview · {} item(s)", plan.len()));
                            let changed = plan.iter().filter(|item| item.source != item.target).count();
                            ui.weak(format!("{changed} rename(s)"));
                        });
                        egui::ScrollArea::vertical().max_height(270.0).show(ui, |ui| {
                            for item in plan {
                                ui.horizontal(|ui| {
                                    ui.label(batch_rename_ui::display_name(&item.source));
                                    ui.weak("→");
                                    ui.strong(batch_rename_ui::display_name(&item.target));
                                });
                            }
                        });
                        if plan.is_empty() {
                            ui.weak("Select one or more files to preview a rename.");
                        }
                    }
                    Err(error) => {
                        ui.colored_label(ui.visuals().error_fg_color, format!("Cannot preview: {error}"));
                    }
                }
                ui.separator();
                ui.horizontal(|ui| {
                    let can_apply = matches!(&dialog.preview, Ok(plan) if !plan.is_empty());
                    if ui.add_enabled(can_apply, egui::Button::new("Apply rename batch")).clicked() {
                        apply = true;
                    }
                    if ui.button("Cancel").clicked() {
                        dismissed = true;
                    }
                });
            });

        if apply {
            // Re-run the planner immediately before touching disk to catch a
            // destination created while the user was reviewing the preview.
            dialog.rebuild_preview();
            let result = dialog
                .preview
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|plan| {
                    let journal = batch_rename_ui::journal_path()?;
                    bc_core::batch_rename::apply(plan, &journal).map_err(|error| error.to_string())
                });
            match result {
                Ok(count) => {
                    self.status = format!(
                        "Renamed {count} item(s). Undo is available from the selection menu."
                    );
                    self.rename_dialog = None;
                    self.refresh_now();
                }
                Err(error) => {
                    self.status = format!("Batch rename failed: {error}");
                    if open && !dismissed {
                        self.rename_dialog = Some(dialog);
                    }
                }
            }
        } else if open && !dismissed {
            self.rename_dialog = Some(dialog);
        }
    }

    fn lightcraft_settings_window(&mut self, ctx: &egui::Context) {
        if !self.lightcraft_settings_window {
            return;
        }
        let mut open = self.lightcraft_settings_window;
        egui::Window::new("LightCraft integration")
            .open(&mut open)
            .default_width(520.0)
            .show(ctx, |ui| {
                ui.label("Select a LightCraft executable, AppImage, or install directory.");
                ui.horizontal(|ui| {
                    ui.label("Path:");
                    ui.text_edit_singleline(&mut self.lightcraft_executable_input);
                });
                ui.horizontal(|ui| {
                    if ui.button("Use path").clicked() {
                        let raw_path = self.lightcraft_executable_input.trim();
                        let result = if raw_path.is_empty() {
                            Err("Enter a LightCraft executable, AppImage, or install directory.".to_string())
                        } else {
                            self.lightcraft_bridge
                                .set_executable(PathBuf::from(raw_path))
                                .map_err(|error| error.to_string())
                        };
                        match result {
                            Ok(()) => self.status = "LightCraft executable configured.".into(),
                            Err(error) => self.status = format!("LightCraft path invalid: {error}"),
                        }
                    }
                    if ui.button("Discover").clicked() {
                        match self.lightcraft_bridge.discover() {
                            Some(path) => {
                                self.lightcraft_executable_input = path.display().to_string();
                                self.status = format!("Found LightCraft at {}", path.display());
                            }
                            None => self.status = "LightCraft was not found.".into(),
                        }
                    }
                });
                match self.lightcraft_bridge.executable() {
                    Some(path) => ui.label(format!("Configured: {}", path.display())),
                    None => ui.weak("No LightCraft executable configured."),
                };
                ui.weak("Selected files are imported into the LightCraft library; non-destructive edits and exports are managed there.");
            });
        self.lightcraft_settings_window = open;
    }

    fn photocraft_settings_window(&mut self, ctx: &egui::Context) {
        if !self.photocraft_settings_window {
            return;
        }
        let mut open = self.photocraft_settings_window;
        egui::Window::new("PhotoCraft integration")
            .open(&mut open)
            .default_width(520.0)
            .show(ctx, |ui| {
                ui.label("Select a PhotoCraft executable, AppImage, or install directory.");
                ui.horizontal(|ui| {
                    ui.label("Path:");
                    ui.text_edit_singleline(&mut self.photocraft_executable_input);
                });
                ui.horizontal(|ui| {
                    if ui.button("Use path").clicked() {
                        let raw_path = self.photocraft_executable_input.trim();
                        let path = if raw_path.is_empty() {
                            None
                        } else {
                            Some(PathBuf::from(raw_path))
                        };
                        let result = path.ok_or_else(|| {
                            "Enter a PhotoCraft executable, AppImage, or install directory.".to_string()
                        }).and_then(|path| self.photocraft_bridge.set_executable(path).map_err(|error| error.to_string()));
                        match result {
                            Ok(()) => self.status = "PhotoCraft executable configured.".into(),
                            Err(error) => self.status = format!("PhotoCraft path invalid: {error}"),
                        }
                    }
                    if ui.button("Discover").clicked() {
                        match self.photocraft_bridge.discover() {
                            Some(path) => {
                                self.photocraft_executable_input = path.display().to_string();
                                self.status = format!("Found PhotoCraft at {}", path.display());
                            }
                            None => self.status = "PhotoCraft was not found.".into(),
                        }
                    }
                });
                match self.photocraft_bridge.executable() {
                    Some(path) => ui.label(format!("Configured: {}", path.display())),
                    None => ui.weak("No PhotoCraft executable configured."),
                };
                ui.weak("Selected files are passed to PhotoCraft's documented desktop file-open interface.");
            });
        self.photocraft_settings_window = open;
    }

    fn plugin_manager_window(&mut self, ctx: &egui::Context) {
        if !self.plugin_manager_window {
            return;
        }
        let mut open = self.plugin_manager_window;
        let mut rediscover = false;
        egui::Window::new("Plugin manager")
            .open(&mut open)
            .default_width(600.0)
            .show(ctx, |ui| {
                ui.label("Local WASM plugins only. Modules receive explicit host imports and run without WASI.");
                if let Some(path) = self.plugin_manager.plugins_dir() {
                    ui.weak(format!("Plugins folder: {}", path.display()));
                }
                if ui.button("Rescan plugins").clicked() {
                    rediscover = true;
                }
                if let Some(error) = &self.plugin_manager_error {
                    ui.colored_label(egui::Color32::LIGHT_RED, error);
                }
                if self.plugin_manager.plugins().is_empty() {
                    ui.weak("No plugins found. Install a plugin directory containing plugin.json and its WASM module.");
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let ids: Vec<_> = self.plugin_manager.plugins().iter().map(|plugin| plugin.id.clone()).collect();
                    for id in ids {
                        let Some(plugin) = self.plugin_manager.find(&id) else { continue; };
                        let name = plugin.manifest.name.clone();
                        let version = plugin.manifest.version.clone();
                        let description = plugin.manifest.description.clone();
                        let enabled = plugin.enabled;
                        let requested_capabilities = plugin.manifest.capabilities.clone();
                        let requested_paths = plugin.manifest.permissions.filesystem_paths.clone();
                        let requested_network = plugin.manifest.permissions.network;
                        let grants = plugin.grants.clone();
                        let extension_count = plugin.manifest.entry_points.menu_actions.len()
                            + plugin.manifest.entry_points.context_menu.len()
                            + plugin.manifest.entry_points.batch_hooks.len()
                            + plugin.manifest.entry_points.panels.len();
                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                ui.strong(format!("{name} · {version}"));
                                let mut enabled = enabled;
                                if ui.checkbox(&mut enabled, "Enabled").changed() {
                                    if let Err(error) = self.plugin_manager.set_enabled(&id, enabled) {
                                        self.plugin_manager_error = Some(error.to_string());
                                    }
                                }
                            });
                            if !description.is_empty() { ui.label(description); }
                            ui.weak(format!("{extension_count} declared extension(s)"));
                            ui.label("Requested capabilities (grant explicitly):");
                            ui.horizontal_wrapped(|ui| {
                                for capability in &requested_capabilities {
                                    let mut granted = grants.capabilities.contains(capability);
                                    if ui.checkbox(&mut granted, format!("{capability:?}")).changed() {
                                        let mut updated = grants.clone();
                                        if granted { updated.capabilities.insert(*capability); }
                                        else { updated.capabilities.remove(capability); }
                                        if let Err(error) = self.plugin_manager.set_grants(&id, updated) {
                                            self.plugin_manager_error = Some(error.to_string());
                                        }
                                    }
                                }
                            });
                            if requested_paths.is_empty() {
                                ui.weak("Filesystem paths: none requested");
                            } else {
                                ui.label("Filesystem roots (grant explicitly):");
                                for path in requested_paths {
                                    let mut granted = grants.filesystem_paths.contains(&path);
                                    if ui.checkbox(&mut granted, path.display().to_string()).changed() {
                                        let mut updated = grants.clone();
                                        if granted { updated.filesystem_paths.push(path.clone()); }
                                        else { updated.filesystem_paths.retain(|existing| existing != &path); }
                                        if let Err(error) = self.plugin_manager.set_grants(&id, updated) {
                                            self.plugin_manager_error = Some(error.to_string());
                                        }
                                    }
                                }
                            }
                            if requested_network {
                                let mut granted = grants.network;
                                if ui.checkbox(&mut granted, "Grant network permission (no network host functions in v1)").changed() {
                                    let mut updated = grants.clone();
                                    updated.network = granted;
                                    if let Err(error) = self.plugin_manager.set_grants(&id, updated) {
                                        self.plugin_manager_error = Some(error.to_string());
                                    }
                                }
                            } else {
                                ui.weak("Network access is not requested and is denied by default.");
                            }
                            let panels: Vec<_> = self.plugin_manager.find(&id).into_iter()
                                .flat_map(|plugin| plugin.manifest.entry_points.panels.iter().map(|panel| (panel.id.clone(), panel.title.clone(), panel.description.clone())))
                                .collect();
                            for (panel_id, title, panel_description) in panels {
                                let panel_key = format!("{id}:{panel_id}");
                                ui.horizontal(|ui| {
                                    ui.label(format!("Panel: {title}"));
                                    let shown = self.plugin_panel_open.contains(&panel_key);
                                    if ui.button(if shown { "Hide" } else { "Show" }).clicked() {
                                        if shown { self.plugin_panel_open.remove(&panel_key); }
                                        else { self.plugin_panel_open.insert(panel_key.clone()); }
                                    }
                                });
                                if self.plugin_panel_open.contains(&panel_key) {
                                    ui.weak(if panel_description.is_empty() { "Plugin panel rendering is not available in v1." } else { &panel_description });
                                }
                            }
                        });
                    }
                });
            });
        self.plugin_manager_window = open;
        if rediscover {
            self.plugin_manager_error = self
                .plugin_manager
                .discover()
                .err()
                .map(|error| error.to_string());
        }
    }

    fn auto_process_window(&mut self, ctx: &egui::Context) {
        if !self.auto_window {
            return;
        }
        let mut open = true;
        let mut remove_rule = None;
        egui::Window::new("Watched folders")
            .default_size([560.0, 470.0])
            .open(&mut open)
            .show(ctx, |ui| {
                let Some(processor) = self.auto_processor.as_mut() else {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        "Auto-processing is unavailable; check the application log.",
                    );
                    return;
                };
                ui.label("Watch a folder and apply actions after incoming files stop changing.");
                let current_settle = processor.settle_duration().as_secs_f64();
                if self.auto_settle_seconds == "2" && current_settle != 2.0 {
                    self.auto_settle_seconds = format!("{current_settle}");
                }
                ui.horizontal(|ui| {
                    ui.label("Root:");
                    ui.add_sized(
                        [300.0, 20.0],
                        egui::TextEdit::singleline(&mut self.auto_watch_root)
                            .hint_text("/path/to/ingest"),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("Extensions:");
                    ui.add_sized(
                        [220.0, 20.0],
                        egui::TextEdit::singleline(&mut self.auto_extensions)
                            .hint_text("jpg,dng,nef (blank = any)"),
                    );
                    ui.label("Min bytes:");
                    ui.add_sized(
                        [80.0, 20.0],
                        egui::TextEdit::singleline(&mut self.auto_min_size),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("Settle seconds:");
                    ui.add_sized(
                        [60.0, 20.0],
                        egui::TextEdit::singleline(&mut self.auto_settle_seconds),
                    );
                    if ui.button("Settle delay").clicked() {
                        match self.auto_settle_seconds.parse::<u64>() {
                            Ok(seconds) => match processor.set_settle(Duration::from_secs(seconds))
                            {
                                Ok(()) => self.status = "Settle delay saved.".into(),
                                Err(error) => {
                                    self.status = format!("Could not save settle delay: {error}")
                                }
                            },
                            Err(error) => self.status = format!("Invalid settle delay: {error}"),
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.auto_action_thumbnail, "Thumbnail");
                    ui.checkbox(&mut self.auto_action_rename, "Rename");
                    ui.checkbox(&mut self.auto_action_export, "Export PNG");
                    ui.checkbox(&mut self.auto_action_dated_move, "Dated folder");
                });
                if self.auto_action_rename {
                    ui.horizontal(|ui| {
                        ui.label("Pattern:");
                        ui.add_sized(
                            [360.0, 20.0],
                            egui::TextEdit::singleline(&mut self.auto_pattern),
                        );
                    });
                }
                if ui.button("Add watched-folder rule").clicked() {
                    let root = PathBuf::from(self.auto_watch_root.trim());
                    let extensions = self
                        .auto_extensions
                        .split(',')
                        .map(str::trim)
                        .filter(|ext| !ext.is_empty())
                        .map(|ext| ext.trim_start_matches('.').to_ascii_lowercase())
                        .collect();
                    let min_size = self.auto_min_size.trim().parse::<u64>().unwrap_or(0);
                    let mut actions = Vec::new();
                    if self.auto_action_thumbnail {
                        actions.push(AutoAction::GenerateThumbnail { max_edge: 256 });
                    }
                    if self.auto_action_rename {
                        actions.push(AutoAction::Rename {
                            pattern: self.auto_pattern.clone(),
                        });
                    }
                    if self.auto_action_export {
                        actions.push(AutoAction::Export {
                            max_edge: 1024,
                            format: bc_core::auto_process::AutoExportFormat::Png,
                        });
                    }
                    if self.auto_action_dated_move {
                        actions.push(AutoAction::MoveToDatedFolder);
                    }
                    let id = format!(
                        "rule-{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis())
                            .unwrap_or(0)
                    );
                    let rule = AutoRule {
                        id,
                        name: format!(
                            "Watch {}",
                            root.file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_else(|| root.display().to_string())
                        ),
                        root,
                        enabled: true,
                        extensions,
                        min_size,
                        subfolder_pattern: None,
                        actions,
                        consecutive_failures: 0,
                        disabled_reason: None,
                    };
                    match processor.add_rule(rule) {
                        Ok(()) => self.status = "Watched-folder rule added.".into(),
                        Err(error) => self.status = format!("Could not add rule: {error}"),
                    }
                }
                ui.separator();
                ui.strong(format!("Queued actions: {}", processor.queued_count()));
                egui::ScrollArea::vertical()
                    .max_height(150.0)
                    .show(ui, |ui| {
                        for rule in processor.rules() {
                            ui.horizontal(|ui| {
                                let state = if rule.enabled { "Watching" } else { "Disabled" };
                                ui.strong(format!("{} · {state}", rule.name));
                                ui.weak(rule.root.display().to_string());
                                if ui
                                    .small_button(if rule.enabled { "Pause" } else { "Enable" })
                                    .clicked()
                                {
                                    match processor.set_rule_enabled(&rule.id, !rule.enabled) {
                                        Ok(_) => {}
                                        Err(error) => {
                                            self.status = format!("Could not update rule: {error}")
                                        }
                                    }
                                }
                                if ui.small_button("Remove").clicked() {
                                    remove_rule = Some(rule.id.clone());
                                }
                            });
                            if let Some(reason) = rule.disabled_reason {
                                ui.colored_label(ui.visuals().error_fg_color, reason);
                            }
                        }
                    });
                ui.separator();
                ui.strong("Recent auto-action history");
                egui::ScrollArea::vertical()
                    .max_height(130.0)
                    .show(ui, |ui| {
                        for item in processor.history().iter().rev().take(100) {
                            let status = match &item.result {
                                Ok(()) => "Done".to_string(),
                                Err(error) => format!("Failed: {error}"),
                            };
                            ui.horizontal(|ui| {
                                ui.label(&item.rule_name);
                                ui.weak(item.path.display().to_string());
                                ui.label(status);
                            });
                        }
                    });
            });
        if let Some(rule_id) = remove_rule {
            if let Some(processor) = self.auto_processor.as_mut() {
                match processor.remove_rule(&rule_id) {
                    Ok(true) => self.status = "Watched-folder rule removed.".into(),
                    Ok(false) => {}
                    Err(error) => self.status = format!("Could not remove rule: {error}"),
                }
            }
        }
        self.auto_window = open;
    }

    /// Non-modal job list with per-file progress rows and a done/failed
    /// summary — browsing stays interactive while exports run.
    fn jobs_window(&mut self, ctx: &egui::Context) {
        if !self.jobs_window {
            return;
        }
        let mut open = true;
        egui::Window::new("Jobs")
            .default_size([440.0, 340.0])
            .open(&mut open)
            .show(ctx, |ui| {
                let snapshots = self.job_queue.snapshot();
                if snapshots.is_empty() {
                    ui.weak(
                        "No jobs yet. Right-click a selection and choose Export…; opening a \
                         folder queues a metadata index pass for it.",
                    );
                    return;
                }
                ui.horizontal(|ui| {
                    if ui
                        .button("Remove finished")
                        .on_hover_text("Hide finished jobs from this list")
                        .clicked()
                    {
                        self.job_queue.clear_finished();
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for job in snapshots.iter().rev() {
                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                ui.strong(format!("Job #{} · {}", job.id, job.kind.heading()));
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if !job.finished && ui.button("Cancel job").clicked() {
                                            self.job_queue.cancel(job.id);
                                        }
                                    },
                                );
                            });
                            ui.weak(format!("→ {}", job.kind.target()));
                            let done = job.completed + job.skipped + job.failed + job.cancelled;
                            ui.add(
                                egui::ProgressBar::new(done as f32 / job.total.max(1) as f32)
                                    .text(format!("{done}/{}", job.total)),
                            );
                            let mut summary = format!(
                                "{} done · {} failed · {} cancelled · {} queued · {} running",
                                job.completed, job.failed, job.cancelled, job.queued, job.running
                            );
                            if job.skipped > 0 {
                                summary = format!("{} skipped · {summary}", job.skipped);
                            }
                            if job.pruned > 0 {
                                summary = format!("{summary} · {} stale rows removed", job.pruned);
                            }
                            ui.weak(summary);
                            let indexing = matches!(job.kind, JobKind::Index { .. });
                            let running_text = if indexing {
                                "indexing…"
                            } else {
                                "exporting…"
                            };
                            egui::ScrollArea::vertical()
                                .max_height(150.0)
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    for file in &job.files {
                                        let name = file
                                            .source
                                            .file_name()
                                            .map(|n| n.to_string_lossy().into_owned())
                                            .unwrap_or_else(|| file.source.display().to_string());
                                        match &file.state {
                                            bc_core::jobs::FileState::Queued => {
                                                ui.weak(format!("· {name}"));
                                            }
                                            bc_core::jobs::FileState::Running => {
                                                ui.label(format!("▶ {name} — {running_text}"));
                                            }
                                            bc_core::jobs::FileState::Done { output } => {
                                                if indexing {
                                                    // An index pass writes a row,
                                                    // not a file, so the only name
                                                    // worth showing is the photo's.
                                                    ui.colored_label(
                                                        egui::Color32::from_rgb(120, 200, 120),
                                                        format!("✓ {name} indexed"),
                                                    )
                                                    .on_hover_text(output.display().to_string());
                                                } else {
                                                    let out_name = output
                                                        .file_name()
                                                        .map(|n| n.to_string_lossy().into_owned())
                                                        .unwrap_or_default();
                                                    ui.colored_label(
                                                        egui::Color32::from_rgb(120, 200, 120),
                                                        format!("✓ {name} → {out_name}"),
                                                    )
                                                    .on_hover_text(output.display().to_string());
                                                }
                                            }
                                            bc_core::jobs::FileState::Skipped { reason } => {
                                                ui.weak(format!("– {name}: {reason}"))
                                                    .on_hover_text(reason);
                                            }
                                            bc_core::jobs::FileState::Failed { message } => {
                                                ui.colored_label(
                                                    egui::Color32::from_rgb(220, 120, 120),
                                                    format!("✗ {name}: {message}"),
                                                );
                                            }
                                            bc_core::jobs::FileState::Cancelled => {
                                                ui.colored_label(
                                                    egui::Color32::GRAY,
                                                    format!("⊘ {name} cancelled"),
                                                );
                                            }
                                        }
                                    }
                                });
                        });
                        ui.add_space(4.0);
                    }
                });
            });
        self.jobs_window = open;
    }

    #[allow(clippy::too_many_arguments)]
    fn cell(
        &self,
        ui: &mut egui::Ui,
        idx: usize,
        path: &Path,
        is_dir: bool,
        size: u64,
        selected: bool,
        cell_size: f32,
        tags: &metadata_edit::TagState,
    ) -> egui::Response {
        let _ = idx;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());

        ui.vertical_centered(|ui| {
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2((cell_size - 12.0).max(1.0), (cell_size - 20.0).max(1.0)),
                egui::Sense::click_and_drag(),
            );

            if is_dir {
                ui.painter()
                    .rect_filled(rect, 6.0, egui::Color32::from_rgb(60, 60, 40));
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "📁",
                    egui::FontId::proportional(36.0),
                    egui::Color32::WHITE,
                );
            } else if let Some(tex) = self.textures.get(&(path.to_path_buf(), THUMB_MAX_EDGE)) {
                let tex_size = tex.size_vec2();
                let scale = (rect.size().x / tex_size.x).min(rect.size().y / tex_size.y);
                let draw_rect = egui::Rect::from_center_size(rect.center(), tex_size * scale);
                ui.painter().image(
                    tex.id(),
                    draw_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            } else {
                ui.painter()
                    .rect_filled(rect, 6.0, egui::Color32::from_rgb(45, 45, 45));
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "…",
                    egui::FontId::proportional(24.0),
                    egui::Color32::GRAY,
                );
            }

            if selected {
                ui.painter()
                    .rect_stroke(rect.expand(3.0), 6.0, egui::Stroke::new(2.0_f32, ACCENT));
            }

            // Tag overlays, in the corners that never fight the filename: the
            // label dot sits on the thumbnail, the rating under it. The flag
            // stays out of the grid, because a third mark on a cell this small
            // is decoration rather than information; it reads in the panel and
            // in the details column. A dark ring keeps the dot legible on a
            // bright thumbnail as well as on the dark placeholder.
            if !tags.label.is_empty() {
                let center = egui::pos2(rect.left() + 10.0, rect.top() + 10.0);
                ui.painter()
                    .circle_filled(center, 5.6, egui::Color32::from_black_alpha(190));
                ui.painter()
                    .circle_filled(center, 4.0, metadata_edit::label_color(&tags.label));
            }
            if tags.stars > 0 {
                tag_widgets::rating_inline(
                    ui,
                    egui::pos2(rect.left(), rect.bottom() + 2.0),
                    tags.stars,
                );
            }

            ui.set_min_width((cell_size - 16.0).max(1.0));
            ui.add_sized(
                [(cell_size - 16.0).max(1.0), 16.0],
                egui::Label::new(egui::RichText::new(&name).small()).truncate(true),
            );
            if !is_dir {
                ui.label(egui::RichText::new(human_size(size)).weak().small());
            }
            response
        })
        .inner
    }

    // ------------------------------------------------------------- chrome --

    /// Toolbar + breadcrumb rows above the content.
    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .button("Open…")
                    .on_hover_text("Open a folder (Ctrl+O)")
                    .clicked()
                {
                    self.open_field = !self.open_field;
                }
                let parent = self
                    .browser
                    .root
                    .as_ref()
                    .and_then(|r| r.parent())
                    .map(|p| p.to_path_buf());
                if ui
                    .button("Up")
                    .on_hover_text("Up to the parent folder")
                    .clicked()
                {
                    if let Some(parent) = parent {
                        self.open_root(&parent);
                    }
                }
                if ui.button("Refresh").on_hover_text("Refresh (F5)").clicked() {
                    self.refresh_now();
                }
                if ui
                    .button("Plugins…")
                    .on_hover_text("Manage local WASM plugins and permissions")
                    .clicked()
                {
                    self.plugin_manager_window = true;
                }
                if ui
                    .button("LightCraft…")
                    .on_hover_text("Configure LightCraft executable")
                    .clicked()
                {
                    self.lightcraft_settings_window = true;
                    if self.lightcraft_executable_input.is_empty() {
                        self.lightcraft_executable_input = self
                            .lightcraft_bridge
                            .executable()
                            .map(|path| path.display().to_string())
                            .unwrap_or_default();
                    }
                }
                if ui
                    .button("PhotoCraft…")
                    .on_hover_text("Configure PhotoCraft executable")
                    .clicked()
                {
                    self.photocraft_settings_window = true;
                    if self.photocraft_executable_input.is_empty() {
                        self.photocraft_executable_input = self
                            .photocraft_bridge
                            .executable()
                            .map(|path| path.display().to_string())
                            .unwrap_or_default();
                    }
                }
                let menu_actions: Vec<_> = self
                    .plugin_manager
                    .plugins_with_capability(PluginCapability::MenuAction)
                    .into_iter()
                    .flat_map(|plugin| {
                        plugin
                            .manifest
                            .entry_points
                            .menu_actions
                            .iter()
                            .map(move |action| {
                                (
                                    plugin.id.clone(),
                                    action.id.clone(),
                                    action.label.clone(),
                                    action.function.clone(),
                                )
                            })
                    })
                    .collect();
                if !menu_actions.is_empty() {
                    ui.menu_button("Plugins", |ui| {
                        for (plugin_id, action_id, label, function) in &menu_actions {
                            if ui.button(label).clicked() {
                                let host = UiPluginHost::from_app(self);
                                self.status = match self.plugin_manager.run_wasm(
                                    plugin_id,
                                    function.as_deref().or(Some(action_id.as_str())),
                                    &host,
                                ) {
                                    Ok(run) => format!(
                                        "Plugin completed in {:?}: {}",
                                        run.elapsed,
                                        run.output.join(" · ")
                                    ),
                                    Err(error) => format!("Plugin failed: {error}"),
                                };
                                ui.close_menu();
                            }
                        }
                    });
                }
                ui.separator();
                if ui.button("Select All").clicked() {
                    self.select_all_visible();
                }
                if ui.button("Clear").clicked() {
                    self.browser.selection.clear();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Sort keys live in `browser.sort_key`, but changing one
                    // has to re-sort. Setting the field alone left the grid in
                    // the previous key's order until the direction button was
                    // also pressed, which is how a tag sort key would have
                    // looked broken the moment it appeared in this menu.
                    let before_key = self.browser.sort_key;
                    egui::ComboBox::from_id_source("sort_key")
                        .selected_text(self.browser.sort_key.label())
                        .show_ui(ui, |ui| {
                            for key in SortKey::ALL {
                                ui.selectable_value(&mut self.browser.sort_key, key, key.label());
                            }
                        });
                    if self.browser.sort_key != before_key {
                        let key = self.browser.sort_key;
                        let dir = self.browser.sort_dir;
                        self.browser.set_sort(key, dir);
                        if key.needs_tags() {
                            self.status = self.tag_sort_status();
                        }
                    }
                    let (dir_label, dir_hover) = self.sort_direction_label();
                    if ui.button(dir_label).on_hover_text(dir_hover).clicked() {
                        let dir = match self.browser.sort_dir {
                            SortDir::Asc => SortDir::Desc,
                            SortDir::Desc => SortDir::Asc,
                        };
                        let key = self.browser.sort_key;
                        self.browser.set_sort(key, dir);
                        if key.needs_tags() {
                            self.status = self.tag_sort_status();
                        }
                    }
                    ui.label("Sort:");
                    ui.separator();
                    if self.tag_filter_button(ui) {
                        let shown = self.browser.set_tag_filter(self.browser.tag_filter.clone());
                        self.status = self.tag_filter_status(shown);
                    }
                    // Library search. Kept apart from the name filter on
                    // purpose: the filter narrows what is on screen from this
                    // folder, the search asks the index for rows that may come
                    // from anywhere in scope, and conflating them would make it
                    // impossible to say which one emptied the grid.
                    let before_text = self.search_text.clone();
                    let search_field = ui.add_sized(
                        [220.0, 20.0],
                        egui::TextEdit::singleline(&mut self.search_text)
                            .hint_text("search tags, names, cameras…"),
                    );
                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if search_field.changed()
                        || !before_text.is_empty() && self.search_text.is_empty()
                    {
                        if self.search_text.trim().is_empty() {
                            self.search_due = None;
                            self.clear_search();
                        } else {
                            // Debounced, because every keystroke is a table
                            // scan: ~70 ms per 100 000 rows is not something to
                            // spend on a half-typed word.
                            self.search_due = Some(Instant::now() + SEARCH_DEBOUNCE);
                        }
                    }
                    if enter && !self.search_text.trim().is_empty() {
                        self.search_due = None;
                        self.request_search();
                    }
                    if self.search_text.trim().is_empty() {
                        ui.label("Search:");
                    } else if ui.button("✕").on_hover_text("Clear the search").clicked() {
                        self.search_text.clear();
                        self.search_due = None;
                        self.clear_search();
                    }
                    // One question decides the control: is the grid showing a
                    // saved collection? If so its scope is the one in force, and
                    // the popup says so without offering choices that would
                    // change nothing.
                    let (scope_label, scope_editable) = match self.browser.search() {
                        Some((_, _, scope)) if self.browser.collection().is_some() => (
                            match scope {
                                bc_core::SearchScope::Library => "the whole library".to_string(),
                                bc_core::SearchScope::Folder(root) => {
                                    format!("saved scope: {}", root.display())
                                }
                                bc_core::SearchScope::FolderAndSubfolders(root) => {
                                    format!("saved scope: {} and below", root.display())
                                }
                            },
                            false,
                        ),
                        _ => (self.search_scope.label().to_string(), true),
                    };
                    let mut combo = egui::ComboBox::from_id_source("search_scope");
                    combo = combo.selected_text(&scope_label);
                    if scope_editable {
                        combo.show_ui(ui, |ui| {
                            for choice in SearchScopeChoice::ALL {
                                ui.selectable_value(&mut self.search_scope, choice, choice.label());
                            }
                        });
                    } else {
                        combo.show_ui(ui, |ui| {
                            ui.weak("A saved collection carries its own scope.");
                        });
                    }

                    let before = self.browser.filter.clone();
                    let resp = ui.add_sized(
                        [200.0, 20.0],
                        egui::TextEdit::singleline(&mut self.browser.filter)
                            .hint_text("filter by name…"),
                    );
                    if resp.changed() || self.browser.filter != before {
                        let shown = self.browser.set_filter(self.browser.filter.clone());
                        self.status = format!(
                            "Filter “{}”: {} of {} items",
                            self.browser.filter,
                            shown,
                            self.browser.len()
                        );
                    }
                });
            });
            if self.open_field {
                ui.horizontal(|ui| {
                    ui.label("Folder:");
                    let resp = ui.add_sized(
                        [(ui.available_width() - 120.0).max(160.0), 20.0],
                        egui::TextEdit::singleline(&mut self.path_input).hint_text("path…"),
                    );
                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (resp.lost_focus() && enter) || ui.button("Open").clicked() {
                        let path = self.path_input.clone();
                        self.open_root(Path::new(&path));
                    }
                    if ui.button("Close").clicked() {
                        self.open_field = false;
                    }
                });
            }
        });

        egui::TopBottomPanel::top("breadcrumbs").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| match &self.browser.root {
                None => {
                    ui.weak("No folder open — press Open… to pick one.");
                }
                Some(root) => {
                    let chain: Vec<PathBuf> = root.ancestors().map(Path::to_path_buf).collect();
                    let last = chain.len().saturating_sub(1);
                    for (i, ancestor) in chain.iter().rev().enumerate() {
                        if i > 0 {
                            ui.weak("›");
                        }
                        let active = i == last;
                        let label = node_label(ancestor);
                        if ui
                            .add(
                                egui::Button::new(egui::RichText::new(label).small())
                                    .selected(active),
                            )
                            .on_hover_text(ancestor.display().to_string())
                            .clicked()
                            && !active
                        {
                            self.open_root(ancestor);
                        }
                    }
                }
            });
        });
    }

    /// Files dragged in from the desktop (a file manager, a terminal) are
    /// copied into the folder under the pointer, or the open folder when
    /// nothing was hovered. Copy, never move: pulling a file into a folder
    /// should not delete it from where it came from.
    ///
    /// The hover is last frame's, which is where the pointer sits while the
    /// drop event arrives; egui reports native drops without a widget. It is a
    /// method rather than a stretch of `update` so the refusal below — and the
    /// unnamed-file counting — can be driven with synthetic dropped files.
    fn handle_os_drops(&mut self, ctx: &egui::Context) {
        // Files dragged in from the desktop (a file manager, a terminal) are
        // copied into the folder under the pointer, or the open folder when
        // nothing was hovered. Copy, never move: pulling a file into a folder
        // should not delete it from where it came from.
        // The hover is last frame's, which is where the pointer sits while the
        // drop event arrives; egui reports native drops without a widget.
        let (os_drops, unnamed) = ctx.input(|i| os_drop_paths(&i.raw.dropped_files));
        if !os_drops.is_empty() {
            let target =
                os_drop_target(self.hovered_folder.as_deref(), self.browser.root.as_deref());
            match target {
                Some(folder) if self.browser.is_searching() => {
                    // The drop target would be the folder the search started
                    // from, which is no longer what is on screen. Copying files
                    // into a folder the user cannot see, and then showing them
                    // only if they happen to match the query, is the kind of
                    // surprise that loses files; say what to do instead.
                    let _ = folder;
                    self.status =
                        "Clear the search (Esc or ✕) before dropping files into a folder."
                            .to_string();
                }
                Some(folder) => {
                    let mut status = apply_transfer(&os_drops, &folder, TransferMode::Copy);
                    if unnamed > 0 {
                        status +=
                            &format!("; {unnamed} item(s) arrived without a path and were skipped");
                    }
                    self.status = status;
                    self.refresh_after_drop();
                }
                None => self.status = "Open a folder before dropping files.".into(),
            }
        } else if unnamed > 0 {
            self.status =
                format!("{unnamed} dropped item(s) had no readable path; nothing was copied.");
        }
    }

    /// Esc with the cursor not in a text field clears a search. With the
    /// cursor *in* a field, the key belongs to that field and clearing still
    /// needs the ✕ — a search box that swallowed Esc would fight with every
    /// other use of the key in this app.
    ///
    /// This reads egui's widget focus, not `RawInput::focused`: the latter
    /// means "the window is focused" and is true in almost every real frame,
    /// so a guard built on it would leave the shortcut dead on screen while
    /// still passing a test that defaulted it the other way.
    fn handle_search_shortcuts(&mut self, ctx: &egui::Context) {
        let typing = ctx.memory(|m| m.focused().is_some());
        if !typing && ctx.input(|i| i.key_pressed(egui::Key::Escape)) && self.browser.is_searching()
        {
            self.search_text.clear();
            self.search_due = None;
            self.clear_search();
        }
    }

    fn status_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            let selected = self.browser.selection.len();
            let selected_size = self.selected_total_size();
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.view_mode, ViewMode::Thumbnails, "Thumbnails")
                    .on_hover_text("Thumbnail grid");
                ui.selectable_value(&mut self.view_mode, ViewMode::List, "List")
                    .on_hover_text("Compact list");
                ui.selectable_value(&mut self.view_mode, ViewMode::Details, "Details")
                    .on_hover_text("Details table");
                ui.separator();
                ui.add_sized(
                    [140.0, 18.0],
                    egui::Slider::new(&mut self.cell_size, MIN_CELL..=MAX_CELL)
                        .text("Zoom")
                        .show_value(false),
                );
                ui.separator();
                ui.checkbox(&mut self.thumbs_only, "Thumbnails only")
                    .on_hover_text("Hide files BridgeCraft cannot preview");
                ui.separator();
                let auto_queued = self
                    .auto_processor
                    .as_ref()
                    .map(AutoProcessor::queued_count)
                    .unwrap_or(0);
                let auto_pending = self
                    .auto_processor
                    .as_ref()
                    .map(AutoProcessor::pending_count)
                    .unwrap_or(0);
                let auto_label = if auto_queued > 0 {
                    format!("Auto ({auto_queued} queued)")
                } else if auto_pending > 0 {
                    format!("Auto ({auto_pending} settling)")
                } else {
                    "Auto".to_string()
                };
                if ui
                    .button(auto_label)
                    .on_hover_text("Watched-folder rules and action history")
                    .clicked()
                {
                    self.auto_window = !self.auto_window;
                }
                ui.separator();
                // Metadata index state, next to the job list it comes from.
                // Three cases, and the idle-and-working one says nothing: a
                // quiet status bar is not a signal to look for problems.
                match (&self.index_progress, &self.index_error) {
                    (Some((done, total)), _) => {
                        ui.colored_label(
                            egui::Color32::from_rgb(120, 200, 120),
                            format!("Indexing {done}/{total}"),
                        )
                        .on_hover_text(
                            "Reading metadata for this folder in the background.                              Files whose row already matches the file are
                             skipped, not re-read.",
                        );
                    }
                    (None, Some(reason)) => {
                        ui.colored_label(egui::Color32::from_rgb(220, 160, 90), "Index offline")
                            .on_hover_text(format!(
                                "The metadata index could not be opened, so search and \
                                 index-backed sorting are unavailable.\n{reason}"
                            ));
                    }
                    (None, None) => {}
                }
                let jobs_label = if self.jobs_active > 0 {
                    format!("Jobs ({})", self.jobs_active)
                } else {
                    "Jobs".to_string()
                };
                if ui
                    .button(jobs_label)
                    .on_hover_text("Show export job list")
                    .clicked()
                {
                    self.jobs_window = !self.jobs_window;
                }
                ui.separator();
                ui.add(
                    egui::Label::new(egui::RichText::new(&self.status).small())
                        .truncate(true)
                        .selectable(false),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let summary = if selected > 0 {
                        format!(
                            "{} of {} items · {} selected · {}",
                            self.browser.visible_len(),
                            self.browser.len(),
                            selected,
                            human_size(selected_size)
                        )
                    } else {
                        format!("{} items", self.browser.len())
                    };
                    ui.weak(egui::RichText::new(summary).small());
                });
            });
        });
    }
}

/// Make + Model as one line; a Make duplicated as the Model's prefix is
/// dropped so "Canon" + "Canon EOS-1D X" reads as just the model.
fn format_camera(meta: &bc_metadata::Metadata) -> Option<String> {
    match (
        meta.text(bc_metadata::NS_EXIF, "Make"),
        meta.text(bc_metadata::NS_EXIF, "Model"),
    ) {
        (Some(make), Some(model)) => Some(if model.starts_with(make) {
            model.to_string()
        } else {
            format!("{make} {model}")
        }),
        (Some(make), None) => Some(make.to_string()),
        (None, Some(model)) => Some(model.to_string()),
        (None, None) => None,
    }
}

/// EXIF-reported pixel dimensions as one line.
fn exif_image_size(meta: &bc_metadata::Metadata) -> Option<String> {
    let width = meta.text(bc_metadata::NS_EXIF, "PixelWidth")?;
    let height = meta.text(bc_metadata::NS_EXIF, "PixelHeight")?;
    Some(format!("{width} × {height}"))
}

/// GPS position (plus altitude when present) as one line; needs both
/// latitude and longitude to be worth showing.
fn exif_gps(meta: &bc_metadata::Metadata) -> Option<String> {
    let lat = meta.text(bc_metadata::NS_GPS, "Latitude")?;
    let lon = meta.text(bc_metadata::NS_GPS, "Longitude")?;
    let mut text = format!("{lat}, {lon}");
    if let Some(alt) = meta.text(bc_metadata::NS_GPS, "Altitude") {
        text.push_str(&format!(" · {alt}"));
    }
    Some(text)
}

struct UiPluginHost {
    selected: Vec<PathBuf>,
    folder: Option<PathBuf>,
}

impl UiPluginHost {
    fn from_app(app: &BridgeApp) -> Self {
        Self {
            selected: app
                .browser
                .selection
                .indices()
                .filter_map(|index| {
                    app.browser
                        .entries
                        .get(index)
                        .map(|entry| entry.path.clone())
                })
                .collect(),
            folder: app.browser.root.clone(),
        }
    }
}

impl PluginHost for UiPluginHost {
    fn selected_paths(&self) -> Vec<PathBuf> {
        self.selected.clone()
    }
    fn current_folder(&self) -> Option<PathBuf> {
        self.folder.clone()
    }

    fn read_metadata(&self, path: &Path) -> bc_core::Result<String> {
        // Same layer chain the panel uses, rather than "whichever reader happens
        // to answer first". The old `or_else` chain only fell through when the
        // previous reader *errored*, so a JPEG with EXIF in it hid the tags the
        // sidecar carried: a plugin saw different metadata than the user did.
        std::fs::metadata(path)
            .map_err(|error| bc_core::CoreError::InvalidOperation(error.to_string()))?;
        let metadata = metadata_edit::read_panel_metadata(path);
        serde_json::to_string(&metadata).map_err(|error| {
            bc_core::CoreError::InvalidOperation(format!("metadata encode failed: {error}"))
        })
    }

    fn read_thumbnail(&self, path: &Path, max_edge: u32) -> bc_core::Result<Vec<u8>> {
        let thumbnail = bc_thumbnails::decoder::decode_thumbnail(path, max_edge.min(4096))
            .map_err(|error| {
                bc_core::CoreError::InvalidOperation(format!("thumbnail decode failed: {error}"))
            })?;
        let rgba = image::RgbaImage::from_raw(thumbnail.width, thumbnail.height, thumbnail.pixels)
            .ok_or_else(|| {
                bc_core::CoreError::InvalidOperation(
                    "thumbnail pixel buffer is inconsistent".into(),
                )
            })?;
        let mut cursor = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(rgba)
            .write_to(&mut cursor, image::ImageFormat::Png)
            .map_err(|error| {
                bc_core::CoreError::InvalidOperation(format!("thumbnail encoding failed: {error}"))
            })?;
        Ok(cursor.into_inner())
    }
}

/// Files dragged in from the desktop, split into those the platform named
/// with a path and those it did not. Only pathed files can be copied; the rest
/// are counted so the status line can say so rather than losing them silently.
fn os_drop_paths(files: &[egui::DroppedFile]) -> (Vec<PathBuf>, usize) {
    let mut paths = Vec::with_capacity(files.len());
    let mut unnamed = 0usize;
    for file in files {
        match file.path.clone() {
            Some(path) => paths.push(path),
            None => unnamed += 1,
        }
    }
    (paths, unnamed)
}

/// Transfer mode for a drop: hold Alt to copy, otherwise move.
///
/// Alt is the free modifier in this shell: Ctrl drives Select All, Open and
/// the clipboard, and Shift extends the selection, so neither can double as
/// the copy modifier without stealing an existing binding.
fn drop_mode(alt_down: bool) -> TransferMode {
    if alt_down {
        TransferMode::Copy
    } else {
        TransferMode::Move
    }
}

/// Where files dropped in from the desktop land: the folder under the pointer
/// if one was hovered last frame, otherwise the open folder, otherwise nothing.
fn os_drop_target(hovered: Option<&Path>, root: Option<&Path>) -> Option<PathBuf> {
    hovered
        .map(Path::to_path_buf)
        .or_else(|| root.map(Path::to_path_buf))
}

/// Apply a drag-and-drop transfer via `bc-core` and render the outcome as a
/// status line. Batch rejections leave the filesystem untouched and are
/// reported as such; per-item failures are counted and the first is shown.
fn apply_transfer(sources: &[PathBuf], destination: &Path, mode: TransferMode) -> String {
    let verb = match mode {
        TransferMode::Move => "Moved",
        TransferMode::Copy => "Copied",
    };
    match transfer_into_folder(sources, destination, mode) {
        Ok(report) => {
            let mut status = format!(
                "{verb} {} item(s) into {}",
                report.transferred,
                destination.display()
            );
            if report.skipped > 0 {
                status += &format!(", skipped {} already in destination", report.skipped);
            }
            if let Some((path, error)) = report.errors.first() {
                status += &format!(
                    "; {} failed, first: {}: {error}",
                    report.errors.len(),
                    path.display()
                );
            }
            status
        }
        Err(error) => format!("Drop rejected: {error}"),
    }
}

fn human_size(size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = size as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{size} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// `Asc` does not mean "ascending" to a user: for Modified it means oldest
/// first, for nothing at all it means A-Z, and a tag sort has no letter range
/// to name. So each key says which end comes first, and the hover text says
/// what clicking does with it.
fn sort_direction_label(key: SortKey, dir: SortDir) -> (String, String) {
    let asc = dir == SortDir::Asc;
    let first = match (key, asc) {
        (SortKey::Name | SortKey::Label, true) => "A-Z",
        (SortKey::Name | SortKey::Label, false) => "Z-A",
        (SortKey::Size, true) => "smallest",
        (SortKey::Size, false) => "largest",
        (SortKey::Modified, true) => "oldest",
        (SortKey::Modified, false) => "newest",
        (SortKey::Rating, true) => "1★",
        (SortKey::Rating, false) => "5★",
        (SortKey::Flag, true) => "unflagged",
        (SortKey::Flag, false) => "rejected",
    };
    (
        first.to_string(),
        format!("Currently {first} first — click to put the other end first"),
    )
}

impl eframe::App for BridgeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_thumbnails(ctx);
        if let Some(processor) = &mut self.auto_processor {
            processor.poll(self.auto_started.elapsed());
            if processor.queued_count() > 0 || processor.watches_roots() {
                ctx.request_repaint_after(Duration::from_millis(250));
            }
        }
        self.poll_jobs(ctx);
        self.poll_search(ctx);
        // The rule editor shares the search worker, so it is serviced here too:
        // answers taken, and a due recount asked for. Before the panels are
        // drawn, so a count that landed while the user was deciding is on screen
        // in the same frame it arrives.
        self.tick_editor_count(ctx);
        self.tick_suggestions(ctx);
        self.handle_search_shortcuts(ctx);
        // A debounced search fires on the clock, not on an event: nothing
        // happens 450 ms after the last keystroke except this check.
        if let Some(due) = self.search_due {
            if Instant::now() >= due {
                self.search_due = None;
                self.request_search();
            } else {
                ctx.request_repaint_after(due.saturating_duration_since(Instant::now()));
            }
        }
        self.poll_watcher();
        self.prune_preview_textures();

        self.handle_os_drops(ctx);
        // A drag that no folder claimed does nothing, matching Explorer and
        // Bridge: releasing items on the background of the folder they are
        // already in would only report a meaningless "skipped" count.
        self.hovered_folder = None;

        // Global shortcuts that must not fire while a text field has focus.
        if !ctx.wants_keyboard_input() {
            let (ctrl, select_all, refresh) = ctx.input(|input| {
                (
                    input.modifiers.ctrl || input.modifiers.command,
                    input.key_pressed(egui::Key::A),
                    input.key_pressed(egui::Key::F5),
                )
            });
            if refresh {
                self.refresh_now();
            }
            if ctrl && select_all {
                self.select_all_visible();
            }
            if ctrl && ctx.input(|i| i.key_pressed(egui::Key::O)) {
                self.open_field = !self.open_field;
            }
        }

        // Viewer shortcuts, active only while the viewer owns the panel and
        // no text field has focus.
        if self.viewer.is_open() && !ctx.wants_keyboard_input() {
            let (esc, left, right, zoom_in, zoom_out, fit, actual) = ctx.input(|input| {
                (
                    input.key_pressed(egui::Key::Escape),
                    input.key_pressed(egui::Key::ArrowLeft),
                    input.key_pressed(egui::Key::ArrowRight),
                    input.key_pressed(egui::Key::Plus) || input.key_pressed(egui::Key::Equals),
                    input.key_pressed(egui::Key::Minus),
                    input.key_pressed(egui::Key::Num0),
                    input.key_pressed(egui::Key::Num1),
                )
            });
            if esc {
                self.close_viewer();
            }
            if left {
                self.viewer_step(-1);
            }
            if right {
                self.viewer_step(1);
            }
            if zoom_in {
                self.viewer.zoom_step(1.25);
            }
            if zoom_out {
                self.viewer.zoom_step(0.8);
            }
            if fit {
                self.viewer.set_fit();
            }
            if actual {
                self.go_actual_size();
            }
        }

        self.top_bar(ctx);
        self.status_bar(ctx);

        egui::SidePanel::left("sidebar")
            .resizable(true)
            .default_width(210.0)
            .min_width(150.0)
            .max_width(380.0)
            .show(ctx, |ui| self.sidebar(ui));

        egui::SidePanel::right("metadata")
            .resizable(true)
            .default_width(260.0)
            .min_width(180.0)
            .max_width(480.0)
            .show(ctx, |ui| self.metadata_panel(ui));

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.viewer.is_open() {
                self.viewer_view(ui, ctx);
            } else if self.browser.root.is_none() {
                ui.centered_and_justified(|ui| {
                    ui.weak("Press Open… (Ctrl+O) and type a folder path.");
                });
            } else if self.browser.visible_len() == 0 {
                ui.centered_and_justified(|ui| {
                    if self.browser.is_searching() {
                        // The distinction the user needs: an empty result grid
                        // says nothing about the folder.
                        let line = match &self.search_error {
                            Some(reason) => format!("Search unavailable: {reason}"),
                            None if self.search_waiting.is_some() => "Searching…".to_string(),
                            None => self.search_status(0),
                        };
                        ui.weak(line);
                    } else if self.browser.is_empty() {
                        ui.weak("This folder is empty.");
                    } else {
                        ui.weak(format!("No items match filter “{}”.", self.browser.filter));
                    }
                });
            } else if self.display_indices().is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.weak("Thumbnails-only hides every item in this folder.");
                });
            } else {
                match self.view_mode {
                    ViewMode::Thumbnails => self.content_grid(ui),
                    ViewMode::List => self.content_list(ui, ctx, false),
                    ViewMode::Details => self.content_list(ui, ctx, true),
                }
            }
        });

        self.trash_window(ctx);
        self.confirm_delete_window(ctx);
        self.rename_window(ctx);
        self.export_window(ctx);
        self.editor_window(ctx);
        self.jobs_window(ctx);
        self.auto_process_window(ctx);
        self.plugin_manager_window(ctx);
        self.photocraft_settings_window(ctx);
        self.lightcraft_settings_window(ctx);
    }
}

fn navigation_target(visible: &[usize], selection: &Selection, delta: i32) -> Option<usize> {
    if visible.is_empty() {
        return None;
    }
    let current = selection
        .anchor()
        .and_then(|anchor| visible.iter().position(|&idx| idx == anchor))
        .or_else(|| {
            selection
                .indices()
                .find_map(|idx| visible.iter().position(|&visible_idx| visible_idx == idx))
        });
    let next = match current {
        Some(position) => (position as i32 + delta).clamp(0, visible.len() as i32 - 1) as usize,
        None if delta < 0 => visible.len() - 1,
        None => 0,
    };
    Some(visible[next])
}

/// Details-view columns, in panel order, with each one's share of the row
/// width. The Name column gets the largest share because the filename is what
/// the eye goes to; Rating and Label are the narrow tails.
pub fn details_columns() -> [(&'static str, f32); 6] {
    [
        ("Name", 0.38),
        ("Type", 0.12),
        ("Size", 0.10),
        ("Modified", 0.16),
        ("Rating", 0.08),
        ("Label", 0.16),
    ]
}

/// Left text edge of a details column, so the header and the rows agree.
pub fn details_column_x(left: f32, width: f32, column: usize) -> f32 {
    let columns = details_columns();
    let offset: f32 = columns[..column.min(columns.len())]
        .iter()
        .map(|(_, share)| share)
        .sum();
    left + 8.0 + width * offset
}

fn format_modified(modified: Option<std::time::SystemTime>) -> String {
    use std::time::UNIX_EPOCH;
    let Some(seconds) = modified
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
    else {
        return "—".to_owned();
    };
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        day_seconds / 3600,
        day_seconds % 3600 / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// egui needs a wall clock to tell a double click from two single ones, so
    /// the synthetic frames advance time the way a running window would.
    fn next_frame_time() -> f64 {
        use std::cell::RefCell;
        thread_local! {
            static FRAME_CLOCK: RefCell<f64> = const { RefCell::new(1.0) };
        }
        FRAME_CLOCK.with(|clock| {
            let mut clock = clock.borrow_mut();
            *clock += 0.05;
            *clock
        })
    }

    /// Store paths come from environment variables read at call time, so
    /// `isolated_app` sets them. Environment mutation is process-global, so the
    /// tests that build a `BridgeApp` take this lock and hold it for their
    /// whole body: otherwise one test can move `HOME` while another is opening
    /// its recents store, and the loser writes into a directory that the other
    /// test has already deleted.
    static STORE_ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Press or release the primary button at `pos`.
    fn button(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        }
    }

    /// Drive one frame of the real list view, so a test presses, drags and
    /// releases exactly the way the window does. `alt` is the frame's modifier
    /// state: egui reads modifiers from `RawInput`, not from the button event,
    /// which is how the winit integration reports the desktop's modifiers.
    fn drive_list(app: &mut BridgeApp, ctx: &egui::Context, alt: bool, events: Vec<egui::Event>) {
        let mut raw = egui::RawInput::default();
        raw.modifiers.alt = alt;
        raw.screen_rect = Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(900.0, 700.0),
        ));
        raw.events = events;
        // The frame's FullOutput is not needed: these tests read the app's
        // own state and the filesystem for their assertions.
        let _ = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.content_list(ui, ctx, false));
        });
    }

    /// Find a point inside the row for `folder` by asking the app which folder
    /// the pointer is over, so the test uses the real row geometry rather than
    /// pixels guessed from the layout.
    fn point_over_folder(
        app: &mut BridgeApp,
        ctx: &egui::Context,
        folder: &Path,
    ) -> Option<egui::Pos2> {
        for y in (2..700).step_by(2) {
            let pos = egui::pos2(300.0, y as f32);
            app.hovered_folder = None;
            drive_list(app, ctx, false, vec![egui::Event::PointerMoved(pos)]);
            if app.hovered_folder.as_deref() == Some(folder) {
                return Some(pos);
            }
        }
        None
    }

    /// The row after `target`, using the pitch measured from the two located
    /// rows rather than a guessed layout constant.
    fn point_after(from: egui::Pos2, target: egui::Pos2) -> egui::Pos2 {
        target + egui::vec2(0.0, (target.y - from.y).abs())
    }

    /// A `BridgeApp` whose JSON stores land in `isolated` instead of the
    /// developer's real per-user data directory. The store paths come from
    /// environment variables read at call time, so these tests set them; no
    /// other test in this binary reads them, which is what makes doing that
    /// from a parallel test tolerable.
    /// Point the per-user store paths at `isolated`.
    ///
    /// The environment is process-global, so reading or writing one of these
    /// paths is only safe while a single test is in that region. Taking the
    /// guard as an argument is what makes it a rule the compiler enforces
    /// rather than one to remember: a test that wants to isolate must first
    /// lock, and must keep the lock for as long as the app it built can still
    /// resolve a path. This is not ceremony — an indexing test that seeded a
    /// fixture before isolating wrote it into whichever `HOME` the previous
    /// test had left set, and in a fresh process that is the developer's real
    /// one, which is how `/tmp/opencode/*` entries ended up in a real
    /// `recents.json`.
    fn isolated_env(isolated: &Path, _stores: &std::sync::MutexGuard<'static, ()>) {
        std::env::set_var("HOME", isolated);
        std::env::set_var("APPDATA", isolated);
        std::env::set_var("XDG_DATA_HOME", isolated.join("data"));
    }

    fn isolated_app(
        ctx: &egui::Context,
        root: &Path,
        isolated: &Path,
        stores: &std::sync::MutexGuard<'static, ()>,
    ) -> BridgeApp {
        isolated_env(isolated, stores);
        BridgeApp::new(ctx, Some(root.display().to_string()))
    }

    fn png(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, [0x89u8, b'P', b'N', b'G', 0, 0, 0, 0]).unwrap();
        path
    }

    /// Rows the indexer would have written, put in place by hand. Ratings only,
    /// through the same `index_file` the indexer's own row write uses, so a
    /// test cannot pass by disagreeing with the indexer about the schema.
    ///
    /// Seeding goes through the same isolated path as the app will read, so the
    /// fixture cannot land in one database while the app opens another.
    fn seed_index(
        isolated: &Path,
        rows: &[(PathBuf, u8)],
        stores: &std::sync::MutexGuard<'static, ()>,
    ) {
        isolated_env(isolated, stores);
        let path = bc_core::paths::metadata_index_path().unwrap();
        let index = bc_core::MetadataIndex::open(&path).unwrap();
        for (file, stars) in rows {
            let mut meta = bc_metadata::Metadata::new();
            meta.set_rating(bc_metadata::ratings::Rating::new(*stars));
            index.index_file(file, &meta).unwrap();
        }
    }

    fn order(app: &BridgeApp) -> Vec<String> {
        app.browser
            .entries
            .iter()
            .map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    /// Let the indexing pass the app queued run to its end.
    fn finish_indexing(app: &mut BridgeApp, ctx: &egui::Context) {
        for _ in 0..400 {
            app.poll_jobs(ctx);
            if app.index_job.is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the index pass never finished");
    }

    /// Drive one frame of the real toolbar and return the text it drew, with
    /// the rect each piece of text was drawn in. A test that clicks what it
    /// can read does not need to guess a layout, and cannot pass because a
    /// widget was removed from the bar while the state behind it stayed.
    fn toolbar_text(app: &mut BridgeApp, ctx: &egui::Context) -> Vec<(String, egui::Rect)> {
        toolbar_text_at(app, ctx, egui::pos2(1_000.0, 12.0))
    }

    /// As `toolbar_text`, but resting the cursor at `at`. An egui menu closes
    /// when the pointer it was opened by moves away, so a frame with no pointer
    /// event reads as "user left the menu" and the items a test is looking for
    /// are no longer drawn. This is the difference between testing a button and
    /// testing something that opens.
    fn toolbar_text_at(
        app: &mut BridgeApp,
        ctx: &egui::Context,
        at: egui::Pos2,
    ) -> Vec<(String, egui::Rect)> {
        // A realistic window height, not a strip the size of the bar: the
        // first version of this test used 60 px and the sort menu, drawn below
        // the bar, was clipped to its first four items — so the test reported
        // two missing keys that a user on a real screen can see.
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1400.0, 900.0),
            )),
            events: vec![egui::Event::PointerMoved(at)],
            ..Default::default()
        };
        let out = ctx.run(raw, |ctx| app.top_bar(ctx));
        let mut texts = Vec::new();
        for clipped in &out.shapes {
            if let egui::Shape::Text(text) = &clipped.shape {
                texts.push((
                    text.galley.text().to_string(),
                    egui::Rect::from_min_size(text.pos, text.galley.size()),
                ));
            }
        }
        texts
    }

    /// Drive one sidebar frame and return the text it drew, with the rect each
    /// piece was drawn in. Same reasoning as `toolbar_text`: a test clicks what
    /// it can read.
    fn sidebar_text(app: &mut BridgeApp, ctx: &egui::Context) -> Vec<(String, egui::Rect)> {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1400.0, 900.0),
            )),
            ..Default::default()
        };
        let out = ctx.run(raw, |ctx| {
            egui::SidePanel::left("sidebar").show(ctx, |ui| app.sidebar(ui));
        });
        let mut texts = Vec::new();
        for clipped in &out.shapes {
            if let egui::Shape::Text(text) = &clipped.shape {
                texts.push((
                    text.galley.text().to_string(),
                    egui::Rect::from_min_size(text.pos, text.galley.size()),
                ));
            }
        }
        texts
    }

    fn click_sidebar(app: &mut BridgeApp, ctx: &egui::Context, at: egui::Pos2) {
        click_at(app, ctx, at, |app, ctx| {
            egui::SidePanel::left("sidebar").show(ctx, |ui| app.sidebar(ui));
        });
    }

    /// Drive one toolbar frame with `events`, resting the pointer at `at`
    /// first so that menus and text fields behave the way they do under a real
    /// cursor.
    fn drive_toolbar(
        app: &mut BridgeApp,
        ctx: &egui::Context,
        at: egui::Pos2,
        events: Vec<egui::Event>,
    ) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1400.0, 900.0),
            )),
            events: {
                let mut queued = vec![egui::Event::PointerMoved(at)];
                queued.extend(events);
                queued
            },
            // The window is focused in these tests, as it is for a user who is
            // typing in it. `handle_search_shortcuts` deliberately ignores keys
            // when a widget has focus, and widget focus is a different signal
            // from this one.
            focused: true,
            ..Default::default()
        };
        let _ = ctx.run(raw, |ctx| app.top_bar(ctx));
    }

    /// Move the pointer to `at` in one frame, then press and release there in
    /// the next.
    ///
    /// Both details are real-window behaviour, and both were learned the slow
    /// way. The pointer has to be over the widget *before* the press arrives:
    /// batching move+press+release into one frame leaves egui no frame in which
    /// the widget was hovered and pressed, and the click is silently not
    /// registered — a sidebar tab that never opened, in an earlier version of
    /// these helpers. And press and release must share a frame, because a
    /// `menu_button` opens on the press and would take the release as a second
    /// click, closing again.
    fn click_at(
        app: &mut BridgeApp,
        ctx: &egui::Context,
        at: egui::Pos2,
        mut draw: impl FnMut(&mut BridgeApp, &egui::Context),
    ) {
        draw(app, ctx);
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1400.0, 900.0),
            )),
            events: vec![egui::Event::PointerMoved(at)],
            ..Default::default()
        };
        let _ = ctx.run(raw, |ctx| draw(app, ctx));
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1400.0, 900.0),
            )),
            events: vec![button(at, true), button(at, false)],
            ..Default::default()
        };
        let _ = ctx.run(raw, |ctx| draw(app, ctx));
    }

    fn click_toolbar(app: &mut BridgeApp, ctx: &egui::Context, at: egui::Pos2) {
        click_at(app, ctx, at, |app, ctx| {
            let _ = ctx;
            app.top_bar(ctx);
        });
    }

    /// Where the toolbar drew the given text, or a failure listing what it did
    /// draw. Without that list a test that breaks because a label was reworded
    /// looks identical to one that breaks because the control disappeared.
    fn toolbar_rect(app: &mut BridgeApp, ctx: &egui::Context, wanted: &str) -> egui::Rect {
        toolbar_rect_at(app, ctx, wanted, egui::pos2(1_000.0, 12.0))
    }

    fn toolbar_rect_at(
        app: &mut BridgeApp,
        ctx: &egui::Context,
        wanted: &str,
        at: egui::Pos2,
    ) -> egui::Rect {
        let drawn = toolbar_text_at(app, ctx, at);
        drawn
            .iter()
            .find(|(text, _)| text == wanted)
            .map(|(_, rect)| *rect)
            .unwrap_or_else(|| {
                panic!(
                    "“{wanted}” was not drawn in the toolbar; it drew {:?}",
                    drawn
                        .iter()
                        .map(|(text, _)| text.as_str())
                        .collect::<Vec<_>>()
                )
            })
    }

    #[test]
    fn a_folder_already_in_the_index_sorts_by_rating_the_moment_it_opens() {
        // The reason `load_folder_tags` runs before the pass rather than only
        // after it: a folder visited once should sort correctly on the second
        // visit, not wait out a pass over files that have not changed.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let low = png(&root, "low.png");
        let mid = png(&root, "mid.png");
        let high = png(&root, "high.png");
        seed_index(
            &data,
            &[(low.clone(), 1), (mid.clone(), 3), (high.clone(), 5)],
            &_stores,
        );

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        assert_eq!(
            app.browser.tags.len(),
            3,
            "the grid read nothing from the index"
        );
        assert_eq!(
            app.browser.tags.get(&high).unwrap().rating,
            5,
            "the rating read back is not the one seeded"
        );

        app.browser.set_sort(SortKey::Rating, SortDir::Desc);
        assert_eq!(order(&app), ["high.png", "mid.png", "low.png"]);
        app.browser.set_sort(SortKey::Rating, SortDir::Asc);
        assert_eq!(order(&app), ["low.png", "mid.png", "high.png"]);
    }

    #[test]
    fn a_saved_collection_puts_its_rows_back_on_screen_after_the_search_is_gone() {
        // The whole point of a collection: an answer worth coming back to,
        // without retyping it and without it drifting into a different scope.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let beach = png(&root, "sunset.png");
        let pines = png(&root, "pines.png");
        seed_index(&data, &[(beach.clone(), 0), (pines.clone(), 0)], &_stores);
        let db = bc_core::paths::metadata_index_path().unwrap();
        let index = bc_core::MetadataIndex::open(&db).unwrap();
        for (path, title) in [(&beach, "a beach evening"), (&pines, "pines at dawn")] {
            let mut meta = bc_metadata::Metadata::new();
            meta.set_text(bc_metadata::NS_DC, "title", title.to_string());
            index.index_file(path, &meta).unwrap();
        }

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        app.search_text = "beach".to_string();
        app.request_search();
        finish_search(&mut app, &ctx);
        assert_eq!(result_names(&app), vec!["sunset.png".to_string()]);

        app.collection_name = "shore things".to_string();
        app.save_current_collection();
        assert!(app.status.starts_with("Saved collection"), "{}", app.status);
        assert_eq!(app.collections.len(), 1, "{:?}", app.collections);
        assert_eq!(
            app.browser.collection(),
            Some("shore things"),
            "the grid should now be labelled as the collection it came from"
        );

        // Leave the search entirely, then come back to the collection.
        app.search_text.clear();
        app.clear_search();
        assert!(!app.browser.is_searching(), "clearing left a listing");
        // Back to the folder's own two files, not the one the search matched.
        assert_eq!(
            result_names(&app),
            vec!["pines.png".to_string(), "sunset.png".to_string()],
            "clearing a search did not restore the folder listing"
        );

        app.run_collection("shore things");
        finish_search(&mut app, &ctx);
        assert!(app.browser.is_searching());
        assert_eq!(
            result_names(&app),
            vec!["sunset.png".to_string()],
            "the collection did not reproduce its own answer"
        );
        assert_eq!(
            app.search_text, "beach",
            "the box should show the collection's words"
        );
        assert!(
            app.status.contains("Collection “shore things”"),
            "the status should name what is on screen: {}",
            app.status
        );
    }

    #[test]
    fn saving_a_name_twice_replaces_the_collection_rather_than_piling_it_up() {
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let sunset = png(&root, "sunset.png");
        let pines = png(&root, "pines.png");
        seed_index(&data, &[(sunset.clone(), 0), (pines.clone(), 0)], &_stores);
        let db = bc_core::paths::metadata_index_path().unwrap();
        let index = bc_core::MetadataIndex::open(&db).unwrap();
        for (path, title) in [(&sunset, "sunset glow"), (&pines, "pines ridge")] {
            let mut meta = bc_metadata::Metadata::new();
            meta.set_text(bc_metadata::NS_DC, "title", title.to_string());
            index.index_file(path, &meta).unwrap();
        }

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);

        app.search_text = "pines".to_string();
        app.request_search();
        finish_search(&mut app, &ctx);
        app.collection_name = "trip".to_string();
        app.save_current_collection();

        // Same name, different words: the earlier rules are gone, not shadowed.
        app.search_text = "sunset".to_string();
        app.request_search();
        finish_search(&mut app, &ctx);
        app.collection_name = "trip".to_string();
        app.save_current_collection();

        assert_eq!(app.collections.len(), 1, "{:?}", app.collections);
        app.run_collection("trip");
        finish_search(&mut app, &ctx);
        assert_eq!(result_names(&app), vec!["sunset.png".to_string()]);
    }

    #[test]
    fn an_unreadable_saved_search_file_is_reported_and_never_overwritten() {
        // This is the reason the definitions live in their own file instead of
        // in the metadata index. The index is a cache: losing rows in it costs
        // a re-scan. This file holds only what the user named, so a store that
        // cannot be read has to be said out loud, refuse to save over itself,
        // and leave the file exactly as it was for someone to repair.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        png(&root, "one.png");

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        app.search_text = "one".to_string();
        app.request_search();
        finish_search(&mut app, &ctx);

        // Asked of the store rather than spelled out: a test that rebuilds the
        // path by hand keeps its own copy of the rule and would quietly check a
        // different file if the rule ever moved.
        let file = bc_core::SavedSearches::open_default()
            .unwrap()
            .path()
            .to_path_buf();
        assert!(
            file.starts_with(&data),
            "{file:?} is not under the isolation root"
        );
        std::fs::write(&file, b"{ this is not json").unwrap();
        let before = std::fs::read(&file).unwrap();

        // Enter the tab, which is where the store is read.
        app.sidebar_tab = SidebarTab::Collections;
        let drawn = sidebar_text(&mut app, &ctx);
        let texts: Vec<&str> = drawn.iter().map(|(t, _)| t.as_str()).collect();
        assert!(
            texts.iter().any(|t| t.contains("could not be read")),
            "the panel hid an unreadable saved-search file: {texts:?}"
        );

        // A save must not turn one bad byte into an empty file.
        app.collection_name = "doomed".to_string();
        app.save_current_collection();
        assert!(
            app.status.contains("could not be read")
                && app.status.contains(&file.display().to_string()),
            "saving into an unreadable store must refuse and name the file: {}",
            app.status
        );
        assert_eq!(
            std::fs::read(&file).unwrap(),
            before,
            "a refused save still overwrote the user's saved searches"
        );
    }

    /// Draw the rule editor alone and collect what it said, the way
    /// `sidebar_text` does for the sidebar. A window has to be testable without
    /// the rest of the interface, and the assertions are about the words a user
    /// reads rather than about fields nobody sees.
    fn editor_text(app: &mut BridgeApp, ctx: &egui::Context) -> Vec<(String, egui::Rect)> {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1400.0, 900.0),
            )),
            ..Default::default()
        };
        // A window needs one pass to lay itself out before it paints anything:
        // the first pass on a fresh context yields the frame's shapes with no
        // text at all, which reads exactly like a dialog that drew nothing. The
        // sidebar does not behave this way, so the warm-up belongs here rather
        // than being assumed of every surface.
        let _ = ctx.run(raw.clone(), |ctx| app.editor_window(ctx));
        let out = ctx.run(raw, |ctx| app.editor_window(ctx));
        let mut texts = Vec::new();
        for clipped in &out.shapes {
            if let egui::Shape::Text(text) = &clipped.shape {
                texts.push((
                    text.galley.text().to_string(),
                    egui::Rect::from_min_size(text.pos, text.galley.size()),
                ));
            }
        }
        texts
    }

    fn editor_rect(app: &mut BridgeApp, ctx: &egui::Context, wanted: &str) -> egui::Rect {
        let drawn = editor_text(app, ctx);
        drawn
            .iter()
            .find(|(text, _)| text == wanted)
            .map(|(_, rect)| *rect)
            .unwrap_or_else(|| {
                panic!(
                    "“{wanted}” was not drawn in the rule editor; it drew {:?}",
                    drawn
                        .iter()
                        .map(|(text, _)| text.as_str())
                        .collect::<Vec<_>>()
                )
            })
    }

    /// Put an answer on the editor's reply channel as if the worker had written
    /// it. Tests that care about *which* reply is accepted cannot wait for the
    /// real thread to produce the wrong one on schedule.
    fn editor_send(app: &mut BridgeApp, answer: CountAnswer) {
        let editor = app.editor.as_ref().unwrap();
        editor.count_tx.send(answer).unwrap();
    }

    /// An editor with one rating rule over `root`, library-wide.
    fn rating_editor(app: &mut BridgeApp, stars: &str) {
        app.open_collection_editor("picked".to_string(), None, None);
        let editor = app.editor.as_mut().unwrap();
        editor.scope = bc_core::SearchScope::Library;
        editor.rows.push(RuleRow {
            field: bc_core::Field::Rating,
            operator: bc_core::Operator::GreaterThanOrEqual,
            value: stars.to_string(),
        });
    }

    /// Wait for the worker's number, collapsing the debounce by moving its
    /// deadline up rather than sleeping through it. The reply still comes from
    /// the worker thread, which is the thing under test.
    fn finish_count(app: &mut BridgeApp, ctx: &egui::Context) -> Option<usize> {
        for _ in 0..500 {
            if let Some(editor) = app.editor.as_mut() {
                editor.count_due = Some(Instant::now());
            }
            app.tick_editor_count(ctx);
            if let Some(editor) = app.editor.as_ref() {
                if editor.outstanding.is_none() && editor.count.is_some() {
                    return editor.count;
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("the count never came back from the worker");
    }

    #[test]
    fn the_editor_asks_the_worker_for_a_count_instead_of_scanning_in_the_frame() {
        // The claim worth testing is not that the number is right — `search`
        // already counts rows — but that the frame that shows it never ran the
        // scan itself. A dialog that computed its own count would freeze while
        // someone typed, and every assertion below would still pass.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let low = png(&root, "low.png");
        let mid = png(&root, "mid.png");
        let high = png(&root, "high.png");
        seed_index(&data, &[(low, 1), (mid, 3), (high, 5)], &_stores);

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        rating_editor(&mut app, "3");

        // One tick sets a debounce and asks for nothing.
        app.tick_editor_count(&ctx);
        {
            let editor = app.editor.as_ref().unwrap();
            assert!(
                editor.count_due.is_some(),
                "the frame did not queue a debounce"
            );
            assert!(
                editor.outstanding.is_none(),
                "a count was asked for before the debounce had passed"
            );
            assert!(
                editor.count.is_none() && editor.counted.is_none(),
                "the frame counted the library on its own thread"
            );
        }

        // Two more frames, still inside the window. This is the assertion the
        // debounce is actually for: without it a fast typist queues one table
        // scan per keystroke, and 70 ms per 100 000 rows is a window that stops
        // answering. A build that sent the request here would pass every check
        // above.
        app.tick_editor_count(&ctx);
        app.tick_editor_count(&ctx);
        assert!(
            app.editor.as_ref().unwrap().outstanding.is_none(),
            "a count was asked for while the debounce was still open"
        );

        assert_eq!(
            finish_count(&mut app, &ctx),
            Some(2),
            "two of three files hold three stars or more"
        );
        let shown = editor_text(&mut app, &ctx);
        let words: Vec<&str> = shown.iter().map(|(t, _)| t.as_str()).collect();
        assert!(
            words.contains(&"2 file(s) match"),
            "the count was computed but never shown: {words:?}"
        );
    }

    #[test]
    fn a_count_for_other_rules_cannot_take_the_place_of_a_newer_one() {
        // Requests share one channel and one worker, so an answer to rules the
        // user has since edited can land after a newer request went out. Taking
        // it would put a confident wrong number on screen — the one failure mode
        // this dialog has more of than any other.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let low = png(&root, "low.png");
        let mid = png(&root, "mid.png");
        let high = png(&root, "high.png");
        // Two files at three stars or more, one at five: the old rules and the
        // new ones have to answer differently, or a test cannot tell them apart.
        seed_index(&data, &[(low, 1), (mid, 3), (high, 5)], &_stores);

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        rating_editor(&mut app, "3");
        assert_eq!(finish_count(&mut app, &ctx), Some(2));

        // The state the guard exists for, built directly: a request for the rules
        // now on screen is outstanding, and an answer for the rules that used to
        // be there arrives. Racing the real worker for this would make the
        // outcome a function of machine load rather than of the code.
        app.editor.as_mut().unwrap().rows[0].value = "5".to_string();
        let late = {
            let editor = app.editor.as_mut().unwrap();
            let older = editor.count_seq;
            editor.outstanding = Some((older + 1, editor.count_key()));
            older
        };
        editor_send(
            &mut app,
            CountAnswer {
                seq: late,
                count: Ok(9_999),
            },
        );

        // Polled away: the outstanding request keeps its place, and nothing the
        // stale answer carried reaches either the field or the screen.
        app.tick_editor_count(&ctx);
        {
            let editor = app.editor.as_ref().unwrap();
            assert!(
                editor.outstanding.is_some(),
                "a reply to a different request was accepted as this one's"
            );
            assert_eq!(
                editor.count,
                Some(2),
                "the stale number overwrote the previous answer"
            );
        }
        let words: Vec<String> = editor_text(&mut app, &ctx)
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        assert!(
            words.iter().any(|t| t == "Counting…"),
            "the dialog stopped saying a request is outstanding: {words:?}"
        );
        assert!(
            !words
                .iter()
                .any(|t| t.contains("9,999") || t.contains("9999")),
            "an answer for rules that no longer exist reached the screen: {words:?}"
        );

        // And the recount the user is owed does arrive, for the new rules.
        assert_eq!(
            finish_count(&mut app, &ctx),
            Some(1),
            "five stars or more is one file, not two"
        );
        let after: Vec<String> = editor_text(&mut app, &ctx)
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        assert!(
            after.iter().any(|t| t == "1 file(s) match"),
            "the recount never made it to the screen: {after:?}"
        );
    }

    #[test]
    fn a_collection_of_rules_only_runs_from_the_sidebar_and_says_so_about_itself() {
        // The run path is what makes authoring worth doing, and a rules-only
        // collection is a shape it has never seen: no words for the box to
        // mirror, and a scope of its own rather than the folder the box last
        // searched.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let seen = png(&root, "seen.png");
        png(&root, "unseen.png");
        seed_index(&data, &[(seen.clone(), 4)], &_stores);

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);

        // Leave a coverage figure on the screen, folder-scoped, as the box does.
        app.search_scope = SearchScopeChoice::Folder;
        app.search_text = "seen".to_string();
        app.request_search();
        finish_search(&mut app, &ctx);
        assert!(
            app.status.contains("not indexed yet"),
            "the folder search should have reported its own gap: {}",
            app.status
        );

        // Author a collection the search box cannot express, with no words.
        app.open_collection_editor("picked".to_string(), None, None);
        {
            let editor = app.editor.as_mut().unwrap();
            editor.scope = bc_core::SearchScope::Library;
            editor.rows.push(RuleRow {
                field: bc_core::Field::Rating,
                operator: bc_core::Operator::GreaterThanOrEqual,
                value: "3".to_string(),
            });
            assert!(
                editor.problems().is_empty(),
                "a plain rating rule should be countable: {:?}",
                editor.problems()
            );
        }
        let draft = app.editor.as_ref().unwrap().as_saved();
        assert!(
            app.save_collection_draft("picked", &draft, None),
            "{}",
            app.status
        );
        app.editor = None;
        app.refresh_collections();

        // Run it the way the sidebar does.
        app.run_collection("picked");
        finish_search(&mut app, &ctx);
        assert!(
            app.status.starts_with("Collection “picked”: 1 file(s)"),
            "a rules-only collection does not report itself honestly: {}",
            app.status
        );
        assert!(
            !app.status.contains("not indexed yet"),
            "the previous folder search's coverage figure followed the run: {}",
            app.status
        );
        assert_eq!(
            result_names(&app),
            vec!["seen.png".to_string()],
            "the rule did not produce the file it describes"
        );
        // And the box stays empty, because there was nothing in this collection
        // for it to mirror — a search box showing "" while the grid answers a
        // rule is the honest state and the one the label above has to explain.
        assert!(
            app.search_text.is_empty(),
            "the box invented words for a rules-only collection: {:?}",
            app.search_text
        );

        // The same collection, edited and re-run: the sidebar's definition and
        // the dialog's are the same object, so a run reflects the edit.
        app.open_collection_editor(
            "picked".to_string(),
            Some("picked".to_string()),
            Some(&draft),
        );
        app.editor.as_mut().unwrap().rows[0].value = "5".to_string();
        let edited = app.editor.as_ref().unwrap().as_saved();
        assert!(
            app.save_collection_draft("picked", &edited, Some("picked")),
            "{}",
            app.status
        );
        app.editor = None;
        app.refresh_collections();
        app.run_collection("picked");
        finish_search(&mut app, &ctx);
        assert!(
            app.status.starts_with("Collection “picked”: 0 file(s)"),
            "four stars is not five: {}",
            app.status
        );
    }

    #[test]
    fn an_editor_with_no_conditions_says_so_and_a_words_only_one_does_not() {
        // The sentence "this matches every file in its scope" is true of an empty
        // rule set and wrong of one holding words, and the two look identical in
        // the dialog until the count arrives. Pinning both halves, because the
        // first version of this dialog printed it while words were typed.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let one = png(&root, "one.png");
        seed_index(&data, &[(one, 2)], &_stores);

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        app.open_collection_editor("blank".to_string(), None, None);
        let words: Vec<String> = editor_text(&mut app, &ctx)
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        assert!(
            words
                .iter()
                .any(|t| t.contains("matches every file in its scope")),
            "an empty rule set did not say what it matches: {words:?}"
        );

        app.editor.as_mut().unwrap().words = "one".to_string();
        let after: Vec<String> = editor_text(&mut app, &ctx)
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        assert!(
            !after
                .iter()
                .any(|t| t.contains("matches every file in its scope")),
            "the dialog still promised every file after words were typed: {after:?}"
        );
        // And the count agrees with the promise it replaced.
        assert_eq!(finish_count(&mut app, &ctx), Some(1));
    }

    #[test]
    fn an_impossible_rule_is_never_reported_as_no_matches() {
        // The failure mode the guard exists for: `compare_i64` reads an
        // unparsable value as zero, so "rating at least big" is a rule that
        // matches two of three files, and "0 files match" is the same screen as
        // "this rule cannot mean anything". Neither number is the truth, so the
        // dialog refuses to show one.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let low = png(&root, "low.png");
        let high = png(&root, "high.png");
        seed_index(&data, &[(low, 1), (high, 5)], &_stores);

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        rating_editor(&mut app, "big");

        // Two ticks: one to set the debounce, one to reach the deadline. If the
        // dialog counted anyway, a request would be outstanding by now.
        app.tick_editor_count(&ctx);
        {
            let editor = app.editor.as_mut().unwrap();
            editor.count_due = Some(Instant::now());
        }
        app.tick_editor_count(&ctx);
        {
            let editor = app.editor.as_ref().unwrap();
            assert!(
                editor.outstanding.is_none(),
                "an impossible rule was still sent to the worker"
            );
            assert!(
                editor.count.is_none(),
                "a count was shown for rules that cannot match: {:?}",
                editor.count
            );
            assert!(
                editor.counted.is_some(),
                "the impossible ruleset will be re-asked every frame"
            );
        }

        let words: Vec<String> = editor_text(&mut app, &ctx)
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        let flat = words.join(" / ");
        assert!(
            flat.contains("cannot match anything"),
            "the dialog did not say why there is no number: {flat}"
        );
        assert!(
            flat.contains("read as zero"),
            "the row has to explain the specific mistake: {flat}"
        );
        assert!(
            !words.iter().any(|t| t.ends_with("file(s) match")),
            "a number was shown for rules that cannot match: {flat}"
        );
    }

    #[test]
    fn the_save_button_is_dead_while_a_rule_cannot_match_and_live_when_it_can() {
        // A negative test with its own positive control: "disabled" that nobody
        // can click is indistinguishable from a button that was never wired up.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let one = png(&root, "one.png");
        seed_index(&data, &[(one, 4)], &_stores);

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        let file = bc_core::SavedSearches::open_default()
            .unwrap()
            .path()
            .to_path_buf();
        rating_editor(&mut app, "nonsense");
        app.editor.as_mut().unwrap().name = "keeper".to_string();

        // Refused: the rule cannot match, so the click must do nothing at all.
        let save = editor_rect(&mut app, &ctx, "Save");
        click_at(&mut app, &ctx, save.center(), |app, ctx| {
            app.editor_window(ctx);
        });
        assert!(
            app.editor.is_some(),
            "the dialog closed on a save it should have refused"
        );
        assert!(
            !file.exists(),
            "a refused save still wrote the user's saved-search file"
        );

        // Accepted: the same button, the same click, one fixed rule.
        app.editor.as_mut().unwrap().rows[0].value = "3".to_string();
        let save = editor_rect(&mut app, &ctx, "Save");
        click_at(&mut app, &ctx, save.center(), |app, ctx| {
            app.editor_window(ctx);
        });
        assert!(
            app.editor.is_none(),
            "a valid save did not close the dialog"
        );
        assert!(file.exists(), "the save did not reach the disk");
        let stored = bc_core::SavedSearches::open_default().unwrap();
        assert!(
            stored.get("keeper").is_some(),
            "the collection is not in the file the sidebar reads"
        );
    }

    #[test]
    fn words_and_rules_come_back_from_the_store_as_the_same_things() {
        // The dialog's load path has to agree with its save path byte for byte,
        // or editing a collection twice drifts it: a rule that reappears
        // rewritten is a collection the user did not save.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        png(&root, "one.png");

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        {
            app.open_collection_editor("trips".to_string(), None, None);
            let editor = app.editor.as_mut().unwrap();
            editor.words = "Beach sunset".to_string();
            editor.combinator = bc_core::Combinator::Or;
            editor.scope = bc_core::SearchScope::FolderAndSubfolders(root.clone());
            editor.rows.push(RuleRow {
                field: bc_core::Field::Keyword,
                operator: bc_core::Operator::HasKeyword,
                value: "Holiday".to_string(),
            });
            editor.rows.push(RuleRow {
                field: bc_core::Field::Rating,
                operator: bc_core::Operator::LessThanOrEqual,
                value: "4".to_string(),
            });
        }
        let draft = app.editor.as_ref().unwrap().as_saved();
        assert!(
            app.save_collection_draft("trips", &draft, None),
            "{}",
            app.status
        );

        // Re-read from disk, not from the object that was just in memory.
        let reopened = bc_core::SavedSearches::open_default().unwrap();
        let stored = reopened.get("trips").expect("gone after reopen").clone();
        assert_eq!(stored, draft, "the store changed the rules on the way out");

        app.open_collection_editor(
            "trips".to_string(),
            Some("trips".to_string()),
            Some(&stored),
        );
        let editor = app.editor.as_ref().unwrap();
        // The letters the user typed, not a folded copy of them: the evaluator
        // case-folds on its own, so anything else here is a silent rewrite.
        assert_eq!(editor.words, "Beach sunset");
        assert_eq!(editor.combinator, bc_core::Combinator::Or);
        assert_eq!(
            editor.scope,
            bc_core::SearchScope::FolderAndSubfolders(root.clone()),
            "a folder scope came back as something else"
        );
        assert_eq!(editor.rows.len(), 2, "rules were lost or invented");
        assert_eq!(editor.rows[0].field, bc_core::Field::Keyword);
        assert_eq!(editor.rows[0].operator, bc_core::Operator::HasKeyword);
        assert_eq!(editor.rows[0].value, "Holiday");
        assert_eq!(editor.rows[1].field, bc_core::Field::Rating);
        assert_eq!(editor.rows[1].value, "4");
        // And rebuilding the query from the loaded rows is the stored query, not
        // a paraphrase of it.
        assert_eq!(editor.query(), stored.query, "round trip was not stable");

        // The count of conditions and words is what the dialog writes down, so a
        // collection that mixes both keeps both.
        assert_eq!(
            stored.query.predicates.len(),
            4,
            "two words and two rules should be four conditions"
        );
        // And the words still find the file they were typed for, folded or not:
        // the case is kept for the user, not for the match.
        let beach = png(&root, "beach.png");
        seed_index(&data, &[(beach, 2)], &_stores);
        let matched = bc_core::MetadataIndex::open(&bc_core::paths::metadata_index_path().unwrap())
            .unwrap()
            .search(&stored.query, &bc_core::SearchScope::Library)
            .unwrap();
        assert!(
            matched
                .iter()
                .any(|row| row.path.file_name().unwrap() == "beach.png"),
            "“Beach” with a capital stopped matching a lowercase file"
        );
    }

    #[test]
    fn the_collections_panel_opens_the_editor_and_a_saved_one_edits_itself() {
        // Reachability: the rule model and the dialog can both be correct and
        // still be a feature nobody can start. This clicks the real button.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        png(&root, "one.png");

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        app.sidebar_tab = SidebarTab::Collections;

        let button = {
            let drawn = sidebar_text(&mut app, &ctx);
            drawn
                .iter()
                .find(|(text, _)| text == "Edit rules…")
                .map(|(_, rect)| *rect)
                .unwrap_or_else(|| {
                    panic!(
                        "the Collections tab does not offer the rule editor: {:?}",
                        drawn.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>()
                    )
                })
        };
        click_sidebar(&mut app, &ctx, button.center());
        assert!(app.editor.is_some(), "the button did not open the editor");
        let words: Vec<String> = editor_text(&mut app, &ctx)
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        assert!(
            words.iter().any(|t| t == "New collection"),
            "a new collection opened something else: {words:?}"
        );

        // Save it, then reopen it from its own row and see the rules again.
        app.editor.as_mut().unwrap().name = "trips".to_string();
        app.editor.as_mut().unwrap().words = "beach".to_string();
        let draft = app.editor.as_ref().unwrap().as_saved();
        assert!(
            app.save_collection_draft("trips", &draft, None),
            "{}",
            app.status
        );
        app.editor = None;
        app.refresh_collections();
        let edit = {
            let drawn = sidebar_text(&mut app, &ctx);
            drawn
                .iter()
                .find(|(text, _)| text == "✎")
                .map(|(_, rect)| *rect)
                .unwrap_or_else(|| {
                    panic!(
                        "a saved collection offers no way back into its rules: {:?}",
                        drawn.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>()
                    )
                })
        };
        click_sidebar(&mut app, &ctx, edit.center());
        let editor = app
            .editor
            .as_ref()
            .expect("the edit button did not open the editor");
        assert_eq!(editor.words, "beach");
        assert_eq!(
            editor.editing.as_deref(),
            Some("trips"),
            "it forgot which collection it opened from"
        );
        assert_eq!(editor.name, "trips");
        let title: Vec<String> = editor_text(&mut app, &ctx)
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        assert!(
            title.iter().any(|t| t == "Edit collection"),
            "editing an existing collection is titled as a new one: {title:?}"
        );
    }

    #[test]
    fn deleting_a_collection_leaves_every_file_where_it_was() {
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let kept = png(&root, "kept.png");
        // A row, or there is nothing for the search to find. The app's own
        // pass would also create one, but on its own thread, and a test that
        // races it is a test that sometimes proves nothing.
        seed_index(&data, &[(kept.clone(), 0)], &_stores);

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        app.search_text = "kept".to_string();
        app.request_search();
        finish_search(&mut app, &ctx);
        app.collection_name = "throwaway".to_string();
        app.save_current_collection();
        assert_eq!(app.collections.len(), 1);

        app.delete_collection("throwaway");
        assert!(app.collections.is_empty(), "{:?}", app.collections);
        assert!(
            app.status.contains("No files were touched"),
            "deleting a saved search should say what it does not do: {}",
            app.status
        );
        assert!(kept.exists(), "a file disappeared with its collection");
        assert_eq!(result_names(&app), vec!["kept.png".to_string()]);
    }

    #[test]
    fn the_collections_tab_is_reachable_and_offers_to_save_the_current_search() {
        // Reachability again: a panel that exists in the code but is not wired
        // into the sidebar is not a feature the user can get to.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        png(&root, "one.png");

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);

        let drawn = sidebar_text(&mut app, &ctx);
        let tab = "◎ Collections";
        let rect = drawn
            .iter()
            .find(|(text, _)| text == tab)
            .map(|(_, r)| *r)
            .unwrap_or_else(|| {
                panic!(
                    "the Collections tab is not in the sidebar; it drew {:?}",
                    drawn.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>()
                )
            });

        click_sidebar(&mut app, &ctx, rect.center());
        assert_eq!(app.sidebar_tab, SidebarTab::Collections);
        let panel = sidebar_text(&mut app, &ctx);
        let texts: Vec<&str> = panel.iter().map(|(t, _)| t.as_str()).collect();
        assert!(
            texts.iter().any(|t| t.contains("Save this search")),
            "the collections panel did not offer to save: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("Nothing to save")),
            "with a folder on screen it should say there is nothing to save: {texts:?}"
        );

        // Search, then look again: the save control is enabled.
        app.search_text = "one".to_string();
        app.request_search();
        finish_search(&mut app, &ctx);
        let after = sidebar_text(&mut app, &ctx);
        let after_texts: Vec<&str> = after.iter().map(|(t, _)| t.as_str()).collect();
        assert!(
            !after_texts.iter().any(|t| t.contains("Nothing to save")),
            "a live search should not be told there is nothing to save: {after_texts:?}"
        );
    }

    #[test]
    fn the_toolbar_offers_rating_and_sorts_the_folder_when_it_is_chosen() {
        // Reachability, not just capability: the sort key exists in `bc-core`
        // whether or not anything lets a user select it, and picking it has to
        // actually re-order the folder in the same click.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let low = png(&root, "low.png");
        let high = png(&root, "high.png");
        seed_index(&data, &[(low.clone(), 1), (high.clone(), 5)], &_stores);

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        app.browser.set_sort(SortKey::Name, SortDir::Asc);
        let start = toolbar_rect(&mut app, &ctx, "Name");

        // Open the combo, then read the items it offered.
        click_toolbar(&mut app, &ctx, start.center());
        let offered: Vec<String> = toolbar_text(&mut app, &ctx)
            .into_iter()
            .map(|(text, _)| text)
            .collect();
        for key in SortKey::ALL {
            assert!(
                offered.iter().any(|t| t == key.label()),
                "the sort menu does not offer {}: {:?}",
                key.label(),
                offered
            );
        }
        let rating = toolbar_rect(&mut app, &ctx, "Rating");

        click_toolbar(&mut app, &ctx, rating.center());
        assert_eq!(
            app.browser.sort_key,
            SortKey::Rating,
            "clicking the menu item did not change the sort key"
        );
        // The direction the user had already chosen carries over: picking a
        // different key is not a request to reverse it, so a folder that was
        // ascending by name comes out ascending by rating — fewest stars first.
        assert_eq!(
            order(&app),
            ["low.png", "high.png"],
            "the key changed but the folder did not re-order"
        );
        assert!(
            app.status.contains("all indexed"),
            "a tag sort over a fully indexed folder should say so: {}",
            app.status
        );

        // And the direction button says which end is on top, then flips it.
        let flip = toolbar_rect(&mut app, &ctx, "1★");
        click_toolbar(&mut app, &ctx, flip.center());
        assert_eq!(
            order(&app),
            ["high.png", "low.png"],
            "the reverse put it back"
        );
        assert_eq!(app.browser.sort_dir, SortDir::Desc);
        toolbar_rect(&mut app, &ctx, "5★");
    }

    #[test]
    fn the_tags_menu_filters_the_grid_to_files_the_index_can_vouch_for() {
        // The filter is only worth having if setting it visibly changes the
        // grid, and if the one thing it does silently — hiding files it knows
        // nothing about — is said out loud.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let starred = png(&root, "starred.png");
        let dull = png(&root, "dull.png");
        let unread = png(&root, "unread.png");

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        let mut values = bc_core::fs::TagValues::default();
        values.insert(
            starred.clone(),
            bc_core::fs::TagValue {
                rating: 4,
                label: None,
                flag: None,
            },
        );
        values.insert(
            dull.clone(),
            bc_core::fs::TagValue {
                rating: 0,
                label: None,
                flag: None,
            },
        );
        app.browser.set_tag_values(values);
        assert!(
            app.browser.tags.get(&unread).is_none(),
            "the file meant to have no row was given one"
        );

        let tags = toolbar_rect(&mut app, &ctx, "Tags");
        click_toolbar(&mut app, &ctx, tags.center());
        // The floor sits inside a combo *within* the menu, so the combo has to
        // be opened before the item can be clicked — three bare "any" labels in
        // one menu would make this address nothing, which is why they are
        // "any rating", "any label" and "any flag".
        let rating_combo = toolbar_rect(&mut app, &ctx, "any rating");
        click_toolbar(&mut app, &ctx, rating_combo.center());
        let floor = toolbar_rect(&mut app, &ctx, "3★ or more");
        click_toolbar(&mut app, &ctx, floor.center());

        assert_eq!(
            app.browser.tag_filter.min_rating,
            Some(3),
            "clicking the floor did not set it"
        );
        // Two visible: the four-star file and the subfolder. `dull.png` is
        // rated below the floor and `unread.png` has no row, so the filter
        // cannot vouch for either — and the folder stays so a filter cannot
        // trap the user in a view they'd have to leave by hand.
        assert_eq!(app.browser.visible_len(), 2, "visible: {:?}", order(&app));
        assert!(
            app.status.contains("1 not indexed and hidden"),
            "the status must separate a low rating from an unknown one: {}",
            app.status
        );

        // Picking an item out of a combo inside a menu closes the combo, not
        // the menu, so the reset is still where it was and the button has
        // already changed its label to say a filter is in force.
        let resting = egui::pos2(tags.center().x, tags.bottom() + 24.0);
        toolbar_rect_at(&mut app, &ctx, "Tags ●", resting);
        let off = toolbar_rect_at(&mut app, &ctx, "Clear tag filter", resting);
        click_toolbar(&mut app, &ctx, off.center());
        assert!(!app.browser.tag_filter.is_active(), "clear left it on");
        assert_eq!(
            app.browser.visible_len(),
            4,
            "clearing did not show the folder again"
        );
    }

    #[test]
    fn starring_a_photo_moves_it_in_a_rating_sort_without_waiting_for_a_pass() {
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let first = png(&root, "first.png");
        let second = png(&root, "second.png");
        seed_index(&data, &[(first.clone(), 1), (second.clone(), 2)], &_stores);

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        app.browser.set_sort(SortKey::Rating, SortDir::Desc);
        assert_eq!(order(&app), ["second.png", "first.png"]);

        // The panel's Save path: write the tags, then the row. Waiting for the
        // watcher would leave the starred file sitting below the two-star one,
        // which reads as the save having been ignored.
        let tags = metadata_edit::TagState {
            stars: 5,
            label: String::new(),
            flag: bc_metadata::ratings::Flag::None,
            keywords: vec![],
        };
        metadata_edit::save_tags(&first, &tags).unwrap();
        app.index_saved_tags(&first);

        assert_eq!(
            order(&app),
            ["first.png", "second.png"],
            "the row did not move"
        );
        assert_eq!(app.browser.tags.get(&first).unwrap().rating, 5);
        // And it moved in the database, not just in memory, so the next folder
        // open sees the same order.
        let path = bc_core::paths::metadata_index_path().unwrap();
        let reread = bc_core::MetadataIndex::open(&path).unwrap();
        assert_eq!(reread.get(&first).unwrap().unwrap().rating, 5);
    }

    #[test]
    fn a_finished_pass_loads_the_values_a_first_visit_could_not_have_had() {
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        for name in ["a.png", "b.png", "c.png"] {
            png(&root, name);
        }

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        assert!(
            app.browser.tags.is_empty(),
            "an empty library should not arrive pre-tagged"
        );
        assert!(
            app.tag_sort_status().contains("not indexed yet"),
            "a tag sort over an unread folder must say so: {}",
            app.tag_sort_status()
        );

        finish_indexing(&mut app, &ctx);
        assert_eq!(
            app.browser.tags.len(),
            3,
            "the finished pass never refreshed the grid"
        );
        assert!(
            app.tag_sort_status().contains("all indexed"),
            "{}",
            app.tag_sort_status()
        );
    }

    #[test]
    fn the_tags_filter_hides_unindexed_files_and_says_so() {
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let starred = png(&root, "starred.png");
        let unread = png(&root, "unread.png");

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        // The values the filter reads are set here rather than left to what the
        // app loaded, because opening the folder also queued a pass over it on
        // a worker thread: whether `unread.png` had gained a row by the time
        // this asserted would be a race, and a test that races with the thing
        // it is meant to exercise proves nothing either way.
        let mut values = bc_core::fs::TagValues::default();
        values.insert(
            starred.clone(),
            bc_core::fs::TagValue {
                rating: 4,
                label: None,
                flag: None,
            },
        );
        app.browser.set_tag_values(values);
        assert!(
            app.browser.tags.get(&unread).is_none(),
            "the file meant to have no row was given one"
        );

        app.browser.tag_filter = bc_core::fs::TagFilter {
            min_rating: Some(3),
            label: None,
            flag: None,
        };
        let shown = app.browser.set_tag_filter(app.browser.tag_filter.clone());
        // Two, not one: the four-star file passes, the unrated file has no row
        // to claim three stars, and the subfolder stays so a filter cannot
        // trap the user in a view they have to navigate out of by hand.
        assert_eq!(shown, 2, "visible: {:?}", app.browser.visible_indices());
        let status = app.tag_filter_status(shown);
        assert!(
            status.contains("1 not indexed and hidden"),
            "the status must count what the filter hid for want of a row: {status}"
        );
    }

    #[test]
    fn every_sort_key_names_the_end_that_comes_first() {
        // A user reading "5★" on the button must be able to trust that the
        // five-star files are on top, and `Asc`/`Desc` say nothing about that.
        for key in SortKey::ALL {
            let (asc, _) = sort_direction_label(key, SortDir::Asc);
            let (desc, hover) = sort_direction_label(key, SortDir::Desc);
            assert!(!asc.is_empty() && !desc.is_empty(), "{key:?} has no label");
            assert_ne!(asc, desc, "{key:?} says {asc} in both directions");
            assert!(hover.contains("first"), "hover text for {key:?}: {hover}");
        }
        assert_eq!(sort_direction_label(SortKey::Rating, SortDir::Desc).0, "5★");
        assert_eq!(sort_direction_label(SortKey::Rating, SortDir::Asc).0, "1★");
        assert_eq!(
            sort_direction_label(SortKey::Modified, SortDir::Asc).0,
            "oldest"
        );
        assert_eq!(
            sort_direction_label(SortKey::Size, SortDir::Desc).0,
            "largest"
        );
    }

    /// Answers the search worker for the current request, without the frame
    /// clock: `poll_search` is what a running window calls every frame.
    fn finish_search(app: &mut BridgeApp, ctx: &egui::Context) {
        for _ in 0..400 {
            app.poll_search(ctx);
            if app.search_waiting.is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("the search never answered");
    }

    fn result_names(app: &BridgeApp) -> Vec<String> {
        app.browser
            .entries
            .iter()
            .map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_search_box_is_drawn_and_taking_typed_characters() {
        // Reachability, the way the sort menu was proven: a field that exists
        // in the struct is not a feature. The box has to be drawn, take focus
        // on a click, accept characters, and schedule the query its keystrokes
        // implied.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let target = png(&root, "beachwalk.png");
        // A row for it, so the answer is a match rather than an explanation of
        // why there is none.
        seed_index(&data, &[(target.clone(), 3)], &_stores);

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);

        let drawn = toolbar_text(&mut app, &ctx);
        let hint = "search tags, names, cameras…";
        let (_, rect) = drawn
            .iter()
            .find(|(text, _)| text == hint)
            .unwrap_or_else(|| {
                panic!(
                    "the search box was not drawn; the bar drew {:?}",
                    drawn.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>()
                )
            });
        // The scope control belongs next to it, or the user cannot tell what
        // the word will be matched against.
        toolbar_rect(&mut app, &ctx, "this folder");

        drive_toolbar(
            &mut app,
            &ctx,
            rect.center(),
            vec![button(rect.center(), true), button(rect.center(), false)],
        );
        for letter in "beach".chars() {
            drive_toolbar(
                &mut app,
                &ctx,
                rect.center(),
                vec![egui::Event::Text(letter.to_string())],
            );
        }
        assert_eq!(app.search_text, "beach", "the box did not take the typing");
        assert!(
            app.search_due.is_some(),
            "a keystroke did not schedule the query"
        );
        // Enter runs it now rather than waiting out the debounce.
        drive_toolbar(
            &mut app,
            &ctx,
            rect.center(),
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            }],
        );
        assert_eq!(app.search_due, None, "Enter should fire immediately");
        finish_search(&mut app, &ctx);
        assert!(app.browser.is_searching());
        assert!(
            app.status.contains("1 file(s)"),
            "the answer should be counted: {}",
            app.status
        );
        assert!(
            !app.status.contains("not indexed yet"),
            "a covered folder should not carry the coverage warning: {}",
            app.status
        );
    }

    #[test]
    fn a_typed_word_replaces_the_grid_with_the_files_the_index_agrees_with() {
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let beach = png(&root, "sunset.png");
        let forest = png(&root, "pines.png");
        let by_name = png(&root, "beachcomber.png");
        seed_index(
            &data,
            &[
                (beach.clone(), 0),
                (forest.clone(), 0),
                (by_name.clone(), 0),
            ],
            &_stores,
        );

        // Give two of them something to be found by, through the same
        // projection the indexer writes.
        let db = bc_core::paths::metadata_index_path().unwrap();
        let index = bc_core::MetadataIndex::open(&db).unwrap();
        for (path, title) in [(&beach, "beach evening"), (&forest, "pines morning")] {
            let mut meta = bc_metadata::Metadata::new();
            meta.set_text(bc_metadata::NS_DC, "title", title.to_string());
            index.index_file(path, &meta).unwrap();
        }

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        assert_eq!(
            result_names(&app).len(),
            3,
            "the folder should be listed first"
        );

        app.search_text = "beach".to_string();
        app.request_search();
        assert!(
            app.browser.entries.is_empty(),
            "the grid kept the folder while the answer was pending"
        );
        finish_search(&mut app, &ctx);

        assert!(app.browser.is_searching());
        let names = result_names(&app);
        assert_eq!(
            names,
            vec!["beachcomber.png".to_string(), "sunset.png".to_string()],
            "the search answered with the wrong files: {names:?}"
        );
        assert!(
            app.status.contains("2 file(s)"),
            "the status should count what the query found: {}",
            app.status
        );
    }

    #[test]
    fn thumbnails_only_hides_sidecar_files_from_search_results_too() {
        // Searching the index returns sidecars, because a `.xmp` is a file and
        // its row carries the same tags as the photo beside it. The remedy is
        // not a special case in search: it is the pre-existing "Thumbnails
        // only" toggle, which filters whatever the browser is showing. This
        // test pins that the toggle still applies when what is showing is a
        // result set, because the two features were built in different slices
        // and nothing else connects them.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let photo = png(&root, "photo.png");
        let sidecar = {
            let p = root.join("photo.xmp");
            std::fs::write(
                &p,
                b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='x'></rdf:RDF></x:xmpmeta>",
            )
            .unwrap();
            p
        };
        seed_index(&data, &[(photo.clone(), 0), (sidecar.clone(), 0)], &_stores);

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        app.search_text = "photo".to_string();
        app.request_search();
        finish_search(&mut app, &ctx);
        assert_eq!(
            app.browser.entries.len(),
            2,
            "both files should answer the query: {:?}",
            result_names(&app)
        );

        app.thumbs_only = true;
        let shown = app.display_indices().len();
        assert_eq!(
            shown, 1,
            "the toggle did not reach the results, only folder listings"
        );

        app.thumbs_only = false;
        assert_eq!(app.display_indices().len(), 2, "the toggle is not one-way");
    }

    #[test]
    fn an_unmatched_search_says_the_query_found_nothing_not_that_the_folder_is_empty() {
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        png(&root, "a.png");

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        app.search_text = "nothingmatches".to_string();
        app.request_search();
        finish_search(&mut app, &ctx);

        assert!(app.browser.entries.is_empty());
        assert!(app.status.contains("0 file(s)"), "{}", app.status);
        assert!(
            !app.status.contains("This folder is empty"),
            "an empty result must not be reported as an empty folder: {}",
            app.status
        );
    }

    #[test]
    fn a_folder_scope_search_tells_you_what_the_index_has_never_seen() {
        // The number a user needs when a search comes back short: files that
        // have no row cannot match anything, and that is a fact about the
        // index, not about their contents.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let seen = png(&root, "seen.png");
        let unseen = png(&root, "unseen.png");
        seed_index(&data, &[(seen.clone(), 2)], &_stores);

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        app.search_scope = SearchScopeChoice::Folder;
        app.search_text = "anything".to_string();
        app.request_search();
        finish_search(&mut app, &ctx);
        assert!(
            app.status.contains("1 of 2 files here are not indexed yet"),
            "{}",
            app.status
        );

        // And once everything has a row, the warning goes away rather than
        // staying as permanent noise.
        let db = bc_core::paths::metadata_index_path().unwrap();
        let index = bc_core::MetadataIndex::open(&db).unwrap();
        index.index_path(&unseen).unwrap();
        app.request_search();
        finish_search(&mut app, &ctx);
        assert!(
            !app.status.contains("not indexed yet"),
            "the coverage warning outlived the gap: {}",
            app.status
        );
    }

    #[test]
    fn navigating_to_another_folder_ends_the_search_and_empties_the_box() {
        // The browser exits search mode on its own when it re-lists; this
        // checks the field agrees, because a stale word in the box next to an
        // unrelated folder reads as a filter nobody applied.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        png(&first, "beach.png");
        png(&second, "mountain.png");

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &first, &data, &_stores);
        app.search_text = "beach".to_string();
        app.request_search();
        finish_search(&mut app, &ctx);
        assert!(app.browser.is_searching());

        app.open_root(&second);
        assert!(!app.browser.is_searching());
        assert!(app.search_text.is_empty(), "the box kept the old query");
        assert_eq!(result_names(&app), vec!["mountain.png".to_string()]);
    }

    #[test]
    fn escaping_a_search_brings_the_folder_back() {
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        png(&root, "one.png");

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        app.search_text = "one".to_string();
        app.request_search();
        finish_search(&mut app, &ctx);
        assert!(app.browser.is_searching());

        // No widget focused, which is the state in which Esc belongs to the
        // application rather than to a text field. The key has to arrive as an
        // event: `key_pressed` reads the frame's input, not a call.
        let raw = egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            }],
            ..Default::default()
        };
        let _ = ctx.run(raw, |ctx| app.handle_search_shortcuts(ctx));
        assert!(!app.browser.is_searching(), "Esc left the search running");
        assert!(app.search_text.is_empty(), "the box kept the cleared word");
        assert_eq!(result_names(&app), vec!["one.png".to_string()]);
    }

    #[test]
    fn dropping_files_while_searching_says_no_and_copies_nothing() {
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        png(&root, "here.png");
        let outsider = temp.path().join("outsider.png");
        std::fs::write(&outsider, b"payload").unwrap();

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        app.search_text = "here".to_string();
        app.request_search();
        finish_search(&mut app, &ctx);

        let raw = egui::RawInput {
            dropped_files: vec![egui::DroppedFile {
                path: Some(outsider.clone()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let _ = ctx.run(raw, |ctx| app.handle_os_drops(ctx));

        assert!(
            app.status.contains("Clear the search"),
            "the refusal is what has to be visible: {}",
            app.status
        );
        assert!(
            !root.join("outsider.png").exists(),
            "files were copied into a folder that was not on screen"
        );
    }

    #[test]
    fn details_columns_line_up_without_overlapping_or_running_off_the_row() {
        let (left, width) = (100.0_f32, 800.0_f32);
        let columns = details_columns();
        let mut previous = details_column_x(left, width, 0);
        assert_eq!(previous, left + 8.0, "the first column keeps a left margin");
        for index in 1..columns.len() {
            let x = details_column_x(left, width, index);
            assert!(x > previous, "column {index} must sit right of the last");
            previous = x;
        }
        let last = details_column_x(left, width, columns.len() - 1);
        assert!(
            last < left + width,
            "the last column must stay inside the row: {last} of {}",
            left + width
        );
        // A narrow panel must not push columns past the row either: the shares
        // are fractions, so they shrink with the width.
        let narrow = details_column_x(0.0, 240.0, columns.len() - 1);
        assert!(narrow < 240.0, "narrow panel overflow: {narrow}");
    }

    #[test]
    fn a_plugin_sees_the_same_tags_the_panel_shows() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("photo.jpg");
        // EXIF-bearing JPEG (the stub parses as JPEG with no EXIF properties)
        // plus tags that live only in the sidecar.
        std::fs::write(&file, [0xFFu8, 0xD8, 0xFF, 0xD9]).unwrap();
        let sidecar = bc_metadata::xmp::sidecar_path(&file);
        let mut side = bc_metadata::Metadata::new();
        side.set_rating(bc_metadata::ratings::Rating::new(5));
        side.set_color_label(bc_metadata::ratings::ColorLabel::Blue);
        bc_metadata::xmp::write_xmp_file(&sidecar, &side).unwrap();

        let panel =
            metadata_edit::TagState::from_metadata(&metadata_edit::read_panel_metadata(&file));
        let host = UiPluginHost {
            selected: vec![file.clone()],
            folder: Some(temp.path().to_path_buf()),
        };
        let json = bc_core::plugins::PluginHost::read_metadata(&host, &file)
            .expect("the plugin host must read the file");
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        // The plugin host serializes the same model the panel reads, so the two
        // agree by construction; assert the properties the old chain hid.
        let text = value.to_string();
        assert!(
            text.contains("Rating"),
            "rating missing for plugins: {text}"
        );
        assert!(text.contains("Blue"), "label missing for plugins: {text}");
        assert_eq!(panel.stars, 5);
        assert_eq!(panel.label, "Blue");

        // An unreadable path is still an error to a plugin, not empty JSON.
        assert!(
            bc_core::plugins::PluginHost::read_metadata(&host, &temp.path().join("nope.jpg"))
                .is_err()
        );
    }

    #[test]
    fn list_navigation_follows_filtered_order_and_clamps() {
        let mut selection = Selection::new();
        assert_eq!(navigation_target(&[1, 4, 7], &selection, 1), Some(1));
        selection.select_only(4);
        assert_eq!(navigation_target(&[1, 4, 7], &selection, 1), Some(7));
        assert_eq!(navigation_target(&[1, 4, 7], &selection, -1), Some(1));
        assert_eq!(navigation_target(&[1, 4, 7], &selection, -1), Some(1));
        assert_eq!(navigation_target(&[], &selection, 1), None);
    }

    #[test]
    fn modified_time_is_displayed_as_utc() {
        use std::time::{Duration, UNIX_EPOCH};
        assert_eq!(format_modified(Some(UNIX_EPOCH)), "1970-01-01 00:00");
        assert_eq!(
            format_modified(Some(UNIX_EPOCH + Duration::from_secs(1_704_067_200))),
            "2024-01-01 00:00"
        );
        assert_eq!(format_modified(None), "—");
    }

    #[test]
    fn node_label_anchors_roots_and_names_children() {
        assert_eq!(node_label(Path::new("/")), "⌂ /");
        assert_eq!(node_label(Path::new("/tmp/demo")), "demo");
    }

    #[test]
    fn human_size_scales_units() {
        assert_eq!(human_size(9), "9 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }

    fn exif_fixture(pairs: &[(&str, &str)]) -> bc_metadata::Metadata {
        let mut m = bc_metadata::Metadata::new();
        for (key, value) in pairs {
            let (ns, name) = key.split_once('.').unwrap();
            let ns = match ns {
                "exif" => bc_metadata::NS_EXIF,
                "gps" => bc_metadata::NS_GPS,
                other => other,
            };
            m.set_text(ns, name, (*value).to_string());
        }
        m
    }

    #[test]
    fn camera_line_deduplicates_a_make_prefix() {
        let prefixed = exif_fixture(&[("exif.Make", "Canon"), ("exif.Model", "Canon EOS-1D X")]);
        assert_eq!(format_camera(&prefixed).as_deref(), Some("Canon EOS-1D X"));
        let split = exif_fixture(&[("exif.Make", "Canon"), ("exif.Model", "EOS-1D X")]);
        assert_eq!(format_camera(&split).as_deref(), Some("Canon EOS-1D X"));
        let model_only = exif_fixture(&[("exif.Model", "Z9")]);
        assert_eq!(format_camera(&model_only).as_deref(), Some("Z9"));
        assert_eq!(format_camera(&exif_fixture(&[])), None);
    }

    #[test]
    fn exif_size_and_gps_rows_join_fields_or_stay_quiet() {
        let meta = exif_fixture(&[
            ("exif.PixelWidth", "5184"),
            ("exif.PixelHeight", "3456"),
            ("gps.Latitude", "47.60621 N"),
            ("gps.Longitude", "122.32000 W"),
            ("gps.Altitude", "120.0 m"),
        ]);
        assert_eq!(exif_image_size(&meta).as_deref(), Some("5184 × 3456"));
        assert_eq!(
            exif_gps(&meta).as_deref(),
            Some("47.60621 N, 122.32000 W · 120.0 m")
        );
        // A half-written GPS pair or a missing dimension shows nothing.
        assert_eq!(
            exif_gps(&exif_fixture(&[("gps.Latitude", "47.6 N")])).as_deref(),
            None
        );
        assert_eq!(
            exif_image_size(&exif_fixture(&[("exif.PixelWidth", "5184")])).as_deref(),
            None
        );
    }

    #[test]
    fn drop_mode_maps_alt_to_copy() {
        assert_eq!(drop_mode(false), TransferMode::Move);
        assert_eq!(drop_mode(true), TransferMode::Copy);
    }

    /// Drive one frame of the real thumbnail grid.
    fn drive_grid(app: &mut BridgeApp, ctx: &egui::Context, alt: bool, events: Vec<egui::Event>) {
        let mut raw = egui::RawInput::default();
        raw.modifiers.alt = alt;
        raw.time = Some(next_frame_time());
        raw.predicted_dt = 0.05;
        raw.time = Some(next_frame_time());
        raw.predicted_dt = 0.05;
        raw.screen_rect = Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(900.0, 700.0),
        ));
        raw.events = events;
        // The frame's FullOutput is not needed: these tests read the app's
        // own state and the filesystem for their assertions.
        let _ = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.content_grid(ui));
        });
    }

    /// Find a point inside the folder's grid cell that both reports the folder
    /// as the drop target and can start a drag. egui resolves gestures against
    /// the previous frame's widget geometry, so the hover frame comes first.
    fn grid_point(app: &mut BridgeApp, ctx: &egui::Context, folder: &Path) -> Option<egui::Pos2> {
        for y in (10..700).step_by(17) {
            for x in (10..900).step_by(17) {
                let pos = egui::pos2(x as f32, y as f32);
                app.hovered_folder = None;
                drive_grid(app, ctx, false, vec![egui::Event::PointerMoved(pos)]);
                if app.hovered_folder.as_deref() != Some(folder) {
                    continue;
                }
                egui::DragAndDrop::clear_payload(ctx);
                drive_grid(app, ctx, false, vec![button(pos, true)]);
                drive_grid(
                    app,
                    ctx,
                    false,
                    vec![egui::Event::PointerMoved(pos + egui::vec2(0.0, 12.0))],
                );
                let published = egui::DragAndDrop::has_any_payload(ctx);
                egui::DragAndDrop::clear_payload(ctx);
                drive_grid(app, ctx, false, vec![button(pos, false)]);
                if published {
                    return Some(pos);
                }
            }
        }
        None
    }

    /// Find a point inside the folder's grid cell using hover alone, so no
    /// earlier click can disturb egui's click counting in the test that
    /// follows.
    fn grid_hover_point(
        app: &mut BridgeApp,
        ctx: &egui::Context,
        folder: &Path,
    ) -> Option<egui::Pos2> {
        for y in (10..700).step_by(17) {
            for x in (10..900).step_by(17) {
                let pos = egui::pos2(x as f32, y as f32);
                app.hovered_folder = None;
                drive_grid(app, ctx, false, vec![egui::Event::PointerMoved(pos)]);
                if app.hovered_folder.as_deref() == Some(folder) {
                    return Some(pos);
                }
            }
        }
        None
    }

    #[test]
    fn a_double_click_still_opens_a_grid_folder() {
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        let source = root.join("a_dir");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&source).unwrap();

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        let from = grid_hover_point(&mut app, &ctx, &source).expect("source cell located");

        // One event per frame, the way the window delivers them: the second
        // press/release pair must descend into the folder.
        drive_grid(&mut app, &ctx, false, vec![egui::Event::PointerMoved(from)]);
        drive_grid(&mut app, &ctx, false, vec![button(from, true)]);
        drive_grid(&mut app, &ctx, false, vec![button(from, false)]);
        drive_grid(&mut app, &ctx, false, vec![button(from, true)]);
        drive_grid(&mut app, &ctx, false, vec![button(from, false)]);
        assert_eq!(
            app.browser.root.as_deref(),
            Some(source.as_path()),
            "the double click should have opened the folder: {}",
            app.status
        );
    }

    #[test]
    fn a_plain_grid_click_selects_and_moves_nothing() {
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        let source = root.join("a_dir");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&source).unwrap();

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        // Hover-only locating: pressing here would already open egui's
        // double-click window before the click under test.
        let from = grid_hover_point(&mut app, &ctx, &source).expect("source cell located");

        // A press and release without moving is a click, not a drag: the cell
        // must be selected and nothing may be moved. The new drag surface has
        // drag sense only, so the click still belongs to the cell's own
        // widgets.
        drive_grid(&mut app, &ctx, false, vec![button(from, true)]);
        drive_grid(&mut app, &ctx, false, vec![button(from, false)]);
        let index = app
            .browser
            .entries
            .iter()
            .position(|entry| entry.path == source)
            .expect("folder is listed");
        assert!(
            app.browser.selection.contains(index),
            "a plain click should select the cell: {}",
            app.status
        );
        assert!(source.exists(), "a click moved nothing");
        assert_eq!(
            app.browser.root.as_deref(),
            Some(root.as_path()),
            "a click must not navigate either"
        );
    }

    #[test]
    fn dragging_a_folder_cell_onto_another_folder_cell_moves_it_on_disk() {
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        let source = root.join("a_dir");
        let target = root.join("z_dir");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        let from = grid_point(&mut app, &ctx, &source).expect("source cell located");
        let to = grid_point(&mut app, &ctx, &target).expect("target cell located");

        drive_grid(&mut app, &ctx, false, vec![button(from, true)]);
        drive_grid(
            &mut app,
            &ctx,
            false,
            vec![egui::Event::PointerMoved(from + egui::vec2(0.0, 12.0))],
        );
        drive_grid(&mut app, &ctx, false, vec![egui::Event::PointerMoved(to)]);
        drive_grid(&mut app, &ctx, false, vec![button(to, false)]);

        assert!(
            target.join("a_dir").is_dir(),
            "the grid drop should have moved the folder in: {}",
            app.status
        );
        assert!(!source.exists(), "the source must not be left behind");
        assert!(app.status.starts_with("Moved 1 item(s)"), "{}", app.status);
    }

    #[test]
    fn dragging_a_folder_row_onto_another_folder_moves_it_on_disk() {
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        let source = root.join("a_dir");
        let target = root.join("z_dir");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(source.join("inner.bin"), b"payload").unwrap();

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        let from = point_over_folder(&mut app, &ctx, &source).expect("source row located");
        let to = point_over_folder(&mut app, &ctx, &target).expect("target row located");

        drive_list(&mut app, &ctx, false, vec![button(from, true)]);
        // Move away from the press point: that is what turns the press into a
        // drag and publishes the selection as the payload.
        drive_list(
            &mut app,
            &ctx,
            false,
            vec![egui::Event::PointerMoved(from + egui::vec2(0.0, 8.0))],
        );
        drive_list(&mut app, &ctx, false, vec![egui::Event::PointerMoved(to)]);
        drive_list(&mut app, &ctx, false, vec![button(to, false)]);

        assert!(
            target.join("a_dir").is_dir(),
            "the drop should have moved the folder into the target: {}",
            app.status
        );
        assert!(!source.exists(), "the source must not be left behind");
        assert!(
            target.join("a_dir").join("inner.bin").is_file(),
            "the tree moves with it"
        );
        assert!(app.status.starts_with("Moved 1 item(s)"), "{}", app.status);
    }

    #[test]
    fn alt_drag_copies_instead_of_moving() {
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        let source = root.join("a_dir");
        let target = root.join("z_dir");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        let from = point_over_folder(&mut app, &ctx, &source).expect("source row located");
        let to = point_over_folder(&mut app, &ctx, &target).expect("target row located");

        drive_list(&mut app, &ctx, true, vec![button(from, true)]);
        drive_list(
            &mut app,
            &ctx,
            true,
            vec![egui::Event::PointerMoved(from + egui::vec2(0.0, 8.0))],
        );
        drive_list(&mut app, &ctx, true, vec![egui::Event::PointerMoved(to)]);
        drive_list(&mut app, &ctx, true, vec![button(to, false)]);

        assert!(
            target.join("a_dir").is_dir(),
            "the copy landed: {}",
            app.status
        );
        assert!(source.exists(), "a copy leaves the original in place");
        assert!(app.status.starts_with("Copied 1 item(s)"), "{}", app.status);
    }

    #[test]
    fn dropping_on_a_file_row_moves_nothing_and_dropping_on_itself_is_rejected() {
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        let source = root.join("a_dir");
        let other_dir = root.join("z_dir");
        let file_row = root.join("b_file.jpg");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&other_dir).unwrap();
        std::fs::write(&file_row, b"asset").unwrap();

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        let from = point_over_folder(&mut app, &ctx, &source).expect("source row located");

        // Rows are folders first, then files by name, so the row after `z_dir`
        // is `b_file.jpg`: only folders report a hover target, so the app must
        // not move anything onto it.
        let sibling = point_over_folder(&mut app, &ctx, &other_dir).expect("sibling row located");
        let onto_file = point_after(from, sibling);
        app.hovered_folder = None;
        drive_list(
            &mut app,
            &ctx,
            false,
            vec![egui::Event::PointerMoved(onto_file)],
        );
        assert_eq!(app.hovered_folder, None, "a file is not a drop target");

        drive_list(&mut app, &ctx, false, vec![button(from, true)]);
        drive_list(
            &mut app,
            &ctx,
            false,
            vec![egui::Event::PointerMoved(from + egui::vec2(0.0, 8.0))],
        );
        drive_list(
            &mut app,
            &ctx,
            false,
            vec![egui::Event::PointerMoved(onto_file)],
        );
        drive_list(&mut app, &ctx, false, vec![button(onto_file, false)]);
        assert!(source.is_dir(), "nothing moved onto a file row");
        assert!(
            !other_dir.join("a_dir").exists(),
            "and nothing moved into the sibling folder either"
        );

        // Releasing a folder onto itself is rejected by the core before any
        // filesystem change, and says so.
        drive_list(&mut app, &ctx, false, vec![button(from, true)]);
        drive_list(
            &mut app,
            &ctx,
            false,
            vec![egui::Event::PointerMoved(from + egui::vec2(0.0, 8.0))],
        );
        drive_list(&mut app, &ctx, false, vec![button(from, false)]);
        assert!(
            app.status.starts_with("Drop rejected"),
            "self-drop reported: {}",
            app.status
        );
        assert!(source.is_dir(), "a rejected drop leaves the tree untouched");
    }

    #[test]
    fn os_drops_prefer_the_hovered_folder_then_the_open_root() {
        let hovered = Path::new("/library/keepers");
        let root = Path::new("/library");
        assert_eq!(
            os_drop_target(Some(hovered), Some(root)).as_deref(),
            Some(hovered)
        );
        assert_eq!(
            os_drop_target(None, Some(root)).as_deref(),
            Some(root),
            "with nothing hovered the open folder takes the files"
        );
        assert_eq!(os_drop_target(None, None), None);
    }

    /// Send one key press to the real metadata form and report what the panel
    /// did. Tab traversal is how the editor is verified: it needs no pixel
    /// guesses, and it is the same path a keyboard user takes.
    fn press_key_in_form(
        app: &mut BridgeApp,
        ctx: &egui::Context,
        path: &Path,
        mtime: i64,
        key: egui::Key,
        repeats: usize,
    ) {
        let target = path.to_path_buf();
        for _ in 0..repeats {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(420.0, 900.0),
                )),
                time: Some(next_frame_time()),
                events: vec![egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::default(),
                }],
                ..Default::default()
            };
            // The frame's FullOutput is not needed: this test reads the app's
            // own state and the sidecar on disk. The form is drawn inside a
            // scroll area because that is how the panel builds it, and egui
            // walks the focus chain in the order widgets register it.
            let _ = ctx.run(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    egui::ScrollArea::vertical()
                        .show(ui, |ui| app.metadata_form_ui(ui, &target, mtime));
                });
            });
        }
    }

    /// Walk the real panel with Tab and activate each candidate with Enter,
    /// stopping when `predicate` holds. Returns the number of steps it took,
    /// which is how the test says which control answered.
    fn walk_until(
        app: &mut BridgeApp,
        ctx: &egui::Context,
        file: &Path,
        mtime: i64,
        limit: usize,
        predicate: impl Fn(&BridgeApp) -> bool,
    ) -> Option<usize> {
        for step in 1..=limit {
            press_key_in_form(app, ctx, file, mtime, egui::Key::Tab, 1);
            press_key_in_form(app, ctx, file, mtime, egui::Key::Enter, 1);
            if predicate(app) {
                return Some(step);
            }
        }
        None
    }

    #[test]
    fn opening_a_folder_queues_an_index_pass_that_lands_in_the_database() {
        // The claim this slice exists to keep: indexing is reachable by
        // navigating, not only by a test calling into `bc-core`. Everything the
        // pass does after this point is the queue's, so what is asserted here
        // is that `open_root` produced a job and that job produced rows.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(root.join("nested")).unwrap();
        let first = root.join("first.jpg");
        let second = root.join("second.jpg");
        std::fs::write(&first, [0xFFu8, 0xD8, 0xFF, 0xD9]).unwrap();
        std::fs::write(&second, [0xFFu8, 0xD8, 0xFF, 0xD9]).unwrap();
        // A folder is listed but never indexed: the pass covers the folder's
        // own files, the same non-recursive reach the grid shows.
        let deeper = root.join("nested").join("deeper.jpg");
        std::fs::write(&deeper, [0xFFu8, 0xD8, 0xFF, 0xD9]).unwrap();

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        assert!(app.index_job.is_some(), "opening a folder queued nothing");
        let job = app.index_job.expect("job id");
        // Frames, not sleeps: `poll_jobs` is what a running window calls.
        for _ in 0..400 {
            app.poll_jobs(&ctx);
            if app.index_job.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(app.index_job, None, "the pass never finished");

        // Ask the same helper the app used rather than spelling the path out:
        // `isolated_app` nests its own `data` under the isolation root, and a
        // guessed second answer to "where is the index" is how two databases
        // for one index get written in the first place.
        let database = bc_core::paths::metadata_index_path().unwrap();
        let index =
            bc_core::MetadataIndex::open(&database).expect("the database the app just wrote");
        assert!(index.contains(&first), "first.jpg never indexed");
        assert!(index.contains(&second), "second.jpg never indexed");
        assert!(
            !index.contains(&deeper),
            "a folder pass indexed a file the listing never contained"
        );
        assert!(
            app.jobs_notified.contains(&job),
            "the finished pass never reached the status bar: {}",
            app.status
        );
        assert!(
            app.status.contains("Indexed"),
            "the status bar should name the pass it finished, got {:?}",
            app.status
        );
    }

    #[test]
    fn navigating_mid_pass_indexes_the_folder_the_user_ended_up_in() {
        // The bug this design was chosen to remove: superseding a pass used to
        // cancel the running one and then return without queueing the new
        // folder, so navigating during a pass dropped the destination.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&left).unwrap();
        std::fs::create_dir_all(&right).unwrap();
        for i in 0..24 {
            std::fs::write(
                left.join(format!("l{i:02}.jpg")),
                [0xFFu8, 0xD8, 0xFF, 0xD9],
            )
            .unwrap();
        }
        let landed = right.join("here.jpg");
        std::fs::write(&landed, [0xFFu8, 0xD8, 0xFF, 0xD9]).unwrap();

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &left, &data, &_stores);
        let first_job = app.index_job.expect("first pass queued");
        // Navigate while that pass is still running.
        app.open_root(&right);
        let second_job = app.index_job.expect("second pass queued");
        assert_ne!(first_job, second_job, "navigating reused the old pass");

        for _ in 0..400 {
            app.poll_jobs(&ctx);
            if app.index_job.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let database = bc_core::paths::metadata_index_path().unwrap();
        let index = bc_core::MetadataIndex::open(&database).expect("database");
        assert!(
            index.contains(&landed),
            "the destination folder of a superseded pass was never indexed"
        );
    }

    /// Index rows carrying keywords, written through the same `index_file` the
    /// indexer uses, so a suggestion test cannot disagree with the indexer about
    /// what a keyword row looks like.
    fn seed_keywords(
        isolated: &Path,
        rows: &[(PathBuf, &[&str])],
        stores: &std::sync::MutexGuard<'static, ()>,
    ) {
        isolated_env(isolated, stores);
        let path = bc_core::paths::metadata_index_path().unwrap();
        let index = bc_core::MetadataIndex::open(&path).unwrap();
        for (file, keywords) in rows {
            std::fs::write(file, [0xFFu8, 0xD8, 0xFF, 0xD9]).unwrap();
            let mut meta = bc_metadata::Metadata::new();
            for keyword in *keywords {
                bc_metadata::keywords::add_keyword(&mut meta, keyword);
            }
            index.index_file(file, &meta).unwrap();
        }
    }

    /// Collapse the debounce and let the worker answer, the way `finish_search`
    /// does for the box. Returns when the list on screen describes what is typed.
    fn finish_suggestions(app: &mut BridgeApp, ctx: &egui::Context) {
        for _ in 0..500 {
            if app.typed_prefix().is_empty() {
                return;
            }
            app.suggest_due = Some(Instant::now());
            app.tick_suggestions(ctx);
            let typed = app.typed_prefix();
            if app.suggest_waiting.is_none() && app.suggestions_prefix == typed {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("the keyword suggestions never answered");
    }

    /// Draw the metadata panel for one file and return every string it wrote.
    fn panel_texts(
        app: &mut BridgeApp,
        ctx: &egui::Context,
        path: &Path,
        mtime: i64,
    ) -> Vec<String> {
        let target = path.to_path_buf();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(420.0, 1_400.0),
            )),
            time: Some(next_frame_time()),
            ..Default::default()
        };
        // Two passes: the panel lives in a scroll area whose contents are only
        // laid out after the first, and a single pass can miss them.
        let _ = ctx.run(raw.clone(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    app.metadata_form_ui(ui, &target, mtime);
                });
            });
        });
        let out = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    app.metadata_form_ui(ui, &target, mtime);
                });
            });
        });
        out.shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                _ => None,
            })
            .collect()
    }

    /// Draw the right-hand panel the way the app does, which chooses the bulk
    /// form whenever more than one file is selected.
    fn right_panel_texts(app: &mut BridgeApp, ctx: &egui::Context) -> Vec<String> {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(420.0, 1_600.0),
            )),
            time: Some(next_frame_time()),
            ..Default::default()
        };
        let _ = ctx.run(raw.clone(), |ctx| {
            egui::SidePanel::right("probe").show(ctx, |ui| app.metadata_panel(ui));
        });
        let out = ctx.run(raw, |ctx| {
            egui::SidePanel::right("probe").show(ctx, |ui| app.metadata_panel(ui));
        });
        out.shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                _ => None,
            })
            .collect()
    }

    /// Click a control in the right-hand panel. Written as its own pass
    /// sequence rather than through `click_at`, because this panel is measured
    /// and clicked with a frame clock: the pointer has to be *moved* in the
    /// clicking pass itself, or egui treats the press as arriving at wherever
    /// the previous frame left it and the button never hears about it.
    fn click_right_panel(app: &mut BridgeApp, ctx: &egui::Context, wanted: &str) {
        let raw = |events: Vec<egui::Event>| egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(420.0, 1_600.0),
            )),
            time: Some(next_frame_time()),
            events,
            ..Default::default()
        };
        let at = {
            let out = ctx.run(raw(vec![]), |ctx| {
                egui::SidePanel::right("probe").show(ctx, |ui| app.metadata_panel(ui));
            });
            let drawn: Vec<(String, egui::Rect)> = out
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) => Some((
                        text.galley.text().to_string(),
                        egui::Rect::from_min_size(text.pos, text.galley.size()),
                    )),
                    _ => None,
                })
                .collect();
            drawn
                .iter()
                .find(|(text, _)| text == wanted)
                .map(|(_, rect)| rect.center())
                .unwrap_or_else(|| {
                    panic!(
                        "“{wanted}” was not drawn in the right panel; it drew {:?}",
                        drawn
                            .iter()
                            .map(|(text, _)| text.as_str())
                            .collect::<Vec<_>>()
                    )
                })
        };
        let _ = ctx.run(raw(vec![egui::Event::PointerMoved(at)]), |ctx| {
            egui::SidePanel::right("probe").show(ctx, |ui| app.metadata_panel(ui));
        });
        let _ = ctx.run(
            raw(vec![
                egui::Event::PointerMoved(at),
                button(at, true),
                button(at, false),
            ]),
            |ctx| {
                egui::SidePanel::right("probe").show(ctx, |ui| app.metadata_panel(ui));
            },
        );
    }

    /// Click the panel control whose text is exactly `wanted`.
    fn click_panel(
        app: &mut BridgeApp,
        ctx: &egui::Context,
        path: &Path,
        mtime: i64,
        wanted: &str,
    ) {
        let target = path.to_path_buf();
        let drawn = {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(420.0, 1_400.0),
                )),
                time: Some(next_frame_time()),
                ..Default::default()
            };
            let out = ctx.run(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        app.metadata_form_ui(ui, &target, mtime);
                    });
                });
            });
            let rects: Vec<(String, egui::Rect)> = out
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) => Some((
                        text.galley.text().to_string(),
                        egui::Rect::from_min_size(text.pos, text.galley.size()),
                    )),
                    _ => None,
                })
                .collect();
            rects
        };
        let at = drawn
            .iter()
            .find(|(text, _)| text == wanted)
            .map(|(_, rect)| rect.center())
            .unwrap_or_else(|| {
                panic!(
                    "“{wanted}” was not drawn in the metadata panel; it drew {:?}",
                    drawn
                        .iter()
                        .map(|(text, _)| text.as_str())
                        .collect::<Vec<_>>()
                )
            });
        click_at(app, ctx, at, |app, ctx| {
            let target = target.clone();
            egui::CentralPanel::default().show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    app.metadata_form_ui(ui, &target, mtime);
                });
            });
        });
    }

    #[test]
    fn the_keyword_box_offers_what_the_library_already_calls_it() {
        // The feature in one assertion: a word typed in lower case finds the
        // spelling the library uses, with the count that makes it worth using.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let photo = root.join("photo.jpg");
        std::fs::write(&photo, [0xFFu8, 0xD8, 0xFF, 0xD9]).unwrap();
        seed_keywords(
            &data,
            &[
                (root.join("a.jpg"), &["Beach"][..]),
                (root.join("b.jpg"), &["Beach"]),
                (root.join("c.jpg"), &["Beach|2024"]),
                (root.join("d.jpg"), &["Mountain"]),
            ],
            &_stores,
        );

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        let mtime = 1_700_000_000_000_000_000;
        // Load the form once: setting the draft afterwards survives the draw,
        // while a first load clears it.
        panel_texts(&mut app, &ctx, &photo, mtime);

        app.keyword_draft = "bea".to_string();
        // The row is what says which prefix the box is asking about, so it has
        // to be drawn before anything can be scheduled — the same frame the
        // live app spends between a keystroke and a query.
        panel_texts(&mut app, &ctx, &photo, mtime);
        // Drawing the row set a deadline and asked for nothing: the query
        // belongs to the worker, and a scan inside the panel draw is a frozen
        // right-hand column.
        assert!(
            app.suggest_due.is_some(),
            "typing into the box queued no deadline"
        );
        assert!(
            app.suggest_waiting.is_none() && app.suggestions.is_empty(),
            "the frame that typed ran the query itself"
        );
        app.tick_suggestions(&ctx);
        assert!(
            app.suggest_waiting.is_none(),
            "the deadline fired a frame early"
        );

        finish_suggestions(&mut app, &ctx);
        let words = panel_texts(&mut app, &ctx, &photo, mtime);
        assert!(
            words.iter().any(|t| t == "In use:"),
            "no completion row was drawn: {words:?}"
        );
        assert!(
            words.iter().any(|t| t == "Beach (2)"),
            "the library's own word and its count are missing: {words:?}"
        );
        assert!(
            words.iter().any(|t| t == "Beach|2024 (1)"),
            "the hierarchy below it was not offered: {words:?}"
        );
        assert!(
            !words.iter().any(|t| t.starts_with("Mountain")),
            "a word unrelated to the prefix was shown: {words:?}"
        );
        // With candidates on screen the row speaks for itself: any status line
        // beside it says either "this is new" or "already on this file", and
        // both are contradicted by the buttons one line above.
        assert!(
            !words.iter().any(|t| t.contains("new keyword")),
            "the panel called a word it just listed as in use a new keyword: {words:?}"
        );
        assert!(
            !words.iter().any(|t| t == "Already on this file."),
            "the panel said nothing was left to add while listing things to add: {words:?}"
        );
        assert!(
            !words.iter().any(|t| t.contains("Checking the library")),
            "the panel claimed to still be working after answering: {words:?}"
        );
    }

    #[test]
    fn clicking_a_keyword_completion_stores_the_library_spelling_not_the_typed_one() {
        // The point of the whole row: "bea" clicked becomes "Beach", so the
        // library keeps one keyword rather than gaining a near-duplicate that
        // no search can join.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let photo = root.join("photo.jpg");
        std::fs::write(&photo, [0xFFu8, 0xD8, 0xFF, 0xD9]).unwrap();
        seed_keywords(
            &data,
            &[
                (root.join("a.jpg"), &["Beach"]),
                (root.join("b.jpg"), &["Beach"]),
            ],
            &_stores,
        );

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        let mtime = 1_700_000_000_000_000_000;
        panel_texts(&mut app, &ctx, &photo, mtime);
        app.keyword_draft = "bea".to_string();
        panel_texts(&mut app, &ctx, &photo, mtime);
        finish_suggestions(&mut app, &ctx);

        click_panel(&mut app, &ctx, &photo, mtime, "Beach (2)");
        assert_eq!(
            app.tags_form.current().keywords,
            vec!["Beach".to_string()],
            "the completion stored what was typed rather than what is in use"
        );
        assert!(
            app.keyword_draft.is_empty(),
            "the box kept the text of a keyword it already added"
        );
    }

    #[test]
    fn a_word_no_indexed_file_uses_is_said_to_be_new() {
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let photo = root.join("photo.jpg");
        std::fs::write(&photo, [0xFFu8, 0xD8, 0xFF, 0xD9]).unwrap();
        seed_keywords(&data, &[(root.join("a.jpg"), &["Beach"])], &_stores);

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        let mtime = 1_700_000_000_000_000_000;
        panel_texts(&mut app, &ctx, &photo, mtime);
        app.keyword_draft = "zzz".to_string();
        panel_texts(&mut app, &ctx, &photo, mtime);
        finish_suggestions(&mut app, &ctx);

        let words = panel_texts(&mut app, &ctx, &photo, mtime);
        let line = words
            .iter()
            .find(|t| t.contains("indexed file"))
            .cloned()
            .unwrap_or_default();
        assert!(
            line.contains("new keyword"),
            "an unseen word was not said to be new: {words:?}"
        );
        assert!(
            !words.iter().any(|t| t == "In use:"),
            "a completion row appeared for a word nothing uses: {words:?}"
        );
    }

    #[test]
    fn an_unreachable_index_is_reported_and_never_called_a_new_keyword() {
        // The two absences that must not look alike. If "no suggestions" is
        // reported as "this word is new" while the index is offline, the user is
        // told to invent a keyword the library already has.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("library");
        std::fs::create_dir_all(&root).unwrap();
        let photo = root.join("photo.jpg");
        std::fs::write(&photo, [0xFFu8, 0xD8, 0xFF, 0xD9]).unwrap();
        // A file standing where the data directory would have to be.
        let blocker = temp.path().join("data");
        std::fs::write(&blocker, b"not a directory").unwrap();
        std::env::set_var("HOME", temp.path());
        std::env::set_var("APPDATA", &blocker);
        std::env::set_var("XDG_DATA_HOME", &blocker);

        let ctx = egui::Context::default();
        let mut app = BridgeApp::new(&ctx, Some(root.display().to_string()));
        assert!(
            app.index_error.is_some(),
            "the fixture did not make the index unreachable"
        );
        let mtime = 1_700_000_000_000_000_000;
        panel_texts(&mut app, &ctx, &photo, mtime);
        app.keyword_draft = "bea".to_string();
        // The row is what registers the question, so it has to be drawn once
        // with the new text before anything can be answered.
        panel_texts(&mut app, &ctx, &photo, mtime);
        finish_suggestions(&mut app, &ctx);

        let words = panel_texts(&mut app, &ctx, &photo, mtime);
        assert!(
            words
                .iter()
                .any(|t| t.contains("Library suggestions unavailable")),
            "the offline index was not said out loud: {words:?}"
        );
        assert!(
            !words.iter().any(|t| t.contains("new keyword")),
            "an unreachable index told the user the word was new: {words:?}"
        );
    }

    #[test]
    fn a_completion_already_on_the_file_is_not_offered_back() {
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let photo = root.join("photo.jpg");
        std::fs::write(&photo, [0xFFu8, 0xD8, 0xFF, 0xD9]).unwrap();
        seed_keywords(
            &data,
            &[
                (photo.clone(), &["Beach"]),
                (root.join("b.jpg"), &["Beach"]),
            ],
            &_stores,
        );

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        let mtime = 1_700_000_000_000_000_000;
        panel_texts(&mut app, &ctx, &photo, mtime);
        assert!(
            app.tags_form.add_keyword("Beach"),
            "the fixture could not put the keyword on the file"
        );
        app.keyword_draft = "bea".to_string();
        panel_texts(&mut app, &ctx, &photo, mtime);
        finish_suggestions(&mut app, &ctx);

        let words = panel_texts(&mut app, &ctx, &photo, mtime);
        assert!(
            !words.iter().any(|t| t.starts_with("Beach (")),
            "the box offered to add a keyword the file already carries: {words:?}"
        );
        assert!(
            words.iter().any(|t| t == "Already on this file."),
            "and it said nothing about why: {words:?}"
        );
        assert!(
            !words.iter().any(|t| t.contains("new keyword")),
            "a word the library holds was called new: {words:?}"
        );
    }

    #[test]
    fn the_bulk_keyword_box_asks_the_same_question_as_the_single_one() {
        // The expensive version of the mistake: a misspelling stamped across a
        // selection is one keyword per file, and nothing later merges them. The
        // row has to be in that panel too, not only beside one photo.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        seed_keywords(
            &data,
            &[
                (root.join("a.jpg"), &["Beach"]),
                (root.join("b.jpg"), &["Beach"]),
                (root.join("c.jpg"), &["Beach"]),
            ],
            &_stores,
        );

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        app.right_tab = RightTab::Metadata;
        let count = app.browser.len();
        assert!(count >= 2, "the fixture needs a multi-file selection");
        app.browser.selection.select_all(count);

        // Draw once so the bulk row registers the prefix it wants an answer for.
        let before = right_panel_texts(&mut app, &ctx);
        assert!(
            before.iter().any(|t| t == "Add keyword:"),
            "the bulk panel does not have the keyword box: {before:?}"
        );
        app.bulk_keyword = "bea".to_string();
        right_panel_texts(&mut app, &ctx);
        finish_suggestions(&mut app, &ctx);

        let words = right_panel_texts(&mut app, &ctx);
        assert!(
            words.iter().any(|t| t == "Beach (3)"),
            "the bulk box offered no library vocabulary: {words:?}"
        );

        click_right_panel(&mut app, &ctx, "Beach (3)");
        assert_eq!(
            app.bulk_keyword, "Beach",
            "the bulk stamp kept the typed spelling instead of the one in use"
        );
    }

    #[test]
    fn a_trailing_separator_asks_about_children_and_only_offers_children() {
        // The hierarchical convention is the reason keywords are worth a
        // vocabulary check at all, and a trailing `|` is the question about it.
        // The stored-keyword normaliser drops that separator, so if the box used
        // it the answer would be the parent and its siblings — a plausible-looking
        // list that is not what was asked.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let photo = root.join("photo.jpg");
        std::fs::write(&photo, [0xFFu8, 0xD8, 0xFF, 0xD9]).unwrap();
        seed_keywords(
            &data,
            &[
                (root.join("a.jpg"), &["Beach", "Beach|2024", "Beaches"]),
                (root.join("b.jpg"), &["Beach|Family"]),
            ],
            &_stores,
        );

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        let mtime = 1_700_000_000_000_000_000;
        panel_texts(&mut app, &ctx, &photo, mtime);
        app.keyword_draft = "Beach|".to_string();
        panel_texts(&mut app, &ctx, &photo, mtime);
        finish_suggestions(&mut app, &ctx);

        let words = panel_texts(&mut app, &ctx, &photo, mtime);
        assert!(
            words.iter().any(|t| t == "Beach|2024 (1)"),
            "the children were not offered: {words:?}"
        );
        assert!(
            words.iter().any(|t| t == "Beach|Family (1)"),
            "and only some of them arrived: {words:?}"
        );
        assert!(
            !words
                .iter()
                .any(|t| t.starts_with("Beach (") || t.starts_with("Beaches (")),
            "a child-prefix answered with the parent and its siblings: {words:?}"
        );

        // And the empty case keeps its own sentence: the separator is not a
        // keyword, so it must not be reported as one about to be created.
        app.keyword_draft = "Beach|Nothing|".to_string();
        panel_texts(&mut app, &ctx, &photo, mtime);
        finish_suggestions(&mut app, &ctx);
        let after = panel_texts(&mut app, &ctx, &photo, mtime);
        assert!(
            after.iter().any(|t| t.contains("Nothing is filed under")),
            "an empty branch was described as a new keyword: {after:?}"
        );
        assert!(
            !after.iter().any(|t| t.contains("new keyword")),
            "the box offered to create a word that was never typed: {after:?}"
        );
    }

    #[test]
    fn an_answer_for_another_word_is_refused_even_when_it_lands() {
        // Two guards, both needed: the sequence rejects an answer that is no
        // longer outstanding, and the prefix gate keeps an answer off the screen
        // once the box has moved on.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let photo = root.join("photo.jpg");
        std::fs::write(&photo, [0xFFu8, 0xD8, 0xFF, 0xD9]).unwrap();
        seed_keywords(&data, &[(root.join("a.jpg"), &["Beach"])], &_stores);

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        let mtime = 1_700_000_000_000_000_000;
        panel_texts(&mut app, &ctx, &photo, mtime);

        app.keyword_draft = "bea".to_string();
        panel_texts(&mut app, &ctx, &photo, mtime);
        app.suggest_due = Some(Instant::now());
        app.tick_suggestions(&ctx);
        assert!(
            app.suggest_waiting.is_some(),
            "the request was never put in flight"
        );
        // A reply for a request nobody outstanding matches.
        app.suggest_reply_tx
            .send(SuggestAnswer {
                seq: 4242,
                results: Ok(vec![bc_core::KeywordSuggestion {
                    keyword: "Wrongword".to_string(),
                    files: 1,
                }]),
            })
            .unwrap();
        app.tick_suggestions(&ctx);
        assert!(
            app.suggestions.iter().all(|s| s.keyword != "Wrongword"),
            "an answer for a stale sequence was taken: {:?}",
            app.suggestions
        );

        // Let the real answer land, so the panel genuinely has a list for "bea".
        finish_suggestions(&mut app, &ctx);
        let before = panel_texts(&mut app, &ctx, &photo, mtime);
        assert!(
            before.iter().any(|t| t == "Beach (1)"),
            "the answer never reached the panel it was asked for: {before:?}"
        );

        // Now the box moves on without another answer arriving. The list the
        // panel holds describes a word that is no longer typed, so showing it
        // would offer a completion for "bea" while the user is asking about
        // "beach|" — the one state in which a clicked keyword is guaranteed to
        // be the wrong one.
        app.keyword_draft = "beach|".to_string();
        let words = panel_texts(&mut app, &ctx, &photo, mtime);
        assert!(
            !words.iter().any(|t| t.starts_with("Beach (")),
            "a list for the previous word was shown under the new one: {words:?}"
        );
        assert!(
            words.iter().any(|t| t.contains("Checking the library")),
            "and it did not say it was re-asking: {words:?}"
        );
    }

    #[test]
    fn an_empty_keyword_box_asks_nothing_of_the_library() {
        // The debounce is not the only saving: an empty box must not be a query
        // at all, or every panel draw on an unselected file costs a scan.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let photo = root.join("photo.jpg");
        std::fs::write(&photo, [0xFFu8, 0xD8, 0xFF, 0xD9]).unwrap();
        seed_keywords(&data, &[(root.join("a.jpg"), &["Beach"])], &_stores);

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        let mtime = 1_700_000_000_000_000_000;
        for _ in 0..5 {
            app.tick_suggestions(&ctx);
        }
        assert_eq!(app.suggest_seq, 0, "an empty box queried the index");
        assert!(app.suggest_due.is_none(), "an empty box queued a scan");
        assert!(app.suggestions_prefix.is_empty());

        // Answer a real prefix, so there is a list and a prefix on record.
        panel_texts(&mut app, &ctx, &photo, mtime);
        app.keyword_draft = "bea".to_string();
        panel_texts(&mut app, &ctx, &photo, mtime);
        finish_suggestions(&mut app, &ctx);
        let asked = app.suggest_seq;
        assert!(asked > 0, "a typed prefix never reached the worker");

        // Clearing the box must not ask another question. This is the half that
        // makes the guard load-bearing: with the empty-prefix branch removed,
        // an empty prefix differs from the last answered one, so the debounce
        // fires and a scan runs for nothing — and `add_keyword` would refuse the
        // word the scan was asked about.
        for blank in ["", "   "] {
            app.keyword_draft = blank.to_string();
            // Drawing the row is how a box says what it holds, cleared included.
            panel_texts(&mut app, &ctx, &photo, mtime);
            for _ in 0..4 {
                app.suggest_due = Some(Instant::now());
                app.tick_suggestions(&ctx);
            }
            assert_eq!(
                app.suggest_seq, asked,
                "{blank:?} asked the library a second question"
            );
        }
        assert!(
            app.suggestions.is_empty(),
            "the list for a cleared box was kept"
        );
        let words = panel_texts(&mut app, &ctx, &photo, mtime);
        assert!(
            !words.iter().any(|t| t.contains("Checking the library")),
            "a blank box claimed to be working: {words:?}"
        );
        assert!(
            !words.iter().any(|t| t == "In use:"),
            "a cleared box still offered completions: {words:?}"
        );
    }

    #[test]
    fn an_unusable_index_is_reported_rather_than_left_invisible() {
        // Where the "index offline" state comes from: the data directory
        // cannot be created, so the app says so instead of letting a later
        // search look like an empty library.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("library");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("shot.jpg"), [0xFFu8, 0xD8, 0xFF, 0xD9]).unwrap();
        // A *file* standing where the data directory would have to be.
        let blocker = temp.path().join("data");
        std::fs::write(&blocker, b"not a directory").unwrap();
        std::env::set_var("HOME", temp.path());
        std::env::set_var("APPDATA", &blocker);
        std::env::set_var("XDG_DATA_HOME", &blocker);

        let ctx = egui::Context::default();
        let app = BridgeApp::new(&ctx, Some(root.display().to_string()));
        assert!(
            app.index_error.is_some(),
            "a database that could not be opened must be recorded"
        );
        assert!(app.index.is_none(), "no handle should exist");
        assert!(
            app.index_job.is_none(),
            "nothing should be queued when the index is unavailable"
        );
        assert!(
            app.status.contains("index unavailable"),
            "the status bar should say why there is no index, got {:?}",
            app.status
        );
    }

    #[test]
    fn every_tag_control_in_the_panel_does_something_when_activated() {
        // The lock is taken by the helper below, but this test also builds a
        // `BridgeApp`, which resolves the metadata index path from the
        // environment on folder open — so it must not run beside another test
        // that is doing the same thing.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("photo.jpg");
        std::fs::write(&file, b"bytes").unwrap();
        let mtime = 1_700_000_000_000_000_000;

        // Colour label: walk until a label name appears on the form.
        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        let label_step = walk_until(&mut app, &ctx, &file, mtime, 30, |app| {
            !app.tags_form.current().label.is_empty()
        });
        assert!(
            label_step.is_some(),
            "no label chip responded to activation in 30 steps"
        );

        // Flag: continue walking until Approve or Reject is set.
        let flag_step = walk_until(&mut app, &ctx, &file, mtime, 12, |app| {
            app.tags_form.current().flag != bc_metadata::ratings::Flag::None
        });
        assert!(
            flag_step.is_some(),
            "no flag button responded to activation after the label chips"
        );

        // Keywords: type into the field, then walk to the Add button.
        app.keyword_draft = "Clients|ACME".to_string();
        let keyword_step = walk_until(&mut app, &ctx, &file, mtime, 14, |app| {
            !app.tags_form.current().keywords.is_empty()
        });
        assert!(
            keyword_step.is_some(),
            "the keyword Add control did not take the typed keyword"
        );
        assert_eq!(
            app.tags_form.current().keywords,
            vec!["Clients|ACME".to_string()],
            "the hierarchy separator survives typing"
        );

        // Removing a keyword is a click on its chip: the chip is before the
        // field and Add in the walk, so reach it by wrapping with Shift+Tab.
        let mut removed = false;
        for _ in 0..16 {
            let mut raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(420.0, 900.0),
                )),
                time: Some(next_frame_time()),
                events: vec![egui::Event::Key {
                    key: egui::Key::Tab,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers {
                        shift: true,
                        ..Default::default()
                    },
                }],
                ..Default::default()
            };
            raw.events.push(egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            });
            let target = file.to_path_buf();
            let _ = ctx.run(raw.clone(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    egui::ScrollArea::vertical()
                        .show(ui, |ui| app.metadata_form_ui(ui, &target, mtime));
                });
            });
            if app.tags_form.current().keywords.is_empty() {
                removed = true;
                break;
            }
        }
        assert!(
            removed,
            "Shift+Tab back onto the keyword chip should remove it"
        );
    }

    #[test]
    fn the_tag_editor_is_reachable_and_usable_from_the_keyboard() {
        // The lock is taken by the helper below, but this test also builds a
        // `BridgeApp`, which resolves the metadata index path from the
        // environment on folder open — so it must not run beside another test
        // that is doing the same thing.
        let _stores = STORE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("photo.jpg");
        std::fs::write(&file, b"original image bytes").unwrap();
        let mtime = 1_700_000_000_000_000_000;

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data, &_stores);
        let sidecar = bc_metadata::xmp::sidecar_path(&file);

        // Tab to the first star and press Enter: a rating must appear.
        let mut rated = false;
        for _ in 0..12 {
            press_key_in_form(&mut app, &ctx, &file, mtime, egui::Key::Tab, 1);
            press_key_in_form(&mut app, &ctx, &file, mtime, egui::Key::Enter, 1);
            if app.tags_form.current().stars > 0 {
                rated = true;
                break;
            }
        }
        assert!(
            rated,
            "Tab plus Enter should set a rating; the panel is not keyboard reachable"
        );
        assert!(
            !sidecar.exists(),
            "editing alone must not write; Save is the write"
        );

        // Walk to Save and activate it. Space rather than Enter: egui's
        // TextEdit ends editing on Enter and drops focus, which would send the
        // walk back to the first widget every time it reached a text field.
        // Space is the other key egui activates a focused button with, and it
        // leaves focus where it is.
        let mut wrote = false;
        for _ in 0..60 {
            press_key_in_form(&mut app, &ctx, &file, mtime, egui::Key::Tab, 1);
            press_key_in_form(&mut app, &ctx, &file, mtime, egui::Key::Space, 1);
            if sidecar.exists() {
                wrote = true;
                break;
            }
        }
        assert!(wrote, "Save should be reachable from the keyboard");
        let saved =
            metadata_edit::TagState::from_metadata(&metadata_edit::read_panel_metadata(&file));
        assert_eq!(
            saved.stars,
            app.tags_form.current().stars,
            "what the form showed is what the sidecar holds"
        );
        assert_eq!(
            std::fs::read(&file).unwrap(),
            b"original image bytes",
            "tags never touch the image bytes"
        );
    }

    #[test]
    fn unnamed_desktop_drops_are_counted_not_dropped_silently() {
        let named = egui::DroppedFile {
            path: Some(PathBuf::from("/tmp/asset.jpg")),
            ..Default::default()
        };
        let anonymous = egui::DroppedFile {
            path: None,
            name: "clipboard-image".to_owned(),
            ..Default::default()
        };
        assert_eq!(
            os_drop_paths(&[named.clone(), anonymous.clone()]),
            (vec![PathBuf::from("/tmp/asset.jpg")], 1)
        );
        // A drop of nothing but anonymous payloads must still be reported.
        assert_eq!(os_drop_paths(&[anonymous]), (Vec::new(), 1));
        assert_eq!(os_drop_paths(&[]), (Vec::new(), 0));
    }

    #[test]
    fn move_drop_relocates_and_copy_drop_duplicates() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        let source = temp.path().join("asset.bin");
        std::fs::write(&source, b"asset").unwrap();

        let status = apply_transfer(std::slice::from_ref(&source), &folder, drop_mode(false));
        assert!(status.starts_with("Moved 1 item(s)"), "{status}");
        assert!(!source.exists(), "a move leaves the source behind");
        assert_eq!(
            std::fs::read(folder.join("asset.bin")).unwrap(),
            b"asset" as &[u8]
        );

        let other = temp.path().join("other.bin");
        std::fs::write(&other, b"copy").unwrap();
        let status = apply_transfer(std::slice::from_ref(&other), &folder, drop_mode(true));
        assert!(status.starts_with("Copied 1 item(s)"), "{status}");
        assert!(other.exists(), "a copy keeps the source");
        assert_eq!(
            std::fs::read(folder.join("other.bin")).unwrap(),
            b"copy" as &[u8]
        );
    }

    #[test]
    fn move_drop_into_same_folder_skips() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("asset.bin");
        std::fs::write(&file, b"asset").unwrap();
        let status = apply_transfer(std::slice::from_ref(&file), temp.path(), drop_mode(false));
        assert!(status.contains("skipped 1"), "{status}");
        assert!(file.exists());
    }

    #[test]
    fn folder_drop_into_itself_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("parent");
        let child = parent.join("child");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(child.join("asset.bin"), b"asset").unwrap();

        let status = apply_transfer(std::slice::from_ref(&parent), &child, drop_mode(false));
        assert!(status.starts_with("Drop rejected"), "{status}");
        assert!(
            child.join("asset.bin").exists(),
            "a rejection touches nothing"
        );
    }

    #[test]
    fn collision_reports_the_first_failure_and_keeps_the_rest() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("taken.bin"), b"existing").unwrap();
        let taken = temp.path().join("taken.bin");
        std::fs::write(&taken, b"new").unwrap();
        let fresh = temp.path().join("fresh.bin");
        std::fs::write(&fresh, b"fresh").unwrap();

        let status = apply_transfer(&[taken, fresh], &folder, drop_mode(true));
        assert!(status.contains("1 failed"), "{status}");
        assert!(
            status.contains("already exists") || status.contains("PathAlreadyExists"),
            "{status}"
        );
        assert_eq!(
            std::fs::read(folder.join("taken.bin")).unwrap(),
            b"existing" as &[u8],
            "a collision never overwrites the existing file"
        );
        assert!(folder.join("fresh.bin").exists(), "the batch continues");
    }
}
