//! compare() with CompareBy::Checksum: server hashes, one-sided hashing, the read-both
//! fallback, and read errors.

mod common;

use common::*;
use filewinch_core::{Caps, Checksum, Entry, ErrorKind, HashAlgo, RPath, Remote, Result, WriteSink};
use filewinch_engine::*;
use std::collections::BTreeMap;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// Wraps a remote: adds "server-listed" checksums to entries and counts reads per path.
struct Hashed {
    inner: Arc<dyn Remote>,
    sums: Mutex<BTreeMap<RPath, Vec<Checksum>>>,
    reads: Mutex<BTreeMap<RPath, usize>>,
}

impl Hashed {
    fn new(inner: Arc<dyn Remote>) -> Arc<Hashed> {
        Arc::new(Hashed { inner, sums: Mutex::default(), reads: Mutex::default() })
    }
    fn list_hash(&self, path: &str, algo: HashAlgo, data: &[u8]) {
        let hex = reference(algo, data);
        self.sums.lock().unwrap().entry(RPath::new(path)).or_default().push(Checksum::new(algo, &hex).unwrap());
    }
    fn reads(&self, path: &str) -> usize {
        self.reads.lock().unwrap().get(&RPath::new(path)).copied().unwrap_or(0)
    }
    fn total_reads(&self) -> usize {
        self.reads.lock().unwrap().values().sum()
    }
    fn decorate(&self, mut e: Entry) -> Entry {
        if let Some(s) = self.sums.lock().unwrap().get(&e.path) {
            e.checksums = s.clone();
        }
        e
    }
}

