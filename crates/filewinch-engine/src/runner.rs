//! Runs one job: scan on the job thread, transfer or delete files on worker threads, then
//! clean up folders and summarise.

use crate::control::JobCtl;
use crate::items::{ItemState, Items, Live};
use crate::meter::Meter;
use crate::{EngineConfig, Event, Inner, Job, JobId, JobState, Op, Phase, transfer, walk};
use filewinch_core::{Entry, Error, ErrorKind, RPath, Remote, Result};
use std::collections::HashSet;
use std::hash::{BuildHasher, Hasher};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::cell::RefCell;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

thread_local! {
    /// The file this thread is moving (a worker, or one of a segmented download's threads).
    static CURRENT: RefCell<Option<Arc<Live>>> = const { RefCell::new(None) };
}

pub(crate) fn current_item() -> Option<Arc<Live>> {
    CURRENT.with(|c| c.borrow().clone())
}

pub(crate) fn set_current_item(live: Option<Arc<Live>>) {
    CURRENT.with(|c| *c.borrow_mut() = live);
}

pub(crate) struct JobCtx {
    pub inner: Arc<Inner>,
    pub id: JobId,
    pub job: Job,
    pub ctl: Arc<JobCtl>,
    pub meter: Arc<Meter>,
    pub done: Arc<Mutex<HashSet<RPath>>>,
    pub notes: Arc<Mutex<Vec<String>>>,
    pub items: Arc<Items>,
    /// Temp files of unfinished segmented downloads (see [`crate::Engine::forget`]).
    pub partials: Arc<Mutex<HashSet<RPath>>>,
    pub cfg: EngineConfig,
}

/// A file (or, for Delete, anything that isn't a folder) to process on a worker.
pub(crate) struct Item {
    pub src: Entry,
    /// Where it should land (before conflict handling). Unused for Delete.
    pub dst: RPath,
    /// Its live counters and skip flag (see [`crate::Engine::skip_item`]).
    pub live: Arc<Live>,
    /// Move only: copy it but leave the source, because it is (inside) a shared item that a
    /// copy-then-delete would empty for everyone.
    pub keep_src: bool,
}

pub(crate) enum Outcome {
    Done,
    /// Left in place (conflict Skip); its source must stay for a Move.
    Skipped,
}

/// Failures and paths that stay behind, shared by the scanner and workers.
#[derive(Default)]
pub(crate) struct Tally {
    pub failed: Mutex<Vec<(RPath, Error)>>,
    /// Source paths that remain after the job (failed or skipped), so their parent folders
    /// must not be removed by a Move or Delete.
    pub kept: Mutex<Vec<RPath>>,
    /// Files the person skipped.
    pub skipped: std::sync::atomic::AtomicUsize,
}

impl Tally {
    pub fn keep(&self, p: &RPath) {
        self.kept.lock().unwrap_or_else(|e| e.into_inner()).push(p.clone());
    }

    fn holds_inside(&self, dir: &RPath) -> bool {
        self.kept.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|k| k.starts_with(dir))
    }
}

impl JobCtx {
    pub fn src(&self) -> &dyn Remote {
        &*self.job.src
    }

    /// True when source and destination are the same session object.
    pub fn same_remote(&self) -> bool {
        self.job.dst.as_ref().is_some_and(|d| {
            Arc::as_ptr(d) as *const () == Arc::as_ptr(&self.job.src) as *const ()
        })
    }

    pub fn emit(&self, ev: Event) {
        self.inner.emit(ev);
    }

    /// Adds a line to the job's notes ([`crate::Engine::notes`]).
    pub fn note(&self, line: String) {
        self.notes.lock().unwrap_or_else(|e| e.into_inner()).push(line);
    }

    /// Records (or clears) a temp file this job keeps on its destination.
    pub fn track_partial(&self, temp: &RPath, kept: bool) {
        let mut p = self.partials.lock().unwrap_or_else(|e| e.into_inner());
        if kept {
            p.insert(temp.clone());
        } else {
            p.remove(temp);
        }
    }

