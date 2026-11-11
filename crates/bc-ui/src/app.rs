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
use std::time::{Duration, Instant};

use bc_core::browser::BrowserState;
use bc_core::export::{ExportFormat, ExportQueue, ExportRequest};
use bc_core::favorites::Favorites;
use bc_core::file_ops::{copy_path, delete_path, move_path, rename_path};
use bc_core::fs::{SortDir, SortKey};
use bc_core::recents::Recents;
use bc_core::selection::Selection;
use bc_core::trash::TrashBin;
use bc_core::watcher::Watcher;
use bc_thumbnails::{CacheStore, Thumbnail};
use eframe::egui;

use crate::batch_rename_ui;
use crate::metadata_edit::{self, BulkReport, MetadataForm};
use crate::viewer::{self, ViewerState, ZoomMode, FULL_EDGE, PREVIEW_EDGE};

/// Default grid cell width; the status-bar zoom slider ranges from this.
const DEFAULT_CELL: f32 = 150.0;
const MIN_CELL: f32 = 96.0;
const MAX_CELL: f32 = 320.0;
const GRID_SPACING: f32 = 8.0;
const THUMB_MAX_EDGE: u32 = 256;
/// Coalesce watcher event bursts into a single refresh.
const WATCHER_DEBOUNCE: Duration = Duration::from_millis(300);
const LIST_ROW_HEIGHT: f32 = 28.0;
/// Sidebar "Recent" section cap.
const SIDEBAR_RECENTS: usize = 6;

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

struct ThumbJob {
    path: PathBuf,
    mtime_ns: i64,
    size: u64,
    max_edge: u32,
    /// Decode through the SQLite cache (grid/preview tiers) or always decode
    /// fresh (the uncached full-resolution tier).
    cached: bool,
}

struct ThumbResult {
    path: PathBuf,
    max_edge: u32,
    thumb: Option<Thumbnail>,
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
    /// Multi-selection bulk editor buffers.
    bulk_description: String,
    bulk_artist: String,
    bulk_copyright: String,
    bulk_report: Option<BulkReport>,

    // ------------------------------------------------------------- export --
    /// Session-lifetime export job queue (all logic lives in `bc-core`).
    export_queue: ExportQueue,
    /// Open Export dialog parameters, if any.
    export_dialog: Option<ExportDialog>,
    /// Jobs list window visibility.
    jobs_window: bool,
    /// Count of jobs still queued/running, refreshed by the poll.
    export_active: usize,
    /// Jobs whose completion summary has already been put in the status bar.
    jobs_notified: HashSet<u64>,
}

