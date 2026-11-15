# Phase 0 — Foundations

- [x] Cargo workspace with crate skeletons (bc-core, bc-metadata, bc-thumbnails, bc-ui)
- [x] CI: build, test, clippy, fmt on Windows / macOS / Linux
- [x] Contributor clean-room attestation (CONTRIBUTING.md)
- [x] GUI framework spike: evaluate egui, Tauri, Iced, Slint
  - criteria: virtualized 100k-item grid, file-drop, custom rendering perf, Linux Wayland support
  - outcome: egui/eframe 0.27 selected; the shell ships a virtualized grid (visible rows only,
    thumbnail requests limited to visible cells). File-drop is available for inbound OS drops via
    `raw.dropped_files`; outbound drag to the OS is not supported by eframe 0.27 (see the drag &
    drop item in ROADMAP.md Phase 1).
- [x] Spike write-up + framework decision (`docs/adr/0001-gui-framework.md`, accepted)
- [x] Error/tracing conventions (`thiserror` in libraries, `anyhow` in the binary, `tracing` throughout)
- [x] Toolchain pin: `rust-toolchain.toml` (1.100.0, with `rustfmt` and `clippy`), matched by CI

  This is a pinned build toolchain, not a verified minimum supported Rust version. `rust-version`
  is deliberately absent from `[workspace.package]`: no lower bound has been proven by a CI run,
  and an unverified MSRV claim is worse than none. Verify one (build the workspace with candidate
  toolchains in CI) before publishing a number.
