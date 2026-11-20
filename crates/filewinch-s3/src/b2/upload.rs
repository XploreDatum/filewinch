//! B2 uploads.
//!
//! - Known size below the large-file threshold: one streamed `b2_upload_file` with
//!   `X-Bz-Content-Sha1: hex_digits_at_end` (Content-Length = size + 40). The SHA-1 is computed
//!   while the bytes stream and appended last, so B2 still verifies the data without the file
//!   being read twice or buffered.
//! - Large or unknown size: the large-file API. Each part's SHA-1 must be sent up front, so
//!   one part at a time is buffered and hashed. Parts use B2's recommended size, capped at
//!   [`Limits::max_buffered_part`] (32 MiB) to bound memory per transfer (the engine runs
//!   several at once), and grow only when a known size would otherwise need over 10 000 parts.
//! - Unknown size that ends before the first part fills: a plain upload of the buffer.

use std::io::{self, Write};
use std::sync::Arc;

use filewinch_core::{Error, ErrorKind, Result, WriteSink};
use filewinch_http::{UploadPipe, to_io, upload_pipe};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};

use super::api::{Failure, Limits, Session, classify, host_of, json_or_failure, upload_target};
use crate::s3::part_size_unknown;
use crate::sigv4::encode_path;

const CONTENT_TYPE: &str = "b2/x-auto";
/// Attempts for a part or buffered upload (each with a fresh upload URL).
const ATTEMPTS: usize = 3;

pub(crate) struct B2Sink {
    session: Arc<Session>,
    bucket_id: String,
    name: String,
    size: Option<u64>,
    written: u64,
    state: State,
    what: String,
}

enum State {
    Small { pipe: UploadPipe<()>, sha: Sha1 },
    Buffer(Vec<u8>),
    Large(Large),
    Done,
}

struct Large {
    file_id: String,
    part_size: u64,
    /// Grow part sizes past 1000 parts (unknown total size).
    growing: bool,
    buf: Vec<u8>,
    sha1s: Vec<String>,
    /// The part upload URL that last worked.
    url: Option<(String, String)>,
}

/// (part size, use the large-file API) for a total size.
pub(crate) fn plan(limits: &Limits, recommended: u64, min_part: u64, size: Option<u64>) -> (u64, bool) {
    let base = recommended.clamp(min_part, limits.max_buffered_part.max(min_part));
    match size {
        None => (base, true),
        Some(n) => {
            let part = base.max(n.div_ceil(limits.max_parts));
            (part, n > part && n >= limits.large_from.min(recommended))
        }
    }
}

impl B2Sink {
    pub fn start(session: Arc<Session>, bucket_id: String, name: String, size: Option<u64>, what: String) -> Result<B2Sink> {
        session.check_open()?;
        let acct = session.account();
        let (part_size, large) = plan(&session.limits, acct.recommended_part, acct.min_part, size);
        let state = match size {
            Some(n) if !large => {
                let (url, token) = session.take_upload_url(&bucket_id, &what)?;
                State::Small { pipe: small_pipe(&session, &bucket_id, &name, n, url, token, &what), sha: Sha1::new() }
            }
            Some(_) => State::Large(start_large(&session, &bucket_id, &name, part_size, false, &what)?),
            None => State::Buffer(Vec::new()),
        };
        Ok(B2Sink { session, bucket_id, name, size, written: 0, state, what })
    }

    fn accept(&mut self, data: &[u8]) -> Result<usize> {
        loop {
            match &mut self.state {
                State::Done => return Err(Error::new(ErrorKind::Other, "The upload has already finished.")),
                State::Small { pipe, sha } => {
                    let n = pipe.write(data).map_err(filewinch_http::from_io)?;
                    sha.update(&data[..n]);
                    return Ok(n);
                }
                State::Buffer(buf) => {
                    let acct = self.session.account();
                    let (part, _) = plan(&self.session.limits, acct.recommended_part, acct.min_part, None);
                    if (buf.len() as u64) < part {
                        let n = (part as usize - buf.len()).min(data.len());
                        buf.extend_from_slice(&data[..n]);
                        return Ok(n);
                    }
                    let first = std::mem::take(buf);
                    let mut large = start_large(&self.session, &self.bucket_id, &self.name, part, true, &self.what)?;
                    large.buf = first;
                    let sent = large.send_part(&self.session, &self.what);
                    self.state = State::Large(large);
                    sent?;
                }
                State::Large(large) => {
                    let target = large.target();
                    if large.buf.len() < target {
                        let n = (target - large.buf.len()).min(data.len());
                        large.buf.extend_from_slice(&data[..n]);
                        return Ok(n);
                    }
                    large.send_part(&self.session, &self.what)?;
                }
            }
        }
    }

