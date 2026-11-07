# Phase 2 — Thumbnails & Cache

Implemented in `bc-thumbnails`:

- **Decoder** (`decoder.rs`): decodes PNG/JPEG/GIF/WebP/TIFF/BMP via the `image` crate, converts to RGBA8, downscales so the longest edge fits the requested size (aspect-preserving box filter via `imageops::thumbnail`). Dispatches video extensions to the video decoder below.
- **Video** (`video.rs`): extracts one representative frame from common video containers (MP4/M4V/MOV/MKV/WebM/AVI/WMV/FLV/MPG/MPEG/M2TS/MTS/3GP/3G2/OGV/TS/VOB) using a **locally installed `ffmpeg` binary** through `ffmpeg-sidecar`. FFmpeg is spawned per clip with `-ss 1` (falling back to `0` for clips shorter than a second), builds a single frame scaled with `scale=<edge>:<edge>:force_original_aspect_ratio=decrease`, and pipes raw RGBA back on stdout — no temporary files and no full-resolution decode. FFmpeg is **not linked or bundled**: when no binary is on `PATH` (or next to the app), `video::is_available()` is false and `decoder::can_thumbnail()` reports video files as unsupported, so browsing a folder of clips does not spam failures. The probe is cached once per process.
- **SQLite cache** (`cache.rs`): rows keyed by `(path, max_edge)` with `(mtime_ns, size)` validation — any file or size-tier change invalidates automatically. `put` upserts; `prune_missing` removes rows for deleted files.
- **Orchestrator** (`lib.rs::load_or_generate`): cache-first with graceful fallback to decode when the cache errors; cache writes are best-effort and never fail the caller.
- **bc-ui**: opens `.bridgecraft-thumbs.db` in the CWD, generates thumbnails for the browsed directory at 256px, logs hit/miss/failed counts, prunes stale entries on startup, and reports whether video thumbnails are enabled. Failed decodes are remembered for the current listing and retried after a refresh or folder change. `--rebuild-cache` uses `can_thumbnail`, so video files are included only when FFmpeg is available.

## Verified
- Decoder max-edge + aspect-ratio tests
- Video extension classification; frame extraction from a generated 4:3 clip (100×75) and a sub-second clip (first-frame fallback); corrupt input reports `ThumbError::Video`
- Cache roundtrip, invalidation on mtime/size/max_edge, prune tests
- End-to-end cache-hit behavior test (`cache_roundtrip_and_invalidation`)

Video tests generate their fixtures with FFmpeg and skip with a log line when it is not installed, so the suite stays green on machines without it.

## Remaining
- PDF previews (pdfium bindings)
- Camera RAW (LibRaw bindings or DNG parsing)
- PSD/AI preview extraction
- Preview quality levels / 1:1 zoom viewer
- Embedded XMP/exif thumbnail reuse (Phase 3, after metadata reader exists)
- Optional: a bounded timeout around the FFmpeg child so a pathological file cannot stall the single thumbnail worker (local files have not hung in practice)
