//! OneDrive uploads: one `PUT …:/name:/content` for small files, an upload session
//! (`createUploadSession`, then `PUT` fragments to the pre-authorised `uploadUrl`) for larger
//! ones. Graph needs the final size for a session, and fragments must be multiples of
//! 320 KiB (except the last). Abandoned sessions are cancelled with `DELETE uploadUrl`.

use std::sync::Arc;

use filewinch_core::{ErrorKind, RPath, Result};
use serde_json::{Value, json};

use super::Inner;
use super::items::{Node, classify};
use crate::http::{Call, error};
use crate::oauth::pct;
use crate::upload::UploadTarget;

/// Graph's fragment granularity.
const UNIT: usize = 320 * 1024;

pub(crate) struct Target {
    inner: Arc<Inner>,
    parent: Node,
    path: RPath,
    upload_url: Option<String>,
    total: u64,
}

impl Target {
    pub(crate) fn new(inner: Arc<Inner>, parent: Node, path: RPath) -> Self {
        Target {
            inner,
            parent,
            path,
            upload_url: None,
            total: 0,
        }
    }

    /// `{parent}:/{name}:` — Graph's path addressing relative to the parent item.
    fn under_parent(&self) -> String {
        format!(
            "{}:/{}:",
            self.inner.item_url(&self.parent),
            pct(self.path.name())
        )
    }

    fn done(&self) {
        self.inner.cache.invalidate(&self.path);
    }

    /// Bytes the server already holds, from `nextExpectedRanges: ["123-"]`.
    fn acknowledged(v: &Value) -> Option<u64> {
        let first = v["nextExpectedRanges"].as_array()?.first()?.as_str()?;
        first.split('-').next()?.parse().ok()
    }
}

impl UploadTarget for Target {
    fn simple(&mut self, data: Vec<u8>) -> Result<()> {
        let url = format!(
            "{}/content?@microsoft.graph.conflictBehavior=replace",
            self.under_parent()
        );
        self.inner.s.send(
            Call::put(url)
                .bytes(data, "application/octet-stream")
                .subject(&self.path)
                .classify(classify),
        )?;
        self.done();
        Ok(())
    }

    fn begin(&mut self, size: Option<u64>) -> Result<usize> {
        let Some(total) = size else {
            return Err(filewinch_core::Error::new(
                ErrorKind::Unsupported,
                format!(
                    "OneDrive needs to know a file's size before uploading it in parts, and the size \
                     of “{}” wasn't known. Copy it from a source that reports sizes.",
                    self.path
                ),
            ));
        };
        let v: Value = self.inner.s.json(
            Call::post(format!("{}/createUploadSession", self.under_parent()))
                .json(&json!({"item": {"@microsoft.graph.conflictBehavior": "replace"}}))
                .subject(&self.path)
                .classify(classify),
        )?;
        let url = v["uploadUrl"]
            .as_str()
            .filter(|u| !u.is_empty())
            .ok_or_else(|| {
                error(
                    ErrorKind::Other,
                    self.inner.service(),
                    &self.path,
                    "OneDrive didn't return an upload URL",
                )
            })?;
        self.upload_url = Some(url.to_string());
        self.total = total;
        Ok((self.inner.tuning.chunk / UNIT).max(1) * UNIT)
    }

    fn chunk(&mut self, offset: u64, data: &[u8], last: bool) -> Result<()> {
        let url = self.upload_url.clone().unwrap_or_default();
        let end = offset + data.len() as u64;
        let mut at = offset;
        // A few rounds at most: each must make progress, or the session is broken.
        for _ in 0..8 {
            let part = &data[(at - offset) as usize..];
            let mut resp = self.inner.s.send(
                Call::put(url.as_str())
                    .no_auth()
                    .header(
                        "Content-Range",
                        format!("bytes {at}-{}/{}", end - 1, self.total),
                    )
                    .bytes(part, "application/octet-stream")
                    .accept(&[202])
                    .subject(&self.path)
                    .classify(classify),
            )?;
            if resp.status().as_u16() != 202 {
                // 200/201: the file is complete.
                self.upload_url = None;
                self.done();
                return Ok(());
            }
            let v: Value = self.inner.s.parse(&mut resp).unwrap_or(Value::Null);
            let next = Self::acknowledged(&v).unwrap_or(end);
            if next >= end {
                if last {
                    return Err(error(
                        ErrorKind::Network,
                        self.inner.service(),
                        &self.path,
                        "OneDrive took every byte but didn't finish the file",
                    ));
                }
                return Ok(());
            }
            if next < at || next < offset {
                break;
            }
            at = next;
        }
        Err(error(
            ErrorKind::Network,
            self.inner.service(),
            &self.path,
            "OneDrive kept refusing part of the upload",
        ))
    }

    fn abort(&mut self) {
        if let Some(url) = self.upload_url.take() {
            let _ = self
                .inner
                .s
                .send(Call::delete(url).no_auth().subject(&self.path));
        }
    }
}
