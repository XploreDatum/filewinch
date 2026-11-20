//! Box (Content API 2.0). `/` is the account's All Files folder (folder ID `0`); paths are
//! resolved to item IDs by listing each parent, with a bounded, short-lived cache that every
//! write invalidates.
//!
//! - Deletes move items to **Box's trash** (restorable from the Box website).
//! - Changes to an existing item send `If-Match` with the ETag Filewinch last saw; if someone
//!   changed it on Box in the meantime the request fails instead of clobbering their change.
//! - Files up to 20,000,000 bytes (Box's chunked-upload minimum) go up in one multipart
//!   request; larger ones use a chunked upload session with SHA-1 digests per part and for
//!   the whole file. Chunked sessions need the size up front, so a large upload of unknown
//!   size is refused with a clear message.
//! - Overwriting a file uploads a new version (version history is kept by Box).
//! - Collaborated items (`has_collaborations`) carry [`Entry::shared`]. Someone else's item can
//!   be left by deleting the user's own collaboration on it, which leaves it for everyone else;
//!   the owner can't leave their own.

mod upload;

use std::io::Read;
use std::sync::Arc;

use filewinch_core::{
    Caps, Checksum, Connector, Entry, EntryKind, Error, ErrorKind, HashAlgo, Protocol, RPath,
    Remote, Result, Secrets, Shared, Site, WriteSink,
};
use serde_json::{Value, json};

use crate::cache::PathCache;
use crate::http::{Call, Session, error};
use crate::oauth::Provider;
use crate::setup::{ConnectorOptions, Endpoints, Tuning};
use crate::upload::ChunkedSink;

const P: Provider = Provider::Box;
const ROOT_ID: &str = "0";
/// Fields requested for every item.
const FIELDS: &str =
    "type,id,name,size,etag,sha1,modified_at,content_modified_at,has_collaborations,owned_by";

#[derive(Clone, Debug, Default)]
pub struct BoxConnector {
    opts: ConnectorOptions,
}

impl BoxConnector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_options(opts: ConnectorOptions) -> Self {
        BoxConnector { opts }
    }
}

impl Connector for BoxConnector {
    fn protocols(&self) -> &'static [Protocol] {
        &[Protocol::Box]
    }

    fn connect(&self, site: &Site, secrets: Arc<dyn Secrets>) -> Result<Box<dyn Remote>> {
        let s = self.opts.open_session(P, site, secrets)?;
        let tuning = self.opts.tuning(P);
        Ok(Box::new(BoxDrive {
            s,
            ep: self.opts.endpoints(P),
            tuning,
            cache: Arc::new(PathCache::new(tuning.cache_capacity, tuning.cache_ttl)),
            me: std::sync::Mutex::new(None),
        }))
    }
}

/// A resolved Box item.
#[derive(Clone, Debug)]
pub(crate) struct Item {
    pub id: String,
    pub dir: bool,
    pub etag: Option<String>,
    /// `owned_by.id`, to tell the user's own collaborated items from ones shared with them.
    pub owner: Option<String>,
    pub entry: Entry,
}

impl Item {
    fn root() -> Item {
        Item {
            id: ROOT_ID.into(),
            dir: true,
            etag: None,
            owner: None,
            entry: Entry {
                name: "All Files".into(),
                path: RPath::root(),
                kind: EntryKind::Dir,
                size: None,
                modified: None,
                mode: None,
                owner: None,
                group: None,
                link_target: None,
                extra: vec![("Box ID".into(), ROOT_ID.into())], checksums: Vec::new(), shared: None,
            },
        }
    }

