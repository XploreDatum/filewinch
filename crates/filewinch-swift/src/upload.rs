//! Uploads: one streamed PUT, or a Static Large Object (segments + manifest) for large or
//! unknown-size files. See the crate docs for the layout and cleanup rules.

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use filewinch_core::{Error, ErrorKind, Result, WriteSink};
use filewinch_http::{PipeReader, UploadPipe, send_reader, to_io, upload_pipe};

use crate::client::{Client, enc, etag, url_path};

/// Swift's default `max_manifest_segments`.
const MAX_SEGMENTS: u64 = 1000;
/// Swift's default largest single object (and so largest segment).
const MAX_OBJECT: u64 = 5 * 1024 * 1024 * 1024;

/// Holds back the end of a fixed-length body until the writer calls `finish` (the pipe's
/// EOF): dropping the sink after writing every byte must not commit the object.
struct GuardEnd {
    inner: PipeReader,
    left: u64,
}

impl Read for GuardEnd {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = if self.left == 0 || buf.is_empty() {
            0
        } else {
            let max = buf.len().min(usize::try_from(self.left).unwrap_or(usize::MAX));
            self.inner.read(&mut buf[..max])?
        };
        self.left -= n as u64;
        if self.left == 0 {
            let mut probe = [0u8; 1];
            if self.inner.read(&mut probe)? != 0 {
                return Err(to_io(Error::new(ErrorKind::Other, "More data was written than the file's declared size.")));
            }
        }
        Ok(n)
    }
}

/// Streams one object body (`len` known → Content-Length, else chunked) and returns the
/// server's ETag. Runs on the upload pipe's worker thread.
fn put_stream(client: &Client, path: &str, reader: PipeReader, len: Option<u64>, what: &str) -> Result<Option<String>> {
    let cur = client.current()?;
    let req = client
        .http
        .agent()
        .put(format!("{}{path}", cur.s.storage_url))
        .header("x-auth-token", cur.s.token.as_str());
    let resp = match len {
        Some(n) => send_reader(req, GuardEnd { inner: reader, left: n }, Some(n)),
        None => send_reader(req, reader, None),
    }
    .map_err(|e| client.transport(e, &cur))?;
    match resp.status().as_u16() {
        200 | 201 => Ok(etag(&resp)),
        401 => Err(client.expired_mid_stream(cur.generation, what)),
        _ => Err(client.fail(resp, what)),
    }
}

/// One object being streamed, with its MD5 computed on the way.
struct Stream {
    pipe: UploadPipe<Option<String>>,
    md5: md5::Context,
    written: u64,
    /// Bytes this stream takes; `None` for chunked.
    len: Option<u64>,
}

impl Stream {
    fn start(client: &Arc<Client>, path: String, len: Option<u64>, what: String) -> Stream {
        let c = client.clone();
        let pipe = upload_pipe(move |r| put_stream(&c, &path, r, len, &what));
        Stream { pipe, md5: md5::Context::new(), written: 0, len }
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<()> {
        self.pipe.write_all(buf)?;
        self.md5.consume(buf);
        self.written += buf.len() as u64;
        Ok(())
    }

    /// Commits. Returns (local MD5, server ETag, size); see [`checksum`].
    fn finish(self) -> Result<(String, Option<String>, u64)> {
        let local = format!("{:x}", self.md5.finalize());
        let remote = self.pipe.finish()?;
        Ok((local, remote, self.written))
    }
}

/// Fails if the server's ETag isn't the MD5 of what was sent (the object is then damaged).
fn checksum(local: &str, remote: Option<&str>, what: &str) -> Result<()> {
    match remote {
        Some(remote) if remote != local => Err(Error::new(
            ErrorKind::Network,
            format!("Couldn't {what}: the data arrived damaged (checksum mismatch)."),
        )
        .with_detail(format!("sent MD5 {local}, server has {remote}"))),
        _ => Ok(()),
    }
}

/// A file uploaded with one PUT.
pub(crate) struct SingleSink {
    client: Arc<Client>,
    path: String,
    stream: Stream,
    what: String,
}

impl SingleSink {
    pub fn new(client: &Arc<Client>, container: &str, key: &str, size: u64, what: String) -> SingleSink {
        let path = url_path(container, Some(key));
        let stream = Stream::start(client, path.clone(), Some(size), what.clone());
        SingleSink { client: client.clone(), path, stream, what }
    }
}

impl Write for SingleSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(len) = self.stream.len
            && self.stream.written + buf.len() as u64 > len
        {
            return Err(too_much());
        }
        self.stream.write(buf)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.pipe.flush()
    }
}

