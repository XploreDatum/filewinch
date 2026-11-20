//! Uploads: one streamed Put Blob for small files of known size, staged blocks (Put Block +
//! Put Block List) for everything else. Nothing is committed until `finish`.

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use filewinch_core::{Error, ErrorKind, RPath, Result, WriteSink};
use filewinch_http::{UploadPipe, to_io, upload_pipe};

use crate::client::{Call, Client};
use crate::remote::{AzureRemote, Loc, quoted};

const MIB: u64 = 1024 * 1024;
/// Files of known size below this go up in one Put Blob.
pub(crate) const SINGLE_PUT_MAX: u64 = 4 * MIB;
/// Default block size.
pub(crate) const BLOCK_MIN: u64 = 8 * MIB;
/// Largest block the service accepts (version 2019-12-12 and later).
pub(crate) const BLOCK_MAX: u64 = 4000 * MIB;
/// Most blocks in one blob.
pub(crate) const MAX_BLOCKS: u64 = 50_000;
/// Longest blob name, in characters.
const MAX_NAME: usize = 1024;
/// Attempts for one block or the block list on transient failures.
const ATTEMPTS: u32 = 4;

/// The block size for a file of `size` bytes: 8 MiB, or larger so it fits in 50 000 blocks.
pub(crate) fn block_size(size: u64) -> Result<u64> {
    let need = size.div_ceil(MAX_BLOCKS).div_ceil(MIB) * MIB;
    let block = need.max(BLOCK_MIN);
    if block > BLOCK_MAX {
        return Err(Error::new(
            ErrorKind::NoSpace,
            "The file is larger than Azure Blob Storage allows (about 190 TiB).",
        ));
    }
    Ok(block)
}

/// For uploads of unknown size: 8 MiB blocks, doubling every 10 000 blocks, so the 50 000
/// block limit is reached only past ~2.4 TiB.
pub(crate) fn growing_block_size(blocks_so_far: usize) -> u64 {
    let doublings = (blocks_so_far / 10_000).min(9) as u32;
    (BLOCK_MIN << doublings).min(BLOCK_MAX)
}

pub(crate) fn open(r: &AzureRemote, path: &RPath, offset: u64, size: Option<u64>) -> Result<Box<dyn WriteSink>> {
    let what = format!("upload {}", quoted(path));
    if offset > 0 {
        return Err(Error::unsupported("resuming uploads"));
    }
    let Loc::Blob(container, key) = r.loc(path)? else {
        return Err(Error::new(ErrorKind::InvalidPath, format!("Couldn't {what}: files must go inside a container.")));
    };
    if key.chars().count() > MAX_NAME {
        return Err(Error::new(
            ErrorKind::InvalidPath,
            format!("Couldn't {what}: Azure allows at most {MAX_NAME} characters in a full path."),
        ));
    }
    let up = Upload { c: r.c.clone(), container, key, what };
    match size {
        Some(n) if n > 0 && n < SINGLE_PUT_MAX => Ok(Box::new(SingleSink::start(up, n))),
        _ => {
            let fixed = size.map(block_size).transpose()?;
            Ok(Box::new(BlockSink { up, declared: size, fixed, written: 0, buf: Vec::new(), ids: Vec::new(), nonce: nonce(), failed: None }))
        }
    }
}

struct Upload {
    c: Arc<Client>,
    container: String,
    key: String,
    what: String,
}

impl Upload {
    fn call(&self) -> Call<'_> {
        Call::new("PUT", Some(&self.container), Some(&self.key))
    }

    /// Runs `f`, retrying transient failures with backoff (or the server's Retry-After).
    fn retry(&self, mut f: impl FnMut() -> Result<()>) -> Result<()> {
        let mut delay = Duration::from_secs(1);
        let mut attempt = 1;
        loop {
            match f() {
                Err(e) if e.kind.is_transient() && attempt < ATTEMPTS => {
                    self.c.sleep(e.retry_after.unwrap_or(delay).min(Duration::from_secs(60)))?;
                    delay *= 2;
                    attempt += 1;
                }
                other => return other,
            }
        }
    }
}

