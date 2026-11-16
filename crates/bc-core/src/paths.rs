//! Where BridgeCraft keeps its own files, in one place.
//!
//! Each feature used to work this out for itself: `recents` had one copy of the
//! platform directory rules, `auto_process` another that pointed at the *config*
//! directory, and the metadata index ended up beside the auto-processing rules
//! while favourites, recents, trash, plugins, version stacks and the thumbnail
//! cache all lived in the *data* directory. Two copies of the same rules drift,
//! and a database in the wrong directory is invisible to backup tools that
//! mirror XDG data.
//!
//! Platform rules: Windows `%APPDATA%`, macOS `~/Library/Application Support`,
//! and on other unixes `$XDG_DATA_HOME` / `$XDG_CONFIG_HOME` falling back to
//! `~/.local/share` and `~/.config`. The app folder is appended in
//! [`app_data_dir`], never inside the root lookups.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::{CoreError, Result};

/// The app folder name, written once.
const APP_FOLDER: &str = "BridgeCraft";

/// Per-user data root, *without* the app folder: `%APPDATA%`,
/// `~/Library/Application Support`, `$XDG_DATA_HOME` or `~/.local/share`.
///
/// `BridgeCraft` is deliberately not appended here. Every existing call site in
/// this crate already joins it, so a helper that returned the app folder would
/// have produced `BridgeCraft/BridgeCraft` the moment a caller switched over.
/// That trap is why the two steps are separate functions.
pub fn user_data_root() -> Result<PathBuf> {
    platform_root(
        env(std::env::var_os("XDG_DATA_HOME")),
        home_joined(".local/share"),
        native("Library/Application Support"),
        env(std::env::var_os("APPDATA")),
        "could not locate user data directory",
    )
}

/// Per-user config root, *without* the app folder. Watched-folder rules live
/// here; everything else lives under [`user_data_root`].
pub fn user_config_root() -> Result<PathBuf> {
    platform_root(
        env(std::env::var_os("XDG_CONFIG_HOME")),
        home_joined(".config"),
        native("Library/Application Support"),
        env(std::env::var_os("APPDATA")),
        "could not locate user config directory",
    )
}

/// `<data root>/BridgeCraft`.
pub fn app_data_dir() -> Result<PathBuf> {
    Ok(user_data_root()?.join(APP_FOLDER))
}

/// `<data root>/BridgeCraft/<name>`, the shape every store in this crate uses.
pub fn data_file(name: &str) -> Result<PathBuf> {
    Ok(app_data_dir()?.join(name))
}

/// `<config root>/BridgeCraft/<name>`.
pub fn config_file(name: &str) -> Result<PathBuf> {
    Ok(user_config_root()?.join(APP_FOLDER).join(name))
}

fn env(value: Option<OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from)
}

fn home_joined(rel: &str) -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(rel))
}

fn native(rel: &str) -> Option<PathBuf> {
    home_joined(rel)
}

/// One platform branch, so the `cfg!` logic exists in a single place.
fn platform_root(
    unix: Option<PathBuf>,
    unix_fallback: Option<PathBuf>,
    macos: Option<PathBuf>,
    windows: Option<PathBuf>,
    message: &str,
) -> Result<PathBuf> {
    let root = if cfg!(target_os = "windows") {
        windows
    } else if cfg!(target_os = "macos") {
        // macOS keeps configuration and data in the same tree, which is what
        // `recents` already resolved, so nothing needs migrating there.
        macos
    } else {
        unix.or(unix_fallback)
    };
    root.ok_or_else(|| CoreError::InvalidOperation(message.to_string()))
}

