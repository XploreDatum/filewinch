//! One authorised HTTP session per connected drive: bearer tokens with refresh-once-on-401,
//! status → [`ErrorKind`] mapping with human messages, `Retry-After`, and close handling.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use filewinch_core::{Error, ErrorKind, Result};
use serde::de::DeserializeOwned;
use serde_json::Value;
use ureq::Agent;
use ureq::http::HeaderMap;

use crate::oauth::token::TokenSource;
use crate::oauth::{Provider, form_encode};

pub(crate) type Response = ureq::http::Response<ureq::Body>;

/// Lets a provider override the default status → kind mapping from its error body
/// (Dropbox answers 409 for "not found", Drive 403 for rate limits, …).
pub(crate) type Classify = fn(status: u16, body: &Value) -> Option<ErrorKind>;

/// The shared HTTP client: connect/response timeouts but no whole-body timeout (large
/// downloads stream for as long as they need), status codes returned rather than raised.
pub(crate) fn agent() -> Agent {
    Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_send_request(Some(Duration::from_secs(60)))
        .timeout_recv_response(Some(Duration::from_secs(120)))
        .user_agent(concat!("Filewinch/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

pub(crate) fn transport_error(e: ureq::Error, service: &str) -> Error {
    let kind = match &e {
        ureq::Error::Io(io) => Error::from(std::io::Error::new(io.kind(), io.to_string())).kind,
        ureq::Error::Timeout(_)
        | ureq::Error::HostNotFound
        | ureq::Error::ConnectionFailed
        | ureq::Error::Protocol(_)
        | ureq::Error::BodyStalled => ErrorKind::Network,
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) => ErrorKind::HostTrust,
        _ => ErrorKind::Other,
    };
    let message = match kind {
        ErrorKind::HostTrust => format!("Filewinch couldn't verify {service}'s secure connection."),
        ErrorKind::Network | ErrorKind::Other => {
            format!("Filewinch couldn't reach {service}. Check your internet connection.")
        }
        _ => format!("The connection to {service} failed."),
    };
    let kind = if kind == ErrorKind::Other {
        ErrorKind::Network
    } else {
        kind
    };
    Error::new(kind, message).with_detail(e)
}

/// Reads a whole (small) response body.
pub(crate) fn read_body(resp: &mut Response, service: &str) -> Result<Vec<u8>> {
    resp.body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_vec()
        .map_err(|e| transport_error(e, service))
}

/// `Retry-After` as seconds or an HTTP date.
pub(crate) fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let v = headers.get("retry-after")?.to_str().ok()?.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = httpdate::parse_http_date(v).ok()?;
    Some(
        at.duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

/// The body of a successful GET sent with `filewinch_core::range_header(offset, len)`, as
/// exactly that range; anything else (a 200 with the whole file) is `Error::range_ignored`.
pub(crate) fn ranged_body(resp: Response, offset: u64, len: u64, what: &str) -> Result<Box<dyn std::io::Read + Send>> {
    let status = resp.status().as_u16();
    let n = filewinch_core::check_range_reply(status, header(&resp, "content-range").as_deref(), offset, len)?;
    Ok(filewinch_core::exact_reader(Box::new(resp.into_body().into_reader()), n, what))
}

pub(crate) fn header(resp: &Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// One request. Built with the constructors, sent with [`Session::send`].
pub(crate) struct Call<'a> {
    method: &'static str,
    url: String,
    query: Vec<(&'a str, String)>,
    headers: Vec<(&'a str, String)>,
    body: Option<(Cow<'a, [u8]>, &'a str)>,
    subject: Option<String>,
    classify: Option<Classify>,
    accept: &'a [u16],
    auth: bool,
    redirects: bool,
}

impl<'a> Call<'a> {
    pub(crate) fn new(method: &'static str, url: impl Into<String>) -> Self {
        Call {
            method,
            url: url.into(),
            query: Vec::new(),
            headers: Vec::new(),
            body: None,
            subject: None,
            classify: None,
            accept: &[],
            auth: true,
            redirects: true,
        }
    }
    pub(crate) fn get(url: impl Into<String>) -> Self {
        Self::new("GET", url)
    }
    pub(crate) fn post(url: impl Into<String>) -> Self {
        Self::new("POST", url)
    }
    pub(crate) fn put(url: impl Into<String>) -> Self {
        Self::new("PUT", url)
    }
    pub(crate) fn patch(url: impl Into<String>) -> Self {
        Self::new("PATCH", url)
    }
    pub(crate) fn delete(url: impl Into<String>) -> Self {
        Self::new("DELETE", url)
    }
    pub(crate) fn query(mut self, k: &'a str, v: impl Into<String>) -> Self {
        self.query.push((k, v.into()));
        self
    }
    pub(crate) fn header(mut self, k: &'a str, v: impl Into<String>) -> Self {
        self.headers.push((k, v.into()));
        self
    }
    pub(crate) fn json(mut self, v: &Value) -> Self {
        self.body = Some((Cow::Owned(v.to_string().into_bytes()), "application/json"));
        self
    }
    pub(crate) fn bytes(mut self, data: impl Into<Cow<'a, [u8]>>, content_type: &'a str) -> Self {
        self.body = Some((data.into(), content_type));
        self
    }
    /// The path (or name) the request is about, for error messages.
    pub(crate) fn subject(mut self, s: impl std::fmt::Display) -> Self {
        self.subject = Some(s.to_string());
        self
    }
    pub(crate) fn classify(mut self, f: Classify) -> Self {
        self.classify = Some(f);
        self
    }
    /// Non-2xx statuses the caller handles itself (e.g. Drive's 308 Resume Incomplete).
    pub(crate) fn accept(mut self, statuses: &'a [u16]) -> Self {
        self.accept = statuses;
        self
    }
    /// For pre-authorised URLs (upload session URLs) that must not carry the bearer token.
    pub(crate) fn no_auth(mut self) -> Self {
        self.auth = false;
        self
    }
    /// Return 3xx responses instead of following them.
    pub(crate) fn no_redirects(mut self) -> Self {
        self.redirects = false;
        self
    }
}

/// An authorised connection to one provider account.
pub(crate) struct Session {
    agent: Agent,
    provider: Provider,
    tokens: TokenSource,
    closed: AtomicBool,
}

impl Session {
    pub(crate) fn new(agent: Agent, provider: Provider, tokens: TokenSource) -> Self {
        Session {
            agent,
            provider,
            tokens,
            closed: AtomicBool::new(false),
        }
    }

    pub(crate) fn service(&self) -> &'static str {
        self.provider.label()
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    pub(crate) fn check_open(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Error::new(
                ErrorKind::Closed,
                format!("This {} connection was closed.", self.service()),
            ));
        }
        Ok(())
    }

    /// Sends `call`; on 401 refreshes the access token and tries once more. Non-2xx statuses
    /// not listed in `accept` become an [`Error`].
    pub(crate) fn send(&self, call: Call<'_>) -> Result<Response> {
        self.check_open()?;
        let mut token = if call.auth {
            Some(self.tokens.access()?)
        } else {
            None
        };
        let mut refreshed = false;
        loop {
            let resp = self.dispatch(&call, token.as_deref())?;
            let status = resp.status().as_u16();
            if status == 401
                && !refreshed
                && let Some(t) = &token
            {
                refreshed = true;
                token = Some(self.tokens.after_unauthorized(t)?);
                continue;
            }
            if resp.status().is_success() || call.accept.contains(&status) {
                return Ok(resp);
            }
            return Err(self.error_from(resp, &call));
        }
    }

    /// [`Session::send`] and parse the JSON reply.
    pub(crate) fn json<T: DeserializeOwned>(&self, call: Call<'_>) -> Result<T> {
        let mut resp = self.send(call)?;
        self.parse(&mut resp)
    }

    pub(crate) fn parse<T: DeserializeOwned>(&self, resp: &mut Response) -> Result<T> {
        let body = read_body(resp, self.service())?;
        serde_json::from_slice(&body).map_err(|e| {
            Error::new(
                ErrorKind::Other,
                format!("{} sent a reply Filewinch couldn't read.", self.service()),
            )
            .with_detail(e)
        })
    }

    fn dispatch(&self, call: &Call<'_>, token: Option<&str>) -> Result<Response> {
        let mut url = call.url.clone();
        if !call.query.is_empty() {
            url.push(if url.contains('?') { '&' } else { '?' });
            url.push_str(&form_encode(&call.query));
        }
        let mut b = ureq::http::Request::builder().method(call.method).uri(&url);
        if let Some(t) = token {
            b = b.header("Authorization", format!("Bearer {t}"));
        }
        for (k, v) in &call.headers {
            b = b.header(*k, v);
        }
        let body: &[u8] = match &call.body {
            Some((data, ct)) => {
                b = b.header("Content-Type", *ct);
                data
            }
            None => &[],
        };
        let request = b.body(body).map_err(|e| {
            Error::new(ErrorKind::Other, "Filewinch built an invalid request.").with_detail(e)
        })?;
        let mut cfg = self.agent.configure_request(request);
        if !call.redirects {
            cfg = cfg.max_redirects(0);
        }
        self.agent
            .run(cfg.build())
            .map_err(|e| transport_error(e, self.service()))
    }

    fn error_from(&self, mut resp: Response, call: &Call<'_>) -> Error {
        let status = resp.status().as_u16();
        let wait = retry_after(resp.headers());
        let body = read_body(&mut resp, self.service()).unwrap_or_default();
        let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let kind = call
            .classify
            .and_then(|f| f(status, &v))
            .unwrap_or(match status {
                401 => ErrorKind::Auth,
                403 => ErrorKind::PermissionDenied,
                404 | 410 => ErrorKind::NotFound,
                409 => ErrorKind::AlreadyExists,
                413 | 507 => ErrorKind::NoSpace,
                429 => ErrorKind::RateLimited,
                408 | 500..=599 => ErrorKind::Network,
                _ => ErrorKind::Other,
            });
        let detail_msg = provider_message(&v)
            .unwrap_or_else(|| String::from_utf8_lossy(&body).chars().take(300).collect());
        let detail = format!(
            "HTTP {status} {} {}: {detail_msg}",
            call.method,
            strip_query(&call.url)
        );
        let mut err = Error::new(
            kind,
            message(kind, self.service(), call.subject.as_deref(), &detail_msg),
        )
        .with_detail(detail);
        let wait = wait.or_else(|| {
            // Dropbox repeats the wait in its JSON error.
            v.pointer("/error/retry_after")
                .and_then(Value::as_u64)
                .map(Duration::from_secs)
        });
        if let Some(w) =
            wait.filter(|_| matches!(kind, ErrorKind::RateLimited | ErrorKind::Network))
        {
            err = err.with_retry_after(w);
        }
        err
    }
}

fn strip_query(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

/// The provider's own error text, from whichever shape it uses.
fn provider_message(v: &Value) -> Option<String> {
    [
        "/error/message",
        "/error_summary",
        "/message",
        "/error_description",
        "/error",
    ]
    .iter()
    .find_map(|p| {
        v.pointer(p)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    })
    .map(str::to_string)
}

pub(crate) fn message(
    kind: ErrorKind,
    service: &str,
    subject: Option<&str>,
    detail: &str,
) -> String {
    let it = subject.map_or_else(|| "the item".to_string(), |s| format!("“{s}”"));
    match kind {
        ErrorKind::Auth => {
            format!("{service} no longer accepts Filewinch's sign-in. Sign in again.")
        }
        ErrorKind::NotFound => format!("{} wasn't found on {service}.", capitalise(&it)),
        ErrorKind::PermissionDenied => format!("{service} doesn't allow this for {it}."),
        ErrorKind::AlreadyExists => format!("{} already exists on {service}.", capitalise(&it)),
        ErrorKind::InvalidPath => format!("{service} can't use {it}: {detail}"),
        ErrorKind::NoSpace => format!("Your {service} storage is full."),
        ErrorKind::RateLimited => {
            format!("{service} is limiting how fast Filewinch can work. It will try again shortly.")
        }
        ErrorKind::Network => format!("{service} isn't responding properly. Filewinch will retry."),
        ErrorKind::Unsupported => format!("{service} doesn't support this for {it}."),
        _ => format!("{service} couldn't complete the request for {it}: {detail}"),
    }
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Builds an error with the standard wording (for conditions found in a 2xx reply).
pub(crate) fn error(
    kind: ErrorKind,
    service: &str,
    subject: impl std::fmt::Display,
    detail: &str,
) -> Error {
    let s = subject.to_string();
    Error::new(kind, message(kind, service, Some(&s), detail))
}
