//! Streaming uploads: the caller writes into an [`UploadPipe`] while a worker thread sends
//! the bytes as an HTTP request body, with bounded memory and backpressure.

use std::io::{self, Read, Write};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;

use filewinch_core::{Error, ErrorKind, Result};

use crate::errors::{from_io, to_io};

/// Bytes per chunk handed to the worker.
pub const CHUNK_SIZE: usize = 256 * 1024;
/// Chunks in flight between the writer and the worker (so at most ~1.25 MiB is buffered).
pub const CHANNEL_CHUNKS: usize = 4;

enum Msg {
    Data(Vec<u8>),
    Eof,
}

/// The worker's side of an [`UploadPipe`]: a `Read` that yields the written bytes, then EOF
/// after `finish`, or an error ("upload cancelled") if the pipe was dropped without `finish`,
/// which makes ureq abort the request mid-body so the server never sees a complete body.
pub struct PipeReader {
    rx: Receiver<Msg>,
    chunk: Vec<u8>,
    pos: usize,
    eof: bool,
}

impl Read for PipeReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            if self.pos < self.chunk.len() {
                let n = out.len().min(self.chunk.len() - self.pos);
                out[..n].copy_from_slice(&self.chunk[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            if self.eof {
                return Ok(0);
            }
            match self.rx.recv() {
                Ok(Msg::Data(chunk)) => {
                    self.chunk = chunk;
                    self.pos = 0;
                }
                Ok(Msg::Eof) => self.eof = true,
                Err(_) => return Err(to_io(Error::new(ErrorKind::Cancelled, "Upload cancelled."))),
            }
        }
    }
}

/// The caller's side of a streaming upload. See [`upload_pipe`].
pub struct UploadPipe<T> {
    tx: Option<SyncSender<Msg>>,
    buf: Vec<u8>,
    worker: Option<JoinHandle<Result<T>>>,
    /// Set once the worker has been joined.
    ended: Option<Result<T>>,
}

/// Starts a streaming upload. `start(reader)` runs on a spawned worker thread and performs
/// the HTTP request with `reader` as the body (see [`crate::send_reader`]). Write the file
/// into the returned pipe, then call [`UploadPipe::finish`] for the worker's result.
///
/// Bytes are buffered into 256 KiB chunks and passed through a channel holding at most 4
/// chunks, so a slow server slows the writer down instead of growing memory.
///
/// If the worker ends early (the server rejected the upload, e.g. 401 or 507 after
/// `Expect: 100-continue`), the next `write` returns `to_io(that error)` and `finish`
/// returns it. Dropping the pipe without `finish` cancels: the reader errors, ureq aborts
/// the request without completing the body, and the drop returns at once (the worker is
/// detached, not joined).
pub fn upload_pipe<T: Send + 'static>(
    start: impl FnOnce(PipeReader) -> Result<T> + Send + 'static,
) -> UploadPipe<T> {
    let (tx, rx) = sync_channel(CHANNEL_CHUNKS);
    let reader = PipeReader {
        rx,
        chunk: Vec::new(),
        pos: 0,
        eof: false,
    };
    let worker = std::thread::Builder::new()
        .name("filewinch-upload".into())
        .spawn(move || start(reader));
    match worker {
        Ok(handle) => UploadPipe {
            tx: Some(tx),
            buf: Vec::with_capacity(CHUNK_SIZE),
            worker: Some(handle),
            ended: None,
        },
        Err(e) => UploadPipe {
            tx: None,
            buf: Vec::new(),
            worker: None,
            ended: Some(Err(Error::new(
                ErrorKind::Other,
                "Couldn't start the upload.",
            )
            .with_detail(e))),
        },
    }
}

impl<T> UploadPipe<T> {
    /// Flushes, signals EOF, waits for the worker and returns its result.
    pub fn finish(mut self) -> Result<T> {
        if self.ended.is_none()
            && self.send_buf().is_ok()
            && let Some(tx) = self.tx.take()
            && tx.send(Msg::Eof).is_err()
        {
            // The worker ended without reading to the end: report why, or that the server
            // stopped early if it claims success.
            return Err(from_io(self.worker_ended()));
        }
        self.tx = None;
        self.join();
        self.ended.take().unwrap_or_else(|| Err(ended_early()))
    }

    fn send_buf(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let Some(tx) = &self.tx else {
            return Err(self.worker_ended());
        };
        let chunk = std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK_SIZE));
        if tx.send(Msg::Data(chunk)).is_err() {
            return Err(self.worker_ended());
        }
        Ok(())
    }

    /// The worker dropped its reader: join it and turn its outcome into the write error.
    fn worker_ended(&mut self) -> io::Error {
        self.tx = None;
        self.join();
        if let Some(Ok(_)) = &self.ended {
            // The server answered before taking all the data; that isn't a complete upload.
            self.ended = Some(Err(ended_early()));
        }
        match &self.ended {
            Some(Err(e)) => to_io(e.clone()),
            _ => to_io(ended_early()),
        }
    }

    fn join(&mut self) {
        if let Some(handle) = self.worker.take() {
            self.ended = Some(handle.join().unwrap_or_else(|_| {
                Err(Error::new(
                    ErrorKind::Other,
                    "The upload stopped unexpectedly.",
                ))
            }));
        }
    }
}

fn ended_early() -> Error {
    Error::new(
        ErrorKind::Network,
        "The server ended the upload before all the data was sent.",
    )
}

impl<T> Write for UploadPipe<T> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if let Some(Err(e)) = &self.ended {
            return Err(to_io(e.clone()));
        }
        if self.tx.is_none() {
            return Err(self.worker_ended());
        }
        let n = data.len().min(CHUNK_SIZE - self.buf.len());
        self.buf.extend_from_slice(&data[..n]);
        if self.buf.len() == CHUNK_SIZE {
            self.send_buf()?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(Err(e)) = &self.ended {
            return Err(to_io(e.clone()));
        }
        self.send_buf()
    }
}

impl<T> Drop for UploadPipe<T> {
    fn drop(&mut self) {
        // Dropping the sender without `Eof` makes the reader fail, so ureq aborts the
        // request. Dropping the JoinHandle detaches the worker; never block here.
        self.tx = None;
        self.worker = None;
    }
}
