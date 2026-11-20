//! Graph `driveItem` parsing and error classification.

use std::time::SystemTime;

use base64::Engine as _;
use filewinch_core::{Checksum, Entry, EntryKind, ErrorKind, HashAlgo, RPath, Shared};
use serde_json::Value;

/// An item's address: its drive (`None` only when Graph didn't say; then `/me/drive` is
/// assumed) and its ID there.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ItemRef {
    pub drive: Option<String>,
    pub id: String,
}

/// A resolved item: which drive it lives in and its ID there.
///
/// For a shortcut ("Add shortcut to My files"), `drive`/`id` address the *target* (usually a
/// folder in the sharer's drive) so listing and reading go through it, and `shortcut` holds the
/// shortcut's own item in the user's drive: the only thing delete, rename and set-mtime may
/// touch. A shortcut is reported as [`EntryKind::Symlink`], so the engine never walks into it
/// on a recursive delete.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Node {
    pub drive: Option<String>,
    pub id: String,
    pub shortcut: Option<ItemRef>,
    pub name: String,
    pub dir: bool,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub etag: Option<String>,
    pub mime: Option<String>,
    pub checksums: Vec<Checksum>,
    /// A shortcut, an item in "Shared with me", or an item with Graph's `shared` facet (the
    /// user shared it). `owned_by_me` is settled by `Inner::children`, which knows the drive.
    pub shared: Option<Shared>,
}

impl Node {
    /// The item that is this path itself: the shortcut for a shortcut, else the item.
    pub(crate) fn own(&self) -> ItemRef {
        self.shortcut.clone().unwrap_or_else(|| ItemRef { drive: self.drive.clone(), id: self.id.clone() })
    }

    pub(crate) fn entry(&self, path: RPath) -> Entry {
        let mut extra = vec![("OneDrive ID".to_string(), self.own().id)];
        if self.shortcut.is_some() {
            let what = if self.dir { "a shared folder" } else { "a shared file" };
            extra.push(("Shortcut to".into(), what.into()));
            extra.push(("Target ID".into(), self.id.clone()));
        }
        if let Some(e) = &self.etag {
            extra.push(("ETag".into(), e.clone()));
        }
        if let Some(m) = &self.mime {
            extra.push(("Content type".into(), m.clone()));
        }
        Entry {
            name: self.name.clone(),
            path,
            kind: if self.shortcut.is_some() {
                EntryKind::Symlink
            } else if self.dir {
                EntryKind::Dir
            } else {
                EntryKind::File
            },
            size: if self.dir || self.shortcut.is_some() { None } else { self.size },
            modified: self.modified,
            mode: None,
            owner: None,
            group: None,
            link_target: None,
            extra,
            checksums: if self.shortcut.is_some() { Vec::new() } else { self.checksums.clone() },
            shared: self.shared,
        }
    }
}

/// `file.hashes`: `sha1Hash` / `sha256Hash` (hex, often upper-case; personal drives) and
/// `quickXorHash` (base64 of 20 bytes; every drive type). Malformed values are dropped.
fn hashes(v: &Value) -> Vec<Checksum> {
    let mut out = Vec::new();
    for (key, algo, len) in [("sha1Hash", HashAlgo::Sha1, 40), ("sha256Hash", HashAlgo::Sha256, 64)] {
        if let Some(c) = s(v, &format!("/file/hashes/{key}")).filter(|h| h.len() == len).and_then(|h| Checksum::new(algo, &h)) {
            out.push(c);
        }
    }
    let qx = s(v, "/file/hashes/quickXorHash").and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok());
    if let Some(bytes) = qx.filter(|b| b.len() == 20) {
        out.push(Checksum::from_bytes(HashAlgo::QuickXor, &bytes));
    }
    out
}

fn s(v: &Value, ptr: &str) -> Option<String> {
    v.pointer(ptr)
        .and_then(Value::as_str)
        .filter(|x| !x.is_empty())
        .map(str::to_string)
}

