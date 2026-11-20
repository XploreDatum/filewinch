//! The [`Remote`] over a pool of FTP control connections (FTP runs one transfer per control
//! connection, so parallel files need several).

use crate::conn::{self, Features, Ftp, Params, ftp_err};
use crate::list::{format_timeval, parse_list_line, parse_mlsd_line};
use crate::{MAX_CONNECTIONS, MAX_SEGMENT_CONNECTIONS};
use filewinch_core::{Caps, Entry, EntryKind, Error, ErrorKind, RPath, Remote, Result, WriteSink};
use std::any::Any;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};
use suppaftp::{FtpError, FtpResult, ImplFtpStream, Status, TlsStream, TransferStream};

/// Idle connections older than this get a NOOP before reuse; dead ones are replaced.
const STALE: Duration = Duration::from_secs(20);

#[derive(Default)]
struct PoolState {
    idle: Vec<(Ftp, Instant)>,
    open: usize,
}

struct Pool {
    params: Params,
    state: Mutex<PoolState>,
    cv: Condvar,
    closed: AtomicBool,
}

/// One control connection checked out of the pool. Returned on drop unless broken.
struct Lease {
    ftp: Option<Ftp>,
    pool: Arc<Pool>,
    broken: bool,
}

impl Lease {
    fn ftp(&mut self) -> &mut Ftp {
        self.ftp.as_mut().expect("present until drop")
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut s = self.pool.state.lock().unwrap_or_else(|e| e.into_inner());
        match self.ftp.take() {
            Some(f) if !self.broken && !self.pool.closed.load(Ordering::SeqCst) => s.idle.push((f, Instant::now())),
            Some(mut f) => {
                s.open -= 1;
                drop(s);
                if !self.broken {
                    let _ = f.quit();
                }
                self.pool.cv.notify_one();
                return;
            }
            None => s.open -= 1,
        }
        drop(s);
        self.pool.cv.notify_one();
    }
}

impl Pool {
    fn take(self: &Arc<Self>) -> Result<Lease> {
        self.take_upto(MAX_CONNECTIONS, false)
    }

    /// A connection for one piece of a segmented download, growing the pool up to
    /// [`MAX_SEGMENT_CONNECTIONS`]. A server that refuses the extra connection (421 "too many
    /// connections", or a login refused on a second connection) answers
    /// [`Error::too_many_connections`], so the engine falls back to one connection.
    fn take_ranged(self: &Arc<Self>) -> Result<Lease> {
        self.take_upto(MAX_SEGMENT_CONNECTIONS, true)
    }

    fn take_upto(self: &Arc<Self>, cap: usize, ranged: bool) -> Result<Lease> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if self.closed.load(Ordering::SeqCst) {
                return Err(Error::new(ErrorKind::Closed, "This connection was closed."));
            }
            if let Some((mut ftp, since)) = s.idle.pop() {
                if since.elapsed() > STALE {
                    drop(s);
                    let alive = ftp.noop().is_ok();
                    if alive {
                        return Ok(Lease { ftp: Some(ftp), pool: self.clone(), broken: false });
                    }
                    s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    s.open -= 1;
                    continue;
                }
                return Ok(Lease { ftp: Some(ftp), pool: self.clone(), broken: false });
            }
            if s.open < cap {
                s.open += 1;
                let others = s.open > 1;
                drop(s);
                return match conn::open(&self.params) {
                    Ok((ftp, _)) => Ok(Lease { ftp: Some(ftp), pool: self.clone(), broken: false }),
                    Err(e) => {
                        self.state.lock().unwrap_or_else(|x| x.into_inner()).open -= 1;
                        self.cv.notify_one();
                        let refused = matches!(e.kind, ErrorKind::Auth | ErrorKind::Network);
                        Err(if ranged && others && refused { Error::too_many_connections(e) } else { e })
                    }
                };
            }
            s = self.cv.wait(s).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Runs a control-connection command, once more on a fresh connection if the first one
    /// turned out to be dead.
    fn run<T>(self: &Arc<Self>, what: &str, mut f: impl FnMut(&mut Ftp) -> FtpResult<T>) -> Result<T> {
        for attempt in 0..2 {
            let mut lease = self.take()?;
            match f(lease.ftp()) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    let dead = matches!(&e, FtpError::ConnectionError(_)) || matches!(&e, FtpError::UnexpectedResponse(r) if r.status.code() == 421);
                    if dead {
                        lease.broken = true;
                        if attempt == 0 {
                            continue;
                        }
                    }
                    return Err(ftp_err(e, what, &self.params.host));
                }
            }
        }
        unreachable!("the loop returns")
    }
}