    /// Builds an item from a Box file/folder object. `None` for web links and other types.
    pub(crate) fn from_json(path: RPath, v: &Value) -> Option<Item> {
        let dir = match v["type"].as_str()? {
            "folder" => true,
            "file" => false,
            _ => return None,
        };
        let id = v["id"].as_str()?.to_string();
        let etag = v["etag"].as_str().map(str::to_string);
        let mut extra = vec![("Box ID".to_string(), id.clone())];
        if let Some(sha) = v["sha1"].as_str() {
            extra.push(("SHA-1".into(), sha.into()));
        }
        if let Some(e) = &etag {
            extra.push(("ETag".into(), e.clone()));
        }
        let entry = Entry {
            name: v["name"]
                .as_str()
                .map_or_else(|| path.name().to_string(), str::to_string),
            kind: if dir { EntryKind::Dir } else { EntryKind::File },
            size: if dir { None } else { v["size"].as_u64() },
            modified: v["content_modified_at"]
                .as_str()
                .or(v["modified_at"].as_str())
                .and_then(crate::time::parse_rfc3339),
            mode: None,
            owner: None,
            group: None,
            link_target: None,
            extra,
            // Box's `sha1` is of the current version's bytes; only a full 40-digit digest counts.
            checksums: v["sha1"]
                .as_str()
                .filter(|_| !dir)
                .and_then(|h| Checksum::new(HashAlgo::Sha1, h))
                .filter(|c| c.hex.len() == 40)
                .into_iter()
                .collect(),
            // Until `BoxDrive::settle_shared` knows who the user is, assume the safe answer:
            // shared, and not something Filewinch can take out of the account.
            shared: (v["has_collaborations"].as_bool() == Some(true))
                .then_some(Shared { owned_by_me: false, can_leave: false }),
            path,
        };
        Some(Item {
            id,
            dir,
            etag,
            owner: v.pointer("/owned_by/id").and_then(Value::as_str).map(str::to_string),
            entry,
        })
    }
}

/// Maps Box's `{type: "error", status, code, message}` codes.
pub(crate) fn classify(status: u16, v: &Value) -> Option<ErrorKind> {
    let code = v["code"].as_str().unwrap_or("");
    Some(match code {
        "not_found" | "trashed" => ErrorKind::NotFound,
        "item_name_in_use" => ErrorKind::AlreadyExists,
        "access_denied_insufficient_permissions"
        | "access_denied_item_locked"
        | "forbidden"
        | "forbidden_by_policy" => ErrorKind::PermissionDenied,
        "storage_limit_exceeded" => ErrorKind::NoSpace,
        "rate_limit_exceeded" => ErrorKind::RateLimited,
        "item_name_invalid"
        | "item_name_too_long"
        | "name_temporarily_reserved"
        | "folder_not_empty"
        | "cannot_make_collaborated_subfolder_private" => ErrorKind::InvalidPath,
        "precondition_failed" => ErrorKind::Other,
        _ if status == 429 => ErrorKind::RateLimited,
        _ => return None,
    })
}

/// True if `e` came from a 412 (the item's ETag no longer matches).
pub(crate) fn is_stale(e: &Error) -> bool {
    e.detail
        .as_deref()
        .is_some_and(|d| d.starts_with("HTTP 412"))
}

pub(crate) struct BoxDrive {
    s: Arc<Session>,
    ep: Endpoints,
    tuning: Tuning,
    cache: Arc<PathCache<Item>>,
    /// The signed-in user's Box ID (`users/me`), fetched the first time it's needed.
    me: std::sync::Mutex<Option<String>>,
}

impl BoxDrive {
    fn me(&self) -> Result<String> {
        let mut g = self.me.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(id) = &*g {
            return Ok(id.clone());
        }
        let v: Value = self.s.json(
            Call::get(self.api("users/me")).query("fields", "id").subject("your Box account").classify(classify),
        )?;
        let id = v["id"].as_str().map(str::to_string).ok_or_else(|| {
            error(ErrorKind::Other, P.label(), "your Box account", "Box didn't say who is signed in")
        })?;
        *g = Some(id.clone());
        Ok(id)
    }

    /// Settles who owns a collaborated item: the owner can't leave it, anyone else can.
    fn settle_shared(&self, item: &mut Item) -> Result<()> {
        if item.entry.shared.is_some() {
            let owned_by_me = item.owner.as_deref() == Some(self.me()?.as_str());
            item.entry.shared = Some(Shared { owned_by_me, can_leave: !owned_by_me });
        }
        Ok(())
    }

    fn api(&self, rest: &str) -> String {
        format!("{}/{rest}", self.ep.api)
    }

