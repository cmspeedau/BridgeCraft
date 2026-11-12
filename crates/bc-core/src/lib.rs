//! bc-core: filesystem scanning, watchers, selection model, and app state.

pub mod auto_process;
pub mod contact_sheet;
pub mod metadata_index;
pub mod plugins;
pub mod publish;
pub use metadata_index as search;
pub use metadata_index as metadata_search;
pub use metadata_index as index;
pub use metadata_index::*;
pub use plugins::*;
pub mod batch_rename;
pub mod browser;
pub mod entry;
pub mod export;
pub mod favorites;
pub mod file_ops;
pub mod fs;
pub mod jobs;
pub mod recents;
pub mod scripting;
pub mod selection;
pub use scripting::*;
pub mod trash;
pub mod virtual_folders;
pub use virtual_folders::*;
pub mod version_stacks;
pub use version_stacks as versions;
pub use version_stacks::*;
pub mod watcher;

pub use entry::Entry;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("path not found: {0}")]
    PathNotFound(String),

    #[error("path already exists: {0}")]
    PathAlreadyExists(String),

    #[error("invalid filesystem operation: {0}")]
    InvalidOperation(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CoreError>;
