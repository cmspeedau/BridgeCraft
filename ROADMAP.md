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
- [x] **Bridge-style workspace shell**: left sidebar (Favorites tab backed by a persistent store `bc-core::favorites`, collapsible Folders tree, Recent dock, Trash window), clickable breadcrumb path bar, right Preview/Metadata panel (live preview from the cached 2048 tier + properties incl. header-read dimensions), bottom status bar (view modes, thumbnail zoom slider, "Thumbnails only" filter, items/selected/size summary), toolbar with F5 / Ctrl+A / Ctrl+O — layout target is the public Adobe Bridge Essentials screenshot (visual target only; clean-room per CLEANROOM.md)
- [~] Virtual folder tree with favorites, recent folders, shared collections: favorites & recents shipped as flat JSON stores (`bc-core::favorites`, `bc-core::recents`) and shown in the sidebar; the `bc-core::virtual_folders` registry has no provider wired into the UI; shared/named collections are not built
- [x] List & details view modes (Bridge parity: Thumbnails / List / Details; Filmstrip remains future work)
- [x] Copy / move / rename / delete (with recents/bin) — core operations implemented; persistent recents are bounded, managed trash supports move/list/restore, and permanent delete is separate; browser/UI integration complete (file operations, recents, and trash/restore)
- [~] Drag & drop (internal reordering + OS interop): shipped. Press and drag from any grid cell or list/details row, release on a folder to move the selection there (hold Alt to copy); files dragged in from the desktop are copied into the folder under the pointer, or into the open folder. Transfers go through `bc_core::file_ops::transfer_into_folder`, which validates the whole batch before touching the filesystem, skips items that already live in the destination, rejects a folder dropped into itself or its own descendant, and reports per-item failures in the status line. Remaining: the sidebar Folders tree and Favorites as drop targets, and outbound drag to the OS (eframe 0.27 has no outgoing file-drag API).
- [ ] Grid cell states: a decode that permanently failed renders the same `…` placeholder as one still pending (`failed` is tracked in `BridgeApp` only to suppress retries, never read by `cell()`), so a broken file looks like a slow file
- [ ] Folder-open failures are reported as a transient status-bar string that the next refresh overwrites; the content panel then looks like an empty folder
- [x] **Phase 1 fix-up:** list/details views

## Phase 2 — Thumbnails & Previews (Weeks 9–20) — ✅ COMPLETE
- [x] Thumbnail cache database: SQLite (`rusqlite` bundled, WAL) keyed by `(path, max_edge)`, invalidation on `(mtime_ns, size)`, `prune_missing()` cleanup (`bc-thumbnails::cache`)
- [x] Cache-first orchestrator with graceful fallback (`bc-thumbnails::load_or_generate`)
- [x] Format decoders via Rust crates: PNG/JPEG/TIFF/GIF/WebP/BMP (via `image`), RGBA8 + aspect-preserving downscale (`bc-thumbnails::decoder`)
- [x] Background generation queue — **DONE** via dedicated worker thread + mpsc (no tokio needed); textures GPU-uploaded in `poll_thumbnails()`
- [x] PDF (via pdfium bindings) — `pdfium-render` binds to a user-provided `libpdfium` at run time (not bundled; see `docs/PHASE2.md`)
- [x] Video via ffmpeg bindings — `ffmpeg-sidecar` drives a user-installed `ffmpeg` binary (not bundled or linked; see `docs/PHASE2.md`)
- [x] Camera RAW support — `raw.rs` binds at run time to a user-installed LibRaw via `libloading` (embedded preview fast-path, full-render fallback; not bundled; see `docs/PHASE2.md`)
- [x] PSD embedded preview extraction (resource IDs 1033/1036; JPEG and 24-bit RGB) and PDF-compatible AI rendering via Pdfium; PostScript-only AI returns an explicit unsupported error
- [x] Preview quality levels: thumbnail / preview / 1:1 zoom viewer (`bc-ui::viewer`) — 256 grid tier, cached 2048 preview tier, on-demand native 1:1 decode capped at 8192 px / 64 MP, fit/±/zoom/pan and ←/→ stepping
- [x] Batch (re)build cache command
- [~] Reuse embedded XMP/EXIF previews as fast-path thumbnails — deferred to Phase 3 (needs metadata reader)

