//! Queued background execution for the batch export pipeline (issue #15).
//!
//! [`ExportQueue`] owns a bounded worker pool (default
//! [`crate::export::default_worker_count`], i.e. half the cores) that drains
//! per-file export tasks. All state — per-file progress, completion counts,
//! cancellation — lives behind one registry lock here, so the UI can stay a
//! thin polling view ([`ExportQueue::snapshot`]) and every rule is testable
//! headless.
//!
//! Guarantees:
//! - **No truncated output.** Pixels are encoded to memory, written to a
//!   hidden temp file beside the target, then renamed onto the final name.
//! - **Never silently overwrite.** Output names follow
//!   `basename_MAXEDGE.ext` with `_1`, `_2`, … collision suffixes, checked
//!   against both disk and the names in-flight jobs have reserved, and
//!   re-checked under the registry lock at publish time.
//! - **Cancellation is honoured between files and before publish.** Already
//!   finished outputs are kept; anything in flight discards its temp file.
//!
//! The one documented race window: between the final availability check and
//! the rename, a *different process* could create the same name. BridgeCraft
//! never overwrites its own planned outputs, and every publish re-checks
//! immediately before renaming.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::export::{self, ExportFormat, ExportRequest, Result};

/// Cap so a stray request cannot spawn hundreds of decode threads.
const MAX_WORKERS: usize = 64;

/// Lifecycle of one file inside an export job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileState {
    Queued,
    Running,
    Done { output: PathBuf },
    Failed { message: String },
    Cancelled,
}

impl FileState {
    fn is_terminal(&self) -> bool {
        !matches!(self, FileState::Queued | FileState::Running)
    }
}

/// One file's progress row, as the job list renders it.
#[derive(Debug, Clone)]
pub struct FileProgress {
    pub source: PathBuf,
    pub state: FileState,
}

/// Pollable summary of one submitted job.
#[derive(Debug, Clone)]
pub struct JobSnapshot {
    pub id: u64,
    pub format: ExportFormat,
    pub max_edge: u32,
    /// Destination folder, or `"beside originals"`.
    pub destination: String,
    pub files: Vec<FileProgress>,
    pub total: usize,
    pub completed: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub queued: usize,
    pub running: usize,
    pub cancel_requested: bool,
    pub finished: bool,
}

/// Token returned by [`ExportQueue::submit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobHandle {
    pub id: u64,
}

struct FileRecord {
    source: PathBuf,
    /// Planned output while Queued/Running (its name is reserved); `None`
    /// once the file reached a terminal state.
    planned: Option<PathBuf>,
    state: FileState,
}

struct JobRecord {
    id: u64,
    format: ExportFormat,
    max_edge: u32,
    destination_label: String,
    files: Vec<FileRecord>,
    cancel: Arc<AtomicBool>,
}

struct Task {
    job: u64,
    file: usize,
}

/// Jobs plus the set of output names currently reserved by queued or
/// running files. One lock, so planning a name and reporting progress can
/// never disagree about what is taken.
#[derive(Default)]
struct Registry {
    jobs: Vec<JobRecord>,
    reserved: HashSet<PathBuf>,
}

struct Pool {
    registry: Mutex<Registry>,
    pending: Mutex<VecDeque<Task>>,
    ready: Condvar,
    shutdown: AtomicBool,
    next_id: AtomicU64,
}

fn lock<T>(slot: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panicking worker would poison the lock; the registry holds no
    // invariants a panic corrupts beyond a lost name reservation, so keep
    // the queue running rather than taking the whole app down with it.
    slot.lock().unwrap_or_else(|e| e.into_inner())
}

/// Bounded worker pool that executes export jobs. The UI holds one for the
/// session lifetime.
pub struct ExportQueue {
    pool: Arc<Pool>,
    /// Detached on drop: a worker finishing its current file is no reason to
    /// block application shutdown, and interrupted work can only ever be a
    /// hidden temp file, never a truncated output.
    _workers: Vec<JoinHandle<()>>,
}

impl ExportQueue {
    /// Spawn a pool of `workers` export threads (clamped to 1..=64).
    pub fn new(workers: usize) -> Self {
        let pool = Arc::new(Pool {
            registry: Mutex::new(Registry::default()),
            pending: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            shutdown: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
        });
        let count = workers.clamp(1, MAX_WORKERS);
        let _workers = (0..count)
            .map(|id| {
                let pool = Arc::clone(&pool);
                std::thread::Builder::new()
                    .name(format!("export-{id}"))
                    .spawn(move || worker_loop(&pool, id))
                    .expect("failed to spawn export worker")
            })
            .collect();
        Self { pool, _workers }
    }