    /// Turns a 412 into a clear "changed meanwhile" error and drops what we knew about `path`.
    /// It stays non-transient on purpose: retrying with a fresh ETag would overwrite or
    /// delete someone else's change without asking.
    fn guard<T>(&self, r: Result<T>, path: &RPath) -> Result<T> {
        r.map_err(|e| {
            if is_stale(&e) {
                self.cache.invalidate(path);
                stale_error(path, e)
            } else {
                e
            }
        })
    }

    /// Lists a folder's children, caching each, following `next_marker`.
    fn children(&self, dir: &RPath, id: &str) -> Result<Vec<Item>> {
        let mut out = Vec::new();
        let mut marker: Option<String> = None;
        loop {
            let mut call = Call::get(self.api(&format!("folders/{id}/items")))
                .query("fields", FIELDS)
                .query("limit", self.tuning.page_size.to_string())
                .query("usemarker", "true")
                .subject(dir)
                .classify(classify);
            if let Some(m) = &marker {
                call = call.query("marker", m.clone());
            }
            let page: Value = self.s.json(call)?;
            for v in page["entries"].as_array().into_iter().flatten() {
                let Some(name) = v["name"]
                    .as_str()
                    .filter(|n| !n.is_empty() && !n.contains('/'))
                else {
                    continue;
                };
                if let Some(mut item) = Item::from_json(dir.join(name), v) {
                    self.settle_shared(&mut item)?;
                    self.cache.put(item.entry.path.clone(), item.clone());
                    out.push(item);
                }
            }
            match page["next_marker"].as_str().filter(|m| !m.is_empty()) {
                Some(m) => marker = Some(m.to_string()),
                None => break,
            }
        }
        Ok(out)
    }

    /// Path → item, listing parents as needed.
    pub(crate) fn resolve(&self, path: &RPath) -> Result<Item> {
        self.s.check_open()?;
        if path.is_root() {
            return Ok(Item::root());
        }
        if let Some(item) = self.cache.get(path) {
            return Ok(item);
        }
        let parent_path = path.parent().unwrap_or_else(RPath::root);
        let parent = self.resolve(&parent_path)?;
        if !parent.dir {
            return Err(error(
                ErrorKind::InvalidPath,
                P.label(),
                &parent_path,
                "it's a file, not a folder",
            ));
        }
        let kids = self.children(&parent_path, &parent.id)?;
        let name = path.name();
        kids.iter()
            .find(|i| i.entry.name == name)
            .or_else(|| {
                kids.iter()
                    .find(|i| i.entry.name.eq_ignore_ascii_case(name))
            })
            .cloned()
            .ok_or_else(|| error(ErrorKind::NotFound, P.label(), path, ""))
    }

    fn resolve_dir(&self, path: &RPath) -> Result<Item> {
        let item = self.resolve(path)?;
        if !item.dir {
            return Err(error(
                ErrorKind::InvalidPath,
                P.label(),
                path,
                "it's a file, not a folder",
            ));
        }
        Ok(item)
    }

    /// `files/{id}` or `folders/{id}`.
    fn item_url(&self, item: &Item) -> String {
        self.api(&format!(
            "{}/{}",
            if item.dir { "folders" } else { "files" },
            item.id
        ))
    }

    fn cache_result(&self, path: &RPath, v: &Value) {
        self.cache.invalidate(path);
        if let Some(mut item) = Item::from_json(path.clone(), v)
            && self.settle_shared(&mut item).is_ok()
        {
            self.cache.put(path.clone(), item);
        }
    }
}

pub(crate) fn stale_error(path: &RPath, e: Error) -> Error {
    let mut out = Error::new(
        ErrorKind::Other,
        format!("“{path}” changed on Box since Filewinch looked at it. Refresh and try again."),
    );
    out.detail = e.detail;
    out
}

fn with_if_match<'a>(call: Call<'a>, item: &Item) -> Call<'a> {
    match &item.etag {
        Some(e) => call.header("If-Match", e.clone()),
        None => call,
    }
}

