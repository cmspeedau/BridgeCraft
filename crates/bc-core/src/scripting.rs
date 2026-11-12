//! WASM scripting / automation API (issue #18).
//!
//! Automation scripts are local plugin-style directories under
//! `<config>/scripts/`. Each directory contains a `plugin.json` manifest and a
//! WASM module. The same `wasmi` sandbox, capability grants, and host API used
//! by installed plugins are used for scripts; scripts receive no ambient
//! filesystem or network access.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::plugins::{Plugin, PluginGrants, PluginHost, PluginManager};
use crate::{CoreError, Result};

const SCRIPT_STATE_FILE: &str = ".script-state.json";

#[derive(Debug, Clone)]
pub struct ScriptResult {
    pub name: String,
    pub path: PathBuf,
    pub success: bool,
    pub output: Vec<String>,
    pub error: Option<String>,
}

/// Discovers and runs local WASM automation scripts through the plugin sandbox.
pub struct ScriptHost {
    scripts_dir: PathBuf,
    manager: PluginManager,
}

impl ScriptHost {
    /// Create a script host rooted at `scripts_dir`.
    ///
    /// Each script must be a child directory with a `plugin.json` manifest and
    /// a WASM entry point. Capability and filesystem grants are persisted in
    /// `.script-state.json` and default to denied.
    pub fn new(scripts_dir: PathBuf) -> Self {
        let state_file = scripts_dir.join(SCRIPT_STATE_FILE);
        Self {
            manager: PluginManager::with_dir(&scripts_dir).with_state_file(state_file),
            scripts_dir,
        }
    }

    pub fn scripts_dir(&self) -> &Path {
        &self.scripts_dir
    }

    /// Discover and validate script manifests. Invalid manifests are reported
    /// rather than silently omitted.
    pub fn discover(&mut self) -> Result<usize> {
        self.manager.discover()
    }

    /// Return all discovered WASM script entry points in deterministic order.
    pub fn list(&self) -> Vec<PathBuf> {
        let mut scripts = self
            .manager
            .plugins()
            .iter()
            .filter_map(Plugin::wasm_path)
            .collect::<Vec<_>>();
        scripts.sort();
        scripts
    }

    /// Run a script with no host data access. This is useful for scripts that
    /// only log output; capability-bearing scripts should use `run_with_host`.
    pub fn run(&self, path: &Path) -> ScriptResult {
        self.run_with_host(path, &NoopPluginHost)
    }

    /// Run a script through the shared, capability-checked plugin WASM runtime.
    pub fn run_with_host(&self, path: &Path, host: &dyn PluginHost) -> ScriptResult {
        let name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let result = self.run_inner(path, host);
        match result {
            Ok(output) => ScriptResult {
                name,
                path: path.to_path_buf(),
                success: true,
                output,
                error: None,
            },
            Err(error) => ScriptResult {
                name,
                path: path.to_path_buf(),
                success: false,
                output: Vec::new(),
                error: Some(error.to_string()),
            },
        }
    }

    fn run_inner(&self, path: &Path, host: &dyn PluginHost) -> Result<Vec<String>> {
        let requested = path.canonicalize()?;
        let plugin = self
            .manager
            .plugins()
            .iter()
            .find(|plugin| {
                plugin
                    .wasm_path()
                    .and_then(|wasm| wasm.canonicalize().ok())
                    .is_some_and(|wasm| wasm == requested)
            })
            .ok_or_else(|| {
                CoreError::InvalidOperation(
                    "script is not a discovered WASM entry point under the scripts directory"
                        .into(),
                )
            })?;
        let result = self.manager.run_wasm(&plugin.id, None, host)?;
        Ok(result.output)
    }

    /// Run every discovered script independently; one failure does not stop
    /// subsequent scripts.
    pub fn run_all(&self) -> Vec<ScriptResult> {
        self.list().iter().map(|path| self.run(path)).collect()
    }

    pub fn run_all_with_host(&self, host: &dyn PluginHost) -> Vec<ScriptResult> {
        self.list()
            .iter()
            .map(|path| self.run_with_host(path, host))
            .collect()
    }

