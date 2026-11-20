//! An in-memory fake of the Box Content API 2.0, shaped on Box's documented JSON: folder item
//! listings with marker pagination, file/folder objects, multipart uploads, chunked upload
//! sessions (verifying each part's and the whole file's SHA-1 `Digest`), trash, copy, and
//! `If-Match` ETag checks. Real Box refuses chunked sessions under 20 MB; the fake doesn't, so
//! tests can use small files.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

use base64::Engine as _;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};

use crate::support::{Mock, Recv, Reply, Tokens};

#[derive(Clone, Debug)]
pub struct Node {
    pub id: String,
    pub dir: bool,
    pub name: String,
    pub parent: String,
    pub data: Vec<u8>,
    pub etag: u64,
    pub trashed: bool,
    pub versions: u32,
}

struct Session {
    folder_id: String,
    file_id: Option<String>,
    name: String,
    size: u64,
    parts: BTreeMap<u64, Vec<u8>>,
}

#[derive(Default)]
pub struct State {
    pub nodes: BTreeMap<String, Node>,
    next: u64,
    sessions: HashMap<String, Session>,
    pub aborted: Vec<String>,
    /// Commits answer 202 this many times before succeeding.
    pub commit_pending: u32,
    /// Folder IDs whose listing is forbidden.
    pub forbidden: Vec<String>,
    /// One-shot scripted replies: (method, path prefix, reply).
    pub script: VecDeque<(String, String, Reply)>,
    /// Collaborations: (collaboration ID, item ID, user ID). An item with any is
    /// `has_collaborations`.
    pub collabs: Vec<(String, String, String)>,
    /// Item ID → owner's user ID; anything not listed belongs to [`ME`].
    pub owners: HashMap<String, String>,
}

/// The signed-in user's Box ID.
pub const ME: &str = "u-me";

pub const PART_SIZE: u64 = 64 * 1024;

pub struct FakeBox {
    pub state: Mutex<State>,
    pub tokens: Arc<Tokens>,
    base: OnceLock<String>,
}

fn err(status: u16, code: &str, message: &str) -> Reply {
    Reply::json(
        status,
        json!({"type": "error", "status": status, "code": code, "message": message,
                               "request_id": "req-1"}),
    )
}

fn sha1_hex(d: &[u8]) -> String {
    Sha1::digest(d).iter().map(|b| format!("{b:02x}")).collect()
}

fn sha1_digest(d: &[u8]) -> String {
    format!(
        "sha={}",
        base64::engine::general_purpose::STANDARD.encode(Sha1::digest(d))
    )
}

impl State {
    fn obj(&self, n: &Node) -> Value {
        let mut v = json!({
            "type": if n.dir { "folder" } else { "file" },
            "id": n.id, "etag": n.etag.to_string(), "name": n.name,
            "modified_at": "2024-03-05T02:20:30-08:00",
            "content_modified_at": "2024-03-05T02:20:30-08:00",
        });
        if !n.dir {
            v["size"] = n.data.len().into();
            v["sha1"] = sha1_hex(&n.data).into();
        }
        v["has_collaborations"] = self.collabs.iter().any(|c| c.1 == n.id).into();
        let owner = self.owners.get(&n.id).map_or(ME, String::as_str);
        v["owned_by"] = json!({"type": "user", "id": owner});
        v
    }

    /// Shares item `id` (owned by `owner`) with `user`; returns the collaboration ID.
    pub fn collaborate(&mut self, id: &str, owner: &str, user: &str) -> String {
        self.next += 1;
        let cid = format!("c-{}", self.next);
        self.owners.insert(id.into(), owner.into());
        self.collabs.push((cid.clone(), id.into(), user.into()));
        cid
    }

    fn live(&self, id: &str) -> Option<&Node> {
        self.nodes.get(id).filter(|n| !n.trashed)
    }

    fn children(&self, id: &str) -> Vec<&Node> {
        self.nodes
            .values()
            .filter(|n| n.parent == id && !n.trashed && n.id != "0")
            .collect()
    }

    fn conflict(&self, parent: &str, name: &str, except: Option<&str>) -> bool {
        self.children(parent)
            .iter()
            .any(|n| n.name.eq_ignore_ascii_case(name) && Some(n.id.as_str()) != except)
    }

    pub fn add(&mut self, parent: &str, name: &str, dir: bool, data: &[u8]) -> String {
        self.next += 1;
        let id = format!("{}", 1000 + self.next);
        self.nodes.insert(
            id.clone(),
            Node {
                id: id.clone(),
                dir,
                name: name.into(),
                parent: parent.into(),
                data: data.to_vec(),
                etag: 0,
                trashed: false,
                versions: 1,
            },
        );
        id
    }

