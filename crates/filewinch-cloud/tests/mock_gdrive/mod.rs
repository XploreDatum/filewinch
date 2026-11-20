//! A stateful in-memory Google Drive v3, shaped on the documented JSON: files.list with
//! `nextPageToken`, files.get (+ `alt=media` with Range), export, multipart and resumable
//! uploads (308 Resume Incomplete with `Range`, 499 on cancel), trash via PATCH, copy,
//! drives.list and about. Auth goes through `support::Tokens`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Value, json};
use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::support::{Mock, Recv, Reply, Tokens};

pub const ROOT: &str = "root-id";
pub const DOC_MIME: &str = "application/vnd.google-apps.document";
pub const FOLDER: &str = "application/vnd.google-apps.folder";
pub const SHORTCUT_MIME: &str = "application/vnd.google-apps.shortcut";

pub fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha1_hex(d: &[u8]) -> String {
    hex(&Sha1::digest(d))
}

pub fn sha256_hex(d: &[u8]) -> String {
    hex(&Sha256::digest(d))
}

/// No MD5 crate in the workspace: the fake's `md5Checksum` is a stand-in 32-digit digest
/// (the first half of the SHA-256), which is all the client can check anyway.
pub fn md5_hex(d: &[u8]) -> String {
    sha256_hex(d)[..32].to_string()
}

#[derive(Clone, Debug)]
pub struct File {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub parents: Vec<String>,
    pub data: Vec<u8>,
    pub created: String,
    pub modified: String,
    pub trashed: bool,
    pub drive_id: Option<String>,
    /// Shared with the user by someone else (listed under `sharedWithMe`).
    pub shared: bool,
    /// In My Drive but owned by someone else (`ownedByMe: false`).
    pub not_mine: bool,
    /// The user's own item, shared with others (`shared: true`). Real Drive reports it on
    /// everything inside a shared folder too; tests set it on children to mimic that.
    pub shared_out: bool,
}

enum Target {
    Create { name: String, parents: Vec<String> },
    Update(String),
}

pub struct Upload {
    target: Target,
    data: Vec<u8>,
    pub cancelled: bool,
}

#[derive(Default)]
pub struct State {
    pub files: HashMap<String, File>,
    pub drives: Vec<(String, String)>,
    pub uploads: HashMap<String, Upload>,
    next: u64,
    /// The next resumable chunk is only half accepted (308 with a short Range).
    pub partial_ack_once: bool,
}

pub type Hook = Box<dyn FnMut(&Recv) -> Option<Reply> + Send>;

pub struct FakeDrive {
    pub state: Arc<Mutex<State>>,
    pub tokens: Arc<Tokens>,
    pub hook: Arc<Mutex<Option<Hook>>>,
    pub mock: Mock,
}

pub fn drive_error(status: u16, reason: &str, message: &str) -> Reply {
    Reply::json(
        status,
        json!({"error": {"code": status, "message": message,
            "errors": [{"domain": "global", "reason": reason, "message": message}]}}),
    )
}

impl FakeDrive {
    pub fn start() -> FakeDrive {
        let state = Arc::new(Mutex::new(State::default()));
        let tokens = Tokens::new(false);
        let hook: Arc<Mutex<Option<Hook>>> = Arc::new(Mutex::new(None));
        let base = Arc::new(OnceLock::<String>::new());
        let (st, tk, hk, b) = (state.clone(), tokens.clone(), hook.clone(), base.clone());
        let mock = Mock::start(move |r| {
            if r.path == "/oauth/token" {
                return tk.handle(r);
            }
            if let Some(reply) = tk.check(r) {
                return reply;
            }
            if let Some(h) = hk.lock().unwrap().as_mut()
                && let Some(reply) = h(r)
            {
                return reply;
            }
            handle(&mut st.lock().unwrap(), b.get().unwrap(), r)
        });
        base.set(mock.base.clone()).unwrap();
        FakeDrive {
            state,
            tokens,
            hook,
            mock,
        }
    }