    /// Validate the batch, plan one collision-free output name per file,
    /// and enqueue it. Batch-level problems return `Err` without touching
    /// the queue; per-file problems become instantly-`Failed` progress rows.
    pub fn submit(&self, request: ExportRequest) -> Result<JobHandle> {
        request.validate()?;
        let id = self.pool.next_id.fetch_add(1, Ordering::Relaxed);
        let queued: Vec<Task> = {
            let mut registry = lock(&self.pool.registry);
            let mut files = Vec::with_capacity(request.sources.len());
            for source in &request.sources {
                let record = plan_file(source, &request, &mut registry.reserved);
                if let Some(planned) = &record.planned {
                    registry.reserved.insert(planned.clone());
                }
                files.push(record);
            }
            let queued = (0..files.len())
                .filter(|&i| files[i].state == FileState::Queued)
                .map(|file| Task { job: id, file })
                .collect();
            registry.jobs.push(JobRecord {
                id,
                format: request.format,
                max_edge: request.max_edge,
                destination_label: match &request.destination {
                    Some(dir) => dir.display().to_string(),
                    None => "beside originals".to_string(),
                },
                files,
                cancel: Arc::new(AtomicBool::new(false)),
            });
            queued
        };

        if !queued.is_empty() {
            lock(&self.pool.pending).extend(queued);
            self.pool.ready.notify_all();
        }
        Ok(JobHandle { id })
    }

    /// Ask a job to stop. Queued files become [`FileState::Cancelled`]
    /// immediately; the file in flight checks the flag before publishing
    /// and discards its temp file. Completed outputs are never removed.
    pub fn cancel(&self, id: u64) {
        let mut registry = lock(&self.pool.registry);
        let Some(index) = registry.jobs.iter().position(|job| job.id == id) else {
            return;
        };
        let job = &mut registry.jobs[index];
        job.cancel.store(true, Ordering::Relaxed);
        let mut released = Vec::new();
        for record in &mut job.files {
            if record.state == FileState::Queued {
                if let Some(planned) = record.planned.take() {
                    released.push(planned);
                }
                record.state = FileState::Cancelled;
            }
        }
        for planned in released {
            registry.reserved.remove(&planned);
        }
    }

    /// Snapshot of every remembered job, oldest first.
    pub fn snapshot(&self) -> Vec<JobSnapshot> {
        lock(&self.pool.registry)
            .jobs
            .iter()
            .map(snapshot_of)
            .collect()
    }

    /// True while any job still has queued or running files.
    pub fn has_active(&self) -> bool {
        lock(&self.pool.registry)
            .jobs
            .iter()
            .any(|job| job.files.iter().any(|f| !f.state.is_terminal()))
    }

    /// Drop finished jobs from the job list (their outputs stay on disk).
    /// A finished job has no pending tasks and no live reservations.
    pub fn clear_finished(&self) {
        lock(&self.pool.registry)
            .jobs
            .retain(|job| job.files.iter().any(|f| !f.state.is_terminal()));
    }
}

fn snapshot_of(job: &JobRecord) -> JobSnapshot {
    let mut snap = JobSnapshot {
        id: job.id,
        format: job.format,
        max_edge: job.max_edge,
        destination: job.destination_label.clone(),
        files: Vec::with_capacity(job.files.len()),
        total: job.files.len(),
        completed: 0,
        failed: 0,
        cancelled: 0,
        queued: 0,
        running: 0,
        cancel_requested: job.cancel.load(Ordering::Relaxed),
        finished: true,
    };
    for file in &job.files {
        snap.files.push(FileProgress {
            source: file.source.clone(),
            state: file.state.clone(),
        });
        match &file.state {
            FileState::Queued => {
                snap.queued += 1;
                snap.finished = false;
            }
            FileState::Running => {
                snap.running += 1;
                snap.finished = false;
            }
            FileState::Done { .. } => snap.completed += 1,
            FileState::Failed { .. } => snap.failed += 1,
            FileState::Cancelled => snap.cancelled += 1,
        }
    }
    snap
}

