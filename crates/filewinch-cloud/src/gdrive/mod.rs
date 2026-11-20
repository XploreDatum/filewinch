//! Google Drive (API v3). See [`resolve`] for the layout (`/My Drive`, `/Shared drives/<name>`,
//! `/Shared with me`), duplicate names and Google Docs exports. Deleting moves items to Drive's
//! trash. Server-side copy works for files (Drive can't copy folders).

mod resolve;
mod upload;

use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use filewinch_core::{
    Caps, Connector, Entry, Error, ErrorKind, Protocol, RPath, Remote, Result, Secrets, Site,
    WriteSink,
};
use serde_json::{Value, json};

use crate::cache::PathCache;
use crate::http::{Call, Session, error};
use crate::oauth::Provider;
use crate::setup::{ConnectorOptions, Endpoints, Tuning};
use crate::upload::ChunkedSink;

use resolve::{
    Area, FOLDER_MIME, Item, MY_DRIVE, Node, SHARED_DRIVES, SHARED_WITH_ME, virtual_entry,
};

const P: Provider = Provider::GoogleDrive;

#[derive(Clone, Debug, Default)]
pub struct GoogleDriveConnector {
    opts: ConnectorOptions,
}

impl GoogleDriveConnector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_options(opts: ConnectorOptions) -> Self {
        GoogleDriveConnector { opts }
    }
}

impl Connector for GoogleDriveConnector {
    fn protocols(&self) -> &'static [Protocol] {
        &[Protocol::GoogleDrive]
    }

    fn connect(&self, site: &Site, secrets: Arc<dyn Secrets>) -> Result<Box<dyn Remote>> {
        let s = self.opts.open_session(P, site, secrets)?;
        let tuning = self.opts.tuning(P);
        Ok(Box::new(GDrive {
            s,
            ep: self.opts.endpoints(P),
            cache: Arc::new(PathCache::new(tuning.cache_capacity, tuning.cache_ttl)),
            tuning,
            root: Mutex::new(None),
        }))
    }
}

pub(crate) struct GDrive {
    s: Arc<Session>,
    ep: Endpoints,
    tuning: Tuning,
    cache: Arc<PathCache<Item>>,
    root: Mutex<Option<String>>,
}

/// Drive puts the real reason for a 403 in `error.errors[0].reason`.
pub(crate) fn classify(status: u16, v: &Value) -> Option<ErrorKind> {
    let reason = v
        .pointer("/error/errors/0/reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    match (status, reason) {
        (403 | 429, "rateLimitExceeded" | "userRateLimitExceeded" | "sharingRateLimitExceeded") => {
            Some(ErrorKind::RateLimited)
        }
        (403, "storageQuotaExceeded" | "quotaExceeded" | "teamDriveFileLimitExceeded") => {
            Some(ErrorKind::NoSpace)
        }
        (403, "exportSizeLimitExceeded" | "fileNotDownloadable") => Some(ErrorKind::Unsupported),
        (403, "dailyLimitExceeded") => Some(ErrorKind::RateLimited),
        _ => None,
    }
}

fn read_only(path: &RPath) -> Error {
    Error::new(
        ErrorKind::PermissionDenied,
        format!(
            "“{path}” is in “Shared with me”, which is read-only in Filewinch. Copy it into My Drive to change it."
        ),
    )
}

/// The Drive name for `item` placed at `to`: a Google Doc stays a Google Doc, so the export
/// extension Filewinch shows (`.docx`, …) is dropped again.
fn drive_name(item: &Item, to: &RPath) -> String {
    match item.export {
        Some(x) => to
            .name()
            .strip_suffix(&format!(".{}", x.ext))
            .unwrap_or(to.name())
            .to_string(),
        None => to.name().to_string(),
    }
}

impl GDrive {
    fn files(&self) -> String {
        format!("{}/files", self.ep.api)
    }

