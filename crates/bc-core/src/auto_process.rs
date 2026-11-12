//! Watched-folder auto-processing rules (issue #17).
//!
//! Rules are persisted as versioned JSON. Watcher events only enqueue candidate
//! paths: a settle gate verifies that size/mtime remain unchanged before a
//! bounded worker executes the rule's actions. Rule matching and settle timing
//! are separately testable without sleeping or an OS watcher.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::batch_rename::{self, Pattern, RenameContext};
use crate::export::{self, ExportFormat};
use crate::watcher::Watcher;
use crate::{CoreError, Result};

const CONFIG_VERSION: u32 = 1;
const HISTORY_LIMIT: usize = 200;
const MAX_CONSECUTIVE_FAILURES: u8 = 10;
const MAX_WORKERS: usize = 4;
const MAX_QUEUED_ACTIONS: usize = 4096;
const MAX_PENDING_FILES: usize = 16_384;
const DEFAULT_SETTLE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutoRule {
    pub id: String,
    pub name: String,
    pub root: PathBuf,
    pub enabled: bool,
    /// Lower-case extension allow-list; empty accepts all extensions.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Minimum file size in bytes.
    #[serde(default)]
    pub min_size: u64,
    /// Optional relative path pattern. `*` matches within one component;
    /// `**` matches zero or more components.
    #[serde(default)]
    pub subfolder_pattern: Option<String>,
    #[serde(default)]
    pub actions: Vec<AutoAction>,
    #[serde(default)]
    pub consecutive_failures: u8,
    #[serde(default)]
    pub disabled_reason: Option<String>,
}

