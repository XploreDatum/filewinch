//! A stateful fake of the Microsoft Graph v1.0 drive endpoints Filewinch uses, shaped on the
//! documented `driveItem` JSON: item GET/PATCH/DELETE, `/children` with `$top` and
//! `@odata.nextLink`, `/content` (302 to a pre-authenticated URL, Range), path-addressed
//! uploads, upload sessions, async copy with a monitor URL, and `sharedWithMe`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::support::{Mock, Recv, Reply, Tokens};

pub const MY_DRIVE: &str = "drive-me";
pub const OTHER_DRIVE: &str = "drive-other";
const MTIME: &str = "2024-03-05T10:20:30Z";

#[derive(Clone, Debug)]
pub struct Item {
    pub id: String,
    pub drive: String,
    pub name: String,
    pub parent: Option<String>,
    pub folder: bool,
    pub data: Vec<u8>,
    pub mtime: String,
    pub version: u32,
    /// A shortcut ("Add shortcut to My files"): the target item's ID, usually in another drive.
    pub remote: Option<String>,
    /// `file.hashes` as Graph would send it (`{}` when None).
    pub hashes: Option<Value>,
}

pub struct UploadSession {
    pub drive: String,
    pub parent: String,
    pub name: String,
    pub total: u64,
    pub data: Vec<u8>,
}

#[derive(Default)]
pub struct State {
    pub items: HashMap<String, Item>,
    pub next_id: usize,
    pub shared: Vec<String>,
    /// Item IDs that carry Graph's `shared` facet (shared with others by their owner).
    pub shared_out: Vec<String>,
    pub readonly: Vec<String>,
    pub sessions: HashMap<String, UploadSession>,
    pub deleted_sessions: Vec<String>,
    /// monitor id -> (polls so far, result: Ok(new item id) or Err(message))
    pub monitors: HashMap<String, (u32, std::result::Result<String, String>)>,
}

pub struct Graph {
    pub mock: Mock,
    pub tokens: Arc<Tokens>,
    pub state: Arc<Mutex<State>>,
    /// Next N API calls answer 429 with Retry-After: 7.
    pub throttle: Arc<AtomicUsize>,
    /// The first upload fragment is only half acknowledged.
    pub short_ack: Arc<AtomicBool>,
    /// The next copy fails.
    pub copy_fails: Arc<AtomicBool>,
    /// `/content` answers 200 with the whole file even when a Range was asked for.
    pub ignore_range: Arc<AtomicBool>,
}

impl State {
    pub fn add(
        &mut self,
        drive: &str,
        parent: Option<&str>,
        name: &str,
        folder: bool,
        data: &[u8],
    ) -> String {
        self.next_id += 1;
        let id = format!("item-{}", self.next_id);
        self.items.insert(
            id.clone(),
            Item {
                id: id.clone(),
                drive: drive.into(),
                name: name.into(),
                parent: parent.map(str::to_string),
                folder,
                data: data.to_vec(),
                mtime: MTIME.into(),
                version: 1,
                remote: None,
                hashes: None,
            },
        );
        id
    }

    /// Adds a shortcut named `name` under `parent` (in My files) pointing at `target`.
    #[allow(dead_code)] // used by onedrive_shortcuts.rs only
    pub fn add_shortcut(&mut self, parent: &str, name: &str, target: &str) -> String {
        let id = self.add(MY_DRIVE, Some(parent), name, false, &[]);
        self.items.get_mut(&id).unwrap().remote = Some(target.to_string());
        id
    }

