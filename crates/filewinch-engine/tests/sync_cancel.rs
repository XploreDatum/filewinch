//! compare_with(): cancelling (between items and mid-hash) and progress reporting.

mod common;

use common::*;
use filewinch_core::{Caps, Entry, EntryKind, Error, ErrorKind, RPath, Remote, Result, WriteSink};
use filewinch_engine::*;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A folder holding one enormous file whose bytes trickle out: hashing it never finishes on
/// its own, so only cancellation can end the compare.
struct Endless {
    served: Arc<AtomicU64>,
}

struct Trickle(Arc<AtomicU64>);

impl Read for Trickle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        std::thread::sleep(Duration::from_millis(1));
        let n = buf.len().min(64 * 1024);
        buf[..n].fill(0);
        self.0.fetch_add(n as u64, Ordering::SeqCst);
        Ok(n)
    }
}

impl Remote for Endless {
    fn caps(&self) -> Caps {
        full_caps()
    }
    fn home(&self) -> Result<RPath> {
        Ok(RPath::root())
    }
    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        let path = dir.join("big.bin");
        Ok(vec![Entry {
            name: "big.bin".into(),
            path,
            kind: EntryKind::File,
            size: Some(1 << 40),
            modified: None,
            mode: None,
            owner: None,
            group: None,
            link_target: None,
            extra: Vec::new(),
            checksums: Vec::new(),
            shared: None,
        }])
    }
    fn stat(&self, _: &RPath) -> Result<Entry> {
        Err(Error::unsupported("stat"))
    }
    fn read(&self, _: &RPath, _: u64) -> Result<Box<dyn Read + Send>> {
        Ok(Box::new(Trickle(self.served.clone())))
    }
    fn write(&self, _: &RPath, _: u64, _: Option<u64>) -> Result<Box<dyn WriteSink>> {
        Err(Error::unsupported("write"))
    }
    fn mkdir(&self, _: &RPath) -> Result<()> {
        Err(Error::unsupported("mkdir"))
    }
    fn rename(&self, _: &RPath, _: &RPath) -> Result<()> {
        Err(Error::unsupported("rename"))
    }
    fn remove_file(&self, _: &RPath) -> Result<()> {
        Err(Error::unsupported("remove"))
    }
    fn remove_dir(&self, _: &RPath) -> Result<()> {
        Err(Error::unsupported("remove"))
    }
}

fn checksum() -> SyncOptions {
    SyncOptions { compare: CompareBy::Checksum, ..SyncOptions::default() }
}

#[test]
fn cancel_mid_hash_returns_cancelled_quickly() {
    let a = Endless { served: Arc::default() };
    let b = Endless { served: Arc::default() };
    let cancel = CancelToken::new();
    let trigger = cancel.clone();
    let reported = Arc::new(AtomicU64::new(0));
    let seen = reported.clone();
    let served = a.served.clone();
    // Cancel once hashing is under way and has been reported.
    let canceller = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        while seen.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        trigger.cancel();
        // Read after the cancel: only a read already under way can still add to it.
        served.load(Ordering::SeqCst)
    });
    let mut last = CompareProgress::default();
    let err = compare_with(&a, &RPath::root(), &b, &RPath::root(), &checksum(), &cancel, &mut |p| {
        reported.store(p.bytes_hashed, Ordering::Relaxed);
        last = p.clone();
    })
    .unwrap_err();
    let served_at_cancel = canceller.join().unwrap();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    // Quickly, in bytes rather than time: it stopped after the one 64 KiB read in progress.
    let more = a.served.load(Ordering::SeqCst) - served_at_cancel;
    assert!(more <= 64 * 1024, "read {more} more bytes after the cancel");
    assert!(last.bytes_hashed > 0, "progress reported the hashing: {last:?}");
    assert_eq!(last.current.as_deref(), Some("big.bin"));
    // It stopped reading: the served count no longer grows.
    let after = a.served.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(a.served.load(Ordering::Relaxed), after);
}

#[test]
fn cancelled_before_start_does_no_io() {
    let a = Endless { served: Arc::default() };
    let b = Endless { served: Arc::default() };
    let cancel = CancelToken::new();
    cancel.cancel();
    let err = compare_with(&a, &RPath::root(), &b, &RPath::root(), &checksum(), &cancel, &mut |_| {}).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert_eq!(a.served.load(Ordering::Relaxed) + b.served.load(Ordering::Relaxed), 0);
}

#[test]
fn cancel_between_items_stops_the_walk() {
    let a = Mem::new(full_caps());
    let b = Mem::new(full_caps());
    for i in 0..50 {
        a.put(&format!("/A/d{i}/f.txt"), b"x");
    }
    b.mkdirs("/B");
    let cancel = CancelToken::new();
    let token = cancel.clone();
    let mut seen = 0;
    // Cancelling from the progress callback (as a UI thread might, any time) stops the walk.
    let err = compare_with(&MemRemote(a), &RPath::new("/A"), &MemRemote(b), &RPath::new("/B"), &SyncOptions::default(), &cancel, &mut |p| {
        seen = p.entries;
        token.cancel();
    })
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert!(seen < 50, "stopped early, after {seen} items");
}

#[test]
fn progress_is_monotonic_and_ends_with_the_totals() {
    let a = Mem::with_chunk(full_caps(), 1000);
    let b = Mem::with_chunk(full_caps(), 1000);
    let mut total = 0u64;
    for i in 0..40 {
        let data = bytes(20_000 + i * 100, i as u8);
        a.put(&format!("/A/sub{}/f{i}.bin", i % 4), &data);
        b.put(&format!("/B/sub{}/f{i}.bin", i % 4), &data);
        total += 2 * data.len() as u64; // no server hashes: both sides are read
    }
    // A slow reader so the throttled reports land more than once.
    let slow = Slow(Arc::new(MemRemote(a)));
    let mut events: Vec<CompareProgress> = Vec::new();
    let plan = compare_with(&slow, &RPath::new("/A"), &MemRemote(b), &RPath::new("/B"), &checksum(), &CancelToken::new(), &mut |p| {
        events.push(p.clone())
    })
    .unwrap();
    assert!(plan.is_empty());
    assert!(events.len() >= 2, "{} events", events.len());
    for w in events.windows(2) {
        assert!(w[1].entries >= w[0].entries && w[1].bytes_hashed >= w[0].bytes_hashed, "{w:?}");
    }
    let last = events.last().unwrap();
    assert_eq!((last.entries, last.bytes_hashed, last.current.clone()), (44, total, None), "4 folders + 40 files");
}

/// Delays each read call a little.
struct Slow(Arc<dyn Remote>);

impl Remote for Slow {
    fn caps(&self) -> Caps {
        self.0.caps()
    }
    fn home(&self) -> Result<RPath> {
        self.0.home()
    }
    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        self.0.list(dir)
    }
    fn stat(&self, path: &RPath) -> Result<Entry> {
        self.0.stat(path)
    }
    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        std::thread::sleep(Duration::from_millis(8));
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
