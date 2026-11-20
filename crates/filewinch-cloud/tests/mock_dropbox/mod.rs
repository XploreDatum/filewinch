//! An in-memory Dropbox shaped on the v2 API: RPC endpoints under `/api`, content endpoints
//! under `/upload`, errors as HTTP 409 with `error_summary`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::support::{Mock, Recv, Reply, Tokens};

#[derive(Clone)]
pub struct Node {
    pub display: String,
    pub dir: bool,
    pub data: Vec<u8>,
    pub rev: u64,
}

#[derive(Default)]
pub struct State {
    /// Lower-cased path → node (Dropbox paths are case-insensitive). Root is implicit.
    pub nodes: BTreeMap<String, Node>,
    pub sessions: BTreeMap<String, Vec<u8>>,
    pub next: u64,
    /// Returned (once) instead of handling the next API call.
    pub fail_next: Option<(u16, Value, Option<String>)>,
    /// Paths that refuse writes (shared read-only folder).
    pub read_only: Vec<String>,
    /// Lower-cased path of a shared folder's root → (shared_folder_id, access type:
    /// "owner", "editor", …).
    pub shares: BTreeMap<String, (String, String)>,
    /// Shared folder IDs the account left, and whether it asked to keep a copy.
    pub left: Vec<(String, bool)>,
    /// `relinquish_folder_membership` answers with an async job, done after this many polls.
    pub leave_async_polls: Option<u32>,
}

pub struct FakeDropbox {
    pub mock: Mock,
    pub tokens: Arc<Tokens>,
    pub state: Arc<Mutex<State>>,
}

fn conflict(summary: &str) -> Reply {
    Reply::json(
        409,
        json!({"error_summary": summary, "error": {".tag": summary.split('/').next().unwrap()}}),
    )
}

fn parent_of(p: &str) -> String {
    match p.rfind('/') {
        Some(0) | None => String::new(),
        Some(i) => p[..i].to_string(),
    }
}

