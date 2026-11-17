//! Per-user persistent list of favorite folders, mirrored from the browser's
//! favorites sidebar concept (a named, ordered, JSON-backed list).

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{CoreError, Result};

const FILE_NAME: &str = "favorites.json";

/// An ordered, de-duplicated list of favorite folder paths persisted as JSON.
#[derive(Debug)]
pub struct Favorites {
    file: PathBuf,
    paths: Vec<PathBuf>,
}

impl Favorites {
    /// Open the current user's default BridgeCraft favorites store.
    pub fn open_default() -> Result<Self> {
        Self::open(crate::paths::data_file(FILE_NAME)?)
    }

    /// Open a favorites store at `file`, loading any previously persisted paths.
    pub fn open(file: impl Into<PathBuf>) -> Result<Self> {
        let file = file.into();
        let paths = match fs::read(&file) {
            Ok(bytes) => serde_json::from_slice::<Vec<String>>(&bytes)
                .map_err(|error| {
                    CoreError::InvalidOperation(format!("invalid favorites file: {error}"))
                })?
                .into_iter()
                .map(PathBuf::from)
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self { file, paths })
    }

    /// Return the favorites in user-visible order.
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Whether `path` is already favorited.
    pub fn contains(&self, path: impl AsRef<Path>) -> bool {
        self.paths.iter().any(|p| p == path.as_ref())
    }

    /// Add a folder to the end of the list. Adding an unknown path or a
    /// duplicate changes nothing and reports `false`.
    pub fn add(&mut self, path: impl AsRef<Path>) -> Result<bool> {
        let path = path.as_ref();
        let is_dir = fs::symlink_metadata(path)
            .map(|meta| meta.is_dir())
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    CoreError::PathNotFound(path.display().to_string())
                } else {
                    CoreError::Io(error)
                }
            })?;
        if !is_dir {
            return Err(CoreError::InvalidOperation(format!(
                "favorites must be folders: {}",
                path.display()
            )));
        }
        if self.contains(path) {
            return Ok(false);
        }
        // The candidate is written before it is adopted: if the write fails,
        // the list a user is looking at must not contain a folder that is not
        // on disk, because the next reopen would silently drop it.
        let mut candidate = self.paths.clone();
        candidate.push(path.to_path_buf());
        self.persist_list(&candidate)?;
        self.paths = candidate;
        Ok(true)
    }

    /// Remove `path` from the list; reports whether anything was removed.
    pub fn remove(&mut self, path: impl AsRef<Path>) -> Result<bool> {
        let mut candidate = self.paths.clone();
        candidate.retain(|existing| existing != path.as_ref());
        let removed = candidate.len() != self.paths.len();
        if removed {
            self.persist_list(&candidate)?;
            self.paths = candidate;
        }
        Ok(removed)
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
            CoreError::InvalidOperation(format!("could not encode favorites: {error}"))
        })?;
        // Atomic: a crash or a full disk mid-write must leave the previous
        // favourites intact rather than a truncated file, which this reader
        // treats as an error rather than a cache miss — so a torn write is the
        // whole list, gone.
        crate::paths::write_atomic(&self.file, &bytes).map_err(CoreError::Io)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_is_ordered_deduplicated_and_persisted() {
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join("settings/favorites.json");
        let a = temp.path().join("a");
        let b = temp.path().join("b");
        for dir in [&a, &b] {
            fs::create_dir(dir).unwrap();
        }

        let mut favorites = Favorites::open(&store).unwrap();
        assert!(favorites.add(&a).unwrap());
        assert!(favorites.add(&b).unwrap());
        assert!(!favorites.add(&a).unwrap(), "duplicate is a no-op");
        assert_eq!(favorites.paths(), &[a.clone(), b.clone()]);

        let loaded = Favorites::open(&store).unwrap();
        assert_eq!(loaded.paths(), &[a.clone(), b]);
        assert!(loaded.contains(&a));
    }

    #[test]
    fn rejects_files_and_missing_paths() {
        let temp = tempfile::tempdir().unwrap();
        let mut favorites = Favorites::open(temp.path().join("favorites.json")).unwrap();
        assert!(matches!(
            favorites.add(temp.path().join("missing")),
            Err(CoreError::PathNotFound(_))
        ));
        let file = temp.path().join("file");
        fs::write(&file, b"x").unwrap();
        assert!(matches!(
            favorites.add(&file),
            Err(CoreError::InvalidOperation(_))
        ));
        assert!(favorites.paths().is_empty());
    }

    /// A save that cannot be written must leave the file and the list in front
    /// of the user agreeing. Two properties, one test: the previous favourites
    /// still load, and the failed add did not quietly take effect in memory
    /// only to vanish on the next reopen.
    ///
    /// Worth saying plainly what this does *not* prove: it cannot distinguish
    /// an atomic writer from `fs::write`, because a permission-denied failure
    /// happens at open, before either would truncate. The torn-write case needs
    /// a crash or a full disk, which no test here can stage honestly; what pins
    /// the atomicity itself is `paths::tests`, which can.
    #[test]
    #[cfg(unix)]
    fn a_failed_save_leaves_the_previous_list_intact() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let file = dir.path().join("favorites.json");

        let mut favorites = Favorites::open(&file).unwrap();
        favorites.add(&first).unwrap();
        let saved = fs::read(&file).unwrap();

        // Read-only directory: the file can still be read, no temp can be made.
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let add_result = favorites.add(&second);
        let remove_result = favorites.remove(&first);
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();

        assert!(
            add_result.is_err(),
            "a save into a read-only directory succeeded"
        );
        assert!(
            remove_result.is_err(),
            "a removal reported a success it could not write"
        );
        assert_eq!(
            fs::read(&file).unwrap(),
            saved,
            "a failed write still changed the file"
        );
        assert!(
            !favorites.contains(&second),
            "the failed add was adopted in memory anyway"
        );
        assert!(
            favorites.contains(&first),
            "a failed remove dropped an entry it could not save"
        );

        // And the file is still a list the next open can read.
        let reopened = Favorites::open(&file).unwrap();
        assert_eq!(reopened.paths().to_vec(), vec![first]);
    }

    #[test]
    fn remove_reports_and_persists() {
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join("favorites.json");
        let a = temp.path().join("a");
        fs::create_dir(&a).unwrap();
        let mut favorites = Favorites::open(&store).unwrap();
        favorites.add(&a).unwrap();
        assert!(favorites.remove(&a).unwrap());
        assert!(!favorites.remove(&a).unwrap());
        let loaded = Favorites::open(&store).unwrap();
        assert!(loaded.paths().is_empty());
    }
}