pub(crate) struct FtpRemote {
    pool: Arc<Pool>,
    home: RPath,
    feat: Features,
}

fn q(p: &RPath) -> String {
    format!("“{p}”")
}

impl FtpRemote {
    pub fn connect(params: Params) -> Result<FtpRemote> {
        let (mut ftp, feat) = conn::open(&params)?;
        let home = ftp.pwd().map(|p| RPath::new(&p)).unwrap_or_else(|_| RPath::root());
        let pool = Arc::new(Pool {
            params,
            state: Mutex::new(PoolState { idle: vec![(ftp, Instant::now())], open: 1 }),
            cv: Condvar::new(),
            closed: AtomicBool::new(false),
        });
        Ok(FtpRemote { pool, home, feat })
    }
}

/// An open data transfer, type-erased (suppaftp doesn't export its rustls stream type).
trait Xfer: Read + Write + Send {
    fn finish(self: Box<Self>) -> FtpResult<()>;
    /// ABOR on the control connection this transfer belongs to.
    fn abort(self: Box<Self>, ftp: &mut dyn Any) -> FtpResult<()>;
}

impl<T: TlsStream + Send + 'static> Xfer for TransferStream<T>
where
    TransferStream<T>: Read + Write + Send,
{
    fn finish(self: Box<Self>) -> FtpResult<()> {
        TransferStream::finish(*self)
    }
    fn abort(self: Box<Self>, ftp: &mut dyn Any) -> FtpResult<()> {
        match ftp.downcast_mut::<ImplFtpStream<T>>() {
            Some(f) => f.abort(*self),
            None => Err(FtpError::BadResponse),
        }
    }
}

/// An open download or upload holding its control connection.
struct Transfer {
    xfer: Option<Box<dyn Xfer>>,
    lease: Lease,
    what: String,
    /// Uploads: (path, size the file must have once finished).
    verify: Option<(String, u64)>,
}

impl Transfer {
    fn err(&mut self, e: io::Error) -> io::Error {
        self.lease.broken = true;
        let kind = match Error::from(io::Error::new(e.kind(), "")).kind {
            ErrorKind::Other => ErrorKind::Network,
            k => k,
        };
        io::Error::other(Error::new(kind, format!("The connection dropped while trying to {}.", self.what)).with_detail(e))
    }

    fn complete(&mut self) -> Result<()> {
        let Some(x) = self.xfer.take() else { return Ok(()) };
        x.finish().map_err(|e| {
            self.lease.broken = matches!(e, FtpError::ConnectionError(_));
            ftp_err(e, &self.what, &self.lease.pool.params.host)
        })?;
        // A resumed upload is only good if the server really appended; some servers start the
        // file over instead. Unsupported makes the engine send the whole file again.
        if let Some((path, want)) = self.verify.take() {
            match self.lease.ftp().size(&path) {
                Ok(n) if n as u64 == want => {}
                Ok(n) => {
                    let msg = format!("The server didn't continue {}; it will be sent again from the start.", self.what.trim_start_matches("upload "));
                    return Err(Error::new(ErrorKind::Unsupported, msg).with_detail(format!("expected {want} bytes, server has {n}")));
                }
                Err(_) => {} // no SIZE support: trust the transfer reply
            }
        }
        Ok(())
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        if let Some(x) = self.xfer.take() {
            let ftp: &mut dyn Any = self.lease.ftp();
            if x.abort(ftp).is_err() {
                self.lease.broken = true;
            }
        }
    }
}

impl Transfer {
    /// Ends a download before the file's end: ABOR, then a NOOP to prove the control
    /// connection is back in step (a server that had already finished answers twice). If
    /// either fails, the connection is dropped instead of going back to the pool.
    fn stop_early(&mut self) {
        let Some(x) = self.xfer.take() else { return };
        let ftp: &mut dyn Any = self.lease.ftp();
        let clean = x.abort(ftp).is_ok() && self.lease.ftp().noop().is_ok();
        if !clean {
            self.lease.broken = true;
        }
    }
}

/// Exactly `left` bytes of a download, then a clean stop (see [`Transfer::stop_early`]).
struct RangeRead {
    t: Transfer,
    left: u64,
}

impl Read for RangeRead {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 || buf.is_empty() {
            return Ok(0);
        }
        let want = buf.len().min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.t.read(&mut buf[..want])?;
        self.left = self.left.saturating_sub(n as u64);
        if self.left == 0 {
            self.t.stop_early();
        }
        Ok(n)
    }
}

