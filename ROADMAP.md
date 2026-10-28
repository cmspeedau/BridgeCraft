# BridgeCraft Roadmap

A 1:1 clean-room reimplementation of Adobe Bridge in Rust.

> **Clean-room policy:** We never read Adobe Bridge source code or decompiled binaries. Features are specified from public documentation, user guides, and observable behavior, then reimplemented from our own specs.

> **Status legend:** [x] done · [~] in progress · [ ] not started · [⛔] blocked on a decision

---

## Phase 0 — Foundations (Weeks 1–4) — ✅ COMPLETE
- [x] Rust workspace: `crates/bc-core`, `crates/bc-ui`, `crates/bc-metadata`, `crates/bc-thumbnails` (CLI folded into `bc-ui` binary; `crates/core` → `bc-core` renamed)
- [x] CI (GitHub Actions): fmt, clippy (`-D warnings`), tests on Win/macOS/Linux
- [x] GUI framework selection & spike — **DONE: egui/eframe 0.27 chosen, ADR-0001 accepted**. Working GUI shell shipped: thumbnail grid, selection (click/ctrl/shift), navigation, sorting, worker-thread thumbnail pipeline with mpsc channel draining. See `docs/adr/0001-gui-framework.md`.
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
- [ ] Copy / move / rename / delete (with recents/bin)
- [ ] Drag & drop (internal reordering + OS interop)
- [x] **Phase 1 fix-up:** list/details views

## Phase 2 — Thumbnails & Previews (Weeks 9–20) — ◐ cache + core decoders DONE
- [x] Thumbnail cache database: SQLite (`rusqlite` bundled, WAL) keyed by `(path, max_edge)`, invalidation on `(mtime_ns, size)`, `prune_missing()` cleanup (`bc-thumbnails::cache`)
- [x] Cache-first orchestrator with graceful fallback (`bc-thumbnails::load_or_generate`)
- [x] Format decoders via Rust crates: PNG/JPEG/TIFF/GIF/WebP/BMP (via `image`), RGBA8 + aspect-preserving downscale (`bc-thumbnails::decoder`)
- [x] Background generation queue — **DONE** via dedicated worker thread + mpsc (no tokio needed); textures GPU-uploaded in `poll_thumbnails()`
- [ ] PDF (via pdfium bindings)
- [ ] Video via ffmpeg bindings
- [ ] Camera RAW support (LibRaw bindings or DNG parsing)
- [ ] PSD/AI/Illustrator preview extraction
- [ ] Preview quality levels: thumbnail / preview / 1:1 zoom viewer
- [ ] Batch (re)build cache command
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

## Phase 5 — Integrations & Plugin Ecosystem (clean-room equivalents) (Weeks 30–40)
- [ ] Export/publish pipelines (replaces CC Libraries)
- [ ] Version control / version stacks for assets
- [ ] Migration importer (reads existing Bridge caches read-only where formats are documented)
- [ ] **Apple Photos support**: read-only integration with the macOS Photos library
  - [ ] Locate and open the user's `Photos.sqlite` library (read-only; never write to it) via `photoslibrary`-style access on macOS
  - [ ] Enumerate moments/albums as virtual folders in the browser tree; surface originals + edits as entries
  - [ ] Extract assets (originals and current renditions) into the thumbnail pipeline and preview viewer; iCloud-shared (`cloudPlaceholder`) items show a downloaded-on-demand state
  - [ ] Metadata mapping: Photos keywords/albums/favorites → bc-metadata tags (Phase 3 alignment)
  - [ ] Platform gate: macOS-only feature, compiled out elsewhere; user must grant disk access
  - *Clean-room note: spec from public Photos library schema documentation and observed behavior only; no Apple code.*
- [ ] **Plugin system for BridgeCraft** (successor to the Phase 4 scripting API; Bridges-style extension model)
  - [ ] Plugin manifest + loader: local plugins discovered from a plugins directory; manifest declares name, version, entry points, and capabilities (menu items, panel, context-menu hooks)
  - [ ] Plugin API surface (Rust trait + WASM host boundary): access to browser selection, current folder, thumbnails, metadata read, and file operations through a permissioned host API
  - [ ] Sandboxing: WASM plugins run isolated with explicit capabilities (fs paths, network off by default); native plugins out of scope for v1
  - [ ] UI extension points: plugin-registered panels, toolbar/menu actions, and batch-processing hooks
  - [ ] Plugin manager dialog: list, enable/disable, permissions review
  - *Relationship to Phase 4: the Phase 4 Lua/WASM scripting API is the automation surface; this is the packaged, distributable plugin layer built on top of it.*
