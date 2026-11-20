//! Recursive permission changes (`Op::SetMode`), on a real folder.
#![cfg(unix)]

mod common;

use common::*;
use filewinch_core::{Caps, Entry, Error, ErrorKind, RPath, Remote, Result, WriteSink};
use filewinch_engine::{ConflictPolicy, EngineConfig, Event, ItemState, Job, JobState, Op, applied_mode};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// A local folder that can be slow, refuse one path, or not do permissions at all.
struct Perms {
    inner: Disk,
    delay: Duration,
    refuse: Option<RPath>,
    caps: Caps,
}

impl Remote for Perms {
    fn caps(&self) -> Caps {
        self.caps
    }
    fn home(&self) -> Result<RPath> {
        self.inner.home()
    }
    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        std::thread::sleep(self.delay);
        self.inner.list(dir)
    }
    fn stat(&self, path: &RPath) -> Result<Entry> {
        self.inner.stat(path)
    }
    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        self.inner.read(path, offset)
    }
    fn write(&self, path: &RPath, offset: u64, size: Option<u64>) -> Result<Box<dyn WriteSink>> {
        self.inner.write(path, offset, size)
    }
    fn mkdir(&self, path: &RPath) -> Result<()> {
        self.inner.mkdir(path)
    }
    fn rename(&self, from: &RPath, to: &RPath) -> Result<()> {
        self.inner.rename(from, to)
    }
    fn remove_file(&self, path: &RPath) -> Result<()> {
        self.inner.remove_file(path)
    }
    fn remove_dir(&self, path: &RPath) -> Result<()> {
        self.inner.remove_dir(path)
    }
    fn set_mode(&self, path: &RPath, mode: u32) -> Result<()> {
        std::thread::sleep(self.delay);
        if self.refuse.as_ref() == Some(path) {
            return Err(Error::new(ErrorKind::PermissionDenied, "Not yours (test)."));
        }
        self.inner.set_mode(path, mode)
    }
}

/// /t/a.txt, /t/sub/b.txt, /t/sub/deep/c.txt, /t/other/d.txt
fn tree(name: &str) -> PathBuf {
    let root = scratch(name);
    for (dir, file) in [("t", "a.txt"), ("t/sub", "b.txt"), ("t/sub/deep", "c.txt"), ("t/other", "d.txt")] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
        std::fs::write(root.join(dir).join(file), b"x").unwrap();
        std::fs::set_permissions(root.join(dir).join(file), std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(root.join(dir), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    root
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
}

/// Gives everything back owner access so the scratch folder can be removed.
fn cleanup(root: &Path) {
    fn walk(p: &Path) {
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755));
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                walk(&e.path());
            }
        }
    }
    walk(root);
    let _ = std::fs::remove_dir_all(root);
}

fn perms(root: &Path, delay: Duration) -> Perms {
    Perms { inner: Disk::new(root), delay, refuse: None, caps: full_caps() }
}

fn job(src: Arc<dyn Remote>, mode: u32, mask: u32, x: bool) -> Job {
    Job {
        op: Op::SetMode { mode, mask, dirs_get_x_where_r: x },
        src,
        src_paths: vec![RPath::new("/t")],
        dst: None,
        dst_dir: None,
        conflict: ConflictPolicy::Skip,
        label: "Changing permissions of “t”".into(),
        delete_shared_contents: false,
    }
}

#[test]
fn the_mode_math_follows_chmod() {
    assert_eq!(applied_mode(0o644, false, 0o600, 0o777, false), 0o600);
    // Only the masked bits change: group and other write off, the rest kept.
    assert_eq!(applied_mode(0o777, false, 0, 0o022, false), 0o755);
    // X: folders get execute where they end up readable (within the mask); files don't.
    assert_eq!(applied_mode(0o700, true, 0o640, 0o777, true), 0o750);
    assert_eq!(applied_mode(0o700, false, 0o640, 0o777, true), 0o640);
    assert_eq!(applied_mode(0o700, true, 0o644, 0o070, true), 0o750, "other's bits are outside the mask");
}

