//! Recursive filesystem watcher backed by the `notify` crate.

use crate::{CoreError, Result};

/// Watches a directory tree and buffers events until polled.
pub struct Watcher {
    // Kept alive for the lifetime of the watcher; dropping it stops delivery.
    _watcher: notify::RecommendedWatcher,
    rx: std::sync::mpsc::Receiver<std::result::Result<notify::Event, notify::Error>>,
}

impl Watcher {
    pub fn new(root: &std::path::Path) -> Result<Self> {
        if !root.exists() {
            return Err(CoreError::PathNotFound(root.display().to_string()));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = notify::recommended_watcher(tx)
            .map_err(|e| CoreError::Io(std::io::Error::other(e)))?;
        notify::Watcher::watch(&mut watcher, root, notify::RecursiveMode::Recursive)
            .map_err(|e| CoreError::Io(std::io::Error::other(e)))?;
        Ok(Self { _watcher: watcher, rx })
    }

    /// Non-blocking poll of the next pending event, if any.
    pub fn try_recv(&self) -> Option<std::result::Result<notify::Event, notify::Error>> {
        self.rx.try_recv().ok()
    }

    /// Drain all currently pending events.
    pub fn drain(&self) -> Vec<std::result::Result<notify::Event, notify::Error>> {
        let mut events = Vec::new();
        while let Some(ev) = self.try_recv() {
            events.push(ev);
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watcher_reports_changes() {
        let dir = tempfile::tempdir().unwrap();
        let w = Watcher::new(dir.path()).unwrap();
        // Give the OS watcher a moment, then touch a file.
        std::thread::sleep(std::time::Duration::from_millis(100));
        std::fs::write(dir.path().join("new.txt"), b"x").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut seen = false;
        while std::time::Instant::now() < deadline {
            if w.drain().iter().any(|result| {
                let e = result.as_ref().expect("watcher reported an event error");
                e.paths
                    .iter()
                    .any(|p| p.file_name().is_some_and(|n| n == "new.txt"))
            }) {
                seen = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(seen, "watcher never reported the new file");
    }

    #[test]
    fn watcher_missing_root_errors() {
        assert!(Watcher::new(std::path::Path::new("/does/not/exist")).is_err());
    }
}