    pub fn progress_tick(&self) {
        if let Some(p) = self.meter.maybe_sample() {
            self.emit(Event::Progress(self.id, p));
        }
        if self.items.take_event(false) {
            self.emit(Event::ItemsChanged(self.id));
        }
    }

    /// Counts bytes for the job and for the file this thread is moving.
    pub fn add_bytes(&self, n: u64) {
        self.meter.add_bytes(n);
        if let Some(l) = current_item() {
            l.bytes.fetch_add(n, Ordering::Relaxed);
        }
    }

    pub fn sub_bytes(&self, n: u64) {
        self.meter.sub_bytes(n);
        if let Some(l) = current_item() {
            let _ = l.bytes.try_update(Ordering::Relaxed, Ordering::Relaxed, |b| Some(b.saturating_sub(n)));
        }
    }

    /// Between chunks: blocks while paused, fails once the job is cancelled or this thread's
    /// file was skipped.
    pub fn checkpoint(&self) -> Result<()> {
        match current_item() {
            Some(l) => self.ctl.checkpoint_or(&l.skip),
            None => self.ctl.checkpoint(),
        }
    }

    /// True when this thread's file was skipped.
    pub fn item_skipped(&self) -> bool {
        current_item().is_some_and(|l| l.skip.load(Ordering::SeqCst))
    }

    /// The path as the Transfers list shows it: from the selected item down ("20nov/a.mov").
    pub fn rel(&self, p: &RPath) -> String {
        let base = self.job.src_paths.iter().find(|s| p.starts_with(s)).and_then(|s| s.parent());
        match base {
            Some(b) if !b.is_root() => p.as_str()[b.as_str().len()..].trim_start_matches('/').to_owned(),
            _ => p.as_str().trim_start_matches('/').to_owned(),
        }
    }

    /// Records a failed item and tells the UI. Cancellation is not a failure.
    pub fn fail(&self, tally: &Tally, path: &RPath, error: Error) {
        tally.keep(path);
        if error.kind == ErrorKind::Cancelled {
            return;
        }
        tally.failed.lock().unwrap_or_else(|e| e.into_inner()).push((path.clone(), error.clone()));
        self.emit(Event::ItemFailed { job: self.id, path: path.clone(), error });
    }

    /// Waits out a backoff for `attempt`, showing the job as Retrying meanwhile.
    pub fn backoff(&self, attempt: u32, err: &Error) -> Result<()> {
        let d = backoff_delay(self.cfg.retry_delay, attempt, err.retry_after);
        let at = Instant::now() + d;
        let token = self.ctl.begin_retry(attempt, at);
        let live = current_item();
        if let Some(l) = &live {
            *l.retry_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(at);
            self.items.touch();
        }
        self.inner.refresh(self.id);
        let r = self.ctl.sleep(d);
        if let Some(l) = &live {
            *l.retry_at.lock().unwrap_or_else(|e| e.into_inner()) = None;
            self.items.touch();
        }
        self.ctl.end_retry(token);
        self.inner.refresh(self.id);
        r
    }

    /// Runs `f`, retrying transient errors with backoff up to `cfg.retries` times.
    pub fn retrying<T>(&self, mut f: impl FnMut() -> Result<T>) -> Result<T> {
        let mut attempt = 0;
        loop {
            self.ctl.checkpoint()?;
            match f() {
                Err(e) if e.kind.is_transient() && attempt < self.cfg.retries => {
                    attempt += 1;
                    self.backoff(attempt, &e)?;
                }
                r => return r,
            }
        }
    }
}

/// Exponential backoff (base × 2^(attempt-1), capped at 60 s) with ±25 % jitter. A server's
/// `retry_after` wins when it is longer.
pub(crate) fn backoff_delay(base: Duration, attempt: u32, retry_after: Option<Duration>) -> Duration {
    let exp = base.saturating_mul(1u32 << attempt.saturating_sub(1).min(16)).min(Duration::from_secs(60));
    let r = std::collections::hash_map::RandomState::new().build_hasher().finish();
    let jitter = 0.75 + (r % 1000) as f64 / 2000.0;
    let d = exp.mul_f64(jitter);
    retry_after.map_or(d, |ra| ra.max(d))
}

