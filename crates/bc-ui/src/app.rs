//! Main application: content grid, folder navigation, selection, thumbnails,
//! live directory watching, and name filtering.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use bc_core::browser::BrowserState;
use bc_core::fs::{SortDir, SortKey};
use bc_core::watcher::Watcher;
use bc_thumbnails::{CacheStore, Thumbnail};
use eframe::egui;

const CELL_SIZE: f32 = 132.0;
const CELL_HEIGHT: f32 = CELL_SIZE + 44.0;
const GRID_SPACING: f32 = 8.0;
const THUMB_MAX_EDGE: u32 = 256;
/// Coalesce watcher event bursts into a single refresh.
const WATCHER_DEBOUNCE: Duration = Duration::from_millis(300);

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
            thumb_tx: job_tx,
            thumb_rx: res_rx,
            pending: HashSet::new(),
            textures: HashMap::new(),
            watcher: None,
            refresh_due: None,
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
                        self.status =
                            format!("Refreshed ({} items)", self.browser.len());
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
                egui::TextEdit::singleline(&mut self.browser.filter)
                    .hint_text("name contains…"),
            );
            // Re-apply immediately on any edit (also handles clearing).
            if resp.changed() || self.browser.filter != before {
                let shown = self.browser.set_filter(self.browser.filter.clone());
                self.status = format!("Filter “{}”: {} of {} items",
                    self.browser.filter, shown, self.browser.len());
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
        let cols = (((available_width + spacing) / (cell_width + spacing)).floor() as usize)
            .max(1);
        let indices = self.browser.visible_indices();
        let row_step = row_height + spacing;
        let row_count = (indices.len() + cols - 1) / cols;

        egui::ScrollArea::vertical().show_viewport(ui, |ui, viewport| {
            // ScrollArea reports the viewport in content coordinates. Use its
            // vertical bounds to lay out only rows that can be seen; spacers
            // retain the full scroll range without creating off-screen cells.
            let first_row = ((viewport.min.y.max(0.0) / row_step).floor() as usize)
                .min(row_count);
            let end_row = ((viewport.max.y.max(0.0) / row_step).ceil() as usize)
                .min(row_count);
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
                                    self.cell(cell_ui, idx, &path, is_dir, size, selected, cell_width)
                                },
                            )
                            .inner;

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
                    }
                });
                // Include row spacing in the virtual extent so the next row's
                // origin matches the row-index calculation above.
                ui.add_space(spacing);
            }

            ui.add_space((row_count - end_row) as f32 * row_step);
        });
    }

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
            } else if let Some(tex) = self.textures.get(path) {
                let tex_size = tex.size_vec2();
                let scale = ((rect.size().x / tex_size.x)
                    .min(rect.size().y / tex_size.y))
                .min(1.0);
                let draw_rect = egui::Rect::from_center_size(
                    rect.center(),
                    tex_size * scale,
                );
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
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(0, 160, 255)),
                );
            }

            ui.set_min_width((cell_size - 16.0).max(1.0));
            ui.add_sized(
                [(cell_size - 16.0).max(1.0), 16.0],
                egui::Label::new(egui::RichText::new(&name).small()).truncate(),
            );
            if !is_dir {
                ui.label(egui::RichText::new(human_size(size)).weak().small());
            }
            response
        })
        .inner
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

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Folder:");
                let resp = ui.add_sized(
                    [ui.available_width() - 260.0, 20.0],
                    egui::TextEdit::singleline(&mut self.path_input),
                );
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.open_root(Path::new(&self.path_input));
                }
                if ui.button("Open").clicked() {
                    self.open_root(Path::new(&self.path_input));
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
                    self.browser.set_sort(self.browser.sort_key, self.browser.sort_dir);
                }
                ui.separator();
                self.filter_bar(ui);
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
                        ui.weak(format!(
                            "No items match filter “{}”.",
                            self.browser.filter
                        ));
                    }
                });
            } else {
                self.content_grid(ui);
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
    }
}