    pub fn set_hook(&self, f: impl FnMut(&Recv) -> Option<Reply> + Send + 'static) {
        *self.hook.lock().unwrap() = Some(Box::new(f));
    }

    pub fn clear_hook(&self) {
        *self.hook.lock().unwrap() = None;
    }

    pub fn add(&self, name: &str, parent: &str, mime: &str, data: &[u8], created: &str) -> String {
        let mut st = self.state.lock().unwrap();
        let drive_id = drive_of(&st, parent);
        let id = new_id(&mut st);
        st.files.insert(
            id.clone(),
            File {
                id: id.clone(),
                name: name.into(),
                mime: mime.into(),
                parents: vec![parent.into()],
                data: data.to_vec(),
                created: created.into(),
                modified: "2024-01-01T00:00:00.000Z".into(),
                trashed: false,
                drive_id,
                shared: false,
                not_mine: false,
                shared_out: false,
            },
        );
        id
    }

    pub fn add_file(&self, name: &str, parent: &str, data: &[u8]) -> String {
        self.add(
            name,
            parent,
            "application/octet-stream",
            data,
            "2024-01-01T00:00:00.000Z",
        )
    }

    pub fn add_folder(&self, name: &str, parent: &str) -> String {
        self.add(name, parent, FOLDER, b"", "2024-01-01T00:00:00.000Z")
    }

    pub fn add_drive(&self, id: &str, name: &str) {
        self.state
            .lock()
            .unwrap()
            .drives
            .push((id.into(), name.into()));
    }

    pub fn file(&self, id: &str) -> File {
        self.state.lock().unwrap().files[id].clone()
    }

    /// Live (untrashed) files with this name.
    pub fn named(&self, name: &str) -> Vec<File> {
        let st = self.state.lock().unwrap();
        st.files
            .values()
            .filter(|f| f.name == name && !f.trashed)
            .cloned()
            .collect()
    }

    pub fn set_shared(&self, id: &str) {
        self.state.lock().unwrap().files.get_mut(id).unwrap().shared = true;
    }

    #[allow(dead_code)] // used by shared_folders.rs only
    pub fn set_shared_out(&self, id: &str) {
        self.state.lock().unwrap().files.get_mut(id).unwrap().shared_out = true;
    }

    #[allow(dead_code)] // used by shared_folders.rs only
    pub fn set_not_mine(&self, id: &str) {
        self.state.lock().unwrap().files.get_mut(id).unwrap().not_mine = true;
    }
}

fn new_id(st: &mut State) -> String {
    st.next += 1;
    format!("f{:04}", st.next)
}

fn drive_of(st: &State, parent: &str) -> Option<String> {
    if st.drives.iter().any(|(d, _)| d == parent) {
        return Some(parent.to_string());
    }
    st.files.get(parent).and_then(|f| f.drive_id.clone())
}

fn meta(f: &File) -> Value {
    let mut v = json!({
        "kind": "drive#file", "id": f.id, "name": f.name, "mimeType": f.mime,
        "createdTime": f.created, "modifiedTime": f.modified, "parents": f.parents,
    });
    if !f.mime.starts_with("application/vnd.google-apps.") {
        v["size"] = f.data.len().to_string().into();
        v["md5Checksum"] = md5_hex(&f.data).into();
        v["sha1Checksum"] = sha1_hex(&f.data).into();
        v["sha256Checksum"] = sha256_hex(&f.data).into();
    }
    if f.mime == SHORTCUT_MIME {
        // `data` holds the target's ID.
        let target = String::from_utf8_lossy(&f.data).to_string();
        v["shortcutDetails"] = json!({"targetId": target, "targetMimeType": FOLDER});
    }
    if let Some(d) = &f.drive_id {
        v["driveId"] = d.clone().into();
    } else {
        // Drive leaves both out for shared-drive items.
        v["ownedByMe"] = (!f.shared && !f.not_mine).into();
        v["shared"] = (f.shared || f.not_mine || f.shared_out).into();
    }
    v
}

fn not_found(id: &str) -> Reply {
    drive_error(404, "notFound", &format!("File not found: {id}."))
}