pub(crate) fn run(ctx: &JobCtx) -> JobState {
    if let Some(p) = ctx.meter.force_sample() {
        ctx.emit(Event::Progress(ctx.id, p));
    }
    let tally = Tally::default();
    let result = match ctx.job.op {
        Op::Delete => run_delete(ctx, &tally),
        Op::SetMode { mode, mask, dirs_get_x_where_r } => run_set_mode(ctx, &tally, mode, mask, dirs_get_x_where_r),
        Op::Copy | Op::Move => run_transfer(ctx, &tally),
    };
    finish_state(ctx, &tally, result)
}

fn finish_state(ctx: &JobCtx, tally: &Tally, result: Result<()>) -> JobState {
    if ctx.ctl.is_cancelled() {
        return JobState::Cancelled;
    }
    if let Err(e) = result {
        return if e.kind == ErrorKind::Cancelled { JobState::Cancelled } else { JobState::Failed(e) };
    }
    let failed = tally.failed.lock().unwrap_or_else(|e| e.into_inner());
    if failed.is_empty() {
        return JobState::Done;
    }
    let verb = match ctx.job.op {
        Op::Copy => "copied",
        Op::Move => "moved",
        Op::Delete => "deleted",
        Op::SetMode { .. } => "changed",
    };
    let total = ctx.meter.progress().files_total.unwrap_or(0).max(failed.len() as u64);
    let mut message = if failed.len() == 1 {
        format!("{} couldn't be {verb}: {}", failed[0].0.name(), failed[0].1.message)
    } else {
        format!("{} of {total} items couldn't be {verb}.", failed.len())
    };
    match tally.skipped.load(Ordering::SeqCst) {
        0 => {}
        1 => message.push_str(" 1 was skipped."),
        n => message.push_str(&format!(" {n} were skipped.")),
    }
    let kind = if failed.iter().all(|f| f.1.kind == failed[0].1.kind) { failed[0].1.kind } else { ErrorKind::Other };
    let mut detail: Vec<String> = failed.iter().take(10).map(|(p, e)| format!("{p}: {e}")).collect();
    if failed.len() > 10 {
        detail.push(format!("…and {} more", failed.len() - 10));
    }
    JobState::Failed(Error::new(kind, message).with_detail(detail.join("\n")))
}

/// Starts `n` workers pulling items from a channel; `scan` feeds it on this thread.
fn with_workers(
    ctx: &JobCtx,
    tally: &Tally,
    work: impl Fn(&Item) -> Result<Outcome> + Sync,
    scan: impl FnOnce(&Sender<Item>) -> Result<()>,
) -> Result<()> {
    let (tx, rx) = channel::<Item>();
    let rx = Mutex::new(rx);
    ctx.ctl.init_files_limit(ctx.cfg.max_files_per_job);
    // Enough workers for the largest limit the job may be given while it runs; the job's
    // files limit (see `Engine::set_job_files`) decides how many work at once.
    let n = ctx.ctl.files_limit().max(crate::MAX_FILES_PER_JOB_LIVE);
    std::thread::scope(|s| {
        for _ in 0..n {
            s.spawn(|| worker(ctx, tally, &rx, &work));
        }
        let r = scan(&tx);
        ctx.meter.scan_done();
        if !matches!(r, Err(ref e) if e.kind == ErrorKind::Cancelled) {
            ctx.inner.set_phase(ctx.id, Phase::Running);
        }
        drop(tx);
        r
    })
}

