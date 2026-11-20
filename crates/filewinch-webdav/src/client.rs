//! The shared session state and the request loop: authentication (challenge, retry, cached
//! scheme and nonce), one same-origin redirect, transport and status error mapping.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use filewinch_core::{Error, ErrorKind, RPath, Result};
use filewinch_http::Http;
use filewinch_http::ureq::{self, Body, http};

use crate::auth::{self, Scheme};
use crate::multistatus::{self, PropEntry};
use crate::target::Target;

/// Largest PROPFIND reply we read (a folder of ~500k items).
const MAX_LISTING: u64 = 256 * 1024 * 1024;

const PROPFIND_BODY: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<D:propfind xmlns:D="DAV:"><D:prop>"#,
    "<D:resourcetype/><D:getcontentlength/><D:getlastmodified/><D:getetag/><D:getcontenttype/>",
    "</D:prop></D:propfind>"
);

pub(crate) struct Creds {
    pub user: String,
    pub pass: String,
}

impl std::fmt::Debug for Creds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Creds").field("user", &self.user).finish_non_exhaustive()
    }
}

pub(crate) struct Client {
    pub http: Http,
    pub target: Target,
    pub creds: Option<Creds>,
    auth: Mutex<Scheme>,
    closed: AtomicBool,
}

/// What a request was trying to do, for messages: "list /photos", "upload report.pdf".
pub(crate) type What<'a> = &'a str;

impl Client {
    /// `trusted`: the site's "always trust" certificate fingerprints.
    pub fn new(target: Target, creds: Option<Creds>, trusted: &[String]) -> Client {
        let http = Http::new();
        http.trust_fingerprints(trusted);
        Client { http, target, creds, auth: Mutex::new(Scheme::None), closed: AtomicBool::new(false) }
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    pub fn check_open(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Error::new(
                ErrorKind::Closed,
                format!("The connection to {} was closed.", self.target.host_name()),
            ));
        }
        Ok(())
    }

    fn scheme(&self) -> MutexGuard<'_, Scheme> {
        self.auth.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The `Authorization` value for a request, if we've been challenged and have creds.
    pub fn authorization(&self, method: &str, request_path: &str) -> Option<String> {
        let creds = self.creds.as_ref()?;
        match &mut *self.scheme() {
            Scheme::None => None,
            Scheme::Basic => Some(auth::basic(&creds.user, &creds.pass)),
            Scheme::Digest(d) => Some(d.authorize(&creds.user, &creds.pass, method, request_path, &auth::cnonce())),
        }
    }

    /// Handles a 401: updates the cached scheme from the challenge and says whether to send
    /// the request again (first challenge, or a stale Digest nonce). `Err` when there's
    /// nothing more to try.
    pub fn on_unauthorized(&self, headers: &http::HeaderMap, sent_auth: bool) -> Result<bool> {
        if self.creds.is_none() {
            return Err(self.auth_error());
        }
        let values = headers.get_all("www-authenticate").iter().filter_map(|v| v.to_str().ok());
        let challenges = auth::parse_challenges(values);
        let (scheme, stale) = auth::choose(&challenges).map_err(|offered| {
            Error::new(
                ErrorKind::Unsupported,
                format!("{} asks for a sign-in method Filewinch doesn't support.", self.target.host_name()),
            )
            .with_detail(format!("WWW-Authenticate: {offered}"))
        })?;
        *self.scheme() = scheme;
        Ok(!sent_auth || stale)
    }

    pub fn auth_error(&self) -> Error {
        let host = self.target.host_name();
        match &self.creds {
            Some(c) if !c.user.is_empty() => Error::new(
                ErrorKind::Auth,
                format!("The server refused the user name or password for {}@{host}.", c.user),
            ),
            _ => Error::new(ErrorKind::Auth, format!("{host} needs a user name and password.")),
        }
        .with_detail("HTTP 401")
    }

    pub fn transport(&self, e: ureq::Error) -> Error {
        self.http.transport_error(e, self.target.host_name())
    }

    /// Sends a request with a small (replayable) body, signing in and following one
    /// same-origin redirect as needed. Returns any final response, success or not.
    pub fn send(
        &self,
        method: &str,
        request_path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Result<http::Response<Body>> {
        let mut path = request_path.to_string();
        let mut redirected = false;
        let mut auth_retries = 0;
        loop {
            self.check_open()?;
            let authz = self.authorization(method, &path);
            let mut req = http::Request::builder().method(method).uri(self.target.url(&path));
            for (k, v) in headers {
                req = req.header(*k, *v);
            }
            if let Some(a) = &authz {
                req = req.header("authorization", a);
            }
            if !body.is_empty() {
                req = req.header("content-type", "application/xml; charset=utf-8");
            }
            let sent = if body.is_empty() {
                req.body(()).map(|r| self.http.agent().run(r))
            } else {
                req.body(body.to_string()).map(|r| self.http.agent().run(r))
            };
            let resp = sent.map_err(|e| self.transport(e.into()))?.map_err(|e| self.transport(e))?;
            match resp.status().as_u16() {
                401 => {
                    if auth_retries < 2 && self.on_unauthorized(resp.headers(), authz.is_some())? {
                        auth_retries += 1;
                        continue;
                    }
                    return Err(self.auth_error());
                }
                301 | 302 | 303 | 307 | 308 if !redirected => {
                    let Some(loc) = resp.headers().get("location").and_then(|v| v.to_str().ok()) else {
                        return Ok(resp);
                    };
                    path = self.target.resolve_location(loc, &path)?;
                    redirected = true;
                }
                _ => return Ok(resp),
            }
        }
    }

    /// The generic failure for a non-success response.
    pub fn fail(&self, resp: http::Response<Body>, what: What) -> Error {
        match resp.status().as_u16() {
            401 => self.auth_error(),
            s @ 300..=399 => Error::new(
                ErrorKind::Other,
                format!("Couldn't {what}: the server redirected the request more than once."),
            )
            .with_detail(format!("HTTP {s}")),
            _ => filewinch_http::status_error(resp, what),
        }
    }

    /// PROPFIND `path` at `depth` (0 or 1). Returns the parsed responses.
    pub fn propfind(&self, path: &RPath, depth: u8, collection: bool, what: What) -> Result<Vec<PropEntry>> {
        let rp = self.target.request_path(path, collection);
        let depth = depth.to_string();
        let resp = self.send("PROPFIND", &rp, &[("depth", &depth)], PROPFIND_BODY)?;
        let status = resp.status().as_u16();
        if status == 405 || status == 501 {
            // Some servers answer PROPFIND on a missing path with 405; tell them apart.
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!("Couldn't {what}: {} doesn't accept WebDAV requests at this address.", self.target.host_name()),
            )
            .with_detail(format!("HTTP {status} to PROPFIND")));
        }
        if !(200..300).contains(&status) {
            return Err(self.fail(resp, what));
        }
        let mut buf = Vec::new();
        resp.into_body()
            .into_reader()
            .take(MAX_LISTING)
            .read_to_end(&mut buf)
            .map_err(filewinch_http::from_io)?;
        multistatus::parse(&buf).map_err(|why| {
            Error::new(
                ErrorKind::Other,
                format!("Couldn't {what}: {} sent a reply Filewinch couldn't read.", self.target.host_name()),
            )
            .with_detail(why)
        })
    }
}
