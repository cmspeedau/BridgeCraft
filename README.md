# BridgeCraft

A clean-room, cross-platform digital asset browser and file manager written in Rust.

**Clean-room policy:** This project contains no Adobe code. See [CLEANROOM.md](CLEANROOM.md). All contributors must sign the attestation in [CONTRIBUTING.md](CONTRIBUTING.md) before their first contribution.

## Workspace layout

| Crate | Purpose |
|---|---|
| `bc-core` | Filesystem scanning, watchers, selection model, app state |
| `bc-metadata` | EXIF / IPTC / XMP / sidecar read & write |
| `bc-thumbnails` | Thumbnail cache (SQLite), decoders (JPEG/PNG/RAW/PSD/PDF/video) |
| `bc-ui` | Frontend shell (GUI framework TBD — see Phase 0 spike in ROADMAP) |

## Roadmap

See [ROADMAP.md](ROADMAP.md). Current phase: **Phase 0 — Foundations**.

## Building

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
```

## Releases & packaging

Final-stage release targets: a macOS universal 2 build, Windows portable ZIP + MSI installer, and a Linux AppImage with `.zsync` delta updates.

> **Note — unsigned builds (accepted trade-off):** Release artifacts are not code-signed. As a result, operating systems will show trust warnings on first launch: macOS Gatekeeper flags the app as from an *unidentified developer*, and Windows SmartScreen warns on the installer/portable binary. Users must dismiss these warnings manually (e.g. right-click → Open on macOS, "More info → Run anyway" on Windows). This trade-off is documented in the [roadmap risks](ROADMAP.md).

## License

Apache-2.0. Third-party notices will be tracked in `THIRD_PARTY_LICENSES.md`.
