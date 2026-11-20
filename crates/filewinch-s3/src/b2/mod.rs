//! Backblaze B2's native API (v2).
//!
//! Paths: `/` lists buckets; `/bucket/a/b` is file name `a/b`. Folders are name prefixes
//! (B2 reports them as `action: "folder"` when listing with a delimiter); `mkdir` uploads
//! the zero-byte `name/.bzEmpty` marker B2's web UI uses, which listings hide.

mod api;
#[cfg(test)]
mod fake;
mod remote;
mod upload;

use std::sync::Arc;

use filewinch_core::{Auth, Connector, Error, ErrorKind, Protocol, RPath, Remote, Result, SecretRef, Secrets, Site};
use filewinch_http::Http;

use api::{DEFAULT_API, Session};

pub(crate) use api::Limits;

/// The folder marker B2 uses.
pub(crate) const MARKER: &str = ".bzEmpty";

/// Connects [`Protocol::B2`] sites.
///
/// - `user` = application key ID, `Auth::Password` = application key.
/// - `host`: empty for Backblaze (`api.backblazeb2.com`), else another authorization base URL.
/// - `initial_path`: `bucket/prefix` to open in. Keys restricted to one bucket open there.
pub struct B2Connector;

impl Connector for B2Connector {
    fn protocols(&self) -> &'static [Protocol] {
        &[Protocol::B2]
    }

    fn connect(&self, site: &Site, secrets: Arc<dyn Secrets>) -> Result<Box<dyn Remote>> {
        let secret = |r: &SecretRef| secrets.get(r);
        let secret = &secret;
        Ok(Box::new(B2Remote::open(site, secret, Http::for_site(site), Limits::default())?))
    }
}

/// A connected B2 session. Connecting authorizes the account, which checks the key.
pub struct B2Remote {
    session: Arc<Session>,
    home: RPath,
}

impl B2Remote {
    pub(crate) fn open(
        site: &Site,
        secret: &dyn Fn(&SecretRef) -> Result<Option<String>>,
        http: Http,
        limits: Limits,
    ) -> Result<B2Remote> {
        let key_id = site.user.trim().to_string();
        if key_id.is_empty() {
            return Err(Error::new(ErrorKind::Auth, "Enter the application key ID for Backblaze B2."));
        }
        let app_key = match &site.auth {
            Auth::Password { secret: r } => secret(r)?.filter(|s| !s.is_empty()),
            _ => None,
        }
        .ok_or_else(|| Error::new(ErrorKind::Auth, "Enter the application key for Backblaze B2."))?;
        let host = site.host.trim().trim_end_matches('/');
        let base = if host.is_empty() {
            DEFAULT_API.to_string()
        } else if host.contains("://") {
            host.to_string()
        } else {
            format!("https://{host}")
        };
        let session = Session::open(http, base, key_id, app_key, limits)?;
        let home = match (&site.initial_path, &session.account().bucket) {
            (Some(p), _) if !p.trim().is_empty() => RPath::new(p),
            (_, Some((_, name))) => RPath::root().join(name),
            _ => RPath::root(),
        };
        Ok(B2Remote { session: Arc::new(session), home })
    }
}

#[cfg(test)]
mod tests;
