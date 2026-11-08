//! Bounded background job queue shared by batch operations.
//!
//! Batch pipelines (export, contact sheets, watched-folder auto-actions) run
//! as jobs here so a large batch can never starve the browsing thread and so
//! progress, cancellation, and failure reporting behave identically for every
//! batch feature. The UI is a thin view over [`JobSnapshot`]s; all logic in
//! this module is testable headless.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Cancellation and progress signal handed to a running job.
///
/// Jobs must poll [`JobContext::check_cancelled`] between items and report
/// progress so the UI can show per-file rows and honour a cancel request.
pub struct JobContext {
    cancelled: AtomicBool,
    done: AtomicU64,
    total: AtomicU64,
}

impl Default for JobContext {
    fn default() -> Self {
        Self::new()
    }
}

impl JobContext {
    pub fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Returns `Err` when a cancel has been requested, so job bodies can
    /// early-return with `?` between items.
    pub fn check_cancelled(&self) -> Result<(), String> {
        if self.is_cancelled() {
            Err("cancelled".to_string())
        } else {
            Ok(())
        }
    }

    pub fn set_progress_total(&self, total: u64) {
        self.total.store(total, Ordering::SeqCst);
    }

    /// Report `n` additional completed items.
    pub fn advance_progress(&self, n: u64) {
        self.done.fetch_add(n, Ordering::SeqCst);
    }

    pub fn progress(&self) -> (u64, u64) {
        (self.done.load(Ordering::SeqCst), self.total.load(Ordering::SeqCst))
    }
}

/// A unit of batch work. `run` must be cancellable: poll
/// [`JobContext::check_cancelled`] between per-item steps and return the
/// cancellation error instead of unwrapping through it.
pub trait Job: Send + Sync + 'static {
    fn id(&self) -> String;
    fn run(&self, ctx: &JobContext) -> Result<(), String>;
}

/// Lifecycle state of a queued or running job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed(String),
    Cancelled,
}

/// Point-in-time view of a job, safe to hand to a UI thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobSnapshot {
    pub id: String,
    pub state: JobState,
    pub progress_done: u64,
    pub progress_total: u64,
}

struct JobRecord {
    id: String,
    state: Mutex<JobState>,
    ctx: Arc<JobContext>,
}

impl JobRecord {
    fn snapshot(&self) -> JobSnapshot {
        let (progress_done, progress_total) = self.ctx.progress();
        let state = self.state.lock().map(|s| s.clone()).unwrap_or(JobState::Failed(
            "status lock poisoned".to_string(),
        ));
        JobSnapshot {
            id: self.id.clone(),
            state,
            progress_done,
            progress_total,
        }
    }
}

/// A bounded pool of worker threads draining submitted jobs.
pub struct JobQueue {
    sender: Option<std::sync::mpsc::Sender<(Arc<JobRecord>, Box<dyn Job>)>>,
    records: Arc<Mutex<Vec<Arc<JobRecord>>>>,
    workers: Vec<JoinHandle<()>>,
}

