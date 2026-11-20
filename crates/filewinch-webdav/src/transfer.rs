//! Downloads (GET with `Range` for resume) and streamed uploads (PUT through an
//! [`filewinch_http::upload_pipe`]).

use std::io::{self, Read, Write};
use std::sync::Arc;

use filewinch_core::{Error, ErrorKind, RPath, Result, WriteSink};
use filewinch_http::ureq::{Body, BodyReader, http};
use filewinch_http::{PipeReader, UploadPipe, from_io, send_reader, to_io, upload_pipe};

use crate::client::Client;

/// Opens `path` for reading from `offset`. A server that ignores `Range` (200 instead of
/// 206) still works: the first `offset` bytes are read and dropped.
pub(crate) fn open_read(client: &Client, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
    let what = format!("download {}", path.name());
    let rp = client.target.request_path(path, false);
    let range = format!("bytes={offset}-");
    let headers: Vec<(&str, &str)> = if offset > 0 { vec![("range", &range)] } else { Vec::new() };
    let resp = client.send("GET", &rp, &headers, "")?;
    let status = resp.status().as_u16();
    let host = client.target.host_name().to_string();
    match status {
        200 => {
            let mut r = DavReader::new(resp, host);
            if offset > 0 {
                let skipped = io::copy(&mut (&mut r).take(offset), &mut io::sink()).map_err(from_io)?;
                if skipped < offset {
                    return Err(shorter_than(&what, offset));
                }
            }
            Ok(Box::new(r))
        }
        206 => {
            let start = content_range(resp.headers()).map(|(s, _)| s);
            if start != Some(offset) {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!("Couldn't {what}: the server sent the wrong part of the file."),
                )
                .with_detail(format!("asked for bytes {offset}-, got Content-Range start {start:?}")));
            }
            Ok(Box::new(DavReader::new(resp, host)))
        }
        416 if offset > 0 => {
            // Nothing left to read when the file is exactly `offset` bytes long.
            match content_range(resp.headers()) {
                Some((_, Some(total))) if total == offset => Ok(Box::new(io::empty())),
                Some((_, Some(total))) if total < offset => Err(shorter_than(&what, offset)),
                _ => Err(client.fail(resp, &what)),
            }
        }
        _ => Err(client.fail(resp, &what)),
    }
}

/// `len` bytes of `path` from `offset`, for segmented downloads. Unlike [`open_read`], a
/// server that ignores `Range` is an error ([`Error::range_ignored`]), never a skip.
pub(crate) fn open_range(client: &Client, path: &RPath, offset: u64, len: u64) -> Result<Box<dyn Read + Send>> {
    let what = format!("download {}", path.name());
    let rp = client.target.request_path(path, false);
    let range = filewinch_core::range_header(offset, len);
    let resp = client.send("GET", &rp, &[("range", &range)], "")?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(client.fail(resp, &what));
    }
    let cr = resp.headers().get("content-range").and_then(|v| v.to_str().ok());
    let n = filewinch_core::check_range_reply(status, cr, offset, len)?;
    let host = client.target.host_name().to_string();
    Ok(filewinch_core::exact_reader(Box::new(DavReader::new(resp, host)), n, &format!("“{}”", path.name())))
}

fn shorter_than(what: &str, offset: u64) -> Error {
    Error::new(
        ErrorKind::InvalidPath,
        format!("Couldn't {what}: the file on the server is now smaller than the part already downloaded."),
    )
    .with_detail(format!("resume offset {offset}"))
}

/// `Content-Range: bytes start-end/total` or `bytes */total` → (start, total). For the
/// unsatisfied form start is `u64::MAX`.
pub(crate) fn content_range(headers: &http::HeaderMap) -> Option<(u64, Option<u64>)> {
    let v = headers.get("content-range")?.to_str().ok()?.trim();
    let rest = v.strip_prefix("bytes")?.trim_start();
    let (range, total) = rest.split_once('/')?;
    let total = total.trim().parse().ok();
    if range.trim() == "*" {
        return Some((u64::MAX, total));
    }
    let start = range.split_once('-')?.0.trim().parse().ok()?;
    Some((start, total))
}

/// The response body as a reader. Errors come out as `to_io(core Error)`; a body that ends
/// before its declared length is a lost connection, never a silent short file.
struct DavReader {
    inner: BodyReader<'static>,
    left: Option<u64>,
    host: String,
}

impl DavReader {
    fn new(resp: http::Response<Body>, host: String) -> DavReader {
        let body = resp.into_body();
        let left = body.content_length();
        DavReader { inner: body.into_reader(), left, host }
    }