/// Reference digests (independent of the engine's hasher): via the same crates' one-shot APIs
/// where possible, and the fixed values from src/hash_tests.rs otherwise.
fn reference(algo: HashAlgo, data: &[u8]) -> String {
    use sha2::Digest;
    match algo {
        HashAlgo::Md5 => format!("{:x}", md5::compute(data)),
        HashAlgo::Sha1 => hex(&sha1::Sha1::digest(data)),
        HashAlgo::Sha256 => hex(&sha2::Sha256::digest(data)),
        // One block: SHA-256 of the block's SHA-256.
        HashAlgo::DropboxContent => hex(&sha2::Sha256::digest(sha2::Sha256::digest(data))),
        HashAlgo::QuickXor => {
            assert_eq!(data, b"Hello, World!", "QuickXor reference only known for this input");
            "4828031bd8f0061680e00a6f9d031bc810020000".into()
        }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Remote for Hashed {
    fn caps(&self) -> Caps {
        self.inner.caps()
    }
    fn home(&self) -> Result<RPath> {
        self.inner.home()
    }
    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        Ok(self.inner.list(dir)?.into_iter().map(|e| self.decorate(e)).collect())
    }
    fn stat(&self, path: &RPath) -> Result<Entry> {
        Ok(self.decorate(self.inner.stat(path)?))
    }
    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        *self.reads.lock().unwrap().entry(path.clone()).or_default() += 1;
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

struct Pair {
    a: Arc<Mem>,
    b: Arc<Mem>,
    ha: Arc<Hashed>,
    hb: Arc<Hashed>,
}

fn pair() -> Pair {
    let a = Mem::new(full_caps());
    let b = Mem::new(full_caps());
    a.mkdirs("/A");
    b.mkdirs("/B");
    let ha = Hashed::new(Arc::new(MemRemote(a.clone())));
    let hb = Hashed::new(Arc::new(MemRemote(b.clone())));
    Pair { a, b, ha, hb }
}

fn run(p: &Pair, compare_by: CompareBy) -> filewinch_core::Result<SyncPlan> {
    let o = SyncOptions { compare: compare_by, ..SyncOptions::default() };
    compare(&*p.ha, &RPath::new("/A"), &*p.hb, &RPath::new("/B"), &o)
}

fn rels(plan: &SyncPlan) -> Vec<&str> {
    plan.actions.iter().map(|a| a.rel.as_str()).collect()
}

#[test]
fn same_size_different_content_is_found_only_by_checksum() {
    let p = pair();
    let t = SystemTime::now() - Duration::from_secs(1000);
    p.a.put_at("/A/f.txt", b"version one", t);
    p.b.put_at("/B/f.txt", b"version two", t);
    p.a.put_at("/A/same.txt", b"identical", t);
    p.b.put_at("/B/same.txt", b"identical", t + Duration::from_secs(500)); // only the time differs
    assert!(run(&p, CompareBy::SizeOnly).unwrap().is_empty());
    let plan = run(&p, CompareBy::Checksum).unwrap();
    assert_eq!(rels(&plan), vec!["f.txt"], "times are ignored; content decides");
    assert_eq!(plan.actions[0].kind, SyncActionKind::CopyAToB);
}

#[test]
fn shared_server_hash_reads_nothing() {
    let p = pair();
    p.a.put("/A/x", b"aaaa");
    p.b.put("/B/x", b"bbbb");
    p.a.put("/A/y", b"same");
    p.b.put("/B/y", b"same");
    for (path, data) in [("/A/x", b"aaaa"), ("/A/y", b"same")] {
        p.ha.list_hash(path, HashAlgo::Md5, data);
        p.ha.list_hash(path, HashAlgo::Sha1, data);
    }
    for (path, data) in [("/B/x", b"bbbb"), ("/B/y", b"same")] {
        p.hb.list_hash(path, HashAlgo::Sha1, data);
    }
    let plan = run(&p, CompareBy::Checksum).unwrap();
    assert_eq!(rels(&plan), vec!["x"]);
    assert_eq!(p.ha.total_reads() + p.hb.total_reads(), 0);
}

#[test]
fn server_hash_on_one_side_reads_only_the_other() {
    let p = pair();
    p.a.put("/A/x", b"aaaa");
    p.b.put("/B/x", b"bbbb");
    p.a.put("/A/y", b"same");
    p.b.put("/B/y", b"same");
    // B (say Dropbox) lists content hashes plus a costlier QuickXor; A lists nothing.
    p.hb.list_hash("/B/x", HashAlgo::DropboxContent, b"bbbb");
    p.hb.list_hash("/B/y", HashAlgo::DropboxContent, b"same");
    let plan = run(&p, CompareBy::Checksum).unwrap();
    assert_eq!(rels(&plan), vec!["x"]);
    assert_eq!((p.ha.reads("/A/x"), p.ha.reads("/A/y")), (1, 1));
    assert_eq!(p.hb.total_reads(), 0, "the side with server hashes is never read");
}

#[test]
fn quickxor_listing_is_checked_against_local_bytes() {
    let p = pair();
    p.a.put("/A/h", b"Hello, World!");
    p.b.put("/B/h", b"Hello, World!");
    p.hb.list_hash("/B/h", HashAlgo::QuickXor, b"Hello, World!");
    assert!(run(&p, CompareBy::Checksum).unwrap().is_empty());
    assert_eq!((p.ha.reads("/A/h"), p.hb.total_reads()), (1, 0));
}

#[test]
fn different_algorithms_hash_the_cheapest_offered() {
    let p = pair();
    p.a.put("/A/h", b"Hello, World!");
    p.b.put("/B/h", b"Hello, World?");
    // A offers MD5, B offers QuickXor: hashing B's bytes with MD5 is cheaper.
    p.ha.list_hash("/A/h", HashAlgo::Md5, b"Hello, World!");
    p.hb.list_hash("/B/h", HashAlgo::QuickXor, b"Hello, World!"); // stale on purpose
    let plan = run(&p, CompareBy::Checksum).unwrap();
    assert_eq!(rels(&plan), vec!["h"]);
    assert_eq!((p.ha.total_reads(), p.hb.reads("/B/h")), (0, 1));
}

#[test]
fn no_server_hashes_reads_both_sides() {
    let p = pair();
    p.a.put("/A/x", b"left");
    p.b.put("/B/x", b"rght");
    p.a.put("/A/y", b"same");
    p.b.put("/B/y", b"same");
    p.a.put("/A/sized", b"longer");
    p.b.put("/B/sized", b"short");
    let plan = run(&p, CompareBy::Checksum).unwrap();
    assert_eq!(rels(&plan), vec!["sized", "x"]);
    assert_eq!((p.ha.reads("/A/x"), p.hb.reads("/B/x")), (1, 1));
    assert_eq!((p.ha.reads("/A/y"), p.hb.reads("/B/y")), (1, 1));
    assert_eq!((p.ha.reads("/A/sized"), p.hb.reads("/B/sized")), (0, 0), "a size difference needs no hashing");
}

#[test]
fn read_error_is_an_error_not_equal() {
    let p = pair();
    p.a.put("/A/x", b"same");
    p.b.put("/B/x", b"same");
    p.b.deny.lock().unwrap().push(RPath::new("/B/x"));
    let err = run(&p, CompareBy::Checksum).unwrap_err();
    assert_eq!(err.kind, ErrorKind::PermissionDenied);
    assert!(err.message.contains("/B/x"), "{}", err.message);

    // A stream that drops mid-file surfaces with its kind too.
    let a = Mem::with_chunk(full_caps(), 2);
    a.put("/A/x", b"same");
    let faulty = Faulty::new(Arc::new(MemRemote(a)));
    faulty.read_faults.lock().unwrap().push_back(2);
    let o = SyncOptions { compare: CompareBy::Checksum, ..SyncOptions::default() };
    p.b.deny.lock().unwrap().clear();
    let err = compare(&faulty, &RPath::new("/A"), &*p.hb, &RPath::new("/B"), &o).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Network);
}

#[test]
fn mirror_with_checksum_picks_the_newer_copy() {
    let p = pair();
    let t = SystemTime::now() - Duration::from_secs(1000);
    p.a.put_at("/A/f", b"old!", t);
    p.b.put_at("/B/f", b"new!", t + Duration::from_secs(100));
    p.a.put_at("/A/g", b"one", t);
    p.b.put_at("/B/g", b"two", t);
    let o = SyncOptions { direction: SyncDirection::Mirror, compare: CompareBy::Checksum, ..SyncOptions::default() };
    let plan = compare(&*p.ha, &RPath::new("/A"), &*p.hb, &RPath::new("/B"), &o).unwrap();
    assert_eq!(plan.actions.iter().map(|a| (a.kind, a.rel.as_str())).collect::<Vec<_>>(), vec![(SyncActionKind::CopyBToA, "f")]);
    assert_eq!(plan.conflicts.iter().map(|c| c.rel.as_str()).collect::<Vec<_>>(), vec!["g"]);
}
