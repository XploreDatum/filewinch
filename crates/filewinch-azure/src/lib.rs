//! Microsoft Azure Blob Storage for Filewinch ([`Protocol::AzureBlob`]), over the Blob
//! service REST API (version 2021-08-06) with Shared Key or SAS authorization.
//!
//! # How a site is read
//!
//! **Address (`Site::host`)**, one of:
//! - an account name, `myaccount` → `https://myaccount.blob.core.windows.net`;
//! - a host name, `myaccount.blob.core.windows.net` (or a custom domain) → HTTPS;
//! - a URL, `https://myaccount.blob.core.windows.net`, optionally with `/container[/folder]`;
//! - a path-style URL, `http://127.0.0.1:10000/devstoreaccount1` (Azurite). An endpoint is
//!   path-style (first path segment = account) when its host is an IP address or
//!   `localhost`, or when `Site::path_style` is set;
//! - a SAS URL, `https://myaccount.blob.core.windows.net/container?sv=…&sig=…`.
//!
//! `Site::port` overrides the URL's port.
//!
//! **Account.** Path-style: the first path segment, else `Site::user`. Otherwise
//! `Site::user`, else the first label of the host name.
//!
//! **Credential**, in this order:
//! 1. a query string on the address is a SAS token (the Keychain secret is not used);
//! 2. `Auth::Password`: the secret is a SAS token when it starts with `?` or `sig=`, or
//!    contains `&` (an account key is plain base64 and never does), with or without the
//!    leading `?`; otherwise it is the base64 account key (Shared Key);
//! 3. `Auth::None`: anonymous access to a public container (named in the initial folder).
//!
//! A SAS token must include `sig=`. A container SAS (`sr=c`) is scoped to its container:
//! the one named in the URL path, else the first component of `Site::initial_path`; the
//! top level then shows only that container. An account SAS (no `sr`) sees every container.
//! A single-blob SAS (`sr=b`) can't browse and is refused.
//!
//! **Initial folder** (`Site::initial_path`) is `container/prefix`; if empty, the URL path
//! (after the account, for path-style) is used.
//!
//! # Paths
//!
//! `/` lists containers, `/container/a/b` is blob `a/b`. Folders are `/`-delimited
//! prefixes plus zero-byte `name/` marker blobs (`mkdir` writes one; `Caps::real_dirs` is
//! false). `mkdir /name` creates a container.
//!
//! # Transfers
//!
//! Downloads resume with `x-ms-range` (a 206 is required). Uploads of known size under
//! 4 MiB are one streamed Put Blob whose last byte is held back until `finish`; anything
//! else is staged in blocks (8 MiB, larger for huge files so they fit in 50 000 blocks;
//! at most one block is buffered, and each block is retried on transient errors) and
//! committed with Put Block List. Dropping a sink without `finish` commits nothing: a
//! single put is aborted mid-body, and staged blocks stay uncommitted until Azure discards
//! them (after about a week). Uploads can't resume.
//!
//! Server-side copy uses Copy Blob and waits for pending copies (aborting them if the
//! session closes or they stall for 10 minutes). Rename is unsupported (the engine copies,
//! then deletes).
//!
//! # Secrets
//!
//! Account keys, request signatures and SAS query strings never appear in errors or logs:
//! messages name the account or host, never a URL, and details are scrubbed.

mod client;
mod config;
mod errors;
mod ops;
mod remote;
mod sign;
mod upload;
mod xml;

use std::sync::Arc;

use filewinch_core::{Connector, Protocol, Remote, Result, SecretRef, Secrets, Site};

use client::{Call, Client};
use remote::AzureRemote;

/// Most entries per listing request (the service maximum).
const PAGE_SIZE: u32 = 5000;

/// Opens Azure Blob Storage sites. See the crate docs for how the site is read.
pub struct AzureConnector;

impl Connector for AzureConnector {
    fn protocols(&self) -> &'static [Protocol] {
        &[Protocol::AzureBlob]
    }

    fn connect(&self, site: &Site, secrets: Arc<dyn Secrets>) -> Result<Box<dyn Remote>> {
        let secret = |r: &SecretRef| secrets.get(r);
        let secret = &secret;
        connect_with(site, secret, PAGE_SIZE)
    }
}

fn connect_with(
    site: &Site,
    secret: &dyn Fn(&SecretRef) -> Result<Option<String>>,
    page_size: u32,
) -> Result<Box<dyn Remote>> {
    let cfg = config::parse(site, secret)?;
    let c = Client::new(cfg, page_size.clamp(1, PAGE_SIZE), &site.trusted_fingerprints);
    // Verify the credential with the cheapest request it is allowed to make.
    match &c.cfg.scoped {
        Some(container) => {
            let call = Call::new("GET", Some(container), None)
                .q("restype", "container")
                .q("comp", "list")
                .q("maxresults", "1");
            c.send(&call, &[], &format!("open the container “{container}”"))?;
        }
        None => {
            let call = Call::new("GET", None, None).q("comp", "list").q("maxresults", "1");
            c.send(&call, &[], &format!("list the containers of {}", c.cfg.account))?;
        }
    }
    Ok(Box::new(AzureRemote { c: Arc::new(c) }))
}

/// Test hooks. Not part of the API.
#[doc(hidden)]
pub mod testing {
    use super::*;

    /// `connect` with a small listing page size, to exercise `marker` paging.
    pub fn connect_with_page_size(
        site: &Site,
        secret: &dyn Fn(&SecretRef) -> Result<Option<String>>,
        page_size: u32,
    ) -> Result<Box<dyn Remote>> {
        connect_with(site, secret, page_size)
    }
}

#[cfg(test)]
mod tests;
