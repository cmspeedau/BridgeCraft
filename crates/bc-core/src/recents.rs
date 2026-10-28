//! Per-user persistent tracking of recently accessed filesystem paths.

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{CoreError, Result};

/// Maximum number of paths kept by default.
pub const DEFAULT_RECENTS_LIMIT: usize = 100;
const FILE_NAME: &str = "recents.json";

/// A bounded, most-recently-used list of paths persisted as JSON.
#[derive(Debug)]
pub struct Recents {
    file: PathBuf,
    limit: usize,
    paths: Vec<PathBuf>,
}

impl Recents {
    /// Open the current user's default BridgeCraft recents store.
    pub fn open_default() -> Result<Self> {
        Self::open(
            user_data_dir()?.join("BridgeCraft").join(FILE_NAME),
            DEFAULT_RECENTS_LIMIT,
        )
    }

    /// Open a recents store at `file`, loading any previously persisted paths.
    pub fn open(file: impl Into<PathBuf>, limit: usize) -> Result<Self> {
        if limit == 0 {
            return Err(CoreError::InvalidOperation(
                "recents limit must be greater than zero".into(),
            ));
        }
        let file = file.into();
        let paths = match fs::read(&file) {
            Ok(bytes) => serde_json::from_slice::<Vec<String>>(&bytes)
                .map_err(|error| {
                    CoreError::InvalidOperation(format!("invalid recents file: {error}"))
                })?
                .into_iter()
                .map(PathBuf::from)
                .take(limit)
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self { file, limit, paths })
    }

    /// Record a successfully accessible file or directory path as most recent.
    /// The path is only added if it exists; symbolic links are recorded as paths
    /// without following them, consistent with the browser's path-based model.
    pub fn record_access(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        fs::symlink_metadata(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                CoreError::PathNotFound(path.display().to_string())
            } else {
                CoreError::Io(error)
            }
        })?;
        let path_string = path.to_string_lossy().into_owned();
        self.paths
            .retain(|existing| existing != Path::new(&path_string));
        self.paths.insert(0, PathBuf::from(path_string));
        self.paths.truncate(self.limit);
        self.persist()
    }

    /// Return paths ordered from most to least recently accessed.
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Remove a path from recents and persist the updated list.
    pub fn remove(&mut self, path: impl AsRef<Path>) -> Result<()> {
        self.paths.retain(|existing| existing != path.as_ref());
        self.persist()
    }

    fn persist(&self) -> Result<()> {
        if let Some(parent) = self.file.parent() {
            fs::create_dir_all(parent)?;
        }
        let strings: Vec<String> = self
            .paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        let bytes = serde_json::to_vec_pretty(&strings).map_err(|error| {
            CoreError::InvalidOperation(format!("could not encode recents: {error}"))
        })?;
        fs::write(&self.file, bytes)?;
        Ok(())
    }
}

/// Resolve BridgeCraft's per-user data directory without external dependencies.
pub(crate) fn user_data_dir() -> Result<PathBuf> {
    #[cfg(target_os = "windows")]
    let path = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let path = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support"));
    #[cfg(all(unix, not(target_os = "macos")))]
    let path = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    #[cfg(not(any(unix, target_os = "windows")))]
    let path: Option<PathBuf> = None;

    path.ok_or_else(|| {
        CoreError::InvalidOperation("could not determine per-user data directory".into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_deduplicates_bounds_and_persists() {
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join("settings/recents.json");
        let a = temp.path().join("a");
        let b = temp.path().join("b");
        let c = temp.path().join("c");
        for path in [&a, &b, &c] {
            fs::write(path, b"x").unwrap();
        }
        let mut recents = Recents::open(&store, 2).unwrap();
        recents.record_access(&a).unwrap();
        recents.record_access(&b).unwrap();
        recents.record_access(&a).unwrap();
        recents.record_access(&c).unwrap();
        assert_eq!(recents.paths(), &[c.clone(), a.clone()]);
        let loaded = Recents::open(&store, 2).unwrap();
        assert_eq!(loaded.paths(), &[c, a]);
    }

    #[test]
    fn does_not_record_missing_paths_and_rejects_zero_capacity() {
        let temp = tempfile::tempdir().unwrap();
        assert!(Recents::open(temp.path().join("recents.json"), 0).is_err());
        let mut recents = Recents::open(temp.path().join("recents.json"), 2).unwrap();
        assert!(matches!(
            recents.record_access(temp.path().join("missing")),
            Err(CoreError::PathNotFound(_))
        ));
        assert!(recents.paths().is_empty());
    }
}
