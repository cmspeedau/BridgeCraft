//! Version stacks for assets (roadmap Phase 5).
//!
//! Groups multiple files that represent versions of the same asset into a stack
//! with one primary (top) member. A stack is a user-authored record — nothing
//! else holds which file is the primary of a set — so it is kept the same way
//! `saved-searches.json` and `favorites.json` are: a versioned document in the
//! data directory, written atomically, reported but never overwritten when it
//! cannot be read.
//!
//! The format is a document rather than one file per stack. A version stack is
//! small, a stack library is small, and the single-document shape is the one
//! this crate already knows how to make durable; a per-stack file would only
//! duplicate that machinery for portability nobody has asked for.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{CoreError, Result};

const FILE_NAME: &str = "version-stacks.json";

/// Bumping this is a deliberate act: the reader refuses anything it does not
/// understand rather than guessing, so a new version needs a migration beside
/// it, not just a bigger number.
const DOCUMENT_VERSION: u32 = 1;

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

#[derive(Debug, Serialize, Deserialize)]
struct PersistedStack {
    id: String,
    name: Option<String>,
    primary: String,
    members: Vec<String>,
    schema_version: u32,
    created_unix: u64,
    modified_unix: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Document {
    version: u32,
    stacks: Vec<PersistedStack>,
}

#[derive(Debug, Clone)]
pub struct VersionStacks {
    file: PathBuf,
    stacks: Vec<VersionStack>,
}

impl VersionStacks {
    pub fn open_default() -> Result<Self> {
        Self::open(crate::paths::data_file(FILE_NAME)?)
    }