    pub fn children(&self, parent: &str) -> Vec<Item> {
        let mut v: Vec<Item> = self
            .items
            .values()
            .filter(|i| i.parent.as_deref() == Some(parent))
            .cloned()
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub fn find(&self, parent: &str, name: &str) -> Option<Item> {
        self.children(parent)
            .into_iter()
            .find(|i| i.name.eq_ignore_ascii_case(name))
    }

    /// Finds an item by path from the My files root, e.g. "Docs/a.txt".
    pub fn at(&self, path: &str) -> Option<Item> {
        let mut cur = self.items.get("root")?.clone();
        for c in path.split('/').filter(|c| !c.is_empty()) {
            cur = self.find(&cur.id, c)?;
        }
        Some(cur)
    }

    fn json(&self, i: &Item, base: &str) -> Value {
        let mut v = json!({
            "id": i.id, "name": i.name,
            "eTag": format!("\"{{{}}},{}\"", i.id, i.version),
            "cTag": format!("\"c:{{{}}},{}\"", i.id, i.version),
            "createdDateTime": MTIME, "lastModifiedDateTime": i.mtime,
            "fileSystemInfo": {"createdDateTime": MTIME, "lastModifiedDateTime": i.mtime},
            "parentReference": {"driveId": i.drive, "driveType": "personal", "id": i.parent},
            "webUrl": format!("https://onedrive.example/{}", i.id),
        });
        if self.shared_out.contains(&i.id) {
            v["shared"] = json!({"scope": "users", "owner": {"user": {"id": format!("owner-of-{}", i.drive)}}});
        }
        if let Some(target) = i.remote.as_ref().and_then(|t| self.items.get(t)) {
            // A shortcut has no folder/file facet of its own, only `remoteItem`.
            v["remoteItem"] = self.json(target, base);
            return v;
        }
        if i.folder {
            v["folder"] = json!({"childCount": self.children(&i.id).len()});
            v["size"] = json!(0);
        } else {
            let hashes = i.hashes.clone().unwrap_or_else(|| json!({}));
            v["file"] = json!({"mimeType": "application/octet-stream", "hashes": hashes});
            v["size"] = json!(i.data.len());
            v["@microsoft.graph.downloadUrl"] = json!(format!("{base}/dl/{}", i.id));
        }
        if i.id == "root" {
            v["root"] = json!({});
            v["parentReference"] = json!({"driveId": i.drive, "driveType": "personal"});
        }
        v
    }

    fn ancestors_readonly(&self, id: &str) -> bool {
        let mut cur = Some(id.to_string());
        while let Some(c) = cur {
            if self.readonly.contains(&c) {
                return true;
            }
            cur = self.items.get(&c).and_then(|i| i.parent.clone());
        }
        false
    }

    fn remove_tree(&mut self, id: &str) {
        for c in self.children(id) {
            self.remove_tree(&c.id);
        }
        self.items.remove(id);
    }

    fn copy_tree(&mut self, id: &str, parent: &str, name: &str) -> String {
        let src = self.items[id].clone();
        let new = self.add(&src.drive, Some(parent), name, src.folder, &src.data);
        for c in self.children(id) {
            self.copy_tree(&c.id, &new, &c.name);
        }
        new
    }
}

fn err(status: u16, code: &str, message: &str) -> Reply {
    Reply::json(
        status,
        json!({"error": {"code": code, "message": message,
        "innerError": {"date": MTIME, "request-id": "req-1"}}}),
    )
}

impl Graph {
    pub fn start(rotate: bool) -> Graph {
        let tokens = Tokens::new(rotate);
        let state = Arc::new(Mutex::new(State::default()));
        {
            let mut st = state.lock().unwrap();
            st.items.insert(
                "root".into(),
                Item {
                    id: "root".into(),
                    drive: MY_DRIVE.into(),
                    name: "root".into(),
                    parent: None,
                    folder: true,
                    data: vec![],
                    mtime: MTIME.into(),
                    version: 1,
                    remote: None,
                    hashes: None,
                },
            );
            st.items.insert(
                "other-root".into(),
                Item {
                    id: "other-root".into(),
                    drive: OTHER_DRIVE.into(),
                    name: "root".into(),
                    parent: None,
                    folder: true,
                    data: vec![],
                    mtime: MTIME.into(),
                    version: 1,
                    remote: None,
                    hashes: None,
                },
            );
        }
        let throttle = Arc::new(AtomicUsize::new(0));
        let short_ack = Arc::new(AtomicBool::new(false));
        let copy_fails = Arc::new(AtomicBool::new(false));
        let ignore_range = Arc::new(AtomicBool::new(false));
        let base = Arc::new(Mutex::new(String::new()));
        let ctx = Ctx {
            tokens: tokens.clone(),
            state: state.clone(),
            throttle: throttle.clone(),
            short_ack: short_ack.clone(),
            copy_fails: copy_fails.clone(),
            ignore_range: ignore_range.clone(),
            base: base.clone(),
        };
        let mock = Mock::start(move |r| ctx.handle(r));
        *base.lock().unwrap() = mock.base.clone();
        Graph {
            mock,
            tokens,
            state,
            throttle,
            short_ack,
            copy_fails,
            ignore_range,
        }
    }
}

struct Ctx {
    tokens: Arc<Tokens>,
    state: Arc<Mutex<State>>,
    throttle: Arc<AtomicUsize>,
    short_ack: Arc<AtomicBool>,
    copy_fails: Arc<AtomicBool>,
    ignore_range: Arc<AtomicBool>,
    base: Arc<Mutex<String>>,
}

impl Ctx {
    fn handle(&self, r: &Recv) -> Reply {
        let base = self.base.lock().unwrap().clone();
        let p = r.path.as_str();
        if p == "/oauth/token" {
            return self.tokens.handle(r);
        }
        // Pre-authenticated URLs: Graph rejects a bearer token on them.
        if let Some(rest) = p.strip_prefix("/dl/") {
            return self.download(r, rest);
        }
        if let Some(sid) = p.strip_prefix("/upsession/") {
            return self.session_put(r, sid);
        }
        if let Some(mid) = p.strip_prefix("/monitor/") {
            return self.monitor(r, mid, &base);
        }
        let Some(api) = p.strip_prefix("/api") else {
            return err(404, "notFound", "no route");
        };
        if let Some(reply) = self.tokens.check(r) {
            return reply;
        }
        if self.throttle.load(Ordering::SeqCst) > 0 {
            self.throttle.fetch_sub(1, Ordering::SeqCst);
            return err(429, "activityLimitReached", "Too many requests")
                .header("Retry-After", "7");
        }
        let mut st = self.state.lock().unwrap();
        match (r.method.as_str(), api) {
            ("GET", "/me/drive/root") => {
                let root = st.items["root"].clone();
                return Reply::json(200, st.json(&root, &base));
            }
            ("GET", "/me/drive/sharedWithMe") => {
                let value: Vec<Value> = st
                    .shared
                    .iter()
                    .map(|id| {
                        let i = &st.items[id];
                        json!({"id": format!("link-{}", i.id), "name": i.name,
                               "remoteItem": st.json(i, &base)})
                    })
                    .collect();
                return Reply::json(200, json!({"value": value}));
            }
            _ => {}
        }
        // /me/drive/items/{rest} or /drives/{d}/items/{rest}
        let rest = api.strip_prefix("/me/drive/items/").or_else(|| {
            api.strip_prefix("/drives/")
                .and_then(|x| x.split_once("/items/").map(|(_, r)| r))
        });
        let Some(rest) = rest else {
            return err(400, "invalidRequest", "bad path");
        };
        let (id, tail) = match rest.find(['/', ':']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let Some(item) = st.items.get(id).cloned() else {
            return err(404, "itemNotFound", "The resource could not be found.");
        };
        let write = r.method != "GET";
        if write && st.ancestors_readonly(id) && tail != "/copy" {
            return err(403, "accessDenied", "Access denied");
        }
        match (r.method.as_str(), tail) {
            ("GET", "") => Reply::json(200, st.json(&item, &base)),
            ("GET", "/children") => {
                let all = st.children(id);
                let top: usize = r
                    .query
                    .get("$top")
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(200);
                let skip: usize = r
                    .query
                    .get("$skiptoken")
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(0);
                let page: Vec<Value> = all
                    .iter()
                    .skip(skip)
                    .take(top)
                    .map(|i| st.json(i, &base))
                    .collect();
                let mut v = json!({"value": page});
                if skip + top < all.len() {
                    v["@odata.nextLink"] = json!(format!(
                        "{base}/api/drives/{}/items/{id}/children?$top={top}&$skiptoken={}",
                        item.drive,
                        skip + top
                    ));
                }
                Reply::json(200, v)
            }
            ("GET", "/content") => {
                if item.folder {
                    return err(400, "invalidRequest", "folder has no content");
                }
                Reply::new(302).header("Location", format!("{base}/dl/{id}"))
            }
            ("POST", "/children") => {
                let b = r.json();
                let name = b["name"].as_str().unwrap_or_default().to_string();
                if st.find(id, &name).is_some() {
                    return err(409, "nameAlreadyExists", "Name already exists");
                }
                let new = st.add(&item.drive, Some(id), &name, true, &[]);
                let v = st.json(&st.items[&new].clone(), &base);
                Reply::json(201, v)
            }
            ("PATCH", "") => {
                let b = r.json();
                let mut it = item.clone();
                if let Some(t) = b
                    .pointer("/fileSystemInfo/lastModifiedDateTime")
                    .and_then(Value::as_str)
                {
                    it.mtime = t.into();
                }
                let parent = b
                    .pointer("/parentReference/id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let name = b["name"].as_str().map(str::to_string);
                if parent.is_some() || name.is_some() {
                    let np = parent.unwrap_or_else(|| it.parent.clone().unwrap_or_default());
                    let nn = name.unwrap_or_else(|| it.name.clone());
                    if st.find(&np, &nn).is_some_and(|x| x.id != it.id) {
                        return err(409, "nameAlreadyExists", "Name already exists");
                    }
                    it.parent = Some(np);
                    it.name = nn;
                }
                it.version += 1;
                st.items.insert(id.into(), it.clone());
                Reply::json(200, st.json(&it, &base))
            }
            ("DELETE", "") => {
                st.remove_tree(id);
                Reply::new(204)
            }
            ("POST", "/copy") => {
                let b = r.json();
                let parent = b
                    .pointer("/parentReference/id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let name = b["name"].as_str().unwrap_or(&item.name).to_string();
                let mid = format!("m{}", st.monitors.len() + 1);
                let result = if self.copy_fails.swap(false, Ordering::SeqCst) {
                    Err("The copy failed: the destination is locked.".to_string())
                } else if st.find(&parent, &name).is_some() {
                    Err("Name already exists".to_string())
                } else {
                    Ok(st.copy_tree(id, &parent, &name))
                };
                st.monitors.insert(mid.clone(), (0, result));
                Reply::new(202).header("Location", format!("{base}/monitor/{mid}"))
            }
            ("PUT", t) if t.starts_with(":/") && t.ends_with(":/content") => {
                let name = &t[2..t.len() - ":/content".len()];
                let new = match st.find(id, name) {
                    Some(existing) => {
                        let e = st.items.get_mut(&existing.id).unwrap();
                        e.data = r.body.clone();
                        e.version += 1;
                        existing.id
                    }
                    None => st.add(&item.drive, Some(id), name, false, &r.body),
                };
                let v = st.json(&st.items[&new].clone(), &base);
                Reply::json(201, v)
            }
            ("POST", t) if t.starts_with(":/") && t.ends_with(":/createUploadSession") => {
                let name = t[2..t.len() - ":/createUploadSession".len()].to_string();
                let sid = format!("s{}", st.sessions.len() + st.deleted_sessions.len() + 1);
                st.sessions.insert(
                    sid.clone(),
                    UploadSession {
                        drive: item.drive.clone(),
                        parent: id.into(),
                        name,
                        total: 0,
                        data: vec![],
                    },
                );
                Reply::json(
                    200,
                    json!({"uploadUrl": format!("{base}/upsession/{sid}"),
                    "expirationDateTime": "2030-01-01T00:00:00Z", "nextExpectedRanges": ["0-"]}),
                )
            }
            _ => err(400, "invalidRequest", "unsupported by mock"),
        }
    }

    fn download(&self, r: &Recv, id: &str) -> Reply {
        if r.header("Authorization").is_some() {
            return err(
                400,
                "invalidRequest",
                "pre-authenticated URL must not carry a token",
            );
        }
        let st = self.state.lock().unwrap();
        let Some(item) = st.items.get(id) else {
            return err(404, "itemNotFound", "gone");
        };
        let start = r
            .header("Range")
            .filter(|_| !self.ignore_range.load(Ordering::SeqCst))
            .and_then(|v| v.strip_prefix("bytes="))
            .and_then(|v| v.trim_end_matches('-').parse::<usize>().ok());
        match start {
            Some(s) => Reply::new(206)
                .header(
                    "Content-Range",
                    format!(
                        "bytes {s}-{}/{}",
                        item.data.len().saturating_sub(1),
                        item.data.len()
                    ),
                )
                .body(item.data[s.min(item.data.len())..].to_vec()),
            None => Reply::new(200).body(item.data.clone()),
        }
    }

    fn session_put(&self, r: &Recv, sid: &str) -> Reply {
        if r.header("Authorization").is_some() {
            return err(401, "unauthenticated", "upload URLs must not carry a token");
        }
        let mut st = self.state.lock().unwrap();
        if r.method == "DELETE" {
            st.sessions.remove(sid);
            st.deleted_sessions.push(sid.into());
            return Reply::new(204);
        }
        let Some(range) = r.header("Content-Range").map(str::to_string) else {
            return err(400, "invalidRequest", "Content-Range required");
        };
        // bytes a-b/total
        let parsed = range.strip_prefix("bytes ").and_then(|x| {
            let (ab, total) = x.split_once('/')?;
            let (a, b) = ab.split_once('-')?;
            Some((
                a.parse::<u64>().ok()?,
                b.parse::<u64>().ok()?,
                total.parse::<u64>().ok()?,
            ))
        });
        let Some((a, b, total)) = parsed else {
            return err(400, "invalidRequest", "bad Content-Range");
        };
        let short = self.short_ack.swap(false, Ordering::SeqCst);
        let Some(s) = st.sessions.get_mut(sid) else {
            return err(404, "itemNotFound", "session expired");
        };
        if a != s.data.len() as u64 || b - a + 1 != r.body.len() as u64 {
            return err(416, "invalidRange", "fragment out of order");
        }
        let unit = 320 * 1024;
        if a.is_multiple_of(unit as u64) && b + 1 < total && !r.body.len().is_multiple_of(unit) {
            return err(400, "invalidRequest", "fragment not a multiple of 320 KiB");
        }
        s.total = total;
        let take = if short {
            r.body.len() / 2
        } else {
            r.body.len()
        };
        s.data.extend_from_slice(&r.body[..take]);
        if (s.data.len() as u64) < total {
            return Reply::json(
                202,
                json!({"expirationDateTime": "2030-01-01T00:00:00Z",
                "nextExpectedRanges": [format!("{}-", s.data.len())]}),
            );
        }
        let s = st.sessions.remove(sid).unwrap();
        let id = match st.find(&s.parent, &s.name) {
            Some(existing) => {
                let e = st.items.get_mut(&existing.id).unwrap();
                e.data = s.data;
                existing.id
            }
            None => st.add(&s.drive, Some(&s.parent), &s.name, false, &s.data),
        };
        let base = self.base.lock().unwrap().clone();
        let v = st.json(&st.items[&id].clone(), &base);
        Reply::json(201, v)
    }

    fn monitor(&self, r: &Recv, mid: &str, base: &str) -> Reply {
        if r.header("Authorization").is_some() {
            return err(400, "invalidRequest", "monitor URLs must not carry a token");
        }
        let mut st = self.state.lock().unwrap();
        let Some((polls, result)) = st.monitors.get_mut(mid) else {
            return err(404, "itemNotFound", "no monitor");
        };
        *polls += 1;
        if *polls == 1 {
            return Reply::json(
                202,
                json!({"operation": "itemCopy", "status": "inProgress", "percentageComplete": 50.0}),
            );
        }
        match result.clone() {
            Ok(id) if *polls == 2 => {
                Reply::new(303).header("Location", format!("{base}/api/drives/x/items/{id}"))
            }
            Ok(id) => Reply::json(
                200,
                json!({"status": "completed", "resourceId": id, "percentageComplete": 100.0}),
            ),
            Err(m) => Reply::json(
                200,
                json!({"status": "failed", "error": {"code": "generalException", "message": m}}),
            ),
        }
    }
}