impl WriteSink for SingleSink {
    fn finish(self: Box<Self>) -> Result<()> {
        let this = *self;
        let (local, remote, _) = this.stream.finish()?;
        let r = checksum(&local, remote.as_deref(), &this.what);
        if r.is_err() {
            // Don't leave a damaged file behind (best effort).
            let _ = this.client.send("DELETE", &this.path, &[], None, &this.what);
        }
        r
    }
}

fn too_much() -> io::Error {
    to_io(Error::new(ErrorKind::Other, "More data was written than the file's declared size."))
}

struct Segment {
    /// `<segments container>/<segment name>`, unencoded (manifest form).
    path: String,
    etag: String,
    size: u64,
}

/// A file uploaded as a Static Large Object.
pub(crate) struct SloSink {
    client: Arc<Client>,
    container: String,
    key: String,
    seg_container: String,
    seg_prefix: String,
    seg_size: u64,
    total: Option<u64>,
    written: u64,
    done: Vec<Segment>,
    /// Segments whose upload failed after reaching the server (bad checksum): cleaned up.
    stray: Vec<String>,
    current: Option<Stream>,
    seg_container_ready: bool,
    finished: bool,
    what: String,
}

impl SloSink {
    pub fn new(client: &Arc<Client>, container: &str, key: &str, total: Option<u64>, what: String) -> Result<SloSink> {
        let mut seg_size = client.tuning.segment_size.max(1);
        if let Some(t) = total {
            seg_size = seg_size.max(t.div_ceil(MAX_SEGMENTS));
            if seg_size > MAX_OBJECT {
                return Err(Error::new(
                    ErrorKind::NoSpace,
                    format!("Couldn't {what}: the file is larger than Swift allows (1000 segments of 5 GB)."),
                ));
            }
        }
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let size_part = total.map_or_else(|| "unknown".to_string(), |t| t.to_string());
        Ok(SloSink {
            client: client.clone(),
            container: container.to_string(),
            key: key.to_string(),
            seg_container: format!("{container}_segments"),
            seg_prefix: format!(
                "{key}/slo/{}.{:06}/{size_part}/{seg_size}/",
                stamp.as_secs(),
                stamp.subsec_micros()
            ),
            seg_size,
            total,
            written: 0,
            done: Vec::new(),
            stray: Vec::new(),
            current: None,
            seg_container_ready: false,
            finished: false,
            what,
        })
    }

    fn segment_name(&self, n: usize) -> String {
        format!("{}{n:08}", self.seg_prefix)
    }

    fn ensure_seg_container(&mut self) -> Result<()> {
        if self.seg_container_ready {
            return Ok(());
        }
        let what = format!("create the container “{}” for large-file segments", self.seg_container);
        let resp = self.client.send("PUT", &url_path(&self.seg_container, None), &[], Some(b""), &what)?;
        if !resp.status().is_success() {
            return Err(self.client.fail(resp, &what));
        }
        self.seg_container_ready = true;
        Ok(())
    }

    fn start_segment(&mut self) -> Result<()> {
        if self.done.len() as u64 >= MAX_SEGMENTS {
            return Err(Error::new(
                ErrorKind::NoSpace,
                format!("Couldn't {}: the file is larger than Swift allows (1000 segments).", self.what),
            ));
        }
        self.ensure_seg_container()?;
        let len = self.total.map(|t| (t - self.written).min(self.seg_size));
        let name = self.segment_name(self.done.len());
        let what = format!("{} (part {})", self.what, self.done.len() + 1);
        self.current = Some(Stream::start(&self.client, url_path(&self.seg_container, Some(&name)), len, what));
        Ok(())
    }

