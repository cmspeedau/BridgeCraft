//! Per-user persistent list of favorite folders, mirrored from the browser's
//! favorites sidebar concept (a named, ordered, JSON-backed list).

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{recents::user_data_dir, CoreError, Result};

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
        Self::open(user_data_dir()?.join("BridgeCraft").join(FILE_NAME))
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
        self.paths.push(path.to_path_buf());
        self.persist()?;
        Ok(true)
    }

    /// Remove `path` from the list; reports whether anything was removed.
    pub fn remove(&mut self, path: impl AsRef<Path>) -> Result<bool> {
        let before = self.paths.len();
        self.paths.retain(|existing| existing != path.as_ref());
        let removed = self.paths.len() != before;
        if removed {
            self.persist()?;
        }
        Ok(removed)
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
            CoreError::InvalidOperation(format!("could not encode favorites: {error}"))
        })?;
        fs::write(&self.file, bytes)?;
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