fn worker(ctx: &JobCtx, tally: &Tally, rx: &Mutex<Receiver<Item>>, work: &(impl Fn(&Item) -> Result<Outcome> + Sync)) {
    loop {
        // A file slot first, then the next item: only a worker holding a slot takes one, so
        // files start in the order they were queued (racing for slots after taking items let
        // a later file jump ahead). A lowered limit holds new files back; in-flight ones finish.
        let slot = ctx.ctl.acquire_file().is_ok();
        let item = match rx.lock().unwrap_or_else(|e| e.into_inner()).recv() {
            Ok(i) => i,
            Err(_) => {
                if slot {
                    ctx.ctl.release_file();
                }
                return;
            }
        };
        if !slot {
            continue; // cancelled: drain without doing anything
        }
        if item.live.skip.load(Ordering::SeqCst) {
            ctx.ctl.release_file();
            skipped_by_person(ctx, tally, &item);
            continue;
        }
        let path = if matches!(ctx.job.op, Op::Delete | Op::SetMode { .. }) { item.src.path.clone() } else { item.dst.clone() };
        ctx.emit(Event::ItemStarted { job: ctx.id, path: path.clone(), size: item.src.size });
        ctx.items.set(&item.src.path, ItemState::Active, None);
        set_current_item(Some(item.live.clone()));
        let result = work(&item);
        set_current_item(None);
        ctx.ctl.release_file();
        // Written, moved, deleted or failed: the destination may have changed. A skip changed
        // nothing, so it says nothing here.
        let done = |ctx: &JobCtx| {
            let from = (ctx.job.op == Op::Move).then(|| item.src.path.clone());
            ctx.emit(Event::ItemDone { job: ctx.id, path: path.clone(), from });
        };
        match result {
            Ok(Outcome::Done) => {
                done(ctx);
                ctx.done.lock().unwrap_or_else(|e| e.into_inner()).insert(item.src.path.clone());
                ctx.meter.file_done();
                ctx.items.set(&item.src.path, ItemState::Done, None);
            }
            Ok(Outcome::Skipped) => {
                tally.keep(&item.src.path);
                ctx.meter.add_bytes(item.src.size.unwrap_or(0));
                ctx.meter.file_done();
                ctx.items.set(&item.src.path, ItemState::Skipped, None);
            }
            Err(_) if item.live.skip.load(Ordering::SeqCst) => {
                // Stopped by a skip: what it moved no longer counts.
                ctx.meter.sub_bytes(item.live.bytes.swap(0, Ordering::SeqCst));
                skipped_by_person(ctx, tally, &item);
            }
            Err(e) => {
                done(ctx);
                ctx.items.set(&item.src.path, ItemState::Failed, Some(e.clone()));
                ctx.fail(tally, &item.src.path, e);
            }
        }
        ctx.progress_tick();
    }
}

/// A file the person skipped: it leaves the totals, keeps its source (for a Move) and never
/// fails the job.
fn skipped_by_person(ctx: &JobCtx, tally: &Tally, item: &Item) {
    tally.keep(&item.src.path);
    tally.skipped.fetch_add(1, Ordering::SeqCst);
    ctx.meter.remove_total(1, item.src.size.unwrap_or(0));
    ctx.items.set(&item.src.path, ItemState::Skipped, None);
}

fn run_transfer(ctx: &JobCtx, tally: &Tally) -> Result<()> {
    let (Some(dst), Some(dst_dir)) = (ctx.job.dst.as_deref(), ctx.job.dst_dir.as_ref()) else {
        return Err(Error::new(ErrorKind::Other, "This transfer has no destination."));
    };
    match ctx.retrying(|| dst.stat(dst_dir)) {
        Ok(e) if e.is_dir() => {}
        Ok(_) => return Err(Error::new(ErrorKind::InvalidPath, format!("{dst_dir} isn't a folder."))),
        Err(e) if e.kind == ErrorKind::NotFound => {
            return Err(Error::new(ErrorKind::NotFound, format!("The folder {dst_dir} no longer exists.")));
        }
        Err(e) => return Err(e),
    }
    let mut src_dirs = Vec::new();
    with_workers(
        ctx,
        tally,
        |item| transfer::file(ctx, item),
        |tx| walk::scan_transfer(ctx, tally, tx, &mut src_dirs),
    )?;
    if ctx.job.op == Op::Move && !ctx.ctl.is_cancelled() {
        remove_dirs(ctx, tally, &src_dirs);
    }
    Ok(())
}

