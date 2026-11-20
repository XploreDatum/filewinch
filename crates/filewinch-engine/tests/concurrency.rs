//! A job's files-at-once limit changed while it runs, and transfers that start while the
//! folders are still being counted.

mod common;

use common::*;
use filewinch_core::{Caps, Entry, RPath, Remote, Result, WriteSink};
use filewinch_engine::{ConflictPolicy, EngineConfig, Event, Job, Op};
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn copy_job(src: &Arc<Mem>, dst: &Arc<Mem>, path: &str) -> Job {
    Job {
        op: Op::Copy,
        src: Arc::new(MemRemote(src.clone())),
        src_paths: vec![RPath::new(path)],
        dst: Some(Arc::new(MemRemote(dst.clone()))),
        dst_dir: Some(RPath::new("/dst")),
        conflict: ConflictPolicy::Overwrite,
        label: "copy".into(),
        delete_shared_contents: false,
    }
}

/// Samples the source's open readers every 2 ms into `peak` until `stop`.
fn sampler(src: Arc<Mem>, peak: Arc<AtomicUsize>, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            peak.fetch_max(src.open_readers.load(Ordering::SeqCst), Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(2));
        }
    })
}

#[test]
fn files_at_once_changes_live_without_cancelling() {
    let src = Mem::with_chunk(full_caps(), 16 * 1024);
    for i in 0..24 {
        src.put(&format!("/many/f{i:02}"), &bytes(128 * 1024, i as u8));
    }
    *src.read_delay.lock().unwrap() = Duration::from_millis(15); // ~9 reads per file: ~135 ms each
    let dst = Mem::new(full_caps());
    dst.mkdirs("/dst");
    let mut h = Harness::new(EngineConfig { max_files_per_job: 2, ..cfg() });
    let id = h.engine.enqueue(copy_job(&src, &dst, "/many"));
    assert_eq!(h.engine.job_files(id), Some(2));

    let (peak, stop) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(false)));
    let s = sampler(src.clone(), peak.clone(), stop.clone());
    std::thread::sleep(Duration::from_millis(400));
    assert!(peak.load(Ordering::SeqCst) <= 2, "never above the limit: {}", peak.load(Ordering::SeqCst));

    // Up: more files start right away.
    h.engine.set_job_files(id, 6);
    assert_eq!(h.engine.job_files(id), Some(6));
    peak.store(0, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(250));
    let up = peak.load(Ordering::SeqCst);
    assert!(up > 2 && up <= 6, "raised to 6: {up} at once");

    // Down: in-flight files finish, no new one starts above the limit.
    h.engine.set_job_files(id, 1);
    std::thread::sleep(Duration::from_millis(300)); // the in-flight ones (≤ 135 ms) are done
    peak.store(0, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(300));
    assert!(peak.load(Ordering::SeqCst) <= 1, "lowered to 1: {}", peak.load(Ordering::SeqCst));

    assert_done(&h.wait(id));
    stop.store(true, Ordering::SeqCst);
    s.join().unwrap();
    assert!(h.failures(id).is_empty(), "nothing was cancelled or failed");
    assert_eq!(dst.tree("/dst/many").len(), 24);
    assert_eq!(src.aborted_sinks.load(Ordering::SeqCst) + dst.aborted_sinks.load(Ordering::SeqCst), 0);
}

#[test]
fn a_queued_job_keeps_the_limit_it_was_given() {
    let src = Mem::new(full_caps());
    src.put("/a/x", b"x");
    let dst = Mem::new(full_caps());
    dst.mkdirs("/dst");
    let mut h = Harness::new(EngineConfig { max_jobs: 1, ..cfg() });
    let id = h.engine.enqueue(copy_job(&src, &dst, "/a"));
    h.engine.set_job_files(id, 99);
    assert_eq!(h.engine.job_files(id), Some(16), "clamped to the live maximum");
    h.engine.set_job_files(id, 0);
    assert_eq!(h.engine.job_files(id), Some(1));
    assert_done(&h.wait(id));
}

/// A source whose listings are slow, like a big server.
struct SlowList {
    inner: MemRemote,
    delay: Duration,
}

impl Remote for SlowList {
    fn caps(&self) -> Caps {
        self.inner.caps()
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
}

#[test]
fn transfers_start_while_folders_are_still_being_counted() {
    // 50 nested folders, each with one file; every listing takes 100 ms (5 s to count it all)
    // and every file 300 ms to read (5 s for 50 files three at a time).
    let src = Mem::new(full_caps());
    let mut dir = String::from("/tree");
    for i in 0..50 {
        dir.push_str(&format!("/d{i:02}"));
        src.put(&format!("{dir}/f.bin"), &bytes(32 * 1024, i as u8));
    }
    *src.read_delay.lock().unwrap() = Duration::from_millis(150); // two reads per file
    let dst = Mem::new(full_caps());
    dst.mkdirs("/dst");
    let slow = Arc::new(SlowList { inner: MemRemote(src.clone()), delay: Duration::from_millis(100) });
    let mut h = Harness::new(EngineConfig { max_files_per_job: 3, ..cfg() });
    let t = Instant::now();
    let id = h.engine.enqueue(Job { src: slow, ..copy_job(&src, &dst, "/tree") });

    let first = h.until(Duration::from_secs(20), |e| matches!(e, Event::Progress(j, p) if *j == id && p.bytes_done > 0)).expect("bytes flow");
    let Event::Progress(_, p) = first else { unreachable!() };
    assert!(p.bytes_total.is_none(), "the first bytes arrive while still counting");
    // When the count ends, most files have already moved: counting and moving overlapped.
    // (Ordering and a ratio, not a time budget, so a loaded machine can't fail it.)
    let counted = h.until(Duration::from_secs(60), |e| matches!(e, Event::Progress(j, p) if *j == id && p.bytes_total.is_some())).expect("counted");
    let Event::Progress(_, at_count) = counted else { unreachable!() };
    assert!(at_count.files_done >= 25, "only {} of 50 files moved while counting", at_count.files_done);
    assert_done(&h.wait(id));
    let _ = t;
    assert_eq!(dst.tree("/dst/tree").iter().filter(|(_, d)| d.is_some()).count(), 50);
}
