//! Building, signing and sending Blob service requests.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use filewinch_core::{Error, ErrorKind, Result};
use filewinch_http::ureq::http::{Request, Response};
use filewinch_http::ureq::{self, Body};
use filewinch_http::{Http, MAX_DETAIL, from_io, send_reader};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

use crate::config::{Config, Credential};
use crate::errors::{Ctx, azure_error};
use crate::sign;

/// The Blob service REST version every request asks for.
pub(crate) const API_VERSION: &str = "2021-08-06";

/// Everything but unreserved characters is encoded in query values.
const QUERY: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');
/// Paths keep `/` between segments.
const PATH: &AsciiSet = &QUERY.remove(b'/');

/// Largest listing body accepted (5000 entries with 1 KiB names is ~6 MB).
const MAX_LISTING: u64 = 64 * 1024 * 1024;

/// One request: what to address and the extra query parameters and headers.
pub(crate) struct Call<'a> {
    pub method: &'static str,
    /// None addresses the account (List Containers).
    pub container: Option<&'a str>,
    /// Blob name (decoded); None addresses the container.
    pub blob: Option<&'a str>,
    pub query: Vec<(&'static str, String)>,
    /// Lowercase names.
    pub headers: Vec<(&'static str, String)>,
}

impl<'a> Call<'a> {
    pub fn new(method: &'static str, container: Option<&'a str>, blob: Option<&'a str>) -> Self {
        Call { method, container, blob, query: Vec::new(), headers: Vec::new() }
    }
    pub fn q(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.query.push((name, value.into()));
        self
    }
    pub fn h(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

/// A signed request ready to send. No `Debug`: the URL may carry a SAS token.
struct Prepared {
    url: String,
    headers: Vec<(String, String)>,
}

pub(crate) struct Client {
    http: Http,
    pub cfg: Config,
    /// `maxresults` for listings (5000 is the service maximum).
    pub page_size: u32,
    closed: AtomicBool,
}

impl Client {
    /// `trusted`: the site's "always trust" certificate fingerprints.
    pub fn new(cfg: Config, page_size: u32, trusted: &[String]) -> Client {
        let http = Http::new();
        http.trust_fingerprints(trusted);
        Client { http, cfg, page_size, closed: AtomicBool::new(false) }
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub fn check_open(&self) -> Result<()> {
        if self.is_closed() {
            return Err(Error::new(ErrorKind::Closed, format!("The connection to {} was closed.", self.cfg.account)));
        }
        Ok(())
    }

    pub fn is_sas(&self) -> bool {
        matches!(self.cfg.credential, Credential::Sas(_))
    }

    /// The encoded URI path for a container/blob (with the path-style account prefix).
    fn path(&self, container: Option<&str>, blob: Option<&str>) -> String {
        let mut p = format!("{}/", self.cfg.path_prefix);
        if let Some(c) = container {
            p.push_str(&utf8_percent_encode(c, PATH).to_string());
            if let Some(b) = blob {
                p.push('/');
                p.push_str(&utf8_percent_encode(b, PATH).to_string());
            }
        }
        p
    }

    /// The URL of a blob as a copy source, with the SAS token when that's the credential.
    pub fn source_url(&self, container: &str, blob: &str) -> String {
        let mut url = format!("{}{}", self.cfg.origin, self.path(Some(container), Some(blob)));
        if let Credential::Sas(q) = &self.cfg.credential {
            url.push('?');
            url.push_str(q);
        }
        url
    }

    fn prepare(&self, call: &Call, content_length: Option<u64>) -> Prepared {
        let path = self.path(call.container, call.blob);
        let mut query: Vec<String> = call
            .query
            .iter()
            .map(|(k, v)| format!("{k}={}", utf8_percent_encode(v, QUERY)))
            .collect();
        if let Credential::Sas(q) = &self.cfg.credential {
            query.push(q.clone());
        }
        let mut url = format!("{}{path}", self.cfg.origin);
        if !query.is_empty() {
            url.push('?');
            url.push_str(&query.join("&"));
        }
        let mut headers: Vec<(String, String)> =
            call.headers.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        headers.push(("x-ms-version".into(), API_VERSION.into()));
        headers.push(("x-ms-date".into(), httpdate::fmt_http_date(SystemTime::now())));
        if let Some(n) = content_length {
            headers.push(("content-length".into(), n.to_string()));
        }
        if let Credential::SharedKey(key) = &self.cfg.credential {
            let decoded: Vec<(String, String)> =
                call.query.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
            let sts = sign::string_to_sign(call.method, &self.cfg.account, &path, &decoded, &headers);
            let sig = sign::signature(key, &sts);
            headers.push(("authorization".into(), format!("SharedKey {}:{sig}", self.cfg.account)));
        }
        Prepared { url, headers }
    }

    /// Sends a request with a small in-memory body (PUT) or none, and returns the response
    /// whatever its status. Only transport failures are errors.
    pub fn send_raw(&self, call: &Call, body: &[u8]) -> Result<Response<Body>> {
        self.check_open()?;
        self.send_raw_unchecked(call, body)
    }

    /// [`Client::send_raw`] even after `close` (for cleanup requests).
    pub fn send_raw_unchecked(&self, call: &Call, body: &[u8]) -> Result<Response<Body>> {
        let has_body = call.method == "PUT" || call.method == "POST";
        let p = self.prepare(call, has_body.then_some(body.len() as u64));
        let mut b = Request::builder().method(call.method).uri(p.url.as_str());
        for (k, v) in &p.headers {
            b = b.header(k.as_str(), v.as_str());
        }
        let result = if has_body {
            b.body(body).map_err(ureq::Error::from).and_then(|r| self.http.agent().run(r))
        } else {
            b.body(()).map_err(ureq::Error::from).and_then(|r| self.http.agent().run(r))
        };
        result.map_err(|e| self.http.transport_error(e, &self.cfg.host))
    }

    /// [`Client::send_raw`], turning a non-success status into an error about `what`.
    pub fn send(&self, call: &Call, body: &[u8], what: &str) -> Result<Response<Body>> {
        let resp = self.send_raw(call, body)?;
        self.ok(resp, what)
    }

    /// PUT with a streamed body of exactly `len` bytes (sent with Content-Length).
    pub fn send_stream(&self, call: &Call, reader: impl Read + Send, len: u64, what: &str) -> Result<Response<Body>> {
        self.check_open()?;
        let p = self.prepare(call, Some(len));
        let mut b = self.http.agent().put(p.url.as_str());
        for (k, v) in &p.headers {
            // send_reader sets Content-Length itself (the signed value is the same).
            if k != "content-length" {
                b = b.header(k.as_str(), v.as_str());
            }
        }
        let resp = send_reader(b, reader, Some(len)).map_err(|e| self.http.transport_error(e, &self.cfg.host))?;
        self.ok(resp, what)
    }

    /// Passes a 2xx response through; otherwise reads the error body and maps it.
    pub fn ok(&self, resp: Response<Body>, what: &str) -> Result<Response<Body>> {
        if resp.status().is_success() {
            return Ok(resp);
        }
        Err(self.error(resp, what))
    }

    pub fn error(&self, resp: Response<Body>, what: &str) -> Error {
        let (parts, body) = resp.into_parts();
        let mut buf = Vec::new();
        // A failed body read only loses the detail; the status and header still decide.
        let _ = body.into_reader().take(MAX_DETAIL as u64).read_to_end(&mut buf);
        let ctx = Ctx { what, account: &self.cfg.account, sas: self.is_sas() };
        azure_error(parts.status.as_u16(), &parts.headers, &String::from_utf8_lossy(&buf), &ctx)
    }

    /// Reads a listing body (bounded).
    pub fn text(&self, resp: Response<Body>) -> Result<String> {
        let mut buf = Vec::new();
        resp.into_body()
            .into_reader()
            .take(MAX_LISTING)
            .read_to_end(&mut buf)
            .map_err(|e| network(from_io(e), &self.cfg.host))?;
        String::from_utf8(buf).map_err(|_| Error::new(ErrorKind::Other, "Azure sent a listing that isn't UTF-8."))
    }

    /// Sleeps up to `d`, waking early (with `Closed`) if the session is closed.
    pub fn sleep(&self, d: Duration) -> Result<()> {
        let end = std::time::Instant::now() + d;
        loop {
            self.check_open()?;
            let now = std::time::Instant::now();
            if now >= end {
                return Ok(());
            }
            std::thread::sleep((end - now).min(Duration::from_millis(100)));
        }
    }
}

/// Body read failures are connection problems unless they already say otherwise.
pub(crate) fn network(mut e: Error, host: &str) -> Error {
    if e.kind == ErrorKind::Other {
        e.kind = ErrorKind::Network;
        e.message = format!("The connection to {host} was interrupted.");
    }
    e
}

pub(crate) fn header(resp: &Response<Body>, name: &str) -> Option<String> {
    resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
}
