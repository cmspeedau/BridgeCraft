//! SQLite-backed thumbnail cache.

use std::path::Path;

use rusqlite::{params, Connection};

use crate::{ThumbError, Thumbnail};

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
    /// Open a cache file, creating it and its parent directory if needed.
    ///
    /// Creating the directory is not politeness: every other store in this
    /// application does it (`MetadataIndex::open`, the JSON stores), and a
    /// cache that cannot be opened because `~/.local/share/BridgeCraft` does
    /// not exist yet is a first-launch failure that no test on a machine which
    /// has run the app before can see. Thumbnails silently stop being cached
    /// on a clean profile — the cost is a full re-decode of every image,
    /// forever, with nothing said about it.
    pub fn open(path: &Path) -> Result<Self, ThumbError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    // The path has to be in the message: the application logs
                    // this once and then keeps running without a cache, so a
                    // bare "File exists (os error 17)" tells nobody which file
                    // could not be made or why thumbnails stopped being cached.
                    ThumbError::Io(std::io::Error::new(
                        error.kind(),
                        format!(
                            "could not create the thumbnail cache directory {}: {error}",
                            parent.display()
                        ),
                    ))
                })?;
            }
        }
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
                let thumb = Thumbnail {
                    width,
                    height,
                    pixels,
                };
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
            .query_map([], |row| row.get(0))?
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
    /// A brand-new profile has no data directory yet, and the GUI opens its
    /// cache before any store that would otherwise have created it. Without the
    /// parent-directory step, `open` fails, the app logs once and then re-decodes
    /// every image forever — a first-launch-only failure, invisible to anyone
    /// testing on a machine that has run the app before.
    #[test]
    fn opening_a_cache_creates_its_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("BridgeCraft").join("thumbnails.sqlite");
        assert!(!path.parent().unwrap().exists());

        // An image path, not the cache file's own path: the cache is keyed by
        // the source it thumbnail. Pixels are RGBA, so 2x2 is 16 bytes — a
        // shorter buffer is not a valid thumbnail, and `get` rejects it as a
        // corrupt row (also asserted below), which is why the first version of
        // this test "failed to persist".
        let image = dir.path().join("subject.png");
        std::fs::write(&image, b"png bytes").unwrap();
        let thumb = crate::Thumbnail {
            width: 2,
            height: 2,
            pixels: vec![7u8; 16],
        };
        {
            let cache = CacheStore::open(&path).expect("a new profile's cache dir");
            cache
                .put(&image, 1_700_000_000_000_000_000, 9, 256, &thumb)
                .unwrap();
        }

        assert!(path.exists(), "the cache file was not created");
        let reopened = CacheStore::open(&path).unwrap();
        let read = reopened
            .get(&image, 1_700_000_000_000_000_000, 9, 256)
            .unwrap();
        assert!(
            read.as_ref().map(|t| t.pixels.len()) == Some(16),
            "a cache written into a new directory did not persist: {read:?}"
        );

        // And the corruption guard the test above tripped over, stated on
        // purpose: a row whose byte count does not match its dimensions is a
        // miss, not a crash and not a blank image.
        let bad = dir.path().join("truncated.png");
        std::fs::write(&bad, b"png bytes").unwrap();
        // `put` stores what it is given, which is exactly how a damaged row
        // gets into a table: an older build, a truncated write, or a bug in a
        // decoder. It is the writer's job to be forgiving and the reader's job
        // to be strict.
        reopened
            .put(
                &bad,
                1,
                1,
                256,
                &crate::Thumbnail {
                    width: 2,
                    height: 2,
                    pixels: vec![0u8; 8],
                },
            )
            .unwrap();
        assert!(
            reopened.get(&bad, 1, 1, 256).unwrap().is_none(),
            "an inconsistent row was served as a thumbnail"
        );
    }

    #[test]
    fn opening_a_cache_reports_an_impossible_location() {
        // Creating parents is not the same as creating anything at all: a path
        // under a regular file still has to fail, and say so, rather than
        // leaving the app running uncached with no reason recorded.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"file").unwrap();
        let path = blocker.join("thumbnails.sqlite");
        let message = match CacheStore::open(&path) {
            Ok(_) => panic!("a cache opened under a regular file"),
            Err(error) => error.to_string(),
        };
        assert!(
            message.contains("not-a-dir") && message.contains("cache directory"),
            "the failure has to name the directory it could not create: {message}"
        );
    }

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
        let thumb = Thumbnail {
            width: 1,
            height: 1,
            pixels: vec![0, 0, 0, 255],
        };
        cache.put(Path::new("/a.png"), 1, 10, 64, &thumb).unwrap();
        assert_eq!(cache.get(Path::new("/a.png"), 2, 10, 64).unwrap(), None);
        assert_eq!(cache.get(Path::new("/a.png"), 1, 11, 64).unwrap(), None);
        assert_eq!(cache.get(Path::new("/a.png"), 1, 10, 128).unwrap(), None);
    }

    #[test]
    fn prune_removes_missing_paths() {
        let cache = CacheStore::open_in_memory().unwrap();
        let thumb = Thumbnail {
            width: 1,
            height: 1,
            pixels: vec![0, 0, 0, 255],
        };
        cache
            .put(Path::new("/definitely/missing/x.png"), 1, 1, 64, &thumb)
            .unwrap();
        assert_eq!(cache.prune_missing().unwrap(), 1);
    }
}
