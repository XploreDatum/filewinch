//! Per-item state of a job (the files and folders it found, what each is doing) for the
//! Transfers list, and skipping single items without stopping the job.
//!
//! The UI pulls pages of it ([`crate::Engine::items`]) when [`crate::Event::ItemsChanged`]
//! says something changed; that event is throttled to about four a second per job.

use filewinch_core::{Error, RPath};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How often a job may say its items changed.
const EVENT_EVERY: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ItemState {
    Queued,
    Active,
    Done,
    Failed,
    /// Skipped by the person ([`crate::Engine::skip_item`]) or by the conflict policy.
    Skipped,
}

/// What a running file is doing right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivePhase {
    /// No bytes yet: opening the connection, waiting for one from the pool, or for the
    /// server to answer.
    Connecting,
    Transferring,
    /// Waiting out a backoff after a transient error; the next try starts at `at`.
    Retrying { at: Instant },
}

/// Which items [`crate::Engine::items`] returns.
pub type ItemFilter = ItemState;

/// One file or folder of a job, as the Transfers list shows it.
#[derive(Clone, Debug)]
pub struct ItemView {
    /// The source path ([`crate::Engine::skip_item`] takes it).
    pub path: RPath,
    /// From the job's selection down: "20nov/raw/clip-07.mov".
    pub rel: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub bytes_done: u64,
    /// Average bytes per second since the item started (active files only).
    pub bytes_per_sec: f64,
    pub state: ItemState,
    /// For an active file: connecting, moving bytes, or waiting to retry.
    pub phase: Option<ActivePhase>,
    pub error: Option<Error>,
}

/// How many items are in each state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ItemCounts {
    pub active: usize,
    pub queued: usize,
    pub done: usize,
    pub failed: usize,
    pub skipped: usize,
}

/// A running file's live counters, shared with whatever moves its bytes.
#[derive(Default)]
pub(crate) struct Live {
    pub bytes: AtomicU64,
    pub skip: AtomicBool,
    /// Set while backing off before a retry.
    pub retry_at: Mutex<Option<Instant>>,
}

struct Rec {
    path: RPath,
    rel: String,
    is_dir: bool,
    /// Acted on itself (see [`Items::found`]).
    work: bool,
    size: Option<u64>,
    state: ItemState,
    error: Option<Error>,
    started: Option<Instant>,
    live: Arc<Live>,
}

#[derive(Default)]
struct State {
    recs: Vec<Rec>,
    index: HashMap<RPath, usize>,
    /// Folders skipped as a whole: anything found under them later is skipped too.
    skipped_dirs: Vec<RPath>,
    last_event: Option<Instant>,
    pending: bool,
}

#[derive(Default)]
pub(crate) struct Items {
    m: Mutex<State>,
}

/// What [`Items::skip`] did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Skip {
    /// Unknown, or already finished: nothing to do.
    Nothing,
    /// Marked; running files stop at their next chunk.
    Skipped,
}

impl Items {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// True when `p` is inside (or is) a folder the person skipped.
    pub fn under_skipped(&self, p: &RPath) -> bool {
        self.lock().skipped_dirs.iter().any(|d| p.starts_with(d))
    }

    /// Records a file or folder the walk found (or finds again on a retry), queued. `work`: the
    /// item itself is acted on (a folder whose permissions change), not just walked through,
    /// so it counts in [`Items::counts`]. Returns its live counters, or None when it lies in a
    /// skipped folder.
    pub fn found(&self, path: &RPath, rel: String, is_dir: bool, size: Option<u64>, work: bool) -> Option<Arc<Live>> {
        let mut s = self.lock();
        if s.skipped_dirs.iter().any(|d| path.starts_with(d)) {
            return None;
        }
        let live = Arc::new(Live::default());
        match s.index.get(path).copied() {
            Some(i) => {
                let r = &mut s.recs[i];
                if r.state == ItemState::Done {
                    return Some(r.live.clone());
                }
                r.state = ItemState::Queued;
                r.error = None;
                r.size = size;
                r.started = None;
                r.live = live.clone();
            }
            None => {
                let i = s.recs.len();
                s.recs.push(Rec { path: path.clone(), rel, is_dir, work, size, state: ItemState::Queued, error: None, started: None, live: live.clone() });
                s.index.insert(path.clone(), i);
            }
        }
        s.pending = true;
        Some(live)
    }