    fn lost(&self, detail: impl std::fmt::Display) -> io::Error {
        to_io(
            Error::new(ErrorKind::Network, format!("The connection to {} was lost during the download.", self.host))
                .with_detail(detail),
        )
    }
}

impl Read for DavReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        match self.inner.read(buf) {
            Ok(0) => match self.left {
                Some(n) if n > 0 => Err(self.lost(format!("{n} bytes missing"))),
                _ => Ok(0),
            },
            Ok(n) => {
                if let Some(left) = &mut self.left {
                    *left = left.saturating_sub(n as u64);
                }
                Ok(n)
            }
            Err(e) => {
                let err = from_io(e);
                Err(match err.kind {
                    // Anything odd while a body streams in is the connection's fault.
                    ErrorKind::Other => self.lost(err.detail.unwrap_or(err.message)),
                    _ => to_io(err),
                })
            }
        }
    }
}

/// A streaming PUT. Dropping it without `finish` aborts the request mid-body.
pub(crate) struct DavSink {
    pipe: UploadPipe<()>,
}

impl Write for DavSink {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.pipe.write(data)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.pipe.flush()
    }
}

impl WriteSink for DavSink {
    fn finish(self: Box<Self>) -> Result<()> {
        self.pipe.finish()
    }
}

pub(crate) fn open_write(client: Arc<Client>, path: &RPath, size: Option<u64>) -> Result<Box<dyn WriteSink>> {
    client.check_open()?;
    let path = path.clone();
    let pipe = upload_pipe(move |reader| put(&client, &path, reader, size));
    Ok(Box::new(DavSink { pipe }))
}

/// Counts what the request has taken from the pipe, so a 401 can be retried only when no
/// body bytes were consumed.
struct Counting<'a> {
    inner: &'a mut PipeReader,
    taken: u64,
}

impl Read for Counting<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.taken += n as u64;
        Ok(n)
    }
}

fn put(client: &Client, path: &RPath, mut reader: PipeReader, size: Option<u64>) -> Result<()> {
    let what = format!("upload {}", path.name());
    let rp = client.target.request_path(path, false);
    let url = client.target.url(&rp);
    let mut body = Counting { inner: &mut reader, taken: 0 };
    let mut auth_retries = 0;
    loop {
        client.check_open()?;
        let authz = client.authorization("PUT", &rp);
        let mut req = client.http.agent().put(&url);
        if let Some(a) = &authz {
            req = req.header("authorization", a);
        }
        let resp = send_reader(req, &mut body, size).map_err(|e| client.transport(e))?;
        let status = resp.status().as_u16();
        match status {
            200..=299 => return Ok(()),
            401 => {
                let retry = client.on_unauthorized(resp.headers(), authz.is_some())?;
                if retry && body.taken == 0 && auth_retries < 2 {
                    auth_retries += 1;
                    continue;
                }
                if retry {
                    // The server's sign-in nonce expired mid-upload; the body can't be
                    // replayed, but a retry of the whole file will sign in afresh.
                    return Err(Error::new(
                        ErrorKind::Network,
                        format!("Couldn't {what}: the server asked to sign in again during the upload."),
                    )
                    .with_detail("HTTP 401 (stale nonce)"));
                }
                return Err(client.auth_error());
            }
            409 => {
                return Err(Error::new(
                    ErrorKind::NotFound,
                    format!("Couldn't {what}: the folder it goes in doesn't exist."),
                )
                .with_detail("HTTP 409"));
            }
            405 => {
                return Err(Error::new(
                    ErrorKind::InvalidPath,
                    format!("Couldn't {what}: the server won't put a file there (is there a folder with that name?)."),
                )
                .with_detail("HTTP 405"));
            }
            _ => return Err(client.fail(resp, &what)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::content_range;
    use filewinch_http::ureq::http::HeaderMap;

    fn h(v: &str) -> HeaderMap {
        let mut m = HeaderMap::new();
        m.insert("content-range", v.parse().unwrap());
        m
    }

    #[test]
    fn parses_content_range() {
        assert_eq!(content_range(&h("bytes 100-199/1000")), Some((100, Some(1000))));
        assert_eq!(content_range(&h("bytes 5-9/*")), Some((5, None)));
        assert_eq!(content_range(&h("bytes */42")), Some((u64::MAX, Some(42))));
        assert_eq!(content_range(&h("items 1-2/3")), None);
        assert_eq!(content_range(&HeaderMap::new()), None);
    }
}
