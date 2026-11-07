//! Camera RAW thumbnails through a locally installed LibRaw library.
//!
//! BridgeCraft does not link or bundle LibRaw. This module binds to the user's
//! `libraw` at run time with [`libloading`] and drives its C API. When no
//! library can be loaded, RAW files are treated as unsupported by the
//! thumbnail pipeline (see [`crate::decoder::can_thumbnail`]) instead of
//! failing for every raw file in a folder.
//!
//! Library discovery order:
//! 1. `LIBRAW_PATH`: a library file, or a directory containing one of the
//!    platform's LibRaw names.
//! 2. The directory containing the BridgeCraft executable.
//! 3. Common install directories, then the platform loader's search path.
//!
//! Decoding prefers the camera's embedded preview (fast, camera-coloured) and
//! falls back to a full LibRaw render, which also applies the camera
//! orientation. Every call is serialised behind one mutex: LibRaw's plain
//! (non `_r`) build keeps global state, and thumbnails are produced from a
//! single worker thread anyway.
//!
//! LibRaw is dual-licensed LGPL-2.1 / CDDL-1.0 and is supplied by the user; no
//! LibRaw code is redistributed with BridgeCraft.

#[cfg(not(windows))]
use std::ffi::CString;
use std::ffi::{c_char, c_int, c_uint, c_void, CStr, OsStr};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use image::{DynamicImage, GrayImage, RgbImage, RgbaImage};
use libloading::Library;

use crate::{decoder, ThumbError, Thumbnail};

/// RAW containers LibRaw can open.
const RAW_EXTENSIONS: [&str; 26] = [
    "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "raf", "orf", "rw2", "raw", "pef",
    "srw", "dng", "3fr", "erf", "kdc", "dcr", "mos", "mrw", "x3f", "rwl", "iiq", "mef", "cap",
];

/// `LIBRAW_IMAGE_JPEG` from `libraw_const.h`.
const IMAGE_JPEG: c_int = 1;
/// `LIBRAW_IMAGE_BITMAP` from `libraw_const.h`.
const IMAGE_BITMAP: c_int = 2;

/// Mirror of `libraw_processed_image_t`. The C struct ends in a flexible
/// `data[1]` array, so only the header layout has to match exactly.
#[repr(C)]
struct ProcessedImage {
    kind: c_int,
    height: u16,
    width: u16,
    colors: u16,
    bits: u16,
    data_size: u32,
    data: [u8; 1],
}

/// Function table for a loaded LibRaw.
struct RawLib {
    /// Kept alive so the symbols below stay valid.
    _library: Library,
    init: unsafe extern "C" fn(c_uint) -> *mut c_void,
    #[cfg(not(windows))]
    open_file: unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int,
    #[cfg(windows)]
    open_wfile: unsafe extern "C" fn(*mut c_void, *const u16) -> c_int,
    unpack_thumb: unsafe extern "C" fn(*mut c_void) -> c_int,
    unpack: unsafe extern "C" fn(*mut c_void) -> c_int,
    process: unsafe extern "C" fn(*mut c_void) -> c_int,
    make_mem_thumb: unsafe extern "C" fn(*mut c_void, *mut c_int) -> *mut ProcessedImage,
    make_mem_image: unsafe extern "C" fn(*mut c_void, *mut c_int) -> *mut ProcessedImage,
    clear_mem: unsafe extern "C" fn(*mut ProcessedImage),
    close: unsafe extern "C" fn(*mut c_void),
    strerror: unsafe extern "C" fn(c_int) -> *const c_char,
    iwidth: unsafe extern "C" fn(*mut c_void) -> c_int,
    iheight: unsafe extern "C" fn(*mut c_void) -> c_int,
    /// Added in LibRaw 0.18; absent in ancient builds.
    set_output_bps: Option<unsafe extern "C" fn(*mut c_void, c_int)>,
}

