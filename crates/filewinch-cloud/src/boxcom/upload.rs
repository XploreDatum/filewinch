//! Box uploads: one multipart request for small files, a chunked upload session (SHA-1
//! digest per part and for the whole file) for large ones. Overwrites upload a new version of
//! the existing file with `If-Match`, so a file changed on Box meanwhile isn't clobbered.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use filewinch_core::{Error, ErrorKind, RPath, Result};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};

use super::{FIELDS, Item, P, classify, is_stale, stale_error};
use crate::cache::PathCache;
use crate::http::{Call, Response, Session, error, retry_after};
use crate::upload::UploadTarget;

/// How many times to ask again while Box is still assembling a committed session (202).
const COMMIT_POLLS: u32 = 30;

pub(crate) struct Target {
    pub s: Arc<Session>,
    pub upload: String,
    pub cache: Arc<PathCache<Item>>,
    pub path: RPath,
    pub parent_id: String,
    /// The file being replaced, if any (new version with `If-Match`).
    pub existing: Option<Item>,
    pub session: Option<Chunked>,
}

pub(crate) struct Chunked {
    upload_part: String,
    commit: String,
    abort: String,
    total: u64,
    parts: Vec<Value>,
    whole: Sha1,
}

fn digest(data: &[u8]) -> String {
    format!("sha={}", STANDARD.encode(Sha1::digest(data)))
}

impl Target {
    fn name(&self) -> &str {
        self.path.name()
    }

    fn if_match<'a>(&self, call: Call<'a>) -> Call<'a> {
        match self.existing.as_ref().and_then(|i| i.etag.clone()) {
            Some(e) => call.header("If-Match", e),
            None => call,
        }
    }

    fn guard<T>(&self, r: Result<T>) -> Result<T> {
        r.map_err(|e| {
            if is_stale(&e) {
                self.cache.invalidate(&self.path);
                stale_error(&self.path, e)
            } else {
                e
            }
        })
    }

    /// Caches the file object from an upload/commit reply (`{entries: [file]}`).
    fn finished(&self, mut resp: Response) -> Result<()> {
        let v: Value = self.s.parse(&mut resp)?;
        self.cache.invalidate(&self.path);
        if let Some(item) = Item::from_json(self.path.clone(), &v["entries"][0]) {
            self.cache.put(self.path.clone(), item);
        }
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        let Some(sess) = self.session.as_mut() else {
            return Err(error(
                ErrorKind::Other,
                P.label(),
                &self.path,
                "no upload session is open",
            ));
        };
        let whole = digest_finish(std::mem::take(&mut sess.whole));
        let body = json!({"parts": sess.parts});
        let url = sess.commit.clone();
        for _ in 0..COMMIT_POLLS {
            let call = Call::post(url.clone())
                .query("fields", FIELDS)
                .header("Digest", whole.clone())
                .json(&body)
                .accept(&[202])
                .subject(&self.path)
                .classify(classify);
            let r = self.s.send(self.if_match(call));
            let resp = self.guard(r)?;
            if resp.status().as_u16() == 202 {
                // Box is still assembling the parts; it says when to ask again.
                let wait = retry_after(resp.headers()).unwrap_or(Duration::from_secs(1));
                std::thread::sleep(wait.min(Duration::from_secs(10)));
                continue;
            }
            self.session = None;
            return self.finished(resp);
        }
        Err(Error::new(
            ErrorKind::Network,
            format!(
                "Box is still processing “{}”. Filewinch will try again.",
                self.path
            ),
        ))
    }
}

fn digest_finish(h: Sha1) -> String {
    format!("sha={}", STANDARD.encode(h.finalize()))
}

