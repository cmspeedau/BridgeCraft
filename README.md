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

## License

Apache-2.0. Third-party notices will be tracked in `THIRD_PARTY_LICENSES.md`.
