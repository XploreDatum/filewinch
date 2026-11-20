//! Amazon S3 and S3-compatible endpoints.
//!
//! Paths: `/` lists buckets; `/bucket/a/b` is key `a/b`. Folders are key prefixes, plus the
//! zero-byte `name/` marker objects `mkdir` writes.

mod client;
mod endpoint;
mod errors;
mod listing;
mod multipart;
mod remote;
mod upload;

use std::sync::Arc;

use filewinch_core::{Auth, Connector, Error, ErrorKind, Protocol, Remote, Result, SecretRef, Secrets, Site};
use filewinch_http::Http;

use crate::sigv4::Credentials;
use client::Client;
use endpoint::Endpoint;

pub(crate) use client::{Limits, lock};
pub(crate) use multipart::{part_size_for, part_size_unknown};

/// Connects [`Protocol::S3`] sites.
///
/// - `host`: empty for AWS, else the endpoint host or URL (`http://` is allowed for local
///   servers such as MinIO); `port` overrides the URL's.
/// - `user` = access key ID, `Auth::Password` = secret access key.
/// - `region`: defaults to one implied by the host (R2 → `auto`), else `us-east-1`. A bucket
///   in another region is detected from the server's redirect and remembered.
/// - `path_style` forces `host/bucket/key`; otherwise virtual-host style is used where the
///   bucket name allows it.
/// - `initial_path`: `bucket/prefix` to open in.
pub struct S3Connector;

impl Connector for S3Connector {
    fn protocols(&self) -> &'static [Protocol] {
        &[Protocol::S3]
    }

    fn connect(&self, site: &Site, secrets: Arc<dyn Secrets>) -> Result<Box<dyn Remote>> {
        let secret = |r: &SecretRef| secrets.get(r);
        let secret = &secret;
        let remote = S3Remote::open(site, secret, Http::for_site(site), Limits::default())?;
        remote.verify()?;
        Ok(Box::new(remote))
    }
}

/// A connected S3 session.
pub struct S3Remote {
    client: Arc<Client>,
    /// `/bucket/prefix` from the site, or `/`.
    home: filewinch_core::RPath,
}

impl S3Remote {
    pub(crate) fn open(
        site: &Site,
        secret: &dyn Fn(&SecretRef) -> Result<Option<String>>,
        http: Http,
        limits: Limits,
    ) -> Result<S3Remote> {
        let (ep, region) = Endpoint::from_site(site)?;
        let access_key = site.user.trim().to_string();
        if access_key.is_empty() {
            return Err(Error::new(ErrorKind::Auth, format!("Enter the access key ID for {}.", ep.display())));
        }
        let secret_key = match &site.auth {
            Auth::Password { secret: r } => secret(r)?.filter(|s| !s.is_empty()),
            _ => None,
        }
        .ok_or_else(|| Error::new(ErrorKind::Auth, format!("Enter the secret access key for {}.", ep.display())))?;
        let creds = Credentials { access_key, secret_key };
        let mut client = Client::new(http, ep, creds, region);
        client.limits = limits;
        let home = filewinch_core::RPath::new(site.initial_path.as_deref().unwrap_or("/"));
        Ok(S3Remote { client: Arc::new(client), home })
    }

    fn initial_bucket(&self) -> Option<&str> {
        self.home.components().next()
    }

    /// Checks the credentials: lists the initial bucket if there is one (works for keys
    /// restricted to that bucket), else lists the buckets.
    pub(crate) fn verify(&self) -> Result<()> {
        match self.initial_bucket() {
            Some(bucket) => {
                listing::list_page(&self.client, bucket, "", true, None, Some(1), &format!("open the bucket “{bucket}”"))
                    .map(drop)
            }
            None => listing::list_buckets(&self.client).map(drop),
        }
    }
}

/// Splits `/bucket/a/b` into (Some("bucket"), "a/b"); the root is (None, "").
pub(crate) fn split(path: &filewinch_core::RPath) -> (Option<&str>, &str) {
    let s = path.as_str().trim_start_matches('/');
    match s.split_once('/') {
        Some((b, k)) => (Some(b), k),
        None if s.is_empty() => (None, ""),
        None => (Some(s), ""),
    }
}

#[cfg(test)]
mod tests;