    fn finish_inner(&mut self) -> Result<()> {
        if self.size.is_some_and(|total| self.written != total) {
            return Err(Error::new(
                ErrorKind::Other,
                format!("Couldn't {}: the file changed size while it was being uploaded.", self.what),
            ));
        }
        match std::mem::replace(&mut self.state, State::Done) {
            State::Done => Ok(()),
            State::Small { mut pipe, sha } => {
                let digest = hex::encode(sha.finalize());
                pipe.write_all(digest.as_bytes()).map_err(filewinch_http::from_io)?;
                pipe.finish()
            }
            State::Buffer(buf) => upload_bytes(&self.session, &self.bucket_id, &self.name, &buf, &self.what).map(drop),
            State::Large(mut large) => {
                let result = (|| {
                    if !large.buf.is_empty() {
                        large.send_part(&self.session, &self.what)?;
                    }
                    let body = json!({"fileId": large.file_id, "partSha1Array": large.sha1s});
                    self.session.call("b2_finish_large_file", &body, &self.what, true).map(drop)
                })();
                if result.is_err() {
                    cancel_in_background(&self.session, &large.file_id);
                }
                result
            }
        }
    }
}

fn start_large(session: &Session, bucket_id: &str, name: &str, part_size: u64, growing: bool, what: &str) -> Result<Large> {
    let body = json!({"bucketId": bucket_id, "fileName": name, "contentType": CONTENT_TYPE});
    let v = session.call("b2_start_large_file", &body, what, false)?;
    let file_id = v["fileId"]
        .as_str()
        .ok_or_else(|| Error::new(ErrorKind::Other, format!("Couldn't {what}: Backblaze B2 didn't start the upload.")))?
        .to_string();
    Ok(Large { file_id, part_size, growing, buf: Vec::new(), sha1s: Vec::new(), url: None })
}

impl Large {
    fn target(&self) -> usize {
        let n = self.sha1s.len() as u32 + 1;
        if self.growing { part_size_unknown(n, self.part_size) as usize } else { self.part_size as usize }
    }