fn run_delete(ctx: &JobCtx, tally: &Tally) -> Result<()> {
    let mut dirs = Vec::new();
    with_workers(
        ctx,
        tally,
        |item| {
            ctx.meter.set_current(&item.src.path);
            match ctx.retrying(|| ctx.src().remove_file(&item.src.path)) {
                Err(e) if e.kind != ErrorKind::NotFound => Err(e),
                _ => {
                    ctx.meter.add_bytes(item.src.size.unwrap_or(0));
                    Ok(Outcome::Done)
                }
            }
        },
        |tx| walk::scan_delete(ctx, tally, tx, &mut dirs),
    )?;
    if !ctx.ctl.is_cancelled() {
        remove_dirs(ctx, tally, &dirs);
    }
    Ok(())
}

fn run_set_mode(ctx: &JobCtx, tally: &Tally, mode: u32, mask: u32, x_rule: bool) -> Result<()> {
    if !ctx.src().caps().permissions {
        return Err(Error::new(ErrorKind::Unsupported, "This server doesn't support changing permissions."));
    }
    let change = |e: &Entry| -> Result<()> {
        ctx.meter.set_current(&e.path);
        let old = match e.mode {
            Some(m) => m,
            None if mask & 0o7777 == 0o7777 => 0,
            None => match ctx.retrying(|| ctx.src().stat(&e.path))?.mode {
                Some(m) => m,
                None => {
                    let msg = format!("The server didn't say what permissions “{}” has, so they couldn't be changed in part.", e.name);
                    return Err(Error::new(ErrorKind::Unsupported, msg));
                }
            },
        };
        let new = crate::applied_mode(old, e.is_dir(), mode, mask, x_rule);
        if new != old & 0o7777 {
            ctx.checkpoint()?;
            ctx.retrying(|| ctx.src().set_mode(&e.path, new))?;
        }
        Ok(())
    };
    let mut dirs = Vec::new();
    with_workers(ctx, tally, |item| change(&item.src).map(|()| Outcome::Done), |tx| walk::scan_set_mode(ctx, tally, tx, &mut dirs))?;
    // Folders last, deepest first: everything inside is done, so no change locks the job out.
    for (d, live) in dirs.iter().rev() {
        if ctx.ctl.is_cancelled() {
            break;
        }
        ctx.ctl.checkpoint()?;
        if live.skip.load(Ordering::SeqCst) || ctx.items.under_skipped(&d.path) {
            tally.skipped.fetch_add(1, Ordering::SeqCst);
            ctx.meter.remove_total(1, 0);
            continue;
        }
        ctx.items.set(&d.path, ItemState::Active, None);
        set_current_item(Some(live.clone()));
        let r = change(d);
        set_current_item(None);
        match r {
            Ok(()) => {
                ctx.items.set(&d.path, ItemState::Done, None);
                ctx.meter.file_done();
            }
            Err(_) if live.skip.load(Ordering::SeqCst) => ctx.meter.remove_total(1, 0),
            Err(e) if e.kind == ErrorKind::Cancelled => return Err(e),
            Err(e) => {
                ctx.items.set(&d.path, ItemState::Failed, Some(e.clone()));
                ctx.fail(tally, &d.path, e);
            }
        }
        ctx.progress_tick();
    }
    Ok(())
}

/// Removes source folders deepest first (`dirs` is in walk pre-order), skipping any that still
/// hold something that failed or was skipped.
fn remove_dirs(ctx: &JobCtx, tally: &Tally, dirs: &[RPath]) {
    for d in dirs.iter().rev() {
        if ctx.ctl.is_cancelled() || tally.holds_inside(d) {
            continue;
        }
        match ctx.retrying(|| ctx.src().remove_dir(d)) {
            Ok(()) => {}
            Err(e) if e.kind == ErrorKind::NotFound => {}
            Err(e) => ctx.fail(tally, d, e),
        }
    }
}
