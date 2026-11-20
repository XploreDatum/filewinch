//! Folder sync: [`compare`] two trees into a reviewable [`SyncPlan`], then [`apply`] it as jobs.

use crate::hash::{cost, hash_file};
use crate::{ConflictPolicy, Job, Op};
use filewinch_core::{Entry, EntryKind, Error, ErrorKind, HashAlgo, RPath, Remote, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncDirection {
    /// Make B match A.
    AToB,
    /// Make A match B.
    BToA,
    /// Both ways: each side gets what it lacks, and where a file differs the newer copy wins.
    /// Orphans are never deleted in this mode (without history, a file missing on one side
    /// can't be told apart from a file newly added on the other).
    Mirror,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareBy {
    /// Files differ when sizes differ or modification times differ by more than `tolerance`.
    /// Use 2 s for FAT/SMB-like servers and 60 s for FTP servers that list minutes only.
    SizeAndMtime { tolerance: Duration },
    SizeOnly,
    /// Files differ when sizes differ or their contents hash differently (times are ignored,
    /// except to pick the newer copy in Mirror). Equal-sized files compare the checksums the
    /// servers listed when both offer the same algorithm; otherwise compare reads the side
    /// without one and hashes it with an algorithm the other side offers; with no server
    /// hash on either side it reads and hashes both (SHA-256). Slower: it reads every
    /// equal-sized file a server can't hash.
    Checksum,
}

#[derive(Clone, Debug)]
pub struct SyncOptions {
    pub direction: SyncDirection,
    pub compare: CompareBy,
    /// Delete items on the target side that the source side doesn't have (not in Mirror).
    pub delete_orphans: bool,
    /// Glob patterns ("*.tmp", ".git", "build/**") matched against each item's name and its
    /// path relative to the synced folder. Matching items are ignored on both sides.
    pub exclude: Vec<String>,
    /// Follow symlinks (into folders too, up to 32 levels deep); otherwise they are ignored.
    pub follow_symlinks: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        SyncOptions {
            direction: SyncDirection::AToB,
            compare: CompareBy::SizeAndMtime { tolerance: Duration::from_secs(2) },
            delete_orphans: false,
            exclude: Vec::new(),
            follow_symlinks: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SyncActionKind {
    CopyAToB,
    CopyBToA,
    DeleteInA,
    DeleteInB,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SyncAction {
    pub kind: SyncActionKind,
    /// Relative to the synced folders, e.g. "css/site.css". A folder action covers its contents.
    pub rel: String,
    pub is_dir: bool,
    /// Bytes to copy (files only; folders are counted when the job scans them).
    pub size: Option<u64>,
}

/// An item the plan leaves alone, with the reason ("A file on one side, a folder on the other").
#[derive(Clone, Debug, PartialEq)]
pub struct SyncConflict {
    pub rel: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SyncPlan {
    pub a_dir: RPath,
    pub b_dir: RPath,
    pub actions: Vec<SyncAction>,
    pub conflicts: Vec<SyncConflict>,
}

impl SyncPlan {
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// Known bytes to copy (folder actions aren't sized until their job scans them).
    pub fn bytes_to_copy(&self) -> u64 {
        self.actions.iter().filter(|a| matches!(a.kind, SyncActionKind::CopyAToB | SyncActionKind::CopyBToA)).filter_map(|a| a.size).sum()
    }
}

/// Stops a running [`compare_with`] from another thread. Clones share one flag.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> CancelToken {
        CancelToken::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    fn check(&self) -> Result<()> {
        if self.is_cancelled() { Err(Error::cancelled()) } else { Ok(()) }
    }
}

/// How far a [`compare_with`] has got. Both counters only grow.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompareProgress {
    /// Items looked at on either side (files and folders, by relative path).
    pub entries: u64,
    /// Bytes read to compute checksums ([`CompareBy::Checksum`] only).
    pub bytes_hashed: u64,
    /// The relative path being compared.
    pub current: Option<String>,
}

/// The cancel token, the progress so far and its throttled reporter, threaded through compare.
struct Run<'a> {
    cancel: &'a CancelToken,
    on_progress: &'a mut dyn FnMut(&CompareProgress),
    now: CompareProgress,
    last: Option<Instant>,
}

impl Run<'_> {
    /// Reports at most about ten times a second; `force` always reports.
    fn report(&mut self, force: bool) {
        if force || self.last.is_none_or(|t| t.elapsed() >= Duration::from_millis(100)) {
            self.last = Some(Instant::now());
            (self.on_progress)(&self.now);
        }
    }

    /// Called per chunk while hashing: counts the bytes and stops when cancelled.
    fn hashed(&mut self, n: u64) -> Result<()> {
        self.cancel.check()?;
        self.now.bytes_hashed += n;
        self.report(false);
        Ok(())
    }
}

fn excludes(patterns: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        let g = Glob::new(p).map_err(|e| {
            Error::new(ErrorKind::Other, format!("“{p}” isn't a valid exclude pattern.")).with_detail(e)
        })?;
        b.add(g);
    }
    b.build().map_err(|e| Error::new(ErrorKind::Other, "The exclude patterns are invalid.").with_detail(e))
}

/// Lists a folder into name → entry, resolving symlinks when asked and dropping excluded items.
fn listing(r: &dyn Remote, dir: &RPath, rel: &str, opts: &SyncOptions, ex: &GlobSet) -> Result<BTreeMap<String, Entry>> {
    let mut out = BTreeMap::new();
    for e in r.list(dir)? {
        let rel_path = join_rel(rel, &e.name);
        if ex.is_match(&e.name) || ex.is_match(&rel_path) {
            continue;
        }
        let e = match e.kind {
            EntryKind::Symlink if opts.follow_symlinks => match crate::walk::follow_link(r, &e) {
                Some(t) => t,
                None => continue,
            },
            EntryKind::Symlink | EntryKind::Other => continue,
            _ => e,
        };
        out.insert(e.name.clone(), e);
    }
    Ok(out)
}

fn join_rel(rel: &str, name: &str) -> String {
    if rel.is_empty() { name.to_string() } else { format!("{rel}/{name}") }
}

/// Walks both trees (iteratively) and plans what to copy and delete. Read-only. See
/// [`compare_with`] to cancel it or follow its progress.
pub fn compare(a: &dyn Remote, a_dir: &RPath, b: &dyn Remote, b_dir: &RPath, opts: &SyncOptions) -> Result<SyncPlan> {
    compare_with(a, a_dir, b, b_dir, opts, &CancelToken::new(), &mut |_| {})
}

/// [`compare`] that stops with [`ErrorKind::Cancelled`] soon after `cancel` fires (it checks
/// between items and between hash chunks; a listing already sent to a server finishes first)
/// and reports [`CompareProgress`] to `on_progress` on this thread, about ten times a second
/// and once more at the end.
pub fn compare_with(
    a: &dyn Remote,
    a_dir: &RPath,
    b: &dyn Remote,
    b_dir: &RPath,
    opts: &SyncOptions,
    cancel: &CancelToken,
    on_progress: &mut dyn FnMut(&CompareProgress),
) -> Result<SyncPlan> {
    let mut run = Run { cancel, on_progress, now: CompareProgress::default(), last: None };
    let ex = excludes(&opts.exclude)?;
    let mut plan = SyncPlan { a_dir: a_dir.clone(), b_dir: b_dir.clone(), actions: Vec::new(), conflicts: Vec::new() };
    let mut stack = vec![(String::new(), a_dir.clone(), b_dir.clone(), 0u32)];
    while let Some((rel, ad, bd, depth)) = stack.pop() {
        cancel.check()?;
        let la = listing(a, &ad, &rel, opts, &ex)?;
        cancel.check()?;
        let lb = listing(b, &bd, &rel, opts, &ex)?;
        let names: BTreeSet<&String> = la.keys().chain(lb.keys()).collect();
        for name in names {
            cancel.check()?;
            let r = join_rel(&rel, name);
            run.now.entries += 1;
            run.now.current = Some(r.clone());
            run.report(false);
            match (la.get(name), lb.get(name)) {
                (Some(ea), None) => only_on_one_side(&mut plan, opts, r, ea, true),
                (None, Some(eb)) => only_on_one_side(&mut plan, opts, r, eb, false),
                (Some(ea), Some(eb)) => match (ea.is_dir(), eb.is_dir()) {
                    (true, true) if depth < 32 => stack.push((r, ea.path.clone(), eb.path.clone(), depth + 1)),
                    (true, true) => plan.conflicts.push(SyncConflict { rel: r, reason: "Folders nest too deeply (possible symlink loop).".into() }),
                    (false, false) => both_files(&mut plan, opts, r, ea, eb, (a, b), &mut run)?,
                    _ => plan.conflicts.push(SyncConflict { rel: r, reason: "A file on one side and a folder on the other.".into() }),
                },
                (None, None) => unreachable!("name came from one of the listings"),
            }
        }
    }
    plan.actions.sort_by(|x, y| (x.kind, &x.rel).cmp(&(y.kind, &y.rel)));
    run.now.current = None;
    run.report(true);
    Ok(plan)
}

fn only_on_one_side(plan: &mut SyncPlan, opts: &SyncOptions, rel: String, e: &Entry, on_a: bool) {
    use SyncActionKind::*;
    let kind = match (opts.direction, on_a) {
        (SyncDirection::AToB, true) | (SyncDirection::Mirror, true) => CopyAToB,
        (SyncDirection::BToA, false) | (SyncDirection::Mirror, false) => CopyBToA,
        (SyncDirection::AToB, false) if opts.delete_orphans => DeleteInB,
        (SyncDirection::BToA, true) if opts.delete_orphans => DeleteInA,
        _ => return,
    };
    plan.actions.push(SyncAction { kind, rel, is_dir: e.is_dir(), size: if e.is_dir() { None } else { e.size } });
}

fn both_files(
    plan: &mut SyncPlan,
    opts: &SyncOptions,
    rel: String,
    ea: &Entry,
    eb: &Entry,
    (a, b): (&dyn Remote, &dyn Remote),
    run: &mut Run<'_>,
) -> Result<()> {
    use SyncActionKind::*;
    let size_differs = ea.size != eb.size;
    // Signed difference a - b in seconds; None when either side lacks a time.
    let mdiff = match (ea.modified, eb.modified) {
        (Some(x), Some(y)) => Some(match x.duration_since(y) {
            Ok(d) => d.as_secs_f64(),
            Err(e) => -e.duration().as_secs_f64(),
        }),
        _ => None,
    };
    let differs = match opts.compare {
        CompareBy::SizeOnly => size_differs,
        CompareBy::SizeAndMtime { tolerance } => {
            size_differs || mdiff.is_some_and(|d| d.abs() > tolerance.as_secs_f64())
        }
        CompareBy::Checksum => size_differs || !same_content(a, ea, b, eb, run)?,
    };
    if !differs {
        return Ok(());
    }
    let kind = match opts.direction {
        SyncDirection::AToB => CopyAToB,
        SyncDirection::BToA => CopyBToA,
        SyncDirection::Mirror => {
            let tol = match opts.compare {
                CompareBy::SizeAndMtime { tolerance } => tolerance.as_secs_f64(),
                CompareBy::SizeOnly | CompareBy::Checksum => 2.0,
            };
            match mdiff {
                Some(d) if d > tol => CopyAToB,
                Some(d) if d < -tol => CopyBToA,
                _ => {
                    let reason = "The file differs, but neither copy is clearly newer.".into();
                    plan.conflicts.push(SyncConflict { rel, reason });
                    return Ok(());
                }
            }
        }
    };
    let size = if kind == CopyAToB { ea.size } else { eb.size };
    plan.actions.push(SyncAction { kind, rel, is_dir: false, size });
    Ok(())
}

/// Whether two equal-sized files hold the same bytes, by checksum (see [`CompareBy::Checksum`]).
/// A read error is returned, never taken as "equal".
fn same_content(a: &dyn Remote, ea: &Entry, b: &dyn Remote, eb: &Entry, run: &mut Run<'_>) -> Result<bool> {
    let mut chunk = |n: u64| run.hashed(n);
    let listed = |e: &Entry, algo: HashAlgo| e.checksums.iter().find(|c| c.algo == algo).map(|c| c.hex.clone());
    // Both servers listed the same algorithm: no reads at all.
    let shared = ea.checksums.iter().filter(|c| listed(eb, c.algo).is_some()).min_by_key(|c| cost(c.algo));
    if let Some(c) = shared {
        return Ok(listed(eb, c.algo).as_deref() == Some(c.hex.as_str()));
    }
    // One side listed a hash: read only the other side, with the cheapest algorithm offered.
    // `true` = hash A's bytes against B's listed value.
    let candidates = eb.checksums.iter().map(|c| (c, true)).chain(ea.checksums.iter().map(|c| (c, false)));
    if let Some((c, hash_a)) = candidates.min_by_key(|(c, _)| cost(c.algo)) {
        let (r, e) = if hash_a { (a, ea) } else { (b, eb) };
        return Ok(hash_file(r, &e.path, c.algo, &mut chunk)?.hex == c.hex);
    }
    // Neither server hashes: read and hash both.
    Ok(hash_file(a, &ea.path, HashAlgo::Sha256, &mut chunk)? == hash_file(b, &eb.path, HashAlgo::Sha256, &mut chunk)?)
}

/// Turns a plan into jobs: copies grouped by destination folder (overwriting), deletes per
/// side. Enqueue them in order; copies come before deletes.
pub fn apply(plan: &SyncPlan, a: Arc<dyn Remote>, b: Arc<dyn Remote>) -> Vec<Job> {
    use SyncActionKind::*;
    let mut groups: BTreeMap<(SyncActionKind, String), Vec<RPath>> = BTreeMap::new();
    for act in &plan.actions {
        let src_dir = match act.kind {
            CopyAToB | DeleteInA => &plan.a_dir,
            CopyBToA | DeleteInB => &plan.b_dir,
        };
        let rel_parent = act.rel.rfind('/').map_or("", |i| &act.rel[..i]).to_string();
        let key_dir = if matches!(act.kind, DeleteInA | DeleteInB) { String::new() } else { rel_parent };
        groups.entry((act.kind, key_dir)).or_default().push(src_dir.join(&act.rel));
    }
    let mut jobs = Vec::new();
    for ((kind, rel_parent), paths) in groups {
        let n = paths.len();
        let items = if n == 1 { format!("“{}”", paths[0].name()) } else { format!("{n} items") };
        let (src, dst, dst_dir, op, label) = match kind {
            CopyAToB => (a.clone(), Some(b.clone()), Some(plan.b_dir.join(&rel_parent)), Op::Copy, format!("Syncing {items} to {}", plan.b_dir.join(&rel_parent))),
            CopyBToA => (b.clone(), Some(a.clone()), Some(plan.a_dir.join(&rel_parent)), Op::Copy, format!("Syncing {items} to {}", plan.a_dir.join(&rel_parent))),
            DeleteInA => (a.clone(), None, None, Op::Delete, format!("Deleting {items} from {}", plan.a_dir)),
            DeleteInB => (b.clone(), None, None, Op::Delete, format!("Deleting {items} from {}", plan.b_dir)),
        };
        jobs.push(Job { op, src, src_paths: paths, dst, dst_dir, conflict: ConflictPolicy::Overwrite, label, delete_shared_contents: false });
    }
    jobs.sort_by_key(|j| j.op == Op::Delete);
    jobs
}