impl RawLib {
    unsafe fn load(name: &OsStr) -> Result<Self, String> {
        let label = name.to_string_lossy().into_owned();
        let library = Library::new(name).map_err(|e| format!("{label}: {e}"))?;
        // Resolve every symbol against the borrow first, then move the
        // library into the struct so the pointers stay valid.
        let init = symbol(&library, b"libraw_init\0")?;
        #[cfg(not(windows))]
        let open_file = symbol(&library, b"libraw_open_file\0")?;
        #[cfg(windows)]
        let open_wfile = symbol(&library, b"libraw_open_wfile\0")?;
        let unpack_thumb = symbol(&library, b"libraw_unpack_thumb\0")?;
        let unpack = symbol(&library, b"libraw_unpack\0")?;
        let process = symbol(&library, b"libraw_dcraw_process\0")?;
        let make_mem_thumb = symbol(&library, b"libraw_dcraw_make_mem_thumb\0")?;
        let make_mem_image = symbol(&library, b"libraw_dcraw_make_mem_image\0")?;
        let clear_mem = symbol(&library, b"libraw_dcraw_clear_mem\0")?;
        let close = symbol(&library, b"libraw_close\0")?;
        let strerror = symbol(&library, b"libraw_strerror\0")?;
        let iwidth = symbol(&library, b"libraw_get_iwidth\0")?;
        let iheight = symbol(&library, b"libraw_get_iheight\0")?;
        let set_output_bps = library
            .get::<unsafe extern "C" fn(*mut c_void, c_int)>(b"libraw_set_output_bps\0")
            .ok()
            .map(|f| *f);

        Ok(Self {
            _library: library,
            init,
            #[cfg(not(windows))]
            open_file,
            #[cfg(windows)]
            open_wfile,
            unpack_thumb,
            unpack,
            process,
            make_mem_thumb,
            make_mem_image,
            clear_mem,
            close,
            strerror,
            iwidth,
            iheight,
            set_output_bps,
        })
    }
}

unsafe fn symbol<T: Copy>(library: &Library, name: &'static [u8]) -> Result<T, String> {
    library
        .get::<T>(name)
        .map(|f| *f)
        .map_err(|e| format!("{}: {e}", String::from_utf8_lossy(&name[..name.len() - 1])))
}

/// True when `path` looks like a camera RAW file by extension.
pub fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| RAW_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// Whether a LibRaw library could be loaded. The probe runs once per process.
pub fn is_available() -> bool {
    library().is_ok()
}

/// Decode a camera RAW file into an RGBA thumbnail whose longest edge fits
/// within `max_edge`, preserving aspect ratio.
pub fn decode_thumbnail(path: &Path, max_edge: u32) -> Result<Thumbnail, ThumbError> {
    let display = path.display().to_string();
    let guard = library().map_err(|message| ThumbError::Raw {
        path: display.clone(),
        message: message.to_string(),
    })?;
    let lib = guard.lock().map_err(|_| ThumbError::Raw {
        path: display.clone(),
        message: "libraw lock was poisoned by an earlier panic".into(),
    })?;

    unsafe { decode(&lib, path, max_edge.max(1)) }.map_err(|message| ThumbError::Raw {
        path: display,
        message,
    })
}

/// Process-wide LibRaw handle; loading and initialising a library is global
/// and expensive, so it happens once.
static LIBRAW: OnceLock<Result<Mutex<RawLib>, String>> = OnceLock::new();

fn library() -> Result<&'static Mutex<RawLib>, &'static str> {
    match LIBRAW.get_or_init(init_library) {
        Ok(library) => Ok(library),
        Err(message) => Err(message.as_str()),
    }
}

fn init_library() -> Result<Mutex<RawLib>, String> {
    let mut problems = Vec::new();
    for candidate in candidates() {
        match unsafe { RawLib::load(candidate.as_os_str()) } {
            Ok(library) => {
                tracing::debug!("libraw loaded from {}", candidate.display());
                return Ok(Mutex::new(library));
            }
            Err(e) => problems.push(e),
        }
    }
    let message = format!(
        "no usable LibRaw library found; set LIBRAW_PATH or install LibRaw (first failures: {})",
        problems.into_iter().take(4).collect::<Vec<_>>().join("; ")
    );
    tracing::debug!("{message}");
    Err(message)
}

fn candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Some(raw) = std::env::var_os("LIBRAW_PATH") {
        let path = PathBuf::from(raw);
        if path.is_dir() {
            paths.extend(library_names().iter().map(|name| path.join(name)));
        } else {
            paths.push(path);
        }
    }

    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        paths.extend(library_names().iter().map(|name| dir.join(name)));
    }

    for dir in platform_library_dirs() {
        paths.extend(
            library_names()
                .iter()
                .map(|name| PathBuf::from(dir).join(name)),
        );
    }

    // Finally let the platform loader resolve bare sonames.
    paths.extend(library_names().iter().map(PathBuf::from));

    paths
}

fn library_names() -> &'static [&'static str] {
    #[cfg(target_os = "linux")]
    {
        &[
            "libraw.so",
            "libraw_r.so",
            "libraw.so.25",
            "libraw.so.24",
            "libraw.so.23",
            "libraw.so.22",
            "libraw.so.21",
            "libraw.so.20",
        ]
    }
    #[cfg(target_os = "macos")]
    {
        &[
            "libraw.dylib",
            "libraw.25.dylib",
            "libraw.24.dylib",
            "libraw.23.dylib",
            "libraw.22.dylib",
            "libraw.21.dylib",
        ]
    }
    #[cfg(target_os = "windows")]
    {
        &["libraw.dll", "raw.dll"]
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        &["libraw.so", "libraw.dylib", "libraw.dll"]
    }
}