*Note: the phase is closed with that one item intentionally deferred — the metadata reader (Phase 3, #5/#6) is a hard dependency, and camera RAW already fast-paths the camera's embedded preview internally. It will be picked up as part of the metadata work in `#5–#19`.*

## Phase 3 — Metadata Engine (Weeks 15–26) — ◐ started: EXIF read + metadata panel shipped (issue #6)
- [x] EXIF reading in `bc-metadata`: pure-Rust TIFF/EXIF parser (JPEG `APP1` + TIFF/DNG containers), both byte orders, IFD0 + Exif + GPS sub-IFDs + thumbnail-IFD reference; IFD loops and absurd counts are `Malformed`, malformed input never panics; hand-built fixture tests plus a real-camera check gated behind `RAW_SAMPLE` (issue #6)
- [x] Metadata panel: Camera Data (EXIF) section for the selected file — camera, taken, exposure, aperture, ISO, focal length, orientation, color space, software, EXIF size, GPS — above File Properties; cached per path + modified time
- [~] Embedded metadata read/write: EXIF read (issue #5 model) and EXIF write for JPEG/TIFF (issue #7) landed; IPTC-IIM read/write for JPEG APP13 (issue #8); XMP sidecar read/write (issue #9); TIFF IPTC container and embedded XMP still pending
- [x] XMP sidecar read/write (`.xmp` files) — `bc-metadata::xmp` (parse/serialize + atomic file I/O)
- [x] Metadata panel with editable fields: grouped list, dirty tracking, Save/Revert via the #5 write policy (`bc-ui::metadata_edit`), multi-selection bulk editor with per-file failure reporting. Sidecar properties now merge in full, so `dc:` and `xmp:` values written by this app or another tool reach the panel instead of being filtered out
- [~] Custom metadata templates: `bc-metadata::templates` has the model (Replace/Merge/Append), JSON persistence (`<data>/BridgeCraft/metadata_templates`) and tests, but nothing calls it: no save/apply UI and no path that writes a template onto a file
- [x] Keyword hierarchy management & tagging: `bc-metadata::keywords` (dc:subject `Parent|Child` convention, tree/type-ahead/tag-count logic) plus a panel editor in `bc-ui::tag_widgets`: chips that remove on click, an add field, and completions drawn from the hierarchy the file already carries. Bulk add and remove across a selection. Cross-library suggestions wait on the index UI
- [x] Star ratings, color labels, approve/reject flags: `bc-metadata::ratings` (XMP `xmp:Rating`, `photoshop:Label`, BridgeCraft `Flag`) plus the panel editor: five drawn stars, the label chips, Approve/Reject/No flag. Clicking the current rating clears it; each property is stampable on its own in the bulk editor, so "set the rating, leave the labels alone" is explicit. Tags write to the XMP sidecar, never into the image bytes, because there is no EXIF equivalent and embedded XMP is still open
- [x] Rating and label display: label dot on the grid thumbnail, dot on list rows, and Rating plus Label columns in the details view, with the header sharing the row column math
- [~] Metadata-driven search & smart collections: the query builder and SQLite index exist (`bc-core::metadata_index`, `bc-metadata::search`) and one auto-processing action writes to the index, but there is no search field, no saved-query UI, and no way to browse a smart collection. Two parallel search/projection implementations exist (see "Tech debt").

## Phase 4 — Batch Operations & Automation (Weeks 24–34)
- [x] Batch rename (token-based patterns) — `bc-core::batch_rename` parser/planner/journaled apply/undo plus `bc-ui` preview/apply dialog, pattern validation, and undo action
- [x] Batch resize/convert/export (issue #15: queued `bc-core` export pipeline + jobs UI; JPEG/PNG/WebP, orientation applied, metadata copy deferred to the Phase 3 writers)
- [~] Contact sheet / PDF contact sheet output: `bc-core::contact_sheet` renders RGBA and paginated PDF sheets and is tested, but no UI or export path calls it. Remaining: a "Contact Sheet…" action plus layout picker
- [~] Watched-folder auto-processing: persisted rules, settle gate, bounded worker queue, preferences/history UI and docs landed. The `IndexIntoMetadataIndex` action exists in core but is not selectable in the rules UI (Thumbnail / Rename / Export PNG / Dated folder only)
- [~] Scripting/automation API (WASM): local manifest-based scripts use the shared `wasmi` sandbox, fuel/memory limits, default-deny persisted capability and filesystem grants, and permissioned selection/current-folder/metadata/thumbnail/file-operation host imports; per-script failures are isolated. **Runtime is done, the surface is not**: `bc-core::scripting::ScriptHost` has no UI (no scripts window), so scripts can only be discovered from the scripts directory; the plugin path below is what users can actually drive today

## Phase 5 — Integrations & Plugin Ecosystem (clean-room equivalents) (Weeks 30–40)
- [~] Export/publish pipelines (replaces CC Libraries): the **export** half is shipped and wired (queued `bc-core` export pipeline, JPEG/PNG/WebP, jobs UI, per-file progress, cancellation). The **publish** half (`bc-core::publish`) has presets, validation and atomic output but no preset persistence and no UI, so nothing can reach it; CC-Library-style shared library sync is not designed yet
- [~] Version control / version stacks for assets: `bc-core::version_stacks` (create/list/remove/promote, JSON persistence) is implemented and tested, but no UI or command creates a stack; the only would-be consumer (`photocraft::register_export`) is itself unwired
- [ ] Migration importer (reads existing Bridge caches read-only where formats are documented)
- [~] **Plugin system for BridgeCraft** (successor to the Phase 4 scripting API; Bridges-style extension model)
  - [x] Plugin manifest + loader: local plugins discovered from a plugins directory; manifest declares name, version, entry points, and capabilities (menu items, panel, context-menu hooks)
  - [x] Plugin API surface (Rust trait + WASM host boundary): selection/current-folder access and permissioned metadata, thumbnail, rename, copy and move operations are implemented
  - [x] Sandboxing: WASM plugins run in wasmi without WASI/ambient imports, with fuel and memory limits; explicit grants are default-deny and network calls are not provided
  - [~] UI extension points: toolbar/menu actions, context-menu actions and batch hooks (selection of 2+ files) are wired and runnable; declared plugin panels are still a text placeholder ("Plugin panel rendering is not available in v1") and batch hooks are not invoked by the export or watched-folder pipelines, only by a right-click
  - [x] Plugin manager dialog: list, enable/disable, requested capability/path review, and persisted grants
  - *Relationship to Phase 4: the Phase 4 WASM scripting API is the automation surface; this is the packaged, distributable plugin layer built on top of the same runtime and permissioned host boundary.*
- [ ] **ArtCraft bridge integration** (planned first-party integration targeting [storytold/artcraft](https://github.com/storytold/artcraft))
  - [x] Licensing and shape verified: ArtCraft is a Rust, MIT/Apache-2.0 dual-licensed "IDE for artists" (AI image/video generation with 2D/3D compositing); the permissive license allows direct integration, subject to respecting upstream notices
  - [ ] Not implemented. There is no ArtCraft code in the workspace: no discovery (`ARTCRAFT_PATH`), no "Edit in ArtCraft" action, no hand-off bundle, no version-stack registration helper. Earlier roadmap entries described this work as in progress; that was wrong and they have been reset (truth pass, 2026-11-15). PhotoCraft and LightCraft below are the shipped precedents to copy
  - [ ] Upstream blocker to confirm first: the inspected ArtCraft desktop entry point declares no CLI arguments, so automatic file-open needs an upstream interface before a round trip is possible
  - [ ] Implement as the first consumer of the plugin system above to prove the API surface. Launching host applications is intentionally not exposed to sandboxed plugins, so the bridge starts as a native action and moves behind the plugin API only when a host-launch capability is designed
- [~] **PhotoCraft desktop integration** (separate from the ArtCraft roadmap item; targets the installed [storytold/photocraft](https://github.com/storytold/photocraft) app)
  - [x] Discover PhotoCraft using `PHOTOCRAFT_PATH`, standard install locations, PATH, and Linux desktop-entry AppImage locations
  - [x] Add an "Edit in PhotoCraft" context-menu action that opens selected files using PhotoCraft's documented desktop CLI
  - [~] Hand-off and stack helpers exist in core and are tested (`prepare_handoff` builds an XMP-aware bundle with a JSON manifest; `register_export` adds exports to a version stack), and `send_selection` combines both. The shipped context-menu action calls only `edit`, which opens the original files: no UI path produces a hand-off bundle or registers an export yet
  - [ ] Settings persistence: the executable chosen in the PhotoCraft settings window lives for the session only; a manual pick is not written to the config directory and re-discovery runs at next launch
- [~] **LightCraft desktop integration** (first-party integration targeting [storytold/lightcraft](https://github.com/storytold/lightcraft); this is distinct from a sandboxed WASM plugin)
  - [x] LightCraft is a permissively licensed, pure-Rust non-destructive photo library and RAW developer, available on macOS, Windows, Linux, and WASM
  - [x] Native "Edit in LightCraft" context-menu action passes selected files to LightCraft's documented desktop CLI, which imports them into its library
  - [~] Discovery supports `LIGHTCRAFT_PATH`, standard install locations, PATH, Linux desktop-entry AppImages, and a settings window for manual configuration
  - [ ] Round-trip export tracking, automatic registration of exports as version-stack entries, and transfer of develop settings/ratings/labels
  - [ ] Settings persistence: a manually configured LightCraft executable is session-only, like PhotoCraft
  - [ ] Evaluate reuse of LightCraft's Rust RAW decoding through a documented, compatible library boundary; current BridgeCraft decoding remains unchanged

## Phase 6 — Polish, Packaging & 1:1 Parity (Weeks 36–52)
- [ ] Move the thumbnail cache and per-app state out of the current working directory: `main.rs` opens `.bridgecraft-thumbs.db` beside the CWD, which is not writable for a bundled `.app`, a per-user Windows install, or an AppImage run from a read-only location. Fold this into a single `bc-core::paths` data/config/cache helper (favorites, recents, trash, plugins, stacks, auto-rules, metadata index and the thumbnail cache each derive their own path today, including two divergent copies of the platform env-var logic)
- [ ] Keyboard shortcut parity + custom shortcut editor. Current coverage is the viewer and grid basics only: Ctrl/Cmd+A, F5, Ctrl/Cmd+O, Enter (path field / open preview from list rows), Esc, ←/→, +, -, 0, 1 and Shift+arrows. Missing: Delete/Backspace, F2 or Enter to rename, arrow navigation and Enter-to-open in the thumbnail grid, Tab focus traversal, Ctrl+F to focus the filter, Home/End, Ctrl+Z
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
| M1 — Browser MVP | Q1 2027 | Browse, sort, filter, live refresh, thumbnails, basic file ops | ◐ shipped including internal drag & drop; remaining: grid failure states, folder-open error surfacing, outbound OS drag |
| M2 — Preview Engine | Q2 2027 | RAW/PSD/PDF/video previews with caching | ✅ PNG/JPEG/GIF/WebP/TIFF/BMP + video/PDF/camera RAW (runtime libs) + PSD/AI + SQLite cache + preview/1:1 zoom viewer |
| M3 — Metadata Complete | Q3 2027 | XMP/EXIF/IPTC read-write, smart collections | ◐ EXIF read/write, IPTC-IIM (JPEG APP13), XMP sidecars, the editable panel, the bulk editor and the rating/label/flag/keyword editor with sidecar storage are shipped; embedded XMP, IPTC-in-TIFF and the smart-collection UI remain |
| M4 — Automation | Q4 2027 | Batch ops, scripting API | ◐ batch rename, export pipeline + jobs UI, contact-sheet core, watched-folder automation and the WASM sandbox done; scripting and contact-sheet UI remain |
| M5 — Integrations | 2028 | Plugin system, ArtCraft bridge plugin, PhotoCraft and LightCraft desktop integrations | ◐ plugin system, plugin manager and PhotoCraft/LightCraft launch actions shipped; ArtCraft not started, plugin panels and hand-off wiring remain |
| M6 — 1:1 Parity Audit & Release | 2028 | Feature checklist parity vs. Bridge 2026 user guide; universal macOS build, Windows portable+MSI, AppImage+.zsync published (unsigned) | not started |

## Critical Path
With ADR-0001 resolved (egui/eframe), the Phase 1 shell shipped (filter, live refresh, virtualized grid, list/details views, file operations, recents, trash and internal drag & drop), and M3's editor half is done: rating, label, flag and keyword editing with sidecar storage, bulk stamping and column display. M1's remaining work is the two surfacing defects above plus outbound OS drag. The next critical path is **embedded metadata**: the XMP packet in JPEG APP1 and TIFF tag 700, the TIFF IPTC container, then the sidecar > embedded XMP > IPTC > EXIF precedence from `docs/design/0005`, which is what unblocks **metadata copy on export** (`export.rs` refuses it until the writers land). In parallel: **a search field and smart-collection browsing** over the index that already exists, then **contact sheets, publish presets, metadata templates and version stacks** wired into the UI. Phase 6 packaging should start with the data-path cleanup, because bundle layouts break the current CWD-based thumbnail cache.

## Tech debt

- **Two metadata search/index implementations.** `bc-metadata::search` projects a `Metadata` into a flat searchable record, while `bc-core::metadata_index` owns a SQLite index with its own query builder and smart-collection table. `bc-core::metadata_index` is the one auto-processing writes to. Pick one home for the query model before the smart-collection UI lands.
- **Duplicate query spellings kept on purpose.** `bc-core::metadata_index` still accepts several field and operator spellings (`Field::Make`/`Field::CameraMake`, `Operator::Eq`/`Operator::Equals`, and `Query`'s `predicates`/`conditions`/`clauses`/`and` fields), normalized by `canonical_predicate`. They are kept because they are serialized into stored smart-collection rows; collapse them in the same change that designs the collection UI, with a migration for anything already on disk.
- **Synonym API removed 2026-11-15.** Duplicate public aliases (`bc_core::search`/`index`/`metadata_search`/`versions` module re-exports, `bc_metadata::extract`/`projection`/`searchable`, `PublishPipeline`/`PublishJob`/`run`/`run_pipeline`, `ScriptEngine`/`AutomationHost`/`ScriptRunner`, the contact-sheet `generate_*`/`create_*`/`build_*` set, and the metadata-index `add`/`insert`/`upsert_file`/`put`/`scan*`/`lookup`/`cleanup` family) were deleted: they had no callers, no behavior, and inflated the apparent API surface of modules that are in fact unwired.
- **Four spellings of the index action.** `bc-core::auto_process::AutoAction` has `ExtractMetadata`, `IndexMetadata`, `UpdateSearchIndex` and `IndexIntoMetadataIndex`, all handled by one arm that calls `MetadataIndex::index_path`. They are serialized into users' `auto-rules.json`, so collapse them with a read-side alias for old files, not a silent rename.
- **Emoji as UI glyphs.** The folder marker in the grid and in the list/details rows is a literal `📁` drawn with the proportional font (`bc-ui::app::cell`, `content_list`), which renders inconsistently across platforms and cannot be themed or recolored. The new rating stars are drawn shapes for exactly that reason; the folder marker is the remaining one.
- **Tags live only in the sidecar.** Rating, label, flag and keywords are written to `.xmp` beside the original for every format, because they have no EXIF equivalent and embedded XMP writing is still open. Consequences to settle when it lands: JPEG and TIFF should carry the packet in place (matching `docs/design/0005` §4), the export pipeline should copy it (it currently drops all metadata), and sorting or filtering by rating/label should read the search index rather than stat and parse one sidecar per row per frame, which is what the grid and details overlays do today.
- **No Bridge shortcut layer for tags.** Setting a rating or label from the keyboard today means Tab to the control and press Enter or Space. Bridge's `Ctrl+1`..`5` shortcuts are widely documented behavior, but the exact modifier chords across platforms are not something I have a source for, so they are left to the Phase 6 shortcut parity audit rather than guessed at here.

## Truth pass log (2026-11-15)

### Drag & drop (slice 1, shipped same day)

Internal drag and inbound desktop drops landed on grid cells and list/details rows, on top of
`bc-core::file_ops::transfer_into_folder`. Two defects were found while wiring it, both caught by
tests that drive the real widgets with synthetic egui pointer events and then check the filesystem:

- A drag-only interaction layer over each cell made egui treat **every press as a drag start**, so a
  plain click on a folder also arrived as a drop onto that same folder. The cell now owns one
  click-and-drag response, so egui decides click-versus-drag only after the pointer actually moves.
- Using `Ui::allocate_ui_with_layout`'s outer response as the drop target silently missed the first
  cell of every row: it never reported the pointer inside it, so that folder could not be dropped
  onto. Fixed by keying the drop target to the cell's own response.

### Ratings, labels, flags and keywords (slice 2, shipped 2026-11-15)

The editor issue #10 left unbuilt is in the metadata panel: drawn stars, the label
chips, Approve/Reject/No flag, and keyword chips with an add field and completions.
The same four properties are stampable per-property across a selection.

- Storage: the XMP sidecar only. Rating, label, flag and keywords have no EXIF
  equivalent, so writing them into JPEG/TIFF in place would have meant inventing a
  location; the image bytes are asserted unchanged in tests, and the packet is
  checked for the standard wrapper and property names so the claim "interoperable"
  is not just "our reader agrees with itself".
- Two defects fixed on the way: `merge_sidecar` filtered every namespace except EXIF
  and TIFF, so `dc:` and `xmp:` values never reached the panel; and `Enter` inside a
  `TextEdit` releases focus in egui, which is why the keyboard test walks with Space.
- Verified by driving the real panel with synthetic keyboard events: Tab plus Enter
  sets a rating, and walking on to Save writes the sidecar. The label chip colors are
  pinned and tested against WCAG's 3:1 non-text floor on the dark panel (lowest 4.71:1).
- Left open, deliberately: cross-library keyword suggestions and tag-count trees need
  the search index, and sorting by rating or label needs the same index rather than a
  per-row sidecar read.

### Documentation corrected against the code on `main` (no behavior changes)

- ArtCraft integration was described as partly shipped (`[~]`, "helpers implemented"). It is not started; no `artcraft` symbol exists in any source file. Reset to `[ ]`.
- `[x]` marks that described **core-only** work (contact sheets, version stacks, search/smart collections, keywords/ratings UI, scripting UI, publish presets) now say `[~]` and name the missing UI or caller.
- Phase 1 drag & drop now records the orphaned `transfer_into_folder` primitive and the stale `origin/feat/drag-and-drop-ui` branch, so the work is ported rather than rewritten.
- Plugin extension points were under-described: batch hooks are runnable today; panel rendering is the remaining placeholder.
- Milestone table (M1, M3, M4, M5) and the Critical Path no longer contradict the phase checklists above them.
- `docs/PHASE0_CHECKLIST.md`, `docs/PHASE1.md` and the stale claim in `docs/auto-processing.md` ("the search index API is not implemented") updated to match the code. The Phase 0 toolchain item is now closed as a **toolchain pin** (`rust-toolchain.toml`, mirrored in CI); no MSRV is claimed, because none has been verified.

## Under consideration

- **Apple Photos integration** — deferred for now. A first-party, read-only Apple Photos locator was previously explored, but it remains macOS-specific, depends on undocumented Photos library behavior, and is not part of the current cross-platform roadmap. Revisit only if a clean-room approach with documented public APIs proves viable.

## Risks
- RAW format coverage breadth (mitigate: LibRaw bindings)
- GUI performance on huge folders (mitigate: virtualized grid paints only visible rows and requests thumbnails only for visible cells)
- Watcher platform quirks (inotify/FSEvents/ReadDirectoryChangesW edge cases, e.g. network drives) — debounced refresh + manual navigation already degrade gracefully
- Parity scope creep (mitigate: milestone audits, feature freeze discipline)
- Apple Photos integration is macOS-only and schema-dependent (mitigate: read-only access, graceful "library unavailable" degradation, feature-gated build)
- Plugin sandboxing mistakes could expose user files (mitigate: WASM-only v1, deny-by-default capabilities, permission prompts in the plugin manager)
- ArtCraft integration depends on an external project's evolving APIs (mitigate: version the bridge plugin against specific ArtCraft releases; its MIT/Apache licensing keeps the dependency clean)
- LightCraft integration depends on an external project's evolving APIs (mitigate: version the bridge plugin against specific LightCraft releases; its MIT/Apache licensing keeps the dependency clean, and it is young/moving fast so pin early)
- Video thumbnails require a user-installed FFmpeg binary, which is not bundled (mitigate: availability probe cached per process, videos simply skipped when absent, binary stays outside our licence surface)
- PDF thumbnails require a user-provided Pdfium library, which is not bundled (mitigate: same availability probe and graceful skip; Pdfium's licence applies only to the user's copy)
- Camera RAW thumbnails require a user-provided LibRaw library, which is not bundled (mitigate: same availability probe and graceful skip; LIBRAW_PATH override; embedded-preview fast-path keeps it fast; licence applies only to the user's copy)
- Unsigned distribution artifacts will trigger OS trust warnings (macOS Gatekeeper, Windows SmartScreen) that users must dismiss manually (accepted trade-off: no signing certificates are purchased or managed)

---


