//! Drive items, the virtual top level, duplicate-name handling and path → item resolution.
//!
//! Layout: `/` holds three virtual folders: `My Drive`, `Shared drives` (one folder per shared
//! drive) and `Shared with me` (read-only). Drive allows several items with the same name in a
//! folder; the oldest (by `createdTime`, then ID) keeps the name and later ones get " (2)",
//! " (3)" … before the extension, so names are stable between listings and resolution agrees
//! with what was listed.
//!
//! Google Docs, Sheets, Slides and Drawings have no bytes of their own; they're listed with
//! the extension of the format they export to (`.docx`, `.xlsx`, `.pptx`, `.pdf`) and unknown
//! size. Other Google types (Forms, Sites, My Maps, Jamboards, …) and shortcuts have nothing
//! to download, so they're left out of listings.
//!
//! Sharing ([`Entry::shared`]): everything at the top of `Shared with me`, items in My Drive
//! the user doesn't own (`ownedByMe: false`), and the user's own items others can see
//! (`shared: true`). Only share roots are marked: inside a shared folder, an item owned by the
//! same person inherits the folder's sharing and is left unmarked. Shared drives belong to a
//! team and aren't marked. Shortcuts are hidden, so nothing here can be "left" from Filewinch.

use std::collections::HashSet;
use std::time::SystemTime;

use filewinch_core::{Checksum, Entry, EntryKind, ErrorKind, HashAlgo, RPath, Result, Shared};
use serde_json::Value;

use super::GDrive;
use crate::http::{Call, error};

pub(crate) const MY_DRIVE: &str = "My Drive";
pub(crate) const SHARED_DRIVES: &str = "Shared drives";
pub(crate) const SHARED_WITH_ME: &str = "Shared with me";
pub(crate) const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
/// Requested for every listed item. The checksum fields are what sync's compare-by-checksum
/// uses; Drive only fills them for files with stored bytes (never Google Docs or shortcuts).
const FIELDS: &str = "id,name,mimeType,size,modifiedTime,createdTime,md5Checksum,sha1Checksum,sha256Checksum,driveId,ownedByMe,shared";

/// Which part of Drive an item lives in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Area {
    MyDrive,
    /// A shared drive, by drive ID.
    SharedDrive(String),
    /// Reached through "Shared with me": read-only in Filewinch.
    SharedWithMe,
}

/// How a Google Workspace document downloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Export {
    pub mime: &'static str,
    pub ext: &'static str,
    pub label: &'static str,
}

fn export_for(mime: &str) -> Option<Export> {
    let (mime, ext, label) = match mime {
        "application/vnd.google-apps.document" => (
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "docx",
            "Google Docs",
        ),
        "application/vnd.google-apps.spreadsheet" => (
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "xlsx",
            "Google Sheets",
        ),
        "application/vnd.google-apps.presentation" => (
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "pptx",
            "Google Slides",
        ),
        "application/vnd.google-apps.drawing" => ("application/pdf", "pdf", "Google Drawings"),
        _ => return None,
    };
    Some(Export { mime, ext, label })
}

#[derive(Clone, Debug)]
pub(crate) struct Item {
    pub id: String,
    /// The name Filewinch shows (export extension and duplicate suffix applied).
    pub name: String,
    pub mime: String,
    pub kind: EntryKind,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub created: String,
    pub md5: Option<String>,
    /// Hashes of the bytes `alt=media` returns; empty for folders and Google types.
    pub checksums: Vec<Checksum>,
    pub export: Option<Export>,
    pub area: Area,
    /// See the module docs. `can_leave` is always false: Drive offers no way to drop an item
    /// shared with you short of a shortcut, and Filewinch hides shortcuts.
    pub shared: Option<Shared>,
}

impl Item {
    pub(crate) fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }

    pub(crate) fn entry(&self, path: RPath) -> Entry {
        let mut extra = vec![("Drive ID".to_string(), self.id.clone())];
        if let Some(x) = self.export {
            extra.push((
                "Google type".into(),
                format!("{} (downloads as .{})", x.label, x.ext),
            ));
        } else if !self.is_dir() {
            extra.push(("Content type".into(), self.mime.clone()));
        }
        if let Some(m) = &self.md5 {
            extra.push(("MD5".into(), m.clone()));
        }
        if self.area == Area::SharedWithMe {
            extra.push((
                "Access".into(),
                "Shared with you (read-only in Filewinch)".into(),
            ));
        }
        Entry {
            name: self.name.clone(),
            path,
            kind: self.kind,
            size: self.size,
            modified: self.modified,
            mode: None,
            owner: None,
            group: None,
            link_target: None,
            extra,
            checksums: self.checksums.clone(),
            shared: self.shared,
        }
    }
}

// Short-lived resolution results; boxing the item would only add an allocation per lookup.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Node {
    Root,
    Drives,
    SharedWithMe,
    Item(Item),
}

