//! PhotoCraft integration for BridgeCraft.
//!
//! PhotoCraft (`storytold/photocraft`) is a permissively licensed Rust image
//! editor. Its documented desktop CLI accepts file paths, so BridgeCraft can
//! open selected files directly. PhotoCraft also documents a headless CLI and
//! control protocol; those can support later automation and export workflows.
//! This module currently focuses on reliable app discovery, opening selected
//! assets, XMP-aware hand-off bundles, and version-stack registration.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::version_stacks::{VersionStack, VersionStacks};
use crate::{CoreError, Result};

const ENV_VAR: &str = "PHOTOCRAFT_PATH";
const HANDOFF_MANIFEST: &str = "photocraft-handoff.json";
const HANDOFF_DIRECTORY: &str = "photocraft-handoffs";
const MAX_HANDOFF_ATTEMPTS: u32 = 100;

/// Executable names tried during discovery.
pub fn executable_names() -> &'static [&'static str] {
    #[cfg(target_os = "windows")]
    {
        &["photocraft.exe", "PhotoCraft.exe", "photocraft-cli.exe"]
    }
    #[cfg(target_os = "macos")]
    {
        &["photocraft", "PhotoCraft", "photocraft-cli"]
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        &["photocraft", "PhotoCraft", "photocraft-cli"]
    }
}

/// Directories inspected for an installed PhotoCraft app.
pub fn candidate_install_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    #[cfg(target_os = "windows")]
    {
        if let Some(value) = env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(value).join("Programs").join("PhotoCraft"));
        }
        if let Some(value) = env::var_os("APPDATA") {
            dirs.push(PathBuf::from(value).join("PhotoCraft"));
        }
        dirs.push(PathBuf::from(r"C:\Program Files\PhotoCraft"));
        dirs.push(PathBuf::from(r"C:\Program Files (x86)\PhotoCraft"));
    }

    #[cfg(target_os = "macos")]
    {
        dirs.push(PathBuf::from("/Applications/PhotoCraft.app/Contents/MacOS"));
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        if let Some(home) = env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join(".local").join("bin"));
        }
    }

    #[cfg(target_os = "linux")]
    {
        dirs.extend([
            PathBuf::from("/usr/bin"),
            PathBuf::from("/usr/local/bin"),
            PathBuf::from("/opt/PhotoCraft"),
            PathBuf::from("/opt/photocraft"),
            PathBuf::from("/app/bin"),
        ]);
        if let Some(home) = env::var_os("HOME") {
            let home = PathBuf::from(home);
            dirs.push(home.join(".local").join("bin"));
            dirs.push(home.join("Applications"));
        }
        // PhotoCraft's AppImage desktop integration installs a .desktop file,
        // whose Exec line is the authoritative location of the AppImage.
        if let Some(home) = env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join(".local/share/applications"));
        }
    }

    dirs
}

/// Locate a PhotoCraft executable by checking an override, app-adjacent files,
/// standard install paths, desktop integration entries, and then `PATH`.
pub fn discover_photocraft() -> Option<PathBuf> {
    let explicit = env::var_os(ENV_VAR).map(PathBuf::from);
    discover_photocraft_with(
        explicit.as_deref(),
        env::current_exe().ok().as_deref().and_then(Path::parent),
        &candidate_install_dirs(),
        env::var_os("PATH").as_deref(),
    )
}

/// Discover PhotoCraft using explicit inputs. Useful for deterministic tests
/// without changing process-global environment variables.
pub fn discover_photocraft_with(
    explicit: Option<&Path>,
    binary_dir: Option<&Path>,
    install_dirs: &[PathBuf],
    path: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if let Some(path) = explicit {
        if path.is_file() && is_executable_file(path) {
            return Some(path.to_path_buf());
        }
        if path.is_dir() {
            if let Some(found) = executable_in_dir(path) {
                return Some(found);
            }
            if let Some(found) = appimage_from_desktop_entries(path) {
                return Some(found);
            }
        }
    }

    if let Some(dir) = binary_dir {
        if let Some(found) = executable_in_dir(dir) {
            return Some(found);
        }
    }

    for dir in install_dirs {
        if let Some(found) = executable_in_dir(dir) {
            return Some(found);
        }
        if let Some(found) = appimage_from_desktop_entries(dir) {
            return Some(found);
        }
    }

    if let Some(path) = path {
        for dir in env::split_paths(path) {
            if let Some(found) = executable_in_dir(&dir) {
                return Some(found);
            }
        }
    }
    None
}