    fn patch(&self, id: &str, body: &Value, path: &RPath) -> Call<'_> {
        Call::patch(format!("{}/{id}", self.files()))
            .query("supportsAllDrives", "true")
            .query("fields", "id")
            .json(body)
            .subject(path)
            .classify(classify)
    }

    /// A folder new items may be created in.
    fn writable_dir(&self, dir: &RPath) -> Result<Item> {
        match self.resolve(dir)? {
            Node::Item(i) if i.area == Area::SharedWithMe => Err(read_only(dir)),
            Node::Item(i) if !i.is_dir() => Err(error(
                ErrorKind::InvalidPath,
                P.label(),
                dir,
                "it's a file, not a folder",
            )),
            Node::Item(i) => Ok(i),
            Node::SharedWithMe => Err(read_only(dir)),
            Node::Root | Node::Drives => Err(error(
                ErrorKind::PermissionDenied,
                P.label(),
                dir,
                "items can only be created inside My Drive or a shared drive",
            )),
        }
    }

    /// An existing item that may be changed (not read-only, not a drive's top folder).
    fn writable_item(&self, path: &RPath) -> Result<Item> {
        let item = self.resolve_item(path)?;
        let top = match &item.area {
            Area::SharedWithMe => return Err(read_only(path)),
            Area::MyDrive => path.components().count() == 1,
            Area::SharedDrive(d) => *d == item.id,
        };
        if top {
            return Err(error(
                ErrorKind::PermissionDenied,
                P.label(),
                path,
                "it's the top of a drive",
            ));
        }
        Ok(item)
    }

    /// Parent folder of `path` plus the name to create there; fails if the name is taken.
    fn new_slot(&self, path: &RPath) -> Result<Item> {
        let parent = path.parent().unwrap_or_else(RPath::root);
        let dir = self.writable_dir(&parent)?;
        if self.lookup(path)?.is_some() {
            return Err(error(ErrorKind::AlreadyExists, P.label(), path, ""));
        }
        Ok(dir)
    }

    fn trash(&self, item: &Item, path: &RPath) -> Result<()> {
        self.s
            .send(self.patch(&item.id, &json!({"trashed": true}), path))?;
        self.cache.invalidate(path);
        Ok(())
    }

    fn file_item(&self, path: &RPath) -> Result<Item> {
        let item = self.resolve_item(path)?;
        if item.is_dir() {
            return Err(error(
                ErrorKind::InvalidPath,
                P.label(),
                path,
                "it's a folder, not a file",
            ));
        }
        Ok(item)
    }
}

impl Remote for GDrive {
    fn caps(&self) -> Caps {
        Caps {
            resume_read: true,
            resume_write: false,
            permissions: false,
            set_mtime: true,
            symlinks: false,
            real_dirs: true,
            server_rename: true,
            server_copy: true,
            ranged_reads: true,
        }
    }

