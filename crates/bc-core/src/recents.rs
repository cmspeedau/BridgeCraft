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
        Self::open(crate::paths::data_file(FILE_NAME)?, DEFAULT_RECENTS_LIMIT)
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
        let mut candidate = self.paths.clone();
        candidate.retain(|existing| existing != Path::new(&path_string));
        candidate.insert(0, PathBuf::from(path_string));
        candidate.truncate(self.limit);
        // Written before adopted, as in `Favorites`: a failed save should leave
        // the list unchanged rather than holding an entry nothing recorded.
        self.persist_list(&candidate)?;
        self.paths = candidate;
        Ok(())
    }

    /// Return paths ordered from most to least recently accessed.
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Remove a path from recents and persist the updated list.
    pub fn remove(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let mut candidate = self.paths.clone();
        candidate.retain(|existing| existing != path.as_ref());
        self.persist_list(&candidate)?;
        self.paths = candidate;
        Ok(())
    }

    /// Write `paths` as the store's whole content, atomically.
    fn persist_list(&self, paths: &[PathBuf]) -> Result<()> {
        if let Some(parent) = self.file.parent() {
            fs::create_dir_all(parent)?;
        }
        let strings: Vec<String> = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        let bytes = serde_json::to_vec_pretty(&strings).map_err(|error| {
            CoreError::InvalidOperation(format!("could not encode recents: {error}"))
        })?;
        // Atomic for the same reason as favourites: the reader errors on a
        // malformed file, so a half-written recents list would be unavailable
        // rather than merely short.
        crate::paths::write_atomic(&self.file, &bytes).map_err(CoreError::Io)?;
        Ok(())
    }
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

    /// The same guarantee as favourites: a recents save that cannot be written
    /// must leave the file and the in-memory order alone. `BridgeApp::open_root`
    /// records every folder it opens, so a silently-adopted entry would show a
    /// recents list that the next launch does not have.
    #[test]
    #[cfg(unix)]
    fn a_failed_record_leaves_the_previous_list_intact() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let file = dir.path().join("recents.json");

        let mut recents = Recents::open(&file, 8).unwrap();
        recents.record_access(&first).unwrap();
        let saved = std::fs::read(&file).unwrap();

        std::fs::set_permissions(dir.path(), PermissionsExt::from_mode(0o500)).unwrap();
        let result = recents.record_access(&second);
        std::fs::set_permissions(dir.path(), PermissionsExt::from_mode(0o700)).unwrap();

        assert!(
            result.is_err(),
            "a record into a read-only directory succeeded"
        );
        assert_eq!(
            std::fs::read(&file).unwrap(),
            saved,
            "a failed write still changed the file"
        );
        assert_eq!(
            recents.paths().to_vec(),
            vec![first],
            "the failed record reordered the live list"
        );
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
