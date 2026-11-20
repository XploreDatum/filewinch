//! Dropbox (API v2). Dropbox is path-based, so Filewinch paths go straight through: `/` is the
//! root of the account's Dropbox. Deletes go to Dropbox's deleted files (restorable from the
//! website for the plan's retention period). Team spaces aren't shown separately: a member of
//! a team space sees the team folders the same way the Dropbox website shows them at the root.
//!
//! A shared folder (one with `sharing_info.shared_folder_id`) is reported with
//! [`Entry::shared`]; whether the account owns it comes from `sharing/get_folder_metadata`.
//! A member who isn't the owner can leave it (`sharing/relinquish_folder_membership`), which
//! takes it out of this Dropbox without touching anyone else's copy.

mod upload;

use std::io::Read;
use std::sync::Arc;

use filewinch_core::{
    Caps, Checksum, Connector, Entry, EntryKind, Error, ErrorKind, HashAlgo, Protocol, RPath,
    Remote, Result, Secrets, Shared, Site, WriteSink,
};
use serde_json::{Value, json};

use crate::http::{Call, Session, error};
use crate::oauth::Provider;
use crate::setup::{ConnectorOptions, Endpoints, Tuning};
use crate::upload::ChunkedSink;

const P: Provider = Provider::Dropbox;

/// How long a `relinquish_folder_membership` job may run before Filewinch stops waiting.
const LEAVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

#[derive(Clone, Debug, Default)]
pub struct DropboxConnector {
    opts: ConnectorOptions,
}

impl DropboxConnector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_options(opts: ConnectorOptions) -> Self {
        DropboxConnector { opts }
    }
}

impl Connector for DropboxConnector {
    fn protocols(&self) -> &'static [Protocol] {
        &[Protocol::Dropbox]
    }

    fn connect(&self, site: &Site, secrets: Arc<dyn Secrets>) -> Result<Box<dyn Remote>> {
        let s = self.opts.open_session(P, site, secrets)?;
        Ok(Box::new(Dropbox {
            s,
            ep: self.opts.endpoints(P),
            tuning: self.opts.tuning(P),
        }))
    }
}

pub(crate) struct Dropbox {
    s: Arc<Session>,
    ep: Endpoints,
    tuning: Tuning,
}

/// Dropbox reports endpoint errors as HTTP 409 with a tag path in `error_summary`
/// (`path/not_found/..`, `path/conflict/folder/..`, …).
pub(crate) fn classify(status: u16, v: &Value) -> Option<ErrorKind> {
    let summary = v["error_summary"].as_str().unwrap_or("");
    let has = |s: &str| summary.contains(s);
    if status == 429 || has("too_many_write_operations") || has("too_many_requests") {
        return Some(ErrorKind::RateLimited);
    }
    if status != 409 {
        return None;
    }
    Some(if has("not_found") {
        ErrorKind::NotFound
    } else if has("conflict") {
        ErrorKind::AlreadyExists
    } else if has("insufficient_space") || has("insufficient_quota") {
        ErrorKind::NoSpace
    } else if has("no_write_permission") || has("restricted_content") || has("no_permission") {
        ErrorKind::PermissionDenied
    } else if has("not_folder")
        || has("not_file")
        || has("malformed_path")
        || has("disallowed_name")
        || has("cant_move_folder_into_itself")
        || has("cant_copy_shared_folder")
        || has("cant_nest_shared_folder")
        || has("too_many_files")
    {
        ErrorKind::InvalidPath
    } else {
        ErrorKind::Other
    })
}

/// Dropbox's path for an `RPath`: the root is the empty string.
pub(crate) fn dbx_path(p: &RPath) -> String {
    if p.is_root() {
        String::new()
    } else {
        p.as_str().to_string()
    }
}

