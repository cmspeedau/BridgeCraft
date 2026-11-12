//! Permissioned WASM plugin system (roadmap Phase 5).
//!
//! Plugins are local directories containing `plugin.json` and a WASM module.
//! Native plugins are deliberately unsupported. The loader validates manifests,
//! keeps enablement in a separate local state file, and executes WASM in wasmi
//! with fuel and linear-memory limits. Guest modules receive only explicit
//! host imports; there is no WASI, filesystem, or network ambient access.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use wasmi::{Caller, Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};

use crate::{CoreError, Result};

const MANIFEST_FILE: &str = "plugin.json";
const STATE_FILE: &str = "plugins.json";
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_WASM_BYTES: u64 = 32 * 1024 * 1024;
const MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_FUEL: u64 = 10_000_000;
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    MenuAction,
    Panel,
    ContextMenu,
    BatchHook,
    SelectionRead,
    CurrentFolderRead,
    MetadataRead,
    FileOps,
    ThumbnailRead,
    Network,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginPermissions {
    /// Explicit filesystem roots the plugin may request through host APIs.
    #[serde(default)]
    pub filesystem_paths: Vec<PathBuf>,
    /// Network remains denied unless explicitly granted. No network host
    /// functions are currently provided by the v1 runtime.
    #[serde(default)]
    pub network: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginEntryPoints {
    #[serde(default)]
    pub wasm: Option<String>,
    #[serde(default)]
    pub menu_actions: Vec<PluginAction>,
    #[serde(default)]
    pub panels: Vec<PluginPanel>,
    #[serde(default)]
    pub context_menu: Vec<PluginAction>,
    #[serde(default)]
    pub batch_hooks: Vec<PluginAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginAction {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    /// Exported WASM function, defaulting to `_start`.
    #[serde(default)]
    pub function: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginPanel {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    /// Legacy shorthand; superseded by `entry_points.wasm`.
    #[serde(default)]
    pub entry: Option<String>,
    #[serde(default)]
    pub entry_points: PluginEntryPoints,
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
    /// User-approved capabilities and permissions, distinct from manifest requests.
    pub grants: PluginGrants,
    pub enabled: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginGrants {
    #[serde(default)]
    pub capabilities: BTreeSet<Capability>,
    #[serde(default)]
    pub filesystem_paths: Vec<PathBuf>,
    #[serde(default)]
    pub network: bool,
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

    pub fn wasm_path(&self) -> Option<PathBuf> {
        self.manifest
            .entry_points
            .wasm
            .as_ref()
            .or(self.manifest.entry.as_ref())
            .map(|entry| self.root.join(entry))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PluginStateFile {
    #[serde(default)]
    disabled: BTreeSet<String>,
    #[serde(default)]
    grants: std::collections::BTreeMap<String, PluginGrants>,
}

#[derive(Debug, Default)]
pub struct PluginManager {
    plugins_dir: Option<PathBuf>,
    plugins: Vec<Plugin>,
    state_file: Option<PathBuf>,
}

impl PluginManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            plugins_dir: Some(dir.into()),
            plugins: Vec::new(),
            state_file: None,
        }
    }

    pub fn open_default() -> Result<Self> {
        let root = crate::recents::user_data_dir()?.join("BridgeCraft");
        let plugins_dir = root.join("plugins");
        fs::create_dir_all(&plugins_dir)?;
        let mut manager = Self::with_dir(plugins_dir);
        manager.state_file = Some(root.join(STATE_FILE));
        manager.discover()?;
        Ok(manager)
    }

    pub fn plugins_dir(&self) -> Option<&Path> {
        self.plugins_dir.as_deref()
    }

    /// Configure a separate persistence file for enablement and capability grants.
    pub fn with_state_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.state_file = Some(path.into());
        self
    }

    pub fn set_plugins_dir(&mut self, dir: impl Into<PathBuf>) {
        self.plugins_dir = Some(dir.into());
    }

    pub fn plugins(&self) -> &[Plugin] {
        &self.plugins
    }

    /// Discover plugin directories deterministically. Invalid manifests are
    /// surfaced as errors; symlinked plugin directories and entries escaping
    /// their plugin root are rejected.
    pub fn discover(&mut self) -> Result<usize> {
        let Some(dir) = self.plugins_dir.clone() else {
            return Ok(0);
        };
        let metadata = match fs::symlink_metadata(&dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.plugins.clear();
                return Ok(0);
            }
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(CoreError::InvalidOperation(format!(
                "plugins path is not a real directory: {}",
                dir.display()
            )));
        }

        let saved_state = self.load_state()?;
        let disabled = saved_state.disabled;
        let grants = saved_state.grants;
        let mut entries = fs::read_dir(&dir)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        let mut found = Vec::new();
        let mut ids = BTreeSet::new();
        for entry in entries {
            let file_type = entry.file_type()?;
            if file_type.is_symlink() || !file_type.is_dir() {
                continue;
            }
            let root = entry.path();
            let manifest_path = root.join(MANIFEST_FILE);
            let manifest_meta = match fs::symlink_metadata(&manifest_path) {
                Ok(meta) => meta,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if manifest_meta.file_type().is_symlink() || !manifest_meta.is_file() {
                return Err(CoreError::InvalidOperation(format!(
                    "plugin manifest must be a regular file: {}",
                    manifest_path.display()
                )));
            }
            if manifest_meta.len() > MAX_MANIFEST_BYTES {
                return Err(CoreError::InvalidOperation(format!(
                    "plugin manifest is too large: {}",
                    manifest_path.display()
                )));
            }
            let bytes = fs::read(&manifest_path)?;
            let manifest: PluginManifest = serde_json::from_slice(&bytes).map_err(|error| {
                CoreError::InvalidOperation(format!(
                    "invalid plugin manifest {}: {error}",
                    manifest_path.display()
                ))
            })?;
            validate_manifest(&manifest, &root)?;
            let id = plugin_id(&manifest.name);
            if !ids.insert(id.clone()) {
                return Err(CoreError::InvalidOperation(format!(
                    "duplicate plugin name: {}",
                    manifest.name
                )));
            }
            let mut granted = grants.get(&id).cloned().unwrap_or_default();
            granted
                .capabilities
                .retain(|capability| manifest.capabilities.contains(capability));
            granted
                .filesystem_paths
                .retain(|path| manifest.permissions.filesystem_paths.contains(path));
            granted.network &= manifest.permissions.network
                && manifest.capabilities.contains(&Capability::Network);
            found.push(Plugin {
                enabled: !disabled.contains(&id),
                grants: granted,
                id,
                root,
                manifest,
                error: None,
            });
        }
        let count = found.len();
        self.plugins = found;
        Ok(count)
    }

    fn load_state(&self) -> Result<PluginStateFile> {
        let Some(path) = &self.state_file else {
            return Ok(PluginStateFile::default());
        };
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                CoreError::InvalidOperation(format!(
                    "invalid plugin state {}: {error}",
                    path.display()
                ))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(PluginStateFile::default())
            }
            Err(error) => Err(error.into()),
        }
    }

    fn save_state(&self) -> Result<()> {
        let Some(path) = &self.state_file else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let state = PluginStateFile {
            disabled: self
                .plugins
                .iter()
                .filter(|plugin| !plugin.enabled)
                .map(|plugin| plugin.id.clone())
                .collect(),
            grants: self
                .plugins
                .iter()
                .filter(|plugin| {
                    !plugin.grants.capabilities.is_empty()
                        || !plugin.grants.filesystem_paths.is_empty()
                        || plugin.grants.network
                })
                .map(|plugin| (plugin.id.clone(), plugin.grants.clone()))
                .collect(),
        };
        let bytes = serde_json::to_vec_pretty(&state).map_err(|error| {
            CoreError::InvalidOperation(format!("could not encode plugin state: {error}"))
        })?;
        let temp = path.with_extension("json.tmp");
        fs::write(&temp, bytes)?;
        fs::rename(temp, path)?;
        Ok(())
    }

    pub fn find(&self, id: &str) -> Option<&Plugin> {
        self.plugins.iter().find(|plugin| plugin.id == id)
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut Plugin> {
        self.plugins.iter_mut().find(|plugin| plugin.id == id)
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<bool> {
        let Some(plugin) = self.find_mut(id) else {
            return Ok(false);
        };
        plugin.enabled = enabled;
        self.save_state()?;
        Ok(true)
    }

    pub fn set_grants(&mut self, id: &str, grants: PluginGrants) -> Result<bool> {
        let Some(plugin) = self.find_mut(id) else {
            return Ok(false);
        };
        if grants.network
            && (!plugin.manifest.capabilities.contains(&Capability::Network)
                || !plugin.manifest.permissions.network)
        {
            return Err(CoreError::InvalidOperation(
                "network cannot be granted because the plugin did not request it".into(),
            ));
        }
        if !grants
            .capabilities
            .iter()
            .all(|capability| plugin.manifest.capabilities.contains(capability))
        {
            return Err(CoreError::InvalidOperation(
                "cannot grant capabilities not requested by the plugin".into(),
            ));
        }
        if !grants
            .filesystem_paths
            .iter()
            .all(|path| plugin.manifest.permissions.filesystem_paths.contains(path))
        {
            return Err(CoreError::InvalidOperation(
                "cannot grant filesystem roots not requested by the plugin".into(),
            ));
        }
        plugin.grants = grants;
        self.save_state()?;
        Ok(true)
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let previous = self.plugins.len();
        self.plugins.retain(|plugin| plugin.id != id);
        let removed = self.plugins.len() != previous;
        if removed {
            if let Err(error) = self.save_state() {
                tracing::warn!("could not persist plugin state after remove: {error}");
            }
        }
        removed
    }

    pub fn enabled_plugins(&self) -> Vec<&Plugin> {
        self.plugins
            .iter()
            .filter(|plugin| plugin.enabled)
            .collect()
    }

    pub fn plugins_with_capability(&self, cap: Capability) -> Vec<&Plugin> {
        self.plugins
            .iter()
            .filter(|plugin| plugin.enabled && plugin.has_capability(cap) && has_grant(plugin, cap))
            .collect()
    }

    /// Execute an exported WASM function using the constrained wasmi runtime.
    /// Elapsed-time checks are diagnostic only; fuel is the effective deterministic
    /// execution bound because wasmi does not interrupt on wall-clock deadlines.
    pub fn run_wasm(
        &self,
        id: &str,
        function: Option<&str>,
        host: &dyn PluginHost,
    ) -> Result<PluginRun> {
        let plugin = self
            .find(id)
            .ok_or_else(|| CoreError::PathNotFound(format!("plugin {id}")))?;
        if !plugin.enabled {
            return Err(CoreError::InvalidOperation("plugin is disabled".into()));
        }
        let path = plugin
            .wasm_path()
            .ok_or_else(|| CoreError::InvalidOperation("plugin has no WASM entry point".into()))?;
        validate_entry_path(&plugin.root, &path)?;
        let metadata = fs::metadata(&path)?;
        if !metadata.is_file() || metadata.len() > MAX_WASM_BYTES {
            return Err(CoreError::InvalidOperation(format!(
                "invalid or oversized WASM module: {}",
                path.display()
            )));
        }
        let bytes = fs::read(&path)?;
        if bytes.len() as u64 > MAX_WASM_BYTES {
            return Err(CoreError::InvalidOperation(
                "WASM module exceeds size limit".into(),
            ));
        }
        let name = function.unwrap_or("_start");
        let timeout = std::time::Duration::from_millis(DEFAULT_TIMEOUT_MS);
        let start = Instant::now();
        let mut config = Config::default();
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, &bytes[..]).map_err(|error| {
            CoreError::InvalidOperation(format!("invalid WASM module: {error}"))
        })?;
        let mut linker = Linker::<WasmHostState>::new(&engine);
        install_host_functions(&mut linker)?;
        let state = WasmHostState {
            output: Vec::new(),
            output_bytes: 0,
            selected_paths: host
                .selected_paths()
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            current_folder: host
                .current_folder()
                .map(|path| path.to_string_lossy().into_owned()),
            host,
            plugin: plugin.clone(),
            limits: StoreLimitsBuilder::new()
                .memory_size(MAX_MEMORY_BYTES)
                .memories(1)
                .tables(4)
                .instances(1)
                .build(),
        };
        let mut store = Store::new(&engine, state);
        store.limiter(|state| &mut state.limits);
        store.add_fuel(DEFAULT_FUEL).map_err(|error| {
            CoreError::InvalidOperation(format!("could not configure WASM fuel: {error}"))
        })?;
        let instance = linker
            .instantiate(&mut store, &module)
            .and_then(|pre| pre.start(&mut store))
            .map_err(|error| {
                CoreError::InvalidOperation(format!("WASM instantiation failed: {error}"))
            })?;
        if start.elapsed() > timeout {
            return Err(CoreError::InvalidOperation("plugin timed out".into()));
        }
        if let Ok(entry) = instance.get_typed_func::<(), ()>(&store, name) {
            entry.call(&mut store, ()).map_err(|error| {
                CoreError::InvalidOperation(format!("plugin execution failed: {error}"))
            })?;
        } else if name != "_start" {
            return Err(CoreError::InvalidOperation(format!(
                "WASM export not found: {name}"
            )));
        }
        if start.elapsed() > timeout {
            return Err(CoreError::InvalidOperation("plugin timed out".into()));
        }
        Ok(PluginRun {
            output: store.into_data().output,
            elapsed: start.elapsed(),
        })
    }
}

fn plugin_id(name: &str) -> String {
    name.trim().to_lowercase().replace(' ', "-")
}

fn validate_manifest(manifest: &PluginManifest, root: &Path) -> Result<()> {
    let declared_actions = manifest
        .entry_points
        .menu_actions
        .iter()
        .chain(&manifest.entry_points.context_menu)
        .chain(&manifest.entry_points.batch_hooks);
    for action in declared_actions {
        let required = match action {
            _ if manifest
                .entry_points
                .menu_actions
                .iter()
                .any(|item| item.id == action.id) =>
            {
                Capability::MenuAction
            }
            _ if manifest
                .entry_points
                .context_menu
                .iter()
                .any(|item| item.id == action.id) =>
            {
                Capability::ContextMenu
            }
            _ => Capability::BatchHook,
        };
        if !manifest.capabilities.contains(&required) {
            return Err(CoreError::InvalidOperation(format!(
                "entry point {} requires capability {:?}",
                action.id, required
            )));
        }
    }
    if !manifest.entry_points.panels.is_empty()
        && !manifest.capabilities.contains(&Capability::Panel)
    {
        return Err(CoreError::InvalidOperation(
            "panel entry point requires panel capability".into(),
        ));
    }
    if manifest.permissions.network && !manifest.capabilities.contains(&Capability::Network) {
        return Err(CoreError::InvalidOperation(
            "network permission requires network capability".into(),
        ));
    }
    if manifest.name.trim().is_empty() || manifest.name.len() > 128 {
        return Err(CoreError::InvalidOperation(
            "plugin name must be 1..=128 bytes".into(),
        ));
    }
    if manifest.version.trim().is_empty() || manifest.version.len() > 64 {
        return Err(CoreError::InvalidOperation(
            "plugin version must be 1..=64 bytes".into(),
        ));
    }
    if manifest.api_version != 1 {
        return Err(CoreError::InvalidOperation(format!(
            "unsupported plugin API version {}",
            manifest.api_version
        )));
    }
    if let Some(entry) = manifest
        .entry_points
        .wasm
        .as_ref()
        .or(manifest.entry.as_ref())
    {
        validate_entry_path(root, &root.join(entry))?;
    }
    for action in manifest
        .entry_points
        .menu_actions
        .iter()
        .chain(&manifest.entry_points.context_menu)
        .chain(&manifest.entry_points.batch_hooks)
    {
        if action.id.trim().is_empty() || action.label.trim().is_empty() {
            return Err(CoreError::InvalidOperation(
                "plugin action needs a non-empty id and label".into(),
            ));
        }
    }
    for panel in &manifest.entry_points.panels {
        if panel.id.trim().is_empty() || panel.title.trim().is_empty() {
            return Err(CoreError::InvalidOperation(
                "plugin panel needs a non-empty id and title".into(),
            ));
        }
    }
    Ok(())
}

fn validate_entry_path(root: &Path, path: &Path) -> Result<()> {
    let relative = path.strip_prefix(root).map_err(|_| {
        CoreError::InvalidOperation("plugin entry point escapes plugin directory".into())
    })?;
    if relative.as_os_str().is_empty()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(CoreError::InvalidOperation(
            "plugin entry point must be a relative path inside its plugin directory".into(),
        ));
    }
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        if fs::symlink_metadata(&current)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(CoreError::InvalidOperation(
                "plugin entry point cannot traverse symlinks".into(),
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct PluginRun {
    pub output: Vec<String>,
    pub elapsed: std::time::Duration,
}

/// Permissioned native host API. WASM guests receive a restricted ABI backed
/// by these same capabilities; they do not receive filesystem handles.
pub trait PluginHost {
    fn selected_paths(&self) -> Vec<PathBuf>;
    fn current_folder(&self) -> Option<PathBuf>;
    fn read_metadata(&self, path: &Path) -> Result<String>;
    fn read_thumbnail(&self, _path: &Path, _max_edge: u32) -> Result<Vec<u8>> {
        Err(CoreError::InvalidOperation(
            "thumbnail access is unavailable".into(),
        ))
    }
    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        crate::file_ops::move_path(from, to)
    }
    fn copy(&self, from: &Path, to: &Path) -> Result<()> {
        crate::file_ops::copy_path(from, to)
    }
    fn move_path(&self, from: &Path, to: &Path) -> Result<()> {
        crate::file_ops::move_path(from, to)
    }
}

struct WasmHostState<'a> {
    output: Vec<String>,
    output_bytes: usize,
    selected_paths: Vec<String>,
    current_folder: Option<String>,
    host: &'a dyn PluginHost,
    plugin: Plugin,
    limits: StoreLimits,
}

#[derive(Clone, Copy)]
enum FileTransfer {
    Copy,
    Move,
}

fn file_transfer(
    caller: Caller<'_, WasmHostState<'_>>,
    from_ptr: i32,
    from_len: i32,
    to_ptr: i32,
    to_len: i32,
    transfer: FileTransfer,
) -> std::result::Result<(), wasmi::core::Trap> {
    if from_ptr < 0
        || from_len < 0
        || from_len as usize > 4096
        || to_ptr < 0
        || to_len < 0
        || to_len as usize > 4096
    {
        return Err(wasmi::core::Trap::new("invalid transfer path range"));
    }
    if !has_grant(&caller.data().plugin, Capability::FileOps) {
        return Err(wasmi::core::Trap::new(
            "file-operation capability not granted",
        ));
    }
    let memory = caller
        .get_export("memory")
        .and_then(wasmi::Extern::into_memory)
        .ok_or_else(|| wasmi::core::Trap::new("module must export memory"))?;
    let mut from_bytes = vec![0; from_len as usize];
    let mut to_bytes = vec![0; to_len as usize];
    memory
        .read(&caller, from_ptr as usize, &mut from_bytes)
        .map_err(|_| wasmi::core::Trap::new("transfer source out of bounds"))?;
    memory
        .read(&caller, to_ptr as usize, &mut to_bytes)
        .map_err(|_| wasmi::core::Trap::new("transfer destination out of bounds"))?;
    let from_text = String::from_utf8(from_bytes)
        .map_err(|_| wasmi::core::Trap::new("transfer source is not UTF-8"))?;
    let to_text = String::from_utf8(to_bytes)
        .map_err(|_| wasmi::core::Trap::new("transfer destination is not UTF-8"))?;
    let root = caller.data().plugin.root.clone();
    let from_requested = Path::new(&from_text);
    let to_requested = Path::new(&to_text);
    let from = if from_requested.is_absolute() {
        from_requested.to_path_buf()
    } else {
        root.join(from_requested)
    };
    let to = if to_requested.is_absolute() {
        to_requested.to_path_buf()
    } else {
        root.join(to_requested)
    };
    if !check_path_access(&caller.data().plugin, &from)
        || !check_path_access(&caller.data().plugin, to.parent().unwrap_or(&to))
    {
        return Err(wasmi::core::Trap::new("filesystem path not granted"));
    }
    match transfer {
        FileTransfer::Copy => caller.data().host.copy(&from, &to),
        FileTransfer::Move => caller.data().host.move_path(&from, &to),
    }
    .map_err(|_| wasmi::core::Trap::new("file transfer denied or failed"))
}

fn install_host_functions(linker: &mut Linker<WasmHostState<'_>>) -> Result<()> {
    linker
        .func_wrap(
            "bridgecraft",
            "selection_count",
            |caller: Caller<'_, WasmHostState<'_>>| -> std::result::Result<i32, wasmi::core::Trap> {
                if !has_grant(&caller.data().plugin, Capability::SelectionRead) {
                    return Err(wasmi::core::Trap::new("selection capability not granted"));
                }
                Ok(caller.data().selected_paths.len().min(i32::MAX as usize) as i32)
            },
        )
        .map_err(|error| CoreError::InvalidOperation(error.to_string()))?;
    linker
        .func_wrap(
            "bridgecraft",
            "selection_path",
            |mut caller: Caller<'_, WasmHostState<'_>>,
             index: i32,
             out_ptr: i32,
             out_cap: i32|
             -> std::result::Result<i32, wasmi::core::Trap> {
                if index < 0 || out_ptr < 0 || out_cap < 0 || out_cap as usize > 64 * 1024 {
                    return Err(wasmi::core::Trap::new("invalid selection buffer range"));
                }
                if !has_grant(&caller.data().plugin, Capability::SelectionRead) {
                    return Err(wasmi::core::Trap::new("selection capability not granted"));
                }
                let path = caller
                    .data()
                    .selected_paths
                    .get(index as usize)
                    .ok_or_else(|| wasmi::core::Trap::new("selection index out of range"))?
                    .as_bytes()
                    .to_vec();
                if path.len() > out_cap as usize {
                    return Err(wasmi::core::Trap::new("selection output buffer too small"));
                }
                let memory = caller
                    .get_export("memory")
                    .and_then(wasmi::Extern::into_memory)
                    .ok_or_else(|| wasmi::core::Trap::new("module must export memory"))?;
                memory
                    .write(&mut caller, out_ptr as usize, &path)
                    .map_err(|_| wasmi::core::Trap::new("selection output out of bounds"))?;
                Ok(path.len() as i32)
            },
        )
        .map_err(|error| CoreError::InvalidOperation(error.to_string()))?;
    linker
        .func_wrap(
            "bridgecraft",
            "current_folder",
            |mut caller: Caller<'_, WasmHostState<'_>>,
             out_ptr: i32,
             out_cap: i32|
             -> std::result::Result<i32, wasmi::core::Trap> {
                if out_ptr < 0 || out_cap < 0 || out_cap as usize > 64 * 1024 {
                    return Err(wasmi::core::Trap::new(
                        "invalid current-folder buffer range",
                    ));
                }
                if !has_grant(&caller.data().plugin, Capability::CurrentFolderRead) {
                    return Err(wasmi::core::Trap::new(
                        "current-folder capability not granted",
                    ));
                }
                let path = caller
                    .data()
                    .current_folder
                    .as_deref()
                    .unwrap_or("")
                    .as_bytes()
                    .to_vec();
                if path.len() > out_cap as usize {
                    return Err(wasmi::core::Trap::new("current-folder buffer too small"));
                }
                let memory = caller
                    .get_export("memory")
                    .and_then(wasmi::Extern::into_memory)
                    .ok_or_else(|| wasmi::core::Trap::new("module must export memory"))?;
                memory
                    .write(&mut caller, out_ptr as usize, &path)
                    .map_err(|_| wasmi::core::Trap::new("current-folder output out of bounds"))?;
                Ok(path.len() as i32)
            },
        )
        .map_err(|error| CoreError::InvalidOperation(error.to_string()))?;
    linker
        .func_wrap(
            "bridgecraft",
            "log",
            |mut caller: Caller<'_, WasmHostState>,
             ptr: i32,
             len: i32|
             -> std::result::Result<(), wasmi::core::Trap> {
                if ptr < 0 || len < 0 || len as usize > 16 * 1024 {
                    return Err(wasmi::core::Trap::new("invalid log buffer range"));
                }
                let memory = caller
                    .get_export("memory")
                    .and_then(wasmi::Extern::into_memory)
                    .ok_or_else(|| wasmi::core::Trap::new("module must export memory"))?;
                let mut bytes = vec![0; len as usize];
                memory
                    .read(&caller, ptr as usize, &mut bytes)
                    .map_err(|_| wasmi::core::Trap::new("log buffer out of bounds"))?;
                let message = String::from_utf8(bytes)
                    .map_err(|_| wasmi::core::Trap::new("log message is not UTF-8"))?;
                if caller.data().output_bytes.saturating_add(message.len()) > MAX_OUTPUT_BYTES {
                    return Err(wasmi::core::Trap::new("script output limit exceeded"));
                }
                let state = caller.data_mut();
                state.output_bytes += message.len();
                state.output.push(message);
                Ok(())
            },
        )
        .map_err(|error| CoreError::InvalidOperation(error.to_string()))?;
    linker
        .func_wrap(
            "bridgecraft",
            "thumbnail_read",
            |mut caller: Caller<'_, WasmHostState<'_>>,
             path_ptr: i32,
             path_len: i32,
             max_edge: i32,
             out_ptr: i32,
             out_cap: i32|
             -> std::result::Result<i32, wasmi::core::Trap> {
                if path_ptr < 0
                    || path_len < 0
                    || path_len as usize > 4096
                    || max_edge <= 0
                    || max_edge > 4096
                    || out_ptr < 0
                    || out_cap < 0
                    || out_cap as usize > 16 * 1024 * 1024
                {
                    return Err(wasmi::core::Trap::new("invalid thumbnail buffer range"));
                }
                if !has_grant(&caller.data().plugin, Capability::ThumbnailRead) {
                    return Err(wasmi::core::Trap::new("thumbnail capability not granted"));
                }
                let memory = caller
                    .get_export("memory")
                    .and_then(wasmi::Extern::into_memory)
                    .ok_or_else(|| wasmi::core::Trap::new("module must export memory"))?;
                let mut path_bytes = vec![0; path_len as usize];
                memory
                    .read(&caller, path_ptr as usize, &mut path_bytes)
                    .map_err(|_| wasmi::core::Trap::new("thumbnail path out of bounds"))?;
                let path_text = String::from_utf8(path_bytes)
                    .map_err(|_| wasmi::core::Trap::new("thumbnail path is not UTF-8"))?;
                let requested = Path::new(&path_text);
                let candidate = if requested.is_absolute() {
                    requested.to_path_buf()
                } else {
                    caller.data().plugin.root.join(requested)
                };
                if !check_path_access(&caller.data().plugin, &candidate) {
                    return Err(wasmi::core::Trap::new("filesystem path not granted"));
                }
                let bytes = caller
                    .data()
                    .host
                    .read_thumbnail(&candidate, max_edge as u32)
                    .map_err(|_| wasmi::core::Trap::new("thumbnail read denied or failed"))?;
                if bytes.len() > out_cap as usize {
                    return Err(wasmi::core::Trap::new("thumbnail output buffer too small"));
                }
                memory
                    .write(&mut caller, out_ptr as usize, &bytes)
                    .map_err(|_| wasmi::core::Trap::new("thumbnail output out of bounds"))?;
                Ok(bytes.len() as i32)
            },
        )
        .map_err(|error| CoreError::InvalidOperation(error.to_string()))?;
    linker
        .func_wrap(
            "bridgecraft",
            "file_rename",
            |caller: Caller<'_, WasmHostState<'_>>,
             from_ptr: i32,
             from_len: i32,
             to_ptr: i32,
             to_len: i32|
             -> std::result::Result<(), wasmi::core::Trap> {
                if from_ptr < 0
                    || from_len < 0
                    || from_len as usize > 4096
                    || to_ptr < 0
                    || to_len < 0
                    || to_len as usize > 4096
                {
                    return Err(wasmi::core::Trap::new("invalid rename path range"));
                }
                if !has_grant(&caller.data().plugin, Capability::FileOps) {
                    return Err(wasmi::core::Trap::new(
                        "file-operation capability not granted",
                    ));
                }
                let memory = caller
                    .get_export("memory")
                    .and_then(wasmi::Extern::into_memory)
                    .ok_or_else(|| wasmi::core::Trap::new("module must export memory"))?;
                let mut from_bytes = vec![0; from_len as usize];
                let mut to_bytes = vec![0; to_len as usize];
                memory
                    .read(&caller, from_ptr as usize, &mut from_bytes)
                    .map_err(|_| wasmi::core::Trap::new("rename source out of bounds"))?;
                memory
                    .read(&caller, to_ptr as usize, &mut to_bytes)
                    .map_err(|_| wasmi::core::Trap::new("rename destination out of bounds"))?;
                let from_text = String::from_utf8(from_bytes)
                    .map_err(|_| wasmi::core::Trap::new("rename source is not UTF-8"))?;
                let to_text = String::from_utf8(to_bytes)
                    .map_err(|_| wasmi::core::Trap::new("rename destination is not UTF-8"))?;
                let from_req = Path::new(&from_text);
                let to_req = Path::new(&to_text);
                let root = caller.data().plugin.root.clone();
                let from = if from_req.is_absolute() {
                    from_req.to_path_buf()
                } else {
                    root.join(from_req)
                };
                let to = if to_req.is_absolute() {
                    to_req.to_path_buf()
                } else {
                    root.join(to_req)
                };
                if !check_path_access(&caller.data().plugin, &from)
                    || !check_path_access(&caller.data().plugin, to.parent().unwrap_or(&to))
                {
                    return Err(wasmi::core::Trap::new("filesystem path not granted"));
                }
                caller
                    .data()
                    .host
                    .rename(&from, &to)
                    .map_err(|_| wasmi::core::Trap::new("rename denied or failed"))
            },
        )
        .map_err(|error| CoreError::InvalidOperation(error.to_string()))?;
    linker
        .func_wrap(
            "bridgecraft",
            "file_copy",
            |caller: Caller<'_, WasmHostState<'_>>,
             from_ptr: i32,
             from_len: i32,
             to_ptr: i32,
             to_len: i32|
             -> std::result::Result<(), wasmi::core::Trap> {
                file_transfer(
                    caller,
                    from_ptr,
                    from_len,
                    to_ptr,
                    to_len,
                    FileTransfer::Copy,
                )
            },
        )
        .map_err(|error| CoreError::InvalidOperation(error.to_string()))?;
    linker
        .func_wrap(
            "bridgecraft",
            "file_move",
            |caller: Caller<'_, WasmHostState<'_>>,
             from_ptr: i32,
             from_len: i32,
             to_ptr: i32,
             to_len: i32|
             -> std::result::Result<(), wasmi::core::Trap> {
                file_transfer(
                    caller,
                    from_ptr,
                    from_len,
                    to_ptr,
                    to_len,
                    FileTransfer::Move,
                )
            },
        )
        .map_err(|error| CoreError::InvalidOperation(error.to_string()))?;
    linker
        .func_wrap(
            "bridgecraft",
            "metadata_read",
            |mut caller: Caller<'_, WasmHostState<'_>>,
             path_ptr: i32,
             path_len: i32,
             out_ptr: i32,
             out_cap: i32|
             -> std::result::Result<i32, wasmi::core::Trap> {
                if path_ptr < 0
                    || path_len < 0
                    || path_len as usize > 4096
                    || out_ptr < 0
                    || out_cap < 0
                    || out_cap as usize > 1024 * 1024
                {
                    return Err(wasmi::core::Trap::new("invalid metadata buffer range"));
                }
                if !has_grant(&caller.data().plugin, Capability::MetadataRead) {
                    return Err(wasmi::core::Trap::new("metadata capability not granted"));
                }
                let memory = caller
                    .get_export("memory")
                    .and_then(wasmi::Extern::into_memory)
                    .ok_or_else(|| wasmi::core::Trap::new("module must export memory"))?;
                let mut path_bytes = vec![0; path_len as usize];
                memory
                    .read(&caller, path_ptr as usize, &mut path_bytes)
                    .map_err(|_| wasmi::core::Trap::new("metadata path out of bounds"))?;
                let path_text = String::from_utf8(path_bytes)
                    .map_err(|_| wasmi::core::Trap::new("metadata path is not UTF-8"))?;
                let requested = Path::new(&path_text);
                let candidate = if requested.is_absolute() {
                    requested.to_path_buf()
                } else {
                    caller.data().plugin.root.join(requested)
                };
                if !check_path_access(&caller.data().plugin, &candidate) {
                    return Err(wasmi::core::Trap::new("filesystem path not granted"));
                }
                let json = caller
                    .data()
                    .host
                    .read_metadata(&candidate)
                    .map_err(|_| wasmi::core::Trap::new("metadata read denied or failed"))?;
                let bytes = json.as_bytes();
                if bytes.len() > out_cap as usize {
                    return Err(wasmi::core::Trap::new("metadata output buffer too small"));
                }
                memory
                    .write(&mut caller, out_ptr as usize, bytes)
                    .map_err(|_| wasmi::core::Trap::new("metadata output out of bounds"))?;
                Ok(bytes.len() as i32)
            },
        )
        .map_err(|error| CoreError::InvalidOperation(error.to_string()))?;
    Ok(())
}

/// Check whether `path` is inside a plugin's explicitly granted filesystem
/// roots. Canonicalize existing paths to prevent `..` and symlink escapes.
pub fn check_path_access(plugin: &Plugin, path: &Path) -> bool {
    let Ok(candidate) = path.canonicalize() else {
        return false;
    };
    plugin
        .grants
        .filesystem_paths
        .iter()
        .filter(|granted| {
            plugin
                .manifest
                .permissions
                .filesystem_paths
                .contains(granted)
        })
        .filter_map(|root| root.canonicalize().ok())
        .any(|root| candidate.starts_with(root))
}

pub fn check_network_access(plugin: &Plugin) -> bool {
    plugin.grants.capabilities.contains(&Capability::Network) && plugin.grants.network
}

fn has_grant(plugin: &Plugin, capability: Capability) -> bool {
    plugin.has_capability(capability) && plugin.grants.capabilities.contains(&capability)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_plugin(dir: &Path, name: &str, extra: serde_json::Value) -> PathBuf {
        let plugin_dir = dir.join(name);
        fs::create_dir_all(&plugin_dir).unwrap();
        let mut manifest = serde_json::json!({
            "name": name,
            "version": "1.0.0",
            "entry_points": {
                "wasm": "plugin.wasm",
                "menu_actions": [{"id":"run", "label":"Run"}],
                "panels": [{"id":"panel", "title":"Panel"}],
                "context_menu": [{"id":"edit", "label":"Edit"}]
            },
            "capabilities": ["menu_action", "panel", "context_menu", "metadata_read"],
            "permissions": {"filesystem_paths": [], "network": false}
        });
        if let (Some(dest), Some(source)) = (manifest.as_object_mut(), extra.as_object()) {
            dest.extend(source.clone());
        }
        fs::write(plugin_dir.join(MANIFEST_FILE), manifest.to_string()).unwrap();
        fs::write(plugin_dir.join("plugin.wasm"), b"not actually wasm").unwrap();
        plugin_dir
    }

    #[test]
    fn discover_manifest_entry_points_and_capabilities() {
        let dir = tempdir().unwrap();
        write_plugin(dir.path(), "Demo", serde_json::json!({}));
        let mut manager = PluginManager::with_dir(dir.path());
        assert_eq!(manager.discover().unwrap(), 1);
        let plugin = &manager.plugins()[0];
        assert_eq!(plugin.id, "demo");
        assert_eq!(plugin.manifest.entry_points.menu_actions[0].label, "Run");
        assert_eq!(plugin.manifest.entry_points.panels[0].title, "Panel");
        assert!(plugin.has_capability(Capability::ContextMenu));
    }

    #[test]
    fn invalid_manifest_errors() {
        let dir = tempdir().unwrap();
        let plugin_dir = dir.path().join("bad");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(plugin_dir.join(MANIFEST_FILE), "{not json}").unwrap();
        let mut manager = PluginManager::with_dir(dir.path());
        assert!(manager.discover().is_err());
    }

    #[test]
    fn rejects_entry_traversal_and_unknown_api() {
        let dir = tempdir().unwrap();
        write_plugin(
            dir.path(),
            "bad-entry",
            serde_json::json!({"entry_points":{"wasm":"../outside.wasm"}}),
        );
        let mut manager = PluginManager::with_dir(dir.path());
        assert!(manager.discover().is_err());

        let other = tempdir().unwrap();
        write_plugin(
            other.path(),
            "bad-api",
            serde_json::json!({"api_version":99}),
        );
        let mut manager = PluginManager::with_dir(other.path());
        assert!(manager.discover().is_err());
    }

    #[test]
    fn enablement_persists_and_default_network_is_denied() {
        let dir = tempdir().unwrap();
        let plugin_dir = dir.path().join("plugins");
        write_plugin(&plugin_dir, "demo", serde_json::json!({}));
        let state_path = dir.path().join("plugin-state.json");
        let mut manager = PluginManager::with_dir(&plugin_dir);
        manager.state_file = Some(state_path);
        manager.discover().unwrap();
        assert!(!manager.plugins()[0].manifest.permissions.network);
        assert!(manager.set_enabled("demo", false).unwrap());
        manager.discover().unwrap();
        assert!(!manager.plugins()[0].enabled);
    }

    #[test]
    fn path_check_requires_existing_canonical_path_and_grant() {
        let dir = tempdir().unwrap();
        let allowed = dir.path().join("allowed");
        fs::create_dir(&allowed).unwrap();
        let file = allowed.join("item");
        fs::write(&file, b"x").unwrap();
        let plugin = Plugin {
            id: "demo".into(),
            root: dir.path().into(),
            manifest: PluginManifest {
                name: "Demo".into(),
                version: "1".into(),
                description: String::new(),
                author: String::new(),
                entry: None,
                entry_points: PluginEntryPoints::default(),
                capabilities: vec![],
                permissions: PluginPermissions {
                    filesystem_paths: vec![allowed.clone()],
                    network: false,
                },
                api_version: 1,
            },
            grants: PluginGrants {
                capabilities: BTreeSet::new(),
                filesystem_paths: vec![allowed.clone()],
                network: false,
            },
            enabled: true,
            error: None,
        };
        assert!(check_path_access(&plugin, &file));
        assert!(!check_path_access(&plugin, &dir.path().join("missing")));
    }

    #[test]
    fn grants_are_default_denied_and_cannot_exceed_manifest() {
        let dir = tempdir().unwrap();
        let plugin_root = dir.path().join("plugins");
        write_plugin(
            &plugin_root,
            "demo",
            serde_json::json!({
                "capabilities": ["menu_action", "panel", "context_menu", "metadata_read", "selection_read"],
                "permissions": {"filesystem_paths": [], "network": false}
            }),
        );
        let mut manager = PluginManager::with_dir(&plugin_root);
        manager.discover().unwrap();
        assert!(manager
            .plugins_with_capability(Capability::MenuAction)
            .is_empty());
        let grants = PluginGrants {
            capabilities: [Capability::MenuAction, Capability::SelectionRead]
                .into_iter()
                .collect(),
            filesystem_paths: Vec::new(),
            network: false,
        };
        assert!(manager.set_grants("demo", grants.clone()).unwrap());
        assert_eq!(
            manager
                .plugins_with_capability(Capability::MenuAction)
                .len(),
            1
        );
        assert!(manager
            .set_grants(
                "demo",
                PluginGrants {
                    capabilities: [Capability::Network].into_iter().collect(),
                    ..PluginGrants::default()
                }
            )
            .is_err());
    }

    #[test]
    fn wasm_runs_with_guest_log_and_limits() {
        // (module (import "bridgecraft" "log" (func $log (param i32 i32)))
        // (memory (export "memory") 1) (data (i32.const 0) "ok")
        // (func (export "_start") (i32.const 0) (i32.const 2) call $log))
        const WASM: &[u8] = &[
            0, 97, 115, 109, 1, 0, 0, 0, 1, 9, 2, 96, 2, 127, 127, 0, 96, 0, 0, 2, 19, 1, 11, 98,
            114, 105, 100, 103, 101, 99, 114, 97, 102, 116, 3, 108, 111, 103, 0, 0, 3, 2, 1, 1, 5,
            3, 1, 0, 1, 7, 19, 2, 6, 109, 101, 109, 111, 114, 121, 2, 0, 6, 95, 115, 116, 97, 114,
            116, 0, 1, 10, 10, 1, 8, 0, 65, 0, 65, 2, 16, 0, 11, 11, 8, 1, 0, 65, 0, 11, 2, 111,
            107,
        ];
        let dir = tempdir().unwrap();
        let plugin_dir = dir.path().join("demo");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(plugin_dir.join("plugin.wasm"), WASM).unwrap();
        let manifest = serde_json::json!({
            "name":"demo", "version":"1",
            "entry_points":{"wasm":"plugin.wasm"},
            "capabilities":[]
        });
        fs::write(plugin_dir.join(MANIFEST_FILE), manifest.to_string()).unwrap();
        let mut manager = PluginManager::with_dir(dir.path());
        manager.discover().unwrap();
        struct Host;
        impl PluginHost for Host {
            fn selected_paths(&self) -> Vec<PathBuf> {
                Vec::new()
            }
            fn current_folder(&self) -> Option<PathBuf> {
                None
            }
            fn read_metadata(&self, _path: &Path) -> Result<String> {
                Ok(String::new())
            }
        }
        let result = manager.run_wasm("demo", None, &Host).unwrap();
        assert_eq!(result.output, vec!["ok"]);
    }
}