impl AutoRule {
    pub fn matches(&self, path: &Path, size: u64) -> bool {
        if !self.enabled
            || size < self.min_size
            || !path.is_file()
            || !path.starts_with(&self.root)
            || is_dated_output(path, &self.root)
            || is_bridgecraft_output(path)
            || self.actions.iter().any(|action| {
                matches!(action, AutoAction::Export { .. })
                    && path.parent() == Some(self.root.as_path())
            })
        {
            return false;
        }
        if !self.extensions.is_empty() {
            let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
                return false;
            };
            if !self.extensions.iter().any(|allowed| {
                allowed
                    .trim_start_matches('.')
                    .eq_ignore_ascii_case(extension)
            }) {
                return false;
            }
        }
        let Some(pattern) = self.subfolder_pattern.as_deref() else {
            return true;
        };
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return false;
        };
        let Some(parent) = relative.parent() else {
            return false;
        };
        glob_path_matches(pattern, parent)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AutoAction {
    GenerateThumbnail {
        max_edge: u32,
    },
    Rename {
        pattern: String,
    },
    Export {
        max_edge: u32,
        format: AutoExportFormat,
    },
    MoveToDatedFolder,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AutoExportFormat {
    Jpeg,
    Png,
    Webp,
}

impl From<AutoExportFormat> for ExportFormat {
    fn from(value: AutoExportFormat) -> Self {
        match value {
            AutoExportFormat::Jpeg => Self::Jpeg { quality: 85 },
            AutoExportFormat::Png => Self::Png,
            AutoExportFormat::Webp => Self::WebP,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutoConfig {
    pub version: u32,
    #[serde(default = "default_settle_millis")]
    pub settle_millis: u64,
    #[serde(default)]
    pub rules: Vec<AutoRule>,
    #[serde(default)]
    pub history: VecDeque<AutoHistoryEntry>,
}

fn default_settle_millis() -> u64 {
    DEFAULT_SETTLE.as_millis() as u64
}

impl Default for AutoConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            settle_millis: default_settle_millis(),
            rules: Vec::new(),
            history: VecDeque::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutoHistoryEntry {
    pub rule_id: String,
    pub rule_name: String,
    pub path: PathBuf,
    pub completed_at_unix: u64,
    pub result: std::result::Result<(), String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fingerprint {
    size: u64,
    modified: Option<SystemTime>,
}

#[derive(Debug, Clone, Copy)]
struct PendingFile {
    fingerprint: Fingerprint,
    since: Duration,
}

#[derive(Default)]
struct State {
    config: AutoConfig,
    pending: HashMap<PathBuf, PendingFile>,
    suppressed_outputs: HashMap<PathBuf, Fingerprint>,
    history: VecDeque<AutoHistoryEntry>,
    queued: usize,
}

struct Shared {
    shutdown: AtomicBool,
    state: Mutex<State>,
    queue: Mutex<VecDeque<(AutoRule, PathBuf)>>,
    wake: Condvar,
    cache_path: PathBuf,
    journal_path: PathBuf,
    config_path: PathBuf,
    rename_lock: Mutex<()>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Persistent, settle-gated watched-folder processor with a bounded worker pool.
pub struct AutoProcessor {
    shared: Arc<Shared>,
    watchers: Vec<(String, PathBuf, Watcher)>,
    config_path: PathBuf,
    workers: Vec<JoinHandle<()>>,
    settle: Duration,
}

impl AutoProcessor {
    /// Open persisted configuration and create up to four action workers.
    pub fn open(config_path: &Path, cache_path: PathBuf, journal_path: PathBuf) -> Result<Self> {
        let config = load_config(config_path)?;
        let history = config.history.clone();
        let settle = Duration::from_millis(config.settle_millis);
        let state = State {
            config,
            history,
            ..State::default()
        };
        let shared = Arc::new(Shared {
            shutdown: AtomicBool::new(false),
            state: Mutex::new(state),
            queue: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            cache_path,
            journal_path,
            config_path: config_path.to_path_buf(),
            rename_lock: Mutex::new(()),
        });
        let workers = (0..MAX_WORKERS)
            .map(|worker_id| {
                let shared = Arc::clone(&shared);
                std::thread::Builder::new()
                    .name(format!("auto-action-{worker_id}"))
                    .spawn(move || worker_loop(shared))
                    .map_err(CoreError::Io)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut processor = Self {
            shared,
            watchers: Vec::new(),
            config_path: config_path.to_path_buf(),
            workers,
            settle,
        };
        processor.sync_watchers()?;
        Ok(processor)
    }

    /// Open using BridgeCraft's normal per-user config and data directories.
    pub fn open_default(cache_path: Option<PathBuf>) -> Result<Self> {
        let config_path = default_config_path()?;
        let data_dir = config_path
            .parent()
            .ok_or_else(|| CoreError::InvalidOperation("auto config has no parent".into()))?;
        let cache_path = cache_path.unwrap_or_else(|| data_dir.join("thumbnails.sqlite"));
        let journal_path = data_dir.join("last-rename.json");
        Self::open(&config_path, cache_path, journal_path)
    }

    /// Open with an explicit settle duration, intended for deterministic tests
    /// and advanced configuration. Production defaults to two seconds.
    pub fn settle_duration(&self) -> Duration {
        self.settle
    }

    pub fn with_settle(mut self, settle: Duration) -> Self {
        self.settle = settle;
        let mut state = lock(&self.shared.state);
        state.config.settle_millis = settle.as_millis().min(u64::MAX as u128) as u64;
        if let Err(error) = save_config(&self.config_path, &state.config) {
            tracing::error!("could not persist auto-processing settle duration: {error}");
        }
        drop(state);
        self
    }

    pub fn set_settle(&mut self, settle: Duration) -> Result<()> {
        let millis = settle.as_millis().min(u64::MAX as u128) as u64;
        let mut state = lock(&self.shared.state);
        state.config.settle_millis = millis;
        save_config(&self.config_path, &state.config)?;
        self.settle = Duration::from_millis(millis);
        Ok(())
    }

    pub fn rules(&self) -> Vec<AutoRule> {
        lock(&self.shared.state).config.rules.clone()
    }

    pub fn add_rule(&mut self, mut rule: AutoRule) -> Result<()> {
        validate_rule(&rule)?;
        {
            let mut state = lock(&self.shared.state);
            if let Some(existing) = state.config.rules.iter().find(|old| old.id == rule.id) {
                if existing.consecutive_failures >= MAX_CONSECUTIVE_FAILURES && rule.enabled {
                    rule.consecutive_failures = 0;
                    rule.disabled_reason = None;
                }
            }
            if let Some(existing) = state.config.rules.iter_mut().find(|old| old.id == rule.id) {
                *existing = rule;
            } else {
                state.config.rules.push(rule);
            }
            save_config(&self.config_path, &state.config)?;
        }
        self.sync_watchers()
    }

    pub fn set_rule_enabled(&mut self, id: &str, enabled: bool) -> Result<bool> {
        {
            let mut state = lock(&self.shared.state);
            let Some(rule) = state.config.rules.iter_mut().find(|rule| rule.id == id) else {
                return Ok(false);
            };
            rule.enabled = enabled;
            if enabled {
                rule.consecutive_failures = 0;
                rule.disabled_reason = None;
            }
            save_config(&self.config_path, &state.config)?;
        }
        self.sync_watchers()?;
        Ok(true)
    }

    pub fn remove_rule(&mut self, id: &str) -> Result<bool> {
        let removed = {
            let mut state = lock(&self.shared.state);
            let before = state.config.rules.len();
            state.config.rules.retain(|rule| rule.id != id);
            save_config(&self.config_path, &state.config)?;
            before != state.config.rules.len()
        };
        self.sync_watchers()?;
        Ok(removed)
    }

    pub fn history(&self) -> Vec<AutoHistoryEntry> {
        lock(&self.shared.state).history.iter().cloned().collect()
    }

    pub fn queued_count(&self) -> usize {
        lock(&self.shared.state).queued
    }

    pub fn pending_count(&self) -> usize {
        lock(&self.shared.state).pending.len()
    }

    pub fn watches_roots(&self) -> bool {
        !self.watchers.is_empty()
    }

    /// Drain watched roots and settle candidates. Call periodically from the UI
    /// event loop; file I/O and action execution are off-thread.
    pub fn poll(&mut self, now: Duration) {
        for (rule_id, root, watcher) in &self.watchers {
            for event in watcher.drain() {
                match event {
                    Ok(event) => {
                        for path in event.paths {
                            if should_ignore_path(&path) || !path.starts_with(root) {
                                continue;
                            }
                            if let Some(file_fingerprint) = fingerprint(&path) {
                                let mut state = lock(&self.shared.state);
                                if state.suppressed_outputs.get(&path) == Some(&file_fingerprint) {
                                    continue;
                                }
                                state.suppressed_outputs.remove(&path);
                                if state.pending.len() < MAX_PENDING_FILES
                                    || state.pending.contains_key(&path)
                                {
                                    state.pending.insert(
                                        path,
                                        PendingFile {
                                            fingerprint: file_fingerprint,
                                            since: now,
                                        },
                                    );
                                }
                            }
                        }
                    }
                    Err(error) => tracing::warn!("auto watcher {rule_id} reported error: {error}"),
                }
            }
        }
        self.settle_pending(now);
    }

    /// Inject an observed path into the settle queue. Also useful to event
    /// adapters other than `notify`; deterministic tests need no sleeps.
    pub fn observe(&self, path: PathBuf, now: Duration) {
        if should_ignore_path(&path) {
            return;
        }
        if let Some(file_fingerprint) = fingerprint(&path) {
            let mut state = lock(&self.shared.state);
            if state.suppressed_outputs.get(&path) == Some(&file_fingerprint) {
                return;
            }
            state.suppressed_outputs.remove(&path);
            if state.pending.len() < MAX_PENDING_FILES || state.pending.contains_key(&path) {
                state.pending.insert(
                    path,
                    PendingFile {
                        fingerprint: file_fingerprint,
                        since: now,
                    },
                );
            }
        }
    }

    pub fn settle_pending(&self, now: Duration) {
        let ready: Vec<(PathBuf, Fingerprint)> = {
            let mut state = lock(&self.shared.state);
            let paths: Vec<PathBuf> = state.pending.keys().cloned().collect();
            let mut ready = Vec::new();
            for path in paths {
                let current = fingerprint(&path);
                let Some(pending) = state.pending.get_mut(&path) else {
                    continue;
                };
                if current != Some(pending.fingerprint) {
                    if let Some(current) = current {
                        pending.fingerprint = current;
                        pending.since = now;
                    } else {
                        state.pending.remove(&path);
                    }
                    continue;
                }
                if now.saturating_sub(pending.since) >= self.settle {
                    ready.push((path.clone(), pending.fingerprint));
                    state.pending.remove(&path);
                }
            }
            ready
        };

        for (path, settled_fingerprint) in ready {
            if fingerprint(&path) != Some(settled_fingerprint) {
                self.observe(path, now);
                continue;
            }
            let matching = {
                let state = lock(&self.shared.state);
                state
                    .config
                    .rules
                    .iter()
                    .filter(|rule| rule.matches(&path, settled_fingerprint.size))
                    .cloned()
                    .collect::<Vec<_>>()
            };
            if matching.is_empty() {
                continue;
            }
            let mut queue = lock(&self.shared.queue);
            let available = MAX_QUEUED_ACTIONS.saturating_sub(queue.len());
            let accepted = matching.len().min(available);
            queue.extend(
                matching
                    .into_iter()
                    .take(accepted)
                    .map(|rule| (rule, path.clone())),
            );
            if accepted > 0 {
                let mut state = lock(&self.shared.state);
                state.queued = state.queued.saturating_add(accepted);
                self.shared.wake.notify_all();
            }
        }
    }

    fn sync_watchers(&mut self) -> Result<()> {
        let rules = self.rules();
        let mut watchers = Vec::new();
        let mut watched_roots = HashSet::new();
        for rule in rules.iter().filter(|rule| rule.enabled) {
            if !watched_roots.insert(rule.root.clone()) {
                continue;
            }
            let watcher = Watcher::new(&rule.root)?;
            watchers.push((rule.id.clone(), rule.root.clone(), watcher));
        }
        self.watchers = watchers;
        Ok(())
    }
}

impl Drop for AutoProcessor {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.wake.notify_all();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn worker_loop(shared: Arc<Shared>) {
    loop {
        let task = {
            let mut queue = lock(&shared.queue);
            while queue.is_empty() {
                if shared.shutdown.load(Ordering::Acquire) {
                    return;
                }
                let waited = shared
                    .wake
                    .wait_timeout(queue, Duration::from_millis(250))
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                queue = waited.0;
            }
            queue.pop_front()
        };
        let Some((rule, path)) = task else {
            continue;
        };
        let result = run_actions(&shared, &rule, &path);
        let mut state = lock(&shared.state);
        state.queued = state.queued.saturating_sub(1);
        record_result(&mut state, &rule, &path, result);
        if let Err(error) = save_config(&shared.config_path, &state.config) {
            tracing::error!("could not persist auto-processing rule state: {error}");
        }
        drop(state);
    }
}

fn record_result(
    state: &mut State,
    rule: &AutoRule,
    path: &Path,
    result: std::result::Result<(), String>,
) {
    if let Some(stored) = state
        .config
        .rules
        .iter_mut()
        .find(|stored| stored.id == rule.id)
    {
        match &result {
            Err(_) => {
                stored.consecutive_failures = stored.consecutive_failures.saturating_add(1);
                if stored.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    stored.enabled = false;
                    stored.disabled_reason = Some(format!(
                        "disabled after {} consecutive failures",
                        stored.consecutive_failures
                    ));
                }
            }
            Ok(()) => {
                stored.consecutive_failures = 0;
                stored.disabled_reason = None;
            }
        }
    }
    let completed_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    state.history.push_back(AutoHistoryEntry {
        rule_id: rule.id.clone(),
        rule_name: rule.name.clone(),
        path: path.to_path_buf(),
        completed_at_unix,
        result,
    });
    while state.history.len() > HISTORY_LIMIT {
        state.history.pop_front();
    }
    state.config.history.clone_from(&state.history);
}

fn run_actions(shared: &Shared, rule: &AutoRule, path: &Path) -> std::result::Result<(), String> {
    let mut current_path = path.to_path_buf();
    for action in &rule.actions {
        match action {
            AutoAction::GenerateThumbnail { max_edge } => {
                let metadata =
                    std::fs::metadata(&current_path).map_err(|error| error.to_string())?;
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| duration.as_nanos() as i64)
                    .unwrap_or(0);
                let cache = bc_thumbnails::CacheStore::open(&shared.cache_path)
                    .map_err(|error| error.to_string())?;
                bc_thumbnails::load_or_generate(
                    Some(&cache),
                    &current_path,
                    modified,
                    metadata.len(),
                    *max_edge,
                )
                .map_err(|error| error.to_string())?;
            }
            AutoAction::Rename { pattern } => {
                let _rename_guard = lock(&shared.rename_lock);
                let parsed = Pattern::parse(pattern).map_err(|error| error.to_string())?;
                let context = RenameContext::from_path(current_path.clone(), 0);
                let plan =
                    batch_rename::plan(&parsed, &[context]).map_err(|error| error.to_string())?;
                batch_rename::apply(&plan, &shared.journal_path)
                    .map_err(|error| error.to_string())?;
                if let Some(item) = plan.first() {
                    current_path.clone_from(&item.target);
                }
            }
            AutoAction::Export { max_edge, format } => {
                let output = export::decode_for_export(&current_path, *max_edge)
                    .and_then(|image| export::encode_image(&image, (*format).into()))
                    .map_err(|error| error.to_string())?;
                let format: ExportFormat = (*format).into();
                let parent = current_path.parent().unwrap_or_else(|| Path::new("."));
                let target = export::unique_export_name(
                    parent,
                    &current_path,
                    *max_edge,
                    format,
                    Path::exists,
                )
                .ok_or_else(|| "no available export filename".to_string())?;
                write_atomic(&target, &output).map_err(|error| error.to_string())?;
                if let Some(output_fingerprint) = fingerprint(&target) {
                    lock(&shared.state)
                        .suppressed_outputs
                        .insert(target, output_fingerprint);
                }
            }
            AutoAction::MoveToDatedFolder => {
                let metadata =
                    std::fs::metadata(&current_path).map_err(|error| error.to_string())?;
                let modified = metadata.modified().map_err(|error| error.to_string())?;
                let seconds = modified
                    .duration_since(UNIX_EPOCH)
                    .map_err(|error| error.to_string())?
                    .as_secs() as i64;
                let (year, month, day) = date_parts(seconds);
                let dated_folder = rule
                    .root
                    .join(format!("{year:04}"))
                    .join(format!("{month:02}-{day:02}"));
                std::fs::create_dir_all(&dated_folder).map_err(|error| error.to_string())?;
                let name = current_path
                    .file_name()
                    .ok_or_else(|| "file has no name".to_string())?;
                let target = dated_folder.join(name);
                if target == current_path {
                    continue;
                }
                if !target.starts_with(&rule.root) {
                    return Err("dated destination escapes the watched root".into());
                }
                if target.exists() {
                    return Err(format!("destination already exists: {}", target.display()));
                }
                std::fs::rename(&current_path, &target).map_err(|error| error.to_string())?;
                if let Some(output_fingerprint) = fingerprint(&target) {
                    lock(&shared.state)
                        .suppressed_outputs
                        .insert(target.clone(), output_fingerprint);
                }
                current_path = target;
            }
        }
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let unique = format!(
        ".{name}.bridgecraft-tmp-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("worker")
    );
    let temporary = parent.join(unique);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    use std::io::Write as _;
    let write_result = (|| {
        file.write_all(bytes)?;
        file.sync_all()
    })();
    drop(file);
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

fn fingerprint(path: &Path) -> Option<Fingerprint> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    Some(Fingerprint {
        size: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn should_ignore_path(path: &Path) -> bool {
    is_bridgecraft_output(path)
        || path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.ends_with(".tmp") || name.ends_with(".part") || name.ends_with(".partial")
            })
}

fn is_bridgecraft_output(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.contains(".bridgecraft-tmp"))
}

fn is_dated_output(path: &Path, root: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let mut components = relative.components();
    let Some(Component::Normal(year)) = components.next() else {
        return false;
    };
    let Some(Component::Normal(month_day)) = components.next() else {
        return false;
    };
    year.to_string_lossy().parse::<u32>().is_ok()
        && month_day
            .to_string_lossy()
            .split_once('-')
            .is_some_and(|(month, day)| month.parse::<u32>().is_ok() && day.parse::<u32>().is_ok())
}

fn validate_rule(rule: &AutoRule) -> Result<()> {
    if rule.id.trim().is_empty() || rule.name.trim().is_empty() {
        return Err(CoreError::InvalidOperation(
            "auto rule id and name must not be empty".into(),
        ));
    }
    if !rule.root.is_dir() {
        return Err(CoreError::PathNotFound(rule.root.display().to_string()));
    }
    if rule.actions.is_empty() {
        return Err(CoreError::InvalidOperation(
            "auto rule needs at least one action".into(),
        ));
    }
    for action in &rule.actions {
        match action {
            AutoAction::GenerateThumbnail { max_edge } if *max_edge == 0 || *max_edge > 16_384 => {
                return Err(CoreError::InvalidOperation(
                    "thumbnail max edge must be 1..=16384".into(),
                ));
            }
            AutoAction::Export { max_edge, .. } if !(16..=16_384).contains(max_edge) => {
                return Err(CoreError::InvalidOperation(
                    "export max edge must be 16..=16384".into(),
                ));
            }
            AutoAction::Rename { pattern } => {
                Pattern::parse(pattern)
                    .map_err(|error| CoreError::InvalidOperation(error.to_string()))?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn glob_path_matches(pattern: &str, relative: &Path) -> bool {
    let pattern: Vec<&str> = pattern.split('/').filter(|part| !part.is_empty()).collect();
    let path: Vec<String> = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    glob_components(&pattern, &path)
}

fn glob_components(pattern: &[&str], path: &[String]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => {
            glob_components(rest, path)
                || path
                    .split_first()
                    .is_some_and(|(_, tail)| glob_components(pattern, tail))
        }
        Some((component, rest)) => path.split_first().is_some_and(|(name, tail)| {
            wildcard_component_matches(component, name) && glob_components(rest, tail)
        }),
    }
}

fn wildcard_component_matches(pattern: &str, value: &str) -> bool {
    let mut rest = value;
    let mut anchored_at_start = !pattern.starts_with('*');
    for part in pattern.split('*').filter(|part| !part.is_empty()) {
        let Some(position) = rest.find(part) else {
            return false;
        };
        if anchored_at_start && position != 0 {
            return false;
        }
        rest = &rest[position + part.len()..];
        anchored_at_start = false;
    }
    pattern.ends_with('*') || rest.is_empty()
}

fn date_parts(seconds: i64) -> (i64, u32, u32) {
    let days = seconds.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

pub fn default_config_path() -> Result<PathBuf> {
    let path = if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    };
    path.map(|path| path.join("BridgeCraft").join("auto-rules.json"))
        .ok_or_else(|| CoreError::InvalidOperation("could not locate user config directory".into()))
}

fn load_config(path: &Path) -> Result<AutoConfig> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut config: AutoConfig = serde_json::from_slice(&bytes).map_err(|error| {
                CoreError::InvalidOperation(format!("invalid auto-rule config: {error}"))
            })?;
            if config.version != CONFIG_VERSION {
                return Err(CoreError::InvalidOperation(format!(
                    "unsupported auto-rule config version {}",
                    config.version
                )));
            }
            for rule in &mut config.rules {
                if let Err(error) = validate_rule(rule) {
                    rule.enabled = false;
                    rule.disabled_reason = Some(format!("invalid persisted rule: {error}"));
                }
            }
            Ok(config)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(AutoConfig::default()),
        Err(error) => Err(error.into()),
    }
}

fn save_config(path: &Path, config: &AutoConfig) -> Result<()> {
    let json = serde_json::to_vec_pretty(config).map_err(|error| {
        CoreError::InvalidOperation(format!("could not encode auto-rules: {error}"))
    })?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    write_atomic(path, &json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(root: &Path) -> AutoRule {
        AutoRule {
            id: "ingest".into(),
            name: "Camera ingest".into(),
            root: root.to_path_buf(),
            enabled: true,
            extensions: vec!["jpg".into(), "dng".into()],
            min_size: 2,
            subfolder_pattern: Some("incoming/**".into()),
            actions: vec![AutoAction::MoveToDatedFolder],
            consecutive_failures: 0,
            disabled_reason: None,
        }
    }

    #[test]
    fn matching_checks_enabled_size_extension_root_and_subfolder() {
        let temp = tempfile::tempdir().unwrap();
        let incoming = temp.path().join("incoming/camera");
        std::fs::create_dir_all(&incoming).unwrap();
        let jpg = incoming.join("shot.JPG");
        std::fs::write(&jpg, b"123").unwrap();
        let rule = rule(temp.path());
        assert!(rule.matches(&jpg, 3));
        assert!(!rule.matches(&jpg, 1));
        assert!(!rule.matches(&incoming.join("shot.png"), 3));
        assert!(!rule.matches(&temp.path().join("elsewhere/shot.jpg"), 3));
        let mut disabled = rule.clone();
        disabled.enabled = false;
        assert!(!disabled.matches(&jpg, 3));
    }

    #[test]
    fn component_globs_support_star_and_double_star() {
        let relative = Path::new("incoming/camera/day1");
        assert!(glob_path_matches("incoming/**", relative));
        assert!(glob_path_matches("incoming/*/day*", relative));
        assert!(!glob_path_matches("incoming/*", relative));
        assert!(!wildcard_component_matches("day*", "otherday"));
        assert!(wildcard_component_matches("day*", "day-one"));
        assert!(wildcard_component_matches("*day", "otherday"));
        assert!(is_dated_output(
            Path::new("/watch/2026/11-11/photo.jpg"),
            Path::new("/watch")
        ));
    }

    #[test]
    fn settle_gate_restarts_when_fingerprint_changes_and_ignores_temp_files() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("capture.jpg");
        std::fs::write(&file, b"a").unwrap();
        let config_path = temp.path().join("settings/auto-rules.json");
        let processor = AutoProcessor::open(
            &config_path,
            temp.path().join("cache.sqlite"),
            temp.path().join("settings/last-rename.json"),
        )
        .unwrap()
        .with_settle(Duration::from_millis(40));
        assert_eq!(load_config(&config_path).unwrap().settle_millis, 40);
        processor.observe(file.clone(), Duration::ZERO);
        processor.settle_pending(Duration::from_millis(20));
        assert_eq!(lock(&processor.shared.state).pending.len(), 1);
        std::fs::write(&file, b"changed").unwrap();
        processor.settle_pending(Duration::from_millis(41));
        assert_eq!(lock(&processor.shared.state).pending.len(), 1);
        processor.settle_pending(Duration::from_millis(82));
        assert!(lock(&processor.shared.state).pending.is_empty());

        let temporary = temp.path().join(".capture.jpg.bridgecraft-tmp");
        std::fs::write(&temporary, b"x").unwrap();
        processor.observe(temporary, Duration::from_secs(1));
        assert!(lock(&processor.shared.state).pending.is_empty());
    }

    #[test]
    fn config_round_trips_and_rejects_unknown_versions() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rules.json");
        let mut config = AutoConfig::default();
        config.rules.push(rule(temp.path()));
        config.settle_millis = 3500;
        save_config(&path, &config).unwrap();
        assert_eq!(load_config(&path).unwrap(), config);
        config.version += 1;
        save_config(&path, &config).unwrap();
        assert!(load_config(&path).is_err());
    }

    #[test]
    fn tenth_consecutive_failure_disables_rule_and_success_resets_count() {
        let mut state = State::default();
        let configured = rule(Path::new("."));
        state.config.rules.push(configured.clone());
        for _ in 0..9 {
            record_result(
                &mut state,
                &configured,
                Path::new("capture.jpg"),
                Err("failure".into()),
            );
        }
        assert!(state.config.rules[0].enabled);
        record_result(
            &mut state,
            &configured,
            Path::new("capture.jpg"),
            Err("failure".into()),
        );
        assert!(!state.config.rules[0].enabled);
        assert_eq!(state.config.rules[0].consecutive_failures, 10);
        assert!(state.config.rules[0]
            .disabled_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("10 consecutive failures")));
        record_result(&mut state, &configured, Path::new("capture.jpg"), Ok(()));
        assert_eq!(state.config.rules[0].consecutive_failures, 0);
        assert_eq!(state.history.len(), 11);
    }

    #[test]
    fn actions_rename_then_run_on_renamed_path_and_export_without_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("original.jpg");
        std::fs::write(&file, tiny_jpeg()).unwrap();
        let shared = Shared {
            shutdown: AtomicBool::new(false),
            state: Mutex::new(State::default()),
            queue: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            cache_path: temp.path().join("thumbs.sqlite"),
            journal_path: temp.path().join("last-rename.json"),
            config_path: temp.path().join("auto-rules.json"),
            rename_lock: Mutex::new(()),
        };
        let mut configured = rule(temp.path());
        configured.actions = vec![
            AutoAction::Rename {
                pattern: "{date:%Y-%m-%d}_{name}".into(),
            },
            AutoAction::Export {
                max_edge: 32,
                format: AutoExportFormat::Png,
            },
        ];
        run_actions(&shared, &configured, &file).unwrap();
        assert!(!file.exists());
        let renamed = std::fs::read_dir(temp.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|ext| ext == "jpg"))
            .unwrap();
        assert!(renamed.file_stem().unwrap().to_string_lossy().contains('_'));
        let export = renamed.with_file_name(format!(
            "{}_32.png",
            renamed.file_stem().unwrap().to_string_lossy()
        ));
        assert!(export.exists());
    }

    fn tiny_jpeg() -> Vec<u8> {
        let image = image::RgbImage::from_pixel(2, 2, image::Rgb([12, 34, 56]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        bytes
    }
}