/// Pre-flight one source: readable plain file, decodable, and a reserved
/// free output name. Anything else becomes an instant `Failed` row so one
/// bad file never rejects the whole batch.
fn plan_file(
    source: &Path,
    request: &ExportRequest,
    reserved: &mut HashSet<PathBuf>,
) -> FileRecord {
    let failed = |message: String| FileRecord {
        source: source.to_path_buf(),
        planned: None,
        state: FileState::Failed { message },
    };
    // Sources must be plain regular files: symlinks are refused here just as
    // in `file_ops`, so an export can never follow a link to something
    // outside the folder the user selected.
    match std::fs::symlink_metadata(source) {
        Ok(meta) if meta.is_file() => {}
        Ok(meta) if meta.file_type().is_symlink() => {
            return failed("symlinks cannot be exported directly".to_string())
        }
        Ok(_) => return failed("not a regular file".to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return failed("file no longer exists".to_string())
        }
        Err(e) => return failed(format!("cannot read file: {e}")),
    }
    if !bc_thumbnails::decoder::can_thumbnail(source) {
        return failed("format is not decodable by the thumbnail pipeline".to_string());
    }
    let folder = match &request.destination {
        Some(dir) => dir.clone(),
        None => match source.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => return failed("path has no parent folder".to_string()),
        },
    };
    let taken = |path: &Path| reserved.contains(path) || path.exists();
    match export::unique_export_name(&folder, source, request.max_edge, request.format, taken) {
        Some(planned) => FileRecord {
            source: source.to_path_buf(),
            planned: Some(planned),
            state: FileState::Queued,
        },
        None => failed("no free output name in the destination folder".to_string()),
    }
}

fn worker_loop(pool: &Pool, worker_id: usize) {
    while let Some(task) = wait_for_task(pool) {
        process_task(pool, worker_id, task);
    }
}

fn wait_for_task(pool: &Pool) -> Option<Task> {
    let mut pending = lock(&pool.pending);
    loop {
        if let Some(task) = pending.pop_front() {
            return Some(task);
        }
        if pool.shutdown.load(Ordering::Relaxed) {
            return None;
        }
        pending = pool.ready.wait(pending).unwrap_or_else(|e| e.into_inner());
    }
}

fn process_task(pool: &Pool, worker_id: usize, task: Task) {
    // Claim the file under the registry lock; cancelled or already-terminal
    // files are skipped without ever touching a decoder.
    let claim = {
        let registry = lock(&pool.registry);
        let Some(job) = registry.jobs.iter().find(|job| job.id == task.job) else {
            return;
        };
        let Some(record) = job.files.get(task.file) else {
            return;
        };
        if record.state != FileState::Queued || job.cancel.load(Ordering::Relaxed) {
            return;
        }
        let Some(planned) = record.planned.clone() else {
            return;
        };
        (
            record.source.clone(),
            planned,
            job.format,
            job.max_edge,
            Arc::clone(&job.cancel),
        )
    };
    let (source, planned, format, max_edge, cancel) = claim;

    {
        let mut registry = lock(&pool.registry);
        let Some(record) = find_file_mut(&mut registry, task.job, task.file) else {
            return;
        };
        if record.state != FileState::Queued {
            return; // cancelled in the claim gap
        }
        record.state = FileState::Running;
    }

    let final_state = match prepare_export(&source, &planned, format, max_edge, &cancel, worker_id)
    {
        Prepared::Ready(tmp) => publish(pool, task.job, task.file, &planned, &tmp, &cancel),
        // prepare_export removed any temp file it had created already.
        Prepared::Failed(message) => FileState::Failed { message },
        Prepared::Cancelled => FileState::Cancelled,
    };

    let mut registry = lock(&pool.registry);
    let released = find_file_mut(&mut registry, task.job, task.file).and_then(|r| r.planned.take());
    if let Some(record) = find_file_mut(&mut registry, task.job, task.file) {
        record.state = final_state;
    }
    if let Some(planned) = released {
        registry.reserved.remove(&planned);
    }
}

fn find_file_mut(registry: &mut Registry, job_id: u64, file: usize) -> Option<&mut FileRecord> {
    registry
        .jobs
        .iter_mut()
        .find(|job| job.id == job_id)
        .and_then(|job| job.files.get_mut(file))
}

/// Decode, encode to memory, then land the bytes in a hidden temp file
/// beside the target. `Ready(tmp)` hands the temp file to [`publish`];
/// every other arm has already removed any file it created.
enum Prepared {
    Ready(PathBuf),
    Failed(String),
    Cancelled,
}

