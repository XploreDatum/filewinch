//! A job's item list and skipping single items without stopping the job.

mod common;

use common::*;
use filewinch_core::{Caps, Entry, RPath, Remote, Result, WriteSink};
use filewinch_engine::{ConflictPolicy, EngineConfig, Event, ItemCounts, ItemState, Job, JobState, Op};
use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn copy(src: Arc<dyn Remote>, dst: &Arc<Mem>, path: &str) -> Job {
    Job {
        op: Op::Copy,
        src,
        src_paths: vec![RPath::new(path)],
        dst: Some(Arc::new(MemRemote(dst.clone()))),
        dst_dir: Some(RPath::new("/dst")),
        conflict: ConflictPolicy::Overwrite,
        label: "copy".into(),
        delete_shared_contents: false,
    }
}

fn dst() -> Arc<Mem> {
    let d = Mem::new(full_caps());
    d.mkdirs("/dst");
    d
}

/// Polls until `f` holds (or panics after 10 s).
fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn active(h: &Harness, id: u64) -> Vec<String> {
    h.engine.items(id, ItemState::Active, 0..100).1.into_iter().filter(|v| !v.is_dir).map(|v| v.rel).collect()
}

#[test]
fn skipping_a_running_file_removes_its_partial_and_the_next_one_starts() {
    let src = Mem::with_chunk(full_caps(), 16 * 1024);
    for n in ["a", "b", "c"] {
        src.put(&format!("/dir/{n}"), &bytes(256 * 1024, 1));
    }
    *src.read_delay.lock().unwrap() = Duration::from_millis(20);
    let d = dst();
    let mut h = Harness::new(EngineConfig { max_files_per_job: 1, ..cfg() });
    let id = h.engine.enqueue(copy(Arc::new(MemRemote(src.clone())), &d, "/dir"));
    eventually("a running", || active(&h, id) == ["dir/a"] && d.get("/dst/dir/a").is_some_and(|b| !b.is_empty()));
    assert!(h.engine.skip_item(id, &RPath::new("/dir/a")));
    eventually("b to start", || active(&h, id) == ["dir/b"]);
    assert_done(&h.wait(id));
    assert!(!d.exists("/dst/dir/a"), "the skipped file's partial is gone");
    assert!(d.get("/dst/dir/b").is_some() && d.get("/dst/dir/c").is_some());
    assert_eq!(h.engine.item_counts(id), ItemCounts { done: 3 - 1, skipped: 1, ..ItemCounts::default() });
    assert!(h.failures(id).is_empty());
    let skipped = h.engine.items(id, ItemState::Skipped, 0..10).1;
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].rel, "dir/a");
    assert!(!h.engine.skip_item(id, &RPath::new("/dir/b")), "a finished file can't be skipped");
}

#[test]
fn skipping_a_queued_file_and_the_items_pages() {
    let src = Mem::with_chunk(full_caps(), 16 * 1024);
    for i in 0..30 {
        src.put(&format!("/q/f{i:02}"), &bytes(64 * 1024, i as u8));
    }
    *src.read_delay.lock().unwrap() = Duration::from_millis(10);
    let d = dst();
    let mut h = Harness::new(EngineConfig { max_files_per_job: 2, ..cfg() });
    let id = h.engine.enqueue(copy(Arc::new(MemRemote(src.clone())), &d, "/q"));
    eventually("files queued", || h.engine.items(id, ItemState::Queued, 0..0).0 >= 20);
    let (total, page) = h.engine.items(id, ItemState::Queued, 5..10);
    assert!(total >= 20 && page.len() == 5, "a page of five out of {total}");
    assert!(page.iter().all(|v| v.state == ItemState::Queued && v.size == Some(64 * 1024)));
    assert!(h.engine.skip_item(id, &RPath::new("/q/f29")));
    assert_done(&h.wait(id));
    assert!(!d.exists("/dst/q/f29"));
    assert_eq!(h.engine.item_counts(id), ItemCounts { done: 29, skipped: 1, ..ItemCounts::default() });
    let done = h.engine.items(id, ItemState::Done, 0..100);
    assert_eq!(done.0, 30, "29 files and the folder");
    assert!(done.1.iter().any(|v| v.is_dir && v.rel == "q"));
}

/// A source whose listings take a while.
struct SlowList(MemRemote, Duration);

