# BridgeCraft

A clean-room, cross-platform digital asset browser and file manager written in Rust.

**Clean-room policy:** This project contains no Adobe code. See [CLEANROOM.md](CLEANROOM.md). All contributors must sign the attestation in [CONTRIBUTING.md](CONTRIBUTING.md) before their first contribution.

## Workspace layout

| Crate | Purpose |
|---|---|
| `bc-core` | Filesystem scanning, watchers, selection model, app state |
| `bc-metadata` | EXIF / IPTC / XMP / sidecar read & write |
| `bc-thumbnails` | Thumbnail cache (SQLite); image, PSD, PDF-compatible AI, video (ffmpeg), PDF (pdfium) & camera RAW (LibRaw) decoders — optional external runtimes for video, PDF/AI and RAW |
| `bc-ui` | Frontend shell (GUI framework TBD — see Phase 0 spike in ROADMAP) |

## Roadmap

See [ROADMAP.md](ROADMAP.md). Current phase: **Phase 2 — Thumbnails & Previews**.

## Building

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
```

### Optional runtime decoders (video, PDF/AI & camera RAW)

PSD previews use embedded JPEG or 24-bit RGB image resources. PDF-compatible Illustrator (`.ai`) files use the same optional Pdfium runtime as PDFs; PostScript-only AI is reported unsupported. Video, PDF/AI and camera-RAW thumbnails use optional, locally installed decoders. None are linked or bundled, and builds work without them:

- **Video:** a `ffmpeg` executable on `PATH` (or next to the app binary).
- **PDF:** a Pdfium library (`libpdfium.so`, `libpdfium.dylib`, or `pdfium.dll`) located via `PDFIUM_PATH` (file or directory), next to the app binary, common install directories, or the OS loader's search path. `PDFIUM_PATH` takes priority.
- **Camera RAW:** a LibRaw library (`libraw.so`/`.dylib`/`.dll`, including versioned sonames) located via `LIBRAW_PATH` (file or directory), next to the app binary, common install directories, or the OS loader's search path. `LIBRAW_PATH` takes priority. RAW decoding prefers the camera's embedded preview and falls back to a full LibRaw render.

Without a decoder, the matching files are shown without a preview and everything else works normally. Tests that need these decoders generate their own fixtures and skip when a decoder is unavailable; a real camera-RAW file can be exercised locally with `RAW_SAMPLE=/path/to/file cargo test`.

## Releases & packaging

Final-stage release targets: a macOS universal 2 build, Windows portable ZIP + MSI installer, and a Linux AppImage with `.zsync` delta updates.

> **Note — unsigned builds (accepted trade-off):** Release artifacts are not code-signed. As a result, operating systems will show trust warnings on first launch: macOS Gatekeeper flags the app as from an *unidentified developer*, and Windows SmartScreen warns on the installer/portable binary. Users must dismiss these warnings manually (e.g. right-click → Open on macOS, "More info → Run anyway" on Windows). This trade-off is documented in the [roadmap risks](ROADMAP.md).

## License

Apache-2.0. Third-party notices will be tracked in `THIRD_PARTY_LICENSES.md`.