fn prepare_export(
    source: &Path,
    planned: &Path,
    format: ExportFormat,
    max_edge: u32,
    cancel: &AtomicBool,
    worker_id: usize,
) -> Prepared {
    let cancelled_or = |message: String| {
        if cancel.load(Ordering::Relaxed) {
            Prepared::Cancelled
        } else {
            Prepared::Failed(message)
        }
    };
    let image = match export::decode_for_export(source, max_edge) {
        Ok(image) => image,
        Err(e) => return cancelled_or(e.to_string()),
    };
    if cancel.load(Ordering::Relaxed) {
        return Prepared::Cancelled;
    }
    let bytes = match export::encode_image(&image, format) {
        Ok(bytes) => bytes,
        Err(e) => return cancelled_or(e.to_string()),
    };
    if cancel.load(Ordering::Relaxed) {
        return Prepared::Cancelled;
    }
    let folder = planned.parent().unwrap_or(Path::new("."));
    let tmp = temp_path(folder, worker_id);
    match write_temp(&tmp, &bytes) {
        Ok(()) if cancel.load(Ordering::Relaxed) => {
            let _ = std::fs::remove_file(&tmp);
            Prepared::Cancelled
        }
        Ok(()) => Prepared::Ready(tmp),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Prepared::Failed(e.to_string())
        }
    }
}

/// Move the temp file onto a final, still-free name and record the result.
/// The collision scan re-runs under the registry lock because another job
/// may have claimed the planned name while this file was encoding.
fn publish(
    pool: &Pool,
    job_id: u64,
    file: usize,
    planned: &Path,
    tmp: &Path,
    cancel: &AtomicBool,
) -> FileState {
    if cancel.load(Ordering::Relaxed) {
        let _ = std::fs::remove_file(tmp);
        return FileState::Cancelled;
    }
    let registry = lock(&pool.registry);
    let info = registry
        .jobs
        .iter()
        .find(|job| job.id == job_id)
        .and_then(|job| {
            let record = job
                .files
                .get(file)
                .filter(|r| r.planned.as_deref() == Some(planned))?;
            Some((record.source.clone(), job.max_edge, job.format))
        });
    let Some((source, max_edge, format)) = info else {
        drop(registry);
        let _ = std::fs::remove_file(tmp);
        return FileState::Cancelled; // job cleared or file re-planned
    };
    let chosen = export::unique_export_name(
        planned.parent().unwrap_or(Path::new(".")),
        &source,
        max_edge,
        format,
        // This file's own reservation must not count as taken; it is the
        // name we are trying to fill.
        |path| (registry.reserved.contains(path) && path != planned) || path.exists(),
    );
    let Some(chosen) = chosen else {
        drop(registry);
        let _ = std::fs::remove_file(tmp);
        return FileState::Failed {
            message: "no free output name in the destination folder".to_string(),
        };
    };
    // Still under the lock, so no other job can claim `chosen` now that it
    // has been picked.
    match std::fs::rename(tmp, &chosen) {
        Ok(()) => FileState::Done { output: chosen },
        Err(e) => {
            drop(registry);
            let _ = std::fs::remove_file(tmp);
            FileState::Failed {
                message: format!("could not publish {}: {e}", chosen.display()),
            }
        }
    }
}

/// Hidden, unique, in-folder temp name: beside the target so the rename is
/// same-filesystem, dot-prefixed so it never shows in a browser listing.
fn temp_path(folder: &Path, worker_id: usize) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    folder.join(format!(
        ".bcexp-{}-{worker_id}-{}-{:0>20}.tmp",
        std::process::id(),
        nanos,
        rand_suffix()
    ))
}

/// Sub-nanosecond clock collisions on coarse timers are possible; mix in a
/// cheap per-call counter so two threads never pick the same temp name.
fn rand_suffix() -> u32 {
    use std::sync::atomic::AtomicU32;
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed) % 1_000_000
}

