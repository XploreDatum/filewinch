//! Test remotes: an in-memory [`Mem`] remote, a [`Faulty`] wrapper that fails reads/writes at
//! a byte position, and a minimal local-disk [`Disk`] remote. Plus an engine harness.

#![allow(dead_code)]

use filewinch_core::{Caps, Checksum, Entry, EntryKind, Error, ErrorKind, PositionalSink, RPath, Remote, Result, WriteSink};
use filewinch_engine::{Engine, EngineConfig, Event, JobId, JobState};
use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

// ---------------------------------------------------------------- in-memory remote

#[derive(Clone, Debug)]
pub enum Node {
    Dir,
    File { data: Vec<u8>, mtime: SystemTime, mode: u32 },
}

pub struct Mem {
    pub caps: Caps,
    pub nodes: Mutex<BTreeMap<RPath, Node>>,
    /// Sleep per read() call, to make transfers slow enough to pause or cancel.
    pub read_delay: Mutex<Duration>,
    /// Largest number of bytes one read() returns.
    pub read_chunk: usize,
    pub open_readers: AtomicUsize,
    pub max_open_readers: AtomicUsize,
    pub aborted_sinks: AtomicUsize,
    pub renames: AtomicUsize,
    /// Paths whose reads fail with PermissionDenied.
    pub deny: Mutex<Vec<RPath>>,
    /// Refuse writes at an offset with Unsupported despite advertising resume_write
    /// (like servers that can't seek on upload).
    pub refuse_offset_writes: std::sync::atomic::AtomicBool,
    /// Checksums `list`/`stat` report per file.
    pub checksums: Mutex<BTreeMap<RPath, Vec<Checksum>>>,
    /// Supports `open_positional` (like the local disk).
    pub positional: AtomicBool,
    /// Bytes handed out by readers (read and read_range).
    pub served: AtomicU64,
}

pub fn full_caps() -> Caps {
    Caps {
        resume_read: true,
        resume_write: true,
        permissions: true,
        set_mtime: true,
        symlinks: false,
        real_dirs: true,
        server_rename: true,
        server_copy: false,
        ranged_reads: false,
    }
}

impl Mem {
    pub fn new(caps: Caps) -> Arc<Mem> {
        let mut nodes = BTreeMap::new();
        nodes.insert(RPath::root(), Node::Dir);
        Arc::new(Mem {
            caps,
            nodes: Mutex::new(nodes),
            read_delay: Mutex::new(Duration::ZERO),
            read_chunk: 64 * 1024,
            open_readers: AtomicUsize::new(0),
            max_open_readers: AtomicUsize::new(0),
            aborted_sinks: AtomicUsize::new(0),
            renames: AtomicUsize::new(0),
            deny: Mutex::new(Vec::new()),
            refuse_offset_writes: std::sync::atomic::AtomicBool::new(false),
            checksums: Mutex::new(BTreeMap::new()),
            positional: AtomicBool::new(false),
            served: AtomicU64::new(0),
        })
    }

    pub fn with_chunk(caps: Caps, read_chunk: usize) -> Arc<Mem> {
        let m = Mem::new(caps);
        let mut m = Arc::try_unwrap(m).ok().expect("fresh");
        m.read_chunk = read_chunk;
        Arc::new(m)
    }

    pub fn put(&self, path: &str, data: &[u8]) {
        self.put_at(path, data, SystemTime::now());
    }

    pub fn put_at(&self, path: &str, data: &[u8], mtime: SystemTime) {
        let p = RPath::new(path);
        let mut n = self.nodes.lock().unwrap();
        let mut d = p.parent();
        while let Some(dir) = d {
            n.entry(dir.clone()).or_insert(Node::Dir);
            d = dir.parent();
        }
        n.insert(p, Node::File { data: data.to_vec(), mtime, mode: 0o644 });
    }

    pub fn mkdirs(&self, path: &str) {
        let p = RPath::new(path);
        let mut n = self.nodes.lock().unwrap();
        let mut d = Some(p);
        while let Some(dir) = d {
            n.entry(dir.clone()).or_insert(Node::Dir);
            d = dir.parent();
        }
    }

    pub fn get(&self, path: &str) -> Option<Vec<u8>> {
        match self.nodes.lock().unwrap().get(&RPath::new(path)) {
            Some(Node::File { data, .. }) => Some(data.clone()),
            _ => None,
        }
    }

