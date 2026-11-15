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

use bc_core::auto_process::{AutoAction, AutoProcessor, AutoRule};
use bc_core::browser::BrowserState;
use bc_core::export::{ExportFormat, ExportQueue, ExportRequest};
use bc_core::favorites::Favorites;
use bc_core::file_ops::{
    copy_path, delete_path, move_path, rename_path, transfer_into_folder, TransferMode,
};
use bc_core::fs::{SortDir, SortKey};
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
    bulk_rating_enabled: bool,
    bulk_label_enabled: bool,
    bulk_flag_enabled: bool,

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
    pub fn new(ctx: &egui::Context, root: Option<String>, cache_path: Option<PathBuf>) -> Self {
        let (job_tx, job_rx) = channel::<ThumbJob>();
        let (res_tx, res_rx) = channel::<ThumbResult>();
        spawn_thumb_worker(cache_path.clone(), job_rx, res_tx);

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
            bulk_rating_enabled: false,
            bulk_label_enabled: false,
            bulk_flag_enabled: false,
            export_queue: ExportQueue::new(bc_core::export::default_worker_count()),
            export_dialog: None,
            jobs_window: false,
            export_active: 0,
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

        // Completions come from the hierarchy this file already carries, so
        // typing a parent offers its children instead of inventing a global
        // keyword pool that nothing fills yet.
        let prefix = self.keyword_draft.trim().to_string();
        let suggestions: Vec<String> = if prefix.is_empty() {
            Vec::new()
        } else {
            bc_metadata::keywords::suggest(&self.tags_form.current().keywords, &prefix)
        };
        let mut chosen: Option<String> = None;
        if !suggestions.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new("Complete:").weak().small())
                        .selectable(false),
                );
                for suggestion in &suggestions {
                    if ui
                        .small_button(egui::RichText::new(suggestion.as_str()).small())
                        .clicked()
                    {
                        chosen = Some(suggestion.clone());
                    }
                }
            });
        }
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
                    "Metadata policy: no XMP packet, rating, label or flag is copied yet. \
                     The writers exist, so this is a wiring gap in the export path, not a \
                     missing capability. EXIF orientation is applied to the exported pixels.",
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

impl eframe::App for BridgeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_thumbnails(ctx);
        if let Some(processor) = &mut self.auto_processor {
            processor.poll(self.auto_started.elapsed());
            if processor.queued_count() > 0 || processor.watches_roots() {
                ctx.request_repaint_after(Duration::from_millis(250));
            }
        }
        self.poll_export_jobs(ctx);
        self.poll_watcher();
        self.prune_preview_textures();

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
    fn isolated_app(ctx: &egui::Context, root: &Path, isolated: &Path) -> BridgeApp {
        std::env::set_var("HOME", isolated);
        std::env::set_var("APPDATA", isolated);
        std::env::set_var("XDG_DATA_HOME", isolated.join("data"));
        BridgeApp::new(ctx, Some(root.display().to_string()), None)
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
        let mut app = isolated_app(&ctx, &root, &data);
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
        let mut app = isolated_app(&ctx, &root, &data);
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
        let mut app = isolated_app(&ctx, &root, &data);
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
        let mut app = isolated_app(&ctx, &root, &data);
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
        let mut app = isolated_app(&ctx, &root, &data);
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
        let mut app = isolated_app(&ctx, &root, &data);
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
    fn every_tag_control_in_the_panel_does_something_when_activated() {
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
        let mut app = isolated_app(&ctx, &root, &data);
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
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().join("library");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("photo.jpg");
        std::fs::write(&file, b"original image bytes").unwrap();
        let mtime = 1_700_000_000_000_000_000;

        let ctx = egui::Context::default();
        let mut app = isolated_app(&ctx, &root, &data);
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
