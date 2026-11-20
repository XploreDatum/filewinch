//! Drive uploads: `uploadType=multipart` for small files, a resumable session for the rest.
//! An existing file at the path gets new content (same ID, keeps sharing and history) instead
//! of a duplicate. Dropping the sink before `finish` cancels the session (DELETE on the
//! session URI), so Drive creates nothing.

use std::sync::Arc;

use base64::Engine as _;
use filewinch_core::{ErrorKind, RPath, Result};
use serde_json::{Value, json};

use super::classify;
use crate::cache::PathCache;
use crate::http::{Call, Session, error, header};
use crate::upload::UploadTarget;

use super::resolve::Item;

/// Drive's resumable chunks must be multiples of 256 KiB (except the last).
const QUANTUM: usize = 256 * 1024;

pub(crate) struct Target {
    pub s: Arc<Session>,
    pub upload: String,
    pub path: RPath,
    pub cache: Arc<PathCache<Item>>,
    /// Existing file to replace, else create `name` in `parent`.
    pub existing: Option<String>,
    pub parent: String,
    pub name: String,
    pub chunk: usize,
    pub session: Option<String>,
}

impl Target {
    fn url(&self) -> String {
        match &self.existing {
            Some(id) => format!("{}/files/{id}", self.upload),
            None => format!("{}/files", self.upload),
        }
    }

    fn start(&self, upload_type: &'static str) -> Call<'_> {
        let call = match self.existing {
            Some(_) => Call::patch(self.url()),
            None => Call::post(self.url()),
        };
        call.query("uploadType", upload_type)
            .query("supportsAllDrives", "true")
            .query("fields", "id")
            .subject(&self.path)
            .classify(classify)
    }

    fn metadata(&self) -> Value {
        match self.existing {
            Some(_) => json!({}),
            None => json!({"name": self.name, "parents": [self.parent]}),
        }
    }

    fn done(&mut self) {
        self.session = None;
        self.cache.invalidate(&self.path);
    }
}

fn boundary() -> String {
    let mut b = [0u8; 18];
    getrandom::getrandom(&mut b).expect("the system random number generator failed");
    format!(
        "filewinch-{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
    )
}

/// Parses a `Range: bytes=0-N` acknowledgement into the number of bytes Drive holds.
fn acknowledged(range: Option<String>) -> u64 {
    range
        .and_then(|r| {
            r.rsplit('-')
                .next()
                .and_then(|n| n.trim().parse::<u64>().ok())
        })
        .map_or(0, |n| n + 1)
}

impl UploadTarget for Target {
    fn simple(&mut self, data: Vec<u8>) -> Result<()> {
        let b = boundary();
        let mut body = Vec::with_capacity(data.len() + 512);
        body.extend_from_slice(
            format!(
                "--{b}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{}\r\n",
                self.metadata()
            )
            .as_bytes(),
        );
        body.extend_from_slice(
            format!("--{b}\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(&data);
        body.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
        let ct = format!("multipart/related; boundary={b}");
        self.s.send(self.start("multipart").bytes(body, &ct))?;
        self.done();
        Ok(())
    }

    fn begin(&mut self, size: Option<u64>) -> Result<usize> {
        let mut call = self
            .start("resumable")
            .header("X-Upload-Content-Type", "application/octet-stream");
        if let Some(n) = size {
            call = call.header("X-Upload-Content-Length", n.to_string());
        }
        let resp = self.s.send(call.json(&self.metadata()))?;
        let uri = header(&resp, "location").ok_or_else(|| {
            error(
                ErrorKind::Other,
                self.s.service(),
                &self.path,
                "Drive didn't return an upload session",
            )
        })?;
        self.session = Some(uri);
        Ok((self.chunk / QUANTUM).max(1) * QUANTUM)
    }

    fn chunk(&mut self, offset: u64, data: &[u8], last: bool) -> Result<()> {
        let uri = self.session.clone().ok_or_else(|| {
            error(
                ErrorKind::Other,
                self.s.service(),
                &self.path,
                "the upload session is gone",
            )
        })?;
        let end = offset + data.len() as u64;
        let total = if last { end.to_string() } else { "*".into() };
        let mut start = 0usize;
        let mut stalls = 0;
        loop {
            let piece = &data[start..];
            let range = if piece.is_empty() {
                format!("bytes */{total}")
            } else {
                format!("bytes {}-{}/{total}", offset + start as u64, end - 1)
            };
            let resp = self.s.send(
                Call::put(uri.clone())
                    .header("Content-Range", range)
                    .bytes(piece, "application/octet-stream")
                    .no_redirects()
                    .accept(&[308])
                    .subject(&self.path)
                    .classify(classify),
            )?;
            if resp.status().as_u16() != 308 {
                if last {
                    self.done();
                    return Ok(());
                }
                return Err(error(
                    ErrorKind::Other,
                    self.s.service(),
                    &self.path,
                    "Drive ended the upload early",
                ));
            }
            // 308 Resume Incomplete: Drive says how much it has.
            let have = acknowledged(header(&resp, "range"));
            if have >= end && !last {
                return Ok(());
            }
            if have < offset + start as u64 || have > end {
                return Err(error(
                    ErrorKind::Network,
                    self.s.service(),
                    &self.path,
                    "Drive lost part of the upload",
                ));
            }
            let next = (have - offset) as usize;
            stalls = if next == start { stalls + 1 } else { 0 };
            if stalls >= 3 {
                return Err(error(
                    ErrorKind::Network,
                    self.s.service(),
                    &self.path,
                    "Drive stopped accepting data",
                ));
            }
            start = next;
        }
    }

    fn abort(&mut self) {
        if let Some(uri) = self.session.take() {
            // Drive answers a cancelled session with 499.
            let _ = self.s.send(Call::delete(uri).no_redirects().accept(&[499]));
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_range_acknowledgement() {
        assert_eq!(super::acknowledged(Some("bytes=0-262143".into())), 262_144);
        assert_eq!(super::acknowledged(None), 0);
    }
}
