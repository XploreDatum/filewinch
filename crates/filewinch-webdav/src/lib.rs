//! WebDAV (`Protocol::WebDav`) and WebDAV over HTTPS (`Protocol::WebDavs`) for Filewinch.
//!
//! - **Address.** `Site::host` is a host name, `host:port`, or a full URL with a base path
//!   (`https://cloud.example.com/remote.php/dav/files/alice/`); the site's `/` is that base.
//!   The scheme comes from the protocol. `initial_path` is where the session opens.
//! - **Sign-in.** Requests go out unsigned until the server answers 401; then Digest
//!   (SHA-256 preferred, MD5, `-sess`, `qop=auth`) or Basic is chosen from its challenge and
//!   reused on later requests, refreshing a stale Digest nonce. `connect` makes a Depth-0
//!   PROPFIND on the base, so a wrong password fails there as `ErrorKind::Auth`.
//! - **TLS.** An untrusted certificate is `ErrorKind::HostTrust` with detail
//!   `"sha256:AB:CD:…; reason"`; after `filewinch_http::trust::trust_fingerprint(host, port,
//!   fp)` the next `connect` succeeds.
//! - **Transfers.** Downloads resume with `Range` (a server that ignores it still works: the
//!   skipped bytes are read and dropped). Uploads stream with `PUT` (`Content-Length` when the
//!   size is known, chunked otherwise); dropping an unfinished sink aborts the request.
//!   Upload resume isn't possible in plain WebDAV.
//! - **Safety.** WebDAV `DELETE` is recursive, so `remove_dir` refuses a folder that isn't
//!   empty and `remove_file` refuses a folder. `rename`/`copy` never overwrite.

mod auth;
mod client;
mod multistatus;
mod remote;
mod target;
mod transfer;

use std::sync::Arc;

use filewinch_core::{Auth, Connector, Error, ErrorKind, Protocol, RPath, Remote, Result, SecretRef, Secrets, Site};

use client::{Client, Creds};
use remote::WebDavRemote;
use target::Target;

/// Connects `Protocol::WebDav` and `Protocol::WebDavs` sites.
pub struct WebDavConnector;

impl Connector for WebDavConnector {
    fn protocols(&self) -> &'static [Protocol] {
        &[Protocol::WebDav, Protocol::WebDavs]
    }

    fn connect(&self, site: &Site, secrets: Arc<dyn Secrets>) -> Result<Box<dyn Remote>> {
        let secret = |r: &SecretRef| secrets.get(r);
        let secret = &secret;
        if !self.protocols().contains(&site.protocol) {
            return Err(Error::unsupported(site.protocol.label()));
        }
        let target = Target::from_site(site)?;
        let host = target.host_name().to_string();
        let user = site.user.trim().to_string();
        let creds = match &site.auth {
            Auth::None if user.is_empty() => None,
            Auth::None => Some(Creds { user, pass: String::new() }),
            Auth::Password { secret: r } => {
                let who = if user.is_empty() { host.clone() } else { format!("{user}@{host}") };
                let pass = secret(r)?.ok_or_else(|| {
                    Error::new(ErrorKind::Auth, format!("There's no saved password for {who}."))
                })?;
                Some(Creds { user, pass })
            }
            _ => {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    "WebDAV sites sign in with a user name and password.",
                ));
            }
        };
        let client = Arc::new(Client::new(target, creds, &site.trusted_fingerprints));
        let what = format!("connect to {host}");
        let root = client.propfind(&RPath::root(), 0, true, &what).map_err(|e| match e.kind {
            ErrorKind::NotFound => Error::new(
                ErrorKind::NotFound,
                format!("The folder {} doesn't exist on {host}.", base_display(&client.target)),
            )
            .with_detail(e.detail.unwrap_or_default()),
            _ => e,
        })?;
        if root.first().is_some_and(|r| !r.is_dir) {
            return Err(Error::new(
                ErrorKind::InvalidPath,
                format!("Couldn't {what}: {} is a file, not a folder.", base_display(&client.target)),
            ));
        }
        let home = RPath::new(site.initial_path.as_deref().unwrap_or("/"));
        Ok(Box::new(WebDavRemote { client, home }))
    }
}

fn base_display(t: &Target) -> String {
    format!("/{}", t.base.join("/"))
}
