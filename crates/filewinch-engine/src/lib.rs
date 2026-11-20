//! Filewinch's transfer engine.
//!
//! The app hands the [`Engine`] [`Job`]s (copy, move or delete a set of paths between two
//! [`Remote`]s) and listens to [`Event`]s. The engine runs up to `max_jobs` jobs at once and up to
//! `max_files_per_job` files in parallel within each job. A job first walks its folders
//! (iteratively) to count files and bytes while the first files already transfer.
//!
//! Behaviour in brief:
//! - Bytes stream through a buffer of `chunk` bytes; nothing is buffered whole.
//! - Pause, resume and cancel act between chunks. Cancel drops the open [`WriteSink`] without
//!   `finish`, so backends abort the upload.
//! - Transient errors ([`ErrorKind::is_transient`]) retry with exponential backoff and jitter,
//!   honouring `retry_after`, and resume from the bytes already written when both sides allow
//!   it ([`Caps::resume_read`] on the source, [`Caps::resume_write`] on the destination).
//! - Any other error fails that item ([`Event::ItemFailed`]); the job carries on and ends
//!   [`JobState::Failed`] with a summary if any item failed.
//! - Conflicts follow the job's [`ConflictPolicy`]; with [`ConflictPolicy::Ask`] the job parks
//!   in [`JobState::WaitingForAnswer`] and emits [`Event::Conflict`] until [`Engine::answer`].
//! - Large downloads to a destination that takes positional writes (the local disk) use up to
//!   `segments_per_file` connections at once when the source has [`Caps::ranged_reads`]:
//!   files of 16 MB or more, at least 4 MB per connection. Pieces go into a hidden
//!   `.name.filewinch-part` with a `.name.filewinch-ranges` sidecar of finished ranges, so a
//!   pause, a failure, a retry or an app restart continues from them; the file is renamed into
//!   place (conflict policy applied then) once complete, its size and any server checksum
//!   checked. A server that refuses the extra connections (too many connections, rate limits,
//!   an ignored range, auth errors on the extra connections) gets one connection for the rest
//!   of the session; the download finishes over one connection and [`Engine::notes`] says
//!   why. A cancel or [`Engine::forget`] deletes the temp files; dropping the engine keeps
//!   them.
//! - One bandwidth limit (token bucket) is shared by all jobs (and every segment).
//! - Progress is throttled to about ten events a second per job, with a smoothed speed and ETA.
//!
//! Backends that surface errors through `std::io::Read`/`Write` should wrap a
//! [`filewinch_core::Error`] in `std::io::Error::other(..)`; the engine unwraps it so the
//! original [`ErrorKind`] (and so the retry decision) survives.
//!
//! Folder sync is two steps: [`compare`] builds a [`SyncPlan`] the user can review, then
//! [`apply`] turns it into jobs. [`CompareBy::Checksum`] compares equal-sized files by content:
//! it uses the hashes servers list ([`filewinch_core::Entry::checksums`]) when both sides share
//! an algorithm, otherwise streams the side without one through the cheapest algorithm the
//! other side offers (MD5/SHA-1/SHA-256, then Dropbox content hash, then QuickXorHash), and
//! with no server hash on either side reads and hashes both with SHA-256. A read error fails
//! the compare; it never counts as "equal". [`compare_with`] takes a [`CancelToken`] (checked
//! between items and between hash chunks) and reports [`CompareProgress`].
//!
//! Shared items ([`filewinch_core::Entry::shared`]) are never emptied by a Delete: a selected
//! one is left ([`Remote::leave_shared`]) or refused with an explanation unless
//! [`Job::delete_shared_contents`] is set; one found inside a selected folder is skipped.
//!
//! Every callback runs on an engine thread. The callback must not block for long; it may call
//! back into the engine (e.g. [`Engine::snapshot`]).

mod control;
mod hash;
mod items;
mod meter;
mod runner;
mod segment;
mod sidecar;
mod sync;
mod transfer;
mod walk;

pub use items::{ActivePhase, ItemCounts, ItemFilter, ItemState, ItemView};
pub use sync::{
    CancelToken, CompareBy, CompareProgress, SyncAction, SyncActionKind, SyncConflict, SyncDirection,
    SyncOptions, SyncPlan, apply, compare, compare_with,
};

use control::{Bucket, JobCtl};
use filewinch_core::{Entry, Error, ErrorKind, RPath, Remote};
use meter::Meter;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