fn executable_in_dir(dir: &Path) -> Option<PathBuf> {
    executable_names()
        .iter()
        .map(|name| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Parse the executable token from a desktop-entry Exec command. Quoted
/// paths are unquoted as one token; desktop field codes (for example `%F`)
/// following the executable are intentionally ignored.
fn desktop_exec_program(exec: &str) -> Option<&str> {
    let exec = exec.trim_start();
    if let Some(quoted) = exec.strip_prefix('"') {
        let end = quoted.find('"')?;
        Some(&quoted[..end])
    } else {
        exec.split_whitespace().next()
    }
}

/// Read PhotoCraft's freedesktop launcher and resolve its AppImage `Exec`
/// target. We only accept an Exec executable whose filename identifies the
/// PhotoCraft AppImage; arbitrary launcher commands are not executed.
fn appimage_from_desktop_entries(directory: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(directory).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("desktop") {
            continue;
        }
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        let Some(exec) = contents.lines().find_map(|line| line.strip_prefix("Exec=")) else {
            continue;
        };
        let Some(program) = desktop_exec_program(exec) else {
            continue;
        };
        let candidate = PathBuf::from(program);
        let file_name = candidate
            .file_name()?
            .to_string_lossy()
            .to_ascii_lowercase();
        if file_name.contains("photocraft")
            && candidate.is_absolute()
            && is_executable_file(&candidate)
        {
            return Some(candidate);
        }
    }
    None
}

/// Whether a PhotoCraft executable can be located.
pub fn is_available() -> bool {
    discover_photocraft().is_some()
}

/// A resolved PhotoCraft installation.
#[derive(Debug, Clone, Default)]
pub struct PhotoCraftBridge {
    executable: Option<PathBuf>,
}

impl PhotoCraftBridge {
    pub fn new() -> Self {
        Self {
            executable: discover_photocraft(),
        }
    }

    pub fn executable(&self) -> Option<&Path> {
        self.executable.as_deref()
    }

    /// Configure a binary, AppImage, or install directory selected by the user.
    pub fn set_executable(&mut self, path: impl Into<PathBuf>) -> Result<()> {
        let path = path.into();
        let executable = if path.is_file() && is_executable_file(&path) {
            path
        } else if path.is_dir() {
            executable_in_dir(&path)
                .or_else(|| appimage_from_desktop_entries(&path))
                .ok_or_else(|| {
                    CoreError::InvalidOperation(format!(
                        "no PhotoCraft executable found in {}",
                        path.display()
                    ))
                })?
        } else {
            return Err(CoreError::InvalidOperation(format!(
                "PhotoCraft path is not an executable file or install directory: {}",
                path.display()
            )));
        };
        self.executable = Some(executable);
        Ok(())
    }

    pub fn discover(&mut self) -> Option<PathBuf> {
        let found = discover_photocraft();
        self.executable.clone_from(&found);
        found
    }

    /// Open files in PhotoCraft's documented desktop interface.
    pub fn edit(&self, paths: &[PathBuf]) -> Result<()> {
        let executable = self
            .executable
            .clone()
            .or_else(discover_photocraft)
            .ok_or_else(|| {
                CoreError::InvalidOperation("PhotoCraft executable was not found".into())
            })?;
        let mut command = Command::new(&executable);
        command.args(paths);
        command.spawn().map_err(|error| {
            CoreError::InvalidOperation(format!(
                "could not launch PhotoCraft {}: {error}",
                executable.display()
            ))
        })?;
        Ok(())
    }

    pub fn launch(&self, paths: &[PathBuf]) -> Result<()> {
        self.edit(paths)
    }

    /// Prepare an XMP-aware bundle in BridgeCraft user data and open source
    /// assets directly in PhotoCraft. The bundle path is returned for reference.
    pub fn send_selection(&self, selection: &[PathBuf]) -> Result<PathBuf> {
        let handoff_dir = self.handoff_default(selection)?;
        self.edit(selection)?;
        Ok(handoff_dir)
    }

    pub fn handoff(&self, destination: &Path, selection: &[PathBuf]) -> Result<PathBuf> {
        prepare_handoff(destination, selection)
    }

    pub fn handoff_default(&self, selection: &[PathBuf]) -> Result<PathBuf> {
        let destination = crate::recents::user_data_dir()?
            .join("BridgeCraft")
            .join(HANDOFF_DIRECTORY);
        prepare_handoff(&destination, selection)
    }
}

/// Manifest entry for one selected image in a hand-off bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PhotoCraftHandoffItem {
    pub source: PathBuf,
    pub file: String,
    pub sidecar: Option<String>,
    pub metadata_json: Option<String>,
}

