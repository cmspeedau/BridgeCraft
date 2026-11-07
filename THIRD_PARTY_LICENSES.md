# Third-Party Licenses

Every external crate dependency must be listed here with its license and
confirmed to be Apache-2.0 / MIT / BSD / ISC compatible (see CLEANROOM.md).

| Component | License | Use | Notes |
|---|---|---|---|
| [`ffmpeg-sidecar`](https://crates.io/crates/ffmpeg-sidecar) | MIT | Drives a user-installed `ffmpeg` binary for video thumbnails | The `download_ffmpeg` feature is disabled; no binary is fetched or bundled |
| [`pdfium-render`](https://crates.io/crates/pdfium-render) | MIT OR Apache-2.0 | Dynamically binds to a user-provided Pdfium library for PDF thumbnails | Loaded at run time with `libloading`; no Pdfium code is linked or bundled |

## Runtime dependencies (not distributed)

- **FFmpeg** is an optional, user-installed executable discovered on `PATH` (or next to the app binary). BridgeCraft does not link against or redistribute FFmpeg, so the licence of the user's FFmpeg build (LGPL or GPL, depending on how it was configured) does not apply to BridgeCraft's own code.
- **Pdfium** (upstream Chromium project, BSD-3-Clause plus third-party components) is an optional library supplied by the user, located through `PDFIUM_PATH`, next to the app binary, common library directories, or the OS loader path. BridgeCraft does not link against or redistribute Pdfium; the licence of the user's copy applies only to that copy.