    fn close_segment(&mut self) -> Result<()> {
        let Some(s) = self.current.take() else { return Ok(()) };
        let n = self.done.len();
        let path = format!("{}/{}", self.seg_container, self.segment_name(n));
        let what = format!("{} (part {})", self.what, n + 1);
        match s.finish().and_then(|(etag, remote, size)| checksum(&etag, remote.as_deref(), &what).map(|_| (etag, size))) {
            Ok((etag, size)) => {
                self.done.push(Segment { path, etag, size });
                Ok(())
            }
            Err(e) => {
                self.stray.push(path);
                Err(e)
            }
        }
    }

    fn commit(&mut self) -> Result<()> {
        self.close_segment()?;
        if let Some(t) = self.total
            && self.written != t
        {
            return Err(Error::new(
                ErrorKind::Other,
                format!("Couldn't {}: the data ended before the file's declared size.", self.what),
            ));
        }
        let target = url_path(&self.container, Some(&self.key));
        if self.done.is_empty() {
            // Unknown size and nothing written: an empty object.
            let resp = self.client.send("PUT", &target, &[], Some(b""), &self.what)?;
            return if resp.status().is_success() { Ok(()) } else { Err(self.client.fail(resp, &self.what)) };
        }
        if self.total.is_none() && self.done.len() == 1 {
            // Small after all: copy the one segment into place, keep it a plain object.
            let src = format!("/{}", self.done[0].path);
            let src = src.split('/').map(enc).collect::<Vec<_>>().join("/");
            let resp = self.client.send("PUT", &target, &[("x-copy-from", &src)], Some(b""), &self.what)?;
            if !resp.status().is_success() {
                return Err(self.client.fail(resp, &self.what));
            }
            delete_segments(&self.client, &self.done.drain(..).map(|s| s.path).collect::<Vec<_>>());
            return Ok(());
        }
        let manifest: Vec<serde_json::Value> = self
            .done
            .iter()
            .map(|s| serde_json::json!({ "path": format!("/{}", s.path), "etag": s.etag, "size_bytes": s.size }))
            .collect();
        let body = serde_json::Value::Array(manifest).to_string();
        let resp = self.client.send(
            "PUT",
            &format!("{target}?multipart-manifest=put"),
            &[("content-type", "application/json")],
            Some(body.as_bytes()),
            &self.what,
        )?;
        if !resp.status().is_success() {
            return Err(self.client.fail(resp, &self.what));
        }
        Ok(())
    }

    /// Deletes every segment written so far, including one being streamed, on a
    /// background thread (best effort).
    fn abandon(&mut self) {
        if self.current.take().is_some() {
            let n = self.done.len();
            self.stray.push(format!("{}/{}", self.seg_container, self.segment_name(n)));
        }
        let mut paths: Vec<String> = self.done.drain(..).map(|s| s.path).collect();
        paths.append(&mut self.stray);
        if paths.is_empty() {
            return;
        }
        let client = self.client.clone();
        let _ = std::thread::Builder::new()
            .name("filewinch-swift-cleanup".into())
            .spawn(move || delete_segments(&client, &paths));
    }
}

/// Deletes segments (`container/name` paths), ignoring failures.
fn delete_segments(client: &Client, paths: &[String]) {
    for p in paths {
        let Some((c, k)) = p.split_once('/') else { continue };
        let _ = client.send("DELETE", &url_path(c, Some(k)), &[], None, "remove an unused file segment");
    }
}

impl Write for SloSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if let Some(t) = self.total
            && self.written + buf.len() as u64 > t
        {
            return Err(too_much());
        }
        if self.current.is_none() {
            self.start_segment().map_err(to_io)?;
        }
        let Some(cur) = self.current.as_mut() else { return Err(io::Error::other("no segment")) };
        let room = cur.len.unwrap_or(self.seg_size) - cur.written;
        let n = buf.len().min(usize::try_from(room).unwrap_or(usize::MAX));
        cur.write(&buf[..n])?;
        self.written += n as u64;
        if cur.written == cur.len.unwrap_or(self.seg_size) {
            self.close_segment().map_err(to_io)?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.current.as_mut() {
            Some(s) => s.pipe.flush(),
            None => Ok(()),
        }
    }
}

impl WriteSink for SloSink {
    fn finish(mut self: Box<Self>) -> Result<()> {
        let r = self.commit();
        self.finished = r.is_ok();
        r
    }
}

impl Drop for SloSink {
    fn drop(&mut self) {
        if !self.finished {
            self.abandon();
        }
    }
}