    pub fn exists(&self, path: &str) -> bool {
        self.nodes.lock().unwrap().contains_key(&RPath::new(path))
    }

    pub fn mtime(&self, path: &str) -> Option<SystemTime> {
        match self.nodes.lock().unwrap().get(&RPath::new(path)) {
            Some(Node::File { mtime, .. }) => Some(*mtime),
            _ => None,
        }
    }

    /// Every path and file content, for comparing trees.
    pub fn tree(&self, under: &str) -> Vec<(String, Option<Vec<u8>>)> {
        let base = RPath::new(under);
        self.nodes
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p.starts_with(&base) && **p != base)
            .map(|(p, n)| {
                let rel = p.as_str()[base.as_str().len()..].trim_start_matches('/').to_string();
                let data = match n {
                    Node::File { data, .. } => Some(data.clone()),
                    Node::Dir => None,
                };
                (rel, data)
            })
            .collect()
    }

    fn entry(path: &RPath, n: &Node) -> Entry {
        let (kind, size, modified, mode) = match n {
            Node::Dir => (EntryKind::Dir, None, None, Some(0o755)),
            Node::File { data, mtime, mode } => (EntryKind::File, Some(data.len() as u64), Some(*mtime), Some(*mode)),
        };
        Entry {
            name: path.name().to_string(),
            path: path.clone(),
            kind,
            size,
            modified,
            mode,
            owner: None,
            group: None,
            link_target: None,
            extra: vec![], checksums: Vec::new(), shared: None,
        }
    }
}

fn not_found(p: &RPath) -> Error {
    Error::new(ErrorKind::NotFound, format!("{p} not found"))
}

struct MemReader {
    mem: Arc<Mem>,
    data: Vec<u8>,
    pos: usize,
}

impl Read for MemReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let d = *self.mem.read_delay.lock().unwrap();
        if !d.is_zero() {
            std::thread::sleep(d);
        }
        let n = buf.len().min(self.mem.read_chunk).min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        self.mem.served.fetch_add(n as u64, Ordering::SeqCst);
        Ok(n)
    }
}

impl Drop for MemReader {
    fn drop(&mut self) {
        self.mem.open_readers.fetch_sub(1, Ordering::SeqCst);
    }
}

struct MemSink {
    mem: Arc<Mem>,
    path: RPath,
    finished: bool,
}

