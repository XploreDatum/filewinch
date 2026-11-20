//! Mock-provider plumbing shared by the integration tests: a tiny_http server running a
//! handler closure, a request log, and a fake OAuth token endpoint that can expire and rotate
//! tokens.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use filewinch_cloud::{ClientConfig, ConnectorOptions, Endpoints, Tuning};
use filewinch_core::{Auth, FnSecrets, Protocol, Result, SecretRef, Secrets, Site};

/// A request as the mock saw it.
#[derive(Clone, Debug)]
pub struct Recv {
    pub method: String,
    /// Path without the query, percent-decoded.
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Recv {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
    pub fn form(&self) -> HashMap<String, String> {
        parse_pairs(&String::from_utf8_lossy(&self.body))
    }
    pub fn bearer(&self) -> Option<&str> {
        self.header("Authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
    }
}

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn new(status: u16) -> Self {
        Reply {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }
    pub fn json(status: u16, v: serde_json::Value) -> Self {
        Reply::new(status)
            .header("Content-Type", "application/json")
            .body(v.to_string())
    }
    pub fn header(mut self, k: &str, v: impl Into<String>) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
    pub fn body(mut self, b: impl Into<Vec<u8>>) -> Self {
        self.body = b.into();
        self
    }
}

pub type Handler = dyn Fn(&Recv) -> Reply + Send + Sync;

pub struct Mock {
    pub base: String,
    pub log: Arc<Mutex<Vec<Recv>>>,
    server: Arc<tiny_http::Server>,
    thread: Option<JoinHandle<()>>,
}

impl Mock {
    pub fn start(handler: impl Fn(&Recv) -> Reply + Send + Sync + 'static) -> Mock {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("bind mock"));
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let log = Arc::new(Mutex::new(Vec::new()));
        let handler: Arc<Handler> = Arc::new(handler);
        let (s, l) = (server.clone(), log.clone());
        let thread = std::thread::spawn(move || {
            for mut req in s.incoming_requests() {
                let recv = to_recv(&mut req);
                l.lock().unwrap().push(recv.clone());
                let h = handler.clone();
                // One thread per request so a slow handler can't stall parallel calls.
                std::thread::spawn(move || {
                    let reply = h(&recv);
                    let mut resp = tiny_http::Response::from_data(reply.body)
                        .with_status_code(tiny_http::StatusCode(reply.status));
                    for (k, v) in reply.headers {
                        resp.add_header(
                            tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap(),
                        );
                    }
                    let _ = req.respond(resp);
                });
            }
        });
        Mock {
            base,
            log,
            server,
            thread: Some(thread),
        }
    }

    pub fn requests(&self) -> Vec<Recv> {
        self.log.lock().unwrap().clone()
    }

    /// Requests whose method and path match (path compared exactly).
    pub fn count(&self, method: &str, path: &str) -> usize {
        self.requests()
            .iter()
            .filter(|r| r.method == method && r.path == path)
            .count()
    }

    pub fn endpoints(&self) -> Endpoints {
        Endpoints::local(&self.base)
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn to_recv(req: &mut tiny_http::Request) -> Recv {
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let mut body = Vec::new();
    let _ = req.as_reader().read_to_end(&mut body);
    Recv {
        method: req.method().as_str().to_string(),
        path: decode(path),
        query: parse_pairs(query),
        headers: req
            .headers()
            .iter()
            .map(|h| (h.field.to_string(), h.value.to_string()))
            .collect(),
        body,
    }
}

pub fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

pub fn parse_pairs(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(&k.replace('+', " ")), decode(&v.replace('+', " ")))
        })
        .collect()
}

/// A fake OAuth token endpoint. Refresh tokens start at `refresh-0`; with `rotate` on, every
/// refresh retires the old one and issues `refresh-N`. Access tokens are `access-N`.
pub struct Tokens {
    pub rotate: bool,
    pub refreshes: AtomicUsize,
    state: Mutex<TokenState>,
}

struct TokenState {
    valid_access: Vec<String>,
    valid_refresh: String,
}

impl Tokens {
    pub fn new(rotate: bool) -> Arc<Tokens> {
        Arc::new(Tokens {
            rotate,
            refreshes: AtomicUsize::new(0),
            state: Mutex::new(TokenState {
                valid_access: Vec::new(),
                valid_refresh: "refresh-0".into(),
            }),
        })
    }

