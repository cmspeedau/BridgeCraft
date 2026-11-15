# Phase 1 — File Browsing Core

Status: core logic and the workspace shell are shipped; hardening items remain (see ROADMAP.md
Phase 1).

## Checklist

- [x] Non-recursive directory listing (`bc_core::fs::list_dir`)
- [x] Recursive scan retained for future collection/flat-view modes (`bc_core::fs::scan`)
- [x] Deterministic sorting: dirs first, then Name / Size / Modified (`bc_core::fs::sort_entries`)
- [x] Recursive fs watcher with buffered, pollable events (`bc_core::watcher::Watcher`, `notify` crate)
- [x] Multi-select model: click, ctrl-click toggle, shift-range, select-all (`bc_core::selection::Selection`)
- [x] Browser state: open root, refresh, re-sort, selection-aware (`bc_core::browser::BrowserState`)
- [x] Unit tests for all of the above
- [x] Virtualized grid rendering: `bc-ui` paints visible rows only and requests thumbnails only for
  visible cells. The 100k-item case is not measured in CI; the perf targets live in ROADMAP.md
  Phase 6 ("cold start < 2s, folder open < 500ms for 10k items") and are still unverified.
- [x] Filter box, watcher-driven debounced refresh, list/details views, favorites/recents/trash
  panels, breadcrumb path bar, status bar, zoom slider ("thumbnails only" mode)
- [x] File operations wired into the browser: copy, move, single rename, batch rename, trash and
  restore, permanent delete
- [~] Drag & drop: internal drag shipped (drag the selection from a grid cell or list/details row onto a folder to move it, Alt to copy; desktop files dropped in are copied). Still open: outbound drag to the OS (not possible on eframe 0.27), sidebar and Favorites as targets

## Notes

Selection indices refer to positions in the currently sorted item list; they are
cleared on re-sort or refresh. Shift-range selection keeps the anchor fixed across
repeated shift-clicks, matching common file-manager behavior.
