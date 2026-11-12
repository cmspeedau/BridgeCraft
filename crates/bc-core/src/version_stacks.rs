//! Version stacks for assets (roadmap Phase 5).
//!
//! Groups multiple files that represent versions of the same asset into a
//! stack with one primary (top) member. Stacks persist as versioned JSON in
//! the user data directory, mirroring `Favorites`/`Recents`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{CoreError, Result};

const FILE_NAME: &str = "version-stacks.json";
const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VersionStack {
    pub id: String,
    pub name: Option<String>,
    pub primary: PathBuf,
    pub members: Vec<PathBuf>,
    pub schema_version: u32,
    pub created_unix: u64,
    pub modified_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedStack {
    id: String,
    name: Option<String>,
    primary: String,
    members: Vec<String>,
    schema_version: u32,
    created_unix: u64,
    modified_unix: u64,
}

#[derive(Debug)]
pub struct VersionStacks {
    file: PathBuf,
    stacks: Vec<VersionStack>,
}

impl VersionStacks {
    pub fn open_default() -> Result<Self> {
        Self::open(
            crate::recents::user_data_dir()?
                .join("BridgeCraft")
                .join(FILE_NAME),
        )
    }

    pub fn open(file: impl Into<PathBuf>) -> Result<Self> {
        let file = file.into();
        let stacks = match fs::read(&file) {
            Ok(bytes) => {
                let persisted: Vec<PersistedStack> =
                    serde_json::from_slice(&bytes).map_err(|e| {
                        CoreError::InvalidOperation(format!("invalid version stacks: {e}"))
                    })?;
                persisted
                    .into_iter()
                    .map(|p| VersionStack {
                        id: p.id,
                        name: p.name,
                        primary: PathBuf::from(p.primary),
                        members: p.members.into_iter().map(PathBuf::from).collect(),
                        schema_version: p.schema_version,
                        created_unix: p.created_unix,
                        modified_unix: p.modified_unix,
                    })
                    .collect()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { file, stacks })
    }

    pub fn list(&self) -> &[VersionStack] {
        &self.stacks
    }

    pub fn get(&self, id: &str) -> Option<&VersionStack> {
        self.stacks.iter().find(|s| s.id == id)
    }

    pub fn stacks_for(&self, path: &Path) -> Vec<&VersionStack> {
        self.stacks
            .iter()
            .filter(|s| s.members.iter().any(|m| m == path))
            .collect()
    }

    pub fn is_stacked(&self, path: &Path) -> bool {
        self.stacks
            .iter()
            .any(|s| s.members.iter().any(|m| m == path))
    }

    pub fn primary_of(&self, path: &Path) -> Option<PathBuf> {
        self.stacks
            .iter()
            .find(|s| s.members.iter().any(|m| m == path))
            .map(|s| s.primary.clone())
    }

    pub fn create(&mut self, paths: &[PathBuf]) -> Result<VersionStack> {
        if paths.len() < 2 {
            return Err(CoreError::InvalidOperation(
                "a version stack needs at least two members".into(),
            ));
        }
        // de-dup preserving order
        let mut members: Vec<PathBuf> = Vec::new();
        for p in paths {
            if !members.iter().any(|m| m == p) {
                members.push(p.clone());
            }
        }
        if members.len() < 2 {
            return Err(CoreError::InvalidOperation(
                "a version stack needs at least two distinct members".into(),
            ));
        }
        let now = now_unix();
        let stack = VersionStack {
            id: format!("vs-{}", now),
            name: None,
            primary: members[0].clone(),
            members,
            schema_version: SCHEMA_VERSION,
            created_unix: now,
            modified_unix: now,
        };
        self.stacks.push(stack.clone());
        self.persist()?;
        Ok(stack)
    }

    pub fn add(&mut self, id: &str, path: &Path) -> Result<bool> {
        let now = now_unix();
        let Some(stack) = self.stacks.iter_mut().find(|s| s.id == id) else {
            return Ok(false);
        };
        if stack.members.iter().any(|m| m == path) {
            return Ok(false);
        }
        stack.members.push(path.to_path_buf());
        stack.modified_unix = now;
        self.persist()?;
        Ok(true)
    }

    pub fn remove(&mut self, id: &str, path: &Path) -> Result<bool> {
        let now = now_unix();
        let Some(stack) = self.stacks.iter_mut().find(|s| s.id == id) else {
            return Ok(false);
        };
        let before = stack.members.len();
        stack.members.retain(|m| m != path);
        if stack.members.len() == before {
            return Ok(false);
        }
        if stack.members.is_empty() {
            // delete stack
            self.stacks.retain(|s| s.id != id);
            self.persist()?;
            return Ok(true);
        }
        if stack.primary == *path {
            stack.primary = stack.members[0].clone();
        }
        stack.modified_unix = now;
        self.persist()?;
        Ok(true)
    }

    pub fn set_primary(&mut self, id: &str, path: &Path) -> Result<bool> {
        let now = now_unix();
        let Some(stack) = self.stacks.iter_mut().find(|s| s.id == id) else {
            return Ok(false);
        };
        if !stack.members.iter().any(|m| m == path) {
            return Ok(false);
        }
        stack.primary = path.to_path_buf();
        stack.modified_unix = now;
        self.persist()?;
        Ok(true)
    }

    pub fn set_name(&mut self, id: &str, name: Option<String>) -> Result<bool> {
        let now = now_unix();
        let Some(stack) = self.stacks.iter_mut().find(|s| s.id == id) else {
            return Ok(false);
        };
        stack.name = name;
        stack.modified_unix = now;
        self.persist()?;
        Ok(true)
    }

    pub fn delete(&mut self, id: &str) -> Result<bool> {
        let before = self.stacks.len();
        self.stacks.retain(|s| s.id != id);
        let removed = self.stacks.len() != before;
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }

    pub fn collapse(&mut self, _path: &Path) {
        // UI hint only; no state change required.
    }

    fn persist(&self) -> Result<()> {
        if let Some(parent) = self.file.parent() {
            fs::create_dir_all(parent)?;
        }
        let persisted: Vec<PersistedStack> = self
            .stacks
            .iter()
            .map(|s| PersistedStack {
                id: s.id.clone(),
                name: s.name.clone(),
                primary: s.primary.to_string_lossy().into_owned(),
                members: s
                    .members
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect(),
                schema_version: s.schema_version,
                created_unix: s.created_unix,
                modified_unix: s.modified_unix,
            })
            .collect();
        let bytes = serde_json::to_vec_pretty(&persisted).map_err(|e| {
            CoreError::InvalidOperation(format!("could not encode version stacks: {e}"))
        })?;
        fs::write(&self.file, bytes)?;
        Ok(())
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// Aliases
pub type VersionStackStore = VersionStacks;
pub type VersionSet = VersionStack;
pub type VersionSets = VersionStacks;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn create_persist_and_reload() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("vs.json");
        let a = temp.path().join("a.jpg");
        let b = temp.path().join("a_v2.jpg");
        fs::write(&a, b"x").unwrap();
        fs::write(&b, b"x").unwrap();
        let mut store = VersionStacks::open(file.clone()).unwrap();
        let stack = store.create(&[a.clone(), b.clone()]).unwrap();
        assert_eq!(stack.primary, a);
        assert_eq!(stack.members.len(), 2);

        let reloaded = VersionStacks::open(file).unwrap();
        assert_eq!(reloaded.list().len(), 1);
        assert_eq!(reloaded.list()[0].members.len(), 2);
    }

    #[test]
    fn primary_promotion_on_remove() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("vs.json");
        let a = temp.path().join("a.jpg");
        let b = temp.path().join("b.jpg");
        fs::write(&a, b"x").unwrap();
        fs::write(&b, b"x").unwrap();
        let mut store = VersionStacks::open(file).unwrap();
        let stack = store.create(&[a.clone(), b.clone()]).unwrap();
        store.remove(&stack.id, &a).unwrap();
        let s = store.get(&stack.id).unwrap();
        assert_eq!(s.primary, b);
        assert_eq!(s.members, vec![b.clone()]);
    }

    #[test]
    fn delete_stack() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("vs.json");
        let a = temp.path().join("a.jpg");
        let b = temp.path().join("b.jpg");
        fs::write(&a, b"x").unwrap();
        fs::write(&b, b"x").unwrap();
        let mut store = VersionStacks::open(file).unwrap();
        let stack = store.create(&[a, b]).unwrap();
        assert!(store.delete(&stack.id).unwrap());
        assert!(store.list().is_empty());
    }

    #[test]
    fn needs_two_members() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("vs.json");
        let a = temp.path().join("a.jpg");
        std::fs::write(&a, b"x").unwrap();
        let mut store = VersionStacks::open(file).unwrap();
        assert!(store.create(&[a]).is_err());
    }

    #[test]
    fn stacks_for_path() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("vs.json");
        let a = temp.path().join("a.jpg");
        let b = temp.path().join("b.jpg");
        fs::write(&a, b"x").unwrap();
        fs::write(&b, b"x").unwrap();
        let mut store = VersionStacks::open(file).unwrap();
        store.create(&[a.clone(), b.clone()]).unwrap();
        assert!(store.is_stacked(&a));
        assert_eq!(store.primary_of(&b), Some(a.clone()));
        assert_eq!(store.stacks_for(&a).len(), 1);
    }
}