/// A `driveItem` (child listing, item GET, upload reply). `inherit_drive` is the parent's drive
/// for replies that leave `parentReference.driveId` out.
pub(crate) fn node_from_item(v: &Value, inherit_drive: Option<&str>) -> Option<Node> {
    let id = s(v, "/id")?;
    Some(Node {
        drive: s(v, "/parentReference/driveId").or_else(|| inherit_drive.map(str::to_string)),
        id,
        shortcut: None,
        name: s(v, "/name").unwrap_or_default(),
        // A remote item (a shared folder added to My files) is a folder if its target is.
        dir: v.get("folder").is_some()
            || v.pointer("/remoteItem/folder").is_some()
            || v.get("root").is_some(),
        size: v["size"].as_u64(),
        modified: s(v, "/fileSystemInfo/lastModifiedDateTime")
            .or_else(|| s(v, "/lastModifiedDateTime"))
            .and_then(|t| crate::time::parse_rfc3339(&t)),
        etag: s(v, "/eTag"),
        mime: s(v, "/file/mimeType"),
        checksums: hashes(v),
        // The `shared` facet: someone shared this item. Assumed to be the user until
        // `Inner::children` compares drives.
        shared: v.get("shared").map(|_| Shared { owned_by_me: true, can_leave: false }),
    })
    .map(|mut n| {
        // A shortcut: keep its own address for delete/rename, and browse through to the
        // target (in another drive) when Graph says where that is.
        if v.get("remoteItem").is_some() {
            n.shortcut = Some(n.own());
            n.checksums.clear();
            // Removing a shortcut leaves the sharer's folder alone.
            n.shared = Some(Shared { owned_by_me: false, can_leave: true });
            if let (Some(rid), Some(rdrive)) = (
                s(v, "/remoteItem/id"),
                s(v, "/remoteItem/parentReference/driveId"),
            ) {
                n.id = rid;
                n.drive = Some(rdrive);
            }
        }
        n
    })
}

/// An entry of `sharedWithMe`: the useful item is `remoteItem`, in the sharer's drive.
pub(crate) fn node_from_shared(v: &Value) -> Option<Node> {
    let r = v.get("remoteItem")?;
    let mut n = node_from_item(r, None)?;
    if n.name.is_empty() {
        n.name = s(v, "/name").unwrap_or_default();
    }
    n.dir = r.get("folder").is_some();
    // Graph has no call to take an item off "Shared with me".
    n.shared = Some(Shared { owned_by_me: false, can_leave: false });
    Some(n)
}