fn write_temp(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    // create_new: never clobbers anything, even a stale temp from a killed
    // run that happens to share the timestamp.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::fixtures::{write_oriented_jpeg, write_test_png};
    use std::time::Instant;

    /// Poll `f` until true; panic after 30 s so a broken invariant fails
    /// the test instead of hanging CI.
    fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
        let start = Instant::now();
        while !f() {
            assert!(start.elapsed().as_secs() < 30, "timeout waiting for {what}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn job_finished(queue: &ExportQueue, id: u64) -> Option<JobSnapshot> {
        queue
            .snapshot()
            .into_iter()
            .find(|job| job.id == id && job.finished)
    }

    fn temp_files_in(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".bcexp-"))
            .collect()
    }

    fn png_dims(path: &Path) -> (u32, u32) {
        image::image_dimensions(path).unwrap()
    }

    #[test]
    fn job_completes_and_writes_decodable_outputs() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("out");
        std::fs::create_dir(&out).unwrap();
        for name in ["a.png", "b.png", "c.png"] {
            write_test_png(&temp.path().join(name), 37, 11);
        }
        let queue = ExportQueue::new(2);
        let handle = queue
            .submit(ExportRequest {
                sources: ["a.png", "b.png", "c.png"]
                    .iter()
                    .map(|name| temp.path().join(name))
                    .collect(),
                destination: Some(out.clone()),
                format: ExportFormat::Png,
                max_edge: 32,
            })
            .unwrap();
        wait_until("job to finish", || {
            job_finished(&queue, handle.id).is_some()
        });
        let snap = job_finished(&queue, handle.id).unwrap();
        assert_eq!((snap.completed, snap.failed, snap.cancelled), (3, 0, 0));
        for name in ["a_32.png", "b_32.png", "c_32.png"] {
            let output = out.join(name);
            assert!(output.exists(), "{name} missing");
            assert_eq!(png_dims(&output), (32, 10) /* 37:11 fitted to 32 */);
        }
        assert!(temp_files_in(&out).is_empty());
    }

    #[test]
    fn names_stay_unique_within_and_across_jobs() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("out");
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        std::fs::create_dir_all(&left).unwrap();
        std::fs::create_dir_all(&right).unwrap();
        std::fs::create_dir(&out).unwrap();
        // Same stem from two folders, plus one already on disk.
        write_test_png(&left.join("shot.png"), 20, 20);
        write_test_png(&right.join("shot.png"), 20, 20);
        std::fs::write(out.join("shot_64.png"), b"KEEP").unwrap();

        let queue = ExportQueue::new(1);
        let a = queue
            .submit(ExportRequest {
                sources: vec![left.join("shot.png")],
                destination: Some(out.clone()),
                format: ExportFormat::Png,
                max_edge: 64,
            })
            .unwrap();
        let b = queue
            .submit(ExportRequest {
                sources: vec![right.join("shot.png")],
                destination: Some(out.clone()),
                format: ExportFormat::Png,
                max_edge: 64,
            })
            .unwrap();
        wait_until("both jobs", || {
            job_finished(&queue, a.id).is_some() && job_finished(&queue, b.id).is_some()
        });
        // The pre-existing file is untouched and both outputs landed with
        // escalating suffixes (which job got _1 vs _2 is timing, so set).
        assert_eq!(std::fs::read(out.join("shot_64.png")).unwrap(), b"KEEP");
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 3);
        assert!(out.join("shot_64_1.png").exists());
        assert!(out.join("shot_64_2.png").exists());
    }

    #[test]
    fn bad_sources_fail_only_themselves() {
        let temp = tempfile::tempdir().unwrap();
        let good = temp.path().join("good.png");
        let missing = temp.path().join("gone.png");
        let text = temp.path().join("notes.txt");
        let broken = temp.path().join("broken.png");
        write_test_png(&good, 40, 20);
        std::fs::write(&text, b"not an image").unwrap();
        std::fs::write(&broken, b"\x89PNG cut off").unwrap();

        let queue = ExportQueue::new(2);
        let handle = queue
            .submit(ExportRequest {
                sources: vec![good.clone(), missing, text, broken],
                destination: None,
                format: ExportFormat::Jpeg { quality: 80 },
                max_edge: 256,
            })
            .unwrap();
        wait_until("mixed job", || job_finished(&queue, handle.id).is_some());
        let snap = job_finished(&queue, handle.id).unwrap();
        assert_eq!((snap.completed, snap.failed, snap.cancelled), (1, 3, 0));
        let output = temp.path().join("good_256.jpg");
        assert!(output.exists());
        assert_eq!(png_dims(&output), (40, 20));
        let messages: Vec<&str> = snap
            .files
            .iter()
            .filter_map(|f| match &f.state {
                FileState::Failed { message } => Some(message.as_str()),
                _ => None,
            })
            .collect();
        assert!(messages.iter().any(|m| m.contains("no longer exists")));
        assert!(messages.iter().any(|m| m.contains("thumbnail pipeline")));
        assert!(messages
            .iter()
            .any(|m| m.contains("decode") || m.contains("Decode")));
        assert!(temp_files_in(temp.path()).is_empty());
    }

    #[test]
    fn cancellation_stops_queued_work_and_leaves_no_partials() {
        let temp = tempfile::tempdir().unwrap();
        let mut sources = Vec::new();
        for i in 0..24 {
            let path = temp.path().join(format!("shot{i}.png"));
            write_test_png(&path, 120, 80);
            sources.push(path);
        }
        let queue = ExportQueue::new(1);
        let handle = queue
            .submit(ExportRequest {
                sources,
                destination: None,
                format: ExportFormat::Png,
                max_edge: 1000,
            })
            .unwrap();
        queue.cancel(handle.id);
        wait_until("cancelled job to settle", || {
            job_finished(&queue, handle.id).is_some()
        });
        let snap = job_finished(&queue, handle.id).unwrap();
        // At most the single in-flight file can have finished; everything
        // still queued is cancelled synchronously by cancel().
        assert!(
            snap.cancelled >= snap.total - 1,
            "expected the queue to stop, got {:?}",
            (snap.cancelled, snap.completed, snap.failed)
        );
        assert_eq!(snap.failed, 0);
        assert!(temp_files_in(temp.path()).is_empty());
        for file in &snap.files {
            if let FileState::Done { output } = &file.state {
                assert!(
                    image::image_dimensions(output).is_ok(),
                    "partial output {output:?}"
                );
            }
        }
    }

    #[test]
    fn export_bakes_exif_orientation_into_pixels() {
        let temp = tempfile::tempdir().unwrap();
        // 40×20 landscape pixels tagged "rotate 90 CW" means the upright
        // photo is 20×40; a naive decode+resize would stay landscape.
        let source = temp.path().join("rotated.jpg");
        write_oriented_jpeg(&source, 40, 20, 6);

        let queue = ExportQueue::new(1);
        let handle = queue
            .submit(ExportRequest {
                sources: vec![source],
                destination: None,
                format: ExportFormat::Png,
                max_edge: 500,
            })
            .unwrap();
        wait_until("orientation export", || {
            job_finished(&queue, handle.id).is_some()
        });
        let snap = job_finished(&queue, handle.id).unwrap();
        assert_eq!(snap.completed, 1);
        let output = temp.path().join("rotated_500.png");
        assert_eq!(png_dims(&output), (20, 40));
    }

    #[test]
    fn clear_finished_keeps_active_jobs_visible() {
        let temp = tempfile::tempdir().unwrap();
        let queue = ExportQueue::new(1);

        let done = queue
            .submit(ExportRequest {
                sources: vec![{
                    let p = temp.path().join("tiny.png");
                    write_test_png(&p, 8, 8);
                    p
                }],
                destination: None,
                format: ExportFormat::Png,
                max_edge: 64,
            })
            .unwrap();
        wait_until("tiny job", || job_finished(&queue, done.id).is_some());

        let mut big_sources = Vec::new();
        for i in 0..16 {
            let path = temp.path().join(format!("big{i}.png"));
            write_test_png(&path, 900, 600);
            big_sources.push(path);
        }
        let active = queue
            .submit(ExportRequest {
                sources: big_sources,
                destination: None,
                format: ExportFormat::Png,
                max_edge: 32,
            })
            .unwrap();
        assert!(queue.has_active());

        queue.clear_finished();
        let remaining: Vec<u64> = queue.snapshot().into_iter().map(|j| j.id).collect();
        assert!(remaining.contains(&active.id));
        assert!(!remaining.contains(&done.id));

        queue.cancel(active.id);
        wait_until("active job to settle", || {
            job_finished(&queue, active.id).is_some()
        });
        queue.clear_finished();
        assert!(queue.snapshot().is_empty());
        assert!(!queue.has_active());
    }

    #[test]
    fn submit_validation_rejects_the_whole_batch() {
        let queue = ExportQueue::new(1);
        assert!(matches!(
            queue.submit(ExportRequest {
                sources: vec![],
                destination: None,
                format: ExportFormat::Png,
                max_edge: 64,
            }),
            Err(crate::export::ExportError::InvalidRequest(_))
        ));
        assert!(queue.snapshot().is_empty());
    }
}