    /// Open a version-stack store at `file`.
    ///
    /// A file that will not parse is reported and left alone. Nothing here
    /// overwrites it: a stack records which files a user grouped together,
    /// nothing else holds that, and a torn write is a lost grouping rather than
    /// a cache miss.
    pub fn open(file: impl Into<PathBuf>) -> Result<Self> {
        let file = file.into();
        let stacks = match fs::read(&file) {
            Ok(bytes) => {
                let text = String::from_utf8(bytes).map_err(|error| {
                    CoreError::InvalidOperation(format!(
                        "{} is not valid text, so the version stacks in it cannot be read: {error}",
                        file.display()
                    ))
                })?;
                parse_document(&text, &file)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self { file, stacks })
    }

    /// The document these stacks live in, named because the only useful thing an
    /// interface can say about an unreadable store is where to look at it.
    pub fn path(&self) -> &Path {
        &self.file
    }

    pub fn list(&self) -> &[VersionStack] {
        &self.stacks
    }

    pub fn get(&self, id: &str) -> Option<&VersionStack> {
        self.stacks.iter().find(|stack| stack.id == id)
    }

    pub fn stacks_for(&self, path: &Path) -> Vec<&VersionStack> {
        self.stacks
            .iter()
            .filter(|stack| stack.members.iter().any(|member| member == path))
            .collect()
    }

    pub fn is_stacked(&self, path: &Path) -> bool {
        self.stacks
            .iter()
            .any(|stack| stack.members.iter().any(|member| member == path))
    }

    pub fn primary_of(&self, path: &Path) -> Option<PathBuf> {
        self.stacks
            .iter()
            .find(|stack| stack.members.iter().any(|member| member == path))
            .map(|stack| stack.primary.clone())
    }

    pub fn create(&mut self, paths: &[PathBuf]) -> Result<VersionStack> {
        if paths.len() < 2 {
            return Err(CoreError::InvalidOperation(
                "a version stack needs at least two members".into(),
            ));
        }
        let mut members: Vec<PathBuf> = Vec::new();
        for path in paths {
            if !members.iter().any(|member| member == path) {
                members.push(path.clone());
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
            schema_version: DOCUMENT_VERSION,
            created_unix: now,
            modified_unix: now,
        };

        // Written before it is adopted, so a failed persist cannot leave the
        // session showing a stack that is not on disk. This is the same rule the
        // favourites and saved-searches stores follow, for the same reason.
        let mut candidate = self.stacks.clone();
        candidate.push(stack.clone());
        persist(&self.file, &candidate)?;
        self.stacks = candidate;
        Ok(stack)
    }

    pub fn add(&mut self, id: &str, path: &Path) -> Result<bool> {
        let now = now_unix();
        let Some(stack) = self.stacks.iter().find(|stack| stack.id == id) else {
            return Ok(false);
        };
        if stack.members.iter().any(|member| member == path) {
            return Ok(false);
        }
        let mut candidate = self.stacks.clone();
        let Some(stack) = candidate.iter_mut().find(|stack| stack.id == id) else {
            return Ok(false);
        };
        stack.members.push(path.to_path_buf());
        stack.modified_unix = now;
        persist(&self.file, &candidate)?;
        self.stacks = candidate;
        Ok(true)
    }

    pub fn remove(&mut self, id: &str, path: &Path) -> Result<bool> {
        let now = now_unix();
        let Some(stack) = self.stacks.iter().find(|stack| stack.id == id) else {
            return Ok(false);
        };
        if !stack.members.iter().any(|member| member == path) {
            return Ok(false);
        }
        let mut candidate = self.stacks.clone();
        let Some(stack) = candidate.iter_mut().find(|stack| stack.id == id) else {
            return Ok(false);
        };
        let before = stack.members.len();
        stack.members.retain(|member| member != path);
        if stack.members.len() == before {
            return Ok(false);
        }
        if stack.members.is_empty() {
            candidate.retain(|stack| stack.id != id);
            persist(&self.file, &candidate)?;
            self.stacks = candidate;
            return Ok(true);
        }
        if stack.primary == *path {
            stack.primary = stack.members[0].clone();
        }
        stack.modified_unix = now;
        persist(&self.file, &candidate)?;
        self.stacks = candidate;
        Ok(true)
    }

    pub fn set_primary(&mut self, id: &str, path: &Path) -> Result<bool> {
        let now = now_unix();
        let Some(stack) = self.stacks.iter().find(|stack| stack.id == id) else {
            return Ok(false);
        };
        if !stack.members.iter().any(|member| member == path) {
            return Ok(false);
        }
        let mut candidate = self.stacks.clone();
        let Some(stack) = candidate.iter_mut().find(|stack| stack.id == id) else {
            return Ok(false);
        };
        stack.primary = path.to_path_buf();
        stack.modified_unix = now;
        persist(&self.file, &candidate)?;
        self.stacks = candidate;
        Ok(true)
    }

    pub fn set_name(&mut self, id: &str, name: Option<String>) -> Result<bool> {
        let now = now_unix();
        let Some(stack) = self.stacks.iter().find(|stack| stack.id == id) else {
            return Ok(false);
        };
        let _ = stack;
        let mut candidate = self.stacks.clone();
        let Some(stack) = candidate.iter_mut().find(|stack| stack.id == id) else {
            return Ok(false);
        };
        stack.name = name;
        stack.modified_unix = now;
        persist(&self.file, &candidate)?;
        self.stacks = candidate;
        Ok(true)
    }

    pub fn delete(&mut self, id: &str) -> Result<bool> {
        let before = self.stacks.len();
        let mut candidate = self.stacks.clone();
        candidate.retain(|stack| stack.id != id);
        if candidate.len() == before {
            return Ok(false);
        }
        persist(&self.file, &candidate)?;
        self.stacks = candidate;
        Ok(true)
    }

    pub fn collapse(&mut self, _path: &Path) {
        // UI hint only; no state change required.
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn parse_document(text: &str, file: &Path) -> Result<Vec<VersionStack>> {
    let document: Document = serde_json::from_str(text).map_err(|error| {
        CoreError::InvalidOperation(format!(
            "{} could not be read ({error}); nothing will overwrite it until it is \
             fixed or removed",
            file.display()
        ))
    })?;
    if document.version != DOCUMENT_VERSION {
        return Err(CoreError::InvalidOperation(format!(
            "{} holds version stacks written by version {}, and this build reads version {} \
             only",
            file.display(),
            document.version,
            DOCUMENT_VERSION
        )));
    }
    Ok(document
        .stacks
        .into_iter()
        .map(persisted_to_stack)
        .collect())
}

fn persisted_to_stack(persisted: PersistedStack) -> VersionStack {
    VersionStack {
        id: persisted.id,
        name: persisted.name,
        primary: PathBuf::from(persisted.primary),
        members: persisted.members.into_iter().map(PathBuf::from).collect(),
        schema_version: persisted.schema_version,
        created_unix: persisted.created_unix,
        modified_unix: persisted.modified_unix,
    }
}

fn stack_to_persisted(stack: &VersionStack) -> PersistedStack {
    PersistedStack {
        id: stack.id.clone(),
        name: stack.name.clone(),
        primary: stack.primary.to_string_lossy().into_owned(),
        members: stack
            .members
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        schema_version: stack.schema_version,
        created_unix: stack.created_unix,
        modified_unix: stack.modified_unix,
    }
}

fn persist(file: &Path, entries: &[VersionStack]) -> Result<()> {
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)?;
    }
    let document = Document {
        version: DOCUMENT_VERSION,
        stacks: entries.iter().map(stack_to_persisted).collect(),
    };
    let bytes = serde_json::to_vec_pretty(&document).map_err(|error| {
        CoreError::InvalidOperation(format!("could not encode the version stacks: {error}"))
    })?;
    crate::paths::write_atomic(file, &bytes).map_err(CoreError::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn store(dir: &Path) -> VersionStacks {
        VersionStacks::open(dir.join(FILE_NAME)).unwrap()
    }

    #[test]
    fn a_missing_file_is_an_empty_list_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let saved = store(dir.path());
        assert!(saved.list().is_empty());
        assert!(
            !saved.path().exists(),
            "opening created a file it does not own"
        );
    }

    #[test]
    fn a_stack_survives_reopening_the_store() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("vs.json");
        let a = temp.path().join("a.jpg");
        let b = temp.path().join("a_v2.jpg");
        fs::write(&a, b"x").unwrap();
        fs::write(&b, b"x").unwrap();
        let mut saved = VersionStacks::open(file.clone()).unwrap();
        let stack = saved.create(&[a.clone(), b.clone()]).unwrap();
        assert_eq!(stack.primary, a);
        assert_eq!(stack.members.len(), 2);

        let reloaded = VersionStacks::open(file).unwrap();
        assert_eq!(reloaded.list().len(), 1);
        assert_eq!(reloaded.list()[0].members.len(), 2);
        assert_eq!(reloaded.list()[0].primary, a);
    }

    #[test]
    fn primary_promotion_on_remove() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("vs.json");
        let a = temp.path().join("a.jpg");
        let b = temp.path().join("b.jpg");
        fs::write(&a, b"x").unwrap();
        fs::write(&b, b"x").unwrap();
        let mut saved = VersionStacks::open(file).unwrap();
        let stack = saved.create(&[a.clone(), b.clone()]).unwrap();
        assert!(saved.remove(&stack.id, &a).unwrap());
        let s = saved.get(&stack.id).unwrap();
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
        let mut saved = VersionStacks::open(file).unwrap();
        let stack = saved.create(&[a, b]).unwrap();
        assert!(saved.delete(&stack.id).unwrap());
        assert!(saved.list().is_empty());
    }