impl Write for MemSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut n = self.mem.nodes.lock().unwrap();
        match n.get_mut(&self.path) {
            Some(Node::File { data, .. }) => {
                data.extend_from_slice(buf);
                Ok(buf.len())
            }
            _ => Err(io::Error::other(not_found(&self.path))),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl WriteSink for MemSink {
    fn finish(mut self: Box<Self>) -> Result<()> {
        self.finished = true;
        Ok(())
    }
}

impl Drop for MemSink {
    fn drop(&mut self) {
        if !self.finished {
            // Like SFTP: the partial file stays (that is what makes resume possible).
            self.mem.aborted_sinks.fetch_add(1, Ordering::SeqCst);
        }
    }
}

struct MemPositional {
    mem: Arc<Mem>,
    path: RPath,
}

impl PositionalSink for MemPositional {
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        match self.mem.nodes.lock().unwrap().get_mut(&self.path) {
            Some(Node::File { data, .. }) => {
                let a = offset as usize;
                assert!(a + buf.len() <= data.len(), "positional write past the preallocated size");
                data[a..a + buf.len()].copy_from_slice(buf);
                Ok(())
            }
            _ => Err(not_found(&self.path)),
        }
    }
    fn finish(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

/// `Remote` for `Arc<Mem>` via a newtype so readers can hold an `Arc` back to the store.
#[derive(Clone)]
pub struct MemRemote(pub Arc<Mem>);

impl MemRemote {
    fn entry(&self, p: &RPath, n: &Node) -> Entry {
        let mut e = Mem::entry(p, n);
        if let Some(c) = self.0.checksums.lock().unwrap().get(p) {
            e.checksums = c.clone();
        }
        e
    }
}

impl Remote for MemRemote {
    fn caps(&self) -> Caps {
        self.0.caps
    }
    fn home(&self) -> Result<RPath> {
        Ok(RPath::root())
    }
    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        let n = self.0.nodes.lock().unwrap();
        match n.get(dir) {
            Some(Node::Dir) => {}
            Some(_) => return Err(Error::new(ErrorKind::InvalidPath, "not a folder")),
            None => return Err(not_found(dir)),
        }
        Ok(n.iter().filter(|(p, _)| p.parent().as_ref() == Some(dir)).map(|(p, x)| self.entry(p, x)).collect())
    }
    fn stat(&self, path: &RPath) -> Result<Entry> {
        let n = self.0.nodes.lock().unwrap();
        n.get(path).map(|x| self.entry(path, x)).ok_or_else(|| not_found(path))
    }
    fn read_range(&self, path: &RPath, offset: u64, len: u64) -> Result<Box<dyn Read + Send>> {
        // Copies only the range (a whole-file copy per segment would be wasteful in tests).
        let data = match self.0.nodes.lock().unwrap().get(path) {
            Some(Node::File { data, .. }) => {
                let a = (offset as usize).min(data.len());
                data[a..(a + len as usize).min(data.len())].to_vec()
            }
            Some(Node::Dir) => return Err(Error::new(ErrorKind::InvalidPath, "is a folder")),
            None => return Err(not_found(path)),
        };
        let now = self.0.open_readers.fetch_add(1, Ordering::SeqCst) + 1;
        self.0.max_open_readers.fetch_max(now, Ordering::SeqCst);
        Ok(Box::new(MemReader { mem: self.0.clone(), data, pos: 0 }))
    }
    fn open_positional(&self, path: &RPath, size: u64) -> Result<Box<dyn PositionalSink>> {
        if !self.0.positional.load(Ordering::SeqCst) {
            return Err(Error::unsupported("positional writes"));
        }
        let mut n = self.0.nodes.lock().unwrap();
        if !matches!(path.parent().and_then(|p| n.get(&p).cloned()), Some(Node::Dir)) {
            return Err(not_found(path));
        }
        match n.get_mut(path) {
            Some(Node::Dir) => return Err(Error::new(ErrorKind::InvalidPath, "is a folder")),
            Some(Node::File { data, .. }) => data.resize(size as usize, 0),
            None => {
                n.insert(path.clone(), Node::File { data: vec![0; size as usize], mtime: SystemTime::now(), mode: 0o644 });
            }
        }
        Ok(Box::new(MemPositional { mem: self.0.clone(), path: path.clone() }))
    }
    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        if self.0.deny.lock().unwrap().contains(path) {
            return Err(Error::new(ErrorKind::PermissionDenied, format!("You don't have permission to read {path}.")));
        }
        let data = match self.0.nodes.lock().unwrap().get(path) {
            Some(Node::File { data, .. }) => data.clone(),
            Some(Node::Dir) => return Err(Error::new(ErrorKind::InvalidPath, "is a folder")),
            None => return Err(not_found(path)),
        };
        assert!(offset == 0 || self.0.caps.resume_read, "engine passed an offset without resume_read");
        let now = self.0.open_readers.fetch_add(1, Ordering::SeqCst) + 1;
        self.0.max_open_readers.fetch_max(now, Ordering::SeqCst);
        Ok(Box::new(MemReader { mem: self.0.clone(), data, pos: offset as usize }))
    }
    fn write(&self, path: &RPath, offset: u64, _size: Option<u64>) -> Result<Box<dyn WriteSink>> {
        assert!(offset == 0 || self.0.caps.resume_write, "engine passed an offset without resume_write");
        if offset > 0 && self.0.refuse_offset_writes.load(Ordering::SeqCst) {
            return Err(Error::new(ErrorKind::Unsupported, "This server can't continue uploads."));
        }
        let mut n = self.0.nodes.lock().unwrap();
        match path.parent().and_then(|p| n.get(&p).cloned()) {
            Some(Node::Dir) => {}
            _ => return Err(not_found(path)),
        }
        match n.get_mut(path) {
            Some(Node::Dir) => return Err(Error::new(ErrorKind::InvalidPath, "is a folder")),
            Some(Node::File { data, .. }) if offset > 0 => {
                assert!(data.len() as u64 >= offset, "resume offset beyond the partial file");
                data.truncate(offset as usize);
            }
            _ => {
                n.insert(path.clone(), Node::File { data: vec![], mtime: SystemTime::now(), mode: 0o644 });
            }
        }
        Ok(Box::new(MemSink { mem: self.0.clone(), path: path.clone(), finished: false }))
    }
    fn mkdir(&self, path: &RPath) -> Result<()> {
        let mut n = self.0.nodes.lock().unwrap();
        if n.contains_key(path) {
            return Err(Error::new(ErrorKind::AlreadyExists, "exists"));
        }
        if !matches!(path.parent().and_then(|p| n.get(&p).cloned()), Some(Node::Dir)) {
            return Err(not_found(path));
        }
        n.insert(path.clone(), Node::Dir);
        Ok(())
    }
    fn rename(&self, from: &RPath, to: &RPath) -> Result<()> {
        self.0.renames.fetch_add(1, Ordering::SeqCst);
        let mut n = self.0.nodes.lock().unwrap();
        if !n.contains_key(from) {
            return Err(not_found(from));
        }
        if n.contains_key(to) {
            return Err(Error::new(ErrorKind::AlreadyExists, "exists"));
        }
        let moving: Vec<RPath> = n.keys().filter(|p| p.starts_with(from)).cloned().collect();
        for p in moving {
            let node = n.remove(&p).unwrap();
            let rel = &p.as_str()[from.as_str().len()..];
            n.insert(RPath::new(&format!("{}{}", to.as_str(), rel)), node);
        }
        Ok(())
    }
    fn remove_file(&self, path: &RPath) -> Result<()> {
        let mut n = self.0.nodes.lock().unwrap();
        match n.get(path) {
            Some(Node::File { .. }) => {
                n.remove(path);
                Ok(())
            }
            Some(Node::Dir) => Err(Error::new(ErrorKind::InvalidPath, "is a folder")),
            None => Err(not_found(path)),
        }
    }
    fn remove_dir(&self, path: &RPath) -> Result<()> {
        let mut n = self.0.nodes.lock().unwrap();
        if n.keys().any(|p| p.parent().as_ref() == Some(path)) {
            return Err(Error::new(ErrorKind::InvalidPath, "The folder isn't empty."));
        }
        match n.get(path) {
            Some(Node::Dir) => {
                n.remove(path);
                Ok(())
            }
            Some(_) => Err(Error::new(ErrorKind::InvalidPath, "not a folder")),
            None => Err(not_found(path)),
        }
    }
    fn set_mode(&self, path: &RPath, mode: u32) -> Result<()> {
        match self.0.nodes.lock().unwrap().get_mut(path) {
            Some(Node::File { mode: m, .. }) => {
                *m = mode;
                Ok(())
            }
            _ => Err(not_found(path)),
        }
    }
    fn set_mtime(&self, path: &RPath, t: SystemTime) -> Result<()> {
        match self.0.nodes.lock().unwrap().get_mut(path) {
            Some(Node::File { mtime, .. }) => {
                *mtime = t;
                Ok(())
            }
            _ => Err(not_found(path)),
        }
    }
}