    fn home(&self) -> Result<RPath> {
        Ok(RPath::root().join(MY_DRIVE))
    }

    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        let items = match self.resolve(dir)? {
            Node::Root => {
                return Ok([MY_DRIVE, SHARED_DRIVES, SHARED_WITH_ME]
                    .into_iter()
                    .map(virtual_entry)
                    .collect());
            }
            Node::Drives => self.shared_drives()?,
            Node::SharedWithMe => self.shared_with_me()?,
            Node::Item(i) if i.is_dir() => self.children(dir, &i)?,
            Node::Item(_) => {
                return Err(error(
                    ErrorKind::InvalidPath,
                    P.label(),
                    dir,
                    "it's a file, not a folder",
                ));
            }
        };
        Ok(items.iter().map(|i| i.entry(dir.join(&i.name))).collect())
    }

    fn stat(&self, path: &RPath) -> Result<Entry> {
        Ok(match self.resolve(path)? {
            Node::Root => {
                let mut e = virtual_entry("Google Drive");
                e.path = RPath::root();
                e
            }
            Node::Drives | Node::SharedWithMe => virtual_entry(path.name()),
            Node::Item(i) => {
                let mut e = i.entry(path.clone());
                if path.components().count() == 1 {
                    e.name = MY_DRIVE.into();
                }
                e
            }
        })
    }

    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        let item = self.file_item(path)?;
        let mut call = match item.export {
            Some(x) => {
                Call::get(format!("{}/{}/export", self.files(), item.id)).query("mimeType", x.mime)
            }
            None => {
                let c = Call::get(format!("{}/{}", self.files(), item.id))
                    .query("alt", "media")
                    .query("supportsAllDrives", "true");
                if offset > 0 {
                    c.header("Range", format!("bytes={offset}-"))
                } else {
                    c
                }
            }
        };
        call = call.subject(path).classify(classify);
        let resp = self.s.send(call)?;
        let partial = resp.status().as_u16() == 206;
        let mut reader: Box<dyn Read + Send> = Box::new(resp.into_body().into_reader());
        if offset > 0 && !partial {
            // Exports (and servers that ignore Range) start at 0: skip to the offset.
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

    fn read_range(&self, path: &RPath, offset: u64, len: u64) -> Result<Box<dyn Read + Send>> {
        let item = self.file_item(path)?;
        if item.export.is_some() {
            return Err(Error::range_ignored("Google Docs exports can't be read in parts"));
        }
        let call = Call::get(format!("{}/{}", self.files(), item.id))
            .query("alt", "media")
            .query("supportsAllDrives", "true")
            .header("Range", filewinch_core::range_header(offset, len))
            .subject(path)
            .classify(classify);
        crate::http::ranged_body(self.s.send(call)?, offset, len, &format!("“{}”", path.name()))
    }
    fn write(&self, path: &RPath, offset: u64, size: Option<u64>) -> Result<Box<dyn WriteSink>> {
        if offset > 0 {
            return Err(Error::unsupported("resuming uploads to Google Drive"));
        }
        let parent_path = path.parent().unwrap_or_else(RPath::root);
        let parent = self.writable_dir(&parent_path)?;
        let existing = match self.lookup(path)? {
            Some(i) if i.is_dir() => {
                return Err(error(
                    ErrorKind::InvalidPath,
                    P.label(),
                    path,
                    "a folder has that name",
                ));
            }
            // A Google Doc can't take uploaded bytes: create a normal file beside it.
            Some(i) if i.export.is_some() => None,
            Some(i) => Some(i.id),
            None => None,
        };
        self.cache.invalidate(path);
        let target = upload::Target {
            s: self.s.clone(),
            upload: self.ep.upload.clone(),
            path: path.clone(),
            cache: self.cache.clone(),
            existing,
            parent: parent.id,
            name: path.name().to_string(),
            chunk: self.tuning.chunk,
            session: None,
        };
        Ok(Box::new(ChunkedSink::new(target, &self.tuning, size)?))
    }

    fn mkdir(&self, path: &RPath) -> Result<()> {
        let dir = self.new_slot(path)?;
        self.s.send(
            Call::post(self.files())
                .query("supportsAllDrives", "true")
                .query("fields", "id")
                .json(&json!({"name": path.name(), "mimeType": FOLDER_MIME, "parents": [dir.id]}))
                .subject(path)
                .classify(classify),
        )?;
        self.cache.invalidate(path);
        Ok(())
    }

    fn rename(&self, from: &RPath, to: &RPath) -> Result<()> {
        let item = self.writable_item(from)?;
        let old_parent = self.resolve_item(&from.parent().unwrap_or_else(RPath::root))?;
        let new_parent = self.new_slot(to)?;
        let mut call = self.patch(&item.id, &json!({"name": drive_name(&item, to)}), from);
        if old_parent.id != new_parent.id {
            call = call
                .query("addParents", new_parent.id.clone())
                .query("removeParents", old_parent.id.clone());
        }
        self.s.send(call)?;
        self.cache.invalidate(from);
        self.cache.invalidate(to);
        Ok(())
    }

    fn remove_file(&self, path: &RPath) -> Result<()> {
        let item = self.writable_item(path)?;
        if item.is_dir() {
            return Err(error(
                ErrorKind::InvalidPath,
                P.label(),
                path,
                "it's a folder, not a file",
            ));
        }
        self.trash(&item, path)
    }

    fn remove_dir(&self, path: &RPath) -> Result<()> {
        let item = self.writable_item(path)?;
        if !item.is_dir() {
            return Err(error(
                ErrorKind::InvalidPath,
                P.label(),
                path,
                "it's a file, not a folder",
            ));
        }
        // Trashing a folder trashes its contents; the contract only removes empty folders.
        let v: Value = self.s.json(
            Call::get(self.files())
                .query("q", format!("'{}' in parents and trashed = false", item.id))
                .query("fields", "files(id)")
                .query("pageSize", "1")
                .query("supportsAllDrives", "true")
                .query("includeItemsFromAllDrives", "true")
                .subject(path)
                .classify(classify),
        )?;
        if v["files"].as_array().is_some_and(|f| !f.is_empty()) {
            return Err(error(
                ErrorKind::InvalidPath,
                P.label(),
                path,
                "the folder isn't empty",
            ));
        }
        self.trash(&item, path)
    }

    fn copy(&self, from: &RPath, to: &RPath) -> Result<()> {
        let item = self.resolve_item(from)?;
        if item.is_dir() {
            return Err(Error::unsupported(
                "copying folders on the server (Google Drive copies files only)",
            ));
        }
        let dir = self.new_slot(to)?;
        let name = drive_name(&item, to);
        self.s.send(
            Call::post(format!("{}/{}/copy", self.files(), item.id))
                .query("supportsAllDrives", "true")
                .query("fields", "id")
                .json(&json!({"name": name, "parents": [dir.id]}))
                .subject(from)
                .classify(classify),
        )?;
        self.cache.invalidate(to);
        Ok(())
    }

    fn set_mtime(&self, path: &RPath, mtime: SystemTime) -> Result<()> {
        let item = self.writable_item(path)?;
        let body = json!({"modifiedTime": crate::time::format_rfc3339(mtime)});
        self.s.send(self.patch(&item.id, &body, path))?;
        self.cache.invalidate(path);
        Ok(())
    }

    fn leave_shared(&self, path: &RPath) -> Result<()> {
        Err(Error::new(
            ErrorKind::Unsupported,
            format!(
                "Google Drive doesn't let Filewinch remove you from “{}”. Remove it from “Shared with me” on drive.google.com.",
                path.name()
            ),
        ))
    }

    fn close(&self) {
        self.s.close();
        self.cache.clear();
    }
}
