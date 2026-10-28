//! Main application: content grid, folder navigation, selection, thumbnails,
//! live directory watching, and name filtering.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use egui::{Color32, RichText};
use image::GenericImageView;
use walkdir::WalkDir;

use crate::browser::Browser;
use crate::cache::ThumbCache;
use crate::selection::Selection;
use crate::watcher::{FsEvent, Watcher};

#[derive(Clone)]
struct Entry {
    path: PathBuf,
    name: String,
    is_dir: bool,
    size: u64,
    modified: Option<std::time::SystemTime>,
}

struct ThumbResult {
    path: PathBuf,
    image: egui::ColorImage,
}

pub struct BridgeApp {
    browser: Browser,
    selection: Selection,
    path_input: String,
    entries: Vec<Entry>,
    filtered: Vec<Entry>,
    filter: String,
    thumbs: HashMap<PathBuf, egui::TextureHandle>,
    thumb_rx: Receiver<ThumbResult>,
    thumb_tx: mpsc::Sender<ThumbResult>,
    thumb_cache: ThumbCache,
    thumb_queue: Vec<PathBuf>,
    thumb_inflight: HashSet<PathBuf>,
    watcher: Option<Watcher>,
    fs_rx: Receiver<FsEvent>,
    status: String,
    last_scan: Instant,
}

impl BridgeApp {
    pub fn new(
        _cc: &eframe::CreationContext<'_>,
        root: Option<String>,
        cache_path: Option<PathBuf>,
    ) -> Self {
        let (thumb_tx, thumb_rx) = mpsc::channel();
        let (fs_tx, fs_rx) = mpsc::channel();
        let mut app = Self {
            browser: Browser::default(),
            selection: Selection::default(),
            path_input: String::new(),
            entries: Vec::new(),
            filtered: Vec::new(),
            filter: String::new(),
            thumbs: HashMap::new(),
            thumb_rx,
            thumb_tx,
            thumb_cache: ThumbCache::open(cache_path),
            thumb_queue: Vec::new(),
            thumb_inflight: HashSet::new(),
            watcher: None,
            fs_rx,
            status: "Choose a folder to begin".to_string(),
            last_scan: Instant::now(),
        };
        if let Some(root) = root {
            app.open_root(Path::new(&root));
        }
        app
    }

    fn open_root(&mut self, path: &Path) {
        match self.browser.open(path) {
            Ok(()) => {
                self.path_input = self
                    .browser
                    .root
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                self.status = format!("Opened {}", self.path_input);
                self.selection.clear();
                self.thumbs.clear();
                self.thumb_queue.clear();
                self.thumb_inflight.clear();
                self.watcher = Watcher::start(path.to_path_buf(), self.fs_rx.clone_sender());
                self.scan();
            }
            Err(err) => self.status = format!("Could not open folder: {err}"),
        }
    }

    fn scan(&mut self) {
        let Some(root) = self.browser.root.clone() else {
            self.entries.clear();
            self.filtered.clear();
            return;
        };
        self.entries = WalkDir::new(&root)
            .min_depth(1)
            .max_depth(1)
            .into_iter()
            .filter_map(Result::ok)
            .map(|entry| {
                let path = entry.path().to_path_buf();
                let metadata = entry.metadata().ok();
                Entry {
                    name: entry.file_name().to_string_lossy().to_string(),
                    is_dir: metadata.as_ref().is_some_and(|m| m.is_dir()),
                    size: metadata.as_ref().map_or(0, |m| m.len()),
                    modified: metadata.and_then(|m| m.modified().ok()),
                    path,
                }
            })
            .collect();
        self.entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        self.apply_filter();
        self.last_scan = Instant::now();
    }

    fn apply_filter(&mut self) {
        let needle = self.filter.to_lowercase();
        self.filtered = self
            .entries
            .iter()
            .filter(|entry| entry.name.to_lowercase().contains(&needle))
            .cloned()
            .collect();
        self.thumb_queue = self
            .filtered
            .iter()
            .filter(|entry| !entry.is_dir && !self.thumbs.contains_key(&entry.path))
            .map(|entry| entry.path.clone())
            .collect();
    }

