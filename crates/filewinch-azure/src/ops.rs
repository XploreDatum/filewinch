//! mkdir, delete and server-side copy.

use std::time::{Duration, Instant};

use filewinch_core::{Error, ErrorKind, RPath, Result};

use crate::client::{Call, header};
use crate::remote::{AzureRemote, Loc, quoted};

/// How long a pending copy may go without progress before it's aborted.
const COPY_STALL: Duration = Duration::from_secs(10 * 60);

/// Azure's container naming rules: 3–63 characters of lowercase letters, digits and
/// hyphens, starting and ending with a letter or digit, no double hyphens.
pub(crate) fn valid_container_name(name: &str) -> bool {
    let b = name.as_bytes();
    (3..=63).contains(&b.len())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
        && !name.contains("--")
}

pub(crate) fn mkdir(r: &AzureRemote, path: &RPath) -> Result<()> {
    let what = format!("create the folder {}", quoted(path));
    match r.loc(path)? {
        Loc::Root => Err(Error::new(ErrorKind::AlreadyExists, "The top level always exists.")),
        Loc::Container(c) => {
            if r.c.cfg.scoped.is_some() {
                return Err(Error::new(ErrorKind::AlreadyExists, format!("Couldn't {what}: it already exists.")));
            }
            if !valid_container_name(&c) {
                return Err(Error::new(
                    ErrorKind::InvalidPath,
                    format!(
                        "Couldn't {what}: container names must be 3–63 lowercase letters, numbers and single hyphens, starting and ending with a letter or number."
                    ),
                ));
            }
            let what = format!("create the container {}", quoted(path));
            r.c.send(&Call::new("PUT", Some(&c), None).q("restype", "container"), &[], &what)?;
            Ok(())
        }
        Loc::Blob(c, k) => {
            if r.head_blob(&c, &k, &what)?.is_some() {
                return Err(Error::new(
                    ErrorKind::AlreadyExists,
                    format!("Couldn't {what}: a file with that name already exists."),
                ));
            }
            let marker = format!("{k}/");
            let call = Call::new("PUT", Some(&c), Some(&marker))
                .h("x-ms-blob-type", "BlockBlob")
                .h("if-none-match", "*");
            r.c.send(&call, &[], &what)?;
            Ok(())
        }
    }
}

pub(crate) fn remove_file(r: &AzureRemote, path: &RPath) -> Result<()> {
    let what = format!("delete {}", quoted(path));
    let Loc::Blob(c, k) = r.loc(path)? else {
        return Err(Error::new(ErrorKind::InvalidPath, format!("Couldn't {what}: it's a container, not a file.")));
    };
    let call = Call::new("DELETE", Some(&c), Some(&k)).h("x-ms-delete-snapshots", "include");
    r.c.send(&call, &[], &what)?;
    Ok(())
}

pub(crate) fn remove_dir(r: &AzureRemote, path: &RPath) -> Result<()> {
    let what = format!("delete the folder {}", quoted(path));
    let not_empty = || Error::new(ErrorKind::InvalidPath, format!("Couldn't {what}: it isn't empty."));
    match r.loc(path)? {
        Loc::Root => Err(Error::new(ErrorKind::InvalidPath, "The top level can't be deleted.")),
        Loc::Container(c) => {
            if r.has_blobs(&c, "", None, &what)? {
                return Err(not_empty());
            }
            let what = format!("delete the container {}", quoted(path));
            r.c.send(&Call::new("DELETE", Some(&c), None).q("restype", "container"), &[], &what)?;
            Ok(())
        }
        Loc::Blob(c, k) => {
            let marker = format!("{k}/");
            if r.has_blobs(&c, &marker, Some(&marker), &what)? {
                return Err(not_empty());
            }
            let call = Call::new("DELETE", Some(&c), Some(&marker)).h("x-ms-delete-snapshots", "include");
            let resp = r.c.send_raw(&call, &[])?;
            // No marker: an implied folder, gone once its last item went. That's success.
            if resp.status().as_u16() == 404 && header(&resp, "x-ms-error-code").as_deref() == Some("BlobNotFound") {
                return Ok(());
            }
            r.c.ok(resp, &what)?;
            Ok(())
        }
    }
}

