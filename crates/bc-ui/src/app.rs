use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Color32, RichText};

use crate::browser::{Browser, Entry};
use crate::selection::Selection;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ViewMode {
    Grid,
    List,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortColumn {
    Name,
    Size,
    Modified,
    Type,
}

impl SortColumn {
    fn next(self) -> Self {
        match self {
            Self::Name => Self::Size,
            Self::Size => Self::Modified,
            Self::Modified => Self::Type,
            Self::Type => Self::Name,
        }
    }
}

pub struct App {
    browser: Browser,
    selection: Selection,
    view_mode: ViewMode,
    sort_column: SortColumn,
    sort_ascending: bool,
    search: String,
    status: String,
    last_status: Instant,
    rename_buffer: String,
    rename_target: Option<PathBuf>,
    context_menu_target: Option<PathBuf>,
    show_hidden: bool,
    show_info: bool,
}

impl Default for App {
    fn default() -> Self {
        let mut browser = Browser::default();
        let status = match browser.refresh() {
            Ok(()) => String::from("Ready"),
            Err(error) => format!("Error: {error}"),
        };

        Self {
            browser,
            selection: Selection::default(),
            view_mode: ViewMode::Grid,
            sort_column: SortColumn::Name,
            sort_ascending: true,
            search: String::new(),
            status,
            last_status: Instant::now(),
            rename_buffer: String::new(),
            rename_target: None,
            context_menu_target: None,
            show_hidden: false,
            show_info: false,
        }
    }
}

impl App {
    fn set_status(&mut self, message: impl Into<String>) {
        self.status = message.into();
        self.last_status = Instant::now();
    }

    fn refresh(&mut self) {
        match self.browser.refresh() {
            Ok(()) => self.set_status("Ready"),
            Err(error) => self.set_status(format!("Error: {error}")),
        }
    }

    fn navigate_up(&mut self) {
        if self.browser.navigate_up() {
            self.selection.clear();
            self.refresh();
        }
    }

    fn open_path(&mut self, path: &Path) {
        match self.browser.navigate_to(path.to_path_buf()) {
            Ok(()) => {
                self.selection.clear();
                self.refresh();
            }
            Err(error) => self.set_status(format!("Error: {error}")),
        }
    }

    fn open_selected(&mut self) {
        let Some(path) = self.selection.selected().next() else {
            return;
        };
        let path = path.to_path_buf();
        if path.is_dir() {
            self.open_path(&path);
        } else {
            self.set_status(format!("Selected {}", path.display()));
        }
    }

    fn delete_selected(&mut self) {
        let paths: Vec<_> = self.selection.selected().cloned().collect();
        if paths.is_empty() {
            return;
        }
        match self.browser.delete(&paths) {
            Ok(()) => {
                self.selection.clear();
                self.refresh();
                self.set_status(format!("Deleted {} item(s)", paths.len()));
            }
            Err(error) => self.set_status(format!("Error: {error}")),
        }
    }

    fn start_rename(&mut self, path: PathBuf) {
        self.rename_buffer = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.rename_target = Some(path);
    }

    fn finish_rename(&mut self) {
        let Some(path) = self.rename_target.take() else {
            return;
        };
        let Some(parent) = path.parent() else {
            self.set_status("Cannot rename this item");
            return;
        };
        let new_path = parent.join(&self.rename_buffer);
        match self.browser.rename(&path, &new_path) {
            Ok(()) => {
                self.refresh();
                self.set_status("Renamed");
            }
            Err(error) => self.set_status(format!("Error: {error}")),
        }
    }

    fn cancel_rename(&mut self) {
        self.rename_target = None;
        self.rename_buffer.clear();
    }

    fn entries(&self) -> Vec<Entry> {
        let mut entries: Vec<_> = self
            .browser
            .entries()
            .iter()
            .filter(|entry| {
                self.show_hidden || !entry.name.starts_with('.')
            })
            .filter(|entry| {
                self.search.is_empty()
                    || entry
                        .name
                        .to_lowercase()
                        .contains(&self.search.to_lowercase())
            })
            .cloned()
            .collect();

        entries.sort_by(|a, b| {
            let ordering = match self.sort_column {
                SortColumn::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortColumn::Size => a.size.cmp(&b.size),
                SortColumn::Modified => a.modified.cmp(&b.modified),
                SortColumn::Type => a.extension.cmp(&b.extension),
            };
            let ordering = ordering.then_with(|| a.is_dir.cmp(&b.is_dir).reverse());
            if self.sort_ascending {
                ordering
            } else {
                ordering.reverse()
            }
        });
        entries
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("⬅").on_hover_text("Back").clicked() {
                self.navigate_up();
            }
            if ui.button("⟳").on_hover_text("Refresh").clicked() {
                self.refresh();
            }
            ui.separator();
            ui.label("Location:");
            let mut path_text = self.browser.current_path().display().to_string();
            let response = ui.text_edit_singleline(&mut path_text);
            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                self.open_path(Path::new(&path_text));
            }
            if ui.button("Go").clicked() {
                self.open_path(Path::new(&path_text));
            }
        });

        ui.horizontal(|ui| {
            ui.label("Search:");
            ui.text_edit_singleline(&mut self.search);
            if ui.button("Clear").clicked() {
                self.search.clear();
            }
            ui.separator();
            ui.checkbox(&mut self.show_hidden, "Show hidden");
            ui.checkbox(&mut self.show_info, "Info");
            ui.separator();
            if ui
                .selectable_label(self.view_mode == ViewMode::Grid, "Grid")
                .clicked()
            {
                self.view_mode = ViewMode::Grid;
            }
            if ui
                .selectable_label(self.view_mode == ViewMode::List, "List")
                .clicked()
            {
                self.view_mode = ViewMode::List;
            }
        });
    }

    fn grid_view(&mut self, ui: &mut egui::Ui, entries: &[Entry]) {
        let available_width = ui.available_width();
        let cell_width = 110.0;
        let spacing = 8.0;
        let cols = ((available_width + spacing) / (cell_width + spacing))
            .floor()
            .max(1.0) as usize;
        let row_height = 100.0;
        let indices = self.browser.visible_indices();
        let row_step = row_height + spacing;

        egui::ScrollArea::vertical().show(ui, |ui| {
            let row_count = indices.len().div_ceil(cols);
            let (first_row, last_row) = ui.clip_rect().top_bottom_range();
            let first_row = (first_row / row_step).floor() as usize;
            let last_row = (last_row / row_step).ceil() as usize;
            let end_row = last_row.max(first_row);
            ui.add_space(first_row as f32 * row_step);

            for row in first_row..end_row {
                ui.horizontal(|ui| {
                    for col in 0..cols {
                        let idx = row * cols + col;
                        if let Some(entry) = entries.get(idx) {
                            let response = self.cell(
                                ui,
                                idx,
                                &entry.path,
                                entry.is_dir,
                                entry.size,
                                self.selection.contains(&entry.path),
                                cell_width,
                            );
                            if response.clicked() {
                                self.selection.toggle(entry.path.clone());
                            }
                            if response.double_clicked() {
                                if entry.is_dir {
                                    self.open_path(&entry.path);
                                } else {
                                    self.set_status(format!("Selected {}", entry.path.display()));
                                }
                            }
                            response.context_menu(|ui| {
                                if ui.button("Open").clicked() {
                                    if entry.is_dir {
                                        self.open_path(&entry.path);
                                    }
                                    ui.close_menu();
                                }
                                if ui.button("Rename").clicked() {
                                    self.start_rename(entry.path.clone());
                                    ui.close_menu();
                                }
                                if ui.button("Delete").clicked() {
                                    self.selection.set_single(entry.path.clone());
                                    self.delete_selected();
                                    ui.close_menu();
                                }
                            });
                        } else {
                            ui.allocate_space(egui::vec2(cell_width, row_height));
                        }
                    }
                });
                ui.add_space(spacing);
            }
            let remaining_rows = row_count.saturating_sub(end_row);
            ui.add_space(remaining_rows as f32 * row_step);
        });
    }

    fn list_view(&mut self, ui: &mut egui::Ui, entries: &[Entry]) {
        let mut sort_clicked = None;
        ui.horizontal(|ui| {
            for (label, column) in [
                ("Name", SortColumn::Name),
                ("Size", SortColumn::Size),
                ("Modified", SortColumn::Modified),
                ("Type", SortColumn::Type),
            ] {
                let label = if self.sort_column == column {
                    format!("{label} {}", if self.sort_ascending { "▲" } else { "▼" })
                } else {
                    label.to_string()
                };
                if ui.button(label).clicked() {
                    sort_clicked = Some(column);
                }
            }
        });
        if let Some(column) = sort_clicked {
            if self.sort_column == column {
                self.sort_ascending = !self.sort_ascending;
            } else {
                self.sort_column = column;
                self.sort_ascending = true;
            }
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            for (idx, entry) in entries.iter().enumerate() {
                let selected = self.selection.contains(&entry.path);
                let response = ui.selectable_label(
                    selected,
                    format!(
                        "{}  {}  {}  {}",
                        if entry.is_dir { "📁" } else { "📄" },
                        entry.name,
                        entry.size,
                        entry.extension,
                    ),
                );
                if response.clicked() {
                    self.selection.toggle(entry.path.clone());
                }
                if response.double_clicked() {
                    if entry.is_dir {
                        self.open_path(&entry.path);
                    } else {
                        self.set_status(format!("Selected {}", entry.path.display()));
                    }
                }
                response.context_menu(|ui| {
                    if ui.button("Open").clicked() {
                        if entry.is_dir {
                            self.open_path(&entry.path);
                        }
                        ui.close_menu();
                    }
                    if ui.button("Rename").clicked() {
                        self.start_rename(entry.path.clone());
                        ui.close_menu();
                    }
                    if ui.button("Delete").clicked() {
                        self.selection.set_single(entry.path.clone());
                        self.delete_selected();
                        ui.close_menu();
                    }
                });
                if self.show_info {
                    ui.label(format!("Item {idx}: {}", entry.path.display()));
                }
            }
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
            .unwrap_or_default();
        let icon = if is_dir { "📁" } else { "📄" };
        let label = if self.show_info {
            format!("{icon}\n{name}\n{size} bytes")
        } else {
            format!("{icon}\n{name}")
        };
        let response = ui.add_sized(
            egui::vec2(cell_size, 100.0),
            egui::Button::new(label).selected(selected),
        );
        response
    }

    fn open_root(&mut self, path: &Path) {
        let _ = self.browser.navigate_to(path.to_path_buf());
        self.selection.clear();
        self.refresh();
    }

    fn handle_keyboard(&mut self, ctx: &egui::Context) {
        ctx.input(|input| {
            if input.key_pressed(egui::Key::Escape) {
                self.cancel_rename();
            }
            if input.key_pressed(egui::Key::Enter) {
                self.open_selected();
            }
            if input.key_pressed(egui::Key::Delete) {
                self.delete_selected();
            }
            if input.key_pressed(egui::Key::F2) {
                if let Some(path) = self.selection.selected().next() {
                    self.start_rename(path.to_path_buf());
                }
            }
            if input.modifiers.ctrl && input.key_pressed(egui::Key::A) {
                self.selection.select_all(self.browser.entries());
            }
            if input.key_pressed(egui::Key::ArrowUp) {
                self.navigate_up();
            }
        });
    }

    fn rename_dialog(&mut self, ctx: &egui::Context) {
        let Some(target) = self.rename_target.clone() else {
            return;
        };
        egui::Window::new("Rename")
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(format!("Rename {}", target.display()));
                let response = ui.text_edit_singleline(&mut self.rename_buffer);
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.finish_rename();
                }
                ui.horizontal(|ui| {
                    if ui.button("Rename").clicked() {
                        self.finish_rename();
                    }
                    if ui.button("Cancel").clicked() {
                        self.cancel_rename();
                    }
                });
            });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let count = self.browser.entries().len();
        let selected = self.selection.len();
        let message = if self.last_status.elapsed() > Duration::from_secs(5) {
            format!("{count} item(s) • {selected} selected")
        } else {
            self.status.clone()
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(message).color(Color32::LIGHT_GRAY));
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_keyboard(ctx);
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| self.toolbar(ui));
        egui::CentralPanel::default().show(ctx, |ui| {
            let entries = self.entries();
            match self.view_mode {
                ViewMode::Grid => self.grid_view(ui, &entries),
                ViewMode::List => self.list_view(ui, &entries),
            }
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| self.status_bar(ui));
        self.rename_dialog(ctx);
    }
}
