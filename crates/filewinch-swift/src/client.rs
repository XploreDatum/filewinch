//! The signed-in HTTP client: the shared token, requests with one re-sign-in on 401, URL
//! building and Swift's error mapping.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use filewinch_core::{Error, ErrorKind, RPath, Result};
use filewinch_http::ureq::Body;
use filewinch_http::ureq::http::{Request, Response};
use filewinch_http::{Http, MAX_DETAIL, status_error_parts};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

use crate::Tuning;
use crate::auth::{Session, authenticate};
use crate::config::AuthConfig;

/// Renew a token this long before its known expiry.
const RENEW_EARLY: Duration = Duration::from_secs(60);
/// Largest listing or JSON reply read into memory.
pub(crate) const MAX_JSON: u64 = 64 * 1024 * 1024;

/// Everything but RFC 3986 unreserved characters.
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');
/// Object names keep their slashes.
const OBJECT: &AsciiSet = &COMPONENT.remove(b'/');

/// Percent-encodes a query value or container name.
pub(crate) fn enc(s: &str) -> String {
    utf8_percent_encode(s, COMPONENT).to_string()
}

/// `/<container>` or `/<container>/<object>`, encoded.
pub(crate) fn url_path(container: &str, object: Option<&str>) -> String {
    match object {
        Some(o) => format!("/{}/{}", enc(container), utf8_percent_encode(o, OBJECT)),
        None => format!("/{}", enc(container)),
    }
}

/// Where a path lives in Swift.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Loc {
    Root,
    Container(String),
    Object { container: String, key: String },
}

impl Loc {
    pub fn of(p: &RPath) -> Loc {
        let mut parts = p.components();
        match parts.next() {
            None => Loc::Root,
            Some(c) => {
                let rest: Vec<&str> = parts.collect();
                if rest.is_empty() {
                    Loc::Container(c.to_string())
                } else {
                    Loc::Object { container: c.to_string(), key: rest.join("/") }
                }
            }
        }
    }
}

/// The current session plus its generation (bumped on every re-sign-in).
#[derive(Clone)]
pub(crate) struct Cur {
    pub s: Arc<Session>,
    pub generation: u64,
}

pub(crate) struct Client {
    pub http: Http,
    cfg: AuthConfig,
    live: Mutex<Cur>,
    closed: AtomicBool,
    pub tuning: Tuning,
}

