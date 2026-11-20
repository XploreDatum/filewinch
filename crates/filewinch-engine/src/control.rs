//! Per-job control (pause, cancel, conflict questions, retry overlay) and the shared
//! bandwidth limiter.

use crate::{ConflictAnswer, ConflictChoice};
use filewinch_core::{Error, Result};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[derive(Default)]
struct CtlState {
    paused: bool,
    cancelled: bool,
    /// Cancelled because the engine is going away: keep partial downloads for next time.
    shutdown: bool,
    /// A worker is waiting for an answer.
    asking: bool,
    answer: Option<ConflictAnswer>,
    /// Set by an answer with `apply_to_all`.
    sticky: Option<ConflictChoice>,
    /// Workers currently backing off, with the latest (attempt, next try).
    retrying: Vec<(u64, u32, Instant)>,
    next_token: u64,
    /// Files this job may transfer at once; 0 until the job starts or someone sets it.
    files_limit: usize,
    /// Files transferring now.
    files_active: usize,
}

/// The flags that change what state a running job shows.
pub(crate) struct Overlay {
    pub paused: bool,
    pub asking: bool,
    pub retrying: Option<(u32, Instant)>,
}

#[derive(Default)]
pub(crate) struct JobCtl {
    m: Mutex<CtlState>,
    cv: Condvar,
}

