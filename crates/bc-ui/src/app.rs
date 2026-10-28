//! Main application: content grid, folder navigation, selection, thumbnails,
//! live directory watching, and name filtering.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use bc_core::browser::BrowserState;
use bc_core::fs::{SortDir, SortKey};
use bc_core::selection::Selection;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortMode {
    NameAsc,
    NameDesc,
    DateNewest,
    DateOldest,
    SizeLargest,
    SizeSmallest,
}

impl SortMode {
    fn label(self) -> &'static str {
        match self {
            Self::NameAsc => "Name ↑",
            Self::NameDesc => "Name ↓",
            Self::DateNewest => "Date ↓",
            Self::DateOldest => "Date ↑",
            Self::SizeLargest => "Size ↓",
            Self::SizeSmallest => "Size ↑",
        }
    }

    fn key_and_dir(self) -> (SortKey, SortDir) {
        match self {
            Self::NameAsc => (SortKey::Name, SortDir::Asc),
            Self::NameDesc => (SortKey::Name, SortDir::Desc),
            Self::DateNewest => (SortKey::Modified, SortDir::Desc),
            Self::DateOldest => (SortKey::Modified, SortDir::Asc),
            Self::SizeLargest => (SortKey::Size, SortDir::Desc),
            Self::SizeSmallest => (SortKey::Size, SortDir::Asc),
        }
    }
}

struct WatcherEvent {
    path: PathBuf,
    is_dir: bool,
    kind: WatcherEventKind,
}

#[derive(Debug, Clone, Copy)]
enum WatcherEventKind {
    Create,
    Remove,
    Rename,
    Modify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThumbnailFilter {
    All,
    Images,
    Videos,
    Audio,
    Documents,
}

impl ThumbnailFilter {
    fn label(self) -> &'static str {
        match self {
            Self::All => "All files",
            Self::Images => "Images",
            Self::Videos => "Videos",
            Self::Audio => "Audio",
            Self::Documents => "Documents",
        }
    }

    fn matches(self, entry: &bc_core::browser::Entry) -> bool {
        if entry.is_dir {
            return true;
        }
        let ext = entry
            .path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        match self {
            Self::All => true,
            Self::Images => matches!(
                ext.as_str(),
                "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "tiff" | "tif" | "ico"
            ),
            Self::Videos => matches!(
                ext.as_str(),
                "mp4" | "mkv" | "mov" | "avi" | "webm" | "wmv" | "m4v" | "flv"
            ),
            Self::Audio => matches!(
                ext.as_str(),
                "mp3" | "wav" | "flac" | "ogg" | "aac" | "m4a" | "wma"
            ),
            Self::Documents => matches!(
                ext.as_str(),
                "pdf" | "doc" | "docx" | "txt" | "rtf" | "odt" | "xls" | "xlsx" | "csv"
            ),
        }
    }
}