// ---------------------------------------------------------------- fault injection

/// Wraps a remote; each queued position makes one read (or write) stream fail with a
/// Network error when it reaches that absolute byte position in the file.
pub struct Faulty {
    pub inner: Arc<dyn Remote>,
    pub read_faults: Arc<Mutex<VecDeque<u64>>>,
    pub write_faults: Arc<Mutex<VecDeque<u64>>>,
    /// Make every fault non-transient instead (PermissionDenied).
    pub hard: bool,
    /// Make every fault RateLimited with this retry_after instead.
    pub rate_limited: Option<Duration>,
}

impl Faulty {
    pub fn new(inner: Arc<dyn Remote>) -> Faulty {
        Faulty { inner, read_faults: Arc::default(), write_faults: Arc::default(), hard: false, rate_limited: None }
    }
}

fn fault_error(hard: bool, rate_limited: Option<Duration>) -> io::Error {
    let e = if let Some(after) = rate_limited {
        Error::new(ErrorKind::RateLimited, "Slow down (injected).").with_retry_after(after)
    } else if hard {
        Error::new(ErrorKind::PermissionDenied, "Permission denied (injected).")
    } else {
        Error::new(ErrorKind::Network, "The connection dropped (injected).")
    };
    io::Error::other(e)
}

struct FaultReader {
    rate_limited: Option<Duration>,
    inner: Box<dyn Read + Send>,
    pos: u64,
    fail_at: Option<u64>,
    hard: bool,
}

impl Read for FaultReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut limit = buf.len();
        if let Some(f) = self.fail_at {
            if self.pos >= f {
                return Err(fault_error(self.hard, self.rate_limited));
            }
            limit = limit.min((f - self.pos) as usize);
        }
        let n = self.inner.read(&mut buf[..limit])?;
        self.pos += n as u64;
        Ok(n)
    }
}

