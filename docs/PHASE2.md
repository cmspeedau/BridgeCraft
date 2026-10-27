# Phase 2 — Thumbnails & Cache

Implemented in `bc-thumbnails`:

- **Decoder** (`decoder.rs`): decodes PNG/JPEG/GIF/WebP/TIFF/BMP via the `image` crate, converts to RGBA8, downscales so the longest edge fits the requested size (aspect-preserving box filter via `imageops::thumbnail`).
- **SQLite cache** (`cache.rs`): rows keyed by `(path, max_edge)` with `(mtime_ns, size)` validation — any file or size-tier change invalidates automatically. `put` upserts; `prune_missing` removes rows for deleted files.
- **Orchestrator** (`lib.rs::load_or_generate`): cache-first with graceful fallback to decode when the cache errors; cache writes are best-effort and never fail the caller.
- **bc-ui**: opens `.bridgecraft-thumbs.db` in the CWD, generates thumbnails for the browsed directory at 256px, logs hit/miss/failed counts, and prunes stale entries on startup.

## Verified
- Decoder max-edge + aspect-ratio tests
- Cache roundtrip, invalidation on mtime/size/max_edge, prune tests
- End-to-end cache-hit behavior test (`cache_roundtrip_and_invalidation`)

## Remaining
- Background generation queue (needs the GUI event loop; blocked on ADR-0001)
- Perceptual cache keys / content hashing if path-based keys prove limiting
- Embedded XMP/exif thumbnail reuse (Phase 3, after metadata reader exists)
