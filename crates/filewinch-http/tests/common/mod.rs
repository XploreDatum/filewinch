//! A tiny HTTP/1.1 server for tests: each accepted connection runs `handler` on its own
//! thread. Helpers parse one request head and read a Content-Length or chunked body.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

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
    pub fn is_chunked(&self) -> bool {
        self.header("transfer-encoding")
            .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
    }
    pub fn content_length(&self) -> Option<u64> {
        self.header("content-length").and_then(|v| v.parse().ok())
    }
}

/// Starts a server on 127.0.0.1 and returns its port. The listener lives for the test
/// process (threads are detached).
pub fn serve<F>(handler: F) -> u16
where
    F: Fn(TcpStream) + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handler = Arc::new(handler);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let handler = handler.clone();
            std::thread::spawn(move || handler(stream));
        }
    });
    port
}

pub fn read_request<R: BufRead>(r: &mut R) -> io::Result<Request> {
    let mut line = String::new();
    r.read_line(&mut line)?;
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
    Ok(Request {
        method,
        path,
        headers,
    })
}

/// Reads the whole body; errors (UnexpectedEof) if the client stops early.
pub fn read_body<R: BufRead>(r: &mut R, req: &Request) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    if req.is_chunked() {
        loop {
            let mut size = String::new();
            if r.read_line(&mut size)? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let size = usize::from_str_radix(size.trim().split(';').next().unwrap(), 16)
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
            if size == 0 {
                let mut end = String::new();
                r.read_line(&mut end)?;
                if end.trim_end().is_empty() && !end.is_empty() {
                    return Ok(body);
                }
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let start = body.len();
            body.resize(start + size, 0);
            r.read_exact(&mut body[start..])?;
            let mut crlf = [0u8; 2];
            r.read_exact(&mut crlf)?;
        }
    } else if let Some(len) = req.content_length() {
        body.resize(len as usize, 0);
        r.read_exact(&mut body)?;
    }
    Ok(body)
}

pub fn respond(stream: &mut impl Write, status: &str, extra_headers: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

/// Deterministic test data.
pub fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

pub fn reader(stream: &TcpStream) -> BufReader<TcpStream> {
    BufReader::new(stream.try_clone().unwrap())
}