    /// Finds a live node by path from the root.
    pub fn at(&self, path: &str) -> Option<&Node> {
        let mut cur = "0".to_string();
        for c in path.split('/').filter(|c| !c.is_empty()) {
            cur = self
                .children(&cur)
                .into_iter()
                .find(|n| n.name == c)?
                .id
                .clone();
        }
        self.nodes.get(&cur)
    }

    fn copy_tree(&mut self, id: &str, parent: &str, name: &str) -> String {
        let src = self.nodes[id].clone();
        let new = self.add(parent, name, src.dir, &src.data);
        let kids: Vec<(String, String)> = self
            .children(id)
            .iter()
            .map(|n| (n.id.clone(), n.name.clone()))
            .collect();
        for (kid, kname) in kids {
            self.copy_tree(&kid, &new, &kname);
        }
        new
    }
}

/// Splits `multipart/form-data` into (name, bytes) parts.
fn multipart(r: &Recv) -> Option<Vec<(String, Vec<u8>)>> {
    let ct = r.header("Content-Type")?;
    let boundary = ct.split("boundary=").nth(1)?.trim_matches('"');
    let delim = format!("--{boundary}").into_bytes();
    let body = &r.body;
    let mut idx = Vec::new();
    let mut i = 0;
    while i + delim.len() <= body.len() {
        if body[i..].starts_with(&delim) {
            idx.push(i);
            i += delim.len();
        } else {
            i += 1;
        }
    }
    let mut parts = Vec::new();
    for w in idx.windows(2) {
        let chunk = &body[w[0] + delim.len() + 2..w[1] - 2]; // skip "\r\n" after delim, before next
        let split = chunk.windows(4).position(|x| x == b"\r\n\r\n")?;
        let head = String::from_utf8_lossy(&chunk[..split]).to_string();
        let name = head.split("name=\"").nth(1)?.split('"').next()?.to_string();
        parts.push((name, chunk[split + 4..].to_vec()));
    }
    Some(parts)
}

impl FakeBox {
    pub fn start() -> (Mock, Arc<FakeBox>) {
        let mut st = State::default();
        st.nodes.insert(
            "0".into(),
            Node {
                id: "0".into(),
                dir: true,
                name: "All Files".into(),
                parent: String::new(),
                data: Vec::new(),
                etag: 0,
                trashed: false,
                versions: 1,
            },
        );
        let fake = Arc::new(FakeBox {
            state: Mutex::new(st),
            tokens: Tokens::new(true),
            base: OnceLock::new(),
        });
        let f = fake.clone();
        let mock = Mock::start(move |r| f.handle(r));
        fake.base.set(mock.base.clone()).unwrap();
        (mock, fake)
    }

