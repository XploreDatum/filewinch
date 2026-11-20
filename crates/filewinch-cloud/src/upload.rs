//! Turns a provider's "one request" and "session" upload calls into a streaming [`WriteSink`].
//!
//! Small files are buffered and sent in one request at `finish`. Once a file is known (or
//! turns out) to be larger than `simple_max`, a session opens and full chunks stream out as
//! they fill; `finish` sends the final chunk and commits. Dropping the sink before `finish`
//! (or any failed chunk) aborts the session so no half-written file is committed.

use std::io::{self, Write};

use filewinch_core::{Error, ErrorKind, Result, WriteSink};

use crate::setup::Tuning;

pub(crate) trait UploadTarget: Send {
    /// Uploads a whole file of at most `simple_max` bytes in one request.
    fn simple(&mut self, data: Vec<u8>) -> Result<()>;
    /// Opens a session. `size` is the final size when known. Returns the chunk size to use
    /// (the provider's multiple, or the part size the server dictates).
    fn begin(&mut self, size: Option<u64>) -> Result<usize>;
    /// Sends bytes at `offset`. Every chunk but the last is exactly the chunk size; `last`
    /// means `offset + data.len()` is the final size and the target must commit.
    fn chunk(&mut self, offset: u64, data: &[u8], last: bool) -> Result<()>;
    /// Best-effort cancel of an open session (drop without `finish`, or after an error).
    fn abort(&mut self);
}

pub(crate) struct ChunkedSink<T: UploadTarget> {
    target: T,
    buf: Vec<u8>,
    simple_max: u64,
    chunk: usize,
    size: Option<u64>,
    sent: u64,
    in_session: bool,
    done: bool,
    failed: Option<Error>,
}

impl<T: UploadTarget> ChunkedSink<T> {
    /// Opens the session right away when `size` already says the file is large, so a refusal
    /// (quota, permissions) shows up before any bytes are read.
    pub(crate) fn new(target: T, tuning: &Tuning, size: Option<u64>) -> Result<Self> {
        let mut sink = ChunkedSink {
            target,
            buf: Vec::new(),
            simple_max: tuning.simple_max,
            chunk: tuning.chunk.max(1),
            size,
            sent: 0,
            in_session: false,
            done: false,
            failed: None,
        };
        if size.is_some_and(|s| s > sink.simple_max) {
            sink.start_session()?;
        }
        Ok(sink)
    }

    fn start_session(&mut self) -> Result<()> {
        self.chunk = self.target.begin(self.size)?.max(1);
        self.in_session = true;
        Ok(())
    }

    fn fail(&mut self, e: Error) -> io::Error {
        if self.in_session {
            self.target.abort();
            self.in_session = false;
        }
        self.done = true;
        self.failed = Some(e.clone());
        io::Error::other(e)
    }

    fn pump(&mut self) -> Result<()> {
        if !self.in_session && self.buf.len() as u64 > self.simple_max {
            self.start_session()?;
        }
        // Keep at least one byte back so the final chunk is always sent by `finish`.
        while self.in_session && self.buf.len() > self.chunk {
            self.target
                .chunk(self.sent, &self.buf[..self.chunk], false)?;
            self.buf.drain(..self.chunk);
            self.sent += self.chunk as u64;
        }
        Ok(())
    }
}

impl<T: UploadTarget> Write for ChunkedSink<T> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if let Some(e) = &self.failed {
            return Err(io::Error::other(e.clone()));
        }
        if self.done {
            return Err(io::Error::other(Error::new(
                ErrorKind::Closed,
                "This upload already finished.",
            )));
        }
        self.buf.extend_from_slice(data);
        match self.pump() {
            Ok(()) => Ok(data.len()),
            Err(e) => Err(self.fail(e)),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<T: UploadTarget + 'static> WriteSink for ChunkedSink<T> {
    fn finish(mut self: Box<Self>) -> Result<()> {
        if let Some(e) = self.failed.take() {
            return Err(e);
        }
        let total = self.sent + self.buf.len() as u64;
        if let Some(expected) = self.size.filter(|&n| n != total) {
            let e = Error::new(
                ErrorKind::Other,
                format!("The upload ended after {total} bytes, but the file has {expected}."),
            );
            self.fail(e.clone());
            return Err(e);
        }
        let data = std::mem::take(&mut self.buf);
        let result = if self.in_session {
            self.target.chunk(self.sent, &data, true)
        } else {
            self.target.simple(data)
        };
        match result {
            Ok(()) => {
                self.done = true;
                Ok(())
            }
            Err(e) => {
                self.fail(e.clone());
                Err(e)
            }
        }
    }
}

impl<T: UploadTarget> Drop for ChunkedSink<T> {
    fn drop(&mut self) {
        if !self.done && self.in_session {
            self.target.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Log(Vec<String>);

    struct Fake(Arc<Mutex<Log>>, usize);

    impl UploadTarget for Fake {
        fn simple(&mut self, data: Vec<u8>) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .0
                .push(format!("simple {}", data.len()));
            Ok(())
        }
        fn begin(&mut self, size: Option<u64>) -> Result<usize> {
            self.0.lock().unwrap().0.push(format!("begin {size:?}"));
            Ok(self.1)
        }
        fn chunk(&mut self, offset: u64, data: &[u8], last: bool) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .0
                .push(format!("chunk {offset}+{} {last}", data.len()));
            Ok(())
        }
        fn abort(&mut self) {
            self.0.lock().unwrap().0.push("abort".into());
        }
    }

    fn tuning(simple_max: u64) -> Tuning {
        Tuning {
            simple_max,
            ..Tuning::for_provider(crate::Provider::Dropbox)
        }
    }

    fn run(size: Option<u64>, data: usize, finish: bool) -> Vec<String> {
        let log = Arc::new(Mutex::new(Log::default()));
        let mut sink = Box::new(ChunkedSink::new(Fake(log.clone(), 4), &tuning(5), size).unwrap());
        for b in vec![7u8; data].chunks(3) {
            sink.write_all(b).unwrap();
        }
        if finish {
            sink.finish().unwrap();
        } else {
            drop(sink);
        }
        log.lock().unwrap().0.clone()
    }

    #[test]
    fn small_file_is_one_request() {
        assert_eq!(run(None, 5, true), ["simple 5"]);
        assert_eq!(run(Some(0), 0, true), ["simple 0"]);
    }

    #[test]
    fn large_file_streams_chunks_and_commits_last() {
        assert_eq!(
            run(None, 12, true),
            [
                "begin None",
                "chunk 0+4 false",
                "chunk 4+4 false",
                "chunk 8+4 true"
            ]
        );
        assert_eq!(
            run(Some(9), 9, true),
            [
                "begin Some(9)",
                "chunk 0+4 false",
                "chunk 4+4 false",
                "chunk 8+1 true"
            ]
        );
    }

    #[test]
    fn drop_without_finish_aborts_session() {
        assert_eq!(
            run(None, 9, false).last().map(String::as_str),
            Some("abort")
        );
        assert!(!run(None, 3, false).contains(&"abort".to_string()));
    }

    #[test]
    fn short_file_fails_and_aborts() {
        let log = Arc::new(Mutex::new(Log::default()));
        let mut sink =
            Box::new(ChunkedSink::new(Fake(log.clone(), 4), &tuning(5), Some(20)).unwrap());
        sink.write_all(&[1; 6]).unwrap();
        assert!(sink.finish().is_err());
        assert_eq!(
            log.lock().unwrap().0.last().map(String::as_str),
            Some("abort")
        );
    }
}
