//! LightCraft desktop integration for BridgeCraft.
//!
//! LightCraft (`storytold/lightcraft`) is a separate, permissively licensed
//! photo library and RAW developer. Its documented desktop interface accepts
//! file and folder paths, importing them into its library, so BridgeCraft can
//! hand off selections without relying on an undocumented protocol. This module
//! discovers the installed app and launches it with selected files. Persistent
//! library synchronization, edit/export callbacks, and shared RAW decoders are
//! not implemented here.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{CoreError, Result};

const ENV_VAR: &str = "LIGHTCRAFT_PATH";

/// Executable names tried during LightCraft discovery.
pub fn executable_names() -> &'static [&'static str] {
    #[cfg(target_os = "windows")]
    {
        &["lightcraft.exe", "LightCraft.exe"]
    }
    #[cfg(not(target_os = "windows"))]
    {
        &["lightcraft", "LightCraft"]
    }
}

/// Standard application locations searched for LightCraft.
pub fn candidate_install_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    #[cfg(target_os = "windows")]
    {
        if let Some(local) = env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(local).join("Programs").join("LightCraft"));
        }
        if let Some(roaming) = env::var_os("APPDATA") {
            dirs.push(PathBuf::from(roaming).join("LightCraft"));
        }
        dirs.push(PathBuf::from(r"C:\Program Files\LightCraft"));
        dirs.push(PathBuf::from(r"C:\Program Files (x86)\LightCraft"));
    }
    #[cfg(target_os = "macos")]
    {
        dirs.extend([
            PathBuf::from("/Applications/LightCraft.app/Contents/MacOS"),
            PathBuf::from("/usr/local/bin"),
            PathBuf::from("/opt/homebrew/bin"),
        ]);
        if let Some(home) = env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join(".local/bin"));
        }
    }
    #[cfg(target_os = "linux")]
    {
        dirs.extend([
            PathBuf::from("/usr/bin"),
            PathBuf::from("/usr/local/bin"),
            PathBuf::from("/opt/LightCraft"),
            PathBuf::from("/opt/lightcraft"),
            PathBuf::from("/app/bin"),
        ]);
        if let Some(home) = env::var_os("HOME") {
            let home = PathBuf::from(home);
            dirs.push(home.join(".local/bin"));
            dirs.push(home.join("Applications"));
            dirs.push(home.join(".local/share/applications"));
        }
    }
    dirs
}

/// Find LightCraft using the explicit override, adjacent locations, platform
/// install paths, desktop launchers, and finally `PATH`.
pub fn discover_lightcraft() -> Option<PathBuf> {
    let explicit = env::var_os(ENV_VAR).map(PathBuf::from);
    discover_lightcraft_with(
        explicit.as_deref(),
        env::current_exe().ok().as_deref().and_then(Path::parent),
        &candidate_install_dirs(),
        env::var_os("PATH").as_deref(),
    )
}

