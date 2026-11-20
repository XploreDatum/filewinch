//! Microsoft OneDrive (personal and work/school) through Microsoft Graph v1.0.
//!
//! Layout: `/` holds two virtual folders, `/My files` (the signed-in user's drive root,
//! `/me/drive/root`) and `/Shared with me` (`/me/drive/sharedWithMe`; each entry points at an
//! item in someone else's drive). Paths are resolved to `(driveId, itemId)` by listing parents,
//! with a bounded, time-limited cache that every write invalidates.
//!
//! A shortcut in My files ("Add shortcut to My files" on a shared folder) is reported as a
//! symlink: listing it lists the shared folder, but delete, rename and set-mtime act only on
//! the shortcut item in the user's own drive, never on the sharer's folder.
//!
//! Shared items carry [`Entry::shared`]: shortcuts (leaving deletes just the shortcut), the
//! items in `/Shared with me` (Graph can't remove them from that list, so they can't be left
//! from Filewinch), and the user's own items with Graph's `shared` facet (the owner can't
//! "leave" those; deleting them would delete them for everyone).
//!
//! Deletes move items to the OneDrive recycle bin. Uploads over `simple_max` use an upload
//! session, which needs the final size up front. Copies are asynchronous on Graph's side;
//! Filewinch polls the monitor URL until the copy finishes.

mod items;
mod upload;

use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use filewinch_core::{
    Caps, Connector, Entry, EntryKind, Error, ErrorKind, Protocol, RPath, Remote, Result,
    Secrets, Site, WriteSink,
};
use serde_json::{Value, json};

use crate::cache::PathCache;
use crate::http::{Call, Session, error, header};
use crate::oauth::{Provider, pct};
use crate::setup::{ConnectorOptions, Endpoints, Tuning};
use crate::upload::ChunkedSink;

use items::{ItemRef, Node, classify, node_from_item, node_from_shared};

const P: Provider = Provider::OneDrive;
const MY_FILES: &str = "My files";
const SHARED: &str = "Shared with me";

/// How long a server-side copy may take before Filewinch stops waiting for it.
const COPY_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Debug, Default)]
pub struct OneDriveConnector {
    opts: ConnectorOptions,
}

impl OneDriveConnector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_options(opts: ConnectorOptions) -> Self {
        OneDriveConnector { opts }
    }
}

impl Connector for OneDriveConnector {
    fn protocols(&self) -> &'static [Protocol] {
        &[Protocol::OneDrive]
    }

    fn connect(&self, site: &Site, secrets: Arc<dyn Secrets>) -> Result<Box<dyn Remote>> {
        let s = self.opts.open_session(P, site, secrets)?;
        let tuning = self.opts.tuning(P);
        Ok(Box::new(OneDrive(Arc::new(Inner {
            s,
            ep: self.opts.endpoints(P),
            cache: PathCache::new(tuning.cache_capacity, tuning.cache_ttl),
            tuning,
        }))))
    }
}

pub(crate) struct Inner {
    pub(crate) s: Arc<Session>,
    pub(crate) ep: Endpoints,
    pub(crate) tuning: Tuning,
    pub(crate) cache: PathCache<Node>,
}

struct OneDrive(Arc<Inner>);

/// Where a path falls in the virtual layout.
enum Zone {
    Root,
    SharedRoot,
    Item,
}

fn zone(path: &RPath) -> Result<Zone> {
    let mut c = path.components();
    match (c.next(), c.next()) {
        (None, _) => Ok(Zone::Root),
        (Some(SHARED), None) => Ok(Zone::SharedRoot),
        (Some(MY_FILES | SHARED), _) => Ok(Zone::Item),
        _ => Err(error(ErrorKind::NotFound, P.label(), path, "")),
    }
}

/// `/`, `/My files`, `/Shared with me` and the items directly in `/Shared with me` can't be
/// created, renamed or deleted from Filewinch.
fn check_writable(path: &RPath) -> Result<()> {
    zone(path)?;
    let parts: Vec<&str> = path.components().collect();
    let fixed = parts.len() < 2 || (parts[0] == SHARED && parts.len() == 2);
    if fixed {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            format!("“{path}” is part of how Filewinch shows OneDrive and can't be changed."),
        ));
    }
    Ok(())
}

