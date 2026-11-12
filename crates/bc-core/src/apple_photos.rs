//! Apple Photos read-only integration (roadmap Phase 5).
//!
//! macOS-only in spirit: locates the user's `Photos Library.photoslibrary`
//! and opens its `Photos.sqlite` read-only. On other platforms the locator
//! returns `None` and the provider is simply unavailable. Enumeration is
//! best-effort against the documented Core Data schema; unknown schemas
//! yield empty results rather than errors.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::virtual_folders::{VirtualFolderProvider, VirtualNode};
use crate::{CoreError, Result};

pub fn default_library_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            let pics = PathBuf::from(home).join("Pictures");
            let candidates = [
                pics.join("Photos Library.photoslibrary"),
                pics.join("Photos Library.photoslibrary/database/Photos.sqlite")
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_default(),
            ];
            for c in candidates {
                let db = c.join("database/Photos.sqlite");
                if db.exists() {
                    return Some(c);
                }
                if c.ends_with("Photos.sqlite") && c.exists() {
                    return Some(c.parent()?.to_path_buf());
                }
            }
        }
        None
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[derive(Debug, Clone)]
pub struct PhotoAlbum {
    pub id: i64,
    pub title: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PhotoAsset {
    pub id: i64,
    pub filename: String,
}

pub struct PhotoLibrary {
    conn: Connection,
}

impl PhotoLibrary {
    pub fn open(path: &Path) -> Result<Self> {
        let db = if path.ends_with("Photos.sqlite") {
            path.to_path_buf()
        } else {
            path.join("database/Photos.sqlite")
        };
        let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| CoreError::InvalidOperation(e.to_string()))?;
        Ok(Self { conn })
    }

    pub fn albums(&self) -> Result<Vec<PhotoAlbum>> {
        let mut stmt = match self
            .conn
            .prepare("SELECT Z_PK, COALESCE(ZTITLE, '') FROM ZGENERICALBUM WHERE ZKIND = 2")
        {
            Ok(s) => s,
            Err(_) => return Ok(Vec::new()),
        };
        let rows = stmt
            .query_map([], |row| {
                Ok(PhotoAlbum {
                    id: row.get(0)?,
                    title: row.get(1)?,
                })
            })
            .map_err(|e| CoreError::InvalidOperation(e.to_string()))?;
        let mut out = Vec::new();
        for a in rows.flatten() {
            out.push(a);
        }
        Ok(out)
    }

    pub fn moments(&self) -> Result<Vec<PhotoAlbum>> {
        let mut stmt = match self
            .conn
            .prepare("SELECT Z_PK, COALESCE(ZTITLE, '') FROM ZMOMENT")
        {
            Ok(s) => s,
            Err(_) => return Ok(Vec::new()),
        };
        let rows = stmt
            .query_map([], |row| {
                Ok(PhotoAlbum {
                    id: row.get(0)?,
                    title: row.get(1)?,
                })
            })
            .map_err(|e| CoreError::InvalidOperation(e.to_string()))?;
        let mut out = Vec::new();
        for a in rows.flatten() {
            out.push(a);
        }
        Ok(out)
    }
}

pub struct PhotoLibraryProvider {
    source_id: String,
    display_name: String,
    library: Option<PhotoLibrary>,
}

impl PhotoLibraryProvider {
    pub fn new(source_id: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            source_id: source_id.into(),
            display_name: display_name.into(),
            library: None,
        }
    }

    pub fn with_library(mut self, library: PhotoLibrary) -> Self {
        self.library = Some(library);
        self
    }

    pub fn open_default() -> Self {
        let mut p = Self::new("apple_photos", "Apple Photos");
        if let Some(path) = default_library_path() {
            if let Ok(lib) = PhotoLibrary::open(&path) {
                p.library = Some(lib);
            }
        }
        p
    }
}

impl VirtualFolderProvider for PhotoLibraryProvider {
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
            children: Vec::new(),
        }
    }

    fn children(&self, path: &[String]) -> Result<Vec<VirtualNode>> {
        // Only top-level enumeration is supported; deeper navigation returns empty.
        if path.len() > 1 {
            return Ok(Vec::new());
        }
        let Some(lib) = &self.library else {
            return Ok(Vec::new());
        };
        let mut nodes = Vec::new();
        for album in lib.albums().unwrap_or_default() {
            nodes.push(VirtualNode::Folder {
                id: format!("album:{}", album.id),
                name: if album.title.is_empty() {
                    format!("Album {}", album.id)
                } else {
                    album.title
                },
                source: self.source_id.clone(),
                children: Vec::new(),
            });
        }
        for moment in lib.moments().unwrap_or_default() {
            nodes.push(VirtualNode::Folder {
                id: format!("moment:{}", moment.id),
                name: if moment.title.is_empty() {
                    format!("Moment {}", moment.id)
                } else {
                    moment.title
                },
                source: self.source_id.clone(),
                children: Vec::new(),
            });
        }
        Ok(nodes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn open_missing_returns_error() {
        let p = std::path::Path::new("/definitely/missing.photoslibrary");
        assert!(PhotoLibrary::open(p).is_err());
    }

    #[test]
    fn albums_from_synthetic_db() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("Photos.sqlite");
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "CREATE TABLE ZGENERICALBUM (Z_PK INTEGER PRIMARY KEY, ZTITLE TEXT, ZKIND INTEGER)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ZGENERICALBUM (Z_PK, ZTITLE, ZKIND) VALUES (1, 'Vacation', 2)",
            [],
        )
        .unwrap();
        drop(conn);
        let lib = PhotoLibrary::open(&db).unwrap();
        let albums = lib.albums().unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].title, "Vacation");
    }

    #[test]
    fn provider_lists_albums() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("Photos.sqlite");
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "CREATE TABLE ZGENERICALBUM (Z_PK INTEGER PRIMARY KEY, ZTITLE TEXT, ZKIND INTEGER)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ZGENERICALBUM (Z_PK, ZTITLE, ZKIND) VALUES (1, 'Vacation', 2)",
            [],
        )
        .unwrap();
        drop(conn);
        let lib = PhotoLibrary::open(&db).unwrap();
        let provider = PhotoLibraryProvider::new("photos", "Apple Photos").with_library(lib);
        let kids = provider.children(&["photos".into()]).unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].name(), "Vacation");
    }
}
