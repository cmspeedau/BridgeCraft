//! bc-core: filesystem scanning, watchers, selection model, and app state.

pub mod browser;
pub mod entry;
pub mod fs;
pub mod selection;
pub mod watcher;

pub use entry::Entry;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("path not found: {0}")]
    PathNotFound(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CoreError>;