/// Versioned description of an exported BridgeCraft selection bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PhotoCraftHandoffManifest {
    pub schema_version: u32,
    pub source: String,
    pub created_unix: u64,
    pub items: Vec<PhotoCraftHandoffItem>,
}

fn create_unique_handoff_dir(destination: &Path, stamp: u64) -> Result<PathBuf> {
    fs::create_dir_all(destination)?;
    for attempt in 0..MAX_HANDOFF_ATTEMPTS {
        let name = if attempt == 0 {
            format!("photocraft-handoff-{stamp}")
        } else {
            format!("photocraft-handoff-{stamp}-{attempt}")
        };
        let candidate = destination.join(name);
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(CoreError::InvalidOperation(
        "could not allocate a unique PhotoCraft hand-off directory".into(),
    ))
}

/// Copy selected files and their XMP sidecars into an isolated hand-off
/// directory with a JSON manifest. Returns the created directory.
pub fn prepare_handoff(destination: &Path, selection: &[PathBuf]) -> Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let directory = create_unique_handoff_dir(destination, stamp)?;
    match populate_handoff(&directory, selection, stamp) {
        Ok(()) => Ok(directory),
        Err(error) => {
            if let Err(cleanup_error) = fs::remove_dir_all(&directory) {
                tracing::warn!(
                    "could not remove incomplete PhotoCraft hand-off {}: {cleanup_error}",
                    directory.display()
                );
            }
            Err(error)
        }
    }
}

fn populate_handoff(directory: &Path, selection: &[PathBuf], stamp: u64) -> Result<()> {
    let mut items = Vec::new();
    for (index, path) in selection.iter().filter(|path| path.is_file()).enumerate() {
        let Some(original_name) = path.file_name() else {
            continue;
        };
        let item_dir_name = format!("item-{:04}", index + 1);
        let item_dir = directory.join(&item_dir_name);
        fs::create_dir(&item_dir)?;
        let file_name = original_name.to_string_lossy().into_owned();
        fs::copy(path, item_dir.join(original_name))?;

        let sidecar = bc_metadata::xmp::sidecar_path(path);
        let sidecar_name = if sidecar.is_file() {
            let name = sidecar
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("{file_name}.xmp"));
            fs::copy(&sidecar, item_dir.join(&name))?;
            Some(format!("{item_dir_name}/{name}"))
        } else {
            None
        };
        let metadata_json = sidecar
            .is_file()
            .then(|| bc_metadata::xmp::read_xmp_file(&sidecar).ok())
            .flatten()
            .or_else(|| bc_metadata::exif::read_exif_file(path).ok())
            .and_then(|metadata| serde_json::to_string(&metadata).ok());

        items.push(PhotoCraftHandoffItem {
            source: path.clone(),
            file: format!("{item_dir_name}/{file_name}"),
            sidecar: sidecar_name,
            metadata_json,
        });
    }
    let manifest = PhotoCraftHandoffManifest {
        schema_version: 1,
        source: "BridgeCraft".into(),
        created_unix: stamp,
        items,
    };
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| {
        CoreError::InvalidOperation(format!("could not encode PhotoCraft hand-off: {error}"))
    })?;
    fs::write(directory.join(HANDOFF_MANIFEST), bytes)?;
    Ok(())
}

/// Register a PhotoCraft output as a new version of its source asset.
pub fn register_export(
    original: &Path,
    exported: &Path,
    stacks: &mut VersionStacks,
) -> Result<VersionStack> {
    if original == exported {
        return Err(CoreError::InvalidOperation(
            "a PhotoCraft export must be a different file from its original".into(),
        ));
    }
    if !original.is_file() {
        return Err(CoreError::PathNotFound(format!(
            "original asset {}",
            original.display()
        )));
    }
    if !exported.is_file() {
        return Err(CoreError::PathNotFound(format!(
            "PhotoCraft export {}",
            exported.display()
        )));
    }

    if let Some(id) = stacks
        .stacks_for(original)
        .first()
        .map(|stack| stack.id.clone())
    {
        let _ = stacks.add(&id, exported)?;
        return stacks
            .get(&id)
            .cloned()
            .ok_or_else(|| CoreError::InvalidOperation("version stack disappeared".into()));
    }
    stacks.create(&[original.to_path_buf(), exported.to_path_buf()])
}

