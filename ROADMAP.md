# BridgeCraft Roadmap

A 1:1 clean-room reimplementation of Adobe Bridge in Rust.

> **Clean-room policy:** We never read Adobe Bridge source code or decompiled binaries. Features are specified from public documentation, user guides, and observable behavior, then reimplemented from our own specs.

---

## Phase 0 — Foundations (Weeks 1–4)
- [ ] Rust workspace: `crates/core`, `crates/ui`, `crates/metadata`, `crates/thumbnail`, `crates/cli`
- [ ] CI (GitHub Actions): fmt, clippy, tests on Win/macOS/Linux
- [ ] Core async runtime (tokio) + event bus architecture
- [ ] GUI framework selection & spike (egui / Tauri / Iced / Slint) targeting GPU-accelerated asset grids

## Phase 1 — File Browsing Core (Weeks 5–12)
- [ ] Virtual folder tree with favorites, recent folders, shared collections
- [ ] Directory watcher (notify) with debounced change events
- [ ] Content panel: list / details / thumbnails views
- [ ] Virtualized thumbnail grid rendering (100k+ items, 60fps target)
- [ ] Sorting (name, date, size, kind, custom), filtering, search
- [ ] Multi-select with rubber-band, shift/ctrl selection
- [ ] Copy / move / rename / delete (with recents/bin)
- [ ] Drag & drop (internal + OS interop)

## Phase 2 — Thumbnails & Previews (Weeks 9–20, overlaps)
- [ ] Thumbnail cache database (SQLite + LRU, background generation)
- [ ] Format decoders via Rust crates: PNG/JPEG/TIFF/GIF/WebP, PDF (via pdfium bindings), video via ffmpeg bindings
- [ ] Camera RAW support (LibRaw bindings or DNG parsing)
- [ ] PSD/AI/Illustrator preview extraction
- [ ] Preview quality levels: thumbnail / preview / 1:1 zoom viewer
- [ ] Batch (re)build cache command

## Phase 3 — Metadata Engine (Weeks 15–26)
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
| Milestone | Target | Definition of Done |
|---|---|---|
| M1 — Browser MVP | Q1 2027 | Browse, sort, thumbnails, basic file ops |
| M2 — Preview Engine | Q2 2027 | RAW/PSD/PDF/video previews with caching |
| M3 — Metadata Complete | Q3 2027 | XMP/EXIF/IPTC read-write, smart collections |
| M4 — Automation | Q4 2027 | Batch ops, scripting API |
| M5 — 1:1 Parity Audit | 2028 | Feature checklist parity vs. Bridge 2026 user guide |

## Risks
- RAW format coverage breadth (mitigate: LibRaw bindings)
- GUI performance on huge folders (mitigate: virtualization + cache early)
- Parity scope creep (mitigate: milestone audits, feature freeze discipline)