    /// Moves an item to `state`. A skipped item stays skipped unless it finished after all
    /// (`Done`: the file landed, so it counts as done).
    pub fn set(&self, path: &RPath, state: ItemState, error: Option<Error>) {
        let mut s = self.lock();
        let Some(i) = s.index.get(path).copied() else { return };
        let r = &mut s.recs[i];
        if r.state == ItemState::Skipped && (state != ItemState::Done || r.is_dir) {
            return;
        }
        if state == ItemState::Active {
            r.started = Some(Instant::now());
        }
        r.state = state;
        r.error = error;
        s.pending = true;
    }

    /// Skips `path`: a queued or running file, or a folder with everything under it (also what
    /// the walk hasn't found yet).
    pub fn skip(&self, path: &RPath) -> Skip {
        let mut s = self.lock();
        let is_dir = match s.index.get(path) {
            Some(&i) => s.recs[i].is_dir,
            None => return Skip::Nothing,
        };
        let any = if is_dir {
            if !s.skipped_dirs.contains(path) {
                s.skipped_dirs.push(path.clone());
            }
            for r in s.recs.iter_mut().filter(|r| r.path.starts_with(path)) {
                mark(r);
            }
            true // later finds under it are skipped too
        } else {
            let i = s.index[path];
            mark(&mut s.recs[i])
        };
        if any {
            s.pending = true;
            Skip::Skipped
        } else {
            Skip::Nothing
        }
    }

    /// Notes a change that has no state of its own (a retry starting or ending).
    pub fn touch(&self) {
        self.lock().pending = true;
    }

    pub fn counts(&self) -> ItemCounts {
        let s = self.lock();
        let mut c = ItemCounts::default();
        for (r, st) in s.recs.iter().zip(derived(&s.recs)) {
            if r.is_dir && !r.work {
                continue; // a folder walked through isn't an item of its own
            }
            match st {
                ItemState::Active => c.active += 1,
                ItemState::Queued => c.queued += 1,
                ItemState::Done => c.done += 1,
                ItemState::Failed => c.failed += 1,
                ItemState::Skipped => c.skipped += 1,
            }
        }
        c
    }

    /// Items in `filter`, in the order found: the total and the slice `range`.
    pub fn page(&self, filter: ItemFilter, range: std::ops::Range<usize>) -> (usize, Vec<ItemView>) {
        let s = self.lock();
        let states = derived(&s.recs);
        let matching: Vec<usize> = (0..s.recs.len()).filter(|&i| states[i] == filter).collect();
        let total = matching.len();
        let now = Instant::now();
        let out = matching
            .get(range.start.min(total)..range.end.min(total))
            .unwrap_or(&[])
            .iter()
            .map(|&i| {
                let r = &s.recs[i];
                let bytes_done = match states[i] {
                    ItemState::Done if !r.is_dir => r.size.unwrap_or_else(|| r.live.bytes.load(Ordering::Relaxed)),
                    _ => r.live.bytes.load(Ordering::Relaxed),
                };
                let bytes_per_sec = match (states[i], r.started) {
                    (ItemState::Active, Some(t)) if !r.is_dir => {
                        let secs = now.duration_since(t).as_secs_f64();
                        if secs > 0.2 { bytes_done as f64 / secs } else { 0.0 }
                    }
                    _ => 0.0,
                };
                let phase = (states[i] == ItemState::Active && !r.is_dir).then(|| {
                    match *r.live.retry_at.lock().unwrap_or_else(|e| e.into_inner()) {
                        Some(at) => ActivePhase::Retrying { at },
                        None if bytes_done == 0 => ActivePhase::Connecting,
                        None => ActivePhase::Transferring,
                    }
                });
                ItemView {
                    phase,
                    path: r.path.clone(),
                    rel: r.rel.clone(),
                    is_dir: r.is_dir,
                    size: r.size,
                    bytes_done,
                    bytes_per_sec,
                    state: states[i],
                    error: r.error.clone(),
                }
            })
            .collect();
        (total, out)
    }

    /// True (once) when the items changed and a quarter second has passed since the last
    /// event; `force` ignores the wait (the job ended).
    pub fn take_event(&self, force: bool) -> bool {
        let mut s = self.lock();
        let due = s.last_event.is_none_or(|t| t.elapsed() >= EVENT_EVERY);
        if s.pending && (due || force) {
            s.pending = false;
            s.last_event = Some(Instant::now());
            true
        } else {
            false
        }
    }
}

