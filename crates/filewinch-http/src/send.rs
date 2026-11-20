//! Sending a streamed request body with or without a known length.

use std::collections::HashSet;
use std::io::{self, Read};
use std::sync::{Mutex, OnceLock};

use filewinch_core::{Error, ErrorKind};
use ureq::http::Response;
use ureq::{Body, RequestBuilder, SendBody, typestate::WithBody};

use crate::errors::{from_io, to_io};

/// Sends `reader` as the request body.
///
/// - `len: Some(n)` sets `Content-Length: n` and sends exactly `n` bytes (S3 and Azure
///   require this and reject chunked encoding). If the reader ends early the request is
///   aborted (the server never sees a complete body) and the error says so; bytes past
///   `n` are left unread.
/// - `len: None` sends `Transfer-Encoding: chunked` (WebDAV and most servers accept it).
///
/// It also sends `Expect: 100-continue`, so a server that rejects the request up front
/// (401, 403, 413, 507...) answers before any body bytes are sent and the caller gets that
/// status instead of a broken-pipe error. ureq waits at most 1 s for the interim `100`
/// before sending the body anyway, so servers that ignore `Expect` still work. A server whose
/// `100 Continue` carries headers (legal, but ureq rejects it) fails the first upload with a
/// retryable `Network` error; later uploads to that host:port go without `Expect`.
pub fn send_reader(
    req: RequestBuilder<WithBody>,
    reader: impl Read + Send,
    len: Option<u64>,
) -> Result<Response<Body>, ureq::Error> {
    let authority = req.uri_ref().and_then(|u| u.authority()).map(|a| a.to_string());
    let use_expect = authority.as_deref().is_none_or(|a| !no_expect().lock().unwrap().contains(a));
    let req = if use_expect { req.header("expect", "100-continue") } else { req };
    let result = send_body(req, reader, len);
    if let (Err(ureq::Error::Protocol(ureq_proto::Error::HeadersWith100)), Some(a)) =
        (&result, authority)
    {
        // Nothing of the body was sent; the caller gets a retryable Network error and the
        // retry goes out without `Expect`.
        no_expect().lock().unwrap().insert(a);
    }
    result
}

/// Servers (host:port) that answered `Expect: 100-continue` with a `100` carrying headers,
/// which ureq can't parse. Uploads to them skip `Expect`.
fn no_expect() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(Default::default)
}

fn send_body(
    req: RequestBuilder<WithBody>,
    reader: impl Read + Send,
    len: Option<u64>,
) -> Result<Response<Body>, ureq::Error> {
    match len {
        Some(n) => {
            let mut exact = ExactLen {
                inner: reader,
                left: n,
            };
            req.header("content-length", n)
                .send(SendBody::from_reader(&mut exact))
        }
        None => {
            let mut reader = reader;
            req.send(SendBody::from_reader(&mut reader))
        }
    }
}

/// Yields at most `left` bytes of `inner`, erroring if it ends early.
struct ExactLen<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for ExactLen<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            // ureq stops reading at Content-Length; any extra bytes stay unread.
            return Ok(0);
        }
        let max = buf
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..max])?;
        if n == 0 {
            return Err(to_io(Error::new(
                ErrorKind::Other,
                "The data to upload ended before its declared size.",
            )));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// The response body as a `'static` reader for `Remote::read`. Read errors (idle timeouts,
/// dropped connections, truncated bodies) come out as io errors wrapping a core `Error` with
/// kind `Network`, so the engine's `?` conversion keeps them retryable instead of `Other`.
pub fn body_reader(resp: Response<Body>) -> Box<dyn Read + Send> {
    Box::new(MappedReader(resp.into_body().into_reader()))
}

struct MappedReader<R>(R);

impl<R: Read> Read for MappedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf).map_err(|e| to_io(from_io(e)))
    }
}
