//! One engine download with N connections, measured: for the backends' segmented-download
//! tests and the throughput measurements.

use filewinch_core::{Caps, Entry, RPath, Remote, Result, WriteSink};
use filewinch_engine::{ConflictPolicy, Engine, EngineConfig, Event, Job, JobState, Op};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Wraps a remote and counts its ranged reads.
pub struct Counted {
    pub inner: Arc<dyn Remote>,
    open: Arc<AtomicUsize>,
    pub max_open: AtomicUsize,
    pub ranges: AtomicUsize,
}

impl Counted {
    pub fn new(inner: Arc<dyn Remote>) -> Arc<Counted> {
        Arc::new(Counted { inner, open: Arc::default(), max_open: AtomicUsize::new(0), ranges: AtomicUsize::new(0) })
    }
}

struct Tracked(Box<dyn Read + Send>, Arc<AtomicUsize>);

impl Read for Tracked {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.1.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Remote for Counted {
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
        let r = self.inner.read_range(path, offset, len)?;
        self.ranges.fetch_add(1, Ordering::SeqCst);
        let now = self.open.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_open.fetch_max(now, Ordering::SeqCst);
        Ok(Box::new(Tracked(r, self.open.clone())))
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

#[derive(Debug)]
pub struct Outcome {
    pub state: JobState,
    pub secs: f64,
    /// Most ranged reads open at once, and how many ran in all.
    pub max_connections: usize,
    pub ranges: usize,
    pub notes: Vec<String>,
    /// Seconds until the first bytes arrived, and until the totals were known.
    pub first_byte: Option<f64>,
    pub counted: Option<f64>,
}

/// Downloads `path` (a file or a folder) from `src` into the local folder `dst_dir` with up to
/// `segments` connections per file (overwriting), and reports how it went.
pub fn download(src: Arc<dyn Remote>, path: &RPath, dst_dir: &Path, segments: usize) -> Outcome {
    let counted = Counted::new(src);
    let (tx, rx) = channel();
    let tx = Mutex::new(tx);
    let cfg = EngineConfig { segments_per_file: segments, retry_delay: Duration::from_millis(200), ..EngineConfig::default() };
    let engine = Engine::new(cfg, move |e| {
        let _ = tx.lock().unwrap().send(e);
    });
    let t = Instant::now();
    let id = engine.enqueue(Job {
        op: Op::Copy,
        src: counted.clone(),
        src_paths: vec![path.clone()],
        dst: Some(Arc::new(filewinch_local::LocalRemote::new())),
        dst_dir: Some(filewinch_local::rpath(dst_dir)),
        conflict: ConflictPolicy::Overwrite,
        label: "segmented".into(),
        delete_shared_contents: false,
    });
    let (mut first_byte, mut totals_at) = (None, None);
    let state = loop {
        match rx.recv_timeout(Duration::from_secs(600)).expect("engine event") {
            Event::State(j, s) if j == id && s.is_finished() => break s,
            Event::Progress(j, p) if j == id => {
                if p.bytes_done > 0 && first_byte.is_none() {
                    first_byte = Some(t.elapsed().as_secs_f64());
                }
                if p.bytes_total.is_some() && totals_at.is_none() {
                    totals_at = Some(t.elapsed().as_secs_f64());
                }
            }
            _ => {}
        }
    };
    Outcome {
        state,
        secs: t.elapsed().as_secs_f64(),
        max_connections: counted.max_open.load(Ordering::SeqCst),
        ranges: counted.ranges.load(Ordering::SeqCst),
        notes: engine.notes(id),
        first_byte,
        counted: totals_at,
    }
}
