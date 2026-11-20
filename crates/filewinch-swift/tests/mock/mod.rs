//! An in-memory Swift + Keystone (v3, v2) + TempAuth server on 127.0.0.1 for tests.
//!
//! Accounts: user `tester`, password `testing`, project/tenant `test` (domain `Default`);
//! TempAuth user `test:tester`. The catalog has two public object-store regions,
//! `RegionOne` (`/v1/AUTH_test`) and `RegionTwo` (`/r2/v1/AUTH_test`, same store), plus an
//! internal endpoint on a dead port that must never be used.

#![allow(dead_code)]

mod shim;
mod store;

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};

use filewinch_core::{Auth, Protocol, SecretRef, Site};
use filewinch_swift::{SwiftRemote, Tuning};
use percent_encoding::percent_decode_str;
use serde_json::{Value, json};
use tiny_http::{Header, Request, Response, Server};

pub const PASS: &str = "testing";

pub struct Obj {
    pub data: Vec<u8>,
    pub content_type: String,
    pub etag: String,
    /// Segment paths (`container/object`) when this is an SLO manifest.
    pub manifest: Option<Vec<String>>,
    /// `X-Object-Manifest` (`container/prefix`) when this is a DLO manifest.
    pub dlo: Option<String>,
}

/// A canned reply: status, headers, body.
pub type Canned = (u16, Vec<(&'static str, String)>, String);

pub struct State {
    pub containers: BTreeMap<String, BTreeMap<String, Obj>>,
    pub tokens: HashSet<String>,
    pub issued: u64,
    /// Successful sign-ins.
    pub auths: u32,
    /// Auth versions answering 404: "v1", "v2", "v3".
    pub disabled: HashSet<&'static str>,
    /// Every storage request gets 401.
    pub reject_tokens: bool,
    /// Canned responses for the next storage requests (after the token check).
    pub fail: VecDeque<Canned>,
    /// "METHOD /path?query" of every storage request.
    pub log: Vec<String>,
    /// Object PUTs answer with a wrong ETag.
    pub bad_etag: bool,
    /// `expires_at` sent with Keystone tokens.
    pub expires: String,
}

pub struct Mock {
    pub addr: SocketAddr,
    pub state: Arc<Mutex<State>>,
    server: Arc<Server>,
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.server.unblock();
    }
}

impl Mock {
    pub fn start() -> Mock {
        let server = Arc::new(Server::http("127.0.0.1:0").expect("bind mock"));
        let addr = shim::start(server.server_addr().to_ip().expect("ip addr"));
        let state = Arc::new(Mutex::new(State {
            containers: BTreeMap::new(),
            tokens: HashSet::new(),
            issued: 0,
            auths: 0,
            disabled: HashSet::new(),
            reject_tokens: false,
            fail: VecDeque::new(),
            log: Vec::new(),
            bad_etag: false,
            expires: "2099-01-01T00:00:00.000000Z".into(),
        }));
        let (srv, st) = (server.clone(), state.clone());
        std::thread::spawn(move || {
            for req in srv.incoming_requests() {
                let st = st.clone();
                std::thread::spawn(move || handle(&st, req));
            }
        });
        Mock { addr, state, server }
    }

    pub fn st(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// `user` in the short form `[domain/]project:user` fills `domain`, `project` and `user`.
    pub fn site(&self, auth_path: &str, user: &str) -> Site {
        let mut site = Site::new("t", "Test Swift", Protocol::Swift);
        site.host = self.url(auth_path);
        let (scope, name) = user.split_once(':').map_or((None, user), |(s, u)| (Some(s), u));
        let (domain, project) = match scope.map(|s| s.split_once('/')) {
            Some(Some((d, p))) => (Some(d), Some(p)),
            Some(None) => (None, scope),
            None => (None, None),
        };
        site.user = name.into();
        site.project = project.map(str::to_string);
        site.domain = domain.map(str::to_string);
        site.auth = Auth::Password { secret: SecretRef("swift-test".into()) };
        site
    }

    /// Keystone v3 as `test:tester`, with `tuning`.
    pub fn connect(&self, tuning: Tuning) -> SwiftRemote {
        connect(&self.site("/v3", "test:tester"), PASS, tuning).expect("connect")
    }

    pub fn put(&self, container: &str, key: &str, data: &[u8]) {
        let mut st = self.st();
        let c = st.containers.entry(container.into()).or_default();
        c.insert(key.into(), store::plain(data.to_vec(), "application/octet-stream"));
    }

    pub fn has(&self, container: &str, key: &str) -> bool {
        self.st().containers.get(container).is_some_and(|c| c.contains_key(key))
    }

    pub fn keys(&self, container: &str) -> Vec<String> {
        self.st().containers.get(container).map(|c| c.keys().cloned().collect()).unwrap_or_default()
    }
}

pub fn connect(site: &Site, pass: &str, tuning: Tuning) -> filewinch_core::Result<SwiftRemote> {
    let pass = pass.to_string();
    filewinch_swift::connect_with(site, &move |_| Ok(Some(pass.clone())), tuning)
}

pub(crate) fn header(req: &Request, name: &str) -> Option<String> {
    req.headers().iter().find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name)).map(|h| h.value.to_string())
}

pub(crate) fn hdr(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("header")
}

pub(crate) fn decode(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

pub(crate) fn query(q: &str) -> BTreeMap<String, String> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect()
}

