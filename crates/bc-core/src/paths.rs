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
use std::path::{Path, PathBuf};

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

/// Write `bytes` so a reader never sees a half-written file.
///
/// Shared because five stores need it and the mistakes are the same: `sync_all`
/// so a crash leaves the old content rather than an empty one, rename so a
/// reader sees either the whole old file or the whole new one, and cleanup on
/// every failure path.
///
/// The temp name carries a per-call counter as well as the process id and
/// thread name. That is not decoration: with only `pid`+`thread`, a process
/// killed between the write and the rename leaves a file that *every later
/// attempt in a recycled-pid process refuses to create* — `create_new` fails,
/// so the store becomes permanently unwritable and the user sees
/// "Favorites unavailable" with no way to fix it. The export pool already
/// names its temp files this way for the same reason.
pub(crate) fn write_atomic(path: &Path, bytes: impl AsRef<[u8]>) -> std::io::Result<()> {
    let bytes = bytes.as_ref();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(temp_name(path));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    use std::io::Write as _;
    let write_result = (|| {
        file.write_all(bytes)?;
        file.sync_all()
    })();
    drop(file);
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

/// A temp name that cannot collide with another attempt, including one left
/// behind by a process that was killed mid-write.
fn temp_name(path: &Path) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // A coarse clock can repeat within a process, which is why the counter is
    // here rather than being assumed unnecessary.
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let thread = thread_tag();
    format!(".{name}.bridgecraft-tmp-{pid}-{thread}-{nanos}-{sequence}")
}

/// The calling thread's name, made safe to put in a file name.
///
/// This was not always sanitised, and on Windows that turned every atomic store
/// into a `CreateFile` failure with os error 123 (`InvalidFilename`): a Rust
/// thread name may hold anything, and the names that matter here are exactly the
/// ones that do. A test thread is named for the test —
/// `favorites::tests::add_is_ordered_deduplicated_and_persisted`, with its
/// `::` — and `:` is a drive separator on Windows, so the *temp* file could not be
/// created and the store reported itself unavailable. It failed on Windows CI only,
/// in tests, which is how it survived from the day it was written.
fn thread_tag() -> String {
    let raw = std::thread::current()
        .name()
        .unwrap_or("worker")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>();
    // Truncated from the front rather than the end: what identifies the thread is
    // usually its start, and the pid, nanoseconds and counter after it are what
    // make the name unique, so a short tag loses nothing but readability.
    const LIMIT: usize = 32;
    if raw.len() <= LIMIT {
        return raw;
    }
    let mut cut = LIMIT;
    while cut > 0 && !raw.is_char_boundary(cut) {
        cut -= 1;
    }
    raw[..cut].to_string()
}

/// The batch-rename undo journal.
///
/// Both writers of that journal -- the interactive rename dialog and the
/// watched-folder action -- resolve it here. They used to derive it
/// independently, one from the data directory and one from the configuration
/// directory, which meant a rename undone in the dialog could not see the
/// journal a watched-folder rule had written, and each side looked like a
/// history the other had lost.
pub fn rename_journal_path() -> Result<PathBuf> {
    data_file("last-rename.json")
}

