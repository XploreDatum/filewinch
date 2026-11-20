//! The [`Remote`] over a small pool of SFTP connections.

use crate::session::{self, Conn, Params};
use crate::{MAX_CONNECTIONS, errors, runtime};
use filewinch_core::{Caps, Entry, EntryKind, Error, ErrorKind, RPath, Remote, Result, WriteSink};
use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::fs::{File, Metadata};
use russh_sftp::protocol::{FileAttributes, OpenFlags};
use std::future::Future;
use std::io::{self, Read, Write};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = std::result::Result<T, SftpError>> + Send + 'a>>;

struct Slot {
    conn: Conn,
    busy: AtomicUsize,
}

impl Slot {
    fn dead(&self) -> bool {
        self.conn.handle.is_closed()
    }
}

/// A connection in use; frees its slot on drop.
struct Lease(Arc<Slot>);

impl Drop for Lease {
    fn drop(&mut self) {
        self.0.busy.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Pool {
    params: Params,
    slots: Mutex<Vec<Arc<Slot>>>,
    closed: AtomicBool,
}

impl Pool {
    fn slots(&self) -> std::sync::MutexGuard<'_, Vec<Arc<Slot>>> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A connection for one call. `exclusive` (file transfers) prefers an idle connection and
    /// opens another while under the cap; shared calls take the least busy.
    fn lease(&self, exclusive: bool) -> Result<Lease> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Error::new(ErrorKind::Closed, "This connection was closed."));
        }
        {
            let mut slots = self.slots();
            slots.retain(|s| !s.dead());
            let idle = slots.iter().find(|s| s.busy.load(Ordering::SeqCst) == 0);
            let least = slots.iter().min_by_key(|s| s.busy.load(Ordering::SeqCst));
            let pick = match (exclusive, idle, least) {
                (_, Some(s), _) => Some(s.clone()),
                (false, None, Some(s)) => Some(s.clone()),
                (true, None, Some(s)) if slots.len() >= MAX_CONNECTIONS => Some(s.clone()),
                _ => None,
            };
            if let Some(s) = pick {
                s.busy.fetch_add(1, Ordering::SeqCst);
                return Ok(Lease(s));
            }
        }
        let conn = runtime().block_on(session::open(&self.params))?;
        let slot = Arc::new(Slot { conn, busy: AtomicUsize::new(1) });
        self.slots().push(slot.clone());
        Ok(Lease(slot))
    }

    fn forget(&self, slot: &Arc<Slot>) {
        self.slots().retain(|s| !Arc::ptr_eq(s, slot));
    }

    /// Runs a metadata call, retrying once on a fresh connection if the session died.
    fn run<T>(&self, what: &str, op: impl for<'a> Fn(&'a SftpSession) -> BoxFut<'a, T>) -> Result<T> {
        for attempt in 0..2 {
            let lease = self.lease(false)?;
            match runtime().block_on(op(&lease.0.conn.sftp)) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    let session_level = !matches!(e, SftpError::Status(_));
                    if attempt == 0 && (lease.0.dead() || session_level) {
                        self.forget(&lease.0);
                        continue;
                    }
                    return Err(errors::sftp(e, what));
                }
            }
        }
        unreachable!("the loop returns")
    }
}

pub(crate) struct SftpRemote {
    pool: Arc<Pool>,
    home: RPath,
}

fn q(p: &RPath) -> String {
    format!("“{}”", p)
}

pub(crate) fn to_entry(path: RPath, m: &Metadata) -> Entry {
    let ft = m.file_type();
    let kind = if ft.is_dir() {
        EntryKind::Dir
    } else if ft.is_symlink() {
        EntryKind::Symlink
    } else if ft.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    let name = if path.is_root() { "/".to_string() } else { path.name().to_string() };
    Entry {
        name,
        kind,
        size: if kind == EntryKind::Dir { None } else { m.size },
        modified: m.mtime.map(|t| UNIX_EPOCH + Duration::from_secs(t as u64)),
        mode: m.permissions.map(|p| p & 0o7777),
        owner: m.user.clone().or(m.uid.map(|u| u.to_string())),
        group: m.group.clone().or(m.gid.map(|g| g.to_string())),
        link_target: None,
        extra: vec![],
        path, checksums: Vec::new(), shared: None,
    }
}