fn size_changed(what: &str, declared: u64, written: u64) -> Error {
    Error::new(ErrorKind::Other, format!("Couldn't {what}: the file changed size while it was being uploaded."))
        .with_detail(format!("expected {declared} bytes, got {written}"))
}

/// Unique per upload, so two uploads to the same name never mix uncommitted blocks.
fn nonce() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    t ^ (u64::from(std::process::id()) << 32) ^ COUNTER.fetch_add(1, Ordering::Relaxed).rotate_left(48)
}

/// Block ids must all have the same length: 16 hex digits of nonce + 8 digits of index.
pub(crate) fn block_id(nonce: u64, index: usize) -> String {
    STANDARD.encode(format!("{nonce:016x}{index:08}"))
}

/// Single Put Blob, streamed. The request can't complete before `finish`: the last byte is
/// held back until the pipe signals the end, so a dropped sink aborts the request and
/// nothing is committed.
struct SingleSink {
    pipe: Option<UploadPipe<()>>,
    declared: u64,
    written: u64,
    what: String,
}

impl SingleSink {
    fn start(up: Upload, n: u64) -> SingleSink {
        let what = up.what.clone();
        let pipe = upload_pipe(move |reader| {
            let call = up.call().h("x-ms-blob-type", "BlockBlob").h("content-type", "application/octet-stream");
            up.c.send_stream(&call, HoldLast::new(reader), n, &up.what).map(|_| ())
        });
        SingleSink { pipe: Some(pipe), declared: n, written: 0, what }
    }
}

impl Write for SingleSink {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.written + data.len() as u64 > self.declared {
            self.pipe = None; // abort the request
            return Err(to_io(size_changed(&self.what, self.declared, self.written + data.len() as u64)));
        }
        let pipe = self.pipe.as_mut().ok_or_else(|| to_io(Error::cancelled()))?;
        let n = pipe.write(data)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.pipe.as_mut().map_or(Ok(()), |p| p.flush())
    }
}

impl WriteSink for SingleSink {
    fn finish(mut self: Box<Self>) -> Result<()> {
        let pipe = self.pipe.take().ok_or_else(|| size_changed(&self.what, self.declared, self.written))?;
        if self.written != self.declared {
            drop(pipe);
            return Err(size_changed(&self.what, self.declared, self.written));
        }
        pipe.finish()
    }
}

/// Yields everything `inner` yields, but keeps the last byte back until `inner` reports
/// the end, so a fixed-length request body is only complete after a real EOF.
pub(crate) struct HoldLast<R> {
    inner: R,
    held: Option<u8>,
    done: bool,
}

impl<R> HoldLast<R> {
    pub(crate) fn new(inner: R) -> Self {
        HoldLast { inner, held: None, done: false }
    }
}

impl<R: Read> Read for HoldLast<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.done {
            return Ok(0);
        }
        loop {
            match self.held {
                None => {
                    let n = self.inner.read(out)?;
                    if n == 0 {
                        self.done = true;
                        return Ok(0);
                    }
                    self.held = Some(out[n - 1]);
                    if n > 1 {
                        return Ok(n - 1);
                    }
                }
                Some(b) => {
                    let n = if out.len() == 1 {
                        let mut one = [0u8; 1];
                        let n = self.inner.read(&mut one)?;
                        if n == 1 {
                            self.held = Some(one[0]);
                        }
                        n
                    } else {
                        let n = self.inner.read(&mut out[1..])?;
                        if n > 0 {
                            self.held = Some(out[n]);
                        }
                        n
                    };
                    out[0] = b;
                    if n == 0 {
                        self.held = None;
                        self.done = true;
                        return Ok(1);
                    }
                    // out[0] is the old held byte, out[1..n] new bytes; out[n] is now held.
                    return Ok(if out.len() == 1 { 1 } else { n });
                }
            }
        }
    }
}