pub(crate) fn virtual_entry(name: &str) -> Entry {
    Entry {
        name: name.into(),
        path: RPath::root().join(name),
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

/// Makes a Drive name usable as one path component: `/` (legal in Drive) becomes the
/// full-width solidus `／`, and empty, `.` and `..` names get stand-ins.
fn safe_name(raw: &str) -> String {
    match raw {
        "" => "(untitled)".into(),
        "." => "．".into(),
        ".." => "．．".into(),
        _ => raw.replace('/', "／"),
    }
}

/// `report.pdf` → `report (2).pdf`; folders and extension-less names get the suffix at the end.
pub(crate) fn with_suffix(name: &str, n: usize, is_dir: bool) -> String {
    match name.rfind('.').filter(|&i| i > 0 && !is_dir) {
        Some(i) => format!("{} ({n}){}", &name[..i], &name[i..]),
        None => format!("{name} ({n})"),
    }
}

/// The server's hashes of a file's stored bytes. Google types (Docs download as an export,
/// shortcuts have no bytes) and anything that isn't a full-length hex digest give nothing:
/// a wrong checksum would make sync skip a changed file.
fn checksums(v: &Value, mime: &str) -> Vec<Checksum> {
    if mime == FOLDER_MIME || mime.starts_with("application/vnd.google-apps.") {
        return Vec::new();
    }
    [
        ("md5Checksum", HashAlgo::Md5, 32),
        ("sha1Checksum", HashAlgo::Sha1, 40),
        ("sha256Checksum", HashAlgo::Sha256, 64),
    ]
    .into_iter()
    .filter_map(|(key, algo, len)| {
        Checksum::new(algo, v[key].as_str()?).filter(|c| c.hex.len() == len)
    })
    .collect()
}

/// Builds an item from a Drive `File`; `None` for types with nothing to download.
pub(crate) fn item_from(v: &Value, area: &Area) -> Option<Item> {
    let id = v["id"].as_str()?.to_string();
    let raw = v["name"].as_str().unwrap_or_default();
    let mime = v["mimeType"].as_str().unwrap_or_default().to_string();
    let export = export_for(&mime);
    let kind = if mime == FOLDER_MIME {
        EntryKind::Dir
    } else {
        EntryKind::File
    };
    if mime.starts_with("application/vnd.google-apps.")
        && kind != EntryKind::Dir
        && export.is_none()
    {
        return None;
    }
    let mut name = safe_name(raw);
    if let Some(x) = export {
        name = format!("{name}.{}", x.ext);
    }
    // Drive v3 sends `size` as a string.
    let size = match &v["size"] {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    };
    Some(Item {
        id,
        name,
        kind,
        size: if kind == EntryKind::File && export.is_none() {
            Some(size.unwrap_or(0))
        } else {
            None
        },
        modified: v["modifiedTime"]
            .as_str()
            .and_then(crate::time::parse_rfc3339),
        created: v["createdTime"].as_str().unwrap_or_default().to_string(),
        md5: v["md5Checksum"].as_str().map(str::to_string),
        checksums: checksums(v, &mime),
        export,
        area: match (area, v["driveId"].as_str()) {
            (Area::SharedWithMe, _) => Area::SharedWithMe,
            (_, Some(d)) => Area::SharedDrive(d.to_string()),
            (a, None) => a.clone(),
        },
        shared: match (area, v["driveId"].as_str(), v["ownedByMe"].as_bool()) {
            (_, Some(_), _) => None,
            (Area::SharedWithMe, _, _) | (_, _, Some(false)) => {
                Some(Shared { owned_by_me: false, can_leave: false })
            }
            _ if v["shared"].as_bool() == Some(true) => Some(Shared { owned_by_me: true, can_leave: false }),
            _ => None,
        },
        mime,
    })
}

/// Gives every item in one folder a unique display name (see the module docs). Output is
/// sorted by name.
pub(crate) fn assign_names(items: &mut [Item]) {
    items.sort_by(|a, b| (&a.name, &a.created, &a.id).cmp(&(&b.name, &b.created, &b.id)));
    let mut taken: HashSet<String> = items.iter().map(|i| i.name.clone()).collect();
    let mut i = 0;
    while i < items.len() {
        let mut j = i + 1;
        while j < items.len() && items[j].name == items[i].name {
            j += 1;
        }
        let mut n = 2;
        for k in i + 1..j {
            let mut candidate = with_suffix(&items[i].name, n, items[k].is_dir());
            while taken.contains(&candidate) {
                n += 1;
                candidate = with_suffix(&items[i].name, n, items[k].is_dir());
            }
            n += 1;
            taken.insert(candidate.clone());
            items[k].name = candidate;
        }
        i = j;
    }
    items.sort_by(|a, b| a.name.cmp(&b.name));
}

/// Quotes a value for a Drive `q` string.
fn q_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

impl GDrive {
    fn not_found(&self, path: &RPath) -> filewinch_core::Error {
        error(ErrorKind::NotFound, self.s.service(), path, "")
    }

    /// The real ID of My Drive's root folder (fetched once).
    pub(crate) fn root_id(&self) -> Result<String> {
        let mut g = self.root.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(id) = &*g {
            return Ok(id.clone());
        }
        let v: Value = self.s.json(
            Call::get(format!("{}/files/root", self.ep.api))
                .query("fields", "id")
                .subject(MY_DRIVE),
        )?;
        let id = v["id"].as_str().unwrap_or("root").to_string();
        *g = Some(id.clone());
        Ok(id)
    }

    fn my_drive(&self) -> Result<Item> {
        Ok(Item {
            id: self.root_id()?,
            name: MY_DRIVE.into(),
            mime: FOLDER_MIME.into(),
            kind: EntryKind::Dir,
            size: None,
            modified: None,
            created: String::new(),
            md5: None,
            checksums: Vec::new(),
            export: None,
            area: Area::MyDrive,
            shared: None,
        })
    }

    /// Runs a paginated `files.list`.
    fn files_list(&self, q: &str, area: &Area, subject: &RPath) -> Result<Vec<Item>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut call = Call::get(format!("{}/files", self.ep.api))
                .query("q", q)
                .query("fields", format!("nextPageToken,files({FIELDS})"))
                .query("pageSize", self.tuning.page_size.to_string())
                .query("supportsAllDrives", "true")
                .query("includeItemsFromAllDrives", "true")
                .subject(subject)
                .classify(super::classify);
            if let Area::SharedDrive(d) = area {
                call = call.query("corpora", "drive").query("driveId", d.clone());
            }
            if let Some(t) = &token {
                call = call.query("pageToken", t.clone());
            }
            let v: Value = self.s.json(call)?;
            out.extend(
                v["files"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|f| item_from(f, area)),
            );
            match v["nextPageToken"].as_str() {
                Some(t) if !t.is_empty() => token = Some(t.to_string()),
                _ => break,
            }
        }
        assign_names(&mut out);
        Ok(out)
    }

    fn remember(&self, dir: &RPath, items: &[Item]) {
        for i in items {
            self.cache.put(dir.join(&i.name), i.clone());
        }
    }

    /// The children of a folder item (fresh from the server), cached by path.
    pub(crate) fn children(&self, dir: &RPath, parent: &Item) -> Result<Vec<Item>> {
        let q = format!("'{}' in parents and trashed = false", q_escape(&parent.id));
        let mut items = self.files_list(&q, &parent.area, dir)?;
        if let Some(ps) = parent.shared {
            // Same owner as the shared folder: its sharing is inherited, not a share of its own.
            for i in &mut items {
                if i.shared.is_some_and(|s| s.owned_by_me == ps.owned_by_me) {
                    i.shared = None;
                }
            }
        }
        self.remember(dir, &items);
        Ok(items)
    }

    pub(crate) fn shared_drives(&self) -> Result<Vec<Item>> {
        let dir = RPath::root().join(SHARED_DRIVES);
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut call = Call::get(format!("{}/drives", self.ep.api))
                .query("pageSize", self.tuning.page_size.min(100).to_string())
                .query("fields", "nextPageToken,drives(id,name,createdTime)")
                .subject(&dir)
                .classify(super::classify);
            if let Some(t) = &token {
                call = call.query("pageToken", t.clone());
            }
            let v: Value = self.s.json(call)?;
            for d in v["drives"].as_array().into_iter().flatten() {
                let Some(id) = d["id"].as_str() else { continue };
                out.push(Item {
                    id: id.to_string(),
                    name: safe_name(d["name"].as_str().unwrap_or_default()),
                    mime: FOLDER_MIME.into(),
                    kind: EntryKind::Dir,
                    size: None,
                    modified: None,
                    created: d["createdTime"].as_str().unwrap_or_default().to_string(),
                    md5: None,
                    checksums: Vec::new(),
                    export: None,
                    area: Area::SharedDrive(id.to_string()),
                    shared: None,
                });
            }
            match v["nextPageToken"].as_str() {
                Some(t) if !t.is_empty() => token = Some(t.to_string()),
                _ => break,
            }
        }
        assign_names(&mut out);
        self.remember(&dir, &out);
        Ok(out)
    }

    pub(crate) fn shared_with_me(&self) -> Result<Vec<Item>> {
        let dir = RPath::root().join(SHARED_WITH_ME);
        let items = self.files_list(
            "sharedWithMe = true and trashed = false",
            &Area::SharedWithMe,
            &dir,
        )?;
        self.remember(&dir, &items);
        Ok(items)
    }

    /// Looks `name` up in a folder: cache first, then a fresh listing.
    fn child(&self, dir: &RPath, parent: &Node, name: &str) -> Result<Option<Item>> {
        let path = dir.join(name);
        if let Some(i) = self.cache.get(&path) {
            return Ok(Some(i));
        }
        let items = match parent {
            Node::Item(p) if p.is_dir() => self.children(dir, p)?,
            Node::Drives => self.shared_drives()?,
            Node::SharedWithMe => self.shared_with_me()?,
            _ => return Ok(None),
        };
        Ok(items.into_iter().find(|i| i.name == name))
    }

    /// Resolves a path to the virtual top level or a Drive item.
    pub(crate) fn resolve(&self, path: &RPath) -> Result<Node> {
        self.s.check_open()?;
        let comps: Vec<&str> = path.components().collect();
        let (mut node, rest) = match comps.first().copied() {
            None => return Ok(Node::Root),
            Some(MY_DRIVE) => (Node::Item(self.my_drive()?), 1),
            Some(SHARED_DRIVES) => (Node::Drives, 1),
            Some(SHARED_WITH_ME) => (Node::SharedWithMe, 1),
            Some(_) => return Err(self.not_found(path)),
        };
        let mut cur = RPath::root().join(comps[0]);
        for c in &comps[rest..] {
            let found = self.child(&cur, &node, c)?;
            cur = cur.join(c);
            node = Node::Item(found.ok_or_else(|| self.not_found(path))?);
        }
        Ok(node)
    }

    /// Resolves a path that must be a real item (not the virtual top level).
    pub(crate) fn resolve_item(&self, path: &RPath) -> Result<Item> {
        match self.resolve(path)? {
            Node::Item(i) => Ok(i),
            _ => Err(error(
                ErrorKind::PermissionDenied,
                self.s.service(),
                path,
                "it's part of Filewinch's Google Drive layout",
            )),
        }
    }

    /// The item at `path` if it exists (any other error is passed on).
    pub(crate) fn lookup(&self, path: &RPath) -> Result<Option<Item>> {
        match self.resolve(path) {
            Ok(Node::Item(i)) => Ok(Some(i)),
            Ok(_) => Err(error(ErrorKind::AlreadyExists, self.s.service(), path, "")),
            Err(e) if e.kind == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn it(name: &str, created: &str, id: &str, dir: bool) -> Item {
        let mime = if dir { FOLDER_MIME } else { "text/plain" };
        item_from(
            &json!({"id": id, "name": name, "mimeType": mime, "createdTime": created}),
            &Area::MyDrive,
        )
        .unwrap()
    }

    #[test]
    fn duplicates_get_stable_suffixes() {
        let mut v = vec![
            it("report.pdf", "2024-03-01T00:00:00Z", "c", false),
            it("report.pdf", "2024-01-01T00:00:00Z", "z", false),
            it("report (2).pdf", "2024-01-01T00:00:00Z", "r", false),
            it("report.pdf", "2024-01-01T00:00:00Z", "a", false),
            it("v1.2", "2024-01-01T00:00:00Z", "d1", true),
            it("v1.2", "2024-02-01T00:00:00Z", "d2", true),
        ];
        assign_names(&mut v);
        let names: Vec<(&str, &str)> = v.iter().map(|i| (i.name.as_str(), i.id.as_str())).collect();
        assert_eq!(
            names,
            [
                ("report (2).pdf", "r"),
                ("report (3).pdf", "z"),
                ("report (4).pdf", "c"),
                ("report.pdf", "a"),
                ("v1.2", "d1"),
                ("v1.2 (2)", "d2"),
            ]
        );
    }

    #[test]
    fn google_types_and_odd_names() {
        let doc = item_from(
            &json!({"id": "1", "name": "Plan", "mimeType": "application/vnd.google-apps.document"}),
            &Area::MyDrive,
        )
        .unwrap();
        assert_eq!((doc.name.as_str(), doc.size), ("Plan.docx", None));
        assert!(
            item_from(
                &json!({"id": "2", "name": "F", "mimeType": "application/vnd.google-apps.form"}),
                &Area::MyDrive
            )
            .is_none()
        );
        let slash = item_from(
            &json!({"id": "3", "name": "a/b", "mimeType": "text/plain", "size": "12"}),
            &Area::MyDrive,
        )
        .unwrap();
        assert_eq!((slash.name.as_str(), slash.size), ("a／b", Some(12)));
        let shared = item_from(
            &json!({"id": "4", "name": "x", "mimeType": "text/plain", "driveId": "d"}),
            &Area::SharedWithMe,
        )
        .unwrap();
        assert_eq!(shared.area, Area::SharedWithMe);
        assert_eq!(with_suffix(".bashrc", 2, false), ".bashrc (2)");
    }
}