/// JSON for the `Dropbox-API-Arg` header: HTTP headers must be ASCII, so everything else is
/// escaped as `\uXXXX` (surrogate pairs above the BMP), as Dropbox documents.
pub(crate) fn header_arg(v: &Value) -> String {
    let mut out = String::new();
    for c in v.to_string().chars() {
        if c.is_ascii() && c != '\u{7f}' {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for u in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

/// `content_hash` of a file whose download returns its stored bytes. Cloud docs and Paper
/// files (`is_downloadable: false`, or `export_info`) download as an export, so their hash
/// isn't of what a read returns; anything but a 64-digit hex digest is ignored too.
fn checksums(kind: EntryKind, m: &Value) -> Vec<Checksum> {
    if kind != EntryKind::File
        || m["is_downloadable"].as_bool() == Some(false)
        || !m["export_info"].is_null()
    {
        return Vec::new();
    }
    m["content_hash"]
        .as_str()
        .and_then(|h| Checksum::new(HashAlgo::DropboxContent, h))
        .filter(|c| c.hex.len() == 64)
        .into_iter()
        .collect()
}

fn entry(path: RPath, m: &Value) -> Entry {
    let kind = match m[".tag"].as_str() {
        Some("folder") => EntryKind::Dir,
        _ => EntryKind::File,
    };
    let mut extra = Vec::new();
    for (label, key) in [
        ("Dropbox ID", "id"),
        ("Revision", "rev"),
        ("Content hash", "content_hash"),
    ] {
        if let Some(v) = m[key].as_str() {
            extra.push((label.to_string(), v.to_string()));
        }
    }
    Entry {
        name: m["name"]
            .as_str()
            .map_or_else(|| path.name().to_string(), str::to_string),
        kind,
        size: if kind == EntryKind::File {
            m["size"].as_u64()
        } else {
            None
        },
        modified: m["client_modified"]
            .as_str()
            .or(m["server_modified"].as_str())
            .and_then(crate::time::parse_rfc3339),
        mode: None,
        owner: None,
        group: None,
        link_target: None,
        extra,
        checksums: checksums(kind, m),
        // Filled in by `Dropbox::mark_shared`, which knows whether the account owns it.
        shared: None,
        path,
    }
}

/// The ID of the shared folder this folder is the root of (`sharing_info.shared_folder_id`, or
/// the deprecated top-level `shared_folder_id`). Folders merely inside a shared folder only
/// carry `parent_shared_folder_id` and aren't share roots.
fn shared_folder_id(m: &Value) -> Option<&str> {
    if m[".tag"] != "folder" {
        return None;
    }
    m.pointer("/sharing_info/shared_folder_id")
        .or_else(|| m.get("shared_folder_id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

impl Dropbox {
    fn rpc(&self, endpoint: &str, args: Value, subject: &RPath) -> Result<Value> {
        self.s.json(
            Call::post(format!("{}/{endpoint}", self.ep.api))
                .json(&args)
                .subject(subject)
                .classify(classify),
        )
    }

    fn metadata(&self, path: &RPath) -> Result<Value> {
        self.rpc("files/get_metadata", json!({"path": dbx_path(path)}), path)
    }

    fn root_entry() -> Entry {
        Entry {
            name: "Dropbox".into(),
            path: RPath::root(),
            kind: EntryKind::Dir,
            size: None,
            modified: None,
            mode: None,
            owner: None,
            group: None,
            link_target: None,
            extra: Vec::new(), checksums: Vec::new(), shared: None,
        }
    }

    /// An entry for `m`, with [`Entry::shared`] set when it is a shared folder's root.
    fn entry_for(&self, path: RPath, m: &Value) -> Result<Entry> {
        let mut e = entry(path, m);
        if let Some(id) = shared_folder_id(m) {
            e.shared = Some(match self.rpc("sharing/get_folder_metadata", json!({"shared_folder_id": id}), &e.path) {
                Ok(info) => {
                    let owned_by_me = info.pointer("/access_type/.tag").and_then(Value::as_str) == Some("owner");
                    // Team folders can't be left by a member either; Dropbox says so when we try.
                    Shared { owned_by_me, can_leave: !owned_by_me }
                }
                Err(err) if err.kind.is_transient() || matches!(err.kind, ErrorKind::Auth | ErrorKind::Closed) => {
                    return Err(err);
                }
                // Dropbox won't describe it (e.g. access through a team): still shared, and
                // the safe answer is that Filewinch can't take it out of the account.
                Err(_) => Shared { owned_by_me: false, can_leave: false },
            });
            e.extra.push(("Shared folder ID".into(), id.to_string()));
        }
        Ok(e)
    }

    /// Waits for an asynchronous sharing job (`sharing/check_job_status`).
    fn wait_for_job(&self, job: &str, subject: &RPath) -> Result<()> {
        let started = std::time::Instant::now();
        let mut delay = std::time::Duration::from_millis(100);
        loop {
            let v = self.rpc("sharing/check_job_status", json!({"async_job_id": job}), subject)?;
            match v[".tag"].as_str() {
                Some("complete") => return Ok(()),
                Some("in_progress") => {}
                _ => {
                    let why = v["error_summary"].as_str().or(v.pointer("/failed/.tag").and_then(Value::as_str));
                    return Err(error(ErrorKind::Other, P.label(), subject, why.unwrap_or("Dropbox couldn't finish leaving the folder")));
                }
            }
            if started.elapsed() > LEAVE_TIMEOUT {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!("Dropbox is still removing “{}” after 5 minutes. It may finish on its own; refresh later to check.", subject.name()),
                ));
            }
            std::thread::sleep(delay);
            delay = (delay * 2).min(std::time::Duration::from_secs(5));
            self.s.check_open()?;
        }
    }

    /// The `.tag` of the item at `path`, plus its revision for files.
    fn kind_of(&self, path: &RPath) -> Result<(EntryKind, Option<String>)> {
        let m = self.metadata(path)?;
        let kind = if m[".tag"] == "folder" {
            EntryKind::Dir
        } else {
            EntryKind::File
        };
        Ok((kind, m["rev"].as_str().map(str::to_string)))
    }
}

impl Remote for Dropbox {
    fn caps(&self) -> Caps {
        Caps {
            resume_read: true,
            resume_write: false,
            permissions: false,
            set_mtime: false,
            symlinks: false,
            real_dirs: true,
            server_rename: true,
            server_copy: true,
        }
    }

    fn home(&self) -> Result<RPath> {
        Ok(RPath::root())
    }

    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        let mut page = self.rpc(
            "files/list_folder",
            json!({"path": dbx_path(dir), "limit": self.tuning.page_size, "include_deleted": false}),
            dir,
        )?;
        let mut out = Vec::new();
        loop {
            for m in page["entries"].as_array().into_iter().flatten() {
                let Some(name) = m["name"].as_str() else {
                    continue;
                };
                if m[".tag"] == "deleted" || name.is_empty() {
                    continue;
                }
                out.push(self.entry_for(dir.join(name), m)?);
            }
            if page["has_more"].as_bool() != Some(true) {
                break;
            }
            let cursor = page["cursor"].as_str().unwrap_or_default().to_string();
            page = self.rpc("files/list_folder/continue", json!({"cursor": cursor}), dir)?;
        }
        Ok(out)
    }

    fn stat(&self, path: &RPath) -> Result<Entry> {
        if path.is_root() {
            self.s.check_open()?;
            return Ok(Self::root_entry());
        }
        let m = self.metadata(path)?;
        self.entry_for(path.clone(), &m)
    }

    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        let mut call = Call::post(format!("{}/files/download", self.ep.upload))
            .header(
                "Dropbox-API-Arg",
                header_arg(&json!({"path": dbx_path(path)})),
            )
            .subject(path)
            .classify(classify);
        if offset > 0 {
            call = call.header("Range", format!("bytes={offset}-"));
        }
        let resp = self.s.send(call)?;
        let partial = resp.status().as_u16() == 206;
        let mut reader: Box<dyn Read + Send> = Box::new(resp.into_body().into_reader());
        if offset > 0 && !partial {
            // The server ignored the range: skip to the offset ourselves.
            let skipped = std::io::copy(&mut (&mut reader).take(offset), &mut std::io::sink())?;
            if skipped < offset {
                return Err(error(
                    ErrorKind::InvalidPath,
                    P.label(),
                    path,
                    "the file is shorter than the resume point",
                ));
            }
        }
        Ok(reader)
    }

    fn write(&self, path: &RPath, offset: u64, size: Option<u64>) -> Result<Box<dyn WriteSink>> {
        if offset > 0 {
            return Err(Error::unsupported("resuming uploads to Dropbox"));
        }
        if path.is_root() {
            return Err(error(
                ErrorKind::InvalidPath,
                P.label(),
                path,
                "that's the Dropbox root",
            ));
        }
        let target = upload::Target::new(
            self.s.clone(),
            self.ep.upload.clone(),
            path.clone(),
            self.tuning.chunk,
        );
        Ok(Box::new(ChunkedSink::new(target, &self.tuning, size)?))
    }

    fn mkdir(&self, path: &RPath) -> Result<()> {
        self.rpc(
            "files/create_folder_v2",
            json!({"path": dbx_path(path), "autorename": false}),
            path,
        )?;
        Ok(())
    }

    fn rename(&self, from: &RPath, to: &RPath) -> Result<()> {
        self.rpc(
            "files/move_v2",
            json!({"from_path": dbx_path(from), "to_path": dbx_path(to), "autorename": false,
                   "allow_ownership_transfer": false}),
            from,
        )?;
        Ok(())
    }

    fn remove_file(&self, path: &RPath) -> Result<()> {
        let (kind, rev) = self.kind_of(path)?;
        if kind == EntryKind::Dir {
            return Err(error(
                ErrorKind::InvalidPath,
                P.label(),
                path,
                "it's a folder, not a file",
            ));
        }
        // `parent_rev` makes Dropbox refuse if the file changed since we looked, and never
        // applies to folders.
        let mut args = json!({"path": dbx_path(path)});
        if let Some(rev) = rev {
            args["parent_rev"] = rev.into();
        }
        self.rpc("files/delete_v2", args, path)?;
        Ok(())
    }

    fn remove_dir(&self, path: &RPath) -> Result<()> {
        if path.is_root() {
            return Err(error(
                ErrorKind::PermissionDenied,
                P.label(),
                path,
                "the Dropbox root",
            ));
        }
        // delete_v2 is recursive; the contract only removes empty folders.
        let page = self.rpc(
            "files/list_folder",
            json!({"path": dbx_path(path), "limit": 1}),
            path,
        )?;
        if page["entries"].as_array().is_some_and(|e| !e.is_empty()) {
            return Err(error(
                ErrorKind::InvalidPath,
                P.label(),
                path,
                "the folder isn't empty",
            ));
        }
        self.rpc("files/delete_v2", json!({"path": dbx_path(path)}), path)?;
        Ok(())
    }

    fn copy(&self, from: &RPath, to: &RPath) -> Result<()> {
        self.rpc(
            "files/copy_v2",
            json!({"from_path": dbx_path(from), "to_path": dbx_path(to), "autorename": false}),
            from,
        )?;
        Ok(())
    }

    fn leave_shared(&self, path: &RPath) -> Result<()> {
        let m = self.metadata(path)?;
        let Some(id) = shared_folder_id(&m) else {
            return Err(error(ErrorKind::InvalidPath, P.label(), path, "it isn't a shared folder"));
        };
        // Keep no copy: the folder stays for its other members, and leaving it is the point.
        let v = self.rpc(
            "sharing/relinquish_folder_membership",
            json!({"shared_folder_id": id, "leave_a_copy": false}),
            path,
        )
        .map_err(|e| leave_error(e, path))?;
        match v["async_job_id"].as_str() {
            Some(job) if v[".tag"] == "async_job_id" => self.wait_for_job(job, path),
            _ => Ok(()),
        }
    }

    fn close(&self) {
        self.s.close();
    }
}

/// Explains why Dropbox refused to let the account leave a shared folder.
fn leave_error(e: Error, path: &RPath) -> Error {
    let d = e.detail.as_deref().unwrap_or("");
    let why = if d.contains("folder_owner") {
        "you own it. Owners can't leave their own shared folder; unshare it on dropbox.com instead"
    } else if d.contains("team_folder") {
        "it's a team folder. Team folders are managed by your Dropbox team admin"
    } else if d.contains("mounted") || d.contains("group_access") || d.contains("no_explicit_access") {
        "you have access through a group or a parent folder. Leave it on dropbox.com instead"
    } else {
        return e;
    };
    let mut out = Error::new(ErrorKind::PermissionDenied, format!("Couldn't leave “{}”: {why}.", path.name()));
    out.detail = e.detail;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_arg_is_ascii() {
        let v = json!({"path": "/Café/😀.txt"});
        assert_eq!(
            header_arg(&v),
            "{\"path\":\"/Caf\\u00e9/\\ud83d\\ude00.txt\"}"
        );
    }

    #[test]
    fn classifies_409_summaries() {
        let v = |s: &str| json!({"error_summary": s});
        assert_eq!(
            classify(409, &v("path/not_found/..")),
            Some(ErrorKind::NotFound)
        );
        assert_eq!(
            classify(409, &v("path/conflict/folder/..")),
            Some(ErrorKind::AlreadyExists)
        );
        assert_eq!(
            classify(409, &v("path/insufficient_space/.")),
            Some(ErrorKind::NoSpace)
        );
        assert_eq!(
            classify(409, &v("to/no_write_permission/")),
            Some(ErrorKind::PermissionDenied)
        );
        assert_eq!(classify(409, &v("something/else")), Some(ErrorKind::Other));
        assert_eq!(classify(500, &v("")), None);
    }
}