    /// Uploads the buffered part (retrying with a fresh part URL), then clears the buffer.
    fn send_part(&mut self, session: &Session, what: &str) -> Result<()> {
        let n = self.sha1s.len() + 1;
        if n as u64 > session.limits.max_parts {
            return Err(Error::new(
                ErrorKind::NoSpace,
                format!("Couldn't {what}: the file is too large to upload without knowing its size first."),
            ));
        }
        let sha = hex::encode(Sha1::digest(&self.buf));
        let mut last = None;
        for attempt in 0..ATTEMPTS {
            let (url, token) = match self.url.take() {
                Some(u) => u,
                None => {
                    let v = session.call("b2_get_upload_part_url", &json!({"fileId": self.file_id}), what, true)?;
                    upload_target(&v, what)?
                }
            };
            let sent = post_bytes(session, &url, &token, &[("x-bz-part-number", n.to_string())], &sha, &self.buf, what);
            match sent {
                Ok(_) => {
                    self.url = Some((url, token));
                    self.sha1s.push(sha);
                    self.buf.clear();
                    return Ok(());
                }
                Err(e) if retry_upload(&e) && attempt + 1 < ATTEMPTS => {
                    std::thread::sleep(std::time::Duration::from_millis(200 * (attempt as u64 + 1)));
                    last = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| Error::new(ErrorKind::Network, format!("Couldn't {what}."))))
    }
}

/// Upload URLs fail over: an expired upload token, a busy pod or a dropped connection all
/// mean "get another URL and try again" (B2's documented behaviour).
fn retry_upload(e: &Error) -> bool {
    matches!(e.kind, ErrorKind::Network | ErrorKind::RateLimited | ErrorKind::Auth)
}

/// POSTs a buffered body to an upload URL with its SHA-1.
fn post_bytes(
    session: &Session,
    url: &str,
    token: &str,
    extra: &[(&str, String)],
    sha: &str,
    data: &[u8],
    what: &str,
) -> Result<Value> {
    let host = host_of(url);
    let mut req = session
        .http
        .agent()
        .post(url)
        .header("authorization", token)
        .header("x-bz-content-sha1", sha);
    for (k, v) in extra {
        req = req.header(*k, v.as_str());
    }
    let resp = req.send(data).map_err(|e| session.http.transport_error(e, &host))?;
    json_or_failure(resp, what, &host).map_err(|f| match f {
        Failure::TokenExpired(e) | Failure::Error(e) => e,
    })
}

/// Uploads a small in-memory file (`b2_upload_file`), retrying with fresh upload URLs.
pub(crate) fn upload_bytes(session: &Session, bucket_id: &str, name: &str, data: &[u8], what: &str) -> Result<Value> {
    let sha = hex::encode(Sha1::digest(data));
    let mut last = None;
    for attempt in 0..ATTEMPTS {
        let (url, token) = session.take_upload_url(bucket_id, what)?;
        let headers = [("x-bz-file-name", encode_path(name)), ("content-type", CONTENT_TYPE.to_string())];
        match post_bytes(session, &url, &token, &headers, &sha, data, what) {
            Ok(v) => {
                session.give_back_upload_url(bucket_id, url, token);
                return Ok(v);
            }
            Err(e) if retry_upload(&e) && attempt + 1 < ATTEMPTS => {
                std::thread::sleep(std::time::Duration::from_millis(200 * (attempt as u64 + 1)));
                last = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| Error::new(ErrorKind::Network, format!("Couldn't {what}."))))
}

fn small_pipe(
    session: &Arc<Session>,
    bucket_id: &str,
    name: &str,
    len: u64,
    url: String,
    token: String,
    what: &str,
) -> UploadPipe<()> {
    let (session, bucket_id, name, what) = (session.clone(), bucket_id.to_string(), name.to_string(), what.to_string());
    upload_pipe(move |reader| {
        let host = host_of(&url);
        let req = session
            .http
            .agent()
            .post(&url)
            .header("authorization", token.as_str())
            .header("x-bz-file-name", encode_path(&name))
            .header("content-type", CONTENT_TYPE)
            .header("x-bz-content-sha1", "hex_digits_at_end");
        let resp = filewinch_http::send_reader(req, reader, Some(len + 40)).map_err(|e| {
            let err = session.http.transport_error(e, &host);
            if err.kind == ErrorKind::Cancelled { Error::cancelled() } else { err }
        })?;
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            session.give_back_upload_url(&bucket_id, url.clone(), token.clone());
            return Ok(());
        }
        let headers = resp.headers().clone();
        let body = filewinch_http::body_reader(resp);
        let mut text = String::new();
        let _ = io::Read::read_to_string(&mut io::Read::take(body, 64 * 1024), &mut text);
        Err(match classify(status, &headers, &text, &what) {
            Failure::TokenExpired(e) | Failure::Error(e) => e,
        })
    })
}

fn cancel_in_background(session: &Arc<Session>, file_id: &str) {
    let (session, file_id) = (session.clone(), file_id.to_string());
    let _ = std::thread::Builder::new().name("filewinch-b2-cancel".into()).spawn(move || {
        // Best effort: an unfinished large file never becomes visible, and B2 lists it for cleanup.
        let _ = session.call("b2_cancel_large_file", &json!({"fileId": file_id}), "cancel the upload", true);
    });
}

impl Write for B2Sink {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        if self.size.is_some_and(|total| self.written + data.len() as u64 > total) {
            return Err(to_io(Error::new(
                ErrorKind::Other,
                format!("Couldn't {}: the file grew while it was being uploaded.", self.what),
            )));
        }
        let n = self.accept(data).map_err(to_io)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl WriteSink for B2Sink {
    fn finish(mut self: Box<Self>) -> Result<()> {
        self.finish_inner()
    }
}

impl Drop for B2Sink {
    fn drop(&mut self) {
        // Not finished: a streaming upload aborts mid-body when its pipe drops (B2 never sees
        // the SHA-1, so nothing is stored); a large file is cancelled.
        if let State::Large(large) = std::mem::replace(&mut self.state, State::Done) {
            cancel_in_background(&self.session, &large.file_id);
        }
    }
}