#[cfg(doc)]
use filewinch_core::{Caps, WriteSink};

pub type JobId = u64;

/// The most files one job can be set to move at once while it runs ([`Engine::set_job_files`]).
pub const MAX_FILES_PER_JOB_LIVE: usize = 16;

/// What to do when the destination already has an item with the same name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictPolicy {
    /// Stop and ask through [`Event::Conflict`].
    Ask,
    Overwrite,
    Skip,
    /// Keep both: the new copy becomes "name 2.ext" (then 3, 4, ...).
    Rename,
    /// Resume when the destination is a shorter partial copy and both sides support resuming;
    /// otherwise overwrite.
    ResumeOrOverwrite,
    /// Overwrite only when the source is newer than the destination; otherwise skip.
    OverwriteIfNewer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Copy,
    /// Copy then delete the source, or a server-side rename when source and destination are the
    /// same remote (the same `Arc`) and it has [`Caps::server_rename`].
    Move,
    /// Deletes `src_paths` recursively. `dst` and `dst_dir` are ignored.
    Delete,
    /// Changes Unix permissions of `src_paths` and everything inside them (folders are listed
    /// first, then changed, so taking away read access never blocks the walk). Only the bits
    /// in `mask` change: new = (old & !mask) | (mode & mask). With `dirs_get_x_where_r`,
    /// folders also get execute wherever the result has read (within `mask`), like chmod's
    /// `X`. Symbolic links are left alone. `dst` and `dst_dir` are ignored. Fails at once with
    /// a clear error when the remote can't set permissions ([`Caps::permissions`]).
    SetMode { mode: u32, mask: u32, dirs_get_x_where_r: bool },
}

/// The mode [`Op::SetMode`] gives an item whose current mode is `old`.
pub fn applied_mode(old: u32, is_dir: bool, mode: u32, mask: u32, dirs_get_x_where_r: bool) -> u32 {
    let mut new = (old & !mask) | (mode & mask);
    if is_dir && dirs_get_x_where_r {
        new |= ((new & 0o444) >> 2) & mask;
    }
    new & 0o7777
}

/// One unit of work in the queue.
#[derive(Clone)]
pub struct Job {
    pub op: Op,
    pub src: Arc<dyn Remote>,
    pub src_paths: Vec<RPath>,
    pub dst: Option<Arc<dyn Remote>>,
    /// The folder the items land in (each keeps its name). None for Delete.
    pub dst_dir: Option<RPath>,
    pub conflict: ConflictPolicy,
    /// "Uploading 3 items to web1"
    pub label: String,
    /// Delete only: the user confirmed deleting the contents of the selected shared items
    /// ([`filewinch_core::Entry::shared`]) for everyone they're shared with. When false (the
    /// default), a selected shared item is left via [`Remote::leave_shared`] or fails with an
    /// explanation, and nothing inside it is deleted. Shared items found *inside* a selected
    /// ordinary folder are never emptied, whatever this says.
    pub delete_shared_contents: bool,
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job")
            .field("op", &self.op)
            .field("src_paths", &self.src_paths)
            .field("dst_dir", &self.dst_dir)
            .field("conflict", &self.conflict)
            .field("label", &self.label)
            .field("delete_shared_contents", &self.delete_shared_contents)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub enum JobState {
    Queued,
    /// Walking folders to count what to transfer. Files may already be moving.
    Scanning,
    Running,
    Paused,
    /// A conflict question is open; see [`Event::Conflict`] and [`Engine::answer`].
    WaitingForAnswer,
    /// A transient error; the next attempt starts at `at`.
    Retrying { attempt: u32, at: Instant },
    Done,
    /// Holds a summary: "2 of 40 items couldn't be copied." with the first failures in `detail`.
    Failed(Error),
    Cancelled,
}

impl JobState {
    /// Done, Failed or Cancelled.
    pub fn is_finished(&self) -> bool {
        matches!(self, JobState::Done | JobState::Failed(_) | JobState::Cancelled)
    }

