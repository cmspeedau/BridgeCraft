//! Plugin system for BridgeCraft (roadmap Phase 5).
//!
//! v1 implements the *manifest, discovery, enable/disable and capability*
//! layer. Plugins are discovered from a plugins directory; each plugin ships a
//! `plugin.json` manifest declaring its name, version, entry point and
//! capabilities. Execution (WASM host) is intentionally deferred — this module
//! provides the permissioned host-API *surface* and sandboxing *policy* so
//! later runtimes can plug in without changing callers.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{CoreError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    MenuAction,
    Panel,
    ContextMenu,
    BatchHook,
    MetadataRead,
    FileOps,
    ThumbnailRead,
    Network,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginPermissions {
    #[serde(default)]
    pub filesystem_paths: Vec<PathBuf>,
    #[serde(default)]
    pub network: bool,
}

impl Default for PluginPermissions {
    fn default() -> Self {
        Self {
            filesystem_paths: Vec::new(),
            network: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub entry: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    #[serde(default)]
    pub permissions: PluginPermissions,
    #[serde(default = "default_api_version")]
    pub api_version: u32,
}

fn default_api_version() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginState {
    Disabled,
    Enabled,
    Error,
}

#[derive(Debug, Clone)]
pub struct Plugin {
    pub id: String,
    pub root: PathBuf,
    pub manifest: PluginManifest,
    pub enabled: bool,
    pub error: Option<String>,
}

impl Plugin {
    pub fn has_capability(&self, cap: Capability) -> bool {
        self.manifest.capabilities.contains(&cap)
    }

    pub fn state(&self) -> PluginState {
        if self.error.is_some() {
            PluginState::Error
        } else if self.enabled {
            PluginState::Enabled
        } else {
            PluginState::Disabled
        }
    }
}

#[derive(Debug, Default)]
pub struct PluginManager {
    plugins_dir: Option<PathBuf>,
    plugins: Vec<Plugin>,
}

impl PluginManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            plugins_dir: Some(dir.into()),
            plugins: Vec::new(),
        }
    }

    pub fn plugins_dir(&self) -> Option<&Path> {
        self.plugins_dir.as_deref()
    }

    pub fn set_plugins_dir(&mut self, dir: impl Into<PathBuf>) {
        self.plugins_dir = Some(dir.into());
    }

    pub fn plugins(&self) -> &[Plugin] {
        &self.plugins
    }

    pub fn discover(&mut self) -> Result<usize> {
        let Some(dir) = self.plugins_dir.clone() else {
            return Ok(0);
        };
        if !dir.exists() {
            return Ok(0);
        }
        let mut count = 0usize;
        for entry in fs::read_dir(&dir).map_err(CoreError::Io)? {
            let entry = entry.map_err(CoreError::Io)?;
            if !entry.file_type().map_err(CoreError::Io)?.is_dir() {
                continue;
            }
            let root = entry.path();
            let manifest_path = root.join("plugin.json");
            if !manifest_path.exists() {
                continue;
            }
            let bytes = fs::read(&manifest_path).map_err(CoreError::Io)?;
            let manifest: PluginManifest = serde_json::from_slice(&bytes).map_err(|e| {
                CoreError::InvalidOperation(format!(
                    "invalid plugin manifest {}: {e}",
                    manifest_path.display()
                ))
            })?;
            let id = manifest.name.clone();
            let plugin = Plugin {
                id,
                root,
                manifest,
                enabled: true,
                error: None,
            };
            self.plugins.push(plugin);
            count += 1;
        }
        Ok(count)
    }

    pub fn find(&self, id: &str) -> Option<&Plugin> {
        self.plugins.iter().find(|p| p.id == id)
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut Plugin> {
        self.plugins.iter_mut().find(|p| p.id == id)
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<bool> {
        let Some(plugin) = self.find_mut(id) else {
            return Ok(false);
        };
        plugin.enabled = enabled;
        Ok(true)
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.plugins.len();
        self.plugins.retain(|p| p.id != id);
        self.plugins.len() != before
    }

    pub fn enabled_plugins(&self) -> Vec<&Plugin> {
        self.plugins.iter().filter(|p| p.enabled).collect()
    }

    pub fn plugins_with_capability(&self, cap: Capability) -> Vec<&Plugin> {
        self.plugins
            .iter()
            .filter(|p| p.enabled && p.has_capability(cap))
            .collect()
    }
}

/// Host API surface exposed to plugins. Implementations are provided by the
/// application; plugins receive a handle to a `dyn PluginHost`.
pub trait PluginHost {
    fn selected_paths(&self) -> Vec<PathBuf>;
    fn current_folder(&self) -> Option<PathBuf>;
    fn read_metadata(&self, path: &Path) -> Result<String>;
}

/// Sandboxing policy: a plugin may only touch paths it declared and network
/// only if explicitly granted.
pub fn check_path_access(plugin: &Plugin, path: &Path) -> bool {
    if plugin.manifest.permissions.network {
        // network permission does not imply filesystem access
    }
    plugin
        .manifest
        .permissions
        .filesystem_paths
        .iter()
        .any(|root| path.starts_with(root))
}

pub fn check_network_access(plugin: &Plugin) -> bool {
    plugin.manifest.permissions.network
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn discover_and_enable() {
        let dir = tempdir().unwrap();
        let plugin_dir = dir.path().join("plugins").join("demo");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let manifest = serde_json::json!({
            "name": "Demo",
            "version": "1.0.0",
            "capabilities": ["menu_action", "panel"],
            "permissions": { "filesystem_paths": ["/tmp"], "network": false }
        });
        std::fs::write(plugin_dir.join("plugin.json"), manifest.to_string()).unwrap();

        let mut mgr = PluginManager::with_dir(dir.path().join("plugins"));
        assert_eq!(mgr.discover().unwrap(), 1);
        assert_eq!(mgr.plugins().len(), 1);
        let p = mgr.plugins()[0].clone();
        assert!(p.has_capability(Capability::MenuAction));
        assert!(p.has_capability(Capability::Panel));
        assert!(!p.has_capability(Capability::Network));
        assert!(mgr.set_enabled("Demo", false).unwrap());
        assert_eq!(mgr.enabled_plugins().len(), 0);
    }

    #[test]
    fn invalid_manifest_errors() {
        let dir = tempdir().unwrap();
        let plugin_dir = dir.path().join("plugins").join("bad");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("plugin.json"), "{not json}").unwrap();
        let mut mgr = PluginManager::with_dir(dir.path().join("plugins"));
        assert!(mgr.discover().is_err());
    }

    #[test]
    fn sandbox_checks() {
        let dir = tempdir().unwrap();
        let plugin_dir = dir.path().join("plugins").join("p");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let manifest = serde_json::json!({
            "name": "P",
            "version": "1.0.0",
            "permissions": { "filesystem_paths": ["/allowed"], "network": true }
        });
        std::fs::write(plugin_dir.join("plugin.json"), manifest.to_string()).unwrap();
        let mut mgr = PluginManager::with_dir(dir.path().join("plugins"));
        mgr.discover().unwrap();
        let p = mgr.plugins()[0].clone();
        assert!(check_path_access(&p, std::path::Path::new("/allowed/sub")));
        assert!(!check_path_access(&p, std::path::Path::new("/other")));
        assert!(check_network_access(&p));
    }
}
