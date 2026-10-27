//! SQLite-backed thumbnail cache.

use std::path::Path;

use rusqlite::{params, Connection};

use crate::{Thumbnail, ThumbError};

/// Persistent thumbnail cache keyed by (path, mtime, size, max_edge).
pub struct CacheStore {
    conn: Connection,
}

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
CREATE TABLE IF NOT EXISTS thumbnails (
    path     TEXT    NOT NULL,
    mtime_ns INTEGER NOT NULL,
    size     INTEGER NOT NULL,
    max_edge INTEGER NOT NULL,
    width    INTEGER NOT NULL,
    height   INTEGER NOT NULL,
    pixels   BLOB    NOT NULL,
    PRIMARY KEY (path, max_edge)
);
";

impl CacheStore {
    /// Open (creating if needed) a cache file.
    pub fn open(path: &Path) -> Result<Self, ThumbError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// In-memory cache, mostly for tests.
    pub fn open_in_memory() -> Result<Self, ThumbError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn get(
        &self,
        path: &Path,
        mtime_ns: i64,
        size: u64,
        max_edge: u32,
    ) -> Result<Option<Thumbnail>, ThumbError> {
        let key = path.to_string_lossy();
        let mut stmt = self.conn.prepare(
            "SELECT width, height, pixels FROM thumbnails
             WHERE path = ?1 AND mtime_ns = ?2 AND size = ?3 AND max_edge = ?4",
        )?;
        let mut rows = stmt.query(params![key, mtime_ns, size, max_edge])?;
        match rows.next()? {
            None => Ok(None),
            Some(row) => {
                let width: u32 = row.get(0)?;
                let height: u32 = row.get(1)?;
                let pixels: Vec<u8> = row.get(2)?;
                let thumb = Thumbnail { width, height, pixels };
                if thumb.is_consistent() {
                    Ok(Some(thumb))
                } else {
                    // Corrupt row: treat as a miss; put() will overwrite it.
                    Ok(None)
                }
            }
        }
    }

    pub fn put(
        &self,
        path: &Path,
        mtime_ns: i64,
        size: u64,
        max_edge: u32,
        thumb: &Thumbnail,
    ) -> Result<(), ThumbError> {
        self.conn.execute(
            "INSERT INTO thumbnails (path, mtime_ns, size, max_edge, width, height, pixels)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(path, max_edge) DO UPDATE SET
                mtime_ns = excluded.mtime_ns,
                size = excluded.size,
                width = excluded.width,
                height = excluded.height,
                pixels = excluded.pixels",
            params![
                path.to_string_lossy(),
                mtime_ns,
                size,
                max_edge,
                thumb.width,
                thumb.height,
                thumb.pixels,
            ],
        )?;
        Ok(())
    }

    /// Remove cache rows whose paths no longer exist on disk.
    /// Returns the number of rows pruned.
    pub fn prune_missing(&self) -> Result<usize, ThumbError> {
        let mut stmt = self.conn.prepare("SELECT DISTINCT path FROM thumbnails")?;
        let paths: Vec<String> = stmt
            .query_map([], |row| row.get(0))?+
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);

        let mut removed = 0;
        for p in paths {
            if !Path::new(&p).exists() {
                self.conn
                    .execute("DELETE FROM thumbnails WHERE path = ?1", params![p])?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_roundtrip() {
        let cache = CacheStore::open_in_memory().unwrap();
        let thumb = Thumbnail {
            width: 2,
            height: 2,
            pixels: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
        };
        cache.put(Path::new("/a.png"), 1, 10, 64, &thumb).unwrap();
        let got = cache.get(Path::new("/a.png"), 1, 10, 64).unwrap();
        assert_eq!(got, Some(thumb));
    }

    #[test]
    fn metadata_change_invalidates() {
        let cache = CacheStore::open_in_memory().unwrap();
        let thumb = Thumbnail { width: 1, height: 1, pixels: vec![0, 0, 0, 255] };
        cache.put(Path::new("/a.png"), 1, 10, 64, &thumb).unwrap();
        assert_eq!(cache.get(Path::new("/a.png"), 2, 10, 64).unwrap(), None);
        assert_eq!(cache.get(Path::new("/a.png"), 1, 11, 64).unwrap(), None);
        assert_eq!(cache.get(Path::new("/a.png"), 1, 10, 128).unwrap(), None);
    }

    #[test]
    fn prune_removes_missing_paths() {
        let cache = CacheStore::open_in_memory().unwrap();
        let thumb = Thumbnail { width: 1, height: 1, pixels: vec![0, 0, 0, 255] };
        cache.put(Path::new("/definitely/missing/x.png"), 1, 1, 64, &thumb).unwrap();
        assert_eq!(cache.prune_missing().unwrap(), 1);
    }
}
