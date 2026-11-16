//! The metadata indexer: one file at a time, on the shared job pool.
//!
//! `bc-core::metadata_index` has been a complete, tested database since well
//! before anything read it. Its only writer was the watched-folder ingest
//! action, so the browser, a sort menu and smart collections had nothing to
//! query. This module is the producer that was missing.
//!
//! It owns the *work*, not the scheduling. [`crate::jobs::JobQueue`] runs the
//! queue, the worker threads, the per-file progress rows and cancellation for
//! exports and index passes alike; what is here is what one indexed file means,
//! and what the end of a folder pass means. That split is why this file has no
//! thread: an earlier version spawned one per pass, kept generation counters to
//! supersede it and a slot to reap it, and hung twice — both times joining a
//! worker while holding the mutex that worker's last line wanted. Per-file
//! tasks on a shared pool cannot hang that way because nobody joins anybody, and
//! navigating to another folder is a `cancel` plus a `submit` on a path that
//! already worked for exports rather than a new queue nobody tested.
//!
//! Reading rules that matter here, learned the hard way:
//!
//! - A file whose metadata cannot be parsed is **not** a failure. Every decoder
//!   in `bc-metadata` treats an unparseable packet or sidecar as "no
//!   properties", so indexing a corrupt JPEG succeeds with an empty row. Only a
//!   real I/O error or a missing file ends up anywhere except `Indexed`.
//! - `is_current` compares size, mtime, sidecar mtime and the extractor
//!   version, so a pass over an already-indexed folder costs two `stat`s and
//!   one indexed lookup per file, and writes nothing. That is what makes
//!   re-running the pass on every settled watcher event cheap enough to do
//!   without diffing the events.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::jobs::FileState;
use crate::metadata_index::MetadataIndex;
use crate::Result;

/// What happened to one file. Kept separate from [`FileState`] on purpose:
/// index semantics should not depend on the queue's row type, and the mapping
/// in [`IndexOutcome::into_state`] is then one visible place where a row gets
/// its reason text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexOutcome {
    /// Metadata was read and the row written.
    Indexed,
    /// The row already matched the file, so nothing was read or written.
    Current,
    /// The file is gone — deleted after the folder was listed, or never a file.
    Missing,
    /// The job was cancelled before this file was written.
    Cancelled,
    /// A real error, reported per row rather than ending the pass.
    Failed(String),
}

impl IndexOutcome {
    /// Turn an outcome into the queue's progress row.
    ///
    /// `Done` carries the source path: an index pass produces no output file,
    /// and the thing a user would want to see beside "indexed" is the file
    /// whose row was written.
    pub fn into_state(self, source: PathBuf) -> FileState {
        match self {
            IndexOutcome::Indexed => FileState::Done { output: source },
            IndexOutcome::Current => FileState::Skipped {
                reason: "already up to date".to_string(),
            },
            IndexOutcome::Missing => FileState::Skipped {
                reason: "file no longer exists".to_string(),
            },
            IndexOutcome::Cancelled => FileState::Cancelled,
            IndexOutcome::Failed(message) => FileState::Failed { message },
        }
    }
}

/// Index one file, honouring the job's cancellation between reading and
/// writing. Never fails the pass: whatever goes wrong is the row's message.
pub fn index_one(index: &MetadataIndex, path: &Path, cancel: &AtomicBool) -> IndexOutcome {
    if cancel.load(Ordering::Relaxed) {
        return IndexOutcome::Cancelled;
    }
    // Checked before `is_current` because a vanished file reports itself as
    // stale, and re-reading a file that is gone is an error, not a skip.
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return IndexOutcome::Missing,
        Err(error) => return IndexOutcome::Failed(error.to_string()),
        Ok(meta) => {
            // A symlink is indexable (reading it is harmless and its row keys
            // the path the listing shows), but a directory or a socket in a
            // file's place is the anomaly worth reporting.
            if !(meta.is_file() || meta.file_type().is_symlink()) {
                return IndexOutcome::Failed("not a regular file".to_string());
            }
        }
    }
    if index.is_current(path) {
        return IndexOutcome::Current;
    }
    // Second check: the read above can be slow on a network mount, and a
    // cancel that arrives during it should not pay for a write nobody wants.
    if cancel.load(Ordering::Relaxed) {
        return IndexOutcome::Cancelled;
    }
    match index.index_path(path) {
        Ok(()) => IndexOutcome::Indexed,
        Err(error) => IndexOutcome::Failed(error.to_string()),
    }
}