impl Read for Transfer {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(x) = self.xfer.as_mut() else { return Ok(0) };
        match x.read(buf) {
            Ok(0) => {
                self.complete().map_err(io::Error::other)?;
                Ok(0)
            }
            Ok(n) => Ok(n),
            Err(e) => Err(self.err(e)),
        }
    }
}

impl Write for Transfer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let x = self.xfer.as_mut().expect("open until finish");
        let n = x.write(buf).map_err(|e| self.err(e))?;
        if let Some((_, want)) = self.verify.as_mut() {
            *want += n as u64;
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        let x = self.xfer.as_mut().expect("open until finish");
        x.flush().map_err(|e| self.err(e))
    }
}

impl WriteSink for Transfer {
    fn finish(mut self: Box<Self>) -> Result<()> {
        self.complete()
    }
}

impl Remote for FtpRemote {
    fn caps(&self) -> Caps {
        Caps {
            resume_read: self.feat.rest,
            resume_write: true, // REST+STOR, or APPE
            permissions: self.feat.unix,
            set_mtime: self.feat.mfmt,
            symlinks: false,
            real_dirs: true,
            server_rename: true,
            server_copy: false,
            ranged_reads: self.feat.rest,
        }
    }

    fn home(&self) -> Result<RPath> {
        Ok(self.home.clone())
    }

    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        let what = format!("list {}", q(dir));
        let path = dir.as_str().to_string();
        if self.feat.mlsd {
            let lines = self.pool.run(&what, |f| f.mlsd(Some(&path)))?;
            return Ok(lines.iter().filter_map(|l| parse_mlsd_line(dir, l)).collect());
        }
        let lines = self.pool.run(&what, |f| {
            f.cwd(&path)?;
            f.list(None)
        })?;
        let now = SystemTime::now();
        Ok(lines.iter().filter_map(|l| parse_list_line(dir, l, now)).collect())
    }

    fn stat(&self, path: &RPath) -> Result<Entry> {
        let parent = match path.parent() {
            None => {
                return Ok(Entry {
                    name: "/".into(),
                    path: RPath::root(),
                    kind: EntryKind::Dir,
                    size: None,
                    modified: None,
                    mode: None,
                    owner: None,
                    group: None,
                    link_target: None,
                    extra: vec![], checksums: Vec::new(), shared: None,
                });
            }
            Some(p) => p,
        };
        let not_found = || Error::new(ErrorKind::NotFound, format!("Couldn't find {}.", q(path)));
        if self.feat.mlsd {
            let p = path.as_str().to_string();
            match self.pool.run(&format!("find {}", q(path)), |f| f.mlst(Some(&p))) {
                Ok(line) => {
                    if let Some(mut e) = parse_mlsd_line(&parent, &line) {
                        e.name = path.name().to_string();
                        e.path = path.clone();
                        return Ok(e);
                    }
                }
                Err(e) if e.kind == ErrorKind::NotFound => return Err(not_found()),
                Err(_) => {} // fall back to listing the parent
            }
        }
        match self.list(&parent) {
            Ok(entries) => entries.into_iter().find(|e| e.name == path.name()).ok_or_else(not_found),
            Err(e) if e.kind == ErrorKind::NotFound => Err(not_found()),
            Err(e) => Err(e),
        }
    }

    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        let what = format!("download {}", q(path));
        let mut lease = self.pool.take()?;
        let ftp = lease.ftp();
        let p = path.as_str();
        let opened = (|| {
            ftp.resume_transfer(offset as usize)?;
            ftp.retr_as_stream(p)
        })();
        match opened {
            Ok(s) => Ok(Box::new(Transfer { xfer: Some(Box::new(s)), lease, what, verify: None })),
            Err(e) => {
                lease.broken = matches!(e, FtpError::ConnectionError(_));
                Err(ftp_err(e, &what, &self.pool.params.host))
            }
        }
    }

    fn read_range(&self, path: &RPath, offset: u64, len: u64) -> Result<Box<dyn Read + Send>> {
        let what = format!("download {}", q(path));
        let mut lease = self.pool.take_ranged()?;
        let ftp = lease.ftp();
        let p = path.as_str();
        let opened = (|| {
            ftp.resume_transfer(offset as usize)?;
            ftp.retr_as_stream(p)
        })();
        match opened {
            Ok(s) => Ok(Box::new(RangeRead { t: Transfer { xfer: Some(Box::new(s)), lease, what, verify: None }, left: len })),
            Err(e) => {
                lease.broken = matches!(e, FtpError::ConnectionError(_));
                let err = ftp_err(e, &what, &self.pool.params.host);
                // 421 mid-session: the server is shedding connections.
                Err(if err.detail.as_deref().is_some_and(|d| d.starts_with("421")) { Error::too_many_connections(err) } else { err })
            }
        }
    }

    fn write(&self, path: &RPath, offset: u64, _size: Option<u64>) -> Result<Box<dyn WriteSink>> {
        let what = format!("upload {}", q(path));
        let host = self.pool.params.host.clone();
        let mut lease = self.pool.take()?;
        let p = path.as_str().to_string();
        let opened = if offset == 0 {
            lease.ftp().put_with_stream(&p)
        } else {
            // Continue a partial file of exactly `offset` bytes: APPE where possible (plain
            // append, no offset to misread), else REST + STOR. `finish` checks the final size.
            let f = lease.ftp();
            match f.size(&p) {
                Ok(n) if n as u64 == offset => match f.append_with_stream(&p) {
                    Err(FtpError::UnexpectedResponse(r)) if (500..=504).contains(&r.status.code()) && self.feat.rest => {
                        f.resume_transfer(offset as usize).and_then(|_| f.put_with_stream(&p))
                    }
                    r => r,
                },
                Ok(n) => {
                    let msg = format!("Can't continue {}: the server has {n} bytes, expected {offset}.", q(path));
                    return Err(Error::new(ErrorKind::Unsupported, msg));
                }
                Err(e) => Err(e),
            }
        };
        match opened {
            Ok(s) => {
                let verify = (offset > 0).then(|| (p.clone(), offset));
                Ok(Box::new(Transfer { xfer: Some(Box::new(s)), lease, what, verify }))
            }
            Err(e) => {
                lease.broken = matches!(e, FtpError::ConnectionError(_));
                Err(ftp_err(e, &what, &host))
            }
        }
    }

    fn mkdir(&self, path: &RPath) -> Result<()> {
        let p = path.as_str().to_string();
        match self.pool.run(&format!("create the folder {}", q(path)), |f| f.mkdir(&p)) {
            Err(e) if e.kind != ErrorKind::AlreadyExists && e.kind != ErrorKind::Network && self.stat(path).is_ok() => {
                Err(Error::new(ErrorKind::AlreadyExists, format!("{} already exists.", q(path))).with_detail(e))
            }
            r => r,
        }
    }

    fn rename(&self, from: &RPath, to: &RPath) -> Result<()> {
        let (a, b) = (from.as_str().to_string(), to.as_str().to_string());
        self.pool.run(&format!("rename {} to {}", q(from), q(to)), |f| f.rename(&a, &b))
    }

    fn remove_file(&self, path: &RPath) -> Result<()> {
        let p = path.as_str().to_string();
        self.pool.run(&format!("delete {}", q(path)), |f| f.rm(&p))
    }

    fn remove_dir(&self, path: &RPath) -> Result<()> {
        let p = path.as_str().to_string();
        match self.pool.run(&format!("delete the folder {}", q(path)), |f| f.rmdir(&p)) {
            Err(e) if e.kind == ErrorKind::NotFound && self.list(path).is_ok_and(|l| !l.is_empty()) => {
                Err(Error::new(ErrorKind::InvalidPath, format!("The folder {} isn't empty.", q(path))).with_detail(e))
            }
            r => r,
        }
    }

    fn set_mode(&self, path: &RPath, mode: u32) -> Result<()> {
        let cmd = format!("CHMOD {:o} {}", mode & 0o7777, path.as_str());
        self.pool.run(&format!("change permissions of {}", q(path)), |f| f.site(&cmd)).map(|_| ())
    }

    fn set_mtime(&self, path: &RPath, mtime: SystemTime) -> Result<()> {
        if !self.feat.mfmt {
            return Err(Error::unsupported("setting modification times"));
        }
        let cmd = format!("MFMT {} {}", format_timeval(mtime), path.as_str());
        self.pool.run(&format!("set the date of {}", q(path)), |f| f.custom_command(&cmd, &[Status::File])).map(|_| ())
    }

    fn close(&self) {
        self.pool.closed.store(true, Ordering::SeqCst);
        let idle: Vec<_> = {
            let mut s = self.pool.state.lock().unwrap_or_else(|e| e.into_inner());
            let v: Vec<_> = std::mem::take(&mut s.idle);
            s.open -= v.len();
            v
        };
        for (mut f, _) in idle {
            let _ = f.quit();
        }
        self.pool.cv.notify_all();
    }
}

impl Drop for FtpRemote {
    fn drop(&mut self) {
        self.close();
    }
}