fn page<T: Clone>(items: &[T], r: &Recv) -> (Vec<T>, Option<String>) {
    let size: usize = r
        .query
        .get("pageSize")
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    let start: usize = r
        .query
        .get("pageToken")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let end = (start + size).min(items.len());
    let next = (end < items.len()).then(|| end.to_string());
    (items[start.min(end)..end].to_vec(), next)
}

fn handle(st: &mut State, base: &str, r: &Recv) -> Reply {
    let parts: Vec<&str> = r.path.trim_start_matches('/').split('/').collect();
    let q = |k: &str| r.query.get(k).map(String::as_str);
    match (r.method.as_str(), parts.as_slice()) {
        ("GET", ["api", "about"]) => {
            Reply::json(200, json!({"user": {"emailAddress": "ana@example.com"}}))
        }
        ("GET", ["api", "files", "root"]) => Reply::json(200, json!({"id": ROOT})),
        ("GET", ["api", "files"]) => list(st, r),
        ("GET", ["api", "drives"]) => {
            let all: Vec<Value> = st
                .drives
                .iter()
                .map(|(id, n)| json!({"id": id, "name": n, "createdTime": "2023-01-01T00:00:00Z"}))
                .collect();
            let (items, next) = page(&all, r);
            Reply::json(200, json!({"drives": items, "nextPageToken": next}))
        }
        ("POST", ["api", "files"]) => {
            let m = r.json();
            let parents: Vec<String> =
                serde_json::from_value(m["parents"].clone()).unwrap_or_default();
            let parent = parents.first().cloned().unwrap_or(ROOT.into());
            let drive_id = drive_of(st, &parent);
            let id = new_id(st);
            let f = File {
                id: id.clone(),
                name: m["name"].as_str().unwrap_or("Untitled").into(),
                mime: m["mimeType"]
                    .as_str()
                    .unwrap_or("application/octet-stream")
                    .into(),
                parents,
                data: Vec::new(),
                created: "2025-01-01T00:00:00.000Z".into(),
                modified: "2025-01-01T00:00:00.000Z".into(),
                trashed: false,
                drive_id,
                shared: false,
                not_mine: false,
                shared_out: false,
            };
            let v = meta(&f);
            st.files.insert(id, f);
            Reply::json(200, v)
        }
        ("GET", ["api", "files", id]) => {
            let Some(f) = st.files.get(*id) else {
                return not_found(id);
            };
            if q("alt") != Some("media") {
                return Reply::json(200, meta(f));
            }
            if f.mime.starts_with("application/vnd.google-apps.") {
                return drive_error(
                    403,
                    "fileNotDownloadable",
                    "Use Export with Docs Editors files.",
                );
            }
            match r
                .header("Range")
                .and_then(|h| h.strip_prefix("bytes="))
                .and_then(|h| h.strip_suffix('-'))
            {
                Some(n) => {
                    let n: usize = n.parse().unwrap();
                    Reply::new(206)
                        .header(
                            "Content-Range",
                            format!(
                                "bytes {n}-{}/{}",
                                f.data.len().saturating_sub(1),
                                f.data.len()
                            ),
                        )
                        .body(f.data[n.min(f.data.len())..].to_vec())
                }
                None => Reply::new(200).body(f.data.clone()),
            }
        }
        ("GET", ["api", "files", id, "export"]) => {
            let Some(f) = st.files.get(*id) else {
                return not_found(id);
            };
            Reply::new(200).body(format!(
                "EXPORT {} OF {}",
                q("mimeType").unwrap_or("?"),
                f.name
            ))
        }
        ("PATCH", ["api", "files", id]) => {
            let m = r.json();
            let (add, remove) = (
                q("addParents").map(str::to_string),
                q("removeParents").map(str::to_string),
            );
            let new_drive = add.as_deref().map(|a| drive_of(st, a));
            let Some(f) = st.files.get_mut(*id) else {
                return not_found(id);
            };
            if let Some(n) = m["name"].as_str() {
                f.name = n.into();
            }
            if let Some(t) = m["trashed"].as_bool() {
                f.trashed = t;
            }
            if let Some(t) = m["modifiedTime"].as_str() {
                f.modified = t.into();
            }
            if let Some(rm) = remove {
                f.parents.retain(|p| *p != rm);
            }
            if let Some(a) = add {
                f.parents.push(a);
                f.drive_id = new_drive.flatten();
            }
            Reply::json(200, meta(f))
        }
        ("POST", ["api", "files", id, "copy"]) => {
            let Some(src) = st.files.get(*id).cloned() else {
                return not_found(id);
            };
            let m = r.json();
            let parents: Vec<String> =
                serde_json::from_value(m["parents"].clone()).unwrap_or(src.parents.clone());
            let drive_id = drive_of(st, &parents[0]);
            let nid = new_id(st);
            let f = File {
                id: nid.clone(),
                name: m["name"].as_str().unwrap_or(&src.name).into(),
                parents,
                drive_id,
                shared: false,
                not_mine: false,
                shared_out: false,
                created: "2025-02-01T00:00:00.000Z".into(),
                ..src
            };
            let v = meta(&f);
            st.files.insert(nid, f);
            Reply::json(200, v)
        }
        ("POST" | "PATCH", ["upload", "files", rest @ ..]) => {
            let target = match rest {
                [] => None,
                [id] if st.files.contains_key(*id) => Some(id.to_string()),
                [id] => return not_found(id),
                _ => return Reply::new(400),
            };
            match q("uploadType") {
                Some("multipart") => multipart(st, r, target),
                Some("resumable") => {
                    let m = r.json();
                    let t = match target {
                        Some(id) => Target::Update(id),
                        None => Target::Create {
                            name: m["name"].as_str().unwrap_or("Untitled").into(),
                            parents: serde_json::from_value(m["parents"].clone())
                                .unwrap_or_default(),
                        },
                    };
                    st.next += 1;
                    let sid = format!("s{}", st.next);
                    st.uploads.insert(
                        sid.clone(),
                        Upload {
                            target: t,
                            data: Vec::new(),
                            cancelled: false,
                        },
                    );
                    Reply::new(200).header("Location", format!("{base}/upload/session/{sid}"))
                }
                _ => drive_error(400, "badRequest", "Unsupported uploadType"),
            }
        }
        ("PUT", ["upload", "session", sid]) => chunk(st, r, sid),
        ("DELETE", ["upload", "session", sid]) => match st.uploads.get_mut(*sid) {
            Some(u) => {
                u.cancelled = true;
                Reply::new(499)
            }
            None => not_found(sid),
        },
        _ => drive_error(
            400,
            "badRequest",
            &format!("mock has no route {} {}", r.method, r.path),
        ),
    }
}