/// Compatibility-friendly discovery helper name.
pub fn find_photocraft() -> Option<PathBuf> {
    discover_photocraft()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn make_executable(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).unwrap();
        }
        #[cfg(not(unix))]
        let _ = path;
    }

    #[test]
    fn desktop_exec_parser_handles_quoted_paths_and_field_codes() {
        assert_eq!(
            desktop_exec_program("\"/home/user/Photo Craft.AppImage\" %F"),
            Some("/home/user/Photo Craft.AppImage")
        );
        assert_eq!(desktop_exec_program("photocraft %F"), Some("photocraft"));
        assert_eq!(desktop_exec_program("\"unterminated"), None);
    }

    #[test]
    fn desktop_entry_discovers_the_installed_appimage() {
        let dir = tempdir().unwrap();
        let appimage = dir.path().join("photocraft-0.21.0-linux-x86_64.AppImage");
        fs::write(&appimage, b"fake appimage").unwrap();
        make_executable(&appimage);
        let applications = dir.path().join("applications");
        fs::create_dir(&applications).unwrap();
        fs::write(
            applications.join("ai.storyteller.photocraft.desktop"),
            format!(
                "[Desktop Entry]\nName=PhotoCraft\nExec=\"{}\" %F\n",
                appimage.display()
            ),
        )
        .unwrap();
        assert_eq!(appimage_from_desktop_entries(&applications), Some(appimage));
    }

    #[test]
    fn discovery_checks_explicit_dir_and_path() {
        let dir = tempdir().unwrap();
        let explicit = dir.path().join("chosen");
        let from_path = dir.path().join("on-path");
        fs::create_dir_all(&explicit).unwrap();
        fs::create_dir_all(&from_path).unwrap();
        let explicit_exe = explicit.join(executable_names()[0]);
        let path_exe = from_path.join(executable_names()[0]);
        fs::write(&explicit_exe, b"fake").unwrap();
        fs::write(&path_exe, b"fake").unwrap();
        make_executable(&explicit_exe);
        make_executable(&path_exe);
        assert_eq!(
            discover_photocraft_with(Some(&explicit), None, &[], None),
            Some(explicit_exe)
        );
        assert_eq!(
            discover_photocraft_with(None, None, &[], Some(from_path.as_os_str())),
            Some(path_exe)
        );
    }

    #[test]
    fn handoff_copies_files_sidecars_and_duplicate_names_safely() {
        let dir = tempdir().unwrap();
        let a_dir = dir.path().join("a");
        let b_dir = dir.path().join("b");
        fs::create_dir_all(&a_dir).unwrap();
        fs::create_dir_all(&b_dir).unwrap();
        let first = a_dir.join("photo.jpg");
        let second = b_dir.join("photo.jpg");
        fs::write(&first, b"first").unwrap();
        fs::write(&second, b"second").unwrap();
        fs::write(bc_metadata::xmp::sidecar_path(&first), b"<xmp/>").unwrap();
        let output = prepare_handoff(&dir.path().join("handoffs"), &[first, second]).unwrap();
        assert_eq!(
            fs::read(output.join("item-0001/photo.jpg")).unwrap(),
            b"first"
        );
        assert_eq!(
            fs::read(output.join("item-0002/photo.jpg")).unwrap(),
            b"second"
        );
        assert!(output.join("item-0001/photo.xmp").is_file());
        let manifest: PhotoCraftHandoffManifest =
            serde_json::from_slice(&fs::read(output.join(HANDOFF_MANIFEST)).unwrap()).unwrap();
        assert_eq!(manifest.items.len(), 2);
        assert_eq!(
            manifest.items[0].sidecar.as_deref(),
            Some("item-0001/photo.xmp")
        );
    }

    #[test]
    fn user_selected_directory_resolves_binary() {
        let dir = tempdir().unwrap();
        let executable = dir.path().join(executable_names()[0]);
        fs::write(&executable, b"fake").unwrap();
        make_executable(&executable);
        let mut bridge = PhotoCraftBridge::new();
        bridge.set_executable(dir.path()).unwrap();
        assert_eq!(bridge.executable(), Some(executable.as_path()));
        assert!(bridge.set_executable(dir.path().join("missing")).is_err());
    }

    #[test]
    fn register_export_creates_and_extends_version_stack() {
        let dir = tempdir().unwrap();
        let original = dir.path().join("image.jpg");
        let output = dir.path().join("image-edited.jpg");
        fs::write(&original, b"source").unwrap();
        fs::write(&output, b"edited").unwrap();
        let mut stacks = VersionStacks::open(dir.path().join("stacks.json")).unwrap();
        let first = register_export(&original, &output, &mut stacks).unwrap();
        let second_output = dir.path().join("image-edited-2.jpg");
        fs::write(&second_output, b"edited again").unwrap();
        let second = register_export(&original, &second_output, &mut stacks).unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(second.members.len(), 3);
    }
}