/// Staged blocks: buffers one block, uploads it with Put Block (retrying transient
/// failures), and commits the list in `finish`. Dropped without `finish`, the staged blocks
/// stay uncommitted and Azure discards them after a week; the blob is never touched.
struct BlockSink {
    up: Upload,
    declared: Option<u64>,
    /// Block size when the total is known.
    fixed: Option<u64>,
    written: u64,
    buf: Vec<u8>,
    ids: Vec<String>,
    nonce: u64,
    /// The first failure; every later call reports it.
    failed: Option<Error>,
}

impl BlockSink {
    fn block_size(&self) -> u64 {
        self.fixed.unwrap_or_else(|| growing_block_size(self.ids.len()))
    }

    fn put_block(&mut self) -> Result<()> {
        if self.ids.len() as u64 >= MAX_BLOCKS {
            return Err(Error::new(
                ErrorKind::NoSpace,
                format!("Couldn't {}: the file is larger than Azure Blob Storage allows.", self.up.what),
            ));
        }
        let id = block_id(self.nonce, self.ids.len());
        let up = &self.up;
        let buf = &self.buf;
        up.retry(|| {
            let call = up
                .call()
                .q("comp", "block")
                .q("blockid", id.clone())
                .h("content-type", "application/octet-stream");
            up.c.send(&call, buf, &up.what).map(|_| ())
        })?;
        self.ids.push(id);
        self.buf.clear();
        Ok(())
    }

    fn fail(&mut self, e: Error) -> io::Error {
        self.failed = Some(e.clone());
        to_io(e)
    }
}

impl Write for BlockSink {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if let Some(e) = &self.failed {
            return Err(to_io(e.clone()));
        }
        if let Err(e) = self.up.c.check_open() {
            return Err(self.fail(e));
        }
        if let Some(d) = self.declared
            && self.written + data.len() as u64 > d {
                let e = size_changed(&self.up.what, d, self.written + data.len() as u64);
                return Err(self.fail(e));
            }
        let room = (self.block_size() as usize).saturating_sub(self.buf.len());
        let n = data.len().min(room);
        self.buf.extend_from_slice(&data[..n]);
        self.written += n as u64;
        if self.buf.len() as u64 >= self.block_size()
            && let Err(e) = self.put_block() {
                return Err(self.fail(e));
            }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        match &self.failed {
            Some(e) => Err(to_io(e.clone())),
            None => Ok(()),
        }
    }
}

impl WriteSink for BlockSink {
    fn finish(mut self: Box<Self>) -> Result<()> {
        if let Some(e) = self.failed.take() {
            return Err(e);
        }
        self.up.c.check_open()?;
        if let Some(d) = self.declared
            && self.written != d {
                return Err(size_changed(&self.up.what, d, self.written));
            }
        if self.ids.is_empty() && self.buf.is_empty() {
            // An empty file: one Put Blob with no body.
            let up = &self.up;
            return up.retry(|| {
                let call = up.call().h("x-ms-blob-type", "BlockBlob").h("content-type", "application/octet-stream");
                up.c.send(&call, &[], &up.what).map(|_| ())
            });
        }
        if !self.buf.is_empty() {
            self.put_block()?;
        }
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList>");
        for id in &self.ids {
            xml.push_str("<Latest>");
            xml.push_str(id);
            xml.push_str("</Latest>");
        }
        xml.push_str("</BlockList>");
        let up = &self.up;
        up.retry(|| {
            let call = up
                .call()
                .q("comp", "blocklist")
                .h("content-type", "application/xml")
                .h("x-ms-blob-content-type", "application/octet-stream");
            up.c.send(&call, xml.as_bytes(), &up.what).map(|_| ())
        })
    }
}