pub(crate) type Resp = Response<std::io::Cursor<Vec<u8>>>;

pub(crate) fn status(code: u16) -> Resp {
    Response::from_data(Vec::new()).with_status_code(code)
}

fn handle(st: &Arc<Mutex<State>>, mut req: Request) {
    let url = req.url().to_string();
    let (path, q) = url.split_once('?').unwrap_or((&url, ""));
    let (path, q) = (path.to_string(), query(q));
    let host = header(&req, "host").unwrap_or_default();
    let method = req.method().as_str().to_string();
    let resp = match (method.as_str(), path.as_str()) {
        ("POST", "/v3/auth/tokens") if !st.lock().unwrap().disabled.contains("v3") => keystone_v3(st, &mut req, &host),
        ("POST", "/v2.0/tokens") if !st.lock().unwrap().disabled.contains("v2") => keystone_v2(st, &mut req, &host),
        ("GET", "/auth/v1.0") if !st.lock().unwrap().disabled.contains("v1") => tempauth(st, &req, &host),
        _ => {
            let rest = path.strip_prefix("/r2/v1/AUTH_test").or_else(|| path.strip_prefix("/v1/AUTH_test"));
            match rest {
                Some(rest) => {
                    let rest = rest.to_string();
                    store::storage(st, &mut req, &method, &path, &rest, &q, &url)
                }
                None => status(404),
            }
        }
    };
    if let Some(resp) = resp_opt(resp) {
        let _ = req.respond(resp);
    }
}

fn resp_opt(r: Resp) -> Option<Resp> {
    // A 599 means "drop the connection without answering" (body read failed).
    if r.status_code().0 == 599 { None } else { Some(r) }
}

fn body_json(req: &mut Request) -> Value {
    let mut s = String::new();
    let _ = req.as_reader().read_to_string(&mut s);
    serde_json::from_str(&s).unwrap_or(Value::Null)
}

fn issue(st: &Arc<Mutex<State>>) -> String {
    let mut s = st.lock().unwrap();
    s.issued += 1;
    s.auths += 1;
    let t = format!("tok-{}", s.issued);
    s.tokens.insert(t.clone());
    t
}

fn unauthorized() -> Resp {
    Response::from_data(br#"{"error": {"code": 401, "title": "Unauthorized"}}"#.to_vec()).with_status_code(401)
}

fn keystone_v3(st: &Arc<Mutex<State>>, req: &mut Request, host: &str) -> Resp {
    let b = body_json(req);
    let user = &b["auth"]["identity"]["password"]["user"];
    if user["name"] != "tester" || user["password"] != PASS || user["domain"]["name"] != "Default" {
        return unauthorized();
    }
    let scope = &b["auth"]["scope"]["project"];
    let catalog = if scope.is_null() {
        json!([])
    } else if scope["name"] != "test" || scope["domain"]["name"] != "Default" {
        return unauthorized();
    } else {
        json!([
            {"type": "identity", "endpoints": [{"interface": "public", "region": "RegionOne", "url": format!("http://{host}/v3")}]},
            {"type": "object-store", "endpoints": [
                {"interface": "internal", "region": "RegionOne", "region_id": "RegionOne", "url": "http://127.0.0.1:1/v1/AUTH_test"},
                {"interface": "public", "region": "RegionOne", "region_id": "RegionOne", "url": format!("http://{host}/v1/AUTH_test")},
                {"interface": "public", "region": "RegionTwo", "region_id": "RegionTwo", "url": format!("http://{host}/r2/v1/AUTH_test/")}
            ]}
        ])
    };
    let token = issue(st);
    let expires = st.lock().unwrap().expires.clone();
    let body = json!({"token": {"expires_at": expires, "catalog": catalog}});
    Response::from_data(body.to_string().into_bytes())
        .with_status_code(201)
        .with_header(hdr("X-Subject-Token", &token))
        .with_header(hdr("Content-Type", "application/json"))
}

fn keystone_v2(st: &Arc<Mutex<State>>, req: &mut Request, host: &str) -> Resp {
    let b = body_json(req);
    let creds = &b["auth"]["passwordCredentials"];
    if creds["username"] != "tester" || creds["password"] != PASS {
        return unauthorized();
    }
    let catalog = if b["auth"]["tenantName"] == "test" {
        json!([{"type": "object-store", "name": "swift", "endpoints": [
            {"region": "RegionOne", "internalURL": "http://127.0.0.1:1/v1/AUTH_test", "publicURL": format!("http://{host}/v1/AUTH_test")},
            {"region": "RegionTwo", "publicURL": format!("http://{host}/r2/v1/AUTH_test")}
        ]}])
    } else {
        json!([])
    };
    let token = issue(st);
    let body = json!({"access": {"token": {"id": token, "expires": "2099-01-01T00:00:00Z"}, "serviceCatalog": catalog}});
    Response::from_data(body.to_string().into_bytes()).with_header(hdr("Content-Type", "application/json"))
}

fn tempauth(st: &Arc<Mutex<State>>, req: &Request, host: &str) -> Resp {
    if header(req, "x-auth-user").as_deref() != Some("test:tester") || header(req, "x-auth-key").as_deref() != Some(PASS) {
        return unauthorized();
    }
    let token = issue(st);
    Response::from_data(Vec::new())
        .with_header(hdr("X-Auth-Token", &token))
        .with_header(hdr("X-Storage-Url", &format!("http://{host}/v1/AUTH_test")))
}
