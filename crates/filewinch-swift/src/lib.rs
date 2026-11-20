//! OpenStack Swift (and Rackspace Cloud Files) for Filewinch.
//!
//! # How a [`Site`] maps to Swift
//!
//! - **`host`** is the auth URL. The version comes from its path:
//!   - ends in `/v3` (or `/v3/auth/tokens`) → Keystone v3,
//!   - ends in `/v2.0` or `/v2` (or `…/tokens`) → Keystone v2 (Rackspace),
//!   - ends in `/v1.0` or `/v1` (e.g. TempAuth's `/auth/v1.0`) → v1 (TempAuth / swauth),
//!   - anything else → tried as `<url>/v3`, then `<url>/v2.0`, then `<url>/auth/v1.0`,
//!     moving on only when the server answers 404/405.
//!
//!   Without a scheme `https://` is assumed. `port`, if set, is used when the URL has none.
//! - **`user`** is the user name; **`project`** and **`domain`** are `Site`'s own fields:
//!   - v3: `project` scopes the token (required: an unscoped token has no catalog) and
//!     `domain` names the domain of both the user and the project (default `Default`).
//!   - v2: `project` is the tenant name (optional, Rackspace doesn't need one); `domain` is
//!     ignored.
//!   - v1: `X-Auth-User` is `project:user` (TempAuth's own `account:user` form), or the user
//!     as typed when there's no project.
//! - **`trusted_fingerprints`**: certificates accepted even when the system doesn't trust them.
//! - **Secret** (`Auth::Password`) is the password. Rackspace rule: when the auth URL's host
//!   ends in `rackspacecloud.com`, v2 is in use and the secret is exactly 32 hex digits (the
//!   form of a Rackspace API key), it is sent as `RAX-KSKEY:apiKeyCredentials`; otherwise as
//!   `passwordCredentials`.
//! - **`region`** picks the catalog's `object-store` endpoint (interface `public`),
//!   case-insensitively; without one the first public endpoint is used. An unknown region
//!   is an error that lists the regions the account has. v1 has no catalog and ignores it.
//! - **`initial_path`** is the folder to open in, e.g. `photos/2024`.
//!
//! The first path component is the container; the rest is the object name. `/` lists the
//! account's containers.
//!
//! # Sign-in and expiry
//!
//! The token is shared by every thread. It is renewed shortly before a known expiry, and a
//! 401 on any request signs in again once and repeats the request once (a second 401 is
//! `Auth`). A streamed upload can't be repeated, so a 401 there signs in again and returns
//! `Network` ("signed in again, retrying"), which the transfer engine retries with the new
//! token; if signing in fails, the upload fails with `Auth`.
//!
//! # Large files
//!
//! Files larger than [`Tuning::large_object_threshold`] (5 GiB, Swift's single-object
//! limit) and files of unknown size are uploaded as a Static Large Object: segments of at
//! most [`Tuning::segment_size`] (1 GiB; grown when needed to stay within 1000 segments),
//! each streamed with a Content-Length (nothing is buffered), go to the container
//! `<container>_segments` as `<object>/slo/<timestamp>/<size>/<segment size>/<n>` (the
//! layout python-swiftclient uses), then a manifest is written with
//! `?multipart-manifest=put`. An unknown-size upload that fits in one segment is copied
//! into place server-side and the segment removed, so small files stay plain objects.
//! Dropping the sink, or a failed `finish`, deletes the uploaded segments (best effort, on
//! a background thread). Deleting an SLO (`X-Static-Large-Object: True`) deletes its
//! segments too (`?multipart-manifest=delete`).
//!
//! Every upload is checked against the MD5 `ETag` the server returns.
//!
//! # Folders
//!
//! Swift is flat. A folder is a name prefix ending in `/`; `mkdir` writes a zero-byte
//! `name/` marker object with `Content-Type: application/directory`. Listings also treat
//! older `application/directory` objects without the slash as folders.

mod auth;
mod client;
mod config;
mod list;
mod remote;
mod time;
mod upload;

use std::sync::Arc;

use filewinch_core::{Auth, Connector, Error, ErrorKind, Protocol, Remote, Result, SecretRef, Secrets, Site};

pub use remote::SwiftRemote;

/// Opens OpenStack Swift / Rackspace Cloud Files sites. See the crate docs for how the
/// site's fields are read.
pub struct SwiftConnector;

/// Sizes the backend works with. [`Tuning::default`] is right for real servers; tests
/// lower the numbers to exercise paging and large-object uploads with small data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tuning {
    /// Entries asked for per listing request (Swift's maximum is 10 000).
    pub page_size: u32,
    /// Uploads larger than this become Static Large Objects.
    pub large_object_threshold: u64,
    /// Preferred segment size for Static Large Objects.
    pub segment_size: u64,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            page_size: 10_000,
            large_object_threshold: 5 * 1024 * 1024 * 1024,
            segment_size: 1024 * 1024 * 1024,
        }
    }
}

impl Connector for SwiftConnector {
    fn protocols(&self) -> &'static [Protocol] {
        &[Protocol::Swift]
    }

    fn connect(&self, site: &Site, secrets: Arc<dyn Secrets>) -> Result<Box<dyn Remote>> {
        let secret = |r: &SecretRef| secrets.get(r);
        let secret = &secret;
        Ok(Box::new(connect_with(site, secret, Tuning::default())?))
    }
}

/// [`SwiftConnector::connect`] with custom [`Tuning`]: signs in and checks the account
/// answers.
pub fn connect_with(
    site: &Site,
    secret: &dyn Fn(&SecretRef) -> Result<Option<String>>,
    tuning: Tuning,
) -> Result<SwiftRemote> {
    let password = match &site.auth {
        Auth::Password { secret: r } => secret(r)?,
        _ => None,
    };
    let Some(password) = password.filter(|p| !p.is_empty()) else {
        return Err(Error::new(
            ErrorKind::Auth,
            format!("Enter the password or API key for {}.", site.user),
        ));
    };
    let cfg = config::AuthConfig::from_site(site, password)?;
    SwiftRemote::connect(cfg, site.initial_path.as_deref(), tuning)
}

#[cfg(test)]
mod tests;