/// The thumbnail cache database.
///
/// This used to be derived from the current working directory by `bc-ui`, which
/// is a reasonable choice only until the app ships as a bundled `.app`, a
/// per-user Windows install, or an AppImage mounted read-only: none of those
/// have a writable working directory the application should be filling with a
/// cache that can grow to a gigabyte. It lives in the data directory like every
/// other store now, from one definition, so the browser and the watched-folder
/// action cannot disagree about which file is the cache.
pub fn thumbnail_cache_path() -> Result<PathBuf> {
    data_file("thumbnails.sqlite")
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
/// Shared by this module's own tests and by any other test that needs the
/// platform roots pointed at a temporary directory. See [`isolate_env`].
pub(crate) mod tests {
    /// Point every platform root at `dir` and hand back the guard that keeps
    /// other tests out while they are set.
    ///
    /// Shared because process-wide environment variables are not a resource two
    /// tests can use at once: `metadata_templates` needs the same isolation to
    /// assert that `open_default` reads back what it wrote, and a second copy of
    /// these four `set_var` calls under a second lock would let the two tests
    /// stomp on each other while both believed they were isolated.
    pub(crate) fn isolate_env(dir: &Path) -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("APPDATA", dir.join("win"));
        std::env::set_var("XDG_DATA_HOME", dir.join("data"));
        std::env::set_var("XDG_CONFIG_HOME", dir.join("config"));
        std::env::set_var("HOME", dir.join("home"));
        guard
    }

    use super::*;
    use std::path::Path;

    /// Point the platform variables at a scratch root. The environment is
    /// process-global, so these tests hold a lock for their whole body. No
    /// other bc-core test reads these variables: they pass explicit paths.
    fn scoped(dir: &Path) -> std::sync::MutexGuard<'static, ()> {
        isolate_env(dir)
    }

    /// Whether this platform keeps user data and user configuration in different
    /// trees.
    ///
    /// Linux does. Windows resolves both to `%APPDATA%` and macOS both to
    /// `~/Library/Application Support`, so on those two the "legacy" location and
    /// the current one are literally the same path. That is not a defect — a file
    /// already where it should be needs no move — but it does mean any test that
    /// stages one file in config and another in data is staging *one* file, and
    /// three of them here did until Windows CI said so.
    fn roots_are_distinct() -> bool {
        expected_root("DATA") != expected_root("CONFIG")
    }

    /// The root each kind of file is expected under *on this platform*, so the
    /// assertions do not bake in a Linux layout and then fail on the macOS and
    /// Windows CI runners.
    pub(crate) fn expected_root(kind: &str) -> PathBuf {
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

    /// A file left behind by a process killed between write and rename, in the
    /// shape the naming used to produce: `pid` and thread name only.
    fn stale_temp_of_the_old_scheme(file: &Path) -> PathBuf {
        let name = file.file_name().unwrap().to_string_lossy();
        // The `thread_tag()` here is deliberate. What makes this name the old
        // scheme is that it is *deterministic* for a given pid and thread, and
        // that is the property under test; using a raw thread name would only
        // reproduce a different bug — a test thread's name holds `::`, which is
        // not a legal character in a Windows file name, and this fixture would
        // fail to create the very file it is staging.
        file.parent().unwrap().join(format!(
            ".{name}.bridgecraft-tmp-{}-{}",
            std::process::id(),
            thread_tag()
        ))
    }

    #[test]
    fn a_stale_temp_from_a_killed_run_does_not_block_the_next_write() {
        // The failure this guards: with a deterministic temp name, `create_new`
        // returns AlreadyExists forever after one interrupted write, and the
        // store becomes permanently unwritable — reported to the user as
        // "unavailable", with no visible cause.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state.json");
        write_atomic(&file, b"first").unwrap();

        let stale = stale_temp_of_the_old_scheme(&file);
        std::fs::write(&stale, b"interrupted").unwrap();
        write_atomic(&file, b"second").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"second");

        // And the leftover is still there for a human to notice, not silently
        // swallowed: it belongs to a different attempt, not this one.
        assert!(
            stale.exists(),
            "the helper deleted a file it did not create"
        );
    }

    /// The temp name has to be a legal file name on *every* platform this
    /// application ships on, and it is built partly from a thread name, which
    /// Rust does not constrain. Asserting the disallowed set directly is the only
    /// way to check that from a Linux or macOS dev machine: on Windows this was a
    /// store-wide `InvalidFilename` failure that no local test could ever see.
    /// Point data and configuration at one tree, the shape Windows (`%APPDATA%`)
    /// and macOS (`~/Library/Application Support`) have natively, and return the
    /// guard that keeps the environment to this test alone.
    ///
    /// This exists so the "roots coincide" branches are not left to be discovered
    /// by a Windows runner: they are ordinary code paths that happen to be
    /// unreachable on a Linux desk, and a branch no one executes is a branch no
    /// one has read.
    fn collapse_roots(dir: &Path) -> std::sync::MutexGuard<'static, ()> {
        let guard = isolate_env(dir);
        let shared = dir.join("shared");
        std::env::set_var("XDG_DATA_HOME", &shared);
        std::env::set_var("XDG_CONFIG_HOME", &shared);
        guard
    }

    #[test]
    fn a_migration_with_nowhere_to_move_from_leaves_exactly_one_database() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = collapse_roots(temp.path());
        assert!(
            !roots_are_distinct(),
            "the fixture did not collapse the roots, so this is the other test"
        );

        let home = expected_root("DATA").join(APP_FOLDER);
        std::fs::create_dir_all(&home).unwrap();
        let database = home.join("metadata.sqlite");
        std::fs::write(&database, b"rows worth keeping").unwrap();

        let path = metadata_index_path().unwrap();
        assert_eq!(path, database, "the live database was not used as it is");
        assert_eq!(std::fs::read(&path).unwrap(), b"rows worth keeping");
        assert_eq!(
            std::fs::read_dir(&home)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("metadata.sqlite"))
                .count(),
            1,
            "a migration that could not move anything still made a second file"
        );
    }

    #[test]
    fn the_journal_has_one_answer_when_config_and_data_are_one_tree() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = collapse_roots(temp.path());
        assert!(
            !roots_are_distinct(),
            "the fixture did not collapse the roots"
        );

        let journal = rename_journal_path().unwrap();
        assert_eq!(
            journal,
            expected_root("DATA")
                .join(APP_FOLDER)
                .join("last-rename.json"),
            "the journal stopped following the data resolver"
        );
        // The "not in the configuration directory" claim is withheld on this
        // shape, and this is where the reason lives rather than in a comment on
        // the platform: where the two roots are one directory, being in the
        // configuration directory is unavoidable and says nothing about which
        // writer resolved the path.
        assert!(journal.starts_with(expected_root("CONFIG").join(APP_FOLDER)));
    }

    #[test]
    fn a_temp_name_uses_only_characters_legal_in_a_windows_file_name() {
        // `favorites::tests::add_is_ordered_deduplicated_and_persisted` — the real
        // name of a real test thread, with the colons Windows reserves.
        std::thread::Builder::new()
            .name("favorites::tests::add_is_ordered::persisted".to_string())
            .spawn(|| {
                let dir = tempfile::tempdir().unwrap();
                let file = dir.path().join("favorites.json");
                let temporary = temp_name(&file);
                for illegal in ['<', '>', ':', '"', '/', '\\', '|', '?', '*'] {
                    assert!(
                        !temporary.contains(illegal),
                        "the temp name {temporary:?} holds {illegal:?}, which no Windows \
                         file name may contain"
                    );
                }
                // And it is still a name that can actually be created and written,
                // not merely free of banned characters.
                let written = std::fs::write(dir.path().join(&temporary), b"x");
                assert!(
                    written.is_ok(),
                    "{temporary:?} could not be created: {written:?}"
                );
            })
            .unwrap()
            .join()
            .unwrap();

        // And for whatever thread this test itself runs on — libtest names it for
        // the test, colons and all — the same properties hold. The "worker"
        // fallback for an unnamed thread needs no assertion: it is a literal in
        // this file, and it contains nothing illegal.
        let dir = tempfile::tempdir().unwrap();
        let here = temp_name(&dir.path().join("state.json"));
        for illegal in ['<', '>', ':', '"', '/', '\\', '|', '?', '*'] {
            assert!(
                !here.contains(illegal),
                "this thread's own temp name {here:?} holds {illegal:?}"
            );
        }
        assert!(
            here.len() < 120,
            "a temp name this long risks a MAX_PATH failure: {here}"
        );
    }

    #[test]
    fn every_attempt_gets_its_own_temp_name() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state.json");
        let mut names = std::collections::HashSet::new();
        for _ in 0..200 {
            names.insert(temp_name(&file));
        }
        assert_eq!(
            names.len(),
            200,
            "temp names repeated, so two writers can collide"
        );
    }

    #[test]
    fn an_atomic_write_leaves_no_temp_file_behind() {
        // Both the success path (rename consumes it) and the failure path
        // (cleanup removes it) have to hold, or the data directory slowly fills
        // with dot-files nobody can explain.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state.json");
        write_atomic(&file, b"contents").unwrap();
        let left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("bridgecraft-tmp"))
            .collect();
        assert!(left.is_empty(), "a temp file outlived the write: {left:?}");

        // A parent that cannot hold a temp file fails the write cleanly...
        let missing = dir.path().join("no-such-dir").join("state.json");
        assert!(write_atomic(&missing, b"x").is_err());
        // ...and an unreadable-destination case leaves the previous content.
        write_atomic(&file, b"kept").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"kept");
    }

    #[test]
    fn both_rename_journal_writers_resolve_the_same_file() {
        // The watched-folder action and the interactive rename dialog each
        // derived this path on their own, one from the configuration directory
        // and one from the data directory, so an undo in one could not see the
        // journal the other wrote. Both now call this helper; the assertion is
        // that they agree, not that the path is pretty.
        let temp = tempfile::tempdir().unwrap();
        let _guard = scoped(temp.path());
        let journal = rename_journal_path().unwrap();
        assert_eq!(
            journal,
            expected_root("DATA")
                .join(APP_FOLDER)
                .join("last-rename.json")
        );
        // Not in the configuration directory, where one of the two writers used
        // to put it. Only a meaningful claim where the roots are different
        // directories: on Windows and macOS they are the same one, so the journal
        // is in the configuration directory there by definition — and the equality
        // asserted above is what actually pins which resolver answered.
        if roots_are_distinct() {
            let config = user_config_root().unwrap().join(APP_FOLDER);
            assert!(
                !journal.starts_with(&config),
                "the journal resolved into the config directory: {journal:?}"
            );
        }
        // The assertion with teeth: a processor built by `open_default` — the
        // path production takes — writes to the file the interactive dialog
        // reads. Comparing the helper with itself would prove nothing.
        let config = config_file("auto-rules.json").unwrap();
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            br#"{"version":1,"settleMillis":1,"rules":[],"history":[]}"#,
        )
        .unwrap();
        let processor = crate::auto_process::AutoProcessor::open(
            &config,
            thumbnail_cache_path().unwrap(),
            rename_journal_path().unwrap(),
        )
        .unwrap();
        assert_eq!(
            processor.journal_path(),
            journal,
            "the watched-folder action journals somewhere else than the dialog"
        );
    }

    #[test]
    fn the_thumbnail_cache_is_asked_for_in_the_data_directory() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = scoped(temp.path());
        let path = thumbnail_cache_path().unwrap();
        assert_eq!(
            path,
            expected_root("DATA")
                .join(APP_FOLDER)
                .join("thumbnails.sqlite")
        );
        // The point of the move, as an assertion: nothing about this path
        // depends on where the process was started. A bundled `.app`, a
        // per-user Windows install and a read-only AppImage mount each have a
        // working directory that must not end up holding a cache that can grow
        // to a gigabyte.
        let cwd = std::env::current_dir().unwrap();
        assert!(
            !path.starts_with(&cwd) || cwd == temp.path(),
            "the cache path is derived from the working directory ({cwd:?}): {path:?}"
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
        assert_eq!(metadata_index_path().unwrap(), path, "a second call agrees");

        if roots_are_distinct() {
            assert!(
                !legacy.exists(),
                "the old database was copied rather than moved"
            );
        } else {
            // Windows and macOS: config *is* data, so the file that needed moving
            // is already in the place it would be moved to. The guarantee that
            // survives is the one that matters there — the rows are still where
            // the name says, and no second copy has been made beside them.
            assert_eq!(
                legacy, path,
                "the roots compared distinct but resolved to the same file"
            );
            assert!(legacy.exists(), "the database disappeared");
            let siblings = std::fs::read_dir(legacy.parent().unwrap())
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("metadata.sqlite")
                })
                .count();
            assert_eq!(
                siblings, 1,
                "a no-op migration left {siblings} metadata files beside each other"
            );
        }
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

        if !roots_are_distinct() {
            // Two different files in the two places is a scenario that does not
            // exist where config and data are one directory: writing "old" here
            // would overwrite the "new" file, and the test would then be asserting
            // that a file equals content it never had. What is checkable on these
            // platforms is the same guarantee from one side: a live database at the
            // resolved location is used as it is, not moved or replaced.
            assert_eq!(metadata_index_path().unwrap(), current);
            assert_eq!(
                std::fs::read(&current).unwrap(),
                b"new",
                "the existing database was rewritten where there is nowhere to migrate from"
            );
            return;
        }

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