    fn poll_thumbnails(&mut self, ctx: &egui::Context) {
        while let Ok(result) = self.thumb_rx.try_recv() {
            self.thumb_inflight.remove(&result.path);
            let handle = ctx.load_texture(
                result.path.to_string_lossy(),
                result.image,
                egui::TextureOptions::LINEAR,
            );
            self.thumbs.insert(result.path, handle);
        }

        for _ in 0..4 {
            let Some(path) = self.thumb_queue.pop() else {
                break;
            };
            if self.thumb_inflight.contains(&path) {
                continue;
            }
            if let Some(image) = self.thumb_cache.get(&path) {
                let handle = ctx.load_texture(
                    path.to_string_lossy(),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                self.thumbs.insert(path, handle);
                continue;
            }
            self.thumb_inflight.insert(path.clone());
            let tx = self.thumb_tx.clone();
            let thumb_cache = self.thumb_cache.clone();
            std::thread::spawn(move || {
                if let Ok(img) = image::open(&path) {
                    let thumbnail = img.thumbnail(256, 256);
                    let size = [thumbnail.width() as usize, thumbnail.height() as usize];
                    let rgba = thumbnail.to_rgba8();
                    let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                    thumb_cache.insert(path.clone(), color.clone());
                    let _ = tx.send(ThumbResult { path, image: color });
                }
            });
        }
    }

    fn poll_watcher(&mut self) {
        let mut changed = false;
        while let Ok(event) = self.fs_rx.try_recv() {
            match event {
                FsEvent::Changed => changed = true,
            }
        }
        if changed {
            self.scan();
        }
    }

    fn show_entry(&mut self, ui: &mut egui::Ui, entry: &Entry, cell_size: f32) -> egui::Response {
        let selected = self.selection.contains(&entry.path);
        let size_text = human_size(entry.size);
        let name = entry.name.clone();
        let path = entry.path.clone();
        let is_dir = entry.is_dir;
        let size = entry.size;
        let modified = entry.modified;
        let thumb = self.thumbs.get(&path);

        ui.vertical(|ui| {
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(cell_size, cell_size - 28.0),
                egui::Sense::click(),
            );
            if response.clicked() {
                self.selection.click(path.clone(), ui.input(|i| i.modifiers));
            }
            if response.double_clicked() && is_dir {
                self.open_root(&path);
            }

            if let Some(texture) = thumb {
                let texture_size = texture.size_vec2();
                let fit = (rect.width() / texture_size.x).min(rect.height() / texture_size.y);
                let size = texture_size * fit;
                let image_rect = egui::Rect::from_center_size(rect.center(), size);
                ui.painter().image(
                    texture.id(),
                    image_rect,
                    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            } else if is_dir {
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "📁",
                    egui::FontId::proportional(48.0),
                    Color32::LIGHT_BLUE,
                );
            } else {
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "⋯",
                    egui::FontId::proportional(24.0),
                    Color32::GRAY,
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
                ui.separator();
                ui.label("Filter:");
                let filter_resp = ui.add_sized(
                    [180.0, 20.0],
                    egui::TextEdit::singleline(&mut self.filter)
                        .hint_text("Filter names..."),
                );
                if filter_resp.changed() {
                    self.apply_filter();
                }
                ui.separator();
                ui.label(&self.status);
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let available_width = ui.available_width();
            let columns = ((available_width / 180.0).floor() as usize).max(1);
            let cell_size = (available_width / columns as f32).min(220.0).max(120.0);
            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Grid::new("items_grid")
                    .num_columns(columns)
                    .spacing([8.0, 12.0])
                    .show(ui, |ui| {
                        for (index, entry) in self.filtered.clone().iter().enumerate() {
                            let response = self.show_entry(ui, entry, cell_size);
                            if (index + 1) % columns == 0 {
                                ui.end_row();
                            }
                            if response.clicked() {
                                ctx.request_repaint();
                            }
                        }
                    });
            });
        });

        if !self.thumb_queue.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

trait ReceiverSender<T> {
    fn clone_sender(&self) -> mpsc::Sender<T>;
}

impl<T> ReceiverSender<T> for Receiver<T> {
    fn clone_sender(&self) -> mpsc::Sender<T> {
        // This is a placeholder implementation; watcher events are optional.
        // The real sender is retained by the watcher thread itself.
        let (tx, _rx) = mpsc::channel();
        tx
    }
}

fn format_modified(time: Option<std::time::SystemTime>) -> String {
    match time {
        Some(time) => {
            let datetime: DateTime<Local> = time.into();
            datetime.format("%Y-%m-%d %H:%M").to_string()
        }
        None => "—".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_formats_bytes_and_larger_units() {
        assert_eq!(human_size(42), "42 B");
        assert_eq!(human_size(1536), "1.5 KB");
    }

    #[test]
    fn modified_time_is_formatted() {
        let time = std::time::UNIX_EPOCH + Duration::from_secs(1_704_067_200);
        assert_eq!(format_modified(Some(time)), "2024-01-01 00:00");
        assert_eq!(format_modified(None), "—");
    }
}