struct FaultSink {
    rate_limited: Option<Duration>,
    inner: Box<dyn WriteSink>,
    pos: u64,
    fail_at: Option<u64>,
    hard: bool,
}

impl Write for FaultSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut limit = buf.len();
        if let Some(f) = self.fail_at {
            if self.pos >= f {
                return Err(fault_error(self.hard, self.rate_limited));
            }
            limit = limit.min((f - self.pos) as usize);
        }
        let n = self.inner.write(&buf[..limit])?;
        self.pos += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl WriteSink for FaultSink {
    fn finish(self: Box<Self>) -> Result<()> {
        self.inner.finish()
    }
}

impl Remote for Faulty {
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
        let fail_at = self.read_faults.lock().unwrap().pop_front();
        Ok(Box::new(FaultReader { rate_limited: self.rate_limited, inner: self.inner.read(path, offset)?, pos: offset, fail_at, hard: self.hard }))
    }
    fn write(&self, path: &RPath, offset: u64, size: Option<u64>) -> Result<Box<dyn WriteSink>> {
        let fail_at = self.write_faults.lock().unwrap().pop_front();
        Ok(Box::new(FaultSink { rate_limited: self.rate_limited, inner: self.inner.write(path, offset, size)?, pos: offset, fail_at, hard: self.hard }))
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
        self.inner.set_mode(path, mode)
    }
    fn set_mtime(&self, path: &RPath, t: SystemTime) -> Result<()> {
        self.inner.set_mtime(path, t)
    }
}

// ---------------------------------------------------------------- local disk

/// A minimal local-disk remote rooted at a folder (RPath "/" = `root`).
pub struct Disk {
    pub root: PathBuf,
}

impl Disk {
    pub fn new(root: &Path) -> Disk {
        std::fs::create_dir_all(root).unwrap();
        Disk { root: root.to_path_buf() }
    }
    fn p(&self, r: &RPath) -> PathBuf {
        self.root.join(r.as_str().trim_start_matches('/'))
    }
    fn entry(&self, r: &RPath) -> Result<Entry> {
        let md = std::fs::symlink_metadata(self.p(r))?;
        #[cfg(unix)]
        let mode = Some(std::os::unix::fs::PermissionsExt::mode(&md.permissions()) & 0o7777);
        #[cfg(not(unix))]
        let mode = None;
        let kind = if md.is_dir() {
            EntryKind::Dir
        } else if md.is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::File
        };
        let link_target = std::fs::read_link(self.p(r)).ok().map(|t| t.to_string_lossy().into_owned());
        Ok(Entry {
            name: r.name().to_string(),
            path: r.clone(),
            kind,
            size: (kind == EntryKind::File).then_some(md.len()),
            modified: md.modified().ok(),
            mode,
            owner: None,
            group: None,
            link_target,
            extra: vec![], checksums: Vec::new(), shared: None,
        })
    }
}

struct DiskSink(std::fs::File);
impl Write for DiskSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
impl WriteSink for DiskSink {
    fn finish(self: Box<Self>) -> Result<()> {
        self.0.sync_all()?;
        Ok(())
    }
}

impl Remote for Disk {
    fn caps(&self) -> Caps {
        Caps { symlinks: true, ..full_caps() }
    }
    fn home(&self) -> Result<RPath> {
        Ok(RPath::root())
    }
    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        let mut out = vec![];
        for e in std::fs::read_dir(self.p(dir))? {
            let name = e?.file_name().to_string_lossy().into_owned();
            out.push(self.entry(&dir.join(&name))?);
        }
        Ok(out)
    }
    /// Like the real local backend: doesn't follow symlinks, reports their target.
    fn stat(&self, path: &RPath) -> Result<Entry> {
        self.entry(path)
    }
    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        use std::io::{Seek, SeekFrom};
        let mut f = std::fs::File::open(self.p(path))?;
        f.seek(SeekFrom::Start(offset))?;
        Ok(Box::new(f))
    }
    fn write(&self, path: &RPath, offset: u64, _size: Option<u64>) -> Result<Box<dyn WriteSink>> {
        use std::io::{Seek, SeekFrom};
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(offset == 0).open(self.p(path))?;
        if offset > 0 {
            f.set_len(offset)?;
            f.seek(SeekFrom::Start(offset))?;
        }
        Ok(Box::new(DiskSink(f)))
    }
    fn mkdir(&self, path: &RPath) -> Result<()> {
        Ok(std::fs::create_dir(self.p(path))?)
    }
    fn rename(&self, from: &RPath, to: &RPath) -> Result<()> {
        Ok(std::fs::rename(self.p(from), self.p(to))?)
    }
    fn remove_file(&self, path: &RPath) -> Result<()> {
        Ok(std::fs::remove_file(self.p(path))?)
    }
    fn remove_dir(&self, path: &RPath) -> Result<()> {
        Ok(std::fs::remove_dir(self.p(path))?)
    }
    #[cfg(unix)]
    fn set_mode(&self, path: &RPath, mode: u32) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        Ok(std::fs::set_permissions(self.p(path), std::fs::Permissions::from_mode(mode))?)
    }
    #[cfg(not(unix))]
    fn set_mode(&self, _path: &RPath, _mode: u32) -> Result<()> {
        Err(filewinch_core::Error::unsupported("Unix permissions"))
    }
    fn set_mtime(&self, path: &RPath, t: SystemTime) -> Result<()> {
        let f = std::fs::File::options().write(true).open(self.p(path))?;
        Ok(f.set_modified(t)?)
    }
}