impl SftpRemote {
    pub fn connect(params: Params) -> Result<SftpRemote> {
        let conn = runtime().block_on(session::open(&params))?;
        let home = runtime()
            .block_on(conn.sftp.canonicalize("."))
            .map(|h| RPath::new(&h))
            .unwrap_or_else(|_| RPath::root());
        let pool = Pool { params, slots: Mutex::new(vec![Arc::new(Slot { conn, busy: AtomicUsize::new(0) })]), closed: AtomicBool::new(false) };
        Ok(SftpRemote { pool: Arc::new(pool), home })
    }

    fn link_target(&self, p: &RPath) -> Option<String> {
        let s = p.as_str().to_string();
        self.pool.run("read the link", |sf| Box::pin(sf.read_link(s.clone()))).ok()
    }

    fn exists(&self, p: &RPath) -> bool {
        self.stat(p).is_ok()
    }
}

impl Remote for SftpRemote {
    fn caps(&self) -> Caps {
        Caps {
            resume_read: true,
            resume_write: true,
            permissions: true,
            set_mtime: true,
            symlinks: true,
            real_dirs: true,
            server_rename: true,
            server_copy: false,
        }
    }

    fn home(&self) -> Result<RPath> {
        Ok(self.home.clone())
    }

    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        let d = dir.as_str().to_string();
        let rd = self.pool.run(&format!("list {}", q(dir)), |s| Box::pin(s.read_dir(d.clone())))?;
        let mut out = Vec::new();
        for e in rd {
            let name = e.file_name();
            if name == "." || name == ".." || name.is_empty() || name.contains('/') {
                continue;
            }
            let mut entry = to_entry(dir.join(&name), &e.metadata());
            if entry.kind == EntryKind::Symlink {
                entry.link_target = self.link_target(&entry.path);
            }
            out.push(entry);
        }
        Ok(out)
    }

    /// Doesn't follow symlinks; fills `link_target` for them.
    fn stat(&self, path: &RPath) -> Result<Entry> {
        let p = path.as_str().to_string();
        let m = self.pool.run(&format!("find {}", q(path)), |s| Box::pin(s.symlink_metadata(p.clone())))?;
        let mut e = to_entry(path.clone(), &m);
        if e.kind == EntryKind::Symlink {
            e.link_target = self.link_target(path);
        }
        Ok(e)
    }

    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        let what = format!("read {}", q(path));
        let lease = self.pool.lease(true)?;
        let p = path.as_str().to_string();
        let file = runtime().block_on(async {
            let mut f = lease.0.conn.sftp.open_with_flags(p, OpenFlags::READ).await.map_err(|e| errors::sftp(e, &what))?;
            if offset > 0 {
                f.seek(io::SeekFrom::Start(offset)).await.map_err(|e| errors::file_io(e, false, &what))?;
            }
            Ok::<File, Error>(f)
        })?;
        Ok(Box::new(SftpReader { file, lease, what }))
    }

    fn write(&self, path: &RPath, offset: u64, _size: Option<u64>) -> Result<Box<dyn WriteSink>> {
        let what = format!("write {}", q(path));
        let lease = self.pool.lease(true)?;
        let p = path.as_str().to_string();
        // Resuming appends to a partial copy that must hold exactly `offset` bytes. Some
        // servers (rclone without its VFS cache) truncate on open regardless of flags, so the
        // size is checked on the open handle; a mismatch is Unsupported and the engine starts
        // the file over instead of writing after a hole.
        let flags = if offset == 0 {
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE
        } else {
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::APPEND
        };
        let file = runtime().block_on(async {
            let mut f = lease.0.conn.sftp.open_with_flags(p, flags).await.map_err(|e| errors::sftp(e, &what))?;
            if offset > 0 {
                let size = f.metadata().await.map_err(|e| errors::sftp(e, &what))?.size;
                if size != Some(offset) {
                    let msg = format!("The server can't continue uploading {}; it will be sent again from the start.", q(path));
                    return Err(Error::new(ErrorKind::Unsupported, msg).with_detail(format!("expected {offset} bytes, server has {size:?}")));
                }
                f.seek(io::SeekFrom::Start(offset)).await.map_err(|e| errors::file_io(e, false, &what))?;
            }
            Ok::<File, Error>(f)
        })?;
        Ok(Box::new(SftpSink { file: Some(file), lease, what }))
    }

    fn mkdir(&self, path: &RPath) -> Result<()> {
        let p = path.as_str().to_string();
        match self.pool.run(&format!("create the folder {}", q(path)), |s| Box::pin(s.create_dir(p.clone()))) {
            Err(e) if e.kind == ErrorKind::Other && self.exists(path) => {
                Err(Error::new(ErrorKind::AlreadyExists, format!("{} already exists.", q(path))).with_detail(e))
            }
            r => r,
        }
    }

    fn rename(&self, from: &RPath, to: &RPath) -> Result<()> {
        let (f, t) = (from.as_str().to_string(), to.as_str().to_string());
        match self.pool.run(&format!("rename {} to {}", q(from), q(to)), |s| Box::pin(s.rename(f.clone(), t.clone()))) {
            Err(e) if e.kind == ErrorKind::Other && self.exists(to) => {
                Err(Error::new(ErrorKind::AlreadyExists, format!("{} already exists.", q(to))).with_detail(e))
            }
            r => r,
        }
    }

    fn remove_file(&self, path: &RPath) -> Result<()> {
        let p = path.as_str().to_string();
        self.pool.run(&format!("delete {}", q(path)), |s| Box::pin(s.remove_file(p.clone())))
    }

    fn remove_dir(&self, path: &RPath) -> Result<()> {
        let p = path.as_str().to_string();
        match self.pool.run(&format!("delete the folder {}", q(path)), |s| Box::pin(s.remove_dir(p.clone()))) {
            Err(e) if e.kind == ErrorKind::Other && self.list(path).is_ok_and(|l| !l.is_empty()) => {
                Err(Error::new(ErrorKind::InvalidPath, format!("The folder {} isn't empty.", q(path))).with_detail(e))
            }
            r => r,
        }
    }

    fn set_mode(&self, path: &RPath, mode: u32) -> Result<()> {
        let p = path.as_str().to_string();
        let attrs = FileAttributes { permissions: Some(mode & 0o7777), ..FileAttributes::empty() };
        self.pool.run(&format!("change permissions of {}", q(path)), |s| Box::pin(s.set_metadata(p.clone(), attrs.clone())))
    }

    fn set_mtime(&self, path: &RPath, mtime: SystemTime) -> Result<()> {
        let p = path.as_str().to_string();
        let secs = mtime.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()).min(u32::MAX as u64) as u32;
        let attrs = FileAttributes { mtime: Some(secs), atime: Some(secs), ..FileAttributes::empty() };
        self.pool.run(&format!("set the date of {}", q(path)), |s| Box::pin(s.set_metadata(p.clone(), attrs.clone())))
    }

    fn close(&self) {
        self.pool.closed.store(true, Ordering::SeqCst);
        let slots: Vec<_> = self.pool.slots().drain(..).collect();
        for s in slots {
            let _ = runtime().block_on(async {
                let _ = s.conn.sftp.close().await;
                s.conn.handle.disconnect(russh::Disconnect::ByApplication, "", "en").await
            });
        }
    }
}

impl Drop for SftpRemote {
    fn drop(&mut self) {
        self.close();
    }
}

struct SftpReader {
    file: File,
    lease: Lease,
    what: String,
}

impl Read for SftpReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        runtime()
            .block_on(self.file.read(buf))
            .map_err(|e| io::Error::other(errors::file_io(e, self.lease.0.dead(), &self.what)))
    }
}

struct SftpSink {
    file: Option<File>,
    lease: Lease,
    what: String,
}

impl Write for SftpSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let f = self.file.as_mut().expect("open until finish");
        runtime()
            .block_on(f.write(buf))
            .map_err(|e| io::Error::other(errors::file_io(e, self.lease.0.dead(), &self.what)))
    }

    fn flush(&mut self) -> io::Result<()> {
        let f = self.file.as_mut().expect("open until finish");
        runtime()
            .block_on(f.flush())
            .map_err(|e| io::Error::other(errors::file_io(e, self.lease.0.dead(), &self.what)))
    }
}

impl WriteSink for SftpSink {
    fn finish(mut self: Box<Self>) -> Result<()> {
        let mut f = self.file.take().expect("finish once");
        runtime().block_on(f.shutdown()).map_err(|e| errors::file_io(e, self.lease.0.dead(), &self.what))
    }
}
