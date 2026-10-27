# ADR-0001: GUI framework — egui + eframe

**Status:** Accepted (2026-10-27)

## Context

BridgeCraft needs an immediate-mode, GPU-accelerated UI capable of rendering
100k+ item asset grids at interactive frame rates, on Windows/macOS/Linux,
from a pure-Rust codebase. Candidates: egui/eframe, Iced, Slint, Tauri.

## Decision

Use **egui with eframe** as the windowing/runtime shell.

## Rationale

- **Immediate-mode layout** fits dynamic grids: no widget-tree reconciliation
  when the content list changes on every filesystem event.
- **Pure Rust, no WebView**: one dependency chain, no browser runtime to ship.
- **First-class texture upload** (`ColorImage` → `TextureHandle`) maps directly
  to our SQLite-cached RGBA thumbnails.
- **Rapid iteration** for the Phase 1 UI track; the core crates stay UI-agnostic.

## Consequences

- Render-loop style: `App::update` called every frame; state lives in the app
  struct (`BrowserState`, `Selection` from `bc-core` already fit this model).
- Filesystem watching and thumbnail decoding run on dedicated threads and
  communicate with the UI via channels — egui is single-threaded by design.
- Con: styling/animation ceiling is lower than a retained-mode toolkit;
  acceptable for a Bridge-style utility UI.

If virtualization performance proves insufficient, the plan is to layer a
custom `Painter`-based grid before reconsidering the toolkit.