    pub fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    fn handle(&self, r: &Recv) -> Reply {
        if r.method == "POST" && r.path == "/oauth/token" {
            return self.tokens.handle(r);
        }
        // The download host serves pre-authorised URLs: no bearer token.
        if let Some(id) = r.path.strip_prefix("/download/") {
            return self.download(id, r);
        }
        if let Some(reply) = self.tokens.check(r) {
            return reply;
        }
        {
            let mut st = self.st();
            if let Some(pos) = st
                .script
                .iter()
                .position(|(m, p, _)| *m == r.method && r.path.starts_with(p.as_str()))
            {
                return st.script.remove(pos).unwrap().2;
            }
        }
        let seg: Vec<&str> = r.path.split('/').filter(|s| !s.is_empty()).collect();
        let mut st = self.st();
        match (r.method.as_str(), seg.as_slice()) {
            ("GET", ["api", "folders", id, "items"]) => self.items(&st, id, r),
            ("GET", ["api", "files", id, "content"]) => match st.live(id) {
                Some(n) if !n.dir => Reply::new(302).header(
                    "Location",
                    format!("{}/download/{id}", self.base.get().unwrap()),
                ),
                _ => err(404, "not_found", "Not Found"),
            },
            ("POST", ["upload", "files", "content"]) => self.upload_new(&mut st, r),
            ("POST", ["upload", "files", id, "content"]) => self.upload_version(&mut st, id, r),
            ("POST", ["upload", "files", "upload_sessions"]) => {
                let a = r.json();
                let folder = a["folder_id"].as_str().unwrap_or("").to_string();
                let name = a["file_name"].as_str().unwrap_or("").to_string();
                if st.live(&folder).is_none() {
                    return err(404, "not_found", "Not Found");
                }
                if st.conflict(&folder, &name, None) {
                    return err(
                        409,
                        "item_name_in_use",
                        "Item with the same name already exists",
                    );
                }
                self.new_session(
                    &mut st,
                    folder,
                    None,
                    name,
                    a["file_size"].as_u64().unwrap_or(0),
                )
            }
            ("POST", ["upload", "files", id, "upload_sessions"]) => {
                let Some(n) = st.live(id).cloned() else {
                    return err(404, "not_found", "Not Found");
                };
                let a = r.json();
                self.new_session(
                    &mut st,
                    n.parent,
                    Some(n.id),
                    a["file_name"].as_str().unwrap_or(&n.name).into(),
                    a["file_size"].as_u64().unwrap_or(0),
                )
            }
            ("PUT", ["upload", "files", "upload_sessions", sid]) => self.part(&mut st, sid, r),
            ("POST", ["upload", "files", "upload_sessions", sid, "commit"]) => {
                self.commit(&mut st, sid, r)
            }
            ("DELETE", ["upload", "files", "upload_sessions", sid]) => {
                st.sessions.remove(*sid);
                st.aborted.push(sid.to_string());
                Reply::new(204)
            }
            ("POST", ["api", "folders"]) => {
                let a = r.json();
                let parent = a["parent"]["id"].as_str().unwrap_or("").to_string();
                let name = a["name"].as_str().unwrap_or("").to_string();
                if st.live(&parent).is_none() {
                    return err(404, "not_found", "Not Found");
                }
                if st.conflict(&parent, &name, None) {
                    return err(
                        409,
                        "item_name_in_use",
                        "Item with the same name already exists",
                    );
                }
                let id = st.add(&parent, &name, true, &[]);
                let o = st.obj(&st.nodes[&id]);
                Reply::json(201, o)
            }
            ("PUT", ["api", kind @ ("files" | "folders"), id]) => {
                let Some(n) = st.live(id).cloned() else {
                    return err(404, "not_found", "Not Found");
                };
                if n.dir != (*kind == "folders") {
                    return err(404, "not_found", "Not Found");
                }
                if let Some(bad) = check_etag(r, &n) {
                    return bad;
                }
                let a = r.json();
                let parent = a["parent"]["id"]
                    .as_str()
                    .map_or(n.parent.clone(), str::to_string);
                let name = a["name"].as_str().map_or(n.name.clone(), str::to_string);
                if st.conflict(&parent, &name, Some(id)) {
                    return err(
                        409,
                        "item_name_in_use",
                        "Item with the same name already exists",
                    );
                }
                let node = st.nodes.get_mut(*id).unwrap();
                node.parent = parent;
                node.name = name;
                node.etag += 1;
                let o = st.obj(&st.nodes[*id]);
                Reply::json(200, o)
            }
            ("DELETE", ["api", kind @ ("files" | "folders"), id]) => {
                let Some(n) = st.live(id).cloned() else {
                    return err(404, "not_found", "Not Found");
                };
                if n.dir != (*kind == "folders") {
                    return err(404, "not_found", "Not Found");
                }
                if let Some(bad) = check_etag(r, &n) {
                    return bad;
                }
                if n.dir
                    && r.query.get("recursive").map(String::as_str) != Some("true")
                    && !st.children(id).is_empty()
                {
                    return err(400, "folder_not_empty", "Cannot delete – folder not empty");
                }
                st.nodes.get_mut(*id).unwrap().trashed = true;
                Reply::new(204)
            }
            ("POST", ["api", kind @ ("files" | "folders"), id, "copy"]) => {
                let Some(n) = st.live(id).cloned() else {
                    return err(404, "not_found", "Not Found");
                };
                if n.dir != (*kind == "folders") {
                    return err(404, "not_found", "Not Found");
                }
                let a = r.json();
                let parent = a["parent"]["id"].as_str().unwrap_or("").to_string();
                let name = a["name"].as_str().map_or(n.name.clone(), str::to_string);
                if st.live(&parent).is_none() {
                    return err(404, "not_found", "Not Found");
                }
                if st.conflict(&parent, &name, None) {
                    return err(
                        409,
                        "item_name_in_use",
                        "Item with the same name already exists",
                    );
                }
                let new = st.copy_tree(id, &parent, &name);
                let o = st.obj(&st.nodes[&new]);
                Reply::json(201, o)
            }
            ("GET", ["api", "users", "me"]) => Reply::json(200, json!({"type": "user", "id": ME})),
            ("GET", ["api", "files" | "folders", id, "collaborations"]) => {
                let entries: Vec<Value> = st
                    .collabs
                    .iter()
                    .filter(|c| c.1 == *id)
                    .map(|c| json!({"type": "collaboration", "id": c.0, "accessible_by": {"type": "user", "id": c.2}}))
                    .collect();
                Reply::json(200, json!({"entries": entries, "total_count": entries.len()}))
            }
            ("DELETE", ["api", "collaborations", cid]) => {
                let Some(pos) = st.collabs.iter().position(|c| c.0 == *cid) else {
                    return err(404, "not_found", "Not Found");
                };
                let (_, item, user) = st.collabs.remove(pos);
                // The item stays for its owner; it just leaves this user's All Files.
                if user == ME && let Some(n) = st.nodes.get_mut(&item) {
                    n.parent = "owner-elsewhere".into();
                }
                Reply::new(204)
            }
            _ => err(
                404,
                "not_found",
                &format!("no route for {} {}", r.method, r.path),
            ),
        }
    }

