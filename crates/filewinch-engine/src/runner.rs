//! Runs one job: scan on the job thread, transfer or delete files on worker threads, then
//! clean up folders and summarise.

use crate::control::JobCtl;
use crate::meter::Meter;
use crate::{EngineConfig, Event, Inner, Job, JobId, JobState, Op, Phase, transfer, walk};
use filewinch_core::{Entry, Error, ErrorKind, RPath, Remote, Result};
use std::collections::HashSet;
use std::hash::{BuildHasher, Hasher};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) struct JobCtx {
    pub inner: Arc<Inner>,
    pub id: JobId,
    pub job: Job,
    pub ctl: Arc<JobCtl>,
    pub meter: Arc<Meter>,
    pub done: Arc<Mutex<HashSet<RPath>>>,
    pub cfg: EngineConfig,
}

/// A file (or, for Delete, anything that isn't a folder) to process on a worker.
pub(crate) struct Item {
    pub src: Entry,
    /// Where it should land (before conflict handling). Unused for Delete.
    pub dst: RPath,
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

    pub fn progress_tick(&self) {
        if let Some(p) = self.meter.maybe_sample() {
            self.emit(Event::Progress(self.id, p));
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
        let token = self.ctl.begin_retry(attempt, Instant::now() + d);
        self.inner.refresh(self.id);
        let r = self.ctl.sleep(d);
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
    };
    let total = ctx.meter.progress().files_total.unwrap_or(0).max(failed.len() as u64);
    let message = if failed.len() == 1 {
        format!("{} couldn't be {verb}: {}", failed[0].0.name(), failed[0].1.message)
    } else {
        format!("{} of {total} items couldn't be {verb}.", failed.len())
    };
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
    let n = ctx.cfg.max_files_per_job.max(1);
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
        let item = match rx.lock().unwrap_or_else(|e| e.into_inner()).recv() {
            Ok(i) => i,
            Err(_) => return,
        };
        if ctx.ctl.is_cancelled() {
            continue; // drain without doing anything
        }
        match work(&item) {
            Ok(Outcome::Done) => {
                ctx.done.lock().unwrap_or_else(|e| e.into_inner()).insert(item.src.path.clone());
                ctx.meter.file_done();
            }
            Ok(Outcome::Skipped) => {
                tally.keep(&item.src.path);
                ctx.meter.add_bytes(item.src.size.unwrap_or(0));
                ctx.meter.file_done();
            }
            Err(e) => ctx.fail(tally, &item.src.path, e),
        }
        ctx.progress_tick();
    }
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