/// Deterministic discovery helper, exposed for tests and callers with custom
/// install locations.
pub fn discover_lightcraft_with(
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
            if let Some(found) =
                executable_in_dir(path).or_else(|| appimage_from_desktop_entries(path))
            {
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
        if let Some(found) = executable_in_dir(dir).or_else(|| appimage_from_desktop_entries(dir)) {
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

fn desktop_exec_program(exec: &str) -> Option<&str> {
    let exec = exec.trim_start();
    if let Some(quoted) = exec.strip_prefix('"') {
        let end = quoted.find('"')?;
        Some(&quoted[..end])
    } else {
        exec.split_whitespace().next()
    }
}

/// Resolve a LightCraft AppImage from a freedesktop launcher. Only an absolute
/// executable path identifying LightCraft is accepted; launcher shell text is
/// never interpreted or executed.
fn appimage_from_desktop_entries(directory: &Path) -> Option<PathBuf> {
    for entry in fs::read_dir(directory).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("desktop") {
            continue;
        }
        let Ok(contents) = fs::read_to_string(path) else {
            continue;
        };
        let Some(exec) = contents.lines().find_map(|line| line.strip_prefix("Exec=")) else {
            continue;
        };
        let Some(program) = desktop_exec_program(exec) else {
            continue;
        };
        let candidate = PathBuf::from(program);
        let is_lightcraft = candidate.file_name().is_some_and(|name| {
            name.to_string_lossy()
                .to_ascii_lowercase()
                .contains("lightcraft")
        });
        if is_lightcraft && candidate.is_absolute() && is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// A resolved LightCraft desktop installation.
#[derive(Debug, Clone, Default)]
pub struct LightCraftBridge {
    executable: Option<PathBuf>,
}

impl LightCraftBridge {
    pub fn new() -> Self {
        Self {
            executable: discover_lightcraft(),
        }
    }

    pub fn executable(&self) -> Option<&Path> {
        self.executable.as_deref()
    }

    /// Configure a LightCraft binary, AppImage, or install directory.
    pub fn set_executable(&mut self, path: impl Into<PathBuf>) -> Result<()> {
        let path = path.into();
        let executable = if path.is_file() && is_executable_file(&path) {
            path
        } else if path.is_dir() {
            executable_in_dir(&path)
                .or_else(|| appimage_from_desktop_entries(&path))
                .ok_or_else(|| {
                    CoreError::InvalidOperation(format!(
                        "no LightCraft executable found in {}",
                        path.display()
                    ))
                })?
        } else {
            return Err(CoreError::InvalidOperation(format!(
                "LightCraft path is not an executable file or install directory: {}",
                path.display()
            )));
        };
        self.executable = Some(executable);
        Ok(())
    }

    /// Re-run automatic discovery and update the configured executable.
    pub fn discover(&mut self) -> Option<PathBuf> {
        let found = discover_lightcraft();
        self.executable.clone_from(&found);
        found
    }

    /// Open/import selected files in LightCraft using its documented desktop
    /// path arguments. LightCraft leaves originals untouched; subsequent
    /// developed exports are handled by LightCraft's own library workflow.
    pub fn edit(&self, paths: &[PathBuf]) -> Result<()> {
        let executable = self
            .executable
            .clone()
            .or_else(discover_lightcraft)
            .ok_or_else(|| {
                CoreError::InvalidOperation("LightCraft executable was not found".into())
            })?;
        Command::new(&executable)
            .args(paths)
            .spawn()
            .map_err(|error| {
                CoreError::InvalidOperation(format!(
                    "could not launch LightCraft {}: {error}",
                    executable.display()
                ))
            })?;
        Ok(())
    }

    pub fn launch(&self, paths: &[PathBuf]) -> Result<()> {
        self.edit(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn desktop_entry_finds_installed_appimage() {
        let dir = tempdir().unwrap();
        let appimage = dir.path().join("lightcraft-0.19.0-linux-x86_64.AppImage");
        fs::write(&appimage, b"fake appimage").unwrap();
        make_executable(&appimage);
        let applications = dir.path().join("applications");
        fs::create_dir(&applications).unwrap();
        fs::write(
            applications.join("ai.storyteller.lightcraft.desktop"),
            format!(
                "[Desktop Entry]\nName=LightCraft\nExec=\"{}\" %F\n",
                appimage.display()
            ),
        )
        .unwrap();
        assert_eq!(appimage_from_desktop_entries(&applications), Some(appimage));
    }

    #[test]
    fn discovery_prefers_explicit_install_and_searches_path() {
        let dir = tempdir().unwrap();
        let explicit = dir.path().join("custom");
        let from_path = dir.path().join("path");
        fs::create_dir_all(&explicit).unwrap();
        fs::create_dir_all(&from_path).unwrap();
        let explicit_exe = explicit.join(executable_names()[0]);
        let path_exe = from_path.join(executable_names()[0]);
        fs::write(&explicit_exe, b"fake").unwrap();
        fs::write(&path_exe, b"fake").unwrap();
        make_executable(&explicit_exe);
        make_executable(&path_exe);
        assert_eq!(
            discover_lightcraft_with(Some(&explicit), None, &[], None),
            Some(explicit_exe)
        );
        assert_eq!(
            discover_lightcraft_with(None, None, &[], Some(from_path.as_os_str())),
            Some(path_exe)
        );
    }

    #[test]
    fn selected_install_directory_resolves_and_invalid_path_errors() {
        let dir = tempdir().unwrap();
        let executable = dir.path().join(executable_names()[0]);
        fs::write(&executable, b"fake").unwrap();
        make_executable(&executable);
        let mut bridge = LightCraftBridge::new();
        bridge.set_executable(dir.path()).unwrap();
        assert_eq!(bridge.executable(), Some(executable.as_path()));
        assert!(bridge.set_executable(dir.path().join("missing")).is_err());
    }
}