fn virtual_dir(path: RPath, name: &str) -> Entry {
    Entry {
        name: name.to_string(),
        path,
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

impl Inner {
    pub(crate) fn service(&self) -> &'static str {
        P.label()
    }

    /// `{api}/drives/{d}/items/{id}`, or `{api}/me/drive/items/{id}` when the drive is unknown.
    pub(crate) fn item_url(&self, n: &Node) -> String {
        self.ref_url(&ItemRef { drive: n.drive.clone(), id: n.id.clone() })
    }

    /// The URL of the item that is the path itself: a shortcut's own item, never its target.
    /// Delete, rename and set-mtime go here so they can't change the sharer's folder.
    fn own_url(&self, n: &Node) -> String {
        self.ref_url(&n.own())
    }

    fn ref_url(&self, r: &ItemRef) -> String {
        match &r.drive {
            Some(d) => format!("{}/drives/{}/items/{}", self.ep.api, pct(d), pct(&r.id)),
            None => format!("{}/me/drive/items/{}", self.ep.api, pct(&r.id)),
        }
    }

    fn get(&self, url: String, subject: &RPath) -> Result<Value> {
        self.s
            .json(Call::get(url).subject(subject).classify(classify))
    }

    fn my_files_root(&self) -> Result<Node> {
        let path = RPath::new(MY_FILES);
        if let Some(n) = self.cache.get(&path) {
            return Ok(n);
        }
        let v = self.get(format!("{}/me/drive/root", self.ep.api), &path)?;
        let mut n = node_from_item(&v, None).ok_or_else(|| self.bad_reply(&path))?;
        n.name = MY_FILES.into();
        self.cache.put(path, n.clone());
        Ok(n)
    }

    fn bad_reply(&self, path: &RPath) -> Error {
        error(
            ErrorKind::Other,
            P.label(),
            path,
            "OneDrive sent an item Filewinch couldn't read",
        )
    }

    /// Resolves a real item (anything below the virtual folders) to its node.
    fn resolve(&self, path: &RPath) -> Result<Node> {
        match zone(path)? {
            Zone::Root | Zone::SharedRoot => {
                return Err(error(
                    ErrorKind::InvalidPath,
                    P.label(),
                    path,
                    "it's a Filewinch folder",
                ));
            }
            Zone::Item => {}
        }
        if path.as_str() == format!("/{MY_FILES}") {
            return self.my_files_root();
        }
        if let Some(n) = self.cache.get(path) {
            return Ok(n);
        }
        let parent = path.parent().unwrap_or_else(RPath::root);
        let children = self.children(&parent)?;
        let name = path.name();
        children
            .iter()
            .find(|n| n.name == name)
            .or_else(|| children.iter().find(|n| n.name.eq_ignore_ascii_case(name)))
            .cloned()
            .ok_or_else(|| error(ErrorKind::NotFound, P.label(), path, ""))
    }

    /// Lists the children of `dir` (a real folder or `/Shared with me`) and caches them.
    fn children(&self, dir: &RPath) -> Result<Vec<Node>> {
        let (mut url, shared, parent) = match zone(dir)? {
            Zone::Root => {
                return Err(error(
                    ErrorKind::InvalidPath,
                    P.label(),
                    dir,
                    "it's a Filewinch folder",
                ));
            }
            Zone::SharedRoot => (format!("{}/me/drive/sharedWithMe", self.ep.api), true, None),
            Zone::Item => {
                let n = self.resolve(dir)?;
                if !n.dir {
                    return Err(error(
                        ErrorKind::InvalidPath,
                        P.label(),
                        dir,
                        "it's a file, not a folder",
                    ));
                }
                (format!("{}/children", self.item_url(&n)), false, Some(n))
            }
        };
        // Whose items these are, for the `shared` facet's owner: only needed off My files.
        let my_drive = match &parent {
            Some(p) if p.drive.is_some() => self.my_files_root()?.drive,
            _ => None,
        };
        let mut first = true;
        let mut out = Vec::new();
        loop {
            let mut call = Call::get(url).subject(dir).classify(classify);
            if first && !shared {
                call = call.query("$top", self.tuning.page_size.to_string());
            }
            first = false;
            let page: Value = self.s.json(call)?;
            for v in page["value"].as_array().into_iter().flatten() {
                let node = if shared {
                    node_from_shared(v)
                } else {
                    node_from_item(v, parent.as_ref().and_then(|p| p.drive.as_deref()))
                };
                let Some(mut node) = node.filter(|n| !n.name.is_empty() && !n.name.contains('/'))
                else {
                    continue;
                };
                if let (Some(p), Some(s)) = (&parent, &mut node.shared)
                    && node.shortcut.is_none()
                {
                    s.owned_by_me = p.drive.is_none() || p.drive == my_drive;
                }
                self.cache.put(dir.join(&node.name), node.clone());
                out.push(node);
            }
            match page["@odata.nextLink"].as_str() {
                Some(next) if !next.is_empty() => url = next.to_string(),
                _ => break,
            }
        }
        Ok(out)
    }

    fn parent_dir(&self, path: &RPath) -> Result<Node> {
        let parent = path.parent().unwrap_or_else(RPath::root);
        let n = self.resolve(&parent)?;
        if !n.dir {
            return Err(error(
                ErrorKind::InvalidPath,
                P.label(),
                &parent,
                "it's a file, not a folder",
            ));
        }
        Ok(n)
    }

    /// Polls an async-copy monitor URL (pre-authorised: no bearer token) until it settles.
    fn wait_for_copy(&self, monitor: &str, subject: &RPath) -> Result<()> {
        let started = Instant::now();
        let mut delay = Duration::from_millis(100);
        loop {
            let mut resp = self.s.send(
                Call::get(monitor)
                    .no_auth()
                    .no_redirects()
                    .accept(&[202, 303])
                    .subject(subject)
                    .classify(classify),
            )?;
            // 303 See Other points at the finished item.
            if resp.status().as_u16() == 303 {
                return Ok(());
            }
            let v: Value = self.s.parse(&mut resp).unwrap_or(Value::Null);
            match v["status"].as_str().unwrap_or("") {
                "completed" => return Ok(()),
                "failed" | "cancelled" | "cancelPending" => {
                    let why = v
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("the copy failed on OneDrive's side");
                    return Err(error(ErrorKind::Other, P.label(), subject, why));
                }
                _ => {}
            }
            if started.elapsed() > COPY_TIMEOUT {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!(
                        "OneDrive is still copying “{subject}” after 30 minutes. It may finish on its \
                         own; refresh later to check."
                    ),
                ));
            }
            std::thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_secs(5));
            self.s.check_open()?;
        }
    }
}

