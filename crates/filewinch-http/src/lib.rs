//! Shared HTTP plumbing for Filewinch's HTTP backends (WebDAV, S3/B2, Azure, Swift, cloud
//! drives): one configured blocking [`ureq`] agent, TLS with the macOS trust store plus
//! user-pinned certificates, mapping of HTTP/transport failures to core errors, and a
//! streaming upload pipe.
//!
//! # The agent
//!
//! [`Http::new`] builds a [`ureq::Agent`] that never turns statuses into errors (check
//! `resp.status()` yourself and call [`status_error`] for failures), never follows
//! redirects (3xx comes back as a response), allows WebDAV methods, sends
//! `User-Agent: Filewinch/<version>`, and doesn't advertise or decode gzip, so byte ranges and
//! sizes are always the stored bytes. Timeouts: connect (and DNS) 15 s, response headers
//! 60 s, and an idle timeout of 60 s on every socket read and write; there is no total
//! deadline, so a 50 GB transfer never times out while bytes keep flowing.
//!
//! # Request bodies: Content-Length vs chunked
//!
//! ureq 3.5.2 picks the body framing like this: if the request has a `Content-Length` (or
//! `Transfer-Encoding`) header, that is used as is; otherwise a body of known size (`&[u8]`,
//! `String`, `File`) gets `Content-Length` and a reader (`SendBody::from_reader`) is sent
//! chunked. So a streamed body with a fixed length needs `.header("content-length", n)` set
//! by hand, which [`send_reader`] does (and guards against short readers):
//!
//! ```no_run
//! # fn demo(http: &filewinch_http::Http, file: std::fs::File, len: u64) -> Result<(), ureq::Error> {
//! // S3 / Azure: fixed length, never chunked.
//! let resp = filewinch_http::send_reader(http.agent().put("https://host/key"), file, Some(len))?;
//! // WebDAV with unknown length: chunked.
//! # let reader = std::io::empty();
//! let resp = filewinch_http::send_reader(http.agent().put("https://host/f"), reader, None)?;
//! # Ok(()) }
//! ```
//!
//! Small bodies (XML, JSON) can go straight to `.send(string)`, which sets Content-Length.
//!
//! # Streaming uploads
//!
//! [`upload_pipe`] runs the request on a worker thread and gives the caller a `Write`; see
//! its docs. The worker typically calls `send_reader(req, reader, Some(len))`, then
//! [`status_error`] on a non-success status.
//!
//! # Custom methods (PROPFIND, MKCOL, MOVE, COPY, LOCK...)
//!
//! Build an `ureq::http::Request` and pass it to `Agent::run` (the agent allows
//! non-standard methods):
//!
//! ```no_run
//! # fn demo(http: &filewinch_http::Http) -> Result<(), ureq::Error> {
//! let req = ureq::http::Request::builder()
//!     .method("PROPFIND")
//!     .uri("https://host/dav/")
//!     .header("Depth", "1")
//!     .header("Content-Type", "application/xml")
//!     .body("<?xml version=\"1.0\"?><propfind xmlns=\"DAV:\"><allprop/></propfind>")?;
//! let resp = http.agent().run(req)?;
//! // No body: `.body(())`.
//! # Ok(()) }
//! ```

mod errors;
mod pipe;
mod send;
mod status;
mod tls;
pub mod trust;

use std::sync::Arc;
use std::time::Duration;

pub use ureq;

pub use errors::{from_io, to_io};
pub use pipe::{CHANNEL_CHUNKS, CHUNK_SIZE, PipeReader, UploadPipe, upload_pipe};
pub use send::{body_reader, send_reader};
pub use status::{MAX_DETAIL, retry_after, status_error, status_error_parts};

use tls::{Rejections, TlsConnector, Trusted};
use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::{Connector, TcpConnector};

/// Default connect (and DNS) timeout.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Default idle timeout for each socket read or write.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Longest wait for response headers once the request is sent.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);
/// The `User-Agent` sent with every request.
pub const USER_AGENT: &str = concat!("Filewinch/", env!("CARGO_PKG_VERSION"));

/// A configured ureq agent plus the TLS trust state needed to explain certificate failures.
/// Cheap to clone (all clones share the connection pool); `Send + Sync`.
#[derive(Clone)]
pub struct Http {
    agent: ureq::Agent,
    rejections: Arc<Rejections>,
    trusted: Trusted,
}

impl Default for Http {
    fn default() -> Self {
        Http::new()
    }
}

impl Http {
    /// Agent with the default timeouts (connect 15 s, idle 60 s). See the crate docs.
    pub fn new() -> Http {
        Http::with_timeouts(CONNECT_TIMEOUT, IDLE_TIMEOUT)
    }

    /// Agent with a custom connect timeout and per-read/per-write idle timeout.
    pub fn with_timeouts(connect: Duration, idle: Duration) -> Http {
        let rejections = Arc::new(Rejections::default());
        let trusted = Trusted::default();
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .allow_non_standard_methods(true)
            .user_agent(USER_AGENT)
            // No env-var proxies: the custom connector chain has no proxy support.
            .proxy(None)
            .timeout_resolve(Some(connect))
            .timeout_connect(Some(connect))
            .timeout_recv_response(Some(RESPONSE_TIMEOUT))
            .timeout_per_read(Some(idle))
            .timeout_per_write(Some(idle))
            .build();
        let connector =
            ().chain(TcpConnector::default())
                .chain(TlsConnector::new(rejections.clone(), trusted.clone()));
        let agent = ureq::Agent::with_parts(config, connector, DefaultResolver::default());
        Http { agent, rejections, trusted }
    }

    /// Accepts certificates with these fingerprints (a site's `trusted_fingerprints`, any form
    /// core normalises) when the platform verifier refuses them. Replaces the previous list;
    /// shared by every clone of this `Http`. Call before the first request.
    pub fn trust_fingerprints(&self, fps: &[String]) {
        *self.trusted.write().unwrap_or_else(|p| p.into_inner()) = fps.to_vec();
    }

    /// [`Http::new`] trusting `site.trusted_fingerprints`.
    pub fn for_site(site: &filewinch_core::Site) -> Http {
        let http = Http::new();
        http.trust_fingerprints(&site.trusted_fingerprints);
        http
    }

    /// The configured agent. Build requests with it (`get`, `put`, `run`...).
    pub fn agent(&self) -> &ureq::Agent {
        &self.agent
    }

    /// Maps a transport-level ureq error (no HTTP status) to a core Error. `host` is the
    /// server's host name, used in the message. An untrusted certificate becomes
    /// `HostTrust` with detail `"SHA256:<base64>; <verifier reason>"`; timeouts, DNS,
    /// refused, reset and EOF become `Network`; everything else `Other`. Never includes URLs
    /// or credentials.
    pub fn transport_error(&self, e: ureq::Error, host: &str) -> filewinch_core::Error {
        let rejection = match &e {
            ureq::Error::Rustls(_) | ureq::Error::Tls(_) | ureq::Error::Io(_) => {
                self.rejections.take(host)
            }
            _ => None,
        };
        errors::map_transport(e, host, rejection)
    }
}

#[cfg(test)]
mod tests;