    fn same(&self, other: &JobState) -> bool {
        match (self, other) {
            (JobState::Retrying { attempt: a, .. }, JobState::Retrying { attempt: b, .. }) => a == b,
            (JobState::Failed(a), JobState::Failed(b)) => a.message == b.message,
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Progress {
    pub bytes_done: u64,
    /// None while the job is still scanning.
    pub bytes_total: Option<u64>,
    pub files_done: u64,
    pub files_total: Option<u64>,
    /// The file most recently started.
    pub current: Option<RPath>,
    /// Files and bytes found so far: while scanning (totals still `None`) they grow; after,
    /// they equal the totals.
    pub files_found: u64,
    pub bytes_found: u64,
    /// Smoothed transfer speed.
    pub bytes_per_sec: f64,
    pub eta: Option<Duration>,
}

/// The user's answer to a conflict question.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictChoice {
    Overwrite,
    Skip,
    /// Keep both; the new copy gets "name 2.ext".
    Rename,
    /// Continue a partial copy. Falls back to Overwrite when the conflict can't be resumed.
    Resume,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConflictAnswer {
    pub choice: ConflictChoice,
    /// Use this choice for the rest of this job's conflicts without asking.
    pub apply_to_all: bool,
}

/// A conflict question: `src` is about to land on `dst`, which already exists.
#[derive(Clone, Debug)]
pub struct ConflictAsk {
    pub src: Entry,
    pub dst: Entry,
    /// Resume is possible: dst is a shorter file and both sides support resuming.
    pub can_resume: bool,
}

// Conflict carries two entries; events are rare (≤10/s per job), so boxing isn't worth it.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum Event {
    Added(JobId),
    State(JobId, JobState),
    Progress(JobId, Progress),
    Conflict { job: JobId, ask: ConflictAsk },
    ItemFailed { job: JobId, path: RPath, error: Error },
    /// A worker took up one file: `path` is where it's written (Copy, Move) or the file being
    /// removed (Delete), before conflict handling renames anything. For live folder views.
    ItemStarted { job: JobId, path: RPath, size: Option<u64> },
    /// That file was written, moved, deleted or failed (a failure also has
    /// [`Event::ItemFailed`]); a skipped file has no `ItemDone` (it leaves the active items,
    /// see [`Event::ItemsChanged`]). `path` as in [`Event::ItemStarted`]; `from` is a Move's
    /// source, which may be gone now.
    ItemDone { job: JobId, path: RPath, from: Option<RPath> },
    /// The job's per-item list changed; pull it with [`Engine::items`] (about four a second at
    /// most per job, see `items`).
    ItemsChanged(JobId),
}

#[derive(Clone, Debug, PartialEq)]
pub struct EngineConfig {
    /// Jobs running at once (paused jobs and jobs waiting for an answer keep their slot).
    pub max_jobs: usize,
    /// Files transferring in parallel within one job.
    pub max_files_per_job: usize,
    /// Retries per file for transient errors.
    pub retries: u32,
    /// Buffer size for streaming, in bytes.
    pub chunk: usize,
    /// Bytes per second across all jobs; None for unlimited.
    pub bandwidth_limit: Option<u64>,
    /// First retry delay; doubles each attempt (capped at 60 s), with ±25 % jitter.
    pub retry_delay: Duration,
    /// Connections one large download may use at once (1 turns segmenting off). Applies only
    /// where the source has [`Caps::ranged_reads`] and the destination takes positional writes.
    pub segments_per_file: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            max_jobs: 2,
            max_files_per_job: 3,
            retries: 5,
            chunk: 256 * 1024,
            bandwidth_limit: None,
            retry_delay: Duration::from_secs(1),
            segments_per_file: 3,
        }
    }
}

/// Where a job is in its life, apart from the overlays (paused, asking, retrying) that
/// [`JobCtl`] tracks.
#[derive(Clone, Debug)]
enum Phase {
    Queued,
    Scanning,
    Running,
    Finished(JobState),
}

struct JobRec {
    job: Job,
    phase: Phase,
    shown: JobState,
    ctl: Arc<JobCtl>,
    meter: Arc<Meter>,
    /// Source files finished in earlier runs; [`Engine::retry`] skips them.
    done: Arc<Mutex<HashSet<RPath>>>,
    /// Things worth telling the person that didn't fail the job (e.g. why a download fell back
    /// to one connection).
    notes: Arc<Mutex<Vec<String>>>,
    items: Arc<items::Items>,
    /// Temp files of segmented downloads this job left on its destination (for resume);
    /// [`Engine::forget`] deletes them.
    partials: Arc<Mutex<HashSet<RPath>>>,
}

struct Shared {
    cfg: EngineConfig,
    jobs: BTreeMap<JobId, JobRec>,
    queue: VecDeque<JobId>,
    running: usize,
    next_id: JobId,
}

pub(crate) struct Inner {
    shared: Mutex<Shared>,
    emit: Box<dyn Fn(Event) + Send + Sync>,
    pub(crate) bucket: Bucket,
    /// Sessions that refused extra connections: their downloads use one connection.
    single: Mutex<Vec<Weak<dyn Remote>>>,
}

/// The transfer queue. Dropping it cancels every job.
pub struct Engine {
    inner: Arc<Inner>,
}

impl Engine {
    pub fn new(cfg: EngineConfig, on_event: impl Fn(Event) + Send + Sync + 'static) -> Engine {
        let bucket = Bucket::new(cfg.bandwidth_limit);
        Engine {
            inner: Arc::new(Inner {
                shared: Mutex::new(Shared {
                    cfg,
                    jobs: BTreeMap::new(),
                    queue: VecDeque::new(),
                    running: 0,
                    next_id: 1,
                }),
                emit: Box::new(on_event),
                bucket,
                single: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn enqueue(&self, job: Job) -> JobId {
        let id = {
            let mut s = self.inner.lock();
            let id = s.next_id;
            s.next_id += 1;
            s.jobs.insert(
                id,
                JobRec {
                    job,
                    phase: Phase::Queued,
                    shown: JobState::Queued,
                    ctl: Arc::new(JobCtl::default()),
                    meter: Arc::new(Meter::default()),
                    done: Arc::default(),
                    notes: Arc::default(),
                    items: Arc::default(),
                    partials: Arc::default(),
                },
            );
            s.queue.push_back(id);
            id
        };
        (self.inner.emit)(Event::Added(id));
        (self.inner.emit)(Event::State(id, JobState::Queued));
        Inner::schedule(&self.inner);
        id
    }

    /// Pauses between chunks. A queued job stays in the queue until resumed.
    pub fn pause(&self, id: JobId) {
        if let Some(ctl) = self.inner.ctl(id) {
            ctl.set_paused(true);
            self.inner.refresh(id);
        }
    }

    pub fn resume(&self, id: JobId) {
        if let Some(ctl) = self.inner.ctl(id) {
            ctl.set_paused(false);
            self.inner.refresh(id);
            Inner::schedule(&self.inner);
        }
    }

    pub fn cancel(&self, id: JobId) {
        let Some(ctl) = self.inner.ctl(id) else { return };
        ctl.cancel();
        // A job that hasn't started finishes right here; a running one ends at its next chunk.
        let was_queued = {
            let mut s = self.inner.lock();
            let queued = s.jobs.get(&id).is_some_and(|r| matches!(r.phase, Phase::Queued));
            if queued {
                s.queue.retain(|q| *q != id);
                if let Some(r) = s.jobs.get_mut(&id) {
                    r.phase = Phase::Finished(JobState::Cancelled);
                }
            }
            queued
        };
        if was_queued {
            self.inner.refresh(id);
        }
    }

    /// Runs a Failed or Cancelled job again. Files it already finished are skipped.
    pub fn retry(&self, id: JobId) {
        {
            let mut s = self.inner.lock();
            let Some(r) = s.jobs.get_mut(&id) else { return };
            match &r.phase {
                Phase::Finished(JobState::Failed(_) | JobState::Cancelled) => {}
                _ => return,
            }
            r.phase = Phase::Queued;
            let limit = r.ctl.files_limit();
            r.ctl = Arc::new(JobCtl::default());
            if limit > 0 {
                r.ctl.set_files_limit(limit);
            }
            r.meter = Arc::new(Meter::default());
            s.queue.push_back(id);
        }
        self.inner.refresh(id);
        Inner::schedule(&self.inner);
    }

    /// Sets how many files job `id` moves at once (1 to [`MAX_FILES_PER_JOB_LIVE`], or the
    /// config's value if that is higher), effective at once: a higher number starts more files
    /// now; a lower one lets the files in flight finish and starts none until fewer are moving.
    /// Nothing is cancelled. Works on queued jobs too.
    pub fn set_job_files(&self, id: JobId, n: usize) {
        let cap = self.inner.lock().cfg.max_files_per_job.max(MAX_FILES_PER_JOB_LIVE);
        if let Some(ctl) = self.inner.ctl(id) {
            ctl.set_files_limit(n.clamp(1, cap));
        }
    }

    /// The files-at-once limit of job `id`: its own if set, else the config's.
    pub fn job_files(&self, id: JobId) -> Option<usize> {
        let s = self.inner.lock();
        let r = s.jobs.get(&id)?;
        Some(match r.ctl.files_limit() {
            0 => s.cfg.max_files_per_job.max(1),
            n => n,
        })
    }

    /// Answers the open conflict question of job `id`. Ignored when nothing is being asked.
    pub fn answer(&self, id: JobId, answer: ConflictAnswer) {
        if let Some(ctl) = self.inner.ctl(id) {
            ctl.answer(answer);
        }
    }

    /// New limits apply to jobs that start afterwards; the bandwidth limit applies at once.
    pub fn set_config(&self, cfg: EngineConfig) {
        self.inner.bucket.set_rate(cfg.bandwidth_limit);
        self.inner.lock().cfg = cfg;
        Inner::schedule(&self.inner);
    }

    pub fn config(&self) -> EngineConfig {
        self.inner.lock().cfg.clone()
    }

    /// Every job the engine knows, oldest first.
    pub fn snapshot(&self) -> Vec<(JobId, String, JobState, Progress)> {
        let s = self.inner.lock();
        s.jobs
            .iter()
            .map(|(id, r)| (*id, r.job.label.clone(), r.shown.clone(), r.meter.progress()))
            .collect()
    }

    /// Drops a finished job from the list (the "clear" button) and deletes the partial
    /// downloads it kept for resuming. Returns false if it is still queued or running.
    pub fn forget(&self, id: JobId) -> bool {
        let rec = {
            let mut s = self.inner.lock();
            if !s.jobs.get(&id).is_some_and(|r| matches!(r.phase, Phase::Finished(_))) {
                return false;
            }
            s.jobs.remove(&id)
        };
        let Some(rec) = rec else { return true };
        let partials: Vec<RPath> = rec.partials.lock().unwrap_or_else(|e| e.into_inner()).drain().collect();
        if let (Some(dst), false) = (rec.job.dst.clone(), partials.is_empty()) {
            // Off the caller's thread: the caller is usually the UI.
            let _ = std::thread::Builder::new().name("filewinch-forget".into()).spawn(move || {
                for temp in partials {
                    sidecar::remove(&*dst, &sidecar::Names::of_temp(&temp));
                }
            });
        }
        true
    }

    /// Job `id`'s items in `filter` (files and folders, in the order found): how many there
    /// are, and those in `range` (for the rows on screen). Folders take their state from what's
    /// inside them.
    pub fn items(&self, id: JobId, filter: ItemFilter, range: std::ops::Range<usize>) -> (usize, Vec<ItemView>) {
        let items = self.inner.lock().jobs.get(&id).map(|r| r.items.clone());
        items.map_or((0, Vec::new()), |i| i.page(filter, range))
    }

    /// How many of job `id`'s files are running, queued, done, failed and skipped.
    pub fn item_counts(&self, id: JobId) -> ItemCounts {
        let items = self.inner.lock().jobs.get(&id).map(|r| r.items.clone());
        items.map(|i| i.counts()).unwrap_or_default()
    }

    /// Skips one item of job `id` without stopping the rest: a running file stops at its next
    /// chunk and its partial (or segmented temp) file is deleted, freeing its slot for the next
    /// file; a queued one never starts; a folder skips everything in it, including what the
    /// walk hasn't found yet. A skipped item never fails the job. Returns false when there was
    /// nothing to skip (unknown, or already finished: a file that lands anyway counts as done).
    pub fn skip_item(&self, id: JobId, path: &RPath) -> bool {
        let found = {
            let s = self.inner.lock();
            s.jobs.get(&id).map(|r| (r.items.clone(), r.ctl.clone()))
        };
        let Some((items, ctl)) = found else { return false };
        let skipped = items.skip(path) == items::Skip::Skipped;
        if skipped {
            ctl.poke();
            if items.take_event(false) {
                self.inner.emit(Event::ItemsChanged(id));
            }
        }
        skipped
    }

    /// Notes about job `id` that didn't fail it, oldest first: for example that a download
    /// fell back to one connection, and why.
    pub fn notes(&self, id: JobId) -> Vec<String> {
        let notes = self.inner.lock().jobs.get(&id).map(|r| r.notes.clone());
        notes.map(|n| n.lock().unwrap_or_else(|e| e.into_inner()).clone()).unwrap_or_default()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let s = self.inner.lock();
        for r in s.jobs.values() {
            r.ctl.shutdown();
        }
    }
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// True when `r` refused extra connections earlier in this session.
    pub(crate) fn is_single(&self, r: &Arc<dyn Remote>) -> bool {
        let p = Arc::as_ptr(r) as *const ();
        self.single.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|w| w.as_ptr() as *const () == p && w.strong_count() > 0)
    }

    pub(crate) fn set_single(&self, r: &Arc<dyn Remote>) {
        let mut v = self.single.lock().unwrap_or_else(|e| e.into_inner());
        v.retain(|w| w.strong_count() > 0);
        if !v.iter().any(|w| w.as_ptr() as *const () == Arc::as_ptr(r) as *const ()) {
            v.push(Arc::downgrade(r));
        }
    }

    fn ctl(&self, id: JobId) -> Option<Arc<JobCtl>> {
        self.lock().jobs.get(&id).map(|r| r.ctl.clone())
    }

    pub(crate) fn emit(&self, ev: Event) {
        (self.emit)(ev);
    }

    /// Recomputes the job's visible state from its phase and control flags; emits on change.
    pub(crate) fn refresh(&self, id: JobId) {
        let changed = {
            let mut s = self.lock();
            let Some(r) = s.jobs.get_mut(&id) else { return };
            let state = match &r.phase {
                Phase::Finished(st) => st.clone(),
                phase => {
                    let o = r.ctl.overlay();
                    if o.paused {
                        JobState::Paused
                    } else if o.asking {
                        JobState::WaitingForAnswer
                    } else if let Some((attempt, at)) = o.retrying {
                        JobState::Retrying { attempt, at }
                    } else {
                        match phase {
                            Phase::Queued => JobState::Queued,
                            Phase::Scanning => JobState::Scanning,
                            _ => JobState::Running,
                        }
                    }
                }
            };
            if state.same(&r.shown) {
                None
            } else {
                r.shown = state.clone();
                Some(state)
            }
        };
        if let Some(st) = changed {
            self.emit(Event::State(id, st));
        }
    }

    pub(crate) fn set_phase(&self, id: JobId, phase: Phase) {
        if let Some(r) = self.lock().jobs.get_mut(&id) {
            r.phase = phase;
        }
        self.refresh(id);
    }

    /// Starts queued jobs while slots are free.
    fn schedule(this: &Arc<Inner>) {
        loop {
            let start = {
                let mut s = this.lock();
                if s.running >= s.cfg.max_jobs.max(1) {
                    return;
                }
                let Some(pos) = s
                    .queue
                    .iter()
                    .position(|id| s.jobs.get(id).is_some_and(|r| !r.ctl.overlay().paused))
                else {
                    return;
                };
                let id = s.queue.remove(pos).expect("position is in range");
                s.running += 1;
                let cfg = s.cfg.clone();
                let r = s.jobs.get_mut(&id).expect("queued jobs exist");
                r.phase = Phase::Scanning;
                (id, r.job.clone(), r.ctl.clone(), r.meter.clone(), r.done.clone(), r.notes.clone(), r.items.clone(), r.partials.clone(), cfg)
            };
            let (id, job, ctl, meter, done, notes, items, partials, cfg) = start;
            this.refresh(id);
            let inner = this.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("filewinch-job-{id}"))
                .spawn(move || {
                    let ctx = runner::JobCtx { inner: inner.clone(), id, job, ctl, meter, done, notes, items, partials, cfg };
                    let end = runner::run(&ctx);
                    if let Some(p) = ctx.meter.force_sample() {
                        inner.emit(Event::Progress(id, p));
                    }
                    if ctx.items.take_event(true) {
                        inner.emit(Event::ItemsChanged(id));
                    }
                    inner.lock().running -= 1;
                    inner.set_phase(id, Phase::Finished(end));
                    Inner::schedule(&inner);
                });
            if let Err(e) = spawned {
                this.lock().running -= 1;
                let err = Error::new(ErrorKind::Other, "Couldn't start the transfer.").with_detail(e);
                this.set_phase(id, Phase::Finished(JobState::Failed(err)));
            }
        }
    }
}