pub(crate) fn copy(r: &AzureRemote, from: &RPath, to: &RPath) -> Result<()> {
    let what = format!("copy {} to {}", quoted(from), quoted(to));
    let (Loc::Blob(sc, sk), Loc::Blob(dc, dk)) = (r.loc(from)?, r.loc(to)?) else {
        return Err(Error::new(
            ErrorKind::InvalidPath,
            format!("Couldn't {what}: only files inside a container can be copied on the server."),
        ));
    };
    if from == to {
        return Ok(());
    }
    let call = Call::new("PUT", Some(&dc), Some(&dk)).h("x-ms-copy-source", r.c.source_url(&sc, &sk));
    let resp = r.c.send(&call, &[], &what)?;
    let id = header(&resp, "x-ms-copy-id").unwrap_or_default();
    match header(&resp, "x-ms-copy-status").as_deref() {
        Some("success") | None => Ok(()),
        _ => wait_for_copy(r, &dc, &dk, &id, &what),
    }
}

/// Polls Get Blob Properties until the copy ends. Aborts it if the session closes or the
/// copy makes no progress for [`COPY_STALL`].
fn wait_for_copy(r: &AzureRemote, container: &str, key: &str, id: &str, what: &str) -> Result<()> {
    let mut delay = Duration::from_millis(250);
    let mut progress = String::new();
    let mut last_change = Instant::now();
    loop {
        if let Err(e) = r.c.sleep(delay) {
            abort_copy(r, container, key, id);
            return Err(e);
        }
        delay = (delay * 2).min(Duration::from_secs(5));
        let resp = r.c.send(&Call::new("HEAD", Some(container), Some(key)), &[], what)?;
        if !id.is_empty() && header(&resp, "x-ms-copy-id").is_some_and(|now| now != id) {
            return Err(Error::new(
                ErrorKind::Other,
                format!("Couldn't {what}: another copy to the same name replaced this one."),
            ));
        }
        let description = header(&resp, "x-ms-copy-status-description").unwrap_or_default();
        match header(&resp, "x-ms-copy-status").as_deref() {
            Some("success") | None => return Ok(()),
            Some("pending") => {
                let now = header(&resp, "x-ms-copy-progress").unwrap_or_default();
                if now != progress {
                    progress = now;
                    last_change = Instant::now();
                } else if last_change.elapsed() > COPY_STALL {
                    abort_copy(r, container, key, id);
                    return Err(Error::new(
                        ErrorKind::Network,
                        format!("Couldn't {what}: Azure made no progress for 10 minutes, so the copy was stopped."),
                    ));
                }
            }
            Some("aborted") => {
                return Err(Error::new(ErrorKind::Other, format!("Couldn't {what}: the copy was stopped on the server."))
                    .with_detail(description));
            }
            Some(other) => {
                let kind = if description.contains("404") { ErrorKind::NotFound } else { ErrorKind::Other };
                return Err(Error::new(kind, format!("Couldn't {what}: Azure reported that the copy failed."))
                    .with_detail(format!("x-ms-copy-status: {other}; {description}")));
            }
        }
    }
}

/// Best effort: a failure here leaves Azure to finish or fail the copy on its own.
fn abort_copy(r: &AzureRemote, container: &str, key: &str, id: &str) {
    if id.is_empty() {
        return;
    }
    let call = Call::new("PUT", Some(container), Some(key))
        .q("comp", "copy")
        .q("copyid", id)
        .h("x-ms-copy-action", "abort");
    // Sent even when the session was just closed (that's when it matters most).
    let _ = r.c.send_raw_unchecked(&call, &[]);
}