impl Remote for OneDrive {
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
        Ok(RPath::new(MY_FILES))
    }

    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        if let Zone::Root = zone(dir)? {
            self.0.s.check_open()?;
            return Ok(vec![
                virtual_dir(RPath::new(MY_FILES), MY_FILES),
                virtual_dir(RPath::new(SHARED), SHARED),
            ]);
        }
        Ok(self
            .0
            .children(dir)?
            .iter()
            .map(|n| n.entry(dir.join(&n.name)))
            .collect())
    }

    fn stat(&self, path: &RPath) -> Result<Entry> {
        match zone(path)? {
            Zone::Root => {
                self.0.s.check_open()?;
                Ok(virtual_dir(RPath::root(), "OneDrive"))
            }
            Zone::SharedRoot => {
                self.0.s.check_open()?;
                Ok(virtual_dir(path.clone(), SHARED))
            }
            Zone::Item => Ok(self.0.resolve(path)?.entry(path.clone())),
        }
    }

    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        let n = self.0.resolve(path)?;
        if n.dir {
            return Err(error(
                ErrorKind::InvalidPath,
                P.label(),
                path,
                "it's a folder",
            ));
        }
        // `/content` redirects to a pre-authenticated download URL; ureq follows it without
        // forwarding the bearer token.
        let mut call = Call::get(format!("{}/content", self.0.item_url(&n)))
            .subject(path)
            .classify(classify);
        if offset > 0 {
            call = call.header("Range", format!("bytes={offset}-"));
        }
        let resp = self.0.s.send(call)?;
        let partial = resp.status().as_u16() == 206;
        let mut reader: Box<dyn Read + Send> = Box::new(resp.into_body().into_reader());
        if offset > 0 && !partial {
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
        let n = self.0.resolve(path)?;
        if n.dir {
            return Err(error(ErrorKind::InvalidPath, P.label(), path, "it's a folder"));
        }
        let call = Call::get(format!("{}/content", self.0.item_url(&n)))
            .header("Range", filewinch_core::range_header(offset, len))
            .subject(path)
            .classify(classify);
        crate::http::ranged_body(self.0.s.send(call)?, offset, len, &format!("“{}”", path.name()))
    }
    fn write(&self, path: &RPath, offset: u64, size: Option<u64>) -> Result<Box<dyn WriteSink>> {
        if offset > 0 {
            return Err(Error::unsupported("resuming uploads to OneDrive"));
        }
        check_writable(path)?;
        let parent = self.0.parent_dir(path)?;
        self.0.cache.invalidate(path);
        let target = upload::Target::new(self.0.clone(), parent, path.clone());
        Ok(Box::new(ChunkedSink::new(target, &self.0.tuning, size)?))
    }

    fn mkdir(&self, path: &RPath) -> Result<()> {
        check_writable(path)?;
        let parent = self.0.parent_dir(path)?;
        self.0.cache.invalidate(path);
        self.0.s.send(
            Call::post(format!("{}/children", self.0.item_url(&parent)))
                .json(&json!({
                    "name": path.name(),
                    "folder": {},
                    "@microsoft.graph.conflictBehavior": "fail",
                }))
                .subject(path)
                .classify(classify),
        )?;
        Ok(())
    }

    fn rename(&self, from: &RPath, to: &RPath) -> Result<()> {
        check_writable(from)?;
        check_writable(to)?;
        let n = self.0.resolve(from)?;
        let parent = self.0.parent_dir(to)?;
        let own = n.own();
        if own.drive.is_some() && parent.drive.is_some() && own.drive != parent.drive {
            return Err(Error::unsupported(
                "moving items between different people's OneDrives",
            ));
        }
        self.0.cache.invalidate(from);
        self.0.cache.invalidate(to);
        self.0.s.send(
            Call::patch(self.0.own_url(&n))
                .query("@microsoft.graph.conflictBehavior", "fail")
                .json(&json!({"name": to.name(), "parentReference": {"id": parent.id}}))
                .subject(from)
                .classify(classify),
        )?;
        Ok(())
    }

    fn remove_file(&self, path: &RPath) -> Result<()> {
        check_writable(path)?;
        let n = self.0.resolve(path)?;
        // A shortcut is deleted like a file, whatever it points at: only the shortcut goes.
        if n.dir && n.shortcut.is_none() {
            return Err(error(
                ErrorKind::InvalidPath,
                P.label(),
                path,
                "it's a folder, not a file",
            ));
        }
        self.0.cache.invalidate(path);
        self.0.s.send(
            Call::delete(self.0.own_url(&n))
                .subject(path)
                .classify(classify),
        )?;
        Ok(())
    }

    fn remove_dir(&self, path: &RPath) -> Result<()> {
        check_writable(path)?;
        let n = self.0.resolve(path)?;
        if n.shortcut.is_some() {
            return Err(error(
                ErrorKind::InvalidPath,
                P.label(),
                path,
                "it's a shortcut to a shared item; delete it as a file to remove just the shortcut",
            ));
        }
        if !n.dir {
            return Err(error(
                ErrorKind::InvalidPath,
                P.label(),
                path,
                "it's a file, not a folder",
            ));
        }
        // DELETE is recursive on Graph; the contract only removes empty folders. Ask fresh.
        let page: Value = self.0.s.json(
            Call::get(format!("{}/children", self.0.item_url(&n)))
                .query("$top", "1")
                .subject(path)
                .classify(classify),
        )?;
        if page["value"].as_array().is_some_and(|v| !v.is_empty()) {
            return Err(error(
                ErrorKind::InvalidPath,
                P.label(),
                path,
                "the folder isn't empty",
            ));
        }
        self.0.cache.invalidate(path);
        self.0.s.send(
            Call::delete(self.0.item_url(&n))
                .subject(path)
                .classify(classify),
        )?;
        Ok(())
    }

    fn copy(&self, from: &RPath, to: &RPath) -> Result<()> {
        check_writable(to)?;
        let n = self.0.resolve(from)?;
        let parent = self.0.parent_dir(to)?;
        let mut parent_ref = json!({"id": parent.id});
        if let Some(d) = &parent.drive {
            parent_ref["driveId"] = d.clone().into();
        }
        self.0.cache.invalidate(to);
        let resp = self.0.s.send(
            Call::post(format!("{}/copy", self.0.item_url(&n)))
                .query("@microsoft.graph.conflictBehavior", "fail")
                .json(&json!({"parentReference": parent_ref, "name": to.name()}))
                .accept(&[202])
                .subject(from)
                .classify(classify),
        )?;
        let monitor = header(&resp, "location").ok_or_else(|| {
            error(
                ErrorKind::Other,
                P.label(),
                from,
                "OneDrive didn't say where to follow the copy",
            )
        })?;
        drop(resp);
        let result = self.0.wait_for_copy(&monitor, to);
        self.0.cache.invalidate(to);
        result
    }

    fn set_mtime(&self, path: &RPath, mtime: SystemTime) -> Result<()> {
        check_writable(path)?;
        let n = self.0.resolve(path)?;
        self.0.cache.invalidate(path);
        self.0.s.send(
            Call::patch(self.0.own_url(&n))
                .json(&json!({"fileSystemInfo": {
                    "lastModifiedDateTime": crate::time::format_rfc3339(mtime)
                }}))
                .subject(path)
                .classify(classify),
        )?;
        Ok(())
    }

    fn leave_shared(&self, path: &RPath) -> Result<()> {
        let n = self.0.resolve(path)?;
        let parts: Vec<&str> = path.components().collect();
        if n.shortcut.is_some() {
            check_writable(path)?;
            // Only the shortcut in the user's own drive goes; the sharer's folder stays.
            self.0.cache.invalidate(path);
            self.0.s.send(Call::delete(self.0.own_url(&n)).subject(path).classify(classify))?;
            return Ok(());
        }
        if parts.len() == 2 && parts[0] == SHARED {
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!(
                    "OneDrive doesn't let apps take “{}” off “Shared with me”. Remove it there on onedrive.com.",
                    path.name()
                ),
            ));
        }
        Err(error(ErrorKind::InvalidPath, P.label(), path, "it isn't something shared with you"))
    }

    fn close(&self) {
        self.0.s.close();
        self.0.cache.clear();
    }
}
