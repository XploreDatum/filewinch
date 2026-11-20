//! Download bodies shared by S3 and B2: ranged-response checks and error mapping on reads.

use std::io::{self, Read};

use filewinch_core::{Error, ErrorKind, Result};
use filewinch_http::ureq::{Body, http::Response};

/// A read failure as a core error naming the host.
pub(crate) fn body_error(e: io::Error, what: &str, host: &str) -> Error {
    let err = filewinch_http::from_io(e);
    if err.kind == ErrorKind::Network || err.kind == ErrorKind::Other {
        let mut mapped = Error::new(ErrorKind::Network, format!("Couldn't {what}: the connection to {host} was lost."));
        mapped.detail = err.detail.or(Some(err.message));
        return mapped;
    }
    err
}

fn header<'a>(resp: &'a Response<Body>, name: &str) -> Option<&'a str> {
    resp.headers().get(name).and_then(|v| v.to_str().ok())
}

/// Turns a successful GET (sent with `Range: bytes=offset-` when `offset > 0`) into a reader
/// positioned at `offset`. 206 is the norm; some servers (rclone) answer 200 with the right
/// Content-Range; a server that ignored the range sends everything, so the start is skipped.
pub(crate) fn ranged_reader(resp: Response<Body>, offset: u64, what: String, host: String) -> Result<Box<dyn Read + Send>> {
    let status = resp.status().as_u16();
    let len = header(&resp, "content-length").and_then(|v| v.trim().parse::<u64>().ok());
    let ranged = status == 206
        || header(&resp, "content-range")
            .is_some_and(|v| v.trim().strip_prefix("bytes ").is_some_and(|r| r.starts_with(&format!("{offset}-"))));
    let skip = if offset > 0 && !ranged { offset } else { 0 };
    let mut reader = BodyReader { inner: filewinch_http::body_reader(resp), left: len, what, host };
    if skip > 0 {
        let skipped = io::copy(&mut (&mut reader).take(skip), &mut io::sink()).map_err(filewinch_http::from_io)?;
        if skipped < skip {
            return Err(Error::new(ErrorKind::InvalidPath, "The file is shorter than the position to resume from."));
        }
    }
    Ok(Box::new(reader))
}

/// Maps read failures to core errors (inside `io::Error`, see `filewinch_http::to_io`) and
/// treats a body that ends before its Content-Length as a dropped connection.
struct BodyReader {
    inner: Box<dyn Read + Send>,
    left: Option<u64>,
    what: String,
    host: String,
}

impl Read for BodyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let n = self.inner.read(buf).map_err(|e| filewinch_http::to_io(body_error(e, &self.what, &self.host)))?;
        if let Some(left) = &mut self.left {
            if n == 0 && *left > 0 {
                return Err(filewinch_http::to_io(Error::new(
                    ErrorKind::Network,
                    format!("Couldn't {}: the connection to {} was lost.", self.what, self.host),
                )));
            }
            *left = left.saturating_sub(n as u64);
        }
        Ok(n)
    }
}