fn list(st: &State, r: &Recv) -> Reply {
    let q = r.query.get("q").cloned().unwrap_or_default();
    let mut items: Vec<&File> = if q.contains("sharedWithMe = true") {
        st.files
            .values()
            .filter(|f| f.shared && !f.trashed)
            .collect()
    } else {
        let Some(parent) = q.split('\'').nth(1) else {
            return drive_error(400, "invalid", "bad q");
        };
        let parent_known = parent == ROOT
            || st.files.contains_key(parent)
            || st.drives.iter().any(|(d, _)| d == parent);
        if !parent_known {
            return not_found(parent);
        }
        if let Some(d) = drive_of(st, parent) {
            let ok = r.query.get("corpora").map(String::as_str) == Some("drive")
                && r.query.get("driveId") == Some(&d)
                && r.query.get("supportsAllDrives").map(String::as_str) == Some("true")
                && r.query.get("includeItemsFromAllDrives").map(String::as_str) == Some("true");
            if !ok {
                return drive_error(
                    400,
                    "invalid",
                    "Shared drive items need corpora=drive and driveId.",
                );
            }
        }
        st.files
            .values()
            .filter(|f| f.parents.iter().any(|p| p == parent) && !f.trashed)
            .collect()
    };
    items.sort_by(|a, b| a.id.cmp(&b.id));
    let (items, next) = page(&items, r);
    Reply::json(
        200,
        json!({"kind": "drive#fileList", "files": items.iter().map(|f| meta(f)).collect::<Vec<_>>(), "nextPageToken": next}),
    )
}