/// Dropbox's `content_hash`: SHA-256 over the SHA-256s of each 4 MiB block.
pub fn content_hash(data: &[u8]) -> String {
    let mut all = Sha256::new();
    for block in data.chunks(4 * 1024 * 1024) {
        all.update(Sha256::digest(block));
    }
    all.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn meta(st: &State, n: &Node) -> Value {
    let name = n.display.rsplit('/').next().unwrap();
    if n.dir {
        let mut v = json!({".tag": "folder", "name": name, "path_display": n.display, "path_lower": n.display.to_lowercase(), "id": format!("id:{}", n.rev)});
        if let Some((id, _)) = st.shares.get(&n.display.to_lowercase()) {
            v["sharing_info"] = json!({"read_only": false, "shared_folder_id": id, "traverse_only": false, "no_access": false});
        }
        v
    } else {
        json!({".tag": "file", "name": name, "path_display": n.display, "path_lower": n.display.to_lowercase(),
               "id": format!("id:{}", n.rev), "rev": format!("{:09x}", n.rev), "size": n.data.len(),
               "client_modified": "2024-03-05T10:20:30Z", "server_modified": "2024-03-05T10:20:31Z",
               "content_hash": content_hash(&n.data), "is_downloadable": true})
    }
}

impl State {
    pub fn put_file(&mut self, path: &str, data: &[u8]) {
        self.ensure_dir(&parent_of(path));
        self.next += 1;
        let n = Node {
            display: path.into(),
            dir: false,
            data: data.to_vec(),
            rev: self.next,
        };
        self.nodes.insert(path.to_lowercase(), n);
    }

    pub fn ensure_dir(&mut self, path: &str) {
        if path.is_empty() || self.nodes.contains_key(&path.to_lowercase()) {
            return;
        }
        self.ensure_dir(&parent_of(path));
        self.next += 1;
        let n = Node {
            display: path.into(),
            dir: true,
            data: Vec::new(),
            rev: self.next,
        };
        self.nodes.insert(path.to_lowercase(), n);
    }

    pub fn file(&self, path: &str) -> Option<Vec<u8>> {
        self.nodes
            .get(&path.to_lowercase())
            .filter(|n| !n.dir)
            .map(|n| n.data.clone())
    }

    fn children(&self, dir: &str) -> Vec<Node> {
        let d = dir.to_lowercase();
        self.nodes
            .iter()
            .filter(|(k, _)| parent_of(k) == d)
            .map(|(_, n)| n.clone())
            .collect()
    }

    fn exists_dir(&self, p: &str) -> bool {
        p.is_empty() || self.nodes.get(&p.to_lowercase()).is_some_and(|n| n.dir)
    }

    fn writable(&self, p: &str) -> bool {
        !self
            .read_only
            .iter()
            .any(|r| p.to_lowercase().starts_with(&r.to_lowercase()))
    }

    fn commit(&mut self, path: &str, data: Vec<u8>) -> Reply {
        if !self.writable(path) {
            return conflict("path/no_write_permission/..");
        }
        if !self.exists_dir(&parent_of(path)) {
            self.ensure_dir(&parent_of(path));
        }
        if self.nodes.get(&path.to_lowercase()).is_some_and(|n| n.dir) {
            return conflict("path/conflict/folder/..");
        }
        self.put_file(path, &data);
        Reply::json(200, meta(self, &self.nodes[&path.to_lowercase()]))
    }

    fn subtree(&self, p: &str) -> Vec<String> {
        let l = p.to_lowercase();
        self.nodes
            .keys()
            .filter(|k| **k == l || k.starts_with(&format!("{l}/")))
            .cloned()
            .collect()
    }
}

impl FakeDropbox {
    pub fn start(rotate: bool) -> FakeDropbox {
        let tokens = Tokens::new(rotate);
        let state = Arc::new(Mutex::new(State::default()));
        let (t, s) = (tokens.clone(), state.clone());
        let mock = Mock::start(move |r| {
            if r.path == "/oauth/token" {
                return t.handle(r);
            }
            if let Some(reply) = t.check(r) {
                return reply;
            }
            let mut st = s.lock().unwrap();
            if let Some((status, body, retry)) = st.fail_next.take() {
                let mut rep = Reply::json(status, body);
                if let Some(ra) = retry {
                    rep = rep.header("Retry-After", ra);
                }
                return rep;
            }
            handle(&mut st, r)
        });
        FakeDropbox {
            mock,
            tokens,
            state,
        }
    }
}

/// Headers must be ASCII; Dropbox escapes the rest as `\uXXXX`.
fn ascii_json(v: &Value) -> String {
    let mut out = String::new();
    for c in v.to_string().chars() {
        if c.is_ascii() {
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

fn arg(r: &Recv) -> Value {
    let raw = r.header("Dropbox-API-Arg").map(str::to_string);
    match raw {
        Some(a) => serde_json::from_str(&a).expect("Dropbox-API-Arg must be JSON"),
        None => r.json(),
    }
}

fn page(st: &State, dir: &str, from: usize, limit: usize) -> Reply {
    let kids = st.children(dir);
    let end = (from + limit).min(kids.len());
    let entries: Vec<Value> = kids[from..end].iter().map(|n| meta(st, n)).collect();
    Reply::json(
        200,
        json!({"entries": entries, "cursor": format!("{end}|{dir}"), "has_more": end < kids.len()}),
    )
}

fn handle(st: &mut State, r: &Recv) -> Reply {
    let a = arg(r);
    let path = |k: &str| a[k].as_str().unwrap_or("").to_string();
    match r.path.as_str() {
        "/api/files/list_folder" => {
            let p = path("path");
            if !st.exists_dir(&p) {
                let summary = if st.nodes.contains_key(&p.to_lowercase()) {
                    "path/not_folder/.."
                } else {
                    "path/not_found/.."
                };
                return conflict(summary);
            }
            page(st, &p, 0, a["limit"].as_u64().unwrap_or(2000) as usize)
        }
        "/api/files/list_folder/continue" => {
            let c = path("cursor");
            let (from, dir) = c.split_once('|').unwrap();
            page(st, dir, from.parse().unwrap(), 2)
        }
        "/api/files/get_metadata" => match st.nodes.get(&path("path").to_lowercase()) {
            Some(n) => Reply::json(200, meta(st, n)),
            None => conflict("path/not_found/.."),
        },
        "/upload/files/download" => {
            let Some(data) = st.file(&path("path")) else {
                return conflict("path/not_found/..");
            };
            let n = &st.nodes[&path("path").to_lowercase()];
            let res = ascii_json(&meta(st, n));
            match r
                .header("Range")
                .and_then(|v| v.strip_prefix("bytes="))
                .and_then(|v| v.strip_suffix('-'))
            {
                Some(from) => {
                    let from: usize = from.parse().unwrap();
                    Reply::new(206)
                        .header("Dropbox-API-Result", res)
                        .body(data[from.min(data.len())..].to_vec())
                }
                None => Reply::new(200).header("Dropbox-API-Result", res).body(data),
            }
        }
        "/upload/files/upload" => {
            assert_eq!(a["mode"], "overwrite");
            st.commit(&path("path"), r.body.clone())
        }
        "/upload/files/upload_session/start" => {
            st.next += 1;
            let id = format!("sess-{}", st.next);
            st.sessions.insert(id.clone(), r.body.clone());
            Reply::json(200, json!({"session_id": id}))
        }
        "/upload/files/upload_session/append_v2" | "/upload/files/upload_session/finish" => {
            let id = a["cursor"]["session_id"].as_str().unwrap().to_string();
            let offset = a["cursor"]["offset"].as_u64().unwrap() as usize;
            let Some(buf) = st.sessions.get_mut(&id) else {
                return conflict("lookup_failed/not_found/");
            };
            if buf.len() != offset {
                return Reply::json(
                    409,
                    json!({"error_summary": "lookup_failed/incorrect_offset/..", "error": {".tag": "lookup_failed", "lookup_failed": {".tag": "incorrect_offset", "correct_offset": buf.len()}}}),
                );
            }
            buf.extend_from_slice(&r.body);
            if r.path.ends_with("append_v2") {
                return Reply::json(200, Value::Null);
            }
            let data = st.sessions.remove(&id).unwrap();
            let p = a["commit"]["path"].as_str().unwrap().to_string();
            st.commit(&p, data)
        }
        "/api/files/create_folder_v2" => {
            let p = path("path");
            if st.nodes.contains_key(&p.to_lowercase()) {
                return conflict("path/conflict/folder/..");
            }
            if !st.writable(&p) {
                return conflict("path/no_write_permission/..");
            }
            st.ensure_dir(&p);
            Reply::json(200, json!({"metadata": meta(st, &st.nodes[&p.to_lowercase()])}))
        }
        "/api/files/delete_v2" => {
            let p = path("path");
            let Some(n) = st.nodes.get(&p.to_lowercase()).cloned() else {
                return conflict("path_lookup/not_found/..");
            };
            if !st.writable(&p) {
                return conflict("path_write/no_write_permission/..");
            }
            if let Some(rev) = a["parent_rev"].as_str()
                && (n.dir || rev != format!("{:09x}", n.rev))
            {
                return conflict("path_lookup/not_found/..");
            }
            for k in st.subtree(&p) {
                st.nodes.remove(&k);
            }
            Reply::json(200, json!({"metadata": meta(st, &n)}))
        }
        "/api/files/move_v2" | "/api/files/copy_v2" => {
            let (from, to) = (path("from_path"), path("to_path"));
            if !st.nodes.contains_key(&from.to_lowercase()) {
                return conflict("from_lookup/not_found/..");
            }
            if st.nodes.contains_key(&to.to_lowercase()) {
                return conflict("to/conflict/file/..");
            }
            let moving = r.path.ends_with("move_v2");
            for k in st.subtree(&from) {
                let n = st.nodes[&k].clone();
                let new_display = format!("{to}{}", &n.display[from.len()..]);
                if moving {
                    st.nodes.remove(&k);
                }
                st.next += 1;
                let rev = st.next;
                st.nodes.insert(
                    new_display.to_lowercase(),
                    Node {
                        display: new_display,
                        rev,
                        ..n
                    },
                );
            }
            Reply::json(
                200,
                json!({"metadata": meta(st, &st.nodes[&to.to_lowercase()])}),
            )
        }
        "/api/sharing/get_folder_metadata" => {
            let id = path("shared_folder_id");
            match st.shares.values().find(|(sid, _)| *sid == id) {
                Some((_, access)) => Reply::json(
                    200,
                    json!({"shared_folder_id": id, "access_type": {".tag": access}, "is_team_folder": false}),
                ),
                None => conflict("invalid_id/.."),
            }
        }
        "/api/sharing/relinquish_folder_membership" => {
            let id = path("shared_folder_id");
            let Some((root, (_, access))) = st.shares.iter().find(|(_, (sid, _))| *sid == id).map(|(k, v)| (k.clone(), v.clone()))
            else {
                return conflict("access_error/invalid_id/..");
            };
            if access == "owner" {
                return conflict("folder_owner/..");
            }
            // The folder leaves this account; its members keep it (not modelled further).
            for k in st.subtree(&root) {
                st.nodes.remove(&k);
            }
            st.shares.remove(&root);
            st.left.push((id, a["leave_a_copy"].as_bool().unwrap_or(true)));
            match st.leave_async_polls {
                Some(_) => Reply::json(200, json!({".tag": "async_job_id", "async_job_id": "job-1"})),
                None => Reply::json(200, json!({".tag": "complete"})),
            }
        }
        "/api/sharing/check_job_status" => match st.leave_async_polls {
            Some(n) if n > 0 => {
                st.leave_async_polls = Some(n - 1);
                Reply::json(200, json!({".tag": "in_progress"}))
            }
            _ => Reply::json(200, json!({".tag": "complete"})),
        },
        "/api/users/get_current_account" => Reply::json(200, json!({"email": "dee@example.com"})),
        other => Reply::json(
            400,
            json!({"error_summary": format!("unknown endpoint {other}")}),
        ),
    }
}
