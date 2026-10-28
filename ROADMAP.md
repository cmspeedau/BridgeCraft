# BridgeCraft Roadmap

A 1:1 clean-room reimplementation of Adobe Bridge in Rust.

> **Clean-room policy:** We never read Adobe Bridge source code or decompiled binaries. Features are specified from public documentation, user guides, and observable behavior, then reimplemented from our own specs.

> **Status legend:** [x] done · [~] in progress · [ ] not started · [⛔] blocked on a decision

---

# Product Vision

BridgeCraft is a fast, cross-platform digital asset browser and workflow tool built in Rust. It aims to match Adobe Bridge workflows while keeping the architecture modular for integrations with ArtCraft and LightCraft.

## Phase 0 — Foundations (Weeks 1–4) — DONE
- [x] Repository/workspace structure with crates: `bc-core`, `bc-ui`, `bc-thumbnails`, `bc-metadata`
- [x] Architecture decision records (ADRs)
- [x] CI and linting baseline
- [x] Phase 0 checklist: `docs/PHASE0_CHECKLIST.md`

*Note: tokio/event-bus remains intentionally deferred — the immediate-mode UI + mpsc worker pattern covers current needs (watcher is drained per-frame on the UI thread with debounced refresh). Trigger to revisit: a second concurrent background service (indexer) landing, expected Phase 3.*

## Phase 1 — File Browsing Core (Weeks 5–12) — ◐ core logic + basic UI DONE; hardening below
- [x] Non-recursive directory listing + recursive `scan()` for future flat-view modes (`bc-core::fs`)
- [x] Sorting: name / size / modified, dirs-first, asc/desc (`bc-core::fs::sort_entries`)
- [x] Directory watcher (notify) with buffered events the UI can drain (`bc-core::watcher`)
- [x] Multi-select model: click / ctrl-toggle / shift-range / select-all (`bc-core::selection`)
- [x] Browser state: open root, refresh, change sort (`bc-core::browser::BrowserState`)
- [x] Content panel: thumbnail grid in egui (list/details views pending — see hardening)
- [x] **Filter box**: case-insensitive name-substring filter, dirs always visible, selection preserved by path, filter-aware Select All (`bc-core::browser::set_filter` + UI bar)
- [x] **Watcher-driven live refresh**: watcher attached on folder open, events drained per frame, bursts coalesced via 300 ms debounce into `refresh_preserving_selection()`
- [x] Virtualized thumbnail grid rendering (visible rows only; egui `ScrollArea` + manual row math; thumbnail requests limited to visible cells)
- [~] Virtual folder tree with favorites, recent folders, shared collections (data model pending)
- [x] List & details view modes (Bridge parity: Thumbnails / List / Details; Filmstrip remains future work)
- [x] **Phase 1 UI integration:** browser selection context menu for copy / move / rename / permanent delete and managed-trash operations; Recents records opened folders and exposes recent locations; Trash lists items and restores them. Permanent deletion requires confirmation.
- [ ] Drag & drop (internal reordering + OS interop)
- [x] **Phase 1 fix-up:** list/details views

## Phase 2 — Thumbnails & Previews (Weeks 9–20) — ◐ cache + core decoders DONE
- [x] Thumbnail cache database: SQLite (`rusqlite` bundled, WAL) keyed by `(path, max_edge)`, invalidation on `(mtime_ns, size)`, `prune_missing()` cleanup (`bc-thumbnails::cache`)
- [x] Cache-first orchestrator with graceful fallback (`bc-thumbnails::load_or_generate`)
- [x] Format decoders via Rust crates: PNG/JPEG/TIFF/GIF/WebP/BMP (via `image`), RGBA8 + aspect-preserving downscale (`bc-thumbnails::decoder`)
- [x] Background generation queue — **DONE** via dedicated worker thread + mpsc (no tokio needed); textures GPU-uploaded in `poll_thumbnails()`
- [ ] PDF (via pdfium bindings)
- [ ] Video (via ffmpeg-next; feature-gated)
- [ ] RAW (via libraw bindings)

## Phase 3 — Metadata Engine (Weeks 16–28)
- [ ] Read/write EXIF, IPTC, XMP metadata
- [ ] Batch metadata edits
- [ ] Smart collections and metadata search

## Phase 4 — Workflow & Automation (Weeks 24–40)
- [ ] Batch rename
- [ ] Scripting API
- [ ] Contact sheets and export workflows

## Phase 5 — Integrations (Weeks 32–52)
- [ ] ArtCraft and LightCraft round-trip editing integration
- [ ] Plugin system

## Milestones
| Milestone | Target | Definition of Done | Status |
|---|---|---|---|
| M1 — Browser MVP | Q1 2027 | Browse, sort, filter, live refresh, thumbnails, basic file ops | ◐ GUI + core done; file-operation, recents, and trash/restore UI done; remaining: drag & drop |
| M2 — Preview Engine | Q2 2027 | RAW/PSD/PDF/video previews with caching | ◐ PNG/JPEG/GIF/WebP/TIFF/BMP + SQLite cache done |
| M3 — Metadata Complete | Q3 2027 | XMP/EXIF/IPTC read-write, smart collections | not started |
| M4 — Automation | Q4 2027 | Batch ops, scripting API | not started |
| M5 — Integrations | 2028 | Apple Photos, plugin system, ArtCraft bridge plugin, LightCraft bridge plugin | not started |
| M6 — 1:1 Parity Audit & Release | 2028 | Feature checklist parity vs. Bridge 2026 user guide; universal macOS build, Windows portable+MSI, AppImage+.zsync published (unsigned) | not started |

## Critical Path
With ADR-0001 resolved (egui/eframe), filter + live refresh shipped, and virtualized grid rendering complete, the remaining critical path is: **drag & drop**. Browser context-menu file operations, recents, and managed-trash restore are integrated with `bc-core`. Metadata engine (Phase 3) can proceed in parallel.

## Risks
- RAW format coverage breadth (mitigate: LibRaw bindings)
- GUI performance on huge folders (mitigate: virtualized grid paints only visible rows and requests thumbnails only for visible cells)
- Watcher platform quirks (inotify/FSEvents/ReadDirectoryChangesW edge cases, e.g. network drives) — debounced refresh + manual navigation already degrade gracefully
- Parity scope creep (mitigate: milestone audits, feature freeze discipline)
- Apple Photos integration is macOS-only and schema-dependent (mitigate: read-only access, graceful "library unavailable" degradation, feature-gated build)