#[test]
fn changes_every_item_with_progress() {
    let root = tree("chmod-all");
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(job(Arc::new(perms(&root, Duration::ZERO)), 0o640, 0o777, true));
    assert_done(&h.wait(id));
    for f in ["t/a.txt", "t/sub/b.txt", "t/sub/deep/c.txt", "t/other/d.txt"] {
        assert_eq!(mode(&root.join(f)), 0o640, "{f}");
    }
    for d in ["t", "t/sub", "t/sub/deep", "t/other"] {
        assert_eq!(mode(&root.join(d)), 0o750, "{d}");
    }
    let p = h.log.iter().rev().find_map(|e| match e {
        Event::Progress(j, p) if *j == id => Some(p.clone()),
        _ => None,
    }).unwrap();
    assert_eq!((p.files_done, p.files_total), (8, Some(8)), "4 files and 4 folders");
    assert_eq!(h.engine.item_counts(id).done, 8);
    cleanup(&root);
}

#[test]
fn taking_away_read_access_doesnt_stop_the_walk() {
    let root = tree("chmod-noread");
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(job(Arc::new(perms(&root, Duration::ZERO)), 0o300, 0o777, false));
    assert_done(&h.wait(id));
    assert_eq!(mode(&root.join("t/sub/deep")), 0o300, "listed before it was changed");
    cleanup(&root);
}

#[test]
fn a_refused_item_fails_alone() {
    let root = tree("chmod-refuse");
    let mut p = perms(&root, Duration::ZERO);
    p.refuse = Some(RPath::new("/t/sub/b.txt"));
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(job(Arc::new(p), 0o700, 0o777, false));
    let JobState::Failed(e) = h.wait(id) else { panic!("expected a failure") };
    assert!(e.message.contains("b.txt"), "{e}");
    assert_eq!(h.failures(id).len(), 1);
    assert_eq!(mode(&root.join("t/a.txt")), 0o700, "the rest changed");
    assert_eq!(mode(&root.join("t/sub/b.txt")), 0o644);
    cleanup(&root);
}

#[test]
fn a_server_without_permissions_says_so() {
    let root = tree("chmod-unsupported");
    let mut p = perms(&root, Duration::ZERO);
    p.caps.permissions = false;
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(job(Arc::new(p), 0o600, 0o777, false));
    let JobState::Failed(e) = h.wait(id) else { panic!("expected a failure") };
    assert_eq!(e.kind, ErrorKind::Unsupported);
    assert!(e.message.contains("doesn't support changing permissions"), "{e}");
    assert_eq!(mode(&root.join("t/a.txt")), 0o644, "nothing touched");
    cleanup(&root);
}

#[test]
fn pause_cancel_and_skipping_a_folder() {
    let root = tree("chmod-control");
    let mut h = Harness::new(EngineConfig { max_files_per_job: 1, ..cfg() });
    let id = h.engine.enqueue(job(Arc::new(perms(&root, Duration::from_millis(60))), 0o700, 0o777, false));
    // Skip the folder "other" as soon as the walk finds it, before it's listed: nothing in it
    // changes, including what wasn't found yet.
    let end = std::time::Instant::now() + Duration::from_secs(10);
    while !h.engine.items(id, ItemState::Queued, 0..50).1.iter().any(|v| v.rel == "t/other") {
        assert!(std::time::Instant::now() < end, "other not found");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(h.engine.skip_item(id, &RPath::new("/t/other")));
    h.engine.pause(id);
    h.until(Duration::from_secs(5), |e| matches!(e, Event::State(j, JobState::Paused) if *j == id)).expect("paused");
    std::thread::sleep(Duration::from_millis(150));
    let before = h.engine.item_counts(id);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(h.engine.item_counts(id).done, before.done, "nothing changes while paused");
    h.engine.resume(id);
    assert_done(&h.wait(id));
    assert_eq!(mode(&root.join("t/other/d.txt")), 0o644, "inside the skipped folder: unchanged");
    assert_eq!(mode(&root.join("t/other")), 0o755, "the skipped folder: unchanged");
    assert_eq!(mode(&root.join("t/a.txt")), 0o700);
    assert!(h.failures(id).is_empty());

    // Cancel stops it part-way.
    let root2 = tree("chmod-cancel");
    let id = h.engine.enqueue(job(Arc::new(perms(&root2, Duration::from_millis(80))), 0o600, 0o777, false));
    std::thread::sleep(Duration::from_millis(200));
    h.engine.cancel(id);
    assert!(matches!(h.wait(id), JobState::Cancelled));
    cleanup(&root);
    cleanup(&root2);
}
