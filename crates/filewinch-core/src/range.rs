//! Byte ranges over HTTP, shared by every HTTP backend's [`crate::Remote::read_range`]: the
//! `Range` header, the check that the reply really is that range, and a reader that yields
//! exactly the promised bytes.

use crate::{Error, ErrorKind, Result};
use std::io::{self, Read};

/// `bytes=a-b` (inclusive) for `len > 0` bytes from `offset`.
pub fn range_header(offset: u64, len: u64) -> String {
    format!("bytes={offset}-{}", offset + len.max(1) - 1)
}

/// Checks the reply to a GET sent with [`range_header`]`(offset, len)` and returns how many
/// bytes its body carries: `len`, or fewer when the file ends inside the range.
///
/// Accepts 206, and 200 when it carries the matching `Content-Range` (some servers, rclone's
/// S3 among them, answer that way). A 200 without one means the server ignored the range and
/// is sending the whole file: [`Error::range_ignored`]. Call it only for 2xx replies; map
/// errors (416 included) the backend's usual way first.
pub fn check_range_reply(status: u16, content_range: Option<&str>, offset: u64, len: u64) -> Result<u64> {
    let Some(cr) = content_range else {
        let what = if status == 206 { "206 without Content-Range" } else { "HTTP 200 without Content-Range" };
        return Err(Error::range_ignored(what));
    };
    if status != 200 && status != 206 {
        return Err(Error::range_ignored(format!("HTTP {status}")));
    }
    let bad = || Error::range_ignored(format!("asked for {}, got Content-Range: {cr}", range_header(offset, len)));
    let rest = cr.trim().strip_prefix("bytes ").ok_or_else(bad)?;
    let (span, total) = rest.split_once('/').ok_or_else(bad)?;
    let (a, b) = span.split_once('-').ok_or_else(bad)?;
    let (a, b): (u64, u64) = (a.trim().parse().map_err(|_| bad())?, b.trim().parse().map_err(|_| bad())?);
    let total: Option<u64> = total.trim().parse().ok();
    if a != offset || b < a {
        return Err(bad());
    }
    let n = b - a + 1;
    if n == len || (n < len && total == Some(b + 1)) {
        Ok(n)
    } else {
        Err(bad())
    }
}

/// Wraps a body so it yields exactly `len` bytes: it stops there, and a body that ends sooner
/// is a dropped connection (kind `Network`, so the engine retries the rest).
pub fn exact_reader(inner: Box<dyn Read + Send>, len: u64, what: &str) -> Box<dyn Read + Send> {
    Box::new(Exact { inner, left: len, what: what.to_owned() })
}

struct Exact {
    inner: Box<dyn Read + Send>,
    left: u64,
    what: String,
}

impl Read for Exact {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 || buf.is_empty() {
            return Ok(0);
        }
        let want = buf.len().min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            let e = Error::new(ErrorKind::Network, format!("The connection closed before all of {} arrived.", self.what))
                .with_detail(format!("{} bytes missing", self.left));
            return Err(io::Error::other(e));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_inclusive() {
        assert_eq!(range_header(0, 1), "bytes=0-0");
        assert_eq!(range_header(100, 50), "bytes=100-149");
    }

    #[test]
    fn partial_reply_must_match() {
        assert_eq!(check_range_reply(206, Some("bytes 100-149/1000"), 100, 50).unwrap(), 50);
        // rclone S3: 200 with the right Content-Range.
        assert_eq!(check_range_reply(200, Some("bytes 100-149/1000"), 100, 50).unwrap(), 50);
        // The range runs past the end: the server trims it.
        assert_eq!(check_range_reply(206, Some("bytes 900-999/1000"), 900, 200).unwrap(), 100);
        assert_eq!(check_range_reply(206, Some("bytes 100-149/*"), 100, 50).unwrap(), 50);
    }

    #[test]
    fn ignored_or_wrong_ranges_are_typed() {
        for (status, cr) in [
            (200, None),
            (206, None),
            (206, Some("bytes 0-49/1000")),
            (206, Some("bytes 100-120/1000")),
            (206, Some("bytes 100-199/1000")),
            (206, Some("items 100-149/1000")),
            (206, Some("bytes x-y/1000")),
        ] {
            let e = check_range_reply(status, cr, 100, 50).unwrap_err();
            assert_eq!(e.kind, ErrorKind::Unsupported, "{status} {cr:?}");
        }
    }

    #[test]
    fn exact_reader_stops_and_detects_short_bodies() {
        let mut r = exact_reader(Box::new(&b"abcdefgh"[..]), 5, "x");
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"abcde");
        let mut r = exact_reader(Box::new(&b"abc"[..]), 5, "x");
        let e: Error = r.read_to_end(&mut Vec::new()).unwrap_err().into();
        assert_eq!(e.kind, ErrorKind::Network);
    }
}
