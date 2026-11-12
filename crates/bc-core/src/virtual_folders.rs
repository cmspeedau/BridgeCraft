//! Virtual folders (roadmap Phase 5 — Apple Photos moments/albums, and other
//! virtual sources such as smart collections).
//!
//! A virtual folder is a node in the browser tree that is not backed by a
//! single on-disk directory. Providers (Apple Photos, smart collections,
//! search results) implement [`VirtualFolderProvider`] and the UI walks the
//! resulting [`VirtualNode`] tree. Originals and edited renditions surface as
//! [`VirtualNode::Entry`] items with their backing paths.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{CoreError, Result};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VirtualEntry {
    pub id: String,
    pub name: String,
    pub path: Option<PathBuf>,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified_unix: Option<u64>,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum VirtualNode {
    Folder {
        id: String,
        name: String,
        source: String,
        children: Vec<VirtualNode>,
    },
    Entry(VirtualEntry),
}

impl VirtualNode {
    pub fn name(&self) -> &str {
        match self {
            VirtualNode::Folder { name, .. } => name,
            VirtualNode::Entry(e) => &e.name,
        }
    }

    pub fn is_dir(&self) -> bool {
        matches!(self, VirtualNode::Folder { .. })
    }

    pub fn id(&self) -> &str {
        match self {
            VirtualNode::Folder { id, .. } => id,
            VirtualNode::Entry(e) => &e.id,
        }
    }

    pub fn source(&self) -> &str {
        match self {
            VirtualNode::Folder { source, .. } => source,
            VirtualNode::Entry(e) => &e.source,
        }
    }
}

pub trait VirtualFolderProvider {
    fn source_id(&self) -> &str;
    fn display_name(&self) -> &str;
    fn root(&self) -> VirtualNode;
    fn children(&self, path: &[String]) -> Result<Vec<VirtualNode>>;
    fn resolve(&self, id: &str) -> Option<VirtualEntry> {
        let _ = id;
        None
    }
}

#[derive(Default)]
pub struct VirtualFolderRegistry {
    providers: Vec<Box<dyn VirtualFolderProvider>>,
}

impl VirtualFolderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, provider: Box<dyn VirtualFolderProvider>) {
        self.providers.push(provider);
    }

    pub fn providers(&self) -> &[Box<dyn VirtualFolderProvider>] {
        &self.providers
    }

    pub fn roots(&self) -> Vec<VirtualNode> {
        self.providers
            .iter()
            .map(|p| VirtualNode::Folder {
                id: format!("vf:{}", p.source_id()),
                name: p.display_name().to_string(),
                source: p.source_id().to_string(),
                children: Vec::new(),
            })
            .collect()
    }

    pub fn find_provider(&self, source_id: &str) -> Option<&dyn VirtualFolderProvider> {
        self.providers
            .iter()
            .find(|p| p.source_id() == source_id)
            .map(|p| p.as_ref())
    }
}

// In-memory provider useful for tests and for grouping search results.
pub struct InMemoryVirtualProvider {
    source_id: String,
    display_name: String,
    root_children: Vec<VirtualNode>,
}

impl InMemoryVirtualProvider {
    pub fn new(source_id: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            source_id: source_id.into(),
            display_name: display_name.into(),
            root_children: Vec::new(),
        }
    }

    pub fn with_children(mut self, children: Vec<VirtualNode>) -> Self {
        self.root_children = children;
        self
    }
}

impl VirtualFolderProvider for InMemoryVirtualProvider {
    fn source_id(&self) -> &str {
        &self.source_id
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    fn root(&self) -> VirtualNode {
        VirtualNode::Folder {
            id: format!("vf:{}", self.source_id),
            name: self.display_name.clone(),
            source: self.source_id.clone(),
            children: self.root_children.clone(),
        }
    }

    fn children(&self, path: &[String]) -> Result<Vec<VirtualNode>> {
        // Simple path walk over the in-memory tree.
        let mut current = &self.root_children;
        // path[0] is the root provider itself; skip.
        for segment in path.iter().skip(1) {
            let next = current.iter().find_map(|n| match n {
                VirtualNode::Folder { name, children, .. } if name == segment => Some(children),
                _ => None,
            });
            match next {
                Some(c) => current = c,
                None => {
                    return Err(CoreError::PathNotFound(format!(
                        "virtual folder not found: {}",
                        segment
                    )))
                }
            }
        }
        Ok(current.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_provider_walks() {
        let child = VirtualNode::Entry(VirtualEntry {
            id: "e1".into(),
            name: "photo.jpg".into(),
            path: Some(std::path::PathBuf::from("/tmp/photo.jpg")),
            is_dir: false,
            size: Some(10),
            modified_unix: None,
            source: "photos".into(),
        });
        let folder = VirtualNode::Folder {
            id: "f1".into(),
            name: "Moments".into(),
            source: "photos".into(),
            children: vec![child.clone()],
        };
        let provider = InMemoryVirtualProvider::new("photos", "Apple Photos")
            .with_children(vec![folder.clone()]);
        let reg = {
            let mut r = VirtualFolderRegistry::new();
            r.register(Box::new(provider));
            r
        };
        assert_eq!(reg.roots().len(), 1);
        let p = reg.find_provider("photos").unwrap();
        let kids = p.children(&["photos".into(), "Moments".into()]).unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].name(), "photo.jpg");
    }

    #[test]
    fn node_helpers() {
        let n = VirtualNode::Folder {
            id: "x".into(),
            name: "X".into(),
            source: "s".into(),
            children: vec![],
        };
        assert!(n.is_dir());
        assert_eq!(n.name(), "X");
        assert_eq!(n.source(), "s");
    }
}