fn platform_library_dirs() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        &["/opt/homebrew/lib", "/usr/local/lib"]
    }
    #[cfg(target_os = "linux")]
    {
        &[
            "/usr/local/lib",
            "/usr/lib/x86_64-linux-gnu",
            "/usr/lib/aarch64-linux-gnu",
            "/usr/lib64",
            "/usr/lib",
        ]
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        &[]
    }
}

unsafe fn decode(lib: &RawLib, path: &Path, max_edge: u32) -> Result<Thumbnail, String> {
    let handle = (lib.init)(0);
    if handle.is_null() {
        return Err("libraw_init returned no handle".into());
    }
    let result = decode_opened(lib, handle, path, max_edge);
    (lib.close)(handle);
    result
}

unsafe fn decode_opened(
    lib: &RawLib,
    handle: *mut c_void,
    path: &Path,
    max_edge: u32,
) -> Result<Thumbnail, String> {
    open_path(lib, handle, path)?;
    // Processed dimensions carry the camera orientation; embedded previews often do not.
    let oriented = ((lib.iwidth)(handle), (lib.iheight)(handle));

    let mut preview_problem = String::new();
    let preview = match extract_preview(lib, handle, oriented, max_edge) {
        Ok(thumb) => thumb,
        Err(e) => {
            preview_problem = e;
            None
        }
    };
    if let Some(thumb) = &preview {
        if thumb.width.max(thumb.height) >= max_edge {
            return Ok(thumb.clone());
        }
    }

    if let Some(set_output_bps) = lib.set_output_bps {
        set_output_bps(handle, 8);
    }
    check(lib, (lib.unpack)(handle)).map_err(|e| render_problem(&preview_problem, &e))?;
    check(lib, (lib.process)(handle)).map_err(|e| render_problem(&preview_problem, &e))?;

    let mut err = 0;
    let processed = (lib.make_mem_image)(handle, &mut err);
    if processed.is_null() {
        return Err(render_problem(
            &preview_problem,
            &error_message(lib, err, "no processed image"),
        ));
    }
    // LibRaw applies the camera flip during processing, so no rotation here.
    let result = processed_to_image(processed).map(|image| fit(image, max_edge));
    (lib.clear_mem)(processed);

    match result {
        Ok(thumb) => Ok(thumb),
        Err(e) => match preview {
            Some(thumb) => Ok(thumb),
            None => Err(render_problem(&preview_problem, &e)),
        },
    }
}

fn render_problem(preview_problem: &str, full_problem: &str) -> String {
    if preview_problem.is_empty() {
        full_problem.to_string()
    } else {
        format!("{full_problem}; embedded preview: {preview_problem}")
    }
}

unsafe fn extract_preview(
    lib: &RawLib,
    handle: *mut c_void,
    oriented: (c_int, c_int),
    max_edge: u32,
) -> Result<Option<Thumbnail>, String> {
    // No embedded preview is normal for many raw formats, not an error.
    if (lib.unpack_thumb)(handle) != 0 {
        return Ok(None);
    }
    let mut err = 0;
    let processed = (lib.make_mem_thumb)(handle, &mut err);
    if processed.is_null() {
        return Err(error_message(lib, err, "no embedded preview"));
    }
    let result = processed_to_image(processed).map(|image| {
        // Previews are stored in sensor orientation; a different aspect from
        // the processed page means the camera flag is a 90/270 degree flip.
        let image = if false_orientation(&image, oriented) {
            image.rotate90()
        } else {
            image
        };
        fit(image, max_edge)
    });
    (lib.clear_mem)(processed);
    result.map(Some)
}

fn false_orientation(image: &DynamicImage, oriented: (c_int, c_int)) -> bool {
    if oriented.0 <= 0 || oriented.1 <= 0 {
        return false;
    }
    (image.width() > image.height()) != (oriented.0 > oriented.1)
}

fn fit(image: DynamicImage, max_edge: u32) -> Thumbnail {
    decoder::thumbnail_from_rgb(image.into_rgb8(), max_edge)
}

unsafe fn processed_to_image(processed: *mut ProcessedImage) -> Result<DynamicImage, String> {
    let header = &*processed;
    let width = u32::from(header.width);
    let height = u32::from(header.height);
    if width == 0 || height == 0 {
        return Err("libraw reported an empty image".into());
    }
    let data = std::slice::from_raw_parts(header.data.as_ptr(), header.data_size as usize);

    match header.kind {
        IMAGE_JPEG => image::load_from_memory(data)
            .map_err(|e| format!("embedded preview is not readable: {e}")),
        IMAGE_BITMAP => bitmap_to_image(header, data),
        other => Err(format!("unsupported embedded image format {other}")),
    }
}

