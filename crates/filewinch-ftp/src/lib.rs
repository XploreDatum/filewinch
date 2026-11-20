//! Filewinch's FTP and FTPS backend, built on `suppaftp` (blocking API) with rustls (`ring`).
//!
//! - Plain FTP, explicit TLS (`AUTH TLS`, the usual FTPS) and implicit TLS (port 990).
//! - Passive mode by default (with the NAT workaround: a private address in a PASV reply is
//!   replaced by the control connection's address); active mode when the site turns passive
//!   off.
//! - Listings use MLSD when the server offers it, else LIST, parsed in Unix (`ls -l`) and
//!   Windows/IIS formats ([`list`]).
//! - UTF-8 file names (`OPTS UTF8 ON`), binary transfers.
//! - Resume: downloads with `REST`; uploads with `REST`+`STOR`, falling back to `APPE` onto a
//!   partial file of the expected size.
//! - `MFMT` for modification times (when offered), `SITE CHMOD` for permissions on Unix servers.
//! - A pool of up to [`MAX_CONNECTIONS`] control connections, since FTP runs one transfer per
//!   control connection. Idle connections get a `NOOP` before reuse and are replaced when dead;
//!   commands retry once on a fresh connection when the old one dropped.
//! - TLS certificates are checked against the Mac's trust roots; a self-signed or otherwise
//!   untrusted certificate fails with `ErrorKind::HostTrust` and its SHA-256 fingerprint
//!   (`SHA256:base64`) in `detail`. [`trust_certificate`] pins it for that host and port.

mod conn;
pub mod list;
mod remote;
mod tls;

pub use tls::{set_trusted_certs_path, trust_certificate, trusted_certs_path};

use std::sync::Arc;

use filewinch_core::{Connector, Protocol, Remote, Result, SecretRef, Secrets, Site};

/// Control connections one FTP site opens at most.
pub const MAX_CONNECTIONS: usize = 4;

/// Opens sessions for [`Protocol::Ftp`] and [`Protocol::Ftps`] sites.
pub struct FtpConnector;

impl Connector for FtpConnector {
    fn protocols(&self) -> &'static [Protocol] {
        &[Protocol::Ftp, Protocol::Ftps]
    }

    fn connect(&self, site: &Site, secrets: Arc<dyn Secrets>) -> Result<Box<dyn Remote>> {
        let secret = |r: &SecretRef| secrets.get(r);
        let secret = &secret;
        let params = conn::Params::resolve(site, secret)?;
        Ok(Box::new(remote::FtpRemote::connect(params)?))
    }
}