fn split<'a>(hay: &'a [u8], delim: &[u8]) -> Vec<&'a [u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i + delim.len() <= hay.len() {
        if &hay[i..i + delim.len()] == delim {
            out.push(&hay[start..i]);
            i += delim.len();
            start = i;
        } else {
            i += 1;
        }
    }
    out.push(&hay[start..]);
    out
}

fn part_content(part: &[u8]) -> &[u8] {
    let i = split(part, b"\r\n\r\n")[0].len() + 4;
    let c = &part[i.min(part.len())..];
    c.strip_suffix(b"\r\n").unwrap_or(c)
}

fn multipart(st: &mut State, r: &Recv, target: Option<String>) -> Reply {
    let ct = r.header("Content-Type").unwrap_or("");
    let Some(b) = ct.split("boundary=").nth(1) else {
        return drive_error(400, "badContent", "no boundary");
    };
    let parts = split(&r.body, format!("--{b}").as_bytes());
    if parts.len() < 4 {
        return drive_error(400, "badContent", "expected metadata and media parts");
    }
    let m: Value = serde_json::from_slice(part_content(parts[1])).unwrap_or(Value::Null);
    let data = part_content(parts[2]).to_vec();
    let t = match target {
        Some(id) => Target::Update(id),
        None => Target::Create {
            name: m["name"].as_str().unwrap_or("Untitled").into(),
            parents: serde_json::from_value(m["parents"].clone()).unwrap_or_default(),
        },
    };
    commit(st, t, data)
}

fn commit(st: &mut State, t: Target, data: Vec<u8>) -> Reply {
    let id = match t {
        Target::Update(id) => {
            let f = st.files.get_mut(&id).unwrap();
            f.data = data;
            f.modified = "2025-06-01T00:00:00.000Z".into();
            id
        }
        Target::Create { name, parents } => {
            let drive_id = parents.first().and_then(|p| drive_of(st, p));
            let id = new_id(st);
            st.files.insert(
                id.clone(),
                File {
                    id: id.clone(),
                    name,
                    mime: "application/octet-stream".into(),
                    parents,
                    data,
                    created: "2025-06-01T00:00:00.000Z".into(),
                    modified: "2025-06-01T00:00:00.000Z".into(),
                    trashed: false,
                    drive_id,
                    shared: false,
                    not_mine: false,
                    shared_out: false,
                },
            );
            id
        }
    };
    Reply::json(200, meta(&st.files[&id]))
}

fn chunk(st: &mut State, r: &Recv, sid: &str) -> Reply {
    let partial = std::mem::take(&mut st.partial_ack_once);
    let Some(u) = st.uploads.get_mut(sid) else {
        return not_found(sid);
    };
    if u.cancelled {
        return not_found(sid);
    }
    let range = r
        .header("Content-Range")
        .unwrap_or("")
        .trim_start_matches("bytes ")
        .to_string();
    let (span, total) = range.split_once('/').unwrap_or(("", "*"));
    if span != "*" {
        let start: usize = span.split('-').next().unwrap().parse().unwrap();
        if start != u.data.len() {
            return drive_error(
                400,
                "badRange",
                &format!("expected offset {}, got {start}", u.data.len()),
            );
        }
        let take = if partial && r.body.len() > 1 {
            r.body.len() / 2
        } else {
            r.body.len()
        };
        u.data.extend_from_slice(&r.body[..take]);
    }
    if total != "*" && total.parse::<usize>().ok() == Some(u.data.len()) {
        let u = st.uploads.remove(sid).unwrap();
        return commit(st, u.target, u.data);
    }
    let mut reply = Reply::new(308);
    if !u.data.is_empty() {
        reply = reply.header("Range", format!("bytes=0-{}", u.data.len() - 1));
    }
    reply
}