pub struct App {
    browser: BrowserState,
    selection: Selection,
    thumbnails: HashMap<PathBuf, Thumbnail>,
    sort: SortMode,
    view_mode: ViewMode,
    thumbnail_filter: ThumbnailFilter,
    search_query: String,
    path_input: String,
    error_message: Option<String>,
    thumbnail_cache: CacheStore,
    watcher_rx: Option<Receiver<WatcherEvent>>,
    watcher_tx: Option<Sender<WatcherEvent>>,
    _watcher: Option<Watcher>,
    last_watch_event: Instant,
    refresh_pending: bool,
    thumbnail_rx: Option<Receiver<(PathBuf, Thumbnail)>>,
    thumbnail_tx: Option<Sender<(PathBuf, Thumbnail)>>,
    /// Paths currently being generated in the background.
    thumbnail_in_flight: std::collections::HashSet<PathBuf>,
    /// Paths deferred while another thumbnail is being generated.
    thumbnail_queue: std::collections::VecDeque<PathBuf>,
    /// Limit concurrent ffmpeg workers to avoid CPU spikes.
    max_concurrent_thumbnails: usize,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let (tx, rx) = channel();
        let mut app = Self {
            browser: BrowserState::new(),
            selection: Selection::new(),
            thumbnails: HashMap::new(),
            sort: SortMode::NameAsc,
            view_mode: ViewMode::Thumbnails,
            thumbnail_filter: ThumbnailFilter::All,
            search_query: String::new(),
            path_input: String::new(),
            error_message: None,
            thumbnail_cache: CacheStore::new(),
            watcher_rx: None,
            watcher_tx: None,
            _watcher: None,
            last_watch_event: Instant::now(),
            refresh_pending: false,
            thumbnail_rx: None,
            thumbnail_tx: None,
            thumbnail_in_flight: std::collections::HashSet::new(),
            thumbnail_queue: std::collections::VecDeque::new(),
            max_concurrent_thumbnails: 4,
        };
        app.open_root(Path::new("."));
        app
    }

    fn open_root(&mut self, path: &Path) {
        self.stop_watcher();
        self.thumbnail_in_flight.clear();
        self.thumbnail_queue.clear();
        match self.browser.open_root(path) {
            Ok(()) => {
                self.path_input = self.browser.root().display().to_string();
                self.error_message = None;
                self.thumbnails.clear();
                self.selection.clear();
                self.start_watcher();
            }
            Err(e) => {
                self.error_message = Some(e.to_string());
            }
        }
    }

    fn start_watcher(&mut self) {
        let (tx, rx) = channel();
        let tx2 = tx.clone();
        let root = self.browser.root().to_path_buf();
        let watcher = match Watcher::new(&root, move |event| {
            let (path, is_dir, kind) = match event {
                notify::Event { kind: notify::EventKind::Create(_), paths, .. } => {
                    let p = paths.into_iter().next()?;
                    let is_dir = p.is_dir();
                    Some((p, is_dir, WatcherEventKind::Create))
                }
                notify::Event { kind: notify::EventKind::Remove(_), paths, .. } => {
                    let p = paths.into_iter().next()?;
                    Some((p, false, WatcherEventKind::Remove))
                }
                notify::Event { kind: notify::EventKind::Modify(notify::event::ModifyKind::Name(_)), paths, .. } => {
                    let p = paths.into_iter().next()?;
                    Some((p, false, WatcherEventKind::Rename))
                }
                notify::Event { kind: notify::EventKind::Modify(_), paths, .. } => {
                    let p = paths.into_iter().next()?;
                    let is_dir = p.is_dir();
                    Some((p, is_dir, WatcherEventKind::Modify))
                }
                _ => None,
            }?;
            let _ = tx2.send(WatcherEvent { path, is_dir, kind });
            Some(())
        }) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("Failed to watch directory: {}", e);
                return;
            }
        };
        self.watcher_rx = Some(rx);
        self.watcher_tx = Some(tx);
        self._watcher = Some(watcher);
    }

    fn stop_watcher(&mut self) {
        self.watcher_rx = None;
        self.watcher_tx = None;
        self._watcher = None;
    }

    fn process_watcher_events(&mut self) {
        let events: Vec<WatcherEvent> = self
            .watcher_rx
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default();
        if events.is_empty() {
            return;
        }
        self.last_watch_event = Instant::now();
        self.refresh_pending = true;
        let root = self.browser.root().to_path_buf();
        for event in events {
            if !event.path.starts_with(&root) {
                continue;
            }
            match event.kind {
                WatcherEventKind::Create => {
                    if let Ok(entry) = bc_core::browser::Entry::from_path(event.path, event.is_dir) {
                        self.browser.add_entry(entry);
                    }
                }
                WatcherEventKind::Remove => {
                    self.browser.remove_entry(&event.path);
                    self.selection.remove(&event.path);
                    self.thumbnails.remove(&event.path);
                }
                WatcherEventKind::Rename => {
                    self.browser.remove_entry(&event.path);
                    self.thumbnails.remove(&event.path);
                }
                WatcherEventKind::Modify => {
                    self.thumbnails.remove(&event.path);
                    self.thumbnail_in_flight.remove(&event.path);
                    if let Ok(entry) = bc_core::browser::Entry::from_path(event.path.clone(), event.is_dir) {
                        self.browser.update_entry(entry);
                    }
                }
            }
        }
    }

    fn process_pending_refresh(&mut self) {
        if !self.refresh_pending || self.last_watch_event.elapsed() < WATCHER_DEBOUNCE {
            return;
        }
        self.refresh_pending = false;
        if let Err(e) = self.browser.refresh() {
            self.error_message = Some(e.to_string());
            return;
        }
        self.selection
            .retain_paths(self.browser.entries().iter().map(|e| &e.path));
    }

    fn process_thumbnail_results(&mut self) {
        let results: Vec<(PathBuf, Thumbnail)> = self
            .thumbnail_rx
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default();
        for (path, thumbnail) in results {
            self.thumbnail_in_flight.remove(&path);
            if self.browser.entries().iter().any(|e| e.path == path) {
                self.thumbnails.insert(path.clone(), thumbnail);
            }
        }
        self.process_thumbnail_queue();
    }

    fn process_thumbnail_queue(&mut self) {
        while self.thumbnail_in_flight.len() < self.max_concurrent_thumbnails {
            let Some(path) = self.thumbnail_queue.pop_front() else {
                break;
            };
            if self.thumbnail_in_flight.contains(&path) {
                continue;
            }
            self.thumbnail_in_flight.insert(path.clone());
            self.spawn_thumbnail_worker(path);
        }
    }

    fn request_thumbnail(&mut self, path: &Path) {
        if self.thumbnails.contains_key(path) || self.thumbnail_in_flight.contains(path) {
            return;
        }
        if let Some(cached) = self.thumbnail_cache.load(path) {
            self.thumbnails.insert(path.to_path_buf(), cached);
            return;
        }
        if !self.thumbnail_queue.iter().any(|p| p == path) {
            self.thumbnail_queue.push_back(path.to_path_buf());
        }
        self.process_thumbnail_queue();
    }

    fn spawn_thumbnail_worker(&self, path: PathBuf) {
        let tx = self.thumbnail_tx.as_ref().unwrap().clone();
        let cache = self.thumbnail_cache.clone();
        std::thread::spawn(move || {
            let thumbnail = bc_thumbnails::generate_thumbnail(&path);
            if let Some(ref t) = thumbnail {
                cache.save(&path, t);
            }
            if let Some(t) = thumbnail {
                let _ = tx.send((path, t));
            } else {
                tracing::debug!("No thumbnail available for {}", path.display());
                let _ = tx.send((path, Thumbnail::empty()));
            }
        });
    }

    fn filtered_entries(&self) -> Vec<&bc_core::browser::Entry> {
        self.browser
            .entries()
            .iter()
            .filter(|e| self.thumbnail_filter.matches(e))
            .filter(|e| {
                if self.search_query.is_empty() {
                    return true;
                }
                e.name
                    .to_lowercase()
                    .contains(&self.search_query.to_lowercase())
            })
            .collect()
    }

    fn sorted_entries(&self) -> Vec<&bc_core::browser::Entry> {
        let mut entries = self.filtered_entries();
        let (key, dir) = self.sort.key_and_dir();
        entries.sort_by(|a, b| {
            if a.is_dir != b.is_dir {
                return b.is_dir.cmp(&a.is_dir);
            }
            let cmp = match key {
                SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortKey::Modified => a.modified.cmp(&b.modified),
                SortKey::Size => a.size.cmp(&b.size),
                SortKey::Type => a.path.extension().cmp(&b.path.extension()),
            };
            match dir {
                SortDir::Asc => cmp,
                SortDir::Desc => cmp.reverse(),
            }
        });
        entries
    }

    fn handle_file_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.selection.clear();
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            if let Some(path) = self.selection.selected().iter().next().cloned() {
                if path.is_dir() {
                    self.open_root(&path);
                }
            }
        }
        if ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::A)) {
            let paths: Vec<PathBuf> = self.browser.entries().iter().map(|e| e.path.clone()).collect();
            self.selection.select_all(paths);
        }
    }

    fn show_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("← Back").clicked() {
                if let Some(parent) = self.browser.root().parent().map(Path::to_path_buf) {
                    self.open_root(&parent);
                }
            }
            ui.separator();
            ui.label("Path:");
            let response = ui.add_sized(
                [ui.available_width() - 160.0, 24.0],
                egui::TextEdit::singleline(&mut self.path_input),
            );
            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let path = self.path_input.clone();
                self.open_root(Path::new(&path));
            }
            if ui.button("Go").clicked() {
                let path = self.path_input.clone();
                self.open_root(Path::new(&path));
            }
            if ui.button("⬆ Up").clicked() {
                if let Some(parent) = self.browser.root().parent().map(Path::to_path_buf) {
                    self.open_root(&parent);
                }
            }
            ui.separator();
            egui::ComboBox::from_id_source("sort")
                .selected_text(self.sort.label())
                .show_ui(ui, |ui| {
                    for mode in [
                        SortMode::NameAsc,
                        SortMode::NameDesc,
                        SortMode::DateNewest,
                        SortMode::DateOldest,
                        SortMode::SizeLargest,
                        SortMode::SizeSmallest,
                    ] {
                        ui.selectable_value(&mut self.sort, mode, mode.label());
                    }
                });
            ui.separator();
            egui::ComboBox::from_id_source("filter")
                .selected_text(self.thumbnail_filter.label())
                .show_ui(ui, |ui| {
                    for filter in [
                        ThumbnailFilter::All,
                        ThumbnailFilter::Images,
                        ThumbnailFilter::Videos,
                        ThumbnailFilter::Audio,
                        ThumbnailFilter::Documents,
                    ] {
                        ui.selectable_value(&mut self.thumbnail_filter, filter, filter.label());
                    }
                });
            ui.separator();
            ui.selectable_value(&mut self.view_mode, ViewMode::Thumbnails, "Grid");
            ui.selectable_value(&mut self.view_mode, ViewMode::List, "List");
            ui.selectable_value(&mut self.view_mode, ViewMode::Details, "Details");
        });
        ui.horizontal(|ui| {
            ui.label("Search:");
            ui.add_sized(
                [ui.available_width(), 22.0],
                egui::TextEdit::singleline(&mut self.search_query)
                    .hint_text("Filter files by name..."),
            );
            ui.label(format!("{} items", self.filtered_entries().len()));
        });
    }

    fn show_grid(&mut self, ui: &mut egui::Ui) {
        let entries: Vec<(PathBuf, String, bool, u64)> = self
            .sorted_entries()
            .iter()
            .map(|e| (e.path.clone(), e.name.clone(), e.is_dir, e.size))
            .collect();
        if entries.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label("This folder is empty");
            });
            return;
        }
        let available_width = ui.available_width();
        let columns = ((available_width + GRID_SPACING) / (CELL_SIZE + GRID_SPACING))
            .floor()
            .max(1.0) as usize;
        let cell_size = (available_width - (columns - 1) as f32 * GRID_SPACING) / columns as f32;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for row in entries.chunks(columns) {
                ui.horizontal(|ui| {
                    for (path, name, is_dir, size) in row {
                        self.show_grid_cell(ui, path, name, *is_dir, *size, cell_size);
                        ui.add_space(GRID_SPACING);
                    }
                });
            }
        });
    }

    fn show_grid_cell(
        &mut self,
        ui: &mut egui::Ui,
        path: &Path,
        name: &str,
        is_dir: bool,
        size: u64,
        cell_size: f32,
    ) {
        let selected = self.selection.is_selected(path);
        let response = ui.allocate_response(
            egui::vec2(cell_size, CELL_HEIGHT),
            egui::Sense::click(),
        );
        if response.clicked() {
            if ui.input(|i| i.modifiers.ctrl) {
                self.selection.toggle(path.to_path_buf());
            } else {
                self.selection.clear();
                self.selection.select(path.to_path_buf());
            }
            if is_dir && response.double_clicked() {
                self.open_root(path);
            }
        }
        let rect = response.rect;
        let thumbnail_rect = egui::Rect::from_min_size(
            rect.min + egui::vec2(0.0, 0.0),
            egui::vec2(cell_size, CELL_SIZE),
        );
        if let Some(thumbnail) = self.thumbnails.get(path) {
            if let Some(texture) = thumbnail.texture(ui.ctx()) {
                let image_rect = fit_aspect(texture.size_vec2(), thumbnail_rect);
                ui.painter().image(
                    texture.id(),
                    image_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
        } else if is_dir {
            let icon = egui::RichText::new("📁").size(48.0);
            ui.painter().text(
                thumbnail_rect.center(),
                egui::Align2::CENTER_CENTER,
                icon.text(),
                egui::FontId::proportional(48.0),
                egui::Color32::from_rgb(210, 180, 90),
            );
        } else {
            self.request_thumbnail(path);
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
    }

    fn show_list(&mut self, ui: &mut egui::Ui) {
        let entries: Vec<(PathBuf, String, bool, u64, Option<std::time::SystemTime>)> = self
            .sorted_entries()
            .iter()
            .map(|e| (e.path.clone(), e.name.clone(), e.is_dir, e.size, e.modified))
            .collect();
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (path, name, is_dir, size, modified) in entries {
                let selected = self.selection.is_selected(&path);
                let row = ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), LIST_ROW_HEIGHT),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        let icon = if is_dir { "📁" } else { "📄" };
                        ui.label(icon);
                        ui.label(&name);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if !is_dir {
                                ui.label(human_size(size));
                            }
                            if let Some(time) = modified {
                                ui.label(format_modified(Some(time)));
                            }
                        });
                    },
                );
                if row.response.clicked() {
                    if ui.input(|i| i.modifiers.ctrl) {
                        self.selection.toggle(path.clone());
                    } else {
                        self.selection.clear();
                        self.selection.select(path.clone());
                    }
                }
                if row.response.double_clicked() && is_dir {
                    self.open_root(&path);
                }
                if selected {
                    ui.painter().rect_filled(
                        row.response.rect,
                        0.0,
                        egui::Color32::from_rgba_unmultiplied(40, 80, 140, 80),
                    );
                }
            }
        });
    }

    fn show_details(&mut self, ui: &mut egui::Ui) {
        let entries: Vec<(PathBuf, String, bool, u64, Option<std::time::SystemTime>)> = self
            .sorted_entries()
            .iter()
            .map(|e| (e.path.clone(), e.name.clone(), e.is_dir, e.size, e.modified))
            .collect();
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong("Name");
                ui.add_space(300.0);
                ui.strong("Size");
                ui.add_space(80.0);
                ui.strong("Modified");
            });
            ui.separator();
            for (path, name, is_dir, size, modified) in entries {
                let response = ui.horizontal(|ui| {
                    ui.label(if is_dir { "📁" } else { "📄" });
                    ui.label(&name);
                    ui.add_space(180.0);
                    ui.label(if is_dir { "—".to_string() } else { human_size(size) });
                    ui.add_space(80.0);
                    ui.label(format_modified(modified));
                });
                if response.response.clicked() {
                    if ui.input(|i| i.modifiers.ctrl) {
                        self.selection.toggle(path.clone());
                    } else {
                        self.selection.clear();
                        self.selection.select(path.clone());
                    }
                }
                if response.response.double_clicked() && is_dir {
                    self.open_root(&path);
                }
            }
        });
    }

    fn show_error(&mut self, ui: &mut egui::Ui) {
        if let Some(message) = &self.error_message {
            ui.colored_label(egui::Color32::LIGHT_RED, message);
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.thumbnail_rx.is_none() {
            let (tx, rx) = channel();
            self.thumbnail_tx = Some(tx);
            self.thumbnail_rx = Some(rx);
        }
        self.process_watcher_events();
        self.process_pending_refresh();
        self.process_thumbnail_results();
        self.handle_file_shortcuts(ctx);
        ctx.request_repaint_after(Duration::from_millis(100));

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            self.show_toolbar(ui);
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            self.show_error(ui);
            match self.view_mode {
                ViewMode::Thumbnails => self.show_grid(ui),
                ViewMode::List => self.show_list(ui),
                ViewMode::Details => self.show_details(ui),
            }
        });
    }
}

