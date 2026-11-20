//! Segmented downloads: several ranged reads of one file into a positional destination.

mod common;

use common::*;
use filewinch_core::{Caps, Checksum, Entry, Error, ErrorKind, HashAlgo, RPath, Remote, Result, WriteSink};
use filewinch_engine::{ConflictAnswer, ConflictChoice, ConflictPolicy, Event, Job, JobState, Op};
use sha2::Digest as _;
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MB: usize = 1024 * 1024;

/// A source in front of a [`Mem`] that advertises ranged reads and can misbehave the ways real
/// servers do: limit connections, ignore ranges, drop a connection at a byte position.
struct Gate {
    inner: MemRemote,
    /// Ranged readers allowed open at once; more fail with "too many connections".
    max_conns: Option<usize>,
    open: Arc<AtomicUsize>,
    max_open: Arc<AtomicUsize>,
    /// Ranged reads allowed before the server starts ignoring ranges.
    honour_ranges: AtomicUsize,
    ignore: AtomicBool,
    /// Absolute positions where one ranged reader fails (each used once).
    faults: Mutex<Vec<u64>>,
    /// (offset, len) of every ranged read, in order.
    ranges: Mutex<Vec<(u64, u64)>>,
    /// Offsets of plain reads.
    reads: Mutex<Vec<u64>>,
}

impl Gate {
    fn new(mem: Arc<Mem>) -> Arc<Gate> {
        Arc::new(Gate {
            inner: MemRemote(mem),
            max_conns: None,
            open: Arc::default(),
            max_open: Arc::default(),
            honour_ranges: AtomicUsize::new(usize::MAX),
            ignore: AtomicBool::new(false),
            faults: Mutex::default(),
            ranges: Mutex::default(),
            reads: Mutex::default(),
        })
    }
    fn limited(mem: Arc<Mem>, conns: usize) -> Arc<Gate> {
        let mut g = Arc::try_unwrap(Gate::new(mem)).ok().unwrap();
        g.max_conns = Some(conns);
        Arc::new(g)
    }
    fn ranges(&self) -> Vec<(u64, u64)> {
        self.ranges.lock().unwrap().clone()
    }
}

struct GateReader {
    inner: Box<dyn Read + Send>,
    open: Arc<AtomicUsize>,
    pos: u64,
    fail_at: Option<u64>,
}