    #[test]
    fn needs_two_members() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("vs.json");
        let a = temp.path().join("a.jpg");
        std::fs::write(&a, b"x").unwrap();
        let mut saved = VersionStacks::open(file).unwrap();
        assert!(saved.create(&[a]).is_err());
    }

    #[test]
    fn stacks_for_path() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("vs.json");
        let a = temp.path().join("a.jpg");
        let b = temp.path().join("b.jpg");
        fs::write(&a, b"x").unwrap();
        fs::write(&b, b"x").unwrap();
        let mut saved = VersionStacks::open(file).unwrap();
        saved.create(&[a.clone(), b.clone()]).unwrap();
        assert!(saved.is_stacked(&a));
        assert_eq!(saved.primary_of(&b), Some(a.clone()));
        assert_eq!(saved.stacks_for(&a).len(), 1);
    }

    #[test]
    fn an_unreadable_store_is_reported_and_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE_NAME);
        std::fs::write(
            &file,
            r#"{"version":1,"stacks":[{"id":"one","name":null,"primary":"/tmp/a.jpg","members":["/tmp/a.jpg","/tmp/b.jpg"],"schema_version":1,"created_unix":1,"modified_unix":1}]} leftover"#,
        )
        .unwrap();
        let before = std::fs::read(&file).unwrap();

        let error = VersionStacks::open(&file).unwrap_err().to_string();
        assert!(error.contains("could not be read"), "{error}");
        assert!(
            error.contains(&file.display().to_string()),
            "the message has to say which file to look at: {error}"
        );
        assert_eq!(
            std::fs::read(&file).unwrap(),
            before,
            "a failed open still rewrote the user's templates"
        );
    }

    #[test]
    fn a_document_from_a_future_version_is_refused_rather_than_guessed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE_NAME);
        std::fs::write(&file, r#"{"version":2,"stacks":[]}"#).unwrap();
        let error = VersionStacks::open(&file).unwrap_err().to_string();
        assert!(error.contains("version 2"), "{error}");
        assert!(file.exists(), "the refused file was deleted");
    }

    #[test]
    fn a_stack_created_by_a_failed_persist_is_not_adopted() {
        // The write-before-adopt rule: a stack that could not be written must
        // not appear in the list the user is looking at, because the next reopen
        // would drop it and the two would silently disagree.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let a = dir.path().join("a.jpg");
            let b = dir.path().join("b.jpg");
            std::fs::write(&a, b"x").unwrap();
            std::fs::write(&b, b"x").unwrap();
            let file = dir.path().join(FILE_NAME);

            let mut store = VersionStacks::open(file).unwrap();
            std::fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
            let result = store.create(&[a, b]);
            std::fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();

            assert!(
                result.is_err(),
                "create into a read-only directory succeeded"
            );
            assert!(
                store.list().is_empty(),
                "the failed create was adopted in memory"
            );
        }
    }
}
