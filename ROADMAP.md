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

## Phase 1 — File Browsing Core (Weeks 5–12) — ◐ shell, file operations and internal drag & drop DONE; surfacing defects and OS outbound drag remain
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

## Phase 3 — Metadata Engine (Weeks 15–26) — ◐ read and write complete for XMP/EXIF/IPTC layers; template and smart-collection UI remain
- [x] EXIF reading in `bc-metadata`: pure-Rust TIFF/EXIF parser (JPEG `APP1` + TIFF/DNG containers), both byte orders, IFD0 + Exif + GPS sub-IFDs + thumbnail-IFD reference; IFD loops and absurd counts are `Malformed`, malformed input never panics; hand-built fixture tests plus a real-camera check gated behind `RAW_SAMPLE` (issue #6)
- [x] Metadata panel: Camera Data (EXIF) section for the selected file — camera, taken, exposure, aperture, ISO, focal length, orientation, color space, software, EXIF size, GPS — above File Properties; cached per path + modified time
- [~] Embedded metadata read/write: EXIF read (issue #5 model) and EXIF write for JPEG/TIFF (issue #7) landed; IPTC-IIM read/write for JPEG APP13 (issue #8); XMP sidecar read/write (issue #9). **Reading is complete** (`bc-metadata::embedded`): the XMP packet in JPEG `APP1` and TIFF tag 700, the Photoshop resource block in TIFF tag 34377, and a raw IIM dataset in TIFF tag 33723. **Writing**: the XMP packet now goes into JPEG and TIFF in place, with an existing sidecar updated in the same save and written first (design/0005 §4, signed off). Still open: PNG (needs an `iTXt` writer, sidecar in the meantime) and writing IPTC-IIM back into TIFF containers
- [x] Tags keep both layers honest: `save_tags` writes the sidecar before the file, so the layer that wins on read can never hide a change the user just made. Covered by `a_jpeg_keeps_tags_in_the_file_and_does_not_grow_a_sidecar` and `an_existing_sidecar_is_updated_so_it_cannot_hide_the_change`
- [x] Panel read precedence implemented as `docs/design/0005-structured-metadata-model.md` §3 specifies: EXIF, then IPTC-IIM, then embedded XMP, then `.xmp` sidecar, merging property by property so a higher layer that lacks a field does not erase the lower layer's value. The plugin host now uses the same chain, so a script and the panel cannot disagree about what a file says
- [x] Write policy signed off (2026-11-15): tags go in place for JPEG/TIFF, sidecar for RAW and PNG, and an existing sidecar is synced, written first. Recorded in `docs/design/0005-structured-metadata-model.md` §4 with an implementation status table, and §3's read precedence is now implemented in the panel chain
- [x] XMP sidecar read/write (`.xmp` files) — `bc-metadata::xmp` (parse/serialize + atomic file I/O)
- [x] Metadata panel with editable fields: grouped list, dirty tracking, Save/Revert via the #5 write policy (`bc-ui::metadata_edit`), multi-selection bulk editor with per-file failure reporting. Sidecar properties now merge in full, so `dc:` and `xmp:` values written by this app or another tool reach the panel instead of being filtered out
- [~] Custom metadata templates: `bc-metadata::templates` has the model (Replace/Merge/Append), JSON persistence (`<data>/BridgeCraft/metadata_templates`) and tests, but nothing calls it: no save/apply UI and no path that writes a template onto a file
- [x] Keyword hierarchy management & tagging: `bc-metadata::keywords` (dc:subject `Parent|Child` convention, tree/type-ahead/tag-count logic) plus a panel editor in `bc-ui::tag_widgets`: chips that remove on click, an add field, and completions drawn from the hierarchy the file already carries. Bulk add and remove across a selection. Cross-library suggestions wait on the index UI
- [x] Star ratings, color labels, approve/reject flags: `bc-metadata::ratings` (XMP `xmp:Rating`, `photoshop:Label`, BridgeCraft `Flag`) plus the panel editor: five drawn stars, the label chips, Approve/Reject/No flag. Clicking the current rating clears it; each property is stampable on its own in the bulk editor, so "set the rating, leave the labels alone" is explicit. Tags write to the XMP sidecar, never into the image bytes, because there is no EXIF equivalent and embedded XMP is still open
- [x] Rating and label display: label dot on the grid thumbnail, dot on list rows, and Rating plus Label columns in the details view, with the header sharing the row column math
- [~] Metadata-driven search & smart collections: the query builder and SQLite index exist (`bc-core::metadata_index`, `bc-metadata::search`) and one auto-processing action writes to the index, but there is no search field, no saved-query UI, and no way to browse a smart collection. Two parallel search/projection implementations exist (see "Tech debt").

## Phase 4 — Batch Operations & Automation (Weeks 24–34) — ◐ rename, export and watched folders shipped with UI; contact sheets and the scripting surface are core-only
- [x] Batch rename (token-based patterns) — `bc-core::batch_rename` parser/planner/journaled apply/undo plus `bc-ui` preview/apply dialog, pattern validation, and undo action
- [x] Batch resize/convert/export (issue #15: queued `bc-core` export pipeline + jobs UI; JPEG/PNG/WebP, orientation applied). JPEG exports also carry the asset's rating, colour label, flag and keywords as an embedded XMP packet; camera/exposure tags are deliberately not copied, and PNG/WebP carry no tags until those containers get a writer. The dialog states which, per format, before the job starts
- [~] Contact sheet / PDF contact sheet output: `bc-core::contact_sheet` renders RGBA and paginated PDF sheets and is tested, but no UI or export path calls it. Remaining: a "Contact Sheet…" action plus layout picker
- [~] Watched-folder auto-processing: persisted rules, settle gate, bounded worker queue, preferences/history UI and docs landed. The `IndexIntoMetadataIndex` action exists in core but is not selectable in the rules UI (Thumbnail / Rename / Export PNG / Dated folder only)
- [~] Scripting/automation API (WASM): local manifest-based scripts use the shared `wasmi` sandbox, fuel/memory limits, default-deny persisted capability and filesystem grants, and permissioned selection/current-folder/metadata/thumbnail/file-operation host imports; per-script failures are isolated. **Runtime is done, the surface is not**: `bc-core::scripting::ScriptHost` has no UI (no scripts window), so scripts can only be discovered from the scripts directory; the plugin path below is what users can actually drive today

## Phase 5 — Integrations & Plugin Ecosystem (clean-room equivalents) (Weeks 30–40) — ◐ plugin layer, manager and desktop launches shipped; panels, publish, stacks and ArtCraft are not wired
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
- [~] Move the thumbnail cache out of the current working directory: `main.rs` opens `.bridgecraft-thumbs.db` beside the CWD, which is not writable for a bundled `.app`, a per-user Windows install, or an AppImage run from a read-only location. The single `bc-core::paths` helper this item called for now exists (`paths::user_data_root`/`user_config_root`/`data_file`/`config_file`), the two divergent copies of the platform env-var logic are gone, and the metadata index moved from the config directory to the data directory with a one-time move of any existing database. Remaining: `bc-ui/src/main.rs` still derives that path from the CWD, and `AutoProcessor::open_default`'s fallback for a missing path points at `thumbnails.sqlite` in the *config* directory, so the two resolve to the same file only while `current_dir()` succeeds
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
| M3 — Metadata Complete | Q3 2027 | XMP/EXIF/IPTC read-write, smart collections | ◐ EXIF read/write, IPTC-IIM, XMP sidecars, the editable panel, the tag editor, and embedded XMP read **and write** for JPEG/TIFF with the documented precedence are shipped; PNG in-place XMP, IPTC-in-TIFF writing and the smart-collection UI remain |
| M4 — Automation | Q4 2027 | Batch ops, scripting API | ◐ batch rename, export pipeline + jobs UI, contact-sheet core, watched-folder automation and the WASM sandbox done; scripting and contact-sheet UI remain |
| M5 — Integrations | 2028 | Plugin system, ArtCraft bridge plugin, PhotoCraft and LightCraft desktop integrations | ◐ plugin system, plugin manager and PhotoCraft/LightCraft launch actions shipped; ArtCraft not started, plugin panels and hand-off wiring remain |
| M6 — 1:1 Parity Audit & Release | 2028 | Feature checklist parity vs. Bridge 2026 user guide; universal macOS build, Windows portable+MSI, AppImage+.zsync published (unsigned) | not started |

## Critical Path
With ADR-0001 resolved (egui/eframe), the Phase 1 shell shipped (filter, live refresh, virtualized grid, list/details views, file operations, recents, trash and internal drag & drop), and M3's metadata work is largely done: rating, label, flag and keyword editing with bulk stamping and column display, embedded XMP and IPTC reading across JPEG and TIFF, the documented read precedence, and in-place tag writing with sidecar sync.

What is left on the critical path, in the order that unblocks the most:

1. **Index-backed tags.** The grid and details overlays stat and parse a sidecar plus a packet for every visible row each frame; that belongs in `bc-core::metadata_index`, which is the same work needed to sort or filter by rating and label. Export copying now reads through `bc_metadata::layers`, so the index can replace that per-row cost in one place.
2. **A search field and smart-collection browsing** over that index, and one search implementation instead of two.
3. **Contact sheets, publish presets, metadata templates and version stacks** wired into the UI: all implemented, tested and unreachable.
4. **The data-path cleanup** before Phase 6 packaging, because bundle layouts break the current CWD-based thumbnail cache.
5. **Tags for PNG and WebP exports, and IPTC-IIM written back into TIFF.** Each needs a container writer that does not exist yet; until then the export dialog says plainly that these formats carry no tags.

M1's remaining items are the two surfacing defects (failed-versus-pending thumbnails, folder-open errors only in the status bar) plus outbound OS drag, which needs an eframe upgrade.

## Tech debt

- **Nothing reads the index.** `bc-core::metadata_index` is fully implemented and tested, but its only writer is the watched-folder ingest action and no code path queries it: no search field, no smart-collection browsing, no tag display. So the index-backed work is not a wiring change on top of the panel, it is the missing indexer itself, and until something populates and reads it the grid and details overlays keep reading each file's layers directly. Bumping `EXTRACTOR_VERSION` makes the rows the ingest action already wrote correct on the next pass, which matters only once there is a pass.
- **Two metadata search/index implementations.** `bc-metadata::search` projects a `Metadata` into a flat searchable record, while `bc-core::metadata_index` owns a SQLite index with its own query builder and smart-collection table. `bc-core::metadata_index` is the one auto-processing writes to. Pick one home for the query model before the smart-collection UI lands.
- **Duplicate query spellings kept on purpose.** `bc-core::metadata_index` still accepts several field and operator spellings (`Field::Make`/`Field::CameraMake`, `Operator::Eq`/`Operator::Equals`, and `Query`'s `predicates`/`conditions`/`clauses`/`and` fields), normalized by `canonical_predicate`. They are kept because they are serialized into stored smart-collection rows; collapse them in the same change that designs the collection UI, with a migration for anything already on disk.
- **Synonym API removed 2026-11-15.** Duplicate public aliases (`bc_core::search`/`index`/`metadata_search`/`versions` module re-exports, `bc_metadata::extract`/`projection`/`searchable`, `PublishPipeline`/`PublishJob`/`run`/`run_pipeline`, `ScriptEngine`/`AutomationHost`/`ScriptRunner`, the contact-sheet `generate_*`/`create_*`/`build_*` set, and the metadata-index `add`/`insert`/`upsert_file`/`put`/`scan*`/`lookup`/`cleanup` family) were deleted: they had no callers, no behavior, and inflated the apparent API surface of modules that are in fact unwired.
- **Four spellings of the index action.** `bc-core::auto_process::AutoAction` has `ExtractMetadata`, `IndexMetadata`, `UpdateSearchIndex` and `IndexIntoMetadataIndex`, all handled by one arm that calls `MetadataIndex::index_path`. They are serialized into users' `auto-rules.json`, so collapse them with a read-side alias for old files, not a silent rename.
- **Emoji as UI glyphs.** The folder marker in the grid and in the list/details rows is a literal `📁` drawn with the proportional font (`bc-ui::app::cell`, `content_list`), which renders inconsistently across platforms and cannot be themed or recolored. The new rating stars are drawn shapes for exactly that reason; the folder marker is the remaining one.
- **Two JPEG marker walks.** `bc-metadata::embedded::for_each_jpeg_segment` and `bc-metadata::exif_write::find_app1` both walk the marker list, with the same rules for padding and standalone markers. `is_tiff` is already shared so the two cannot disagree about what a TIFF is; the marker walk should be unified the same way, because a segment both code paths skip silently would drop or duplicate an APP1.
- **Third-party format checks are opt-in.** `XMP_SAMPLE` and `RAW_SAMPLE` gate real-file checks (see the tests). They are the only evidence that the readers agree with what other applications write, and neither runs in CI: CI has no sample file to point them at. Publishing a CC0 sample to the repository would make them normal tests instead of local ones.
- **Where tags live, and the cost of keeping two layers honest.** Rating, label, flag and keywords are written into JPEG and TIFF files as an embedded XMP packet, and to the sidecar for RAW and PNG. When a sidecar exists it is updated in the same save, *first*, because reading prioritizes the sidecar above the packet and a stale sidecar would silently hide the user's change. That is two copies of the same four properties on purpose (decided 2026-11-15), so any new tag property has to be written and read on both layers, and `save_tags` is the only place that should do it. Two things still follow from it: the export pipeline drops all metadata (it currently refuses to copy anything), and sorting or filtering by rating/label should read the search index rather than stat and parse one sidecar plus one packet per visible row per frame, which is what the grid and details overlays do today.
- **Unreadable packets are refused, so a file can become untaggable in place.** `existing_packet` refuses a JPEG or TIFF whose packet cannot be turned into properties, for two distinct reasons, each with its own test: the XML is broken (an undeclared namespace prefix, which `parse_xmp` reports as `MalformedPacket`), or it is well-formed but nothing in it is in a namespace we understand (parses to empty, refused by the guard added in slice 4). Refusing is right, because merging into an empty result and writing back would erase the other application's properties. The consequence for the user: with a sidecar present the save still lands and the panel shows the new value (`a_refused_packet_leaves_the_sidecar_updated_and_reports_it`), and either way the refusal reason reaches the status line verbatim through `Save failed: {error}`. What is not handled is the read side of the same file: a packet we cannot parse makes the item look simply *unrated*, with nothing saying why. The proper fix is a fallback that keeps the unrecognized bytes verbatim and merges around them, not a relaxed guard; short of that, the tags row should say the packet exists but is unreadable instead of showing zero stars.
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

### Embedded metadata reading (slice 3, shipped 2026-11-15)

`bc-metadata::embedded` locates packets that other applications put inside the
file, and the panel now merges all four layers in the priority order
`docs/design/0005-structured-metadata-model.md` §3 already specified:

- XMP packet in a JPEG `APP1` (`http://ns.adobe.com/xap/1.0/` signature) and in
  TIFF/DNG tag 700, with the padding real writers add after `xpacket end` trimmed.
- IPTC-IIM from the Photoshop resource block in a JPEG `APP13` **or** TIFF tag
  34377, and from a raw IIM dataset in TIFF tag 33723. `iptc.rs` previously
  claimed TIFF was "deferred" and its file helper assumed any file was JPEG.
- The `.xmp` sidecar still wins last, property by property, so a sidecar that
  carries only a rating does not erase an EXIF `Make` the file itself has.

Fixed on the way: the plugin host read metadata with `exif.or_else(iptc).or_else(xmp)`,
which only fell through when a reader *errored*, so a JPEG with EXIF hid its
sidecar tags from scripts. It now uses the panel's chain, and a test asserts the
two agree. `read_panel_metadata` also stopped reading the file twice.

Verification: unit fixtures for each container form, malformed-input tests for
each (oversized IFD counts, a segment length past the end, lost marker sync, a
value claiming 4 GiB), and a gated real-file check (`XMP_SAMPLE`) run against a
JPEG written by **Pillow**, which the panel reports as `stars=4 label="Blue"
keywords=["ThirdParty", "Clients|ACME"]` with no sidecar present.

Not done, and it needs your decision rather than mine: **writing** embedded XMP
and the TIFF IPTC block. §4 of the design doc is still unsigned off, and moving
tag edits from the sidecar into the file changes what happens to the original on
every save.

### Embedded metadata writing (slice 4, shipped 2026-11-15)

Storage policy signed off, so tags now go where design/0005 §4 said they should:

- `bc-metadata::xmp::write_embedded_xmp` writes the packet into a JPEG `APP1`
  (replacing an existing one in place, or inserting after the EXIF `APP1`) and
  into TIFF tag 700 through `exif_write::write_tiff_bytes_tag`, so the TIFF
  rewrite path is shared with the EXIF writer rather than duplicated.
- **Merge, never replace.** The existing packet is read first and only the
  changed properties are applied, so setting a rating does not delete another
  application's keywords or namespaces. Verified by a test that rewrites a
  packet carrying `dc:subject` and a foreign namespace and asserts both survive.
- **An unreadable packet is refused, not overwritten.** A packet that parses to
  zero properties returns `MalformedPacket`. Without that guard the merge starts
  from empty and the write destroys everything the reader did not understand,
  which is the worst possible failure mode for an asset browser.
- `save_tags` writes the **sidecar first**, then the file, because reading
  prioritizes the sidecar: writing the file first and failing on the sidecar
  would leave the higher-priority layer stale and hide the user's own change.
- The bulk tag stamp now merges onto each file and calls the same `save_tags`,
  instead of maintaining its own sidecar-only path. A rating-only stamp on a file
  with keywords keeps the keywords, which that unification is what makes testable.

Verified against a real third-party file. A JPEG produced by **Pillow** carrying
a valid XMP packet is read as 4 properties; after `write_embedded_xmp` sets the
rating to 5, **Pillow reads the packet back** showing rating 5 with the original
label, both keywords and its own `CreatorTool` intact, `x:xmptk` now
"BridgeCraft", and the pixels still decoding. That is the only interoperability
evidence in this project that does not rely on our own parser.

The check also caught a bad fixture of mine: an RDF namespace written as
`1999-02-22-rdf-syntax-ns#` instead of `1999/02/22-rdf-syntax-ns#` parses to zero
properties, and the guard above correctly **refused** to write the file rather
than replacing a packet it could not understand. The fix was the fixture, not the
guard: a packet whose structure we cannot read must not be overwritten with a
partial one, however the file got that way.

Still open from §4: PNG in-place XMP (needs an `iTXt` writer; sidecar in the
meantime, asserted by a test) and writing IPTC-IIM into TIFF.

### Metadata copy on export (slice 5, shipped 2026-11-15)

- The layer chain moved out of `bc-ui` into `bc_metadata::layers`, because the
  export pipeline needed exactly the same rule and a second implementation would
  have meant a file could mean one thing in the panel and another in an export.
  `read_panel_metadata` is now a call, not a copy of the chain, and the two
  duplicate namespace constants went with it.
- JPEG exports carry rating, colour label, flag and keywords as an embedded XMP
  packet. Camera and tool tags are deliberately **not** copied: an export is a
  downscaled re-encode, so repeating `Make`/`Model`/`Software`/`CreatorTool`
  would assert a provenance the output does not have. `capture_data_is_not_claimed_as_the_export_s_provenance`
  pins that decision rather than leaving it in a comment.
- PNG and WebP gain nothing at all, including no sidecar, because design/0005
  section 4 forbids surprise files appearing beside output. The dialog now says
  which behaviour applies, per format, before the job starts.
- Reading the source's tags can never fail an export: unreadable metadata costs
  the tags, not the picture.
- **A wiring gap in my own testing, caught by checking rather than assuming.**
  The first six tests called `carry_tags` directly, so deleting the call inside
  `prepare_export` left all 105 tests green. `the_job_pipeline_itself_carries_tags_into_a_jpeg_export`
  now runs submit -> worker -> publish and fails, naming the exported file, when
  the wiring is removed. Unit tests of a helper prove the helper; only a
  pipeline test proves the pipeline.

### A latent index bug that my own slice 4 activated (2026-11-15)

`metadata_index::read_metadata_for_path` was a fourth hand-written copy of the
layer order, carrying the comment "Embedded XMP not extracted here; sidecar
covers the common case." That was defensible while tags only ever went to a
sidecar. Slice 4 moved tag writes **into** JPEG and TIFF files, so the sentence
became false and the index began reporting every tagged photo as unrated, with
anything built on it (sorting, filtering, smart collections) wrong about the
common case.

Found while planning index-backed tags, not from a failure: a test written to
check the indexer saw in-place tags failed with `left: 0, right: 4`.

Fixed by deleting the fourth copy and calling `bc_metadata::layers::read_file`,
plus `EXTRACTOR_VERSION` 1 to 2 so `is_current` reports the wrong rows stale on
the next pass.

Honest limit on that fix: the version bump is **not covered by any test**, and
cannot be in-process, because proving it needs a database written by the
previous code. What `an_in_place_tag_edit_makes_the_indexed_row_stale` does
prove is the mechanism the bump relies on, that an in-place tag edit changes the
file fingerprint. Checked by hand: setting `EXTRACTOR_VERSION` back to 1 leaves
all 108 bc-core tests green, which is why the bump is stated as a migration for
rows written by the ingest action, not as something a test guarantees.

Two things this exposed about my own testing, both now closed:

- `merge_xmp` had its own assignment loop instead of calling
  `merge_properties`, so the layer priority lived in two places. Inverting
  `merge_properties` left `the_sidecar_outranks_the_embedded_packet_...` green,
  which is how I found it. They are unified, and that test now fails on the
  inversion, so the rule is pinned.
- The roadmap and backlog implied index-backed browsing was a wiring step away.
  It is not: nothing anywhere reads the index, so the missing piece is an
  indexer, and the docs now say that instead of hinting at plumbing.

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