    /// Update a script's user-approved grants. Unrequested capabilities and
    /// filesystem roots are rejected by `PluginManager`.
    pub fn set_grants(&mut self, id: &str, grants: PluginGrants) -> Result<bool> {
        self.manager.set_grants(id, grants)
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<bool> {
        self.manager.set_enabled(id, enabled)
    }

    pub fn plugins(&self) -> &[Plugin] {
        self.manager.plugins()
    }

    /// Kept as a source-compatible no-op for callers of the former Lua host.
    /// WASM execution is bounded by deterministic fuel and memory limits;
    /// this diagnostic duration does not interrupt running modules.
    #[deprecated(note = "WASM scripts use deterministic fuel limits, not wall-clock timeouts")]
    pub fn with_timeout(self, _timeout: Duration) -> Self {
        self
    }
}

struct NoopPluginHost;

impl PluginHost for NoopPluginHost {
    fn selected_paths(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    fn current_folder(&self) -> Option<PathBuf> {
        None
    }

    fn read_metadata(&self, _path: &Path) -> Result<String> {
        Err(CoreError::InvalidOperation(
            "metadata access is unavailable for this script invocation".into(),
        ))
    }
}

// Compatibility aliases that do not imply or retain a Lua runtime.
pub type ScriptEngine = ScriptHost;
pub type AutomationHost = ScriptHost;
pub type ScriptRunner = ScriptHost;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    // Minimal WASM module importing bridgecraft.log and writing "ok".
    const LOG_WASM: &[u8] = &[
        0, 97, 115, 109, 1, 0, 0, 0, 1, 9, 2, 96, 2, 127, 127, 0, 96, 0, 0, 2, 19, 1, 11, 98, 114,
        105, 100, 103, 101, 99, 114, 97, 102, 116, 3, 108, 111, 103, 0, 0, 3, 2, 1, 1, 5, 3, 1, 0,
        1, 7, 19, 2, 6, 109, 101, 109, 111, 114, 121, 2, 0, 6, 95, 115, 116, 97, 114, 116, 0, 1,
        10, 10, 1, 8, 0, 65, 0, 65, 2, 16, 0, 11, 11, 8, 1, 0, 65, 0, 11, 2, 111, 107,
    ];

    fn write_script(scripts_dir: &Path, name: &str, capabilities: &[&str]) -> PathBuf {
        let script_dir = scripts_dir.join(name);
        fs::create_dir_all(&script_dir).unwrap();
        fs::write(script_dir.join("script.wasm"), LOG_WASM).unwrap();
        let manifest = serde_json::json!({
            "name": name,
            "version": "1.0.0",
            "entry_points": {"wasm": "script.wasm"},
            "capabilities": capabilities,
            "permissions": {"filesystem_paths": [], "network": false}
        });
        fs::write(script_dir.join("plugin.json"), manifest.to_string()).unwrap();
        script_dir.join("script.wasm")
    }

    #[test]
    fn discovers_and_runs_wasm_script_with_output() {
        let dir = tempdir().unwrap();
        let script = write_script(dir.path(), "hello", &[]);
        let mut host = ScriptHost::new(dir.path().to_path_buf());
        assert_eq!(host.discover().unwrap(), 1);
        let result = host.run(&script);
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.output, vec!["ok"]);
        assert_eq!(host.list(), vec![script]);
    }

    #[test]
    fn script_execution_is_disabled_until_discovery_and_requires_in_root_module() {
        let dir = tempdir().unwrap();
        let script = write_script(dir.path(), "hello", &[]);
        let host = ScriptHost::new(dir.path().to_path_buf());
        assert!(!host.run(&script).success);
        let mut host = host;
        host.discover().unwrap();
        let elsewhere = tempdir().unwrap();
        let outside = elsewhere.path().join("script.wasm");
        fs::write(&outside, LOG_WASM).unwrap();
        assert!(!host.run(&outside).success);
    }

    #[test]
    fn grants_are_default_denied_and_persisted() {
        let dir = tempdir().unwrap();
        write_script(dir.path(), "hello", &["selection_read"]);
        let mut host = ScriptHost::new(dir.path().to_path_buf());
        host.discover().unwrap();
        assert!(host.plugins()[0].grants.capabilities.is_empty());
        host.set_grants(
            "hello",
            PluginGrants {
                capabilities: [crate::plugins::Capability::SelectionRead]
                    .into_iter()
                    .collect(),
                ..PluginGrants::default()
            },
        )
        .unwrap();
        let mut reloaded = ScriptHost::new(dir.path().to_path_buf());
        reloaded.discover().unwrap();
        assert!(reloaded.plugins()[0]
            .grants
            .capabilities
            .contains(&crate::plugins::Capability::SelectionRead));
    }

    #[test]
    fn malformed_wasm_returns_an_error_result() {
        let dir = tempdir().unwrap();
        let script = write_script(dir.path(), "broken", &[]);
        fs::write(&script, b"not wasm").unwrap();
        let mut host = ScriptHost::new(dir.path().to_path_buf());
        host.discover().unwrap();
        let result = host.run(&script);
        assert!(!result.success);
        assert!(result.error.is_some());
    }
}
