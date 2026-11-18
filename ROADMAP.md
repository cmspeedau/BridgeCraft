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
- [~] Virtual folder tree with favorites, recent folders, shared collections: favorites & recents shipped as flat JSON stores (`bc-core::favorites`, `bc-core::recents`) and shown in the sidebar; the `bc-core::virtual_folders` registry has no provider wired into the UI; named collections are built, as the saved searches under Phase 3. What is missing is the *tree*: one sidebar node whose listing is the union of several folders, which is what `virtual_folders` was written for and still has neither a provider nor a panel. Nothing is shared with another machine, and no part of this line is reachable as a hierarchy
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

## Phase 3 — Metadata Engine (Weeks 15–26) — ◐ read and write complete for XMP/EXIF/IPTC layers, saved collections authored through a rule editor, and metadata templates saved and applied from the panel; PNG in-place XMP and IPTC-in-TIFF writing remain
- [x] EXIF reading in `bc-metadata`: pure-Rust TIFF/EXIF parser (JPEG `APP1` + TIFF/DNG containers), both byte orders, IFD0 + Exif + GPS sub-IFDs + thumbnail-IFD reference; IFD loops and absurd counts are `Malformed`, malformed input never panics; hand-built fixture tests plus a real-camera check gated behind `RAW_SAMPLE` (issue #6)
- [x] Metadata panel: Camera Data (EXIF) section for the selected file — camera, taken, exposure, aperture, ISO, focal length, orientation, color space, software, EXIF size, GPS — above File Properties; cached per path + modified time
- [~] Embedded metadata read/write: EXIF read (issue #5 model) and EXIF write for JPEG/TIFF (issue #7) landed; IPTC-IIM read/write for JPEG APP13 (issue #8); XMP sidecar read/write (issue #9). **Reading is complete** (`bc-metadata::embedded`): the XMP packet in JPEG `APP1` and TIFF tag 700, the Photoshop resource block in TIFF tag 34377, and a raw IIM dataset in TIFF tag 33723. **Writing**: the XMP packet now goes into JPEG and TIFF in place, with an existing sidecar updated in the same save and written first (design/0005 §4, signed off). Still open: PNG (needs an `iTXt` writer, sidecar in the meantime) and writing IPTC-IIM back into TIFF containers
- [x] Tags keep both layers honest: `save_tags` writes the sidecar before the file, so the layer that wins on read can never hide a change the user just made. Covered by `a_jpeg_keeps_tags_in_the_file_and_does_not_grow_a_sidecar` and `an_existing_sidecar_is_updated_so_it_cannot_hide_the_change`
- [x] Panel read precedence implemented as `docs/design/0005-structured-metadata-model.md` §3 specifies: EXIF, then IPTC-IIM, then embedded XMP, then `.xmp` sidecar, merging property by property so a higher layer that lacks a field does not erase the lower layer's value. The plugin host now uses the same chain, so a script and the panel cannot disagree about what a file says
- [x] Write policy signed off (2026-11-15): tags go in place for JPEG/TIFF, sidecar for RAW and PNG, and an existing sidecar is synced, written first. Recorded in `docs/design/0005-structured-metadata-model.md` §4 with an implementation status table, and §3's read precedence is now implemented in the panel chain
- [x] XMP sidecar read/write (`.xmp` files) — `bc-metadata::xmp` (parse/serialize + atomic file I/O)
- [x] Metadata panel with editable fields: grouped list, dirty tracking, Save/Revert via the #5 write policy (`bc-ui::metadata_edit`), multi-selection bulk editor with per-file failure reporting. Sidecar properties now merge in full, so `dc:` and `xmp:` values written by this app or another tool reach the panel instead of being filtered out
- [x] Custom metadata templates: a **Templates (N)** disclosure at the bottom of the metadata panel, in both its single-file and bulk forms. It lists the saved templates, shows what the chosen one holds and in which mode, applies it to the file or to the selection, and saves the values currently in the panel — unsaved edits included — under a new name. Applying goes through the two writers the panel already uses (`FieldChange` for the text fields, `TagStamp` for rating/label/flag/keywords), so a template cannot reach a storage policy the user has not already accepted. The rule that keeps it safe: **a template changes only the properties it names** — the engine's `Replace` replaces the whole property map, which for a template holding a copyright line would otherwise mean clearing the description, the artist and the keywords of every file it is pointed at. Capture also names only the fields the bulk editor will stamp, so a template taken from a JPEG cannot carry that camera's Make onto another maker's files; and a file the template leaves unchanged is not written at all, which is what keeps "apply to 400 selected" from creating 400 sidecars for nothing. `bc_core::MetadataTemplates` keeps them in `metadata-templates.json` in the data directory, versioned, written atomically, and never overwritten when it cannot be parsed; the status line distinguishes files updated from files that already matched
- [x] Keyword hierarchy management & tagging: `bc-metadata::keywords` (dc:subject `Parent|Child` convention, tree/type-ahead/tag-count logic) plus a panel editor in `bc-ui::tag_widgets`: chips that remove on click, an add field, and completions drawn from the hierarchy the file already carries. Bulk add and remove across a selection. Completions now come from the whole library rather than from the file alone — see the saved-searches line below
- [x] Star ratings, color labels, approve/reject flags: `bc-metadata::ratings` (XMP `xmp:Rating`, `photoshop:Label`, BridgeCraft `Flag`) plus the panel editor: five drawn stars, the label chips, Approve/Reject/No flag. Clicking the current rating clears it — which means the property leaves the JPEG or TIFF too, not just the panel: an embedded save now states whether it is the whole tag state (`TagWrite::Replace`) or a partial update (`Merge`), because a metadata that omits `xmp:Rating` cannot otherwise tell "cleared" from "not touched", and until that distinction existed a cleared star kept its old value in the file forever. Each property is stampable on its own in the bulk editor, so "set the rating, leave the labels alone" is explicit. Tags write to the XMP sidecar, never into the image bytes, because there is no EXIF equivalent and embedded XMP is still open
- [x] Rating and label display: label dot on the grid thumbnail, dot on list rows, and Rating plus Label columns in the details view, with the header sharing the row column math
- [x] Background metadata indexing: opening a folder queues a pass over its files on the shared job pool (`bc-core::jobs::JobQueue::submit_index` running `bc-core::indexer::index_one`), and a settled watcher event re-runs it. A row that still matches its file is skipped after two `stat`s and one indexed lookup rather than re-read; a pass also records the folder (`add_root`) and prunes rows under it whose file is gone (`prune_absent_from_listing`), clearing their keyword projection rows with them. Progress, per-file states and cancellation are the Jobs window's, the same ones exports use (`bc-ui::app::start_indexing`, an `Indexing 4/7` readout in the status bar, `Index offline` when the database cannot be opened). Verified against the release binary, not only in unit tests: a run over a 6-file folder wrote 6 rows and the folder into `$XDG_DATA_HOME/BridgeCraft/metadata.sqlite`; a second run with one file added left all six existing rows' `indexed_at` untouched and added the seventh; a third run after deleting two files took the table from 7 rows to 5
- [x] Index-backed sorting and filtering: the grid, list and details views can order by rating, colour label or flag (`SortKey::Rating`/`Label`/`Flag` in `bc-core::fs`, fed by `MetadataIndex::tag_values_for` — one indexed range query per folder, not one lookup per file), and the toolbar's Tags menu filters by a rating floor, a label or a flag. A tag sort inherits the direction the user already chose and the button names the end that comes first (`1★`/`5★`, `A-Z`/`Z-A`, `smallest`/`largest`), because `Asc` means nothing useful for a rating. Ties break by name so the order is the same twice in a row. Saving tags writes the row back through `index_saved_tags`, so a starred photo moves immediately instead of sitting in the unrated half of the folder until the next pass. Files the index has not read sort as unrated and are *hidden* by a tag filter, which the status bar and the menu both count — the alternative is a grid that quietly omits photos
- [x] Cross-library keyword suggestions: both keyword boxes — the single file's and the bulk stamp's — ask the index what the library already calls things beginning with what is typed, and answer with up to eight candidates ordered by how many indexed files carry each (`Beach (73246)`). Clicking stores the library's spelling rather than the letters typed, which is the one moment a near-duplicate keyword can still be prevented: after the save there are two `dc:subject` entries and no interface merges them. Typing a trailing `|` asks what hangs under a parent and gets only children, because `keywords::normalize_prefix` keeps the separator that `normalize_keyword` (rightly) drops for a stored value. A word nothing uses is said to be new, an empty branch is said to be empty, and an offline index is reported as unavailable rather than silently as a vocabulary with nothing in it. The single-file box also withholds a keyword the file already carries, because clicking it could only be refused; the bulk box cannot make that claim about a selection, so it offers the vocabulary and the stamp path skips files that already hold the word (`keywords::add_keyword` de-duplicates per file). `MetadataIndex::keyword_suggestions` answers on the shared search worker behind a 150 ms debounce, and only ever shows a list whose sequence matches the outstanding request
- [~] The tag overlay on grid cells still reads files directly: the rating badge on a visible cell keeps its own mtime-checked cache, deliberately — one indexed lookup measured 98 µs per row against 14.9 µs to parse the file, and the cache answers in 2.3 µs. This is the remaining per-file work in this area, and it stays because the measurement says so rather than because nobody got to it
- [x] Library search over the index: a toolbar box (`bc-ui::app::request_search`, `poll_search`, `handle_os_drops`) takes one or more words, matches them against the row's searchable text (`Field::AnyText`, `Query::text` — name, path, keywords, title, description, camera make and model, label, flag), and is scoped to this folder, this folder and subfolders, or the whole library (`bc_core::SearchScope`, applied as a primary-key range so a folder search stays proportional to the folder). Results replace the grid through `BrowserState`'s listing mode, so sorting, the tag filter, thumbnails and selection work on a result exactly as on a folder listing; the status line counts the files the index has never read in scope, because those cannot match however the words are phrased. Runs on its own thread with the existing mpsc pattern: one query is a table scan at ~1.4 ms per 2 000 rows, which is not something to spend per keystroke on the paint thread. Esc leaves the search, navigating away empties the box, and dropping files from the desktop while searching is refused rather than quietly copying into a folder that is not on screen
- [x] Saved searches (collections): a Collections tab in the sidebar lists every stored search in name order, one click runs it into the grid, ✎ opens it in the rule editor and ✕ deletes it. Storage is `bc_core::SavedSearches` over `saved-searches.json` in the data directory — deliberately not the metadata index, which is a cache; a store that will not parse is reported and never overwritten. A running collection keeps the scope it was saved with (`SearchScope` inside the stored document, so a folder search does not silently widen into a library search on reopen), and the box mirrors the collection's words only when the rules really are words. The tab reads the list when it is entered and after a save or delete, not on every redraw. `an_unreadable_saved_search_file_is_reported_and_never_overwritten` covers the damaged store: the panel says the file cannot be read, the save control is disabled with that reason, and the bytes on disk are asserted unchanged afterwards
- [x] A rule editor for collections: "Edit rules…" in the Collections tab opens a dialog that authors the whole stored shape — words, any number of conditions on the eleven indexed fields the editor offers (file name, path, extension, keyword, title, description, camera make and model, colour label, flag, star rating), AND or OR across them, and the scope — and ✎ on a saved collection opens that collection's own rules. It shows a live "N file(s) match" count, debounced like the search box and answered by the same worker thread, so no rule set is ever saved on faith. Each operator menu lists only the tests that field can actually answer: a text field is never offered `>` and a number is never offered "contains", because `eval_predicate` answers those with false rather than with an error — and the same guard, `Predicate::problem`, refuses a star count outside 0–5, a non-number in a number field (the engine reads it as zero), an empty value, a value typed into "is set", and "is set" on a field no file can lack. While any row is refused the dialog counts nothing and says so, because an impossible rule and an empty library look identical in a grid, and it will not save a rule set it has just explained cannot match. Two tests hold the menus against the evaluator rather than against each other: `every_operator_a_field_offers_can_actually_match_a_row` constructs a value per pair and asserts a populated row matches it, and `is_set_is_offered_exactly_where_a_value_can_actually_be_absent` derives `Field::can_be_unset` from `field_exists` on an empty row. The pair that is *not* offered is deliberate: `Size`, `Mtime`, `Width`, `Height` and the two date fields are indexed and queryable but absent from the menus, because a size in bytes and a 
  nanosecond timestamp are not things a user should be asked to type

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
- [x] Move the thumbnail cache out of the current working directory: it is now `thumbnails.sqlite` in the per-user data directory, resolved by one helper (`bc_core::paths::thumbnail_cache_path`) that the browser worker, the watched-folder fallback and `--rebuild-cache` all call — previously `main.rs` derived it from `current_dir()` and `AutoProcessor::open_default`'s fallback pointed at the *config* directory, so the two only agreed while `current_dir()` happened to succeed. `BridgeApp::new` no longer takes a cache path at all: a parameter that a caller could fill with a working-directory guess was the bug, and an unresolvable data directory now simply means no cache, which costs regeneration and nothing else. Verified with the release binary launched from a scratch directory: the launch directory stays empty, the cache appears in the data directory holding its 3 warmed entries, and `--rebuild-cache` with no file argument writes the same file the GUI opens (proved by deleting it, rebuilding, and finding it recreated with 3 rows)
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
| M3 — Metadata Complete | Q3 2027 | XMP/EXIF/IPTC read-write, smart collections | ◐ EXIF read/write, IPTC-IIM, XMP sidecars, the editable panel, the tag editor, and embedded XMP read **and write** for JPEG/TIFF with the documented precedence are shipped, as are saved collections with their rule editor and metadata templates with a save/apply UI; PNG in-place XMP and IPTC-in-TIFF writing remain |
| M4 — Automation | Q4 2027 | Batch ops, scripting API | ◐ batch rename, export pipeline + jobs UI, contact-sheet core, watched-folder automation and the WASM sandbox done; scripting and contact-sheet UI remain |
| M5 — Integrations | 2028 | Plugin system, ArtCraft bridge plugin, PhotoCraft and LightCraft desktop integrations | ◐ plugin system, plugin manager and PhotoCraft/LightCraft launch actions shipped; ArtCraft not started, plugin panels and hand-off wiring remain |
| M6 — 1:1 Parity Audit & Release | 2028 | Feature checklist parity vs. Bridge 2026 user guide; universal macOS build, Windows portable+MSI, AppImage+.zsync published (unsigned) | not started |

## Critical Path
With ADR-0001 resolved (egui/eframe), the Phase 1 shell shipped (filter, live refresh, virtualized grid, list/details views, file operations, recents, trash and internal drag & drop), and M3's metadata work is largely done: rating, label, flag and keyword editing with bulk stamping and column display, embedded XMP and IPTC reading across JPEG and TIFF, the documented read precedence, and in-place tag writing with sidecar sync.

What is left on the critical path, in the order that unblocks the most:

1. **Index-backed tags.** Done for sorting and filtering, which is where the index was the only way to do the job at all: ordering a folder by stars needs every file's rating, and reading every file costs 14.9 µs each against 1.3 µs each for one indexed range query. What this slice measured and then *rejected* was the plan I wrote in the previous commit — "the grid and details overlays stat and parse a sidecar plus a packet for every visible row each frame; that belongs in `metadata_index`". The overlays cache on the file's own mtime, so a frame costs 2.3 µs per visible row and can never show a stale tag, while a per-file index lookup costs 98 µs — 6.6× slower than parsing the file, and permitted to lag it. Per-row display stays as it is. Cross-library keyword suggestions — the other half of that older note — shipped in slice 14; what still needs more than the four tag columns the index holds is the panel's full layer read, which keeps parsing the file so a saved star shows up immediately.
2. ~~A rule editor for saved collections~~ — shipped: `Edit rules…` authors words, conditions, combinator and scope, with a live match count from the worker. What that item also settled was that the remaining authorship gap is *typed ergonomics*, not the query model: the fields the index stores but the editor does not offer (size, dates, dimensions) need unit and date controls, not a new query language.
3. **Contact sheets, publish presets and version stacks** wired into the UI: all implemented, tested and unreachable. Metadata templates came off this list in slice 15; what that taught worth keeping is in the slice log — the safety question was never the merge rules but *which properties a template is allowed to name*, and the answer ("only the ones it holds, and only the bulk-stampable fields") is what made the engine safe to reach.
4. ~~The data-path cleanup~~ — done: the thumbnail cache moved into the data directory and the CWD is no longer written to. Atomic persistence for `favorites.json` and `recents.json` is the remaining sibling of that work, listed under Tech debt rather than here.
5. **Tags for PNG and WebP exports, and IPTC-IIM written back into TIFF.** Each needs a container writer that does not exist yet; until then the export dialog says plainly that these formats carry no tags.

M1's remaining items are the two surfacing defects (failed-versus-pending thumbnails, folder-open errors only in the status bar) plus outbound OS drag, which needs an eframe upgrade.

## Tech debt

- **The index is written and read, for nine columns.** Sorting and the tag filter answer from `rating`, `color_label`, `flag` and `keywords` plus the path; free text adds the stored `title`, `description`, `camera_make` and `camera_model` (see `haystack`), and the rule editor can ask about each of those on its own. Six more columns are stored and queryable by the engine but not offered by the interface — `size`, `mtime_ns`, `width`, `height`, `created`, `modified` — for the reason in the bullet below. What no query can ask, offered or not, is anything the projection drops: shutter speed, ISO, focal length, GPS, a copyright string. Those are not in the row, so a rule about one is unanswerable rather than wrong. Still written-but-unread: `roots()` and `status()`/`IndexStatus`, so a pass records the folder it covered and no interface reports that coverage. (`query_rows()` and `query()` are gone — the first had no callers, the second had only tests, and `search()` is the one reader now.) The grid's rating badge and the metadata panel keep reading files directly, deliberately: see the measurement in Critical Path item 1.
- **A test can assert the in-memory copy and pass while the database is untouched.** The first test for the `root` column checked `index.roots()` — the table the same sweep writes — and stayed green through a release run in which every real row's `root` was `NULL`. Split the claim when two stores are involved, and check the new assertion fails by deleting the line it covers; that is how the ordering flaw below was found at all.
- **`IndexFile::path` is a lossy key, not a name.** Rows are keyed by `to_string_lossy()`, so for a filename whose bytes are not valid UTF-8 (legal on Linux, common in mirrored downloads) the row's own `path` opens no file. Lookups by the real path work, so such a file is read once and then reported current; what does not work is taking a search result's path and acting on it. `prune_absent_from_listing` therefore refuses to delete rows whose key contains the replacement character: two different names can share one lossy key, so the key's absence from disk proves nothing. Measured and pinned by `a_non_utf8_name_is_findable_by_the_path_but_its_row_does_not_name_a_file`. Storing paths as the OS bytes (a BLOB) is the fix, and it needs a schema version bump.
- **Reading one file's tags from the index is slower than reading the file.** Measured on a 2 000-row index: `get` 98 µs per file, `bc_metadata::layers::read_file` 14.9 µs, the overlays' mtime-checked cache 2.3 µs per visible row per frame. Bulk is the only shape worth querying: `tag_values_for` is 1.3 µs per file in one statement. Any proposal to "put the grid onto the index" has to beat those numbers per row and cannot, and it would also give up the freshness that makes a saved star appear at once.
- **Sidecar files are listed and indexed like any other file, and library search returns them.** Confirmed against a database written by the release binary rather than reasoned about: searching `blue` over a folder of four sidecar-tagged JPEGs matched eight rows, because each `.xmp` is a file with its own row carrying the same tags as the photo beside it. The remedy is not a special case in search — it is the pre-existing "Thumbnails only" toggle, which filters whatever the browser is showing, and `thumbnails_only_hides_sidecar_files_from_search_results_too` now pins that it reaches result sets as well as folder listings. What is still undecided is whether a sidecar should ever have had a row: excluding them at index time would make counts and coverage honest for photo libraries, and that is a listing-policy call worth making deliberately rather than by accretion.
- **Staleness depends on a clock that is coarser than an edit.** Measured on this machine: 149 of 400 same-size tag edits reused the previous write's modification timestamp, because the kernel serves a cached timestamp across a stretch of writes. Since `set_rating(0)` *removes* the property rather than writing zero, such an edit changes neither size nor timestamp, and `is_current` then calls the file current forever — the row keeps the old rating, and a rating sort or tag filter serves it. Two things follow, both in force: saving tags re-indexes **explicitly** (`indexer::reindex`) instead of asking, and a folder pass keeps the skip because it cannot afford to re-read every file. The gap that remains is an edit made by *another* application within one timestamp tick of the last indexing, which no pass will notice until the file changes size or the extractor version moves. The durable fix is a content signature on the row, which costs a read to compute and so has to be weighed against simply not skipping.
- **`TagWrite::Replace` and `TagWrite::Merge` are one bit of intent every tag writer must state.** Absence of a tag property in a metadata means either "the user cleared it" or "this save does not touch tags", and the two need opposite results. The panel and export use `Replace`; a partial update uses `Merge`. Nothing can tell them apart from the data, so the parameter is mandatory rather than defaulted — the default is exactly what made clearing silently fail for embedded packets.
- **Persistence is atomic and single-sourced; one store still adopts before it writes.** `favorites.json`, `recents.json`, `saved-searches.json`, the version-stack file and the watched-folder state all write through `bc_core::paths::write_atomic` now (the helper moved out of `auto_process` rather than being copied), and `favorites`/`recents` additionally write the *candidate* list before adopting it, so a save that fails leaves the on-screen list and the file agreeing. Two residuals. `version_stacks` still mutates in memory before persisting at eight call sites — it is atomic, so a crash cannot corrupt the file, but a failed save leaves the session showing a stack that is not on disk; it is listed as deferred not because it is fine but because the feature has no UI at all, so nobody can experience it, and the same argument applied to it eight times is how this file keeps accruing debts.
- **Two path bugs that only surfaced because the rename journal was drawn twice.** `bc-ui::batch_rename_ui::journal_path` had its own copy of the platform rules and an XDG-only one, so a macOS or Windows launch wrote the undo journal somewhere every other store ignores; and the watched-folder action derived the same journal from the *configuration* directory while the dialog derived it from the *data* directory, so a rename made by a rule could not be undone from the dialog and vice versa. Both now resolve through `paths::rename_journal_path`, and `both_rename_journal_writers_resolve_the_same_file` asserts the agreement through `AutoProcessor::open_default` rather than comparing the helper with itself. Five call sites that hand-appended `"BridgeCraft"` (`favorites`, `recents`, `trash`, `plugins`, `photocraft`, `version_stacks`) were moved onto the helpers, and `recents::user_data_dir` — the alias those sites leaned on — is deleted. One consequence of settling the journal on the data directory: an undo journal a previous build left in the configuration directory is no longer read, so a rename that was waiting to be undone across a restart loses that history. Nothing else about it is affected, the file is left where it is for anyone who wants it, and the journal is a transient record of one dialog action rather than user-authored content — which is why no migration was written for it.

- **A sidecar is a file in the listing, so it can be selected like an asset — and a
  template applied to it.** `sidecar_path` is `with_extension("xmp")`, which maps `a.xmp` to
  itself, so a selected sidecar is its own target: the write lands inside the metadata
  document rather than beside a photograph. Nothing crashes and nothing is lost (the same
  properties are written), but a bulk apply over a folder that shows its sidecars reports
  one more file changed than there are images, and the `Thumbnails only` toggle is the only
  thing keeping sidecars out of a selection in the first place. The decision this needs is a
  listing-policy one — whether a sidecar should ever have had a row — and it is the same
  question the indexing note below has been carrying since slice 9, now with a write path
  attached to it.
- **A template apply writes a file twice when it changes both halves.** Text fields go
  through `apply_changes_to_path` and the four tag properties through the stamp path, because
  those are the two writers the panel already had and reusing them was the point. On an
  in-place JPEG that is two reads and two rewrites of the same file per apply. It is correct,
  and it is the slowest thing in the panel; a single-pass writer that takes fields and tags
  together is the fix, and it should be measured before it is attempted for its own sake.

- **Suggestions reach the two keyword boxes, not the rule editor's.** The dialog's keyword
  row takes free text with no completions, because there is one suggestion state and the
  editor can hold several keyword rows at once: deciding *which row* a candidate list
  belongs to needs the row's identity, not another query. The live match count already
  tells an editor user when a spelling finds nothing, which is the same fact in a less
  convenient form, so this is listed rather than fixed here.
- **The first suggestion query pays for the keyword index, once per library.**
  `MetadataIndex::keyword_suggestions` runs `CREATE INDEX IF NOT EXISTS` on a NOCASE
  expression index before it asks anything, because putting that statement in `init`
  would make every open of an existing library pay it — measured at **0.645 s on 735 000
  keyword rows** built from a database the release binary wrote. A no-op re-run costs
  0.04 ms, and the queries themselves cost 15-45 ms depending on how many rows carry the
  prefix (a miss is 0.03 ms), which is why they sit behind a 150 ms debounce on the
  worker thread rather than in the panel draw. What is not done: building the index
  during a background indexing pass, which would move the 0.645 s off the first keystroke
  and is the obvious next step if anyone finds the delay noticeable.
- **A keyword prefix's meaning is decided by which normaliser runs, and the two differ
  by one character.** `keywords::normalize_keyword("Beach|")` is `"Beach"` — correct for
  a value being stored, wrong for a question being asked, because dropping the trailing
  separator turns "what hangs under Beach" into "Beach" and answers it with the parent and
  its siblings. That is exactly what happened before `normalize_prefix` existed, and the
  test that found it (`a_parent_prefix_offers_the_tree_and_a_separator_offers_only_children`)
  was a *core* test: nothing in the interface would have looked wrong, the list simply
  contained answers to a different question. Any future prefix input must use the prefix
  normaliser.
- **Suggestion state is registered by drawing, which the tests must do explicitly.** The
  completion row is what tells the frame which prefix it is asking about, so the query is
  scheduled during a draw and answered by the next tick. That is right for the app (egui
  repaints after every keystroke) but it means a test that sets a draft and ticks without
  drawing sees nothing happen at all — three of this slice's tests failed for exactly that
  reason before they drew the panel once after typing. A unit test cannot treat the state
  machine as reachable by field writes alone.

- **Two lists of colour-label names, and they do not agree.** `bc_metadata::ratings::ColorLabel` has five fixed names (Red, Yellow, Green, Blue, Purple) plus `Custom`, which absorbs anything else; the interface's `metadata_edit::LABEL_CHOICES` offers seven (it adds Orange and Grey) and calls them known (its `is_known_label` asks about drawable chips, not about the enum). Nothing checks the pair, and the mismatch is invisible in practice only because `Custom` is doing the work: choosing "Orange" writes a label the enum has no variant for, round-trips as `Custom("Orange")`, and `is_custom()` then reports true for what the UI presents as a standard label. A rule editor has to sit this out — the index stores whatever string a file carries, so a popup of any one list would offer an incomplete vocabulary and hide the rest of the library — which is why the label row takes free text and the live count is the check on a misspelling. The fix is one list, owned by `bc-metadata`, with the colours living in the UI beside it.
- **Collections can now be authored as rules; two of their inputs are still boxes of raw values.** The editor offers eleven fields and refuses every operator a field cannot answer, which closes the item this file kept deferring. It opens a smaller one: `Size`, `Mtime`, `Width`, `Height`, `Created` and `Modified` stay unoffered because the only control available for them is a text box asking for bytes and for nanoseconds since the epoch, and `Created`/`Modified` are stored as ISO *text*, so "taken after March" would be a string comparison that happens to work only while the format stays ISO-8601 and zero-padded. Date and size controls are the remaining half; the query model is not.
- **Duplicate query spellings removed, not normalised.** `bc-core::metadata_index` used to accept several field and operator spellings (`Field::Make`/`CameraMake`, `Operator::Eq`/`Equals`, and `Query`'s `predicates`/`conditions`/`clauses`/`and`/`or`/`expr`) and reconcile them at match time, with the rule that they had to be kept because they were serialised into stored smart-collection rows. The premise was checked before it was dropped: nothing in the repository has ever called `save_smart_collection`, so no such row exists on any disk, and `matches` was quietly giving different answers depending on which synonym a query happened to use. There is now one predicate container, one combinator, and one spelling per field and operator; the `a_stored_query_round_trips_through_the_shape_that_is_saved` test asserts the serialised form contains none of the old containers, so the shape cannot regrow unnoticed.
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
- Left open, deliberately: tag-count trees still need the search index (the keyword
  table holds every row needed to build one, and `build_tree` already does the maths
  for a folder's worth of keywords), and sorting by rating or label needs the same
  index rather than a per-row sidecar read. Cross-library keyword suggestions — the
  other half of that older note — shipped here: they are the same query, one prefix
  at a time.

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

### Background indexing (slice 7, shipped 2026-11-16)

The first version of this slice was a dedicated thread with a generation
counter and a slot to reap it. Both of its hangs were the same mistake, found
by reading rather than by running: in Rust 2021 the guard created in an
`if let` scrutinee lives for the whole block, so `shutdown()` and
`reap_finished()` each held the worker mutex across `handle.join()`, and the
worker's last line wanted that same mutex. Near-zero CPU time on a
four-minute test is the signature. The rework moved indexing onto the export
pool, which removes the class instead of fixing the instance — there is no
handle to join — and made supersession a `cancel` plus a `submit` on a path
that was already tested.

- **A design bug, proven by putting it back.** `start()` cancelled the running pass and returned *without queueing the new folder*, so navigating during a pass silently dropped the destination. `bc-ui::app::tests::navigating_mid_pass_indexes_the_folder_the_user_ended_up_in` now fails when that behaviour is reinstated, which I checked by reinstating it rather than by trusting the new code.
- **A test premise that was wrong, not flaky.** `one_unreadable_file_does_not_stop_the_pass` asserted a corrupt JPEG would be counted as a failure. Every decoder in `bc-metadata` treats an unparseable packet as "no properties", so indexing it *succeeds* with an empty row. The test now states that (`an_unreadable_file_indexes_as_having_no_tags_rather_than_failing`), and "failed" in this module means an I/O error only.
- **Oversized fixtures deleted.** Cancellation is checked per task now, not once per 256 files, so the 300- and 400-file fixtures proved nothing beyond what 16 files prove; fixture creation was most of the suite's runtime.
- **`prune_missing` and `index_root` deleted.** Neither had a caller outside its own module. `index_root` was a recursive walk that swallowed every per-file error, which is the wrong shape for anything that reports per-file progress. `prune_missing` deleted `files` rows only, orphaning the keyword projection table because the `ON DELETE CASCADE` it declares is inert — SQLite does not enforce foreign keys unless `PRAGMA foreign_keys` is turned on per connection, and it is not. The scoped `prune_absent_from_listing` goes through `delete`, which clears both tables by hand, and a test counts the keyword rows to prove it.
- **A live performance bug, found by measuring a claim I had just written.** A doc comment said a repeat pass "costs one `stat` per file". `is_current` → `get` → `all()` plus a linear search, and `all()` re-prepared a keyword statement for every row: **8.41 s for 200 staleness checks over a 2 000-row index**, about 42 ms per file, which a folder pass pays once per file on every open. `get` is now a point query and keywords are read once and grouped: **17.8 ms** for the same loop, 474× faster. `a_lookup_does_not_cost_a_full_table_scan` holds a bound ~100× above the fixed cost and ~4× below the broken one, so the shape of the regression is caught rather than its constant.
- **One flake, not explained.** `an_in_place_tag_edit_makes_the_indexed_row_stale` failed once, in the first full run after I added a two-connection concurrent-write test. It then passed 60 consecutive isolated runs, 8 full parallel runs and a single-threaded run, so the cause is not established and I am not claiming it is fixed. Its assertion now prints both fingerprints it compared, so a recurrence explains itself instead of being re-run until green.
- **A limitation the tests found rather than I did.** Rows are keyed by `to_string_lossy()`. For a non-UTF-8 filename the lookup still works (both sides lose the same way), but `IndexedFile::path` is the key, not the name, so it opens nothing — recorded in Tech debt. It also decided the sweep's rules: such a row is never pruned, because two names can share one lossy key and the key's absence from disk proves nothing about the file.
- **Verified against the shipped binary, not only in tests.** A release build pointed at a 6-file folder wrote 6 rows and the folder's root; a second run left every existing row's `indexed_at` untouched and added the one new file; a third run after deleting two files took the table from 7 rows to 5.

### Index-backed sorting and filtering (slice 8, shipped 2026-11-17)

The premise for this slice came from me and was wrong: the previous commit's
Critical Path entry said the grid's per-row tag reads "belong in
`bc-core::metadata_index`". Measured, an index lookup for one file costs 98 µs
against 14.9 µs to parse the file, and the existing mtime-checked cache costs
2.3 µs per visible row per frame. Moving the overlays onto the index would have
made the grid slower and allowed it to show tags the user had already changed.
The index is the right tool for a different job — ordering or filtering a whole
folder, where you cannot avoid knowing every file's value — so that is what
shipped, and the entry now records the numbers instead of the intention.

- **A false alarm I raised and retracted.** Running the release binary against
  a library I had generated showed every rating as 0, and I reported a
  significant interop bug in the XMP reader. The reader was fine: **my fixture
  was wrong**. I had retyped the RDF namespace from text rendered on screen,
  and the date separators arrived as hyphens (`1999-02-22`) instead of soliduses
  (`1999/02/22`), so `parse_xmp` correctly returned an empty result for a packet
  in a namespace that does not exist. Rebuilt byte-by-byte, the same run read
  1/3/5/0 with the right labels. Two lessons kept: verify identifiers as bytes,
  and an assertion that a fixture "looks right" is not a test — see the new Tech
  debt entry about a wrong namespace reading as a parser gap.
- **A real ordering bug in the same sweep, found by the retraction.** Rows carry
  the folder they were indexed from; I had that stamped by a range `UPDATE` at
  the end of a pass, so a row written after the sweep kept `NULL` — the release
  run showed three rows stamped and one not. Stamped at write time instead
  (`index_file` uses the containing directory, which is what a non-recursive
  pass covered for every writer including the ingest action), and the back-fill
  function was deleted along with its caller. Rows written by an earlier build
  keep their `NULL` folder until they are indexed again — a row that still
  matches its file is skipped rather than rewritten — which is harmless today
  because nothing in the interface reads the column yet, and it repairs itself
  on the next tag edit or extractor-version bump.
- **The test that hid it.** My first `root` assertion checked the `roots` table,
  which a different line of the same function writes, and stayed green for the
  whole time the column was broken. It now asserts the column too, and I checked
  it fails when the stamping line is removed — the same verification I did for
  supersession in the previous slice.
- **A pre-existing interface bug fixed in passing:** the sort menu assigned
  `browser.sort_key` directly, so choosing a different key re-labelled the menu
  without re-ordering the folder until the direction button was also pressed.
  Changing the key now goes through `set_sort`, which is what the direction
  button already did.
- **A test-suite race that my change exposed.** Two older tests built a
  `BridgeApp` without holding `STORE_ENV_LOCK`, and since folder open now
  resolves the index path, an indexing test could read whichever `HOME` another
  thread had set. Their fixtures landed in another test's directory — and, in a
  fresh process, the developer's real one: `/tmp/opencode/*` paths appeared in a
  real `recents.json`, which is what made me look. Isolation is now
  compile-enforced: the helpers take the guard as an argument, so a test cannot
  isolate without holding it.
- **One design question settled by consistency rather than guesswork.** Whether
  a descending tag sort also reverses its name tie-break: it does, because that
  is what `Desc` already means for name and size. My first test asserted the
  opposite on an assumption, and the code was the more coherent answer. The
  label ordering is alphabetical with unlabeled last rather than the eight chip
  names, because those are an identity list and no Bridge behaviour for label
  ordering has been verified.
- **Framework limits learned the hard way.** A frame drawn 60 px tall clipped
  the sort menu, so a new test reported two missing keys that a real user can
  see; and an egui menu closes when the frame following its click carries no
  pointer, so the toolbar helpers now rest the cursor where a hand would be.
  Both were bugs in my tests, not in the app.

### Library search, and a claim of mine that turned out to be a bug report about my own fixture (slice 9, shipped 2026-11-17)

Started as "index-backed search and one query implementation", ended with the
query model collapsed, a search field, a results listing mode — and two defects
in already-shipped behaviour, one of which I caused and one of which I nearly
reported wrongly.

- **A false interop alarm, caught by checking bytes instead of reading text.** A release-binary run against sidecars I had generated showed *every* rating as 0. I wrote this up as a serious bug in the XMP reader. The reader was fine: my fixture carried the RDF namespace as `…/1999-02-22-rdf-syntax-ns#` instead of `…/1999/02/22-rdf-syntax-ns#`, because I had retyped it from rendered output and this environment displays `/` as `-` inside such strings. A packet in a namespace that does not exist parses as empty, which is indistinguishable from a reader that cannot handle attributes. Rebuilt byte-by-byte, the same run read 1/3/5/0 with the right labels. Kept as a lesson in the tech debt list: an assertion that a fixture *looks* right is not a test.
- **A real bug in shipped behaviour, found while measuring that non-bug:** clearing a star never reached the file. `write_embedded_xmp` merged into the existing packet, and `set_rating(Rating::new(0))` removes the property rather than writing zero — so a save meaning "cleared" carried no rating, and absence was read as "not touched". A JPEG that held four stars kept them, and the sidecar beside it disagreed. Proven with `tmp_probe_whether_a_cleared_rating_persists` (run once, then deleted), then fixed by making the intent explicit in the API: `TagWrite::Replace` for the whole tag state (panel save, export) and `Merge` for a partial update, which is what `rewriting_a_packet_preserves_what_the_caller_did_not_change` has always described. Both guarantees are now tested; the Merge one failing when `Replace` is substituted, and the Replace one failing the same way round, is how I checked neither test is decorative. Slice 2's roadmap line said "clicking the current rating clears it" — true in the session since then, and only true in the file from now on.
- **Timestamp ties, which were never a flaky test.** The staleness test that flaked once in the previous slice flaked again here, and instead of re-running until it went green I bracketed two same-size writes in a loop: 149 of 400 reused the previous timestamp, and 80 rows were left permanently stale. So the previous slice's "loose end, cause not established" was a real hole, and the assertion message I had added last time is what made it diagnosable rather than luck. Fixed for the case the application controls (`indexer::reindex`, used by the save path) and recorded as a limit for the case it does not, with `same_size_edits_reuse_the_timestamp_and_a_reindex_covers_them` asserting the ties happen — a test that fails if the filesystem ever stops reusing timestamps, so the workaround cannot outlive its reason unnoticed.
- **The `root` column was still wrong, and my own test was why.** Last slice I stamped it in the end-of-pass sweep, so rows written after the sweep kept `NULL`: a release run showed three rows stamped and one not. Stamped at write time from the containing directory instead, the sweep's `mark_root` deleted, and the test that had passed all along now asserts the column *and* the table — it only ever checked the table.
- **Query aliases deleted rather than migrated.** The debt said they were kept because stored smart-collection rows might contain them. Nothing has ever called `save_smart_collection`, so no such row exists anywhere; the six containers and fourteen aliases went, and `matches` now behaves the same whatever spelling a query arrived in. `Query::and`/`or` went with them: `with(predicates, combinator)` is the same thing named once.
- **A guard that only worked in tests:** `handle_search_shortcuts` read `RawInput::focused` to decide whether Esc belonged to a text field. That field means *window* focus and is true in nearly every real frame, so the shortcut would have been dead on screen — the test caught it only because a default happened to make it true. Now reads egui's widget focus.
- **Two more test-harness lessons worth keeping.** `menu_button` closes when the release lands in a later frame, so synthetic clicks batch press and release into one frame; and egui clips a menu to the frame's screen rect, so an over-short test frame reports missing menu items.
- **Still open, deliberately:** sidecar `.xmp` files are listed and indexed as the files they are, which roughly doubles row counts for a sidecar workflow and lets a search return a sidecar. That is a listing decision, not a query bug, and it is named in the tech debt rather than patched silently.

### Saved collections, and three bugs I wrote before finding them (slice 10, shipped 2026-11-17)

- **Saving a collection wiped the grid.** `save_current_collection` re-entered
  the query to "make the listing a collection", and `enter_query` clears the
  entries on the sound principle that a new answer is pending. So naming a
  search emptied the results the user was staring at. Caught by
  `deleting_a_collection_leaves_every_file_where_it_was`, which asserted the
  grid still held its file after a save-and-delete and got an empty list — the
  test was written for a different claim and caught this instead. Fixed by
  adding `BrowserState::name_collection`, which relabels the listing without
  touching its rows: re-running an unchanged query to produce an unchanged grid
  is work that only creates the chance of being wrong.
- **The Collections panel queried the database every frame.** It refreshed on
  each draw, which put a `smart_collections` SELECT in the paint path *and*
  clobbered any state a caller had just put in the list. Now the refresh is
  edge-triggered on entering the tab (tracked with `sidebar_tab_shown`) plus
  explicit refreshes after a save or delete.
- **The scope popup would have lied.** Running a collection scoped to folder A
  while the user was in folder B set the popup to "the whole library", because
  my helper had no honest label for "a folder you are not in". A saved scope is
  now shown as the saved scope, non-editable, and the choices return as soon as
  the grid is an ad hoc search again.
- **egui needs the pointer to arrive before the press.** Batching
  move+press+release into one frame silently registers no click: the sidebar tab
  test failed with the click swallowed, while the same helper worked on the
  toolbar because an earlier frame had already left the pointer somewhere else.
  `click_at` now moves in one frame and presses in the next, which is what a
  hand does, and the reason is written where the next reader will find it.
- **A test-only dependency refused.** The damaged-definition test originally
  corrupted a row through `rusqlite`, which bc-ui does not depend on and should
  not gain for a test. The claim was split instead: proving a stored definition
  *can* be unreadable belongs to the engine, against a real database
  (`an_unreadable_definition_is_reported_rather_than_dropped`, which is also the
  test that replaced the reader's silent `if let Ok(...)` filter); proving the
  interface handles that state belongs to the UI, with the state constructed
  directly. Both halves are honest, and neither drags SQLite into the front end.
- **A storage consequence, recorded rather than buried.** Saved collections live
  in `metadata.sqlite`, which is otherwise a rebuildable cache — so the file
  that `EXTRACTOR_VERSION` bumps exist to make disposable now also holds
  something a user wrote by hand. Tolerable while nothing could write a
  collection, not tolerable now that a button does. Named in Tech debt with the
  fix (their own store beside favourites and recents) and with the version field
  that makes the move a copy rather than a rewrite.

### CI was already red on two of three platforms, and one of those was mine (2026-11-19)

Pushing the eleven accumulated commits ran the matrix for the first time since
2026-11-15, and both the old and the new runs fail the same two legs — while
everything I checked locally passed, because local is Linux.

- **Windows: the atomic writer's temp file name, and a colon.** `write_atomic` names its
  temp file `.‹file›.bridgecraft-tmp-‹pid›-‹thread›-‹nanos›-‹seq›`, and a Rust thread name is
  arbitrary text. Under `cargo test` the thread is named for the test —
  `favorites::tests::add_is_ordered_deduplicated_and_persisted` — and `:` on Windows starts
  a drive specifier, so `CreateFile` failed with os error 123 (`InvalidFilename`) and *every*
  store that writes atomically reported itself unusable. The bug is older than this session:
  the 2026-11-15 run already failed three `auto_process` tests this way, when the helper was
  still private to that module. What this session did was widen its reach — favourites,
  recents, saved searches and now templates all went through the one writer — turning 3 red
  tests into 20. Fixed by sanitising the thread tag to `[A-Za-z0-9_-]` and truncating it to
  32 characters (the pid, clock and counter are what make the name unique; the thread is
  only a label), with `a_temp_name_uses_only_characters_legal_in_a_windows_file_name`
  asserting the illegal set directly — which reproduces the Windows failure on Linux, the
  only place I can prove the fix without a Windows machine. Checked by hand: with the
  sanitiser removed the test fails, naming the colon.
- **macOS: a dependency that no longer compiles, and it is not mine.** `objc2 0.4.1`, pulled
  in transitively by the winit/eframe stack, fails type-checking with the current toolchain
  (`E0277` inside `objc2`'s own `ivar_forwarding_impls`). The same failure is present in the
  2026-11-15 run, so nothing in these commits caused it, and nothing in these commits can fix
  it: it needs the GUI stack updated or the CI toolchain pinned. Recorded in Risks rather
  than left as a red badge somebody else will one day investigate.
- **The Windows fix took two rounds, and the second round was the more interesting one.**
  Sanitising the thread tag took the Windows failures from twenty to four — `paths::tests`,
  all of them assuming that the data root and the configuration root are different
  directories. They are on Linux. On Windows both resolve to `%APPDATA%` and on macOS both to
  `~/Library/Application Support`, so "write the legacy database in config and the current one
  in data" writes *one file twice*, and "assert the journal is not in the config directory"
  asserts something that cannot be true there. Three tests now branch on `roots_are_distinct()`,
  and the same shape is reachable deliberately on any machine: `collapse_roots()` points both
  XDG variables at one directory, so `a_migration_with_nowhere_to_move_from_leaves_exactly_one_database`
  and `the_journal_has_one_answer_when_config_and_data_are_one_tree` exercise on Linux the code
  paths Windows and macOS take. The first of those caught something worth catching: with the
  early return removed and a cleanup `remove_file(legacy)` after a self-rename, the live
  database is deleted during a migration that had nowhere to migrate from — a data-loss bug
  that no Linux-root layout can trigger and that was, until it had a test, unexamined.
- **The uncomfortable general point.** Nine slices of "verified against the release binary"
  were verified on one platform, and the repository's own quality gate —
  `cargo test --workspace` — was green the whole time. A cross-platform claim in the README
  is worth nothing if the only matrix run is the one nobody looked at. The matrix has been
  red since before this session started; the difference now is that it is named.

### Metadata templates that can only change what they name (slice 15, shipped 2026-11-19)

Critical path item 3 said four engines were "implemented, tested and unreachable".
This slice wires one of them, and the reason it took a slice rather than an afternoon
is that the interesting problem was not the merge rules — `bc_metadata::templates`
already had those and they are fine — it was deciding what a template is *allowed to
name*, which is a question the engine cannot ask because it has no idea which of a
file's properties a human meant to overwrite.

- **The second consecutive stale deferral note.** Phase 3's line said templates waited
  on "no path that writes a template onto a file". There were two, and both were
  reachable from the panel: `apply_changes` for the text fields and `TagStamp` for the
  four tag properties. Yesterday's lesson was that a roadmap claim about what is missing
  needs checking before it is built on; today it was the same lesson in the other
  direction — the missing piece was not a writer but a *projection*, and the writers were
  already there.
- **`Replace` meant "erase the file".** The engine's `TemplateMode::Replace` assigns the
  template's property map over the target's, so a template holding one copyright line,
  applied to a file with a description, an artist and six keywords, drops all of them. No
  user means that by "overwrite existing values", and no preview would have warned them in
  time across four hundred files. So the plan is built **property by property over the
  properties the template names**, and the mode decides what happens to those. The engine
  still does the merge — one definition of the three modes — and the projection reads only
  the parts of its answer that correspond to a named property. `a_template_names_only_the_
  fields_that_hold_something` checks all three modes against a file that has fields the
  template never mentions.
- **A template cannot carry someone else's camera.** Capture takes only the fields the
  bulk editor will stamp (`spec.bulk`): Description, Artist, Copyright, plus the tags. Make,
  Model, Software and Date Taken are editable for one file and refused in a many-file write,
  and a template is by definition many files — so capturing from a JPEG does not produce a
  template that stamps "Canon EOS 5D" onto the Nikon files it is later pointed at.
  `plan` deliberately does *not* repeat that restriction: a hand-edited document that names
  a Make should be obeyed rather than second-guessed, because after capture the document is
  the user's.
- **A no-op apply must not write, because writing creates files.** The tag writer
  establishes a sidecar where none exists, so an apply that changed nothing and still called
  it would leave a `.xmp` behind for every file it touched — four hundred selected, four
  hundred new files, all saying nothing. The plan is checked empty first, and the report
  counts `already_matching` apart from `changed`, because "applied to 0 of 400" and "400
  already matched" look identical in a success message and mean opposite things.
- **The status line distinguishes three absences** the way slice 14's keyword row does:
  nothing chosen, nothing to change, and something failed — with the first failing path
  named rather than a count of failures that cannot be acted on.
- **A disclosure, forced by a test I broke.** The first version drew its widgets directly
  in the panel, and `every_tag_control_in_the_panel_does_something_when_activated` — the
  keyboard walk that Tabs through the tag controls and Shift+Tabs back to a keyword chip —
  started failing. The honest reading was not "the walk needs more steps" but "the panel has
  grown six focusable controls since that test was written, and a combo box now swallows the
  Enter meant for the chip". Folding the section makes the panel cheaper to look at and the
  tab order costless until someone asks for it. One open question this exposes is worth
  recording rather than arguing: the existing walk test is a *sequence* test, and every
  future panel control will trip it. It survives because folding keeps the count where it
  was, not because the test is robust.
- **Three harness facts, each worth more than the code that taught them.**
  (1) A `CollapsingHeader` reveals its body over several frames — probed: one row per frame,
  so a test that opens a section and looks for a widget inside sees only the first row and
  concludes the control is missing. `still_ctx()` switches `animation_time` off for the
  tests that need a settled layout.
  (2) A `SidePanel` with no fixed width resizes to its content, and *each pass moves
  everything inside it*: measured at x = 246, 148, 80, 31, 26 across five passes as one
  section opened. A helper that finds a widget in one pass and clicks it in the next is then
  aiming at a coordinate that no longer holds it, and the failure is indistinguishable from a
  control that does not respond. `exact_width(380)` makes the layout stable and the test
  honest.
  (3) My first explanation for (2) was wrong and I wrote it down before testing it: I was
  sure `click_at`'s leading no-event draw was double-toggling the header. A probe that
  clicked once per pass and printed the open state after each one refuted it in a single run
  — `click_panel` toggles exactly once. The real causes were a shared header id (the section
  belongs to the panel, not the file, so the test's second "open" click had closed it again)
  and the width drift. Writing the hypothesis down was still worth it; believing it would
  not have been.
- **Four dead file helpers went with it.** `bc_metadata::templates` carried
  `save_template`, `load_template`, `load_templates` and `templates_dir` — a
  plain-`fs::write` persistence path that no code called, and that bc-metadata
  cannot fix: the atomic writer lives in bc-core, which depends on this crate rather
  than the other way round. Deleting them is not tidiness — leaving a non-atomic
  template writer one `use` away from a future caller is exactly how two answers to
  one question come back. Their round-trip test became a test of the *shape*, since
  the shape is what `bc_core::MetadataTemplates` writes, and writing it made the
  missing tolerance visible: `Metadata`'s two members had no `serde(default)`, so a
  hand-written `{"name":"plain","metadata":{}}` — the likeliest thing a person would
  type into a template file — failed to parse with "missing field `native`". Both
  members now default on read; the writer still emits both.
- **A flake of my own making, caught by running it twice.** The store test that asserts
  `open_default` finds its document set `HOME` and `XDG_DATA_HOME` itself, under no lock. It
  passed every time under a test filter and failed in a full run, where it was racing
  `paths::tests` for the same four process-wide variables. The fix was not to add a second
  lock — two guards over one shared resource is how two tests both believe they are isolated
  — but to make `paths::tests::isolate_env` and `expected_root` crate-visible and put the new
  test under the same one, which also drops the Linux-shaped path the assertion had baked in.
  Then three runs of `bc-core` and two of the workspace, all 430 green, because "it passed
  once" is not evidence about a race.
- **One test of mine was wrong about the engine, not the other way round.** For Replace
  mode with template {Clients|ACME, Holiday} against a file holding {Clients, Holiday} I
  asserted the plan would remove both and add both. It removes `Clients` and adds
  `Clients|ACME` and leaves `Holiday` alone — which is correct, and is the minimal edit a
  writer should make. Deriving the expectation from the sets instead of eyeballing it is what
  caught it; the code never changed.
- **What was not touched, and why the other three are still on the list.** Contact sheets,
  publish presets and version stacks remain unreachable. `bc-core::publish` is the one that
  needs a decision rather than a dialog: it runs *synchronously*, while exports of the same
  shape go through the job queue for progress and cancellation, so wiring it as it stands
  would put a multi-file write on the UI thread — the same class of choice the live count
  this file argues for twice. Version stacks also still carry the eight mutate-before-persist
  sites from slice 12. Individual template files (import/export of one template) are not
  done: the store is one document, deliberately the same shape as `saved-searches.json`,
  which buys atomicity across the set and costs portability of a member.

26 tests added — 12 for the store, 8 for the projection and its modes, 6 through the panel
and its widgets. fmt clean; clippy `--workspace --all-targets --locked -D warnings` clean;
430 tests pass, up from 404. Release binary re-run on a clean profile: 200 files indexed, no
errors logged, no `metadata-templates.json` created until something is actually saved.

### Cross-library keyword suggestions, and the one question a keyword box should ask (slice 14, shipped 2026-11-19)

The roadmap had deferred this feature for three slices with the same reason: the
suggestions menu "waits on the index UI". That became untrue when the indexer started
filling the keyword table in slice 7 — the pool to ask had existed for a week and nothing
had been re-reading the note. So this slice asks it, in both keyword boxes, and the
interesting part is not the query but what the query said back.

- **The value of the feature is the moment it prevents, not the list it shows.** A
  near-duplicate keyword is invisible after the save: two `dc:subject` entries, both
  plausible, and every search that asks for one misses the other. The completion row is
  the last point at which a person can choose the spelling the library already uses, so
  clicking a candidate stores *that* string, and the count beside it is what makes the
  choice: `Beach (73246)` is a vocabulary, `Beach (1)` is worth a look before it is
  copied. A word nothing uses is said out loud as new, because silence would read as
  "the library has no opinion", which is a different claim.
- **What the row it replaced could actually do was nothing.** The old
  `Complete:` line offered `keywords::suggest` over *the file's own* keywords, and the
  click handler then called `add_keyword` on the file with the word it had just taken
  from the file — which returns "already on this file" every time. It looked like
  type-ahead and was a button that could only refuse itself. The new row filters those
  out before drawing, so the difference is visible rather than apologetic. Deleting it
  needed a reason beyond tidiness and this is one: the reason the roadmap had deferred
  library suggestions ("a global keyword pool that nothing fills yet") was still in the
  code comment, three slices after the indexer started filling that pool.
- **A prefix is not a keyword, and the difference was one character.** The engine reused
  `normalize_keyword` for the query, which drops empty segments — so `"Beach|"` became
  `"Beach"` and a question about children was answered with the parent and its siblings.
  It looked correct in the UI: a list of plausible keywords, all of them wrong for what
  was typed. `keywords::normalize_prefix` now keeps exactly one thing that the stored form
  must drop, and the two functions carry a test that walks every case where they agree so
  they cannot drift.
- **A bug the tests found, not the eye.** `a_parent_prefix_offers_the_tree_and_a_separator_offers_only_children`
  failed on the full run after everything had already passed under a name filter — the
  same shape as slice 11's stale release binary: a filtered run is a partial
  verification wearing a green badge. The full-suite run is the only one that means
  "nothing is broken".
- **Two claims on one screen.** The probe that showed the query working also showed
  `"In use:  Beach (2)"` drawn immediately above `"“bea” is not used by any indexed file —
  this would be a new keyword."`. The row was answering and the status line was denying it.
  No test had checked for that because each test checked its own half. The fix is one
  branch — candidates present means no status line — and the assertions now check the
  absence of *every* status line, not just the one that was wrong, so the two cannot be
  friends again.
- **First-use cost, measured on a database the release binary wrote:** 0.645 s to build a
  NOCASE expression index over 735 000 keyword rows, then 0.04 ms to discover it exists,
  then 15-45 ms per query depending on how many rows share the prefix (a miss: 0.03 ms).
  `init` does not create it — that would charge every open of every existing library for a
  feature most sessions never use — so the first keyword keystroke pays it, on the worker
  thread. The honest framing in the code says what this buys and what it costs; the honest
  framing in Tech debt says nobody has measured whether users mind.
- **Harness learning worth keeping:** in this codebase's click helpers the pressing pass has
  to *move* the pointer in that same pass, or egui treats the press as arriving wherever the
  previous frame left it and the button never hears about it. `click_right_panel` now does
  its own three passes for that reason while the single-file helper keeps using `click_at`,
  and I verified the difference by printing the button's rect across passes rather than by
  reasoning about egui's pointer state.
- **Four ways this slice could have shipped wrong, and what each one actually was**: the
  prefix normaliser (a real bug, caught by a core test), the contradictory status line (a
  real bug, caught by a probe because no test spanned both halves), the tests that ticked
  without drawing (a test bug, not an app bug — but it looked identical from the failure),
  and the click helper (a harness bug). Three of the four were only visible once I asked the
  running code what it did instead of reading what I meant.

### A rule editor that counts, and six bugs it counted out of the woodwork (slice 13, shipped 2026-11-18)

The engine has honoured arbitrary rules since slice 9 and the sidebar has run
them since slice 10, so what was missing was authorship: the only way to build a
`Query` through the interface was to type words into a box. This slice adds the
editor. It also added the one control that makes an editor safe to ship — a live
"N file(s) match" — and the two together turned up six defects, all of them
mine: five made while building this dialog, and one — a coverage figure that
followed a collection run out of the search box — left in slice 10 and only
reachable now that rules can be authored.

- **`keyword is not X` was a rule that could only ever be false.** `eval_predicate`
  has no `NotEquals` arm for the keyword list, so it falls through to `_ => false`,
  while the operator menu I wrote this session *offered* it. `problem()` approved
  the pair, because it was checking a list, not the behaviour. The new test
  (`every_operator_a_field_offers_can_actually_match_a_row`) builds a value for
  every offered pair and asserts a populated row matches it, and it failed on
  exactly that pair. `NotEquals` is now not offered for keywords, with the reason
  in the list's own comment: on a multi-valued field "not Holiday" is ambiguous for
  a file tagged Holiday *and* Family, and a rule whose answer depends on which
  reading the engine happened to pick is worse than no rule.
- **"Is set" was offered on fields no file can lack.** `rating`, `mtime`, `size`,
  `path`, `name` and `extension` are always present, so `Exists` on them is not a
  filter but the whole scope — the mirror image of the bug above, and invisible in
  the same way. `Field::can_be_unset` states it once, `is_set_is_offered_exactly_where_a_value_can_actually_be_absent`
  derives it from `field_exists` on an empty row rather than from my reading of the
  code, and the guard now says "every file has a star rating, so "is set" would
  match the whole scope" instead of the generic "cannot be tested for".
- **The shortcut button I added was itself one of those.** "starred" built
  `Rating Exists`, i.e. "every file", under a label promising at least one star. It
  is `Rating >= 1` now. Nobody reviewing that line would have found it: the button
  worked, the rule was valid, only the count was the whole library.
- **Saved values came back in different letters.** `query_value()` lowercased text
  needles, copied from `Query::text()`, where the box holds the user's own text in
  a separate field and the fold is invisible. In a collection the *query* is the
  only copy, so "Holiday" was stored as "holiday" and reopened as "holiday". The
  evaluator already folds both sides of every text comparison, so the pre-folding
  bought nothing and cost the user what they typed. Gone, and the round-trip test
  now asserts a capital survives and still matches a lowercase file.
- **A collection run inherited the search box's coverage figure.**
  `search_scope_unindexed` was only ever computed in `request_search`, so running a
  library-wide collection straight after a folder search appended "2 of 5 files here
  are not indexed yet and cannot match" to a result set that had nothing to do with
  that folder. Coverage is now one helper asked of the scope being run, and the test
  that pins it fails when the call is deleted — which is the only reason I know it
  was ever wrong, because I wrote the bug and the status line in the same slice.
- **The dialog promised "every file in its scope" while words were typed in it.**
  Caught by reading my own sentence back rather than by a test: an empty *rule* set
  matches everything, an empty *conditions* list with words in the box does not, and the
  label was gated on the second while claiming the first. The test that now pins it
  (`an_editor_with_no_conditions_says_so_and_a_words_only_one_does_not`) asserts both
  halves and was checked to fail when the gate goes back.
- **Two of my own tests were hollow, and the only way to know is to break the code
  on purpose.** The stale-count test injected an answer while *nothing* was
  outstanding, so it was dropped by the receiver's drain and stayed green with the
  sequence guard deleted; rewritten to construct the outstanding state directly,
  because racing a real worker for that would make the outcome a function of machine
  load rather than of the code. The debounce test asserted after a single tick, which
  cannot distinguish "queued a scan" from "sent it now"; it now ticks two more frames
  inside the window. Both were checked by reverting the line they cover and watching
  them fail. Same shape as last slice's journal test, which compared
  `rename_journal_path()` with itself, and the same shape as the tech-debt entry
  about a test that asserted the in-memory copy while the database stayed untouched:
  an assertion about a field rather than about what a user would see.
- **A python slice by marker deleted a type declaration.** The patch cut from
  `/// The operators that can meaningfully test this field.` to `impl Operator {`,
  which happened to sit *after* `pub enum Operator` — so the enum went with the old
  comment block, and the only thing standing between that and a confusing commit was
  the compiler. Marker-delimited deletion in a file where declarations are not
  adjacent to their `impl` blocks is a footgun; anchoring on the declaration itself
  is not cleverer, it is just safer.
- **A window paints no text on its first pass.** `editor_text` returned an empty
  list and I read that as "the dialog drew nothing". A fresh `Context` needs one pass
  to lay a window out before the second one emits its glyphs; the sidebar does not
  behave this way, so the warm-up belongs in the helper and not in an assumption
  about surfaces. The probe that found it was three lines of `eprintln!` and beat any
  amount of reasoning about egui internals.
- **Two decisions the user chose on the form, recorded because they are load-bearing.**
  The count is live, debounced and answered by the *existing* worker rather than a
  second thread: one SQLite reader, two threads competing for it, and a count is a
  scan — 1.4 ms per 2 000 rows, roughly 70 ms per 100 000 per keystroke. And a
  collection stores words + rules + scope, with the live Tags filter deliberately
  *not* folded in: the Tags filter is a view narrowing, and having it leak into the
  saved document would give one stored search two meanings that can drift apart.
- **Deliberately not done.** `MetadataIndex::count` was not added: the cost of a
  search is reading the rows, which happens either way, so it would be a second name
  for one scan; the saving is only in what crosses the channel, and that is a usize.
  Size, date and dimension fields are still not in the menus — they need a unit and a
  picker, not a box that asks for nanoseconds since the epoch, and `Created`/`Modified`
  are ISO *text*, so ordering them is a string compare that works only while the
  format stays zero-padded. That is the remaining half of this feature, listed in Tech
  debt rather than left unmentioned.

### Where the app writes: thumbnail cache, and saved searches out of the cache (slice 11, shipped 2026-11-17)

The item on the critical path said the thumbnail cache had to leave the working
directory because bundle layouts break it. It also quietly said nothing about
where saved searches lived, and the previous slice had put them in the one file
this project treats as disposable. Both are fixed here, and both were verified
against the release binary rather than only against `cargo test`.

- **One path definition, three callers.** `paths::thumbnail_cache_path` is now
  the only place that answer exists: the browser worker, the watched-folder
  fallback and `--rebuild-cache` all use it. The old arrangement had `main.rs`
  building the path from `current_dir()` and `AutoProcessor::open_default`
  guessing at the *config* directory when handed `None` — so they only opened
  the same file while `current_dir()` succeeded, which is not a property anyone
  would have thought to test. `BridgeApp::new` no longer accepts a cache path at
  all: the parameter was the mechanism by which a working-directory guess could
  reach the worker.
- **Two of my own verification mistakes, both worth recording.** First, a
  "cache file missing" result was an artifact of my own check: the empty
  `thumbnails.sqlite` I found had been created by the `sqlite3` query I ran to
  look for it, which is exactly how a probe becomes the observation. Re-run into
  a fresh home with no post-hoc queries, the file is there with its WAL. Second,
  I ran the legacy-database check against a release binary built *before* the
  collections change, and concluded from its output that the schema edit had not
  worked; the source was right and the artifact was stale. Both mistakes had the
  same shape — trusting a result without asking which build produced it — and
  both were caught by re-running after a rebuild, not by reasoning harder.
- **A dangling `);` that only the tests could tell me about.** Cutting the
  `smart_collections` `CREATE TABLE` out of the schema batch left its closing
  parenthesis behind, producing SQL that failed at `init()` and lit up 16 tests
  at once. The failure message named the syntax and the offset; the fix was one
  line. It is worth noting that the change was a deletion of *nothing the code
  still used* — which is exactly the kind of edit that a green type check would
  not have caught.
- **The panel was lying, and my new test caught it before anything shipped.**
  An unreadable `saved-searches.json` produced "No saved searches yet. Run one,
  then name it above." — the file still held the user's collections; the panel
  simply could not read them. The cause was a field left over from the database
  version (`collections_error`) that the panel still displayed and nothing still
  set. Fixed by reading the live error, naming the file, and disabling the save
  control with a reason, because the worst available outcome is a save that
  appears to work and turns one bad byte into an empty file.
- **A guarantee worth more than the feature.** The store refuses to overwrite a
  file it cannot parse, and `an_unreadable_saved_search_file_is_reported_and_never_overwritten`
  asserts the bytes are unchanged afterwards. That behaviour only matters
  because this data is not rebuildable — which is the entire reason it moved.
- **The scope decision, argued rather than assumed.** Saving a search no longer
  needs the metadata index at all, so the panel's old preamble ("Collections are
  stored in the metadata index, which is offline…") is gone: an offline index
  costs *running* a search, not keeping one, and the list and the save control
  stay available. `run_collection` now says so specifically when the index is
  unavailable, in a sentence that distinguishes it from the store being
  unreadable — two failures that used to share one message.
- **Still open, recorded rather than quietly deferred:** `favorites.json` and
  `recents.json` persist with a plain `fs::write`, so a crash mid-write costs
  the user's favourites, and this crate's readers treat unparseable JSON as an
  error rather than a miss. The atomic writer is now shared and one line to
  adopt; it was not this slice's job, and the temptation to fix adjacent files
  while in the neighbourhood is how slices stop being reviewable.

### Durability of the JSON stores, and two path bugs they led me to (slice 12, shipped 2026-11-17)

Last slice I wrote that `favorites.json` and `recents.json` still used a plain
`fs::write` and that adopting the shared atomic writer would be "one line", in a
note warning against exactly that kind of "someone will do it later". Doing it
this slice turned the note into two bug reports, which is the argument for
fixing a hazard the moment it is named rather than after it has been written
down twice.

- **The batch-rename journal had a sixth copy of the platform rules — and it was
  XDG-only.** `bc-ui::batch_rename_ui::journal_path` read `XDG_DATA_HOME`/`HOME`
  itself, so on macOS and Windows it pointed somewhere none of the other stores
  live.
- **Two writers, two journals, one undo history.** The watched-folder action
  derived the journal from the *configuration* directory; the interactive dialog
  from the *data* directory. A rename performed by a rule could not be undone
  from the dialog, and neither side could explain why its history looked empty.
  One helper now answers for both.
- **Five sites were still recomposing the app folder by hand**
  (`user_data_dir()?.join("BridgeCraft")` in favourites, recents, trash, plugins,
  photocraft and version stacks) even though `paths::data_file` had existed since
  slice 8 — the double-append trap that helper's own comment describes. All
  routed through the helpers and the `recents::user_data_dir` alias deleted, so
  there is one place the app folder name is written.
- **The cache never opened on a clean profile, and my first verification missed
  it.** The headline of this slice was a shared thumbnail path, and the first
  real run I checked showed `thumbnails.sqlite` present — in a home directory an
  *earlier* run of mine had already created. Testing `CacheStore::open` against a
  directory that exists is not testing a first launch. The thumbnail crate
  created the cache file but not its parent, unlike every other store here, so on
  a genuinely new profile the worker logged "thumbnail cache unavailable" once and
  then silently re-decoded every image forever. The opener now creates the parent
  (`opening_a_cache_creates_its_parent_directory`) and names the directory when it
  cannot (`opening_a_cache_reports_an_impossible_location`, because "File exists
  (os error 17)" alone identifies nothing). The before/after is observed rather
  than argued: the same release flow with a clean home produced no cache file at
  all before this, and three cached thumbnails after. The lesson is about the
  check, not the code — a verification run on a machine that has been used before
  can only ever observe the used state.

- **Durability, with the limits of what I could prove stated rather than
  blurred.** Writes go through temp+`sync_all`+rename; favourites and recents now
  persist the *candidate* list before adopting it, so a failed save cannot leave
  the screen showing something the disk never got. What is *not* proven: a torn
  write cannot be staged honestly in a test — the permission-denied failure
  happens at open, before either the old or the new code would truncate — so the
  store-level test says so in its own comment, and the properties that can be
  pinned (`paths::tests`: no leftover temp, a stale temp does not block the next
  write, unique names) are pinned there instead.
- **A stale temp name was an availability bug waiting to be inherited.**
  `write_atomic` named its temp file `pid`+thread only. A process killed between
  write and rename leaves that file behind, and a later process with a reused pid
  then fails `create_new` — permanently, with a message the user cannot act on.
  I was about to hand that helper to three more stores, so the name now carries a
  per-call counter as well, the same reasoning the export pool already documents.
  `every_attempt_gets_its_own_temp_name` and
  `a_stale_temp_from_a_killed_run_does_not_block_the_next_write` both fail against
  the previous scheme; I checked that rather than assuming it.
- **Four mistakes of mine, all caught before committing**: a test I wrote against
  an assumed filename in `paths.rs` instead of the real one; an anchor I invented
  rather than grepped, so a patch silently no-op'd; a hand-written config fixture
  missing `version`, whose failure message told me exactly what was wrong; and —
  most worth keeping — a first version of the journal test that compared
  `rename_journal_path()` with itself. It passed, and proved nothing. The rewrite
  constructs an `AutoProcessor` through the production default and asserts the two
  writers name the same file.

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
- **The macOS build is broken by a transitive dependency, and has been since before this
  phase of work.** `objc2 0.4.1` — pulled in by the winit/eframe stack, not chosen by us —
  does not type-check against the current Rust toolchain (`E0277` in its own
  `ivar_forwarding_impls`), so `cargo test --workspace` fails on `macos-latest` CI while
  passing on Linux. Ubuntu and Windows legs are the ones anyone has watched. Mitigate: update
  the GUI stack (eframe/winit → a release resolving a newer objc2), or pin the CI toolchain to
  a version `objc2 0.4.1` still compiles under; a universal-2 Phase 6 build is not reachable
  until one of the two is done. Windows had its own red — an atomic-writer temp name built
  from a test thread name, which contains `:` and so is illegal there — fixed on 2026-11-19.
- Unsigned distribution artifacts will trigger OS trust warnings (macOS Gatekeeper, Windows SmartScreen) that users must dismiss manually (accepted trade-off: no signing certificates are purchased or managed)

---


