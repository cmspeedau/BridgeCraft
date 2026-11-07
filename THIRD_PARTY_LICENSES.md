# Third-Party Licenses

Every external crate dependency must be listed here with its license and
confirmed to be Apache-2.0 / MIT / BSD / ISC compatible (see CLEANROOM.md).

| Component | License | Use | Notes |
|---|---|---|---|
| [`ffmpeg-sidecar`](https://crates.io/crates/ffmpeg-sidecar) | MIT | Drives a user-installed `ffmpeg` binary for video thumbnails | The `download_ffmpeg` feature is disabled; no binary is fetched or bundled |

## Runtime dependencies (not distributed)

- **FFmpeg** is an optional, user-installed executable discovered on `PATH` (or next to the app binary). BridgeCraft does not link against or redistribute FFmpeg, so the licence of the user's FFmpeg build (LGPL or GPL, depending on how it was configured) does not apply to BridgeCraft's own code.