// ---------------------------------------------------------------- harness

pub fn scratch(name: &str) -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let n = format!("{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::SeqCst));
    // System temp (APFS): the build dir is on exFAT here, which adds AppleDouble `._` files.
    let p = std::env::temp_dir().join("filewinch-tests").join(format!("engine-{name}-{n}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Deterministic test bytes.
pub fn bytes(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| ((i as u32).wrapping_mul(2_654_435_761).rotate_left(seed as u32 % 31) >> 13) as u8 ^ seed).collect()
}

pub struct Harness {
    pub engine: Engine,
    pub rx: Receiver<Event>,
    pub log: Vec<Event>,
}

pub fn cfg() -> EngineConfig {
    EngineConfig {
        max_jobs: 2,
        max_files_per_job: 4,
        retries: 4,
        chunk: 16 * 1024,
        bandwidth_limit: None,
        retry_delay: Duration::from_millis(10),
        segments_per_file: 3,
    }
}

impl Harness {
    pub fn new(cfg: EngineConfig) -> Harness {
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        let engine = Engine::new(cfg, move |e| {
            let _ = tx.lock().unwrap().send(e);
        });
        Harness { engine, rx, log: vec![] }
    }

    /// Collects events until `pred` matches one or `timeout` passes. Returns the match.
    pub fn until(&mut self, timeout: Duration, mut pred: impl FnMut(&Event) -> bool) -> Option<Event> {
        let end = Instant::now() + timeout;
        loop {
            let left = end.checked_duration_since(Instant::now())?;
            match self.rx.recv_timeout(left) {
                Ok(e) => {
                    self.log.push(e.clone());
                    if pred(&e) {
                        return Some(e);
                    }
                }
                Err(_) => return None,
            }
        }
    }

    /// Waits for job `id` to finish and returns its final state.
    pub fn wait(&mut self, id: JobId) -> JobState {
        // It may have finished while we were waiting for something else; take the newest state
        // only if it is final (a retried job logs an older final state first).
        while let Ok(e) = self.rx.try_recv() {
            self.log.push(e);
        }
        let last = self.log.iter().rev().find_map(|e| match e {
            Event::State(j, s) if *j == id => Some(s.clone()),
            _ => None,
        });
        if let Some(s) = last.filter(|s| s.is_finished()) {
            return s;
        }
        match self.until(Duration::from_secs(30), |e| matches!(e, Event::State(j, s) if *j == id && s.is_finished())) {
            Some(Event::State(_, s)) => s,
            _ => panic!("job {id} didn't finish; events: {:#?}", self.log.iter().rev().take(20).collect::<Vec<_>>()),
        }
    }

    pub fn failures(&self, id: JobId) -> Vec<(RPath, Error)> {
        self.log
            .iter()
            .filter_map(|e| match e {
                Event::ItemFailed { job, path, error } if *job == id => Some((path.clone(), error.clone())),
                _ => None,
            })
            .collect()
    }

    pub fn states(&self, id: JobId) -> Vec<JobState> {
        self.log.iter().filter_map(|e| match e {
            Event::State(j, s) if *j == id => Some(s.clone()),
            _ => None,
        }).collect()
    }
}

pub fn assert_done(s: &JobState) {
    assert!(matches!(s, JobState::Done), "expected Done, got {s:?}");
}