fn fit_aspect(source: egui::Vec2, target: egui::Rect) -> egui::Rect {
    if source.x <= 0.0 || source.y <= 0.0 {
        return target;
    }
    let scale = (target.width() / source.x).min(target.height() / source.y);
    let size = source * scale;
    egui::Rect::from_center_size(target.center(), size)
}

fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    if bytes == 0 {
        return "0 B".into();
    }
    let i = ((bytes as f64).log(1024.0).floor() as usize).min(UNITS.len() - 1);
    let value = bytes as f64 / 1024_f64.powi(i as i32);
    format!("{value:.1} {}", UNITS[i])
}

fn format_modified(time: Option<std::time::SystemTime>) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let Some(time) = time else {
        return "—".into();
    };
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = duration.as_secs() as i64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_formats_correctly() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512.0 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1_048_576), "1.0 MB");
        assert_eq!(human_size(1_073_741_824), "1.0 GB");
    }

    #[test]
    fn civil_from_days_epoch() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn civil_from_days_known_date() {
        // 2024-01-01 is 19723 days after the Unix epoch.
        assert_eq!(civil_from_days(19723), (2024, 1, 1));
    }

    #[test]
    fn format_modified_is_displayed_as_utc() {
        use std::time::{Duration, UNIX_EPOCH};
        assert_eq!(format_modified(Some(UNIX_EPOCH)), "1970-01-01 00:00");
        assert_eq!(
            format_modified(Some(UNIX_EPOCH + Duration::from_secs(1_704_067_200))),
            "2024-01-01 00:00"
        );
        assert_eq!(format_modified(None), "—");
    }
}
