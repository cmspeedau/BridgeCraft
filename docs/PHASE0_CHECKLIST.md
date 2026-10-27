# Phase 0 — Foundations

- [x] Cargo workspace with crate skeletons (bc-core, bc-metadata, bc-thumbnails, bc-ui)
- [x] CI: build, test, clippy, fmt on Windows / macOS / Linux
- [x] Contributor clean-room attestation (CONTRIBUTING.md)
- [ ] GUI framework spike: evaluate egui, Tauri, Iced, Slint
  - criteria: virtualized 100k-item grid, file-drop, custom rendering perf, Linux Wayland support
- [ ] Spike write-up + framework decision (ADR in `docs/adr/0001-gui-framework.md`)
- [ ] Error/tracing conventions (thiserror for libs, anyhow for bins, `tracing` everywhere)
- [ ] MSRV pin + `rust-toolchain.toml`