- [ ] **ArtCraft bridge plugin** (first-party plugin targeting [storytold/artcraft](https://github.com/storytold/artcraft))
  - [ ] ArtCraft is a Rust, MIT/Apache-2.0 dual-licensed "IDE for artists" (AI image/video generation with Photoshop-style 2D/3D canvas compositing, layers, masking, inpainting) — a natural round-trip target for asset workflows, and its permissive license allows direct API integration (no clean-room constraints needed for this partner)
  - [ ] Round-trip workflow: "Edit in ArtCraft" context-menu action on selected images → open in ArtCraft canvas → on save/export, updated file lands back in the browsed folder and the watcher refresh picks it up automatically (Phase 1 live refresh makes this near-free)
  - [ ] Asset hand-off: send BridgeCraft selections (with XMP sidecar metadata, once Phase 3 lands) into ArtCraft projects as layers/ingredients; receive exports back as versioned assets (version stacks integration)
  - [ ] Discovery: locate the ArtCraft executable/library on Win/macOS/Linux (it ships for all three), or let the user point at an install
  - [ ] Implement as the first consumer of the plugin system above to prove the API surface
- [ ] **LightCraft bridge plugin** (first-party plugin targeting [storytold/lightcraft](https://github.com/storytold/lightcraft))
  - [ ] LightCraft is a Rust, MIT/Apache-2.0 dual-licensed, non-destructive photo library and raw developer written in pure Rust (the Lightroom analogue to ArtCraft's Photoshop analogue), cross-platform on macOS/Windows/Linux plus WASM — a natural round-trip target for photo culling and RAW workflows, and its permissive license allows direct API integration (no clean-room constraints needed for this partner)
  - [ ] Round-trip workflow: "Edit in LightCraft" context-menu action on selected images/RAWs → develop non-destructively in LightCraft → on export, rendered files land back in the browsed folder and the watcher refresh picks them up automatically (Phase 1 live refresh makes this near-free)
  - [ ] Library-aware hand-off: send BridgeCraft selections (with XMP sidecar metadata and ratings/labels, once Phase 3 lands) into LightCraft's library; receive develop settings and exports back as versioned assets (version stacks integration)
  - [ ] RAW synergy: LightCraft's Rust RAW decoding can complement Phase 2's camera-RAW plans (evaluate reusing its decoders via the plugin boundary rather than duplicating LibRaw bindings)
  - [ ] Discovery: locate the LightCraft executable/library on Win/macOS/Linux (it ships for all three), or let the user point at an install
  - [ ] Implement alongside ArtCraft as a second consumer of the plugin system to prove the API surface generalizes beyond a single partner

## Phase 6 — Polish, Packaging & 1:1 Parity (Weeks 36–52)
- [ ] Keyboard shortcut parity + custom shortcut editor
- [ ] Workspaces (customizable panel layouts)
- [ ] Preferences dialog parity audit
- [ ] Localization framework (i18n)
- [ ] High-DPI + accessibility (screen reader, keyboard nav)
- [ ] Performance profiling: cold start < 2s, folder open < 500ms for 10k items
- [ ] **Packaging & distribution** (final release targets; unsigned)
  - [ ] **macOS**: universal 2 binary (arm64 + x86_64 via `lipo`), unsigned `.app` bundle in a `.dmg`
  - [ ] **Windows**: portable ZIP build + MSI installer (WiX); per-machine and per-user install options
  - [ ] **Linux**: AppImage bundling all shared libs, with `.zsync` metadata for delta updates (AppImageUpdater / `appimageupdatetool`-compatible)
  - [ ] CI release pipeline: per-platform build matrix, checksums, and GitHub release publishing
  - [ ] In-app or on-launch update check feeding the AppImage delta channel (opt-in, respects privacy)
  - *Note: the AppImage `.zsync` delta channel only applies to Linux; no signing/notarization anywhere — users may see OS warnings (Gatekeeper "unidentified developer", SmartScreen) on macOS/Windows.*

## Milestones
| Milestone | Target | Definition of Done | Status |
|---|---|---|---|
| M1 — Browser MVP | Q1 2027 | Browse, sort, filter, live refresh, thumbnails, basic file ops | ◐ GUI + core done; remaining: list/details views, file ops, drag & drop |
| M2 — Preview Engine | Q2 2027 | RAW/PSD/PDF/video previews with caching | ◐ PNG/JPEG/GIF/WebP/TIFF/BMP + SQLite cache done |
| M3 — Metadata Complete | Q3 2027 | XMP/EXIF/IPTC read-write, smart collections | not started |
| M4 — Automation | Q4 2027 | Batch ops, scripting API | not started |
| M5 — Integrations | 2028 | Apple Photos, plugin system, ArtCraft bridge plugin, LightCraft bridge plugin | not started |
| M6 — 1:1 Parity Audit & Release | 2028 | Feature checklist parity vs. Bridge 2026 user guide; universal macOS build, Windows portable+MSI, AppImage+.zsync published (unsigned) | not started |

## Critical Path
With ADR-0001 resolved (egui/eframe), filter + live refresh shipped, and virtualized grid rendering complete, the remaining critical path is: **list/details views → file operations → drag & drop**. Metadata engine (Phase 3) can proceed in parallel.

## Risks
- RAW format coverage breadth (mitigate: LibRaw bindings)
- GUI performance on huge folders (mitigate: virtualized grid paints only visible rows and requests thumbnails only for visible cells)
- Watcher platform quirks (inotify/FSEvents/ReadDirectoryChangesW edge cases, e.g. network drives) — debounced refresh + manual navigation already degrade gracefully
- Parity scope creep (mitigate: milestone audits, feature freeze discipline)
- Apple Photos integration is macOS-only and schema-dependent (mitigate: read-only access, graceful "library unavailable" degradation, feature-gated build)
- Plugin sandboxing mistakes could expose user files (mitigate: WASM-only v1, deny-by-default capabilities, permission prompts in the plugin manager)
- ArtCraft integration depends on an external project's evolving APIs (mitigate: version the bridge plugin against specific ArtCraft releases; its MIT/Apache licensing keeps the dependency clean)
- LightCraft integration depends on an external project's evolving APIs (mitigate: version the bridge plugin against specific LightCraft releases; its MIT/Apache licensing keeps the dependency clean, and it is young/moving fast so pin early)
- Unsigned distribution artifacts will trigger OS trust warnings (macOS Gatekeeper, Windows SmartScreen) that users must dismiss manually (accepted trade-off: no signing certificates are purchased or managed)