impl Read for GateReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut limit = buf.len();
        if let Some(f) = self.fail_at {
            if self.pos >= f {
                return Err(io::Error::other(Error::new(ErrorKind::Network, "The connection dropped (injected).")));
            }
            limit = limit.min((f - self.pos) as usize);
        }
        let n = self.inner.read(&mut buf[..limit])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Drop for GateReader {
    fn drop(&mut self) {
        self.open.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Remote for Gate {
    fn caps(&self) -> Caps {
        Caps { ranged_reads: true, ..full_caps() }
    }
    fn home(&self) -> Result<RPath> {
        self.inner.home()
    }
    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        self.inner.list(dir)
    }
    fn stat(&self, path: &RPath) -> Result<Entry> {
        self.inner.stat(path)
    }
    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        self.reads.lock().unwrap().push(offset);
        self.inner.read(path, offset)
    }
    fn read_range(&self, path: &RPath, offset: u64, len: u64) -> Result<Box<dyn Read + Send>> {
        self.ranges.lock().unwrap().push((offset, len));
        if self.ignore.load(Ordering::SeqCst)
            || self.honour_ranges.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_err()
        {
            self.ignore.store(true, Ordering::SeqCst);
            return Err(Error::range_ignored("HTTP 200 without Content-Range (test)"));
        }
        let now = self.open.fetch_add(1, Ordering::SeqCst) + 1;
        if self.max_conns.is_some_and(|m| now > m) {
            self.open.fetch_sub(1, Ordering::SeqCst);
            return Err(Error::too_many_connections("421 Too many connections (test)"));
        }
        self.max_open.fetch_max(now, Ordering::SeqCst);
        let fail_at = {
            let mut f = self.faults.lock().unwrap();
            f.iter().position(|x| *x > offset && *x < offset + len).map(|i| f.remove(i))
        };
        let inner = match self.inner.read_range(path, offset, len) {
            Ok(r) => r,
            Err(e) => {
                self.open.fetch_sub(1, Ordering::SeqCst);
                return Err(e);
            }
        };
        Ok(Box::new(GateReader { inner, open: self.open.clone(), pos: offset, fail_at }))
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

/// A destination that takes positional writes, like the local disk.
fn disk_like() -> Arc<Mem> {
    let m = Mem::new(full_caps());
    m.positional.store(true, Ordering::SeqCst);
    m.mkdirs("/dl");
    m
}

fn source(size: usize, seed: u8) -> (Arc<Mem>, Vec<u8>) {
    let m = Mem::new(full_caps());
    let data = bytes(size, seed);
    m.put("/srv/big.bin", &data);
    (m, data)
}

fn job(src: Arc<dyn Remote>, dst: &Arc<Mem>, conflict: ConflictPolicy) -> Job {
    Job {
        op: Op::Copy,
        src,
        src_paths: vec![RPath::new("/srv/big.bin")],
        dst: Some(Arc::new(MemRemote(dst.clone()))),
        dst_dir: Some(RPath::new("/dl")),
        conflict,
        label: "Downloading big.bin".into(),
        delete_shared_contents: false,
    }
}

fn seg_cfg(n: usize) -> filewinch_engine::EngineConfig {
    filewinch_engine::EngineConfig { segments_per_file: n, chunk: 64 * 1024, ..cfg() }
}

/// No temp file or sidecar left in /dl.
fn no_leftovers(dst: &Mem) {
    let left: Vec<String> = dst.tree("/dl").into_iter().map(|(p, _)| p).filter(|p| p.contains("filewinch")).collect();
    assert!(left.is_empty(), "leftovers: {left:?}");
}

fn last_progress(h: &Harness, id: u64) -> filewinch_engine::Progress {
    h.log.iter().rev().find_map(|e| match e {
        Event::Progress(j, p) if *j == id => Some(p.clone()),
        _ => None,
    }).expect("a progress event")
}

#[test]
fn assembles_byte_equal_with_1_3_and_16_segments() {
    let (src, data) = source(64 * MB, 1);
    for n in [1, 3, 16] {
        let gate = Gate::new(src.clone());
        let dst = disk_like();
        let mut h = Harness::new(seg_cfg(n));
        let id = h.engine.enqueue(job(gate.clone(), &dst, ConflictPolicy::Ask));
        assert_done(&h.wait(id));
        assert!(dst.get("/dl/big.bin").unwrap() == data, "{n} segments: bytes differ");
        no_leftovers(&dst);
        let p = last_progress(&h, id);
        assert_eq!(p.bytes_done, data.len() as u64, "{n}: progress adds up across segments");
        let ranges = gate.ranges();
        if n == 1 {
            assert!(ranges.is_empty(), "one connection streams as before");
        } else {
            let max = gate.max_open.load(Ordering::SeqCst);
            assert!(max >= 2 && max <= n, "{n} segments: {max} connections at once");
            assert!(ranges.len() >= n, "{n}: {ranges:?}");
        }
    }
}

#[test]
fn uneven_final_segment_ends_at_the_size() {
    let size = 20 * MB + 12_345;
    let (src, data) = source(size, 2);
    let gate = Gate::new(src);
    let dst = disk_like();
    let mut h = Harness::new(seg_cfg(3));
    let id = h.engine.enqueue(job(gate.clone(), &dst, ConflictPolicy::Ask));
    assert_done(&h.wait(id));
    assert!(dst.get("/dl/big.bin").unwrap() == data);
    let ends: Vec<u64> = gate.ranges().iter().map(|(a, l)| a + l).collect();
    assert_eq!(ends.iter().max(), Some(&(size as u64)));
}

#[test]
fn files_under_16_mb_stream_over_one_connection() {
    let (src, data) = source(16 * MB - 1, 3);
    let gate = Gate::new(src);
    let dst = disk_like();
    let mut h = Harness::new(seg_cfg(8));
    let id = h.engine.enqueue(job(gate.clone(), &dst, ConflictPolicy::Ask));
    assert_done(&h.wait(id));
    assert!(dst.get("/dl/big.bin").unwrap() == data);
    assert!(gate.ranges().is_empty());
    assert_eq!(*gate.reads.lock().unwrap(), vec![0]);
}

#[test]
fn destinations_without_positional_writes_and_uploads_stream() {
    let (src, data) = source(20 * MB, 4);
    let gate = Gate::new(src);
    let dst = Mem::new(full_caps()); // an SFTP-like server: no positional writes
    dst.mkdirs("/dl");
    let mut h = Harness::new(seg_cfg(4));
    let id = h.engine.enqueue(job(gate.clone(), &dst, ConflictPolicy::Ask));
    assert_done(&h.wait(id));
    assert!(dst.get("/dl/big.bin").unwrap() == data);
    assert!(gate.ranges().is_empty());
    no_leftovers(&dst);
}

#[test]
fn a_segment_that_drops_retries_from_where_it_stopped() {
    let (src, data) = source(24 * MB, 5);
    let gate = Gate::new(src);
    let at = (13 * MB + 4096) as u64; // inside the second of three pieces
    gate.faults.lock().unwrap().push(at);
    let dst = disk_like();
    let mut h = Harness::new(seg_cfg(3));
    let id = h.engine.enqueue(job(gate.clone(), &dst, ConflictPolicy::Ask));
    assert_done(&h.wait(id));
    assert!(dst.get("/dl/big.bin").unwrap() == data);
    let ranges = gate.ranges();
    assert!(ranges.iter().any(|(a, _)| *a == at), "the retry resumes at the drop: {ranges:?}");
    assert!(h.engine.notes(id).is_empty(), "a retried drop isn't a fallback");
    assert!(h.states(id).iter().any(|s| matches!(s, JobState::Retrying { .. })));
}

#[test]
fn too_many_connections_falls_back_without_redownloading() {
    let (src, data) = source(32 * MB, 6);
    *src.read_delay.lock().unwrap() = Duration::from_millis(1);
    let gate = Gate::limited(src.clone(), 1);
    let dst = disk_like();
    let mut h = Harness::new(seg_cfg(4));
    let id = h.engine.enqueue(job(gate.clone(), &dst, ConflictPolicy::Ask));
    assert_done(&h.wait(id));
    assert!(dst.get("/dl/big.bin").unwrap() == data, "no corruption");
    no_leftovers(&dst);
    let served = src.served.load(Ordering::SeqCst);
    assert!(served <= data.len() as u64 + 4 * 64 * 1024, "finished ranges aren't fetched again: {served} for {}", data.len());
    let notes = h.engine.notes(id);
    assert!(notes.len() == 1 && notes[0].contains("one connection"), "{notes:?}");

    // The server stays at one connection for the rest of the session.
    MemRemote(dst.clone()).remove_file(&RPath::new("/dl/big.bin")).unwrap();
    gate.ranges.lock().unwrap().clear();
    let id = h.engine.enqueue(job(gate.clone(), &dst, ConflictPolicy::Ask));
    assert_done(&h.wait(id));
    assert!(dst.get("/dl/big.bin").unwrap() == data);
    assert!(gate.ranges().is_empty(), "no more ranged connections: {:?}", gate.ranges());
}

#[test]
fn an_ignored_range_falls_back_and_fills_only_the_gaps() {
    let (src, data) = source(24 * MB, 7);
    *src.read_delay.lock().unwrap() = Duration::from_millis(1);
    let gate = Gate::new(src);
    gate.honour_ranges.store(1, Ordering::SeqCst); // the first connection works, then 200s
    let dst = disk_like();
    let mut h = Harness::new(seg_cfg(3));
    let id = h.engine.enqueue(job(gate.clone(), &dst, ConflictPolicy::Ask));
    assert_done(&h.wait(id));
    assert!(dst.get("/dl/big.bin").unwrap() == data, "no corruption");
    no_leftovers(&dst);
    let notes = h.engine.notes(id);
    assert!(notes.iter().any(|n| n.contains("ignored a byte range")), "{notes:?}");
    assert!(!gate.reads.lock().unwrap().is_empty(), "the rest came as a plain stream");
}

#[test]
fn cancel_deletes_the_temp_file() {
    let (src, _) = source(24 * MB, 8);
    *src.read_delay.lock().unwrap() = Duration::from_millis(5);
    let gate = Gate::new(src);
    let dst = disk_like();
    let mut h = Harness::new(seg_cfg(3));
    let id = h.engine.enqueue(job(gate.clone(), &dst, ConflictPolicy::Ask));
    h.until(Duration::from_secs(20), |e| matches!(e, Event::Progress(j, p) if *j == id && p.bytes_done > 2 * MB as u64))
        .expect("progress");
    assert!(dst.exists("/dl/.big.bin.filewinch-part"));
    h.engine.cancel(id);
    assert!(matches!(h.wait(id), JobState::Cancelled));
    wait_closed(&gate);
    no_leftovers(&dst);
    assert!(!dst.exists("/dl/big.bin"));
}

fn wait_closed(gate: &Gate) {
    let end = Instant::now() + Duration::from_secs(10);
    while gate.open.load(Ordering::SeqCst) > 0 {
        assert!(Instant::now() < end, "segment readers still open");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn pause_stops_every_segment_and_resume_continues() {
    let (src, data) = source(24 * MB, 9);
    *src.read_delay.lock().unwrap() = Duration::from_millis(3);
    let gate = Gate::new(src.clone());
    let dst = disk_like();
    let mut h = Harness::new(seg_cfg(3));
    let id = h.engine.enqueue(job(gate.clone(), &dst, ConflictPolicy::Ask));
    h.until(Duration::from_secs(20), |e| matches!(e, Event::Progress(j, p) if *j == id && p.bytes_done > 2 * MB as u64))
        .expect("progress");
    h.engine.pause(id);
    std::thread::sleep(Duration::from_millis(100));
    let before = src.served.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(src.served.load(Ordering::SeqCst), before, "nothing moves while paused");
    h.engine.resume(id);
    assert_done(&h.wait(id));
    assert!(dst.get("/dl/big.bin").unwrap() == data);
    assert!(src.served.load(Ordering::SeqCst) <= data.len() as u64 + 3 * 64 * 1024, "no restart");
}

#[test]
fn a_restarted_engine_resumes_from_the_sidecar() {
    let (src, data) = source(24 * MB, 10);
    *src.read_delay.lock().unwrap() = Duration::from_millis(3);
    let gate = Gate::new(src.clone());
    let dst = disk_like();
    {
        let mut h = Harness::new(seg_cfg(3));
        let id = h.engine.enqueue(job(gate.clone(), &dst, ConflictPolicy::Ask));
        h.until(Duration::from_secs(20), |e| matches!(e, Event::Progress(j, p) if *j == id && p.bytes_done > 6 * MB as u64))
            .expect("progress");
    } // the app quits: the engine drops and keeps partial downloads
    // The job thread holds the source until it has saved the sidecar and ended.
    let end = Instant::now() + Duration::from_secs(10);
    while Arc::strong_count(&gate) > 1 {
        assert!(Instant::now() < end, "the job thread didn't end");
        std::thread::sleep(Duration::from_millis(10));
    }
    let sidecar = String::from_utf8(dst.get("/dl/.big.bin.filewinch-ranges").expect("a sidecar")).unwrap();
    let saved: u64 = sidecar.lines().filter_map(|l| l.strip_prefix("done ")).map(|l| {
        let (a, b) = l.split_once(' ').unwrap();
        b.parse::<u64>().unwrap() - a.parse::<u64>().unwrap()
    }).sum();
    assert!(saved >= 6 * MB as u64 - 3 * 64 * 1024, "sidecar records the finished ranges: {sidecar}");

    *src.read_delay.lock().unwrap() = Duration::ZERO;
    src.served.store(0, Ordering::SeqCst);
    let mut h = Harness::new(seg_cfg(3));
    let id = h.engine.enqueue(job(gate.clone(), &dst, ConflictPolicy::Ask));
    assert_done(&h.wait(id));
    assert!(dst.get("/dl/big.bin").unwrap() == data);
    no_leftovers(&dst);
    let served = src.served.load(Ordering::SeqCst);
    assert!(served <= data.len() as u64 - saved, "only the missing ranges: {served} after {saved} saved");
}

#[test]
fn a_checksum_mismatch_fails_and_discards() {
    let (src, data) = source(17 * MB, 11);
    let wrong = Checksum::from_bytes(HashAlgo::Sha256, &sha2::Sha256::digest(b"something else"));
    src.checksums.lock().unwrap().insert(RPath::new("/srv/big.bin"), vec![wrong]);
    let gate = Gate::new(src.clone());
    let dst = disk_like();
    let mut h = Harness::new(seg_cfg(3));
    let id = h.engine.enqueue(job(gate.clone(), &dst, ConflictPolicy::Ask));
    let JobState::Failed(e) = h.wait(id) else { panic!("expected a failure") };
    assert!(e.message.contains("damaged"), "{e}");
    assert!(!dst.exists("/dl/big.bin"));
    no_leftovers(&dst);

    // The right checksum passes.
    let right = Checksum::from_bytes(HashAlgo::Sha256, &sha2::Sha256::digest(&data));
    src.checksums.lock().unwrap().insert(RPath::new("/srv/big.bin"), vec![right]);
    let id = h.engine.enqueue(job(gate, &dst, ConflictPolicy::Ask));
    assert_done(&h.wait(id));
    assert!(dst.get("/dl/big.bin").unwrap() == data);
}

#[test]
fn the_bandwidth_limit_covers_every_segment() {
    let (src, data) = source(16 * MB, 12);
    let gate = Gate::new(src);
    let dst = disk_like();
    let limit = 8 * MB as u64;
    let mut h = Harness::new(filewinch_engine::EngineConfig { bandwidth_limit: Some(limit), ..seg_cfg(4) });
    let t = Instant::now();
    let id = h.engine.enqueue(job(gate.clone(), &dst, ConflictPolicy::Ask));
    assert_done(&h.wait(id));
    let secs = t.elapsed().as_secs_f64();
    let ideal = data.len() as f64 / limit as f64; // 2 s
    assert!(secs > ideal * 0.8 && secs < ideal * 3.0, "{secs:.2} s for an ideal {ideal:.2} s");
    assert!(gate.max_open.load(Ordering::SeqCst) >= 2);
}

#[test]
fn conflict_policies_apply_at_the_rename() {
    let (src, data) = source(17 * MB, 13);
    // Keep both: lands as "big 2.bin".
    let dst = disk_like();
    dst.put("/dl/big.bin", b"old");
    let mut h = Harness::new(seg_cfg(3));
    let id = h.engine.enqueue(job(Gate::new(src.clone()), &dst, ConflictPolicy::Rename));
    assert_done(&h.wait(id));
    assert_eq!(dst.get("/dl/big.bin").unwrap(), b"old");
    assert!(dst.get("/dl/big 2.bin").unwrap() == data);
    no_leftovers(&dst);

    // Overwrite: replaced, nothing left aside.
    let id = h.engine.enqueue(job(Gate::new(src.clone()), &dst, ConflictPolicy::Overwrite));
    assert_done(&h.wait(id));
    assert!(dst.get("/dl/big.bin").unwrap() == data);
    no_leftovers(&dst);

    // Something appears at the target while downloading: Skip keeps it.
    *src.read_delay.lock().unwrap() = Duration::from_millis(2);
    let dst = disk_like();
    let id = h.engine.enqueue(job(Gate::new(src.clone()), &dst, ConflictPolicy::Skip));
    h.until(Duration::from_secs(20), |e| matches!(e, Event::Progress(j, p) if *j == id && p.bytes_done > MB as u64)).expect("progress");
    dst.put("/dl/big.bin", b"theirs");
    assert_done(&h.wait(id));
    assert_eq!(dst.get("/dl/big.bin").unwrap(), b"theirs");
    no_leftovers(&dst);

    // ...and Ask asks then, before replacing it.
    let dst = disk_like();
    let id = h.engine.enqueue(job(Gate::new(src.clone()), &dst, ConflictPolicy::Ask));
    h.until(Duration::from_secs(20), |e| matches!(e, Event::Progress(j, p) if *j == id && p.bytes_done > MB as u64)).expect("progress");
    dst.put("/dl/big.bin", b"theirs");
    h.until(Duration::from_secs(30), |e| matches!(e, Event::Conflict { job, .. } if *job == id)).expect("asked at the rename");
    h.engine.answer(id, ConflictAnswer { choice: ConflictChoice::Overwrite, apply_to_all: false });
    assert_done(&h.wait(id));
    assert!(dst.get("/dl/big.bin").unwrap() == data);
    no_leftovers(&dst);
}

#[test]
fn forgetting_a_failed_job_deletes_its_partial_download() {
    let (src, _) = source(24 * MB, 14);
    let gate = Gate::new(src.clone());
    // A hard error mid-way (not a refusal): the item fails, the partial stays for a retry.
    let dst = disk_like();
    let failing = Arc::new(HardFail { inner: gate, after: 20 * MB as u64 });
    let mut h = Harness::new(seg_cfg(3));
    let id = h.engine.enqueue(job(failing, &dst, ConflictPolicy::Ask));
    assert!(matches!(h.wait(id), JobState::Failed(_)));
    assert!(dst.exists("/dl/.big.bin.filewinch-part"), "kept for a retry");
    assert!(h.engine.forget(id));
    let end = Instant::now() + Duration::from_secs(5);
    while dst.exists("/dl/.big.bin.filewinch-part") {
        assert!(Instant::now() < end, "temp file not removed");
        std::thread::sleep(Duration::from_millis(10));
    }
    no_leftovers(&dst);
}

/// Ranged reads past `after` fail with a hard error.
struct HardFail {
    inner: Arc<Gate>,
    after: u64,
}

impl Remote for HardFail {
    fn caps(&self) -> Caps {
        self.inner.caps()
    }
    fn home(&self) -> Result<RPath> {
        self.inner.home()
    }
    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        self.inner.list(dir)
    }
    fn stat(&self, path: &RPath) -> Result<Entry> {
        self.inner.stat(path)
    }
    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        self.inner.read(path, offset)
    }
    fn read_range(&self, path: &RPath, offset: u64, len: u64) -> Result<Box<dyn Read + Send>> {
        if offset >= self.after {
            return Err(Error::new(ErrorKind::NotFound, "It's gone (test)."));
        }
        self.inner.read_range(path, offset, len.min(self.after - offset))
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
fn skipping_a_segmented_download_deletes_its_temp_files() {
    let (src, _) = source(24 * MB, 15);
    *src.read_delay.lock().unwrap() = Duration::from_millis(3);
    let gate = Gate::new(src);
    let dst = disk_like();
    let mut h = Harness::new(seg_cfg(3));
    let id = h.engine.enqueue(job(gate.clone(), &dst, ConflictPolicy::Ask));
    h.until(Duration::from_secs(20), |e| matches!(e, Event::Progress(j, p) if *j == id && p.bytes_done > 2 * MB as u64))
        .expect("progress");
    assert!(dst.exists("/dl/.big.bin.filewinch-part"));
    assert!(h.engine.skip_item(id, &RPath::new("/srv/big.bin")));
    assert_done(&h.wait(id));
    wait_closed(&gate);
    no_leftovers(&dst);
    assert!(!dst.exists("/dl/big.bin"));
    assert_eq!(h.engine.item_counts(id).skipped, 1);
}