/// Marks a queued or running item skipped; true if it was.
fn mark(r: &mut Rec) -> bool {
    match r.state {
        ItemState::Queued | ItemState::Active => {
            r.state = ItemState::Skipped;
            r.live.skip.store(true, Ordering::SeqCst);
            true
        }
        _ => false,
    }
}

/// Each item's state, with a folder's taken from what's inside it: running while anything
/// in it runs, queued while anything waits (or it hasn't been listed), failed if something
/// in it failed, else done.
fn derived(recs: &[Rec]) -> Vec<ItemState> {
    let mut out: Vec<ItemState> = recs.iter().map(|r| r.state).collect();
    let dirs: Vec<usize> = (0..recs.len()).filter(|&i| recs[i].is_dir && !recs[i].work && recs[i].state != ItemState::Skipped).collect();
    if dirs.is_empty() {
        return out;
    }
    // (active, queued, failed) below each folder.
    let mut below: HashMap<&RPath, (bool, bool, bool)> = dirs.iter().map(|&i| (&recs[i].path, (false, false, false))).collect();
    for r in recs.iter().filter(|r| !r.is_dir || r.state == ItemState::Queued) {
        let mut p = r.path.parent();
        while let Some(d) = p {
            if let Some(f) = below.get_mut(&d) {
                f.0 |= r.state == ItemState::Active;
                f.1 |= r.state == ItemState::Queued;
                f.2 |= r.state == ItemState::Failed;
            }
            p = d.parent();
        }
    }
    for i in dirs {
        let (active, queued, failed) = below[&recs[i].path];
        out[i] = if recs[i].state == ItemState::Queued || queued && !active {
            if active { ItemState::Active } else { ItemState::Queued }
        } else if active {
            ItemState::Active
        } else if failed || recs[i].state == ItemState::Failed {
            ItemState::Failed
        } else {
            ItemState::Done
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_take_their_state_from_inside_and_skips_cover_later_finds() {
        let it = Items::default();
        it.found(&RPath::new("/a"), "a".into(), true, None, false);
        it.set(&RPath::new("/a"), ItemState::Done, None); // listed
        it.found(&RPath::new("/a/x"), "a/x".into(), false, Some(10), true);
        it.found(&RPath::new("/a/y"), "a/y".into(), false, Some(10), true);
        it.found(&RPath::new("/a/b"), "a/b".into(), true, None, false);
        it.set(&RPath::new("/a/x"), ItemState::Active, None);
        assert_eq!(it.page(ItemState::Active, 0..10).1.iter().map(|v| v.rel.as_str()).collect::<Vec<_>>(), ["a", "a/x"]);
        assert_eq!(it.page(ItemState::Queued, 0..10).0, 2, "a/y and the unlisted a/b");
        // Skip a folder before its children are found: they arrive skipped.
        assert_eq!(it.skip(&RPath::new("/a/b")), Skip::Skipped);
        assert!(it.found(&RPath::new("/a/b/z"), "a/b/z".into(), false, Some(1), true).is_none());
        assert_eq!(it.counts(), ItemCounts { active: 1, queued: 1, done: 0, failed: 0, skipped: 0 }, "folders walked through aren't items");
        // Skipping a finished file does nothing; a skipped file that lands anyway is done.
        it.set(&RPath::new("/a/x"), ItemState::Done, None);
        assert_eq!(it.skip(&RPath::new("/a/x")), Skip::Nothing);
        assert_eq!(it.skip(&RPath::new("/a/y")), Skip::Skipped);
        it.set(&RPath::new("/a/y"), ItemState::Failed, None);
        assert_eq!(it.page(ItemState::Skipped, 0..10).0, 2, "a/b and a/y; a/y stays skipped, not failed");
        assert_eq!(it.counts().skipped, 1);
        // Ranges page through.
        let (total, page) = it.page(ItemState::Skipped, 1..5);
        assert_eq!((total, page.len()), (2, 1));
    }

    #[test]
    fn events_are_throttled() {
        let it = Items::default();
        it.found(&RPath::new("/x"), "x".into(), false, Some(1), true);
        assert!(it.take_event(false));
        it.set(&RPath::new("/x"), ItemState::Active, None);
        assert!(!it.take_event(false), "within a quarter second");
        assert!(it.take_event(true), "forced at the end");
        assert!(!it.take_event(true), "nothing new");
    }
}
