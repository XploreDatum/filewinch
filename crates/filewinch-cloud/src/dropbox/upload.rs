//! Dropbox uploads: `files/upload` for small files, `upload_session/start|append_v2|finish`
//! for the rest. Dropbox has no call to abandon a session; unfinished sessions hold no
//! visible file and expire on Dropbox's side after a few days.

use std::sync::Arc;

use filewinch_core::{RPath, Result};
use serde_json::{Value, json};

use super::{classify, dbx_path, header_arg};
use crate::http::{Call, Session};
use crate::upload::UploadTarget;

pub(crate) struct Target {
    s: Arc<Session>,
    content: String,
    path: RPath,
    session_id: Option<String>,
    chunk: usize,
}

impl Target {
    pub(crate) fn new(s: Arc<Session>, content: String, path: RPath, chunk: usize) -> Self {
        Target {
            s,
            content,
            path,
            session_id: None,
            chunk,
        }
    }

    fn commit(&self) -> Value {
        json!({"path": dbx_path(&self.path), "mode": "overwrite", "autorename": false, "mute": true})
    }

    fn post(&self, endpoint: &str, arg: Value, data: &[u8]) -> Result<Value> {
        let mut resp = self.s.send(
            Call::post(format!("{}/{endpoint}", self.content))
                .header("Dropbox-API-Arg", header_arg(&arg))
                .bytes(data, "application/octet-stream")
                .subject(&self.path)
                .classify(classify),
        )?;
        // append_v2 answers with an empty body or `null`.
        let body = crate::http::read_body(&mut resp, self.s.service())?;
        Ok(serde_json::from_slice(&body).unwrap_or(Value::Null))
    }
}

impl UploadTarget for Target {
    fn simple(&mut self, data: Vec<u8>) -> Result<()> {
        self.post("files/upload", self.commit(), &data)?;
        Ok(())
    }

    fn begin(&mut self, _size: Option<u64>) -> Result<usize> {
        let v = self.post("files/upload_session/start", json!({"close": false}), &[])?;
        let id = v["session_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                crate::http::error(
                    filewinch_core::ErrorKind::Other,
                    self.s.service(),
                    &self.path,
                    "Dropbox didn't return an upload session",
                )
            })?;
        self.session_id = Some(id.to_string());
        Ok(self.chunk)
    }

    fn chunk(&mut self, offset: u64, data: &[u8], last: bool) -> Result<()> {
        let cursor = json!({"session_id": self.session_id, "offset": offset});
        if last {
            self.post(
                "files/upload_session/finish",
                json!({"cursor": cursor, "commit": self.commit()}),
                data,
            )?;
            self.session_id = None;
        } else {
            self.post(
                "files/upload_session/append_v2",
                json!({"cursor": cursor, "close": false}),
                data,
            )?;
        }
        Ok(())
    }

    fn abort(&mut self) {
        // No abort endpoint; dropping the ID is all that can be done.
        self.session_id = None;
    }
}