impl BridgeApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        root: Option<String>,
        cache_path: Option<PathBuf>,
    ) -> Self {
        let (job_tx, job_rx) = channel::<ThumbJob>();
        let (res_tx, res_rx) = channel::<ThumbResult>();
        spawn_thumb_worker(cache_path, job_rx, res_tx);

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
            dims_cache: HashMap::new(),
            exif_cache: HashMap::new(),
            metadata_form: MetadataForm::default(),
            metadata_form_path: None,
            metadata_form_mtime: 0,
            metadata_status: None,
            bulk_description: String::new(),
            bulk_artist: String::new(),
            bulk_copyright: String::new(),
            bulk_report: None,
            export_queue: ExportQueue::new(bc_core::export::default_worker_count()),
            export_dialog: None,
            jobs_window: false,
            export_active: 0,
            jobs_notified: HashSet::new(),
        };

        if let Some(root) = root {
            app.open_root(Path::new(&root));
        } else if let Ok(cwd) = std::env::current_dir() {
            app.open_root(&cwd);
        }

        cc.egui_ctx.set_visuals(egui::Visuals::dark());
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

        match self.sidebar_tab {
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

    /// Editable metadata form for the single selection: dirty rows are
    /// tinted, Save writes through the #5 policy and Revert restores the
    /// values loaded from disk.
    fn metadata_form_ui(&mut self, ui: &mut egui::Ui, path: &Path, mtime: i64) {
        let needs_load =
            self.metadata_form_path.as_deref() != Some(path) || self.metadata_form_mtime != mtime;
        if needs_load {
            let meta = self.exif_for(path, mtime).unwrap_or_default();
            self.metadata_form.load(&meta);
            self.metadata_form_path = Some(path.to_path_buf());
            self.metadata_form_mtime = mtime;
            self.metadata_status = None;
        }

        ui.add(egui::Label::new(egui::RichText::new("Metadata").weak().small()).selectable(false));
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

        let dirty_count = self.metadata_form.dirty_count();
        ui.horizontal(|ui| {
            let dirty = self.metadata_form.is_dirty();
            if ui
                .add_enabled(dirty, egui::Button::new("Save").small())
                .clicked()
            {
                let changes = self.metadata_form.dirty_changes();
                let report = metadata_edit::apply_changes(
                    std::slice::from_ref(&path.to_path_buf()),
                    &changes,
                    bc_metadata::WritePolicy::Auto,
                );
                match report.failures.first() {
                    Some(failure) => {
                        self.metadata_status = Some(format!("Save failed: {}", failure.error));
                    }
                    None => {
                        self.exif_cache.remove(path);
                        let saved = metadata_edit::read_panel_metadata(path);
                        self.metadata_form.load(&saved);
                        self.metadata_form_mtime =
                            mtime_ns(std::fs::metadata(path).and_then(|m| m.modified()).ok());
                        self.metadata_status = Some("Saved.".to_string());
                        self.status = format!("Saved metadata for {}", path.display());
                    }
                }
            }
            if ui
                .add_enabled(dirty, egui::Button::new("Revert").small())
                .clicked()
            {
                self.metadata_form.revert();
                self.metadata_status = None;
            }
            if dirty_count > 0 {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("{dirty_count} unsaved"))
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

            let has_value = !self.bulk_description.trim().is_empty()
                || !self.bulk_artist.trim().is_empty()
                || !self.bulk_copyright.trim().is_empty();
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
                    let report = metadata_edit::apply_changes(
                        &paths,
                        &changes,
                        bc_metadata::WritePolicy::Auto,
                    );
                    self.status = format!(
                        "Bulk metadata: {} saved, {} failed",
                        report.succeeded,
                        report.failures.len()
                    );
                    self.exif_cache.clear();
                    self.metadata_form_path = None;
                    self.bulk_report = Some(report);
                }
                if ui.add(egui::Button::new("Clear").small()).clicked() {
                    self.bulk_description.clear();
                    self.bulk_artist.clear();
                    self.bulk_copyright.clear();
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
                        let response = ui
                            .allocate_ui_with_layout(
                                egui::vec2(cell_width, row_height),
                                egui::Layout::top_down(egui::Align::Center),
                                |cell_ui| {
                                    self.cell(
                                        cell_ui, idx, &path, is_dir, size, selected, cell_width,
                                    )
                                },
                            )
                            .inner;

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
            ui.horizontal(|ui| {
                ui.strong("Name");
                ui.add_space(ui.available_width() * 0.48);
                ui.strong("Type");
                ui.add_space(48.0);
                ui.strong("Size");
                ui.add_space(42.0);
                ui.strong("Modified");
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
                    egui::Sense::click(),
                );
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
                    let widths = [
                        rect.width() * 0.52,
                        rect.width() * 0.18,
                        rect.width() * 0.14,
                        rect.width() * 0.16,
                    ];
                    let values = [
                        format!("{}  {name}", if is_dir { "📁" } else { "·" }),
                        file_type,
                        if is_dir {
                            String::new()
                        } else {
                            human_size(size)
                        },
                        modified,
                    ];
                    let mut x = rect.left() + 8.0;
                    for (value, width) in values.iter().zip(widths) {
                        ui.painter().text(
                            egui::pos2(x, rect.center().y),
                            egui::Align2::LEFT_CENTER,
                            value,
                            egui::FontId::proportional(13.0),
                            color,
                        );
                        x += width;
                    }
                } else {
                    ui.painter().text(
                        egui::pos2(rect.left() + 8.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        format!("{}  {name}", if is_dir { "📁" } else { "·" }),
                        egui::FontId::proportional(14.0),
                        color,
                    );
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
        ui.label(format!("{} selected", paths.len()));
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
    fn poll_export_jobs(&mut self, ctx: &egui::Context) {
        let snapshots = self.export_queue.snapshot();
        self.export_active = snapshots.iter().filter(|job| !job.finished).count();
        for job in snapshots.iter().filter(|job| job.finished) {
            if self.jobs_notified.insert(job.id) {
                self.status = format!(
                    "Export job #{} finished: {} done, {} failed, {} cancelled",
                    job.id, job.completed, job.failed, job.cancelled
                );
            }
        }
        if self.export_active > 0 {
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
                ui.weak(
                    "Metadata policy: no XMP packet, rating, label or flag is copied yet — the \
                     Phase 3 writers decide how that happens. EXIF orientation is applied to the \
                     exported pixels.",
                );
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
            match self.export_queue.submit(request) {
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
                let snapshots = self.export_queue.snapshot();
                if snapshots.is_empty() {
                    ui.weak("No export jobs yet. Right-click a selection and choose Export…");
                    return;
                }
                ui.horizontal(|ui| {
                    if ui
                        .button("Remove finished")
                        .on_hover_text("Hide finished jobs from this list")
                        .clicked()
                    {
                        self.export_queue.clear_finished();
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for job in snapshots.iter().rev() {
                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                ui.strong(format!(
                                    "Job #{} · {} · ≤{} px",
                                    job.id,
                                    job.format.label(),
                                    job.max_edge
                                ));
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if !job.finished && ui.button("Cancel job").clicked() {
                                            self.export_queue.cancel(job.id);
                                        }
                                    },
                                );
                            });
                            ui.weak(format!("→ {}", job.destination));
                            let done = job.completed + job.failed + job.cancelled;
                            ui.add(
                                egui::ProgressBar::new(done as f32 / job.total.max(1) as f32)
                                    .text(format!("{done}/{}", job.total)),
                            );
                            ui.weak(format!(
                                "{} done · {} failed · {} cancelled · {} queued · {} running",
                                job.completed, job.failed, job.cancelled, job.queued, job.running
                            ));
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
                                                ui.label(format!("▶ {name} — exporting…"));
                                            }
                                            bc_core::jobs::FileState::Done { output } => {
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
    ) -> egui::Response {
        let _ = idx;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());

        ui.vertical_centered(|ui| {
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2((cell_size - 12.0).max(1.0), (cell_size - 20.0).max(1.0)),
                egui::Sense::click(),
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
                ui.painter().rect_stroke(
                    rect.expand(3.0),
                    6.0,
                    egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(0, 160, 255)),
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
                ui.separator();
                if ui.button("Select All").clicked() {
                    self.select_all_visible();
                }
                if ui.button("Clear").clicked() {
                    self.browser.selection.clear();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    egui::ComboBox::from_id_source("sort_key")
                        .selected_text(match self.browser.sort_key {
                            SortKey::Name => "Name",
                            SortKey::Size => "Size",
                            SortKey::Modified => "Modified",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.browser.sort_key, SortKey::Name, "Name");
                            ui.selectable_value(&mut self.browser.sort_key, SortKey::Size, "Size");
                            ui.selectable_value(
                                &mut self.browser.sort_key,
                                SortKey::Modified,
                                "Modified",
                            );
                        });
                    let dir_label = match self.browser.sort_dir {
                        SortDir::Asc => "A-Z",
                        SortDir::Desc => "Z-A",
                    };
                    if ui.button(dir_label).clicked() {
                        self.browser.sort_dir = match self.browser.sort_dir {
                            SortDir::Asc => SortDir::Desc,
                            SortDir::Desc => SortDir::Asc,
                        };
                        self.browser
                            .set_sort(self.browser.sort_key, self.browser.sort_dir);
                    }
                    ui.label("Sort:");
                    ui.separator();
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
                let jobs_label = if self.export_active > 0 {
                    format!("Jobs ({})", self.export_active)
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

impl eframe::App for BridgeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_thumbnails(ctx);
        self.poll_export_jobs(ctx);
        self.poll_watcher();
        self.prune_preview_textures();

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
                    if self.browser.is_empty() {
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
        self.jobs_window(ctx);
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
}
