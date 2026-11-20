//! Test-only HTTP/1.1 mock server on 127.0.0.1: hands each request to a closure and records
//! what it saw. Hand-rolled rather than tiny_http because tiny_http answers
//! `Expect: 100-continue` with a `100` carrying headers, which ureq rejects.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug)]
pub(crate) struct Seen {
    pub method: String,
    /// Path and query, as sent.
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    pub fn path(&self) -> &str {
        self.url.split('?').next().unwrap_or("")
    }
    pub fn query(&self, name: &str) -> Option<String> {
        let q = self.url.split_once('?')?.1;
        q.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (k == name).then(|| percent_encoding::percent_decode_str(v).decode_utf8_lossy().into_owned())
        })
    }
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

pub(crate) struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Reply {
        Reply { status, headers: Vec::new(), body: body.into() }
    }
    pub fn header(mut self, k: &str, v: &str) -> Reply {
        self.headers.push((k.into(), v.into()));
        self
    }
    pub fn json(status: u16, v: serde_json::Value) -> Reply {
        Reply::new(status, v.to_string()).header("Content-Type", "application/json")
    }
}

type Handler = dyn Fn(&Seen) -> Reply + Send + Sync;

pub(crate) struct Mock {
    pub port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
    stop: Arc<AtomicBool>,
}

impl Mock {
    pub fn start(handler: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let port = listener.local_addr().expect("addr").port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handler: Arc<Handler> = Arc::new(handler);
        let (s2, st2) = (seen.clone(), stop.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if st2.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(conn) = conn else { continue };
                let (h, s) = (handler.clone(), s2.clone());
                std::thread::spawn(move || serve(conn, &*h, &s));
            }
        });
        Mock { port, seen, stop }
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

fn serve(conn: TcpStream, handler: &Handler, seen: &Mutex<Vec<Seen>>) {
    let _ = conn.set_read_timeout(Some(Duration::from_secs(10)));
    let mut out = conn.try_clone().expect("clone");
    let mut r = BufReader::new(conn);
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = line.split_whitespace();
        let (Some(method), Some(url)) = (parts.next(), parts.next()) else { return };
        let (method, url) = (method.to_string(), url.to_string());
        let mut headers = Vec::new();
        loop {
            let mut h = String::new();
            if r.read_line(&mut h).unwrap_or(0) == 0 {
                return;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        let get = |n: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.clone());
        if get("expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue"))
            && out.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").is_err()
        {
            return;
        }
        let mut body = Vec::new();
        if let Some(n) = get("content-length").and_then(|v| v.parse::<usize>().ok()) {
            body.resize(n, 0);
            if r.read_exact(&mut body).is_err() {
                return; // client aborted mid-body
            }
        } else if get("transfer-encoding").is_some_and(|v| v.contains("chunked")) {
            loop {
                let mut size = String::new();
                if r.read_line(&mut size).unwrap_or(0) == 0 {
                    return;
                }
                let Ok(n) = usize::from_str_radix(size.trim(), 16) else { return };
                let mut chunk = vec![0; n + 2];
                if r.read_exact(&mut chunk).is_err() {
                    return;
                }
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..n]);
            }
        }
        let s = Seen { method, url, headers, body };
        let reply = handler(&s);
        let head_only = s.method == "HEAD";
        seen.lock().unwrap().push(s);
        let mut resp = format!("HTTP/1.1 {} X\r\n", reply.status);
        let has_len = reply.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-length"));
        for (k, v) in &reply.headers {
            resp.push_str(&format!("{k}: {v}\r\n"));
        }
        if !has_len {
            resp.push_str(&format!("Content-Length: {}\r\n", reply.body.len()));
        }
        resp.push_str("\r\n");
        let mut bytes = resp.into_bytes();
        if !head_only {
            bytes.extend_from_slice(&reply.body);
        }
        if out.write_all(&bytes).is_err() {
            let _ = out.shutdown(Shutdown::Both);
            return;
        }
    }
}