impl JobCtl {
    fn lock(&self) -> MutexGuard<'_, CtlState> {
        self.m.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wait<'a>(&self, g: MutexGuard<'a, CtlState>) -> MutexGuard<'a, CtlState> {
        self.cv.wait(g).unwrap_or_else(|e| e.into_inner())
    }

    pub fn overlay(&self) -> Overlay {
        let s = self.lock();
        Overlay {
            paused: s.paused,
            asking: s.asking,
            retrying: s.retrying.iter().max_by_key(|r| r.2).map(|r| (r.1, r.2)),
        }
    }

    pub fn set_paused(&self, paused: bool) {
        self.lock().paused = paused;
        self.cv.notify_all();
    }

    pub fn cancel(&self) {
        self.lock().cancelled = true;
        self.cv.notify_all();
    }

    /// Cancels because the engine is shutting down; partial segmented downloads stay.
    pub fn shutdown(&self) {
        let mut s = self.lock();
        s.cancelled = true;
        s.shutdown = true;
        drop(s);
        self.cv.notify_all();
    }

    /// True when a cancel should keep partial downloads (see [`JobCtl::shutdown`]).
    pub fn keeps_partials(&self) -> bool {
        self.lock().shutdown
    }

    pub fn is_cancelled(&self) -> bool {
        self.lock().cancelled
    }

    /// Called between chunks: blocks while paused, fails once cancelled.
    pub fn checkpoint(&self) -> Result<()> {
        let mut s = self.lock();
        loop {
            if s.cancelled {
                return Err(Error::cancelled());
            }
            if !s.paused {
                return Ok(());
            }
            s = self.wait(s);
        }
    }

    /// Like [`JobCtl::checkpoint`], but also fails (as cancelled) once `skip` is set, even
    /// while paused.
    pub fn checkpoint_or(&self, skip: &std::sync::atomic::AtomicBool) -> Result<()> {
        let mut s = self.lock();
        loop {
            if s.cancelled || skip.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(Error::cancelled());
            }
            if !s.paused {
                return Ok(());
            }
            s = self.wait(s);
        }
    }

    /// Wakes every waiter to look again (an item was skipped).
    pub fn poke(&self) {
        let _g = self.lock();
        self.cv.notify_all();
    }

    /// Sleeps up to `d`, waking early (with an error) on cancel. Pausing during the sleep
    /// extends it until resumed.
    pub fn sleep(&self, d: Duration) -> Result<()> {
        let end = Instant::now() + d;
        let mut s = self.lock();
        loop {
            if s.cancelled {
                return Err(Error::cancelled());
            }
            let now = Instant::now();
            if now >= end && !s.paused {
                return Ok(());
            }
            let left = if s.paused { Duration::from_secs(3600) } else { end - now };
            s = self.cv.wait_timeout(s, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// Files the job may move at once (0 while unset).
    pub fn files_limit(&self) -> usize {
        self.lock().files_limit
    }

    /// Changes how many files may move at once. Raising it lets waiting workers start at
    /// once; lowering it lets in-flight files finish and starts no new one until fewer than
    /// `n` are moving.
    pub fn set_files_limit(&self, n: usize) {
        self.lock().files_limit = n.max(1);
        self.cv.notify_all();
    }

    /// Sets the limit unless someone already did (a job about to start).
    pub fn init_files_limit(&self, n: usize) {
        let mut s = self.lock();
        if s.files_limit == 0 {
            s.files_limit = n.max(1);
        }
    }

    /// Waits for a free file slot; fails once cancelled.
    pub fn acquire_file(&self) -> Result<()> {
        let mut s = self.lock();
        loop {
            if s.cancelled {
                return Err(Error::cancelled());
            }
            if s.files_active < s.files_limit.max(1) {
                s.files_active += 1;
                return Ok(());
            }
            s = self.wait(s);
        }
    }

    pub fn release_file(&self) {
        let mut s = self.lock();
        s.files_active = s.files_active.saturating_sub(1);
        drop(s);
        self.cv.notify_all();
    }

    pub fn begin_retry(&self, attempt: u32, at: Instant) -> u64 {
        let mut s = self.lock();
        s.next_token += 1;
        let t = s.next_token;
        s.retrying.push((t, attempt, at));
        t
    }

    pub fn end_retry(&self, token: u64) {
        self.lock().retrying.retain(|r| r.0 != token);
    }

    /// Asks one question at a time. `open` runs once this worker owns the question (it should
    /// refresh the job state and emit the Conflict event); returns the user's choice, or a
    /// sticky choice that another answer set meanwhile.
    pub fn ask(&self, open: impl FnOnce()) -> Result<ConflictChoice> {
        let mut s = self.lock();
        loop {
            if s.cancelled {
                return Err(Error::cancelled());
            }
            if let Some(c) = s.sticky {
                return Ok(c);
            }
            if !s.asking {
                break;
            }
            s = self.wait(s);
        }
        s.asking = true;
        s.answer = None;
        drop(s);
        open();
        let mut s = self.lock();
        let result = loop {
            if s.cancelled {
                break Err(Error::cancelled());
            }
            if let Some(a) = s.answer.take() {
                if a.apply_to_all {
                    s.sticky = Some(a.choice);
                }
                break Ok(a.choice);
            }
            s = self.wait(s);
        };
        s.asking = false;
        drop(s);
        self.cv.notify_all();
        result
    }

    pub fn answer(&self, a: ConflictAnswer) {
        let mut s = self.lock();
        if s.asking {
            s.answer = Some(a);
            drop(s);
            self.cv.notify_all();
        }
    }
}

/// Token bucket shared by every job. Tokens may go negative: a caller takes its whole chunk and
/// then sleeps off the debt, so chunk size never deadlocks against a low rate.
pub(crate) struct Bucket {
    m: Mutex<BucketState>,
}

struct BucketState {
    rate: Option<u64>,
    tokens: f64,
    last: Instant,
}

impl Bucket {
    pub fn new(rate: Option<u64>) -> Bucket {
        Bucket { m: Mutex::new(BucketState { rate, tokens: 0.0, last: Instant::now() }) }
    }

    pub fn set_rate(&self, rate: Option<u64>) {
        let mut b = self.m.lock().unwrap_or_else(|e| e.into_inner());
        b.rate = rate.filter(|r| *r > 0);
        b.tokens = b.tokens.min(0.0);
        b.last = Instant::now();
    }

    /// Accounts for `n` bytes, sleeping (interruptibly) while over the limit.
    pub fn take(&self, n: usize, ctl: &JobCtl) -> Result<()> {
        let wait = {
            let mut b = self.m.lock().unwrap_or_else(|e| e.into_inner());
            let Some(rate) = b.rate.filter(|r| *r > 0) else { return Ok(()) };
            let rate = rate as f64;
            let now = Instant::now();
            let burst = rate / 4.0;
            b.tokens = (b.tokens + now.duration_since(b.last).as_secs_f64() * rate).min(burst);
            b.last = now;
            b.tokens -= n as f64;
            if b.tokens >= 0.0 { Duration::ZERO } else { Duration::from_secs_f64(-b.tokens / rate) }
        };
        if wait.is_zero() { Ok(()) } else { ctl.sleep(wait) }
    }
}
