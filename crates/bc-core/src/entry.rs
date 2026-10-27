//! The item model shown in the browser grid.

/// A single item (file or folder) shown in the browser grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: std::path::PathBuf,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<std::time::SystemTime>,
}

impl Entry {
    pub fn name(&self) -> &std::ffi::OsStr {
        self.path
            .file_name()
            .unwrap_or_else(|| self.path.as_os_str())
    }
}