impl JobQueue {
    /// Spawn `workers` threads (configurable; callers typically pass half the
    /// available cores). Returns an error when `workers` is zero.
    pub fn with_workers(workers: usize) -> Result<Self, String> {
        if workers == 0 {
            return Err("job queue needs at least one worker".to_string());
        }
        let (sender, receiver) = std::sync::mpsc::channel::<(Arc<JobRecord>, Box<dyn Job>)>();
        let receiver = Arc::new(Mutex::new(receiver));
        let records: Arc<Mutex<Vec<Arc<JobRecord>>>> = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let receiver = Arc::clone(&receiver);
            let records = Arc::clone(&records);
            handles.push(std::thread::spawn(move || loop {
                let next = {
                    let guard = receiver.lock();
                    match guard {
                        Ok(rx) => rx.recv(),
                        Err(_) => break,
                    }
                };
                let (record, job) = match next {
                    Ok(pair) => pair,
                    Err(_) => break, // channel closed: queue shutting down
                };
                if let Ok(mut state) = record.state.lock() {
                    *state = JobState::Running;
                }
                let result = job.run(&record.ctx);
                if let Ok(mut state) = record.state.lock() {
                    *state = if record.ctx.is_cancelled() {
                        JobState::Cancelled
                    } else {
                        match result {
                            Ok(()) => JobState::Done,
                            Err(message) => JobState::Failed(message),
                        }
                    };
                }
                // Keep finished records so the UI can render the done/failed
                // summary; the registry only ever grows per queue instance.
                if let Ok(mut all) = records.lock() {
                    all.push(Arc::clone(&record));
                }
            }));
        }
        Ok(Self {
            sender: Some(sender),
            records,
            workers: handles,
        })
    }

    /// Queue a job and return a handle for progress/cancel. The job starts as
    /// soon as a worker is free (bounded pool, so 200 queued files cannot
    /// create 200 threads).
    pub fn submit(&self, job: Box<dyn Job>) -> Result<JobHandle, String> {
        let sender = self
            .sender
            .as_ref()
            .ok_or_else(|| "job queue is shut down".to_string())?;
        let record = Arc::new(JobRecord {
            id: job.id(),
            state: Mutex::new(JobState::Queued),
            ctx: Arc::new(JobContext::new()),
        });
        sender
            .send((Arc::clone(&record), job))
            .map_err(|_| "worker pool is gone".to_string())?;
        Ok(JobHandle { record })
    }

    /// Snapshots of all known jobs, oldest first.
    pub fn snapshots(&self) -> Vec<JobSnapshot> {
        match self.records.lock() {
            Ok(records) => records.iter().map(|r| r.snapshot()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Snapshot for one job id, if known.
    pub fn snapshot(&self, id: &str) -> Option<JobSnapshot> {
        self.records
            .lock()
            .ok()?
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.snapshot())
    }

    /// Shut the pool down: stop accepting jobs and wait for running jobs to
    /// finish or observe cancellation. Consumes the queue.
    pub fn shutdown(mut self) {
        self.sender.take();
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for JobQueue {
    fn drop(&mut self) {
        // Behave like an explicit shutdown so a dropped queue never leaks
        // running jobs.
        self.sender.take();
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }
}

/// Handle to a submitted job: poll state/progress or request cancellation.
#[derive(Clone)]
pub struct JobHandle {
    record: Arc<JobRecord>,
}

impl JobHandle {
    pub fn snapshot(&self) -> JobSnapshot {
        self.record.snapshot()
    }

    pub fn cancel(&self) {
        self.record.ctx.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct CountingJob {
        id: String,
        items: u64,
    }

    impl Job for CountingJob {
        fn id(&self) -> String {
            self.id.clone()
        }
        fn run(&self, ctx: &JobContext) -> Result<(), String> {
            ctx.set_progress_total(self.items);
            for _ in 0..self.items {
                ctx.check_cancelled()?;
                ctx.advance_progress(1);
            }
            Ok(())
        }
    }

    struct CancellingJob;

    impl Job for CancellingJob {
        fn id(&self) -> String {
            "cancelling".to_string()
        }
        fn run(&self, ctx: &JobContext) -> Result<(), String> {
            ctx.set_progress_total(u64::MAX);
            // Poll cancel, briefly yielding so the test can flip the flag.
            for _ in 0..10_000 {
                ctx.check_cancelled()?;
                ctx.advance_progress(1);
                std::thread::sleep(Duration::from_micros(50));
            }
            Ok(())
        }
    }

    #[test]
    fn jobs_run_to_done_with_progress() {
        let queue = JobQueue::with_workers(2).unwrap();
        let handle = queue
            .submit(Box::new(CountingJob {
                id: "count-5".to_string(),
                items: 5,
            }))
            .unwrap();
        queue.shutdown();
        let snap = queue.snapshot("count-5").unwrap_or_else(|| handle.snapshot());
        assert_eq!(snap.state, JobState::Done);
        assert_eq!((snap.progress_done, snap.progress_total), (5, 5));
    }

    #[test]
    fn cancelled_job_reports_cancelled_state() {
        let queue = JobQueue::with_workers(1).unwrap();
        let handle = queue.submit(Box::new(CancellingJob)).unwrap();
        handle.cancel();
        queue.shutdown();
        let snap = handle.snapshot();
        assert_eq!(snap.state, JobState::Cancelled);
        assert!(snap.progress_total > 0 && snap.progress_done < snap.progress_total);
    }

    #[test]
    fn failing_job_reports_message_without_killing_worker() {
        struct Boom;
        impl Job for Boom {
            fn id(&self) -> String {
                "boom".to_string()
            }
            fn run(&self, _ctx: &JobContext) -> Result<(), String> {
                Err("boom".to_string())
            }
        }
        let queue = JobQueue::with_workers(1).unwrap();
        queue.submit(Box::new(Boom)).unwrap();
        // A following job must still run on the same worker.
        queue
            .submit(Box::new(CountingJob {
                id: "after-boom".to_string(),
                items: 1,
            }))
            .unwrap();
        queue.shutdown();
        let snaps = queue.snapshots();
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].state, JobState::Failed("boom".to_string()));
        assert_eq!(snaps[1].state, JobState::Done);
    }

    #[test]
    fn rejects_zero_workers_and_shut_down_queue() {
        assert!(JobQueue::with_workers(0).is_err());
        let queue = JobQueue::with_workers(1).unwrap();
        queue.shutdown();
        assert!(queue.submit(Box::new(Boom)).is_err());
        struct Boom;
        impl Job for Boom {
            fn id(&self) -> String {
                String::new()
            }
            fn run(&self, _ctx: &JobContext) -> Result<(), String> {
                Ok(())
            }
        }
    }

    #[test]
    fn bounded_pool_runs_larger_batch_than_workers() {
        let queue = JobQueue::with_workers(2).unwrap();
        for n in 0..6 {
            queue
                .submit(Box::new(CountingJob {
                    id: format!("j{n}"),
                    items: 3,
                }))
                .unwrap();
        }
        queue.shutdown();
        let snaps = queue.snapshots();
        assert_eq!(snaps.len(), 6);
        assert!(snaps.iter().all(|s| s.state == JobState::Done));
    }

    // Silence unused warning for HashMap import in non-test builds if any.
    #[allow(dead_code)]
    fn _assert_types() {
        let _map: HashMap<String, u64> = HashMap::new();
    }
}