unsafe fn bitmap_to_image(header: &ProcessedImage, data: &[u8]) -> Result<DynamicImage, String> {
    let width = u32::from(header.width);
    let height = u32::from(header.height);
    let pixels = (width as usize) * (height as usize);
    let colors = usize::from(header.colors);

    let bytes: Vec<u8> = match header.bits {
        8 if data.len() >= pixels * colors => data[..pixels * colors].to_vec(),
        16 if data.len() >= pixels * colors * 2 => {
            // Keep the most significant byte of each 16-bit channel.
            data[..pixels * colors * 2]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| pair[1])
                .collect()
        }
        bits => {
            return Err(format!(
                "embedded bitmap is too small for its {bits}-bit {colors}-channel {}x{} buffer",
                width, height
            ))
        }
    };

    match colors {
        1 => GrayImage::from_raw(width, height, bytes)
            .map(DynamicImage::ImageLuma8)
            .ok_or_else(|| "unexpected grey buffer".to_string()),
        3 => RgbImage::from_raw(width, height, bytes)
            .map(DynamicImage::ImageRgb8)
            .ok_or_else(|| "unexpected rgb buffer".to_string()),
        4 => RgbaImage::from_raw(width, height, bytes)
            .map(DynamicImage::ImageRgba8)
            .ok_or_else(|| "unexpected rgba buffer".to_string()),
        other => Err(format!("unsupported {other}-channel embedded bitmap")),
    }
}

#[cfg(unix)]
unsafe fn open_path(lib: &RawLib, handle: *mut c_void, path: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    let name = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| "path contains an interior NUL byte".to_string())?;
    check(lib, (lib.open_file)(handle, name.as_ptr()))
}

#[cfg(windows)]
unsafe fn open_path(lib: &RawLib, handle: *mut c_void, path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    check(lib, (lib.open_wfile)(handle, wide.as_ptr()))
}

#[cfg(not(any(unix, windows)))]
unsafe fn open_path(lib: &RawLib, handle: *mut c_void, path: &Path) -> Result<(), String> {
    let name = CString::new(path.to_string_lossy().as_bytes())
        .map_err(|_| "path contains an interior NUL byte".to_string())?;
    check(lib, (lib.open_file)(handle, name.as_ptr()))
}

unsafe fn check(lib: &RawLib, code: c_int) -> Result<(), String> {
    if code == 0 {
        Ok(())
    } else {
        Err(error_message(lib, code, "libraw call failed"))
    }
}

unsafe fn error_message(lib: &RawLib, code: c_int, fallback: &str) -> String {
    let text = (lib.strerror)(code);
    if text.is_null() {
        format!("{fallback} (libraw error {code})")
    } else {
        format!(
            "{} (libraw error {code})",
            CStr::from_ptr(text).to_string_lossy()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_classification() {
        assert!(is_supported(Path::new("shot.CR2")));
        assert!(is_supported(Path::new("dir/shot.dng")));
        assert!(is_supported(Path::new("shot.RW2")));
        assert!(is_supported(Path::new("shot.arw")));
        assert!(!is_supported(Path::new("page.pdf")));
        assert!(!is_supported(Path::new("clip.mp4")));
        assert!(!is_supported(Path::new("photo.jpg")));
    }

    #[test]
    fn corrupt_raw_reports_an_error() {
        if !is_available() {
            eprintln!("skipping: libraw is not available");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.cr2");
        std::fs::write(&path, b"not a raw file").unwrap();

        assert!(matches!(
            decode_thumbnail(&path, 64),
            Err(ThumbError::Raw { .. })
        ));
    }

    /// Decodes a real camera RAW when one is supplied through `RAW_SAMPLE`,
    /// which keeps sample files out of the repository while still allowing
    /// local and CI verification.
    #[test]
    fn decodes_supplied_raw_sample() {
        if !is_available() {
            eprintln!("skipping: libraw is not available");
            return;
        }
        let Some(sample) = std::env::var_os("RAW_SAMPLE").map(PathBuf::from) else {
            eprintln!("skipping: RAW_SAMPLE is not set");
            return;
        };
        if !sample.is_file() {
            eprintln!("skipping: RAW_SAMPLE {} is not a file", sample.display());
            return;
        }

        let thumb = decode_thumbnail(&sample, 256).expect("sample should decode");
        assert!(thumb.is_consistent());
        assert!(thumb.width <= 256 && thumb.height <= 256);
        assert!(thumb.width > 0 && thumb.height > 0);
        // A decoded raw is never uniformly black.
        assert!(thumb
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .any(|p| (p[0] | p[1] | p[2]) != 0));
    }
}
