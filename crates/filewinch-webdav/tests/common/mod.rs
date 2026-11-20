//! Shared helpers for the integration tests: sites for `rclone serve webdav`, a scripted
//! HTTP/1.1 mock server, and deterministic pseudo-random data.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use filewinch_core::{Auth, Connector, Protocol, Remote, Result, SecretRef, Site};
use filewinch_testkit::Server;
use filewinch_webdav::WebDavConnector;

pub fn base() -> &'static std::path::Path {
    std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
}

pub fn site(protocol: Protocol, host: &str, port: u16, user: &str) -> Site {
    let mut s = Site::new("t", "test", protocol);
    s.host = host.into();
    s.port = Some(port);
    s.user = user.into();
    s.auth = Auth::Password { secret: SecretRef("pw".into()) };
    s
}

pub fn connect_with(site: &Site, pass: &str) -> Result<Box<dyn Remote>> {
    let pass = pass.to_string();
    WebDavConnector.connect(site, std::sync::Arc::new(filewinch_core::FnSecrets(move |_: &filewinch_core::SecretRef| Ok(Some(pass.clone())))))
}

/// Connects to an rclone server (or a proxy in front of it on `port`).
pub fn connect_rclone(srv: &Server, port: u16) -> Box<dyn Remote> {
    let s = site(Protocol::WebDav, "127.0.0.1", port, &srv.user);
    connect_with(&s, &srv.pass).expect("connect to rclone webdav")
}

/// Deterministic pseudo-random bytes (xorshift64*), streamed without holding them all.
pub struct Noise {
    state: u64,
    left: u64,
}

impl Noise {
    pub fn new(seed: u64, len: u64) -> Noise {
        Noise { state: seed | 1, left: len }
    }
}

impl Read for Noise {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = buf.len().min(usize::try_from(self.left).unwrap_or(usize::MAX));
        for b in &mut buf[..n] {
            self.state ^= self.state >> 12;
            self.state ^= self.state << 25;
            self.state ^= self.state >> 27;
            *b = (self.state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8;
        }
        self.left -= n as u64;
        Ok(n)
    }
}

pub fn noise(seed: u64, len: u64) -> Vec<u8> {
    let mut v = Vec::new();
    Noise::new(seed, len).read_to_end(&mut v).unwrap();
    v
}

/// Compares two readers byte for byte, in chunks. Returns the number of bytes compared.
pub fn assert_same(mut a: impl Read, mut b: impl Read) -> u64 {
    let (mut ba, mut bb) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    let mut total = 0u64;
    loop {
        let na = read_full(&mut a, &mut ba);
        let nb = read_full(&mut b, &mut bb);
        assert_eq!(na, nb, "lengths differ near byte {total}");
        assert!(ba[..na] == bb[..nb], "contents differ near byte {total}");
        total += na as u64;
        if na == 0 {
            return total;
        }
    }
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> usize {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]).expect("read") {
            0 => break,
            k => n += k,
        }
    }
    n
}

// ---- Mock server -----------------------------------------------------------------------

pub struct Request {
    pub method: String,
    pub path: String,
    /// Lowercased names.
    pub headers: HashMap<String, String>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

/// What the handler does with a request.
pub enum Reply {
    /// Send this full response and keep the connection open.
    Send(Vec<u8>),
    /// Send these bytes and close the connection.
    SendAndClose(Vec<u8>),
}

pub fn resp(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    let mut out = out.into_bytes();
    out.extend_from_slice(body);
    out
}

/// Starts a mock server on 127.0.0.1. `handler(req, conn)` may read the body from `conn`
/// (with [`read_body`]) or not (to answer early, as a server rejecting an upload would).
pub fn mock<F>(handler: F) -> u16
where
    F: Fn(&Request, &mut BufReader<TcpStream>) -> Reply + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handler = Arc::new(handler);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let handler = handler.clone();
            std::thread::spawn(move || {
                let mut w = stream.try_clone().unwrap();
                let mut r = BufReader::new(stream);
                while let Ok(req) = read_request(&mut r) {
                    match handler(&req, &mut r) {
                        Reply::Send(bytes) => {
                            if w.write_all(&bytes).is_err() {
                                return;
                            }
                        }
                        Reply::SendAndClose(bytes) => {
                            let _ = w.write_all(&bytes);
                            let _ = w.shutdown(std::net::Shutdown::Both);
                            return;
                        }
                    }
                }
            });
        }
    });
    port
}

fn read_request(r: &mut impl BufRead) -> io::Result<Request> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    Ok(Request { method, path, headers })
}

/// Reads a Content-Length or chunked body.
pub fn read_body(req: &Request, r: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    if req.header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        loop {
            let mut size = String::new();
            r.read_line(&mut size)?;
            let size = usize::from_str_radix(size.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
            if size == 0 {
                let mut end = String::new();
                r.read_line(&mut end)?;
                return Ok(body);
            }
            let start = body.len();
            body.resize(start + size, 0);
            r.read_exact(&mut body[start..])?;
            let mut crlf = [0u8; 2];
            r.read_exact(&mut crlf)?;
        }
    } else if let Some(len) = req.header("content-length").and_then(|v| v.parse::<usize>().ok()) {
        body.resize(len, 0);
        r.read_exact(&mut body)?;
    }
    Ok(body)
}

/// A minimal multistatus for `href`s: (href, is_dir, size).
pub fn multistatus(items: &[(&str, bool, u64)]) -> Vec<u8> {
    let mut x = String::from(r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:">"#);
    for (href, dir, size) in items {
        let rt = if *dir { "<D:collection/>" } else { "" };
        let len = if *dir { String::new() } else { format!("<D:getcontentlength>{size}</D:getcontentlength>") };
        x.push_str(&format!(
            "<D:response><D:href>{href}</D:href><D:propstat><D:prop><D:resourcetype>{rt}</D:resourcetype>{len}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"
        ));
    }
    x.push_str("</D:multistatus>");
    x.into_bytes()
}

/// Reply for a PROPFIND (the caller has read the request body).
pub fn propfind_ok(items: &[(&str, bool, u64)]) -> Reply {
    Reply::Send(resp("207 Multi-Status", &[("Content-Type", "application/xml")], &multistatus(items)))
}
