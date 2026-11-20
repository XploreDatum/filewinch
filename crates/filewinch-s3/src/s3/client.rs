//! Signed S3 requests: addressing, SigV4, per-bucket region discovery, one retry for
//! transient failures, and error mapping.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use filewinch_core::{Error, ErrorKind, Result};
use filewinch_http::Http;
use filewinch_http::ureq::{self, Body, http::Response};

use super::endpoint::Endpoint;
use super::errors::{map_error, parse_error};
use crate::sigv4::{self, Credentials, UNSIGNED_PAYLOAD, canonical_query, sha256_hex};
use crate::time::amz_date;

/// Largest error or XML body read into memory (ListObjectsV2 pages are ~0.5 MB).
const MAX_XML: u64 = 32 * 1024 * 1024;

/// Longest server-requested wait honoured inside one call before giving up to the engine.
pub(crate) const MAX_INLINE_WAIT: Duration = Duration::from_secs(2);

/// Sizes that decide single PUT vs multipart, and multipart part sizes. Fields so tests can
/// shrink them; production uses [`Limits::default`].
#[derive(Clone, Debug)]
pub(crate) struct Limits {
    /// Known sizes below this go up in one PUT.
    pub single_put_below: u64,
    /// Smallest multipart part (S3's floor is 5 MiB).
    pub part_size: u64,
    /// S3's part count ceiling.
    pub max_parts: u64,
    /// Largest object CopyObject can copy in one request (5 GiB).
    pub copy_max: u64,
    /// Part size for UploadPartCopy.
    pub copy_part: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            single_put_below: 16 * 1024 * 1024,
            part_size: 8 * 1024 * 1024,
            max_parts: 10_000,
            copy_max: 5 * 1024 * 1024 * 1024,
            copy_part: 512 * 1024 * 1024,
        }
    }
}

/// One request. `body` is hashed into the signature; streamed bodies use [`Client::put_stream`].
pub(crate) struct Call<'a> {
    pub method: &'static str,
    pub bucket: Option<&'a str>,
    pub key: &'a str,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: &'a [u8],
}

impl<'a> Call<'a> {
    pub fn new(method: &'static str, bucket: Option<&'a str>, key: &'a str) -> Self {
        Call { method, bucket, key, query: Vec::new(), headers: Vec::new(), body: &[] }
    }
    pub fn query(mut self, k: &str, v: impl Into<String>) -> Self {
        self.query.push((k.into(), v.into()));
        self
    }
    pub fn header(mut self, k: &str, v: impl Into<String>) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
    pub fn body(mut self, body: &'a [u8]) -> Self {
        self.body = body;
        self
    }
}

pub(crate) struct Client {
    pub http: Http,
    pub ep: Endpoint,
    creds: Credentials,
    default_region: String,
    /// Region per bucket, learned from redirects and `x-amz-bucket-region`.
    regions: Mutex<HashMap<String, String>>,
    /// Buckets whose region has been confirmed before a streamed upload.
    probed: Mutex<HashSet<String>>,
    closed: AtomicBool,
    pub limits: Limits,
}

impl Client {
    pub fn new(http: Http, ep: Endpoint, creds: Credentials, region: String) -> Client {
        Client {
            http,
            ep,
            creds,
            default_region: region,
            regions: Mutex::new(HashMap::new()),
            probed: Mutex::new(HashSet::new()),
            closed: AtomicBool::new(false),
            limits: Limits::default(),
        }
    }

    pub fn host(&self) -> String {
        self.ep.display()
    }