/// End of a folder pass: record the folder, then drop rows for files it no
/// longer contains.
///
/// Returns the number of rows removed. The root is recorded even when the pass
/// was cancelled, since the folder has been indexed as of now and pruning is
/// correct either way.
pub fn sweep_folder(index: &MetadataIndex, root: &Path, listed: &[PathBuf]) -> Result<usize> {
    index.add_root(root)?;
    index.prune_absent_from_listing(root, listed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::fixtures::write_oriented_jpeg;
    use crate::jobs::JobQueue;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// A real JPEG, so a green test means a file was parsed and not merely
    /// skipped.
    fn jpeg(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        write_oriented_jpeg(&path, 12, 10, 1);
        path
    }

    fn index_in(dir: &Path) -> Arc<MetadataIndex> {
        Arc::new(MetadataIndex::open(&dir.join("metadata.sqlite")).unwrap())
    }

    /// Run one pass to completion, failing rather than hanging the suite.
    fn run(queue: &JobQueue, id: u64) -> crate::jobs::JobSnapshot {
        let start = Instant::now();
        loop {
            if let Some(job) = queue.snapshot().into_iter().find(|job| job.id == id) {
                if job.finished {
                    return job;
                }
            }
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "pass never ended"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn tagged(dir: &Path, name: &str, stars: u8) -> PathBuf {
        let path = jpeg(dir, name);
        let mut tags = bc_metadata::Metadata::new();
        tags.set_rating(bc_metadata::ratings::Rating::new(stars));
        bc_metadata::xmp::write_embedded_xmp_file(&path, &tags).unwrap();
        path
    }

    fn counts(job: &crate::jobs::JobSnapshot) -> (usize, usize, usize, usize) {
        (job.completed, job.skipped, job.failed, job.cancelled)
    }

    #[test]
    fn a_pass_writes_rows_that_carry_tags_from_inside_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let rated = tagged(dir, "rated.jpg", 4);
        let plain = jpeg(dir, "plain.jpg");
        let index = index_in(dir);
        let queue = JobQueue::new(2);

        let job = queue.submit_index(dir, vec![rated.clone(), plain.clone()], Arc::clone(&index));
        let snap = run(&queue, job.id);

        assert_eq!(counts(&snap), (2, 0, 0, 0), "{:?}", snap.files);
        // The property the whole indexer exists for: a rating that lives in
        // the file's XMP packet is queryable from the database.
        let row = index.get(&rated).unwrap().expect("row written");
        assert_eq!(row.rating, 4, "in-file rating lost by the index");
        assert!(
            index.get(&plain).unwrap().is_some(),
            "plain file has no row"
        );
        assert_eq!(row.path, rated);
    }

    #[test]
    fn a_second_pass_over_the_same_folder_reads_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let files: Vec<PathBuf> = (0..4).map(|i| jpeg(dir, &format!("f{i}.jpg"))).collect();
        let index = index_in(dir);
        let queue = JobQueue::new(2);

        let first = queue.submit_index(dir, files.clone(), Arc::clone(&index));
        assert_eq!(counts(&run(&queue, first.id)), (4, 0, 0, 0));

        let second = queue.submit_index(dir, files, Arc::clone(&index));
        let snap = run(&queue, second.id);
        assert_eq!(counts(&snap), (0, 4, 0, 0), "re-pass re-wrote rows");
        for file in &snap.files {
            match &file.state {
                FileState::Skipped { reason } => {
                    assert_eq!(reason, "already up to date", "{reason}")
                }
                other => panic!("unexpected row state {other:?}"),
            }
        }
    }

    #[test]
    fn a_file_that_vanished_after_listing_is_skipped_not_failed() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let kept = jpeg(dir, "kept.jpg");
        let gone = jpeg(dir, "gone.jpg");
        let index = index_in(dir);
        let queue = JobQueue::new(1);

        std::fs::remove_file(&gone).unwrap();
        let job = queue.submit_index(dir, vec![kept.clone(), gone.clone()], Arc::clone(&index));
        let snap = run(&queue, job.id);

        assert_eq!(counts(&snap), (1, 1, 0, 0), "{:?}", snap.files);
        assert!(matches!(
            &snap.files[1].state,
            FileState::Skipped { reason } if reason == "file no longer exists"
        ));
        assert!(index.get(&kept).unwrap().is_some());
        assert!(index.get(&gone).unwrap().is_none(), "row for nothing");
    }

    #[test]
    fn an_unreadable_file_indexes_as_having_no_tags_rather_than_failing() {
        // The premise this project got wrong once: a corrupt packet is not an
        // indexing failure. Every reader in bc-metadata treats it as "no
        // properties", so the honest row is an indexed file with nothing in it.
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let good = jpeg(dir, "good.jpg");
        let corrupt = dir.join("corrupt.jpg");
        std::fs::write(&corrupt, b"\xFF\xD8 not really a jpeg").unwrap();
        let index = index_in(dir);
        let queue = JobQueue::new(1);

        let job = queue.submit_index(dir, vec![good.clone(), corrupt.clone()], Arc::clone(&index));
        let snap = run(&queue, job.id);

        assert_eq!(counts(&snap), (2, 0, 0, 0), "{:?}", snap.files);
        let row = index.get(&corrupt).unwrap().expect("row written anyway");
        assert_eq!(row.rating, 0);
        assert!(row.keywords.is_empty());
        assert!(index.get(&good).unwrap().is_some());
    }

    #[test]
    fn a_cancelled_pass_writes_nothing_half_done_and_can_be_rerun() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        // One worker: at most the file it claimed can have been written, and
        // every later task is marked cancelled inside `cancel()` itself. This
        // needs no oversized fixture, because cancellation is a per-task check
        // now — the old thread design only looked once per 256 files, which is
        // why it needed 300 of them to test at all.
        let files: Vec<PathBuf> = (0..16)
            .map(|i| jpeg(dir, &format!("x{i:02}.jpg")))
            .collect();
        let index = index_in(dir);
        let queue = JobQueue::new(1);

        let job = queue.submit_index(dir, files.clone(), Arc::clone(&index));
        queue.cancel(job.id);
        let snap = run(&queue, job.id);
        assert!(
            snap.cancelled >= snap.total - 1,
            "the queue kept going: {:?}",
            (snap.cancelled, snap.completed)
        );
        assert_eq!(snap.failed, 0);
        assert_eq!(snap.completed + snap.cancelled, snap.total);
        assert_eq!(
            snap.pruned, 0,
            "nothing was ever indexed here, so the sweep had no stale row to find"
        );
        assert!(
            index.all().unwrap().len() <= 1,
            "rows written after cancellation: {:?}",
            index.all().unwrap().len()
        );

        // Not a permanent kill switch: the same folder indexes fully next time.
        let retry = queue.submit_index(dir, files, Arc::clone(&index));
        let snap = run(&queue, retry.id);
        assert_eq!(snap.completed + snap.skipped, 16);
        assert_eq!(snap.cancelled, 0);
        assert_eq!(index.all().unwrap().len(), 16);
    }

    #[test]
    fn a_cancelled_pass_still_sweeps_because_the_sweep_does_not_need_the_pass() {
        // The documented guarantee: cancellation stops writing rows, not
        // pruning the ones that no longer describe a file. Two files are
        // deleted after being indexed, the pass over what is left is cancelled
        // at once, and both stale rows still go.
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let keep = jpeg(dir, "keep.jpg");
        let stale: Vec<PathBuf> = (0..2)
            .map(|i| jpeg(dir, &format!("stale{i}.jpg")))
            .collect();
        let index = index_in(dir);
        let queue = JobQueue::new(1);

        let first = queue.submit_index(
            dir,
            std::iter::once(keep.clone()).chain(stale.clone()).collect(),
            Arc::clone(&index),
        );
        assert_eq!(run(&queue, first.id).completed, 3);
        for path in &stale {
            std::fs::remove_file(path).unwrap();
        }

        let cancelled = queue.submit_index(dir, vec![keep.clone()], Arc::clone(&index));
        queue.cancel(cancelled.id);
        let snap = run(&queue, cancelled.id);
        assert_eq!(snap.pruned, 2, "the sweep was skipped along with the pass");
        for path in &stale {
            assert!(index.get(path).unwrap().is_none(), "{path:?} still rowed");
        }
        assert!(index.get(&keep).unwrap().is_some());
    }

    #[test]
    fn a_sweep_removes_rows_the_folder_no_longer_has() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let keep = jpeg(dir, "keep.jpg");
        let doomed = jpeg(dir, "doomed.jpg");
        let index = index_in(dir);
        let queue = JobQueue::new(1);

        let job = queue.submit_index(dir, vec![keep.clone(), doomed.clone()], Arc::clone(&index));
        assert_eq!(run(&queue, job.id).completed, 2);
        std::fs::remove_file(&doomed).unwrap();

        // The next listing does not mention the deleted file at all.
        let second = queue.submit_index(dir, vec![keep.clone()], Arc::clone(&index));
        let snap = run(&queue, second.id);
        assert_eq!(snap.pruned, 1, "stale row left in place");
        assert!(index.get(&doomed).unwrap().is_none());
        assert!(index.get(&keep).unwrap().is_some());
    }

    #[test]
    fn a_sweep_never_touches_a_folder_whose_name_merely_starts_like_its_own() {
        // The prefix trap: `/lib` pruning must not reach `/lib-archive`, which
        // a `LIKE '/lib%'` query would silently do.
        let temp = tempfile::tempdir().unwrap();
        let lib = temp.path().join("lib");
        let archive = temp.path().join("lib-archive");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::create_dir_all(&archive).unwrap();
        let other = jpeg(&archive, "shot.jpg");
        let index = index_in(temp.path());
        let queue = JobQueue::new(1);

        let job = queue.submit_index(&archive, vec![other.clone()], Arc::clone(&index));
        assert_eq!(run(&queue, job.id).completed, 1);

        // An empty pass over the sibling folder.
        let sweep = queue.submit_index(&lib, vec![], Arc::clone(&index));
        let snap = run(&queue, sweep.id);
        assert_eq!(snap.pruned, 0, "pruned a neighbouring folder's row");
        assert_eq!(snap.total, 0, "an empty folder still gets a sweep");
        assert!(index.get(&other).unwrap().is_some());
    }

    #[test]
    fn a_pass_records_the_folder_it_covered() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let index = index_in(dir);
        let queue = JobQueue::new(1);
        let job = queue.submit_index(dir, vec![jpeg(dir, "one.jpg")], Arc::clone(&index));
        run(&queue, job.id);
        // What is asserted here is the *write*. `roots()` has no reader in the
        // interface yet, which is named in the roadmap rather than assumed
        // away by a test that pretends otherwise.
        assert!(
            index.roots().unwrap().contains(&dir.to_path_buf()),
            "the pass never recorded the folder it indexed"
        );
    }

    #[test]
    fn an_index_job_is_labelled_as_indexing_not_exporting() {
        // The job list shares one window with exports. "→ beside originals"
        // on a metadata pass would be the kind of wrong that reads as fine.
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let index = index_in(dir);
        let queue = JobQueue::new(1);
        let job = queue.submit_index(dir, vec![], Arc::clone(&index));
        let snap = run(&queue, job.id);
        assert_eq!(snap.kind.heading(), "Metadata index");
        assert_eq!(snap.kind.target(), dir.display().to_string());
        assert!(snap.pruned == 0);
    }

    #[test]
    fn outcome_reasons_are_stated_in_the_user_terms_the_panel_shows() {
        let path = PathBuf::from("/lib/shot.jpg");
        assert_eq!(
            IndexOutcome::Current.clone().into_state(path.clone()),
            FileState::Skipped {
                reason: "already up to date".to_string()
            }
        );
        assert_eq!(
            IndexOutcome::Missing.into_state(path.clone()),
            FileState::Skipped {
                reason: "file no longer exists".to_string()
            }
        );
        assert_eq!(
            IndexOutcome::Indexed.into_state(path.clone()),
            FileState::Done { output: path }
        );
    }

    #[test]
    fn a_cancelled_task_never_opens_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let file = jpeg(dir, "never.jpg");
        let index = index_in(dir);
        let cancel = AtomicBool::new(true);
        assert_eq!(
            index_one(&index, &file, &cancel),
            IndexOutcome::Cancelled,
            "cancel was honoured only after the read"
        );
        assert!(index.get(&file).unwrap().is_none());
    }

    #[test]
    fn a_directory_in_a_files_place_is_reported_not_swallowed() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let impostor = dir.join("sub.jpg");
        std::fs::create_dir(&impostor).unwrap();
        let index = index_in(dir);
        let cancel = AtomicBool::new(false);
        assert_eq!(
            index_one(&index, &impostor, &cancel),
            IndexOutcome::Failed("not a regular file".to_string())
        );
    }
}