/// The metadata search index database, in the data directory.
///
/// Older builds created it next to the auto-processing rules, because its path
/// was derived from whatever the caller already held. If that file exists and
/// the data-directory one does not, it is moved rather than left as a stray:
/// the rows are worth keeping, since re-reading a large library is the
/// expensive part, and two databases for one index is the ambiguity this module
/// exists to remove.
pub fn metadata_index_path() -> Result<PathBuf> {
    let current = data_file("metadata.sqlite")?;
    if current.exists() {
        return Ok(current);
    }
    let legacy = config_file("metadata.sqlite")?;
    if legacy.exists() {
        if let Some(parent) = current.parent() {
            // A rename into a directory that does not exist yet fails, and that
            // failure would otherwise look like a broken install.
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::rename(&legacy, &current) {
            Ok(()) => {
                tracing::info!(
                    "moved the metadata index from {} to {}",
                    legacy.display(),
                    current.display()
                );
            }
            Err(error) => {
                // Best effort. A cross-device or permission failure must not
                // stop browsing: indexing simply starts fresh.
                tracing::warn!(
                    "could not move the metadata index from {}: {error}; a new one will be created",
                    legacy.display()
                );
            }
        }
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Point the platform variables at a scratch root. The environment is
    /// process-global, so these tests hold a lock for their whole body. No
    /// other bc-core test reads these variables: they pass explicit paths.
    fn scoped(dir: &Path) -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("APPDATA", dir.join("win"));
        std::env::set_var("XDG_DATA_HOME", dir.join("data"));
        std::env::set_var("XDG_CONFIG_HOME", dir.join("config"));
        std::env::set_var("HOME", dir.join("home"));
        guard
    }

    /// The root each kind of file is expected under *on this platform*, so the
    /// assertions do not bake in a Linux layout and then fail on the macOS and
    /// Windows CI runners.
    fn expected_root(kind: &str) -> PathBuf {
        if cfg!(target_os = "windows") {
            PathBuf::from(std::env::var("APPDATA").unwrap())
        } else if cfg!(target_os = "macos") {
            PathBuf::from(std::env::var("HOME").unwrap()).join("Library/Application Support")
        } else {
            let var = match kind {
                "DATA" => "XDG_DATA_HOME",
                _ => "XDG_CONFIG_HOME",
            };
            PathBuf::from(std::env::var(var).unwrap())
        }
    }

    #[test]
    fn roots_are_platform_correct_and_append_the_app_folder_exactly_once() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = scoped(temp.path());

        assert_eq!(user_data_root().unwrap(), expected_root("DATA"));
        assert_eq!(user_config_root().unwrap(), expected_root("CONFIG"));
        assert_eq!(
            app_data_dir().unwrap(),
            expected_root("DATA").join(APP_FOLDER)
        );
        assert_eq!(
            data_file("favorites.json").unwrap(),
            expected_root("DATA")
                .join(APP_FOLDER)
                .join("favorites.json")
        );
        assert_eq!(
            config_file("auto-rules.json").unwrap(),
            expected_root("CONFIG")
                .join(APP_FOLDER)
                .join("auto-rules.json")
        );
        let path = data_file("x").unwrap();
        assert_eq!(
            path.to_string_lossy().matches(APP_FOLDER).count(),
            1,
            "the app folder appears twice in {path:?}: a double-append"
        );
    }

    #[test]
    fn unix_falls_back_under_home_when_xdg_is_unset() {
        if cfg!(not(all(unix, not(target_os = "macos")))) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let _guard = scoped(temp.path());
        std::env::remove_var("XDG_DATA_HOME");
        std::env::remove_var("XDG_CONFIG_HOME");
        let home = temp.path().join("home");
        // The fallback must be what `recents` already used, or existing users
        // would find their favourites had moved.
        assert_eq!(
            user_data_root().unwrap(),
            home.join(".local/share"),
            "changed freedesktop fallback"
        );
        assert_eq!(user_config_root().unwrap(), home.join(".config"));
    }

    #[test]
    fn a_legacy_index_in_the_config_directory_is_moved_by_one_call() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = scoped(temp.path());

        let legacy = expected_root("CONFIG")
            .join(APP_FOLDER)
            .join("metadata.sqlite");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"rows worth keeping").unwrap();

        // One call has to do the whole job: name the new location, carry the
        // rows there, and leave nothing behind in config.
        let path = metadata_index_path().unwrap();
        assert_eq!(
            path,
            expected_root("DATA")
                .join(APP_FOLDER)
                .join("metadata.sqlite")
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"rows worth keeping");
        assert!(
            !legacy.exists(),
            "the old database was copied rather than moved"
        );
        assert_eq!(metadata_index_path().unwrap(), path, "a second call agrees");
    }

    #[test]
    fn an_existing_index_wins_and_the_legacy_file_is_left_for_the_user() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = scoped(temp.path());

        let current = expected_root("DATA")
            .join(APP_FOLDER)
            .join("metadata.sqlite");
        std::fs::create_dir_all(current.parent().unwrap()).unwrap();
        std::fs::write(&current, b"new").unwrap();
        let legacy = expected_root("CONFIG")
            .join(APP_FOLDER)
            .join("metadata.sqlite");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"old").unwrap();

        assert_eq!(metadata_index_path().unwrap(), current);
        assert_eq!(
            std::fs::read(&current).unwrap(),
            b"new",
            "never overwritten"
        );
        assert!(
            legacy.exists(),
            "with a live database in place the old file is not touched; deleting it is the user's call"
        );
    }

    #[test]
    fn no_platform_directory_reports_which_one_was_missing() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = scoped(temp.path());
        // Clear every variable each platform could fall back to, including the
        // ones `scoped` set, or the assertion only tests one OS branch.
        for var in ["APPDATA", "XDG_DATA_HOME", "XDG_CONFIG_HOME", "HOME"] {
            std::env::remove_var(var);
        }
        let error = user_data_root().unwrap_err();
        assert!(
            error.to_string().contains("user data directory"),
            "the message should name the missing directory, got: {error}"
        );
        let config_error = user_config_root().unwrap_err();
        assert!(
            config_error.to_string().contains("user config directory"),
            "the two failures must name different things: {config_error}"
        );
    }

    #[test]
    fn the_macos_and_windows_roots_match_what_the_stores_already_used() {
        // Pinning the platform branches by name, because a change here silently
        // relocates every store on those systems.
        let temp = tempfile::tempdir().unwrap();
        let _guard = scoped(temp.path());
        if cfg!(target_os = "macos") {
            assert_eq!(
                user_data_root().unwrap(),
                user_config_root().unwrap(),
                "macOS keeps both in one tree, as recents did"
            );
        } else if cfg!(target_os = "windows") {
            assert_eq!(
                user_data_root().unwrap(),
                PathBuf::from(std::env::var("APPDATA").unwrap()),
                "Windows uses APPDATA for both, as recents did"
            );
        } else {
            assert_ne!(
                user_data_root().unwrap(),
                user_config_root().unwrap(),
                "unix keeps data and config apart"
            );
        }
    }
}
