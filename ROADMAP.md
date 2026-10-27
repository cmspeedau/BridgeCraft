# BridgeCraft Roadmap

A 1:1 clean-room reimplementation of Adobe Bridge in Rust.

> **Clean-room policy:** We never read Adobe Bridge source code or decompiled binaries. Features are specified from public documentation, user guides, and observable behavior, then reimplemented from our own specs.

> **Status legend:** [x] done · [~] in progress · [ ] not started · [⛔] blocked on a decision

---

## Phase 0 — Foundations (Weeks 1–4) — ✅ COMPLETE (except GUI spike)
- [x] Rust workspace: `crates/bc-core`, `crates/bc-ui`, `crates/bc-metadata`, `crates/bc-thumbnails` (CLI folded into `bc-ui` binary; `crates/core` → `bc-core` renamed)
- [x] CI (GitHub Actions): fmt, clippy (`-D warnings`), tests on Win/macOS/Linux
- [~] GUI framework selection & spike (egui / Tauri / Iced / Slint) targeting GPU-accelerated asset grids — ⛔ blocks ADR-0001
- [x] Phase 0 checklist: `docs/PHASE0_CHECKLIST.md`

*Note: tokio is in the workspace deps but an event bus architecture has not been built yet; it is deferred into the GUI spike work (Phase 1 items below assume polling-friendly designs in `bc-core::watcher` until then).*

## Phase 1 — File Browsing Core (Weeks 5–12) — ◐ core logic DONE, UI items ⛔ blocked on ADR-0001
- [x] Non-recursive directory listing + recursive `scan()` for future flat-view modes (`bc-core::fs`)
- [x] Sorting: name / size / modified, dirs-first, asc/desc (`bc-core::fs::sort_entries`)
- [x] Directory watcher (notify) with buffered events the UI can drain (`bc-core::watcher`)
- [x] Multi-select model: click / ctrl-toggle / shift-range / select-all (`bc-core::selection`)
- [x] Browser state: open root, refresh, change sort (`bc-core::browser::BrowserState`)
- [~] Virtual folder tree with favorites, recent folders, shared collections (data model pending; UI blocked)
- [⛔] Content panel: list / details / thumbnails views — blocked on GUI framework (ADR-0001)
- [⛔] Virtualized thumbnail grid rendering (100k+ items, 60fps target) — blocked on ADR-0001
- [~] Filtering, search (sort done; filter/search pending)
- [ ] Copy / move / rename / delete (with recents/bin)
- [ ] Drag & drop (internal + OS interop)

## Phase 2 — Thumbnails & Previews (Weeks 9–20) — ◐ cache + core decoders DONE
- [x] Thumbnail cache database: SQLite (`rusqlite` bundled, WAL) keyed by `(path, max_edge)`, invalidation on `(mtime_ns, size)`, `prune_missing()` cleanup (`bc-thumbnails::cache`)
- [x] Cache-first orchestrator with graceful fallback (`bc-thumbnails::load_or_generate`)
- [x] Format decoders via Rust crates: PNG/JPEG/TIFF/GIF/WebP/BMP (via `image`), RGBA8 + aspect-preserving downscale (`bc-thumbnails::decoder`)
- [ ] PDF (via pdfium bindings)
- [ ] Video via ffmpeg bindings
- [ ] Camera RAW support (LibRaw bindings or DNG parsing)
- [ ] PSD/AI/Illustrator preview extraction
- [ ] Preview quality levels: thumbnail / preview / 1:1 zoom viewer
- [ ] Batch (re)build cache command
- [⛔] Background generation queue — blocked on GUI framework / threading decision (ADR-0001)
- [~] Reuse embedded XMP/EXIF previews as fast-path thumbnails — deferred to Phase 3 (needs metadata reader)

## Phase 3 — Metadata Engine (Weeks 15–26) — NOT STARTED (skeleton only)
- [~] `bc-metadata` crate skeleton exists with a `Reader` trait; real implementation pending (from open specs: CIPA EXIF, IPTC IIM4, ISO 16684-2 XMP)
- [ ] Embedded metadata read/write: EXIF, IPTC-IIM, XMP
- [ ] XMP sidecar read/write (`.xmp` files)
- [ ] Metadata panel with editable fields
- [ ] Custom metadata templates
- [ ] Keyword hierarchy management & tagging
- [ ] Star ratings, color labels, approve/reject flags
- [ ] Metadata-driven search & smart collections (query builder)

## Phase 4 — Batch Operations & Automation (Weeks 24–34)
- [ ] Batch rename (token-based patterns)
- [ ] Batch resize/convert/export
- [ ] Contact sheet / PDF contact sheet output
- [ ] Watched-folder auto-processing
- [ ] Scripting/automation API (Lua or WASM plugins)

## Phase 5 — Creative Cloud-Style Integrations (clean-room equivalents) (Weeks 30–40)
- [ ] Export/publish pipelines (replaces CC Libraries)
- [ ] Version control / version stacks for assets
- [ ] Migration importer (reads existing Bridge caches read-only where formats are documented)

## Phase 6 — Polish & 1:1 Parity (Weeks 36–52)
- [ ] Keyboard shortcut parity + custom shortcut editor
- [ ] Workspaces (customizable panel layouts)
- [ ] Preferences dialog parity audit
- [ ] Localization framework (i18n)
- [ ] High-DPI + accessibility (screen reader, keyboard nav)
- [ ] Performance profiling: cold start < 2s, folder open < 500ms for 10k items

## Milestones
| Milestone | Target | Definition of Done | Status |
|---|---|---|---|
| M1 — Browser MVP | Q1 2027 | Browse, sort, thumbnails, basic file ops | ◐ core logic done; UI blocked on ADR-0001 |
| M2 — Preview Engine | Q2 2027 | RAW/PSD/PDF/video previews with caching | ◐ PNG/JPEG/GIF/WebP/TIFF/BMP + SQLite cache done |
| M3 — Metadata Complete | Q3 2027 | XMP/EXIF/IPTC read-write, smart collections | not started |
| M4 — Automation | Q4 2027 | Batch ops, scripting API | not started |
| M5 — 1:1 Parity Audit | 2028 | Feature checklist parity vs. Bridge 2026 user guide | not started |

## Critical Path
**GUI framework spike → ADR-0001** is now the single blocking decision for all remaining UI work (Phase 1 content panel/virtualized grid/drag & drop, Phase 2 background queue). Metadata engine (Phase 3) and file ops (Phase 1 copy/move/rename/delete) can proceed in parallel without it.

## Risks
- RAW format coverage breadth (mitigate: LibRaw bindings)
- GUI performance on huge folders (mitigate: virtualization + cache early — cache shipped in Phase 2)
- Parity scope creep (mitigate: milestone audits, feature freeze discipline)
- GUI framework choice delays (mitigate: run spike next; every UI item on the critical path depends on it)