    /// Handles `POST /oauth/token` (refresh grant only).
    pub fn handle(&self, r: &Recv) -> Reply {
        let f = r.form();
        let mut st = self.state.lock().unwrap();
        if f.get("grant_type").map(String::as_str) != Some("refresh_token")
            || f.get("refresh_token") != Some(&st.valid_refresh)
            || f.get("client_id").map(String::as_str) != Some("test-client")
        {
            return Reply::json(
                400,
                serde_json::json!({"error": "invalid_grant", "error_description": "bad refresh token"}),
            );
        }
        let n = self.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
        let access = format!("access-{n}");
        st.valid_access.push(access.clone());
        let mut v =
            serde_json::json!({"access_token": access, "token_type": "Bearer", "expires_in": 3600});
        if self.rotate {
            st.valid_refresh = format!("refresh-{n}");
            v["refresh_token"] = st.valid_refresh.clone().into();
        }
        Reply::json(200, v)
    }

    /// `None` if the bearer token is valid, else the provider-agnostic 401 reply.
    pub fn check(&self, r: &Recv) -> Option<Reply> {
        let ok = r.bearer().is_some_and(|b| {
            self.state
                .lock()
                .unwrap()
                .valid_access
                .iter()
                .any(|a| a == b)
        });
        if ok {
            None
        } else {
            Some(Reply::json(
                401,
                serde_json::json!({"error": {"code": 401, "message": "Invalid Credentials"}}),
            ))
        }
    }

    /// Makes every issued access token invalid (as if they expired server-side).
    pub fn expire_all(&self) {
        self.state.lock().unwrap().valid_access.clear();
    }

    pub fn current_refresh(&self) -> String {
        self.state.lock().unwrap().valid_refresh.clone()
    }
}

pub fn site(protocol: Protocol) -> Site {
    let mut s = Site::new("site-1", "Test drive", protocol);
    s.auth = Auth::OAuth {
        refresh_token: SecretRef("oauth.site-1".into()),
    };
    s
}

/// Secret reader that hands out `refresh-0`.
pub fn secret(r: &SecretRef) -> Result<Option<String>> {
    assert_eq!(r.0, "oauth.site-1");
    Ok(Some("refresh-0".into()))
}

/// [`secret`] as the connector's read-only `Secrets`.
#[allow(dead_code)]
pub fn secrets() -> Arc<dyn Secrets> {
    Arc::new(FnSecrets(secret))
}

/// Any fixed value as the connector's read-only `Secrets`.
#[allow(dead_code)]
pub fn fixed(value: Option<&str>) -> Arc<dyn Secrets> {
    let value = value.map(str::to_string);
    Arc::new(FnSecrets(move |_: &SecretRef| Ok(value.clone())))
}

/// A Keychain stand-in that starts with [`secret`]'s token and records every rotated one.
#[derive(Default)]
#[allow(dead_code)]
pub struct Recording(Mutex<Vec<(String, String)>>);

#[allow(dead_code)]
impl Recording {
    pub fn saved(&self) -> Vec<(String, String)> {
        self.0.lock().unwrap().clone()
    }
}

impl Secrets for Recording {
    fn get(&self, r: &SecretRef) -> Result<Option<String>> {
        let last = self.0.lock().unwrap().last().map(|(_, t)| t.clone());
        Ok(last.or(secret(r)?))
    }
    fn set(&self, r: &SecretRef, value: &str) -> Result<()> {
        self.0.lock().unwrap().push((r.0.clone(), value.into()));
        Ok(())
    }
}

/// Options pointing at the mock, with a test client and small tuning.
pub fn options(mock: &Mock, tuning: Tuning) -> ConnectorOptions {
    ConnectorOptions {
        client: Some(ClientConfig::new("test-client", Some("test-secret".into()))),
        endpoints: Some(mock.endpoints()),
        tuning: Some(tuning),
    }
}

/// Deterministic pseudo-random bytes for upload/download tests.
pub fn bytes(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}