impl Remote for SlowList {
    fn caps(&self) -> Caps {
        self.0.caps()
    }
    fn home(&self) -> Result<RPath> {
        self.0.home()
    }
    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        std::thread::sleep(self.1);
        self.0.list(dir)
    }
    fn stat(&self, path: &RPath) -> Result<Entry> {
        self.0.stat(path)
    }
    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        self.0.read(path, offset)
    }
    fn write(&self, path: &RPath, offset: u64, size: Option<u64>) -> Result<Box<dyn WriteSink>> {
        self.0.write(path, offset, size)
    }
    fn mkdir(&self, path: &RPath) -> Result<()> {
        self.0.mkdir(path)
    }
    fn rename(&self, from: &RPath, to: &RPath) -> Result<()> {
        self.0.rename(from, to)
    }
    fn remove_file(&self, path: &RPath) -> Result<()> {
        self.0.remove_file(path)
    }
    fn remove_dir(&self, path: &RPath) -> Result<()> {
        self.0.remove_dir(path)
    }
}

#[test]
fn skipping_a_folder_before_and_after_its_files_are_found() {
    let src = Mem::with_chunk(full_caps(), 16 * 1024);
    for i in 0..4 {
        src.put(&format!("/t/early/f{i}"), &bytes(64 * 1024, i));
        src.put(&format!("/t/late/deep/f{i}"), &bytes(64 * 1024, i));
        src.put(&format!("/t/keep/f{i}"), &bytes(1024, i));
    }
    *src.read_delay.lock().unwrap() = Duration::from_millis(15);
    let d = dst();
    let slow = Arc::new(SlowList(MemRemote(src.clone()), Duration::from_millis(150)));
    let mut h = Harness::new(EngineConfig { max_files_per_job: 1, ..cfg() });
    let id = h.engine.enqueue(copy(slow, &d, "/t"));
    // "late" is found (queued, not listed yet): skip it with nothing inside known.
    eventually("late found", || h.engine.items(id, ItemState::Queued, 0..50).1.iter().any(|v| v.rel == "t/late"));
    assert!(h.engine.skip_item(id, &RPath::new("/t/late")));
    // "early"'s files are found: skip the folder with them queued and one maybe running.
    eventually("early's files", || h.engine.items(id, ItemState::Queued, 0..50).1.iter().any(|v| v.rel.starts_with("t/early/")));
    assert!(h.engine.skip_item(id, &RPath::new("/t/early")));
    assert_done(&h.wait(id));
    assert!(d.tree("/dst/t/late").iter().all(|(_, b)| b.is_none()), "nothing under late copied: {:?}", d.tree("/dst/t/late"));
    assert!(d.tree("/dst/t/early").iter().all(|(_, b)| b.is_none()), "nothing under early copied");
    assert_eq!(d.tree("/dst/t/keep").len(), 4);
    let c = h.engine.item_counts(id);
    assert_eq!((c.done, c.failed, c.active, c.queued), (4, 0, 0, 0), "{c:?}");
    assert!(h.failures(id).is_empty());
}

#[test]
fn skips_racing_completion_end_one_way_or_the_other() {
    let src = Mem::new(full_caps());
    for i in 0..40 {
        src.put(&format!("/r/f{i:02}"), &bytes(4096, i as u8));
    }
    let d = dst();
    let mut h = Harness::new(EngineConfig { max_files_per_job: 4, ..cfg() });
    let id = h.engine.enqueue(copy(Arc::new(MemRemote(src.clone())), &d, "/r"));
    for i in 0..40 {
        h.engine.skip_item(id, &RPath::new(&format!("/r/f{i:02}")));
    }
    assert_done(&h.wait(id));
    let c = h.engine.item_counts(id);
    assert_eq!(c.done + c.skipped, 40, "{c:?}");
    for i in 0..40 {
        let p = RPath::new(&format!("/r/f{i:02}"));
        let done = h.engine.items(id, ItemState::Done, 0..100).1.iter().any(|v| v.path == p);
        assert_eq!(done, d.get(&format!("/dst/r/f{i:02}")).is_some(), "f{i:02}: done means landed, skipped means absent");
    }
}

#[test]
fn item_events_are_throttled_and_end_with_one() {
    let src = Mem::new(full_caps());
    for i in 0..300 {
        src.put(&format!("/e/f{i:03}"), &bytes(2048, i as u8));
    }
    *src.read_delay.lock().unwrap() = Duration::from_millis(2);
    let d = dst();
    let mut h = Harness::new(EngineConfig { max_files_per_job: 4, ..cfg() });
    let t = Instant::now();
    let id = h.engine.enqueue(copy(Arc::new(MemRemote(src.clone())), &d, "/e"));
    let end = h.wait(id);
    assert!(matches!(end, JobState::Done));
    let secs = t.elapsed().as_secs_f64();
    let n = h.log.iter().filter(|e| matches!(e, Event::ItemsChanged(j) if *j == id)).count();
    assert!(n >= 1, "at least the final one");
    assert!((n as f64) <= secs * 4.0 + 2.0, "{n} events in {secs:.2} s");
}