    fn items(&self, st: &State, id: &str, r: &Recv) -> Reply {
        if st.forbidden.iter().any(|f| f == id) {
            return err(
                403,
                "access_denied_insufficient_permissions",
                "Access denied - insufficient permission",
            );
        }
        match st.live(id) {
            Some(n) if n.dir => {}
            _ => return err(404, "not_found", "Not Found"),
        }
        assert_eq!(r.query.get("usemarker").map(String::as_str), Some("true"));
        let limit: usize = r
            .query
            .get("limit")
            .and_then(|l| l.parse().ok())
            .unwrap_or(100);
        let start: usize = r
            .query
            .get("marker")
            .and_then(|m| m.strip_prefix("m-"))
            .and_then(|m| m.parse().ok())
            .unwrap_or(0);
        let mut kids = st.children(id);
        kids.sort_by(|a, b| (!a.dir, &a.name).cmp(&(!b.dir, &b.name)));
        let page: Vec<Value> = kids
            .iter()
            .skip(start)
            .take(limit)
            .map(|n| st.obj(n))
            .collect();
        let mut v =
            json!({"limit": limit, "entries": page, "order": [{"by": "type", "direction": "ASC"}]});
        if start + limit < kids.len() {
            v["next_marker"] = format!("m-{}", start + limit).into();
        }
        Reply::json(200, v)
    }

    fn download(&self, id: &str, r: &Recv) -> Reply {
        let st = self.st();
        let Some(n) = st.live(id) else {
            return Reply::new(404);
        };
        if let Some(range) = r.header("Range").and_then(|h| h.strip_prefix("bytes=")) {
            let from: usize = range.trim_end_matches('-').parse().unwrap();
            let len = n.data.len();
            return Reply::new(206)
                .header(
                    "Content-Range",
                    format!("bytes {from}-{}/{len}", len.saturating_sub(1)),
                )
                .body(n.data[from.min(len)..].to_vec());
        }
        Reply::new(200).body(n.data.clone())
    }

    fn upload_new(&self, st: &mut State, r: &Recv) -> Reply {
        let Some(parts) = multipart(r) else {
            return err(400, "bad_request", "Bad multipart");
        };
        assert_eq!(
            parts[0].0, "attributes",
            "attributes must come before the file"
        );
        let attrs: Value = serde_json::from_slice(&parts[0].1).unwrap();
        let data = &parts.iter().find(|p| p.0 == "file").unwrap().1;
        let parent = attrs["parent"]["id"].as_str().unwrap_or("").to_string();
        let name = attrs["name"].as_str().unwrap_or("").to_string();
        if st.live(&parent).is_none() {
            return err(404, "not_found", "Not Found");
        }
        if st.conflict(&parent, &name, None) {
            return err(
                409,
                "item_name_in_use",
                "Item with the same name already exists",
            );
        }
        let id = st.add(&parent, &name, false, data);
        Reply::json(
            201,
            json!({"total_count": 1, "entries": [st.obj(&st.nodes[&id])]}),
        )
    }

    fn upload_version(&self, st: &mut State, id: &str, r: &Recv) -> Reply {
        let Some(n) = st.live(id).cloned() else {
            return err(404, "not_found", "Not Found");
        };
        if let Some(bad) = check_etag(r, &n) {
            return bad;
        }
        let Some(parts) = multipart(r) else {
            return err(400, "bad_request", "Bad multipart");
        };
        let data = parts.iter().find(|p| p.0 == "file").unwrap().1.clone();
        let node = st.nodes.get_mut(id).unwrap();
        node.data = data;
        node.etag += 1;
        node.versions += 1;
        Reply::json(
            201,
            json!({"total_count": 1, "entries": [st.obj(&st.nodes[id])]}),
        )
    }

