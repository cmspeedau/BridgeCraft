# Phase 1 — File Browsing Core

Status: implemented in `crates/bc-core` + textual shell in `crates/bc-ui`.

## Checklist

- [x] Non-recursive directory listing (`bc_core::fs::list_dir`)
- [x] Recursive scan retained for future collection/flat-view modes (`bc_core::fs::scan`)
- [x] Deterministic sorting: dirs first, then Name / Size / Modified (`bc_core::fs::sort_entries`)
- [x] Recursive fs watcher with buffered, pollable events (`bc_core::watcher::Watcher`, `notify` crate)
- [x] Multi-select model: click, ctrl-click toggle, shift-range, select-all (`bc_core::selection::Selection`)
- [x] Browser state: open root, refresh, re-sort, selection-aware (`bc_core::browser::BrowserState`)
- [x] Unit tests for all of the above
- [ ] Virtualized 100k-item grid rendering — blocked on GUI framework spike (ADR-0001)
- [ ] Drag & drop — same dependency on ADR-0001

## Notes

Selection indices refer to positions in the currently sorted item list; they are
cleared on re-sort or refresh. Shift-range selection keeps the anchor fixed across
repeated shift-clicks, matching common file-manager behavior.
