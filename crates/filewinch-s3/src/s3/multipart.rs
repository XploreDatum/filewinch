//! Multipart upload calls shared by uploads and large copies.

use std::sync::Arc;

use filewinch_core::{Error, ErrorKind, Result};

use super::client::{Call, Client};
use crate::xml;

/// An open multipart upload on `bucket/key`.
pub(crate) struct Multipart {
    pub client: Arc<Client>,
    pub bucket: String,
    pub key: String,
    pub upload_id: String,
    /// (part number, ETag) of the parts uploaded so far, in order.
    pub parts: Vec<(u32, String)>,
}

impl Multipart {
    pub fn create(client: Arc<Client>, bucket: &str, key: &str, what: &str) -> Result<Multipart> {
        let resp = client.send(&Call::new("POST", Some(bucket), key).query("uploads", ""), what)?;
        let body = client.text(resp, what)?;
        client.check_body_error(&body, what)?;
        let upload_id = xml::parse(&body)
            .ok()
            .and_then(|root| root.text_of("UploadId").map(|s| s.trim().to_string()))
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                Error::new(ErrorKind::Other, format!("Couldn't {what}: {} didn't start the upload.", client.host()))
                    .with_detail("CreateMultipartUpload response had no UploadId")
            })?;
        Ok(Multipart { client, bucket: bucket.into(), key: key.into(), upload_id, parts: Vec::new() })
    }

    pub fn next_part(&self) -> u32 {
        self.parts.len() as u32 + 1
    }

    /// A call addressed to part `n` of this upload.
    pub fn part_call(&self, n: u32) -> Call<'_> {
        Call::new("PUT", Some(&self.bucket), &self.key)
            .query("partNumber", n.to_string())
            .query("uploadId", self.upload_id.clone())
    }

    /// Uploads a part held in memory (hashed and signed; replayable after a redirect).
    pub fn upload_bytes(&mut self, data: &[u8], what: &str) -> Result<()> {
        let n = self.next_part();
        let resp = self.client.send(&self.part_call(n).body(data), what)?;
        let etag = etag(&resp, &self.client, what)?;
        self.parts.push((n, etag));
        Ok(())
    }

    pub fn complete(&self, what: &str) -> Result<()> {
        let mut doc = String::from("<CompleteMultipartUpload xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">");
        for (n, etag) in &self.parts {
            doc.push_str(&format!("<Part><PartNumber>{n}</PartNumber><ETag>{}</ETag></Part>", xml::escape(etag)));
        }
        doc.push_str("</CompleteMultipartUpload>");
        let call = Call::new("POST", Some(&self.bucket), &self.key)
            .query("uploadId", self.upload_id.clone())
            .header("content-type", "application/xml")
            .body(doc.as_bytes());
        let resp = self.client.send(&call, what)?;
        // S3 can answer 200 and still fail: the error is in the body.
        let body = self.client.text(resp, what)?;
        self.client.check_body_error(&body, what)
    }

    /// Aborts on a background thread so a cancelled transfer never waits on the network.
    pub fn abort_in_background(&self) {
        let client = self.client.clone();
        let (bucket, key, id) = (self.bucket.clone(), self.key.clone(), self.upload_id.clone());
        let _ = std::thread::Builder::new().name("filewinch-s3-abort".into()).spawn(move || {
            let call = Call::new("DELETE", Some(&bucket), &key).query("uploadId", id);
            // Best effort: an upload left behind is removed by the bucket's lifecycle rules,
            // and it never becomes a visible object.
            let _ = client.send(&call, "cancel the upload");
        });
    }
}

/// The ETag header of a part upload.
pub(crate) fn etag(
    resp: &filewinch_http::ureq::http::Response<filewinch_http::ureq::Body>,
    client: &Client,
    what: &str,
) -> Result<String> {
    resp.headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .ok_or_else(|| {
            Error::new(ErrorKind::Other, format!("Couldn't {what}: {} didn't confirm the part.", client.host()))
                .with_detail("part response had no ETag")
        })
}

/// Part size for a known total: at least `min`, large enough to fit in `max_parts`, rounded
/// up to a whole MiB.
pub(crate) fn part_size_for(total: u64, min: u64, max_parts: u64) -> u64 {
    const MIB: u64 = 1024 * 1024;
    let needed = total.div_ceil(max_parts.max(1));
    if needed <= min { min } else { needed.div_ceil(MIB).saturating_mul(MIB) }
}

/// Part size for part `n` (1-based) of an upload of unknown size: `min` for the first
/// 1000 parts, then growing by `min` every 1000 parts, so 10 000 parts reach 55 × 1000 × `min`
/// (440 GB at 8 MiB) while memory stays at one part.
pub(crate) fn part_size_unknown(n: u32, min: u64) -> u64 {
    min * (1 + u64::from(n.saturating_sub(1)) / 1000)
}
