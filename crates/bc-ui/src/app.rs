//! Main application: content grid, folder navigation, selection, thumbnails,
//! live directory watching, and name filtering.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use bc_core::browser::BrowserState;
use bc_core::file_ops::{
    copy_path, delete_path, move_path, rename_path, transfer_into_folder, TransferMode,
};
use bc_core::fs::{SortDir, SortKey};
use bc_core::recents::Recents;
use bc_core::selection::Selection;
use bc_core::trash::TrashBin;
use bc_core::watcher::Watcher;
use bc_thumbnails::{CacheStore, Thumbnail};
use eframe::egui;

const CELL_SIZE: f32 = 132.0;
const CELL_HEIGHT: f32 = CELL_SIZE + 44.0;
const GRID_SPACING: f32 = 8.0;
const THUMB_MAX_EDGE: u32 = 256;
/// Coalesce watcher event bursts into a single refresh.
const WATCHER_DEBOUNCE: Duration = Duration::from_millis(300);
const LIST_ROW_HEIGHT: f32 = 28.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ViewMode {
    #[default]
    Thumbnails,
    List,
    Details,
}

struct ThumbJob {
    path: PathBuf,
    mtime_ns: i64,
    size: u64,
    max_edge: u32,
}

struct ThumbResult {
    path: PathBuf,
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
                    cache.as_ref(),
                    &job.path,
                    job.mtime_ns,
                    job.size,
                    job.max_edge,
                );
                if let Err(e) = &thumb {
                    tracing::debug!("thumb decode failed for {}: {e}", job.path.display());
                }
                if tx
                    .send(ThumbResult {
                        path: job.path,
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
    pending: HashSet<PathBuf>,
    /// Decoded thumbnails uploaded to the GPU, keyed by path.
    textures: HashMap<PathBuf, egui::TextureHandle>,

    /// Watcher for the open root; recreated on navigation.
    watcher: Option<Watcher>,
    /// When a watcher event was seen; refresh fires after the debounce elapses.
    refresh_due: Option<Instant>,
    /// Folder under the pointer last frame; OS file drops land here when set.
    hovered_folder: Option<PathBuf>,
    /// Set once a folder target has taken a drag payload this frame, so the
    /// panel-wide fallback zone does not handle the same drop twice.
    drop_taken: bool,
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
            watcher: None,
            refresh_due: None,
            hovered_folder: None,
            drop_taken: false,
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
                match Recents::open_default().and_then(|mut recent| recent.record_access(root)) {
                    Ok(()) => {}
                    Err(e) => {
                        self.status = format!("Opened {}; recents unavailable: {e}", root.display())
                    }
                }
                self.pending.clear();
                // Drop thumbnails unrelated to the new folder to bound memory use.
                self.textures.retain(|p, _| p.starts_with(root));
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
            self.pending.remove(&result.path);
            if let Some(thumb) = result.thumb {
                if !thumb.is_consistent() {
                    continue;
                }
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [thumb.width as usize, thumb.height as usize],
                    &thumb.pixels,
                );
                let handle = ctx.load_texture(
                    result.path.display().to_string(),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                self.textures.insert(result.path, handle);
            }
        }
    }

    fn request_thumbnail(&mut self, path: &Path, mtime: i64, size: u64) {
        if self.textures.contains_key(path) || self.pending.contains(path) {
            return;
        }
        if self
            .thumb_tx
            .send(ThumbJob {
                path: path.to_path_buf(),
                mtime_ns: mtime,
                size,
                max_edge: THUMB_MAX_EDGE,
            })
            .is_ok()
        {
            self.pending.insert(path.to_path_buf());
        }
    }

    fn filter_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Filter:");
            let before = self.browser.filter.clone();
            let resp = ui.add_sized(
                [220.0, 20.0],
                egui::TextEdit::singleline(&mut self.browser.filter).hint_text("name contains…"),
            );
            // Re-apply immediately on any edit (also handles clearing).
            if resp.changed() || self.browser.filter != before {
                let shown = self.browser.set_filter(self.browser.filter.clone());
                self.status = format!(
                    "Filter “{}”: {} of {} items",
                    self.browser.filter,
                    shown,
                    self.browser.len()
                );
            }
            if ui.button("✕").clicked() && !self.browser.filter.is_empty() {
                self.browser.set_filter("");
            }
        });
    }

    fn content_grid(&mut self, ui: &mut egui::Ui) {
        // Keep all layout dimensions positive: a collapsed/zero-width UI should
        // still show one column rather than divide by zero or skip the grid.
        let cell_width = if CELL_SIZE.is_finite() && CELL_SIZE > 0.0 {
            CELL_SIZE
        } else {
            132.0
        };
        let row_height = if CELL_HEIGHT.is_finite() && CELL_HEIGHT >= cell_width {
            CELL_HEIGHT
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
        let indices = self.browser.visible_indices();
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
                            let mods = ui.input(|i| i.modifiers);
                            if mods.shift {
                                self.browser.selection.select_range(idx);
                            } else if mods.ctrl || mods.command {
                                self.browser.selection.toggle(idx);
                            } else {
                                self.browser.selection.select_only(idx);
                            }
                        }
                        if response.double_clicked() && is_dir {
                            self.open_root(&path);
                        }
                        if !is_dir {
                            self.request_thumbnail(&path, mtime_ns(modified), size);
                        }
                        if is_dir && response.hovered() {
                            self.hovered_folder = Some(path.clone());
                        }
                        if response.drag_started() {
                            let paths = if self.browser.selection.contains(idx) {
                                self.selected_paths(&path)
                            } else {
                                self.browser.selection.select_only(idx);
                                vec![path.clone()]
                            };
                            response.dnd_set_drag_payload(paths);
                        }
                        if is_dir {
                            if response.dnd_hover_payload::<Vec<PathBuf>>().is_some() {
                                ui.painter().rect_stroke(
                                    response.rect.expand(2.0),
                                    6.0,
                                    egui::Stroke::new(
                                        2.0_f32,
                                        egui::Color32::from_rgb(0, 160, 255),
                                    ),
                                );
                            }
                            if let Some(payload) = response.dnd_release_payload::<Vec<PathBuf>>() {
                                self.drop_taken = true;
                                let mode = drop_mode(ui.input(|i| i.modifiers.alt));
                                self.status = apply_transfer(&payload, &path, mode);
                                self.refresh_after_drop();
                            }
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
        let visible = self.browser.visible_indices();
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
            let (up, down, shift) = ui.input(|input| {
                (
                    input.key_pressed(egui::Key::ArrowUp),
                    input.key_pressed(egui::Key::ArrowDown),
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
                    egui::Sense::click_and_drag(),
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
                        format!("{}  {name}", if is_dir { "▸" } else { "·" }),
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
                        format!("{}  {name}", if is_dir { "▸" } else { "·" }),
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
                if response.double_clicked() && is_dir {
                    let path = self.browser.entries[idx].path.clone();
                    self.open_root(&path);
                }
                if is_dir && response.hovered() {
                    self.hovered_folder = Some(context_path.clone());
                }
                if response.drag_started() {
                    // Dragging an item inside the selection drags the whole
                    // selection; dragging an unselected item selects and drags
                    // just that item.
                    let paths = if self.browser.selection.contains(idx) {
                        self.selected_paths(&context_path)
                    } else {
                        self.browser.selection.select_only(idx);
                        vec![context_path.clone()]
                    };
                    response.dnd_set_drag_payload(paths);
                }
                if is_dir {
                    if response.dnd_hover_payload::<Vec<PathBuf>>().is_some() {
                        ui.painter().rect_stroke(
                            rect,
                            2.0,
                            egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(0, 160, 255)),
                        );
                    }
                    if let Some(payload) = response.dnd_release_payload::<Vec<PathBuf>>() {
                        self.drop_taken = true;
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

    fn recents_menu(&mut self, ui: &mut egui::Ui) {
        match Recents::open_default() {
            Ok(recents) if !recents.paths().is_empty() => {
                for path in recents.paths().to_vec() {
                    if ui.button(path.display().to_string()).clicked() {
                        self.open_root(&path);
                        ui.close_menu();
                    }
                }
            }
            Ok(_) => {
                ui.weak("No recent locations.");
            }
            Err(e) => {
                ui.label(format!("Recents unavailable: {e}"));
            }
        }
    }

    fn trash_menu(&mut self, ui: &mut egui::Ui) {
        match TrashBin::open_default() {
            Ok(mut bin) => {
                let entries = bin.list();
                if entries.is_empty() {
                    ui.weak("Trash is empty.");
                } else {
                    for entry in entries {
                        ui.horizontal(|ui| {
                            ui.label(entry.original_path.display().to_string());
                            if ui.button("Restore").clicked() {
                                match bin.restore(&entry.id) {
                                    Ok(path) => {
                                        self.status = format!("Restored {}", path.display())
                                    }
                                    Err(e) => self.status = format!("Could not restore item: {e}"),
                                }
                                ui.close_menu();
                            }
                        });
                    }
                }
            }
            Err(e) => {
                ui.label(format!("Trash unavailable: {e}"));
            }
        }
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
                egui::vec2((cell_size - 12.0).max(1.0), (cell_size - 12.0).max(1.0)),
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
            } else if let Some(tex) = self.textures.get(path) {
                let tex_size = tex.size_vec2();
                let scale = ((rect.size().x / tex_size.x).min(rect.size().y / tex_size.y)).min(1.0);
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
                    "⋯",
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
}

/// Transfer mode for a drop: hold Alt to copy, otherwise move.
fn drop_mode(alt_down: bool) -> TransferMode {
    if alt_down {
        TransferMode::Copy
    } else {
        TransferMode::Move
    }
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
        self.poll_watcher();

        // Files dropped in from the OS (Finder/Explorer) are copied into the
        // hovered folder, or into the open folder when nothing is hovered.
        // This uses last frame's hover, which is where the pointer sits while
        // the drop event arrives; egui reports native drops without a widget.
        let os_drops: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .collect()
        });
        if !os_drops.is_empty() {
            let target = self
                .hovered_folder
                .clone()
                .or_else(|| self.browser.root.clone());
            match target {
                Some(folder) => {
                    self.status = apply_transfer(&os_drops, &folder, TransferMode::Copy);
                    self.refresh_after_drop();
                }
                None => self.status = "Open a folder before dropping files.".into(),
            }
        }
        self.hovered_folder = None;
        self.drop_taken = false;

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Folder:");
                let resp = ui.add_sized(
                    [ui.available_width() - 260.0, 20.0],
                    egui::TextEdit::singleline(&mut self.path_input),
                );
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    let path = self.path_input.clone();
                    self.open_root(Path::new(&path));
                }
                if ui.button("Open").clicked() {
                    let path = self.path_input.clone();
                    self.open_root(Path::new(&path));
                }
                if ui.button("⬆ Up").clicked() {
                    if let Some(parent) = self
                        .browser
                        .root
                        .as_ref()
                        .and_then(|r| r.parent())
                        .map(|p| p.to_path_buf())
                    {
                        self.open_root(&parent);
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("Sort:");
                let key_names = [
                    (SortKey::Name, "Name"),
                    (SortKey::Size, "Size"),
                    (SortKey::Modified, "Modified"),
                ];
                egui::ComboBox::from_id_source("sort_key")
                    .selected_text(
                        key_names
                            .iter()
                            .find(|(k, _)| *k == self.browser.sort_key)
                            .map_or("?", |(_, n)| n),
                    )
                    .show_ui(ui, |ui| {
                        for (k, n) in key_names {
                            ui.selectable_value(&mut self.browser.sort_key, k, n);
                        }
                    });
                let dir_label = match self.browser.sort_dir {
                    SortDir::Asc => "▲ Asc",
                    SortDir::Desc => "▼ Desc",
                };
                if ui.button(dir_label).clicked() {
                    self.browser.sort_dir = match self.browser.sort_dir {
                        SortDir::Asc => SortDir::Desc,
                        SortDir::Desc => SortDir::Asc,
                    };
                    self.browser
                        .set_sort(self.browser.sort_key, self.browser.sort_dir);
                }
                ui.separator();
                self.filter_bar(ui);
            });
            ui.horizontal(|ui| {
                ui.menu_button("Recents", |ui| self.recents_menu(ui));
                ui.menu_button("Trash", |ui| self.trash_menu(ui));
            });
            ui.horizontal(|ui| {
                ui.label("View:");
                ui.selectable_value(&mut self.view_mode, ViewMode::Thumbnails, "Thumbnails");
                ui.selectable_value(&mut self.view_mode, ViewMode::List, "List");
                ui.selectable_value(&mut self.view_mode, ViewMode::Details, "Details");
            });
            ui.horizontal(|ui| {
                if ui.button("Select All").clicked() {
                    // Select only currently visible entries so Select All
                    // respects the active filter.
                    let visible = self.browser.visible_indices();
                    self.browser.selection.clear();
                    for idx in visible {
                        self.browser.selection.toggle(idx);
                    }
                }
                if ui.button("Clear").clicked() {
                    self.browser.selection.clear();
                }
                ui.separator();
                ui.label(format!("{} selected", self.browser.selection.len()));
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.browser.root.is_none() {
                ui.centered_and_justified(|ui| {
                    ui.weak("Type a folder path above and press Open.");
                });
            } else if self.browser.visible_len() == 0 {
                ui.centered_and_justified(|ui| {
                    if self.browser.is_empty() {
                        ui.weak("This folder is empty.");
                    } else {
                        ui.weak(format!("No items match filter “{}”.", self.browser.filter));
                    }
                });
            } else {
                match self.view_mode {
                    ViewMode::Thumbnails => self.content_grid(ui),
                    ViewMode::List => self.content_list(ui, ctx, false),
                    ViewMode::Details => self.content_list(ui, ctx, true),
                }
            }
            // Fallback drop zone: a drag released anywhere in the content
            // panel (not on a folder) transfers into the currently open
            // folder. Hover sense only, so it never steals clicks from the
            // cells painted above it.
            let background = ui.interact(
                ui.min_rect(),
                ui.id().with("drop-background"),
                egui::Sense::hover(),
            );
            if !self.drop_taken {
                if let Some(payload) = background.dnd_release_payload::<Vec<PathBuf>>() {
                    if let Some(root) = self.browser.root.clone() {
                        let mode = drop_mode(ui.input(|i| i.modifiers.alt));
                        self.status = apply_transfer(&payload, &root, mode);
                        self.refresh_after_drop();
                    }
                }
            }
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(&self.status);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak(format!(
                        "{} of {} items",
                        self.browser.visible_len(),
                        self.browser.len()
                    ));
                    if let Some(root) = &self.browser.root {
                        ui.weak(root.display().to_string());
                    }
                });
            });
        });
        self.confirm_delete_window(ctx);
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

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"payload").unwrap();
        path
    }

    #[test]
    fn drop_mode_maps_alt_to_copy() {
        assert_eq!(drop_mode(false), TransferMode::Move);
        assert_eq!(drop_mode(true), TransferMode::Copy);
    }

    #[test]
    fn move_drop_relocates_and_copy_drop_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let source = touch(dir.path(), "a.txt");
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();

        let status = apply_transfer(std::slice::from_ref(&source), &target, TransferMode::Move);
        assert!(status.starts_with("Moved 1 item(s)"), "{status}");
        assert!(!source.exists());
        assert!(target.join("a.txt").exists());

        let status = apply_transfer(&[target.join("a.txt")], dir.path(), TransferMode::Copy);
        assert!(status.starts_with("Copied 1 item(s)"), "{status}");
        assert!(dir.path().join("a.txt").exists());
        assert!(target.join("a.txt").exists());
    }

    #[test]
    fn move_drop_into_same_folder_skips() {
        let dir = tempfile::tempdir().unwrap();
        let source = touch(dir.path(), "a.txt");

        let status = apply_transfer(
            std::slice::from_ref(&source),
            dir.path(),
            TransferMode::Move,
        );
        assert!(status.contains("skipped 1"), "{status}");
        assert!(source.exists());
    }

    #[test]
    fn copy_collision_is_reported_and_leaves_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = touch(dir.path(), "a.txt");
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        touch(&target, "a.txt");

        let status = apply_transfer(std::slice::from_ref(&source), &target, TransferMode::Copy);
        assert!(status.contains("1 failed"), "{status}");
        assert!(source.exists());
    }

    #[test]
    fn folder_drop_into_itself_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();

        let status = apply_transfer(std::slice::from_ref(&sub), &sub, TransferMode::Move);
        assert!(status.starts_with("Drop rejected"), "{status}");
        assert!(sub.exists());
    }
}