    fn new_session(
        &self,
        st: &mut State,
        folder: String,
        file_id: Option<String>,
        name: String,
        size: u64,
    ) -> Reply {
        st.next += 1;
        let sid = format!("sess-{}", st.next);
        st.sessions.insert(
            sid.clone(),
            Session {
                folder_id: folder,
                file_id,
                name,
                size,
                parts: BTreeMap::new(),
            },
        );
        let base = format!(
            "{}/upload/files/upload_sessions/{sid}",
            self.base.get().unwrap()
        );
        Reply::json(
            201,
            json!({
                "id": sid, "type": "upload_session", "part_size": PART_SIZE,
                "total_parts": size.div_ceil(PART_SIZE), "num_parts_processed": 0,
                "session_expires_at": "2030-01-01T00:00:00Z",
                "session_endpoints": {
                    "upload_part": base, "commit": format!("{base}/commit"), "abort": base,
                    "list_parts": format!("{base}/parts"), "status": base,
                }
            }),
        )
    }

    fn part(&self, st: &mut State, sid: &str, r: &Recv) -> Reply {
        let Some(sess) = st.sessions.get_mut(sid) else {
            return err(404, "not_found", "Session not found");
        };
        let range = r.header("Content-Range").unwrap_or("").to_string();
        let (span, total) = range
            .strip_prefix("bytes ")
            .unwrap()
            .split_once('/')
            .unwrap();
        let (a, b) = span.split_once('-').unwrap();
        let (a, b, total): (u64, u64, u64) = (
            a.parse().unwrap(),
            b.parse().unwrap(),
            total.parse().unwrap(),
        );
        if total != sess.size || b - a + 1 != r.body.len() as u64 {
            return err(416, "range_mismatch", "Content-Range doesn't match");
        }
        if b + 1 != total && r.body.len() as u64 != PART_SIZE {
            return err(
                416,
                "range_mismatch",
                "Parts must be part_size except the last",
            );
        }
        if r.header("Digest") != Some(sha1_digest(&r.body).as_str()) {
            return err(412, "sha1_mismatch", "Part digest doesn't match");
        }
        sess.parts.insert(a, r.body.clone());
        Reply::json(
            200,
            json!({"part": {"part_id": format!("{a:08X}"), "offset": a, "size": r.body.len(), "sha1": sha1_hex(&r.body)}}),
        )
    }

    fn commit(&self, st: &mut State, sid: &str, r: &Recv) -> Reply {
        let Some(sess) = st.sessions.get(sid) else {
            return err(404, "not_found", "Session not found");
        };
        let whole: Vec<u8> = sess.parts.values().flatten().copied().collect();
        if whole.len() as u64 != sess.size {
            return err(400, "missing_parts", "Not all parts were uploaded");
        }
        let listed = r.json()["parts"].as_array().map_or(0, Vec::len);
        if listed != sess.parts.len() {
            return err(400, "bad_request", "Parts list doesn't match");
        }
        if r.header("Digest") != Some(sha1_digest(&whole).as_str()) {
            return err(422, "sha1_mismatch", "File digest doesn't match");
        }
        if let Some(fid) = &sess.file_id
            && let Some(bad) = check_etag(r, &st.nodes[fid])
        {
            return bad;
        }
        if st.commit_pending > 0 {
            st.commit_pending -= 1;
            return Reply::new(202).header("Retry-After", "0");
        }
        let sess = st.sessions.remove(sid).unwrap();
        let id = match sess.file_id {
            Some(fid) => {
                let n = st.nodes.get_mut(&fid).unwrap();
                n.data = whole;
                n.etag += 1;
                n.versions += 1;
                fid
            }
            None => {
                if st.conflict(&sess.folder_id, &sess.name, None) {
                    return err(
                        409,
                        "item_name_in_use",
                        "Item with the same name already exists",
                    );
                }
                st.add(&sess.folder_id, &sess.name, false, &whole)
            }
        };
        Reply::json(
            201,
            json!({"total_count": 1, "entries": [st.obj(&st.nodes[&id])]}),
        )
    }
}

fn check_etag(r: &Recv, n: &Node) -> Option<Reply> {
    match r.header("If-Match") {
        Some(e) if e != n.etag.to_string() => Some(err(
            412,
            "precondition_failed",
            "The resource has been modified. Please retrieve the resource again and retry",
        )),
        _ => None,
    }
}