impl Client {
    pub fn connect(cfg: AuthConfig, tuning: Tuning) -> Result<Client> {
        let http = Http::new();
        http.trust_fingerprints(&cfg.trusted);
        let s = authenticate(&http, &cfg)?;
        Ok(Client {
            http,
            cfg,
            live: Mutex::new(Cur { s: Arc::new(s), generation: 0 }),
            closed: AtomicBool::new(false),
            tuning,
        })
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    pub fn check_open(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Error::new(ErrorKind::Closed, "This connection was closed."));
        }
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Cur> {
        self.live.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The session to use now, renewed first if it is about to expire.
    pub fn current(&self) -> Result<Cur> {
        self.check_open()?;
        let cur = self.lock().clone();
        let expiring = cur.s.expires.is_some_and(|t| t <= SystemTime::now() + RENEW_EARLY);
        if expiring {
            self.reauth(cur.generation)?;
            return Ok(self.lock().clone());
        }
        Ok(cur)
    }

    /// Signs in again unless another thread already did since `generation`.
    pub fn reauth(&self, generation: u64) -> Result<()> {
        let mut live = self.lock();
        if live.generation != generation {
            return Ok(());
        }
        let s = authenticate(&self.http, &self.cfg)?;
        *live = Cur { s: Arc::new(s), generation: generation + 1 };
        Ok(())
    }

    /// For a streamed request that got 401: sign in again so the engine's retry works, and
    /// say so; if signing in fails, return that.
    pub fn expired_mid_stream(&self, generation: u64, what: &str) -> Error {
        match self.reauth(generation) {
            Ok(()) => Error::new(
                ErrorKind::Network,
                format!("Couldn't {what}: the sign-in expired part-way. Filewinch signed in again; retrying."),
            )
            .with_detail("HTTP 401"),
            Err(e) => e,
        }
    }

    pub fn transport(&self, e: filewinch_http::ureq::Error, cur: &Cur) -> Error {
        self.http.transport_error(e, &cur.s.host)
    }

    /// Sends a request to `<storage url><path_query>`. A 401 signs in again and repeats it
    /// once; a second 401 is `Auth`. Other statuses are returned for the caller to check.
    pub fn send(
        &self,
        method: &str,
        path_query: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
        what: &str,
    ) -> Result<Response<Body>> {
        let mut retried = false;
        loop {
            let cur = self.current()?;
            let resp = self.send_once(&cur, method, path_query, headers, body)?;
            if resp.status().as_u16() != 401 {
                return Ok(resp);
            }
            if retried {
                let mut e = self.fail(resp, what);
                e.message = format!(
                    "Couldn't {what}: {} keeps refusing the sign-in for {}.",
                    cur.s.host, self.cfg.user
                );
                return Err(e);
            }
            retried = true;
            self.reauth(cur.generation)?;
        }
    }

    fn send_once(
        &self,
        cur: &Cur,
        method: &str,
        path_query: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<Response<Body>> {
        let mut b = Request::builder()
            .method(method)
            .uri(format!("{}{path_query}", cur.s.storage_url))
            .header("x-auth-token", cur.s.token.as_str());
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let bad = |e: filewinch_http::ureq::http::Error| {
            Error::new(ErrorKind::InvalidPath, "That name can't be used on this server.").with_detail(e)
        };
        let agent = self.http.agent();
        let r = match body {
            Some(bytes) => agent.run(b.body(bytes).map_err(bad)?),
            None => agent.run(b.body(()).map_err(bad)?),
        };
        r.map_err(|e| self.transport(e, cur))
    }

    /// Maps a failed response to an error, with Swift's own statuses.
    pub fn fail(&self, resp: Response<Body>, what: &str) -> Error {
        let (parts, body) = resp.into_parts();
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut std::io::Read::take(body.into_reader(), MAX_DETAIL as u64), &mut buf);
        swift_error(parts.status.as_u16(), &parts.headers, &String::from_utf8_lossy(&buf), what)
    }

    /// Reads a JSON body.
    pub fn json<T: serde::de::DeserializeOwned>(&self, mut resp: Response<Body>, what: &str) -> Result<T> {
        let text = resp.body_mut().with_config().limit(MAX_JSON).read_to_string().map_err(|e| {
            Error::new(ErrorKind::Network, format!("Couldn't {what}: the reply was cut off.")).with_detail(e)
        })?;
        serde_json::from_str(&text).map_err(|e| {
            Error::new(ErrorKind::Other, format!("Couldn't {what}: the server's reply wasn't understood."))
                .with_detail(e)
        })
    }
}

/// `status_error_parts` plus Swift's quirks: 413 can mean a quota, 498 is rate limiting.
pub(crate) fn swift_error(status: u16, headers: &filewinch_http::ureq::http::HeaderMap, body: &str, what: &str) -> Error {
    let mut e = status_error_parts(status, headers, body, what);
    match status {
        413 if body.to_ascii_lowercase().contains("quota") => {
            e.message = format!("Couldn't {what}: the account or container quota is full.");
        }
        413 => e.message = format!("Couldn't {what}: the file is larger than this server allows."),
        498 => {
            e.kind = ErrorKind::RateLimited;
            e.message = format!("Couldn't {what}: the server is limiting requests, try again shortly.");
        }
        _ => {}
    }
    e
}

/// Unquoted, lower-case ETag header value.
pub(crate) fn etag(resp: &Response<Body>) -> Option<String> {
    resp.headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().trim_matches('"').to_ascii_lowercase())
        .filter(|v| !v.is_empty())
}