/// Graph puts a code in `error.code` (and sometimes a more specific one in `innererror`).
pub(crate) fn classify(status: u16, v: &Value) -> Option<ErrorKind> {
    let mut codes = Vec::new();
    let mut e = v.get("error");
    while let Some(err) = e {
        if let Some(c) = err["code"].as_str() {
            codes.push(c.to_string());
        }
        e = err.get("innererror").or_else(|| err.get("innerError"));
    }
    let has = |c: &str| codes.iter().any(|x| x.eq_ignore_ascii_case(c));
    if status == 429 || has("activityLimitReached") || has("throttledRequest") {
        return Some(ErrorKind::RateLimited);
    }
    if has("itemNotFound") {
        return Some(ErrorKind::NotFound);
    }
    if has("nameAlreadyExists") {
        return Some(ErrorKind::AlreadyExists);
    }
    if has("quotaLimitReached") || has("insufficientStorage") {
        return Some(ErrorKind::NoSpace);
    }
    if has("accessDenied") || has("notAllowed") {
        return Some(ErrorKind::PermissionDenied);
    }
    if has("invalidRequest") && status == 400 {
        return Some(ErrorKind::InvalidPath);
    }
    if has("resourceModified") || status == 412 {
        return Some(ErrorKind::Other);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_items_and_shared_entries() {
        let f = json!({"id": "A", "name": "a.txt", "size": 5, "file": {"mimeType": "text/plain"},
            "parentReference": {"driveId": "d1", "id": "P"},
            "fileSystemInfo": {"lastModifiedDateTime": "2024-03-05T10:20:30Z"}, "eTag": "\"A,1\""});
        let n = node_from_item(&f, None).unwrap();
        assert_eq!(
            (n.drive.as_deref(), n.id.as_str(), n.dir, n.size),
            (Some("d1"), "A", false, Some(5))
        );
        assert!(n.modified.is_some());
        let sh = json!({"id": "X", "name": "Plans", "remoteItem": {"id": "R", "name": "Plans",
            "folder": {"childCount": 2}, "parentReference": {"driveId": "other"}}});
        let n = node_from_shared(&sh).unwrap();
        assert_eq!(
            (n.drive.as_deref(), n.id.as_str(), n.dir),
            (Some("other"), "R", true)
        );
        let shortcut = json!({"id": "S", "name": "Team", "parentReference": {"driveId": "mine"},
            "remoteItem": {"id": "T", "folder": {}, "parentReference": {"driveId": "theirs"}}});
        let n = node_from_item(&shortcut, None).unwrap();
        assert_eq!(
            (n.drive.as_deref(), n.id.as_str(), n.dir),
            (Some("theirs"), "T", true)
        );
        // The shortcut's own item is kept for delete/rename, and it shows as a link.
        assert_eq!(n.own(), ItemRef { drive: Some("mine".into()), id: "S".into() });
        let e = n.entry(RPath::new("/My files/Team"));
        assert_eq!((e.kind, e.size), (EntryKind::Symlink, None));
        // A shared item itself (in sharedWithMe) is not a shortcut.
        assert_eq!(node_from_shared(&sh).unwrap().shortcut, None);
    }

    #[test]
    fn parses_file_hashes_and_drops_bad_ones() {
        let f = json!({"id": "A", "name": "a", "file": {"hashes": {
            "sha1Hash": "0A0A9F2A6772942557AB5355D76AF442F8F65E01",
            "sha256Hash": "DFFD6021BB2BD5B0AF676290809EC3A53191DD81C7F70A4B28688A362182986F",
            "quickXorHash": "SCgDG9jwBhaA4ApvnQMbyBACAAA="}}});
        let hex: Vec<_> = node_from_item(&f, None).unwrap().checksums.into_iter().map(|c| (c.algo, c.hex)).collect();
        assert_eq!(hex, [
            (HashAlgo::Sha1, "0a0a9f2a6772942557ab5355d76af442f8f65e01".to_string()),
            (HashAlgo::Sha256, "dffd6021bb2bd5b0af676290809ec3a53191dd81c7f70a4b28688a362182986f".to_string()),
            (HashAlgo::QuickXor, "4828031bd8f0061680e00a6f9d031bc810020000".to_string()),
        ]);
        let bad = json!({"id": "B", "name": "b", "file": {"hashes": {
            "sha1Hash": "XYZ", "sha256Hash": "abcd", "quickXorHash": "AAAA"}}});
        assert!(node_from_item(&bad, None).unwrap().checksums.is_empty());
        let folder = json!({"id": "C", "name": "c", "folder": {}});
        assert!(node_from_item(&folder, None).unwrap().checksums.is_empty());
    }

    #[test]
    fn classifies_graph_codes() {
        let e = |c: &str| json!({"error": {"code": c, "message": "m"}});
        assert_eq!(classify(404, &e("itemNotFound")), Some(ErrorKind::NotFound));
        assert_eq!(
            classify(403, &e("accessDenied")),
            Some(ErrorKind::PermissionDenied)
        );
        assert_eq!(
            classify(409, &e("nameAlreadyExists")),
            Some(ErrorKind::AlreadyExists)
        );
        assert_eq!(
            classify(507, &e("insufficientStorage")),
            Some(ErrorKind::NoSpace)
        );
        assert_eq!(
            classify(429, &e("activityLimitReached")),
            Some(ErrorKind::RateLimited)
        );
        let inner =
            json!({"error": {"code": "accessDenied", "innererror": {"code": "quotaLimitReached"}}});
        assert_eq!(classify(403, &inner), Some(ErrorKind::NoSpace));
        assert_eq!(classify(500, &json!(null)), None);
    }
}