/// `multipart/form-data` with the `attributes` JSON first, as Box requires, then the file.
fn multipart(attributes: &Value, data: &[u8]) -> (String, Vec<u8>) {
    let boundary = format!("filewinch-{:016x}", {
        let mut b = [0u8; 8];
        let _ = getrandom::getrandom(&mut b);
        u64::from_le_bytes(b)
    });
    let mut body = Vec::with_capacity(data.len() + 512);
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"attributes\"\r\n\r\n{attributes}\r\n\
             --{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"upload\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

impl UploadTarget for Target {
    fn simple(&mut self, data: Vec<u8>) -> Result<()> {
        let (url, attributes) = match &self.existing {
            Some(f) => (
                format!("{}/files/{}/content", self.upload, f.id),
                json!({"name": self.name()}),
            ),
            None => (
                format!("{}/files/content", self.upload),
                json!({"name": self.name(), "parent": {"id": self.parent_id}}),
            ),
        };
        let (content_type, body) = multipart(&attributes, &data);
        let call = Call::post(url)
            .query("fields", FIELDS)
            .bytes(body, &content_type)
            .subject(&self.path)
            .classify(classify);
        let r = self.s.send(self.if_match(call));
        let resp = self.guard(r)?;
        self.finished(resp)
    }

    fn begin(&mut self, size: Option<u64>) -> Result<usize> {
        let Some(total) = size else {
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!(
                    "Box needs to know a file's size before uploading more than 20 MB, and \
                     Filewinch didn't have it for “{}”.",
                    self.path
                ),
            ));
        };
        let (url, args) = match &self.existing {
            Some(f) => (
                format!("{}/files/{}/upload_sessions", self.upload, f.id),
                json!({"file_size": total, "file_name": self.name()}),
            ),
            None => (
                format!("{}/files/upload_sessions", self.upload),
                json!({"folder_id": self.parent_id, "file_size": total, "file_name": self.name()}),
            ),
        };
        let v: Value = self.s.json(
            Call::post(url)
                .json(&args)
                .subject(&self.path)
                .classify(classify),
        )?;
        let (Some(id), Some(part_size)) = (v["id"].as_str(), v["part_size"].as_u64()) else {
            return Err(error(
                ErrorKind::Other,
                P.label(),
                &self.path,
                "Box didn't return an upload session",
            ));
        };
        let base = format!("{}/files/upload_sessions/{id}", self.upload);
        let endpoint = |k: &str, default: String| {
            v["session_endpoints"][k]
                .as_str()
                .map_or(default, str::to_string)
        };
        self.session = Some(Chunked {
            upload_part: endpoint("upload_part", base.clone()),
            commit: endpoint("commit", format!("{base}/commit")),
            abort: endpoint("abort", base),
            total,
            parts: Vec::new(),
            whole: Sha1::new(),
        });
        Ok(part_size.max(1) as usize)
    }

    fn chunk(&mut self, offset: u64, data: &[u8], last: bool) -> Result<()> {
        let Some(sess) = self.session.as_mut() else {
            return Err(error(
                ErrorKind::Other,
                P.label(),
                &self.path,
                "no upload session is open",
            ));
        };
        if data.is_empty() {
            return Err(error(
                ErrorKind::Other,
                P.label(),
                &self.path,
                "empty upload part",
            ));
        }
        sess.whole.update(data);
        let end = offset + data.len() as u64 - 1;
        let v: Value = self.s.json(
            Call::put(sess.upload_part.clone())
                .header(
                    "Content-Range",
                    format!("bytes {offset}-{end}/{}", sess.total),
                )
                .header("Digest", digest(data))
                .bytes(data, "application/octet-stream")
                .subject(&self.path)
                .classify(classify),
        )?;
        if v["part"].is_null() {
            return Err(error(
                ErrorKind::Other,
                P.label(),
                &self.path,
                "Box didn't acknowledge an upload part",
            ));
        }
        sess.parts.push(v["part"].clone());
        if last { self.commit() } else { Ok(()) }
    }

    fn abort(&mut self) {
        if let Some(sess) = self.session.take() {
            // Best effort: an abandoned session expires on Box's side anyway.
            let _ = self.s.send(Call::delete(sess.abort).subject(&self.path));
        }
    }
}