impl Remote for BoxDrive {
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
        let item = self.resolve_dir(dir)?;
        Ok(self
            .children(dir, &item.id)?
            .into_iter()
            .map(|i| i.entry)
            .collect())
    }

    fn stat(&self, path: &RPath) -> Result<Entry> {
        Ok(self.resolve(path)?.entry)
    }

    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        let item = self.resolve(path)?;
        if item.dir {
            return Err(error(
                ErrorKind::InvalidPath,
                P.label(),
                path,
                "it's a folder, not a file",
            ));
        }
        // Box answers with a redirect to a download host; the agent follows it without
        // forwarding the bearer token.
        let mut call = Call::get(self.api(&format!("files/{}/content", item.id)))
            .subject(path)
            .classify(classify);
        if offset > 0 {
            call = call.header("Range", format!("bytes={offset}-"));
        }
        let resp = self.s.send(call)?;
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

    fn write(&self, path: &RPath, offset: u64, size: Option<u64>) -> Result<Box<dyn WriteSink>> {
        if offset > 0 {
            return Err(Error::unsupported("resuming uploads to Box"));
        }
        if path.is_root() {
            return Err(error(
                ErrorKind::InvalidPath,
                P.label(),
                path,
                "that's the All Files folder",
            ));
        }
        let parent = self.resolve_dir(&path.parent().unwrap_or_else(RPath::root))?;
        let existing = match self.resolve(path) {
            Ok(i) if i.dir => {
                return Err(error(
                    ErrorKind::InvalidPath,
                    P.label(),
                    path,
                    "a folder has that name",
                ));
            }
            Ok(i) => Some(i),
            Err(e) if e.kind == ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let target = upload::Target {
            s: self.s.clone(),
            upload: self.ep.upload.clone(),
            cache: self.cache.clone(),
            path: path.clone(),
            parent_id: parent.id,
            existing,
            session: None,
        };
        Ok(Box::new(ChunkedSink::new(target, &self.tuning, size)?))
    }

    fn mkdir(&self, path: &RPath) -> Result<()> {
        if path.is_root() {
            return Err(error(ErrorKind::AlreadyExists, P.label(), path, ""));
        }
        let parent = self.resolve_dir(&path.parent().unwrap_or_else(RPath::root))?;
        let v: Value = self.s.json(
            Call::post(self.api("folders"))
                .query("fields", FIELDS)
                .json(&json!({"name": path.name(), "parent": {"id": parent.id}}))
                .subject(path)
                .classify(classify),
        )?;
        self.cache_result(path, &v);
        Ok(())
    }

    fn rename(&self, from: &RPath, to: &RPath) -> Result<()> {
        if from.is_root() || to.is_root() {
            return Err(error(ErrorKind::PermissionDenied, P.label(), from, ""));
        }
        let item = self.resolve(from)?;
        let parent = self.resolve_dir(&to.parent().unwrap_or_else(RPath::root))?;
        let call = Call::put(self.item_url(&item))
            .query("fields", FIELDS)
            .json(&json!({"name": to.name(), "parent": {"id": parent.id}}))
            .subject(from)
            .classify(classify);
        let r = self.s.json::<Value>(with_if_match(call, &item));
        let v = self.guard(r, from)?;
        self.cache.invalidate(from);
        self.cache_result(to, &v);
        Ok(())
    }

    fn remove_file(&self, path: &RPath) -> Result<()> {
        let item = self.resolve(path)?;
        if item.dir {
            return Err(error(
                ErrorKind::InvalidPath,
                P.label(),
                path,
                "it's a folder, not a file",
            ));
        }
        let call = Call::delete(self.item_url(&item))
            .subject(path)
            .classify(classify);
        let r = self.s.send(with_if_match(call, &item));
        self.guard(r, path)?;
        self.cache.invalidate(path);
        Ok(())
    }

    fn remove_dir(&self, path: &RPath) -> Result<()> {
        if path.is_root() {
            return Err(error(ErrorKind::PermissionDenied, P.label(), path, ""));
        }
        let item = self.resolve_dir(path)?;
        // recursive=false: Box refuses (folder_not_empty) rather than trashing contents.
        let call = Call::delete(self.item_url(&item))
            .query("recursive", "false")
            .subject(path)
            .classify(classify);
        let r = self.s.send(with_if_match(call, &item));
        self.guard(r, path).map_err(|e| {
            if e.kind == ErrorKind::InvalidPath {
                let mut out = error(
                    ErrorKind::InvalidPath,
                    P.label(),
                    path,
                    "the folder isn't empty",
                );
                out.detail = e.detail;
                out
            } else {
                e
            }
        })?;
        self.cache.invalidate(path);
        Ok(())
    }

    fn copy(&self, from: &RPath, to: &RPath) -> Result<()> {
        if to.is_root() {
            return Err(error(ErrorKind::AlreadyExists, P.label(), to, ""));
        }
        let item = self.resolve(from)?;
        let parent = self.resolve_dir(&to.parent().unwrap_or_else(RPath::root))?;
        let v: Value = self.s.json(
            Call::post(format!("{}/copy", self.item_url(&item)))
                .query("fields", FIELDS)
                .json(&json!({"name": to.name(), "parent": {"id": parent.id}}))
                .subject(to)
                .classify(classify),
        )?;
        self.cache_result(to, &v);
        Ok(())
    }

    fn leave_shared(&self, path: &RPath) -> Result<()> {
        let item = self.resolve(path)?;
        match item.entry.shared {
            Some(s) if s.owned_by_me => {
                return Err(Error::new(
                    ErrorKind::PermissionDenied,
                    format!("“{}” is yours, so it can't be left. Remove its collaborators on box.com instead.", path.name()),
                ));
            }
            Some(_) => {}
            None => return Err(error(ErrorKind::InvalidPath, P.label(), path, "it isn't shared with you")),
        }
        let me = self.me()?;
        let kind = if item.dir { "folders" } else { "files" };
        let url = self.api(&format!("{kind}/{}/collaborations", item.id));
        let mut marker: Option<String> = None;
        let mine = loop {
            let mut call = Call::get(url.clone()).query("fields", "id,accessible_by").subject(path).classify(classify);
            if let Some(m) = &marker {
                call = call.query("marker", m.clone()).query("usemarker", "true");
            }
            let page: Value = self.s.json(call)?;
            let found = page["entries"].as_array().into_iter().flatten().find(|c| {
                c.pointer("/accessible_by/id").and_then(Value::as_str) == Some(me.as_str())
            });
            if let Some(c) = found {
                break c["id"].as_str().map(str::to_string);
            }
            match page["next_marker"].as_str().filter(|m| !m.is_empty()) {
                Some(m) => marker = Some(m.to_string()),
                None => break None,
            }
        };
        let Some(collab) = mine else {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                format!(
                    "You have “{}” through a group or a parent folder, not your own invitation, so Filewinch can't leave it. Leave it on box.com instead.",
                    path.name()
                ),
            ));
        };
        self.s.send(Call::delete(self.api(&format!("collaborations/{collab}"))).subject(path).classify(classify))?;
        self.cache.invalidate(path);
        Ok(())
    }

    fn close(&self) {
        self.s.close();
        self.cache.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_box_codes() {
        let v = |c: &str| json!({"type": "error", "code": c});
        assert_eq!(classify(404, &v("not_found")), Some(ErrorKind::NotFound));
        assert_eq!(
            classify(409, &v("item_name_in_use")),
            Some(ErrorKind::AlreadyExists)
        );
        assert_eq!(
            classify(403, &v("access_denied_insufficient_permissions")),
            Some(ErrorKind::PermissionDenied)
        );
        assert_eq!(
            classify(507, &v("storage_limit_exceeded")),
            Some(ErrorKind::NoSpace)
        );
        assert_eq!(
            classify(400, &v("folder_not_empty")),
            Some(ErrorKind::InvalidPath)
        );
        assert_eq!(classify(429, &v("whatever")), Some(ErrorKind::RateLimited));
        assert_eq!(classify(500, &v("internal_server_error")), None);
    }

    #[test]
    fn item_from_json_skips_web_links() {
        let f = json!({"type": "file", "id": "7", "name": "a.txt", "size": 3, "etag": "2",
                       "sha1": "abc", "content_modified_at": "2024-03-05T02:20:30-08:00"});
        let i = Item::from_json(RPath::new("/a.txt"), &f).unwrap();
        assert_eq!(
            (i.id.as_str(), i.dir, i.etag.as_deref(), i.entry.size),
            ("7", false, Some("2"), Some(3))
        );
        assert!(i.entry.modified.is_some());
        assert!(
            Item::from_json(RPath::new("/l"), &json!({"type": "web_link", "id": "1"})).is_none()
        );
    }
}