    pub fn default_region(&self) -> &str {
        &self.default_region
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    pub fn check_open(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Error::new(ErrorKind::Closed, "The connection was closed."));
        }
        Ok(())
    }

    pub fn region(&self, bucket: Option<&str>) -> String {
        bucket
            .and_then(|b| lock(&self.regions).get(b).cloned())
            .unwrap_or_else(|| self.default_region.clone())
    }

    pub fn set_region(&self, bucket: &str, region: &str) {
        lock(&self.regions).insert(bucket.to_string(), region.to_string());
    }

    /// Before a streamed upload (which can't be replayed), learns the bucket's region with a
    /// HEAD if it hasn't been seen yet. Failures are ignored: the upload reports its own.
    pub fn ensure_region(&self, bucket: &str) {
        if !lock(&self.probed).insert(bucket.to_string()) || lock(&self.regions).contains_key(bucket) {
            return;
        }
        let _ = self.send(&Call::new("HEAD", Some(bucket), ""), "check the bucket");
    }

    /// (URL with query, headers to send including Authorization).
    fn sign(&self, call: &Call<'_>, region: &str, payload_hash: &str) -> (String, Vec<(String, String)>) {
        let (authority, path, url) = self.ep.locate(region, call.bucket, call.key);
        let date = amz_date(SystemTime::now());
        let mut headers = call.headers.clone();
        headers.push(("x-amz-date".into(), date.clone()));
        headers.push(("x-amz-content-sha256".into(), payload_hash.into()));
        let mut signed = headers.clone();
        signed.push(("host".into(), authority));
        let req = sigv4::Request {
            method: call.method,
            canonical_uri: &path,
            query: &call.query,
            headers: &signed,
            payload_hash,
        };
        let auth = sigv4::authorization(&self.creds, region, "s3", &date, &req);
        headers.push(("authorization".into(), auth));
        let query = canonical_query(&call.query);
        let url = if query.is_empty() { url } else { format!("{url}?{query}") };
        (url, headers)
    }

    /// Sends a request with a hashed body. Ok only for 2xx; anything else is mapped to an
    /// error (after one region switch, and one retry of a dropped connection or 5xx for
    /// non-POST requests). `what` is a phrase like "list /photos".
    pub fn send(&self, call: &Call<'_>, what: &str) -> Result<Response<Body>> {
        self.check_open()?;
        let hash = sha256_hex(call.body);
        let mut retried = false;
        let mut switched = false;
        loop {
            let region = self.region(call.bucket);
            let (url, headers) = self.sign(call, &region, &hash);
            let mut builder = ureq::http::Request::builder().method(call.method).uri(&url);
            for (k, v) in &headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
            let bad_request = |e: ureq::http::Error| {
                Error::new(ErrorKind::Other, format!("Couldn't {what}: the request couldn't be built.")).with_detail(e)
            };
            let result = if call.body.is_empty() && call.method != "PUT" && call.method != "POST" {
                self.http.agent().run(builder.body(()).map_err(bad_request)?)
            } else {
                self.http.agent().run(builder.body(call.body).map_err(bad_request)?)
            };
            let resp = match result {
                Ok(resp) => resp,
                Err(e) => {
                    let err = self.http.transport_error(e, &self.host());
                    if err.kind == ErrorKind::Network && !retried && call.method != "POST" {
                        retried = true;
                        std::thread::sleep(Duration::from_millis(300));
                        continue;
                    }
                    return Err(err);
                }
            };
            let status = resp.status().as_u16();
            if (200..300).contains(&status) {
                return Ok(resp);
            }
            let headers = resp.headers().clone();
            let body = if call.method == "HEAD" { String::new() } else { read_limited(resp, MAX_XML) };
            if let (Some(bucket), false, Some(hint)) =
                (call.bucket, switched, region_hint(status, &headers, &body).filter(|r| *r != region))
            {
                self.set_region(bucket, &hint);
                switched = true;
                continue;
            }
            let wait = filewinch_http::retry_after(&headers).unwrap_or(Duration::from_millis(500));
            // One quick retry for a passing 5xx; longer waits are the engine's to schedule.
            if matches!(status, 500 | 502 | 503 | 504) && !retried && call.method != "POST" && wait <= MAX_INLINE_WAIT {
                retried = true;
                std::thread::sleep(wait);
                continue;
            }
            return Err(map_error(status, &headers, &body, what, &self.host()));
        }
    }

    /// Sends `reader` as a PUT body of exactly `len` bytes (UNSIGNED-PAYLOAD). The bucket's
    /// region should already be known ([`Client::ensure_region`]); a streamed body can't be
    /// replayed after a redirect.
    pub fn put_stream(&self, call: &Call<'_>, reader: impl Read + Send, len: u64, what: &str) -> Result<Response<Body>> {
        self.check_open()?;
        let region = self.region(call.bucket);
        let (url, headers) = self.sign(call, &region, UNSIGNED_PAYLOAD);
        let mut req = self.http.agent().put(&url);
        for (k, v) in &headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let resp = filewinch_http::send_reader(req, reader, Some(len)).map_err(|e| {
            let err = self.http.transport_error(e, &self.host());
            // A cancelled pipe surfaces as the transport error wrapping our Cancelled.
            if err.kind == ErrorKind::Cancelled { Error::cancelled() } else { err }
        })?;
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(resp);
        }
        let headers = resp.headers().clone();
        let body = read_limited(resp, MAX_XML);
        // Too late for this body, but the engine's retry will use the right region.
        if let (Some(bucket), Some(hint)) = (call.bucket, region_hint(status, &headers, &body)) {
            self.set_region(bucket, &hint);
        }
        Err(map_error(status, &headers, &body, what, &self.host()))
    }

    /// Reads a success body (XML) as text.
    pub fn text(&self, resp: Response<Body>, what: &str) -> Result<String> {
        let mut buf = Vec::new();
        resp.into_body()
            .into_reader()
            .take(MAX_XML)
            .read_to_end(&mut buf)
            .map_err(|e| crate::body::body_error(e, what, &self.host()))?;
        String::from_utf8(buf).map_err(|_| {
            Error::new(ErrorKind::Other, format!("Couldn't {what}: {} sent text that isn't UTF-8.", self.host()))
        })
    }

    /// A 200 body that is really an `<Error>` (CopyObject, UploadPartCopy,
    /// CompleteMultipartUpload can do this) becomes an error.
    pub fn check_body_error(&self, body: &str, what: &str) -> Result<()> {
        if let Some(e) = parse_error(body) {
            let mut err = map_error(500, &Default::default(), body, what, &self.host());
            if e.code.is_empty() {
                err.kind = ErrorKind::Network;
            }
            return Err(err);
        }
        Ok(())
    }
}

/// The region a failed response points at, if it says one.
fn region_hint(status: u16, headers: &ureq::http::HeaderMap, body: &str) -> Option<String> {
    if !matches!(status, 301 | 307 | 400 | 403) {
        return None;
    }
    if let Some(r) = headers.get("x-amz-bucket-region").and_then(|v| v.to_str().ok()) {
        let r = r.trim();
        if !r.is_empty() {
            return Some(r.to_string());
        }
    }
    let e = parse_error(body)?;
    match e.code.as_str() {
        "AuthorizationHeaderMalformed" | "PermanentRedirect" | "TemporaryRedirect" | "IncorrectEndpoint" => e.region,
        _ => None,
    }
}

fn read_limited(resp: Response<Body>, limit: u64) -> String {
    let mut buf = Vec::new();
    let _ = resp.into_body().into_reader().take(limit).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
