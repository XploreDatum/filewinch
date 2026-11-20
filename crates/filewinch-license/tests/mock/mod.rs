//! A tiny HTTP/1.1 server on 127.0.0.1 standing in for Polar: each path has a queue of
//! scripted replies, and every request is recorded (path and JSON body).

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Clone)]
pub enum Reply {
    Json(u16, &'static str),
    Text(u16, &'static str),
    /// Accept, read the request, then say nothing for this long.
    Hang(Duration),
}

#[derive(Default)]
struct State {
    replies: HashMap<String, VecDeque<Reply>>,
    seen: Vec<(String, serde_json::Value)>,
}

pub struct Mock {
    pub base: String,
    state: Arc<Mutex<State>>,
}

impl Mock {
    pub fn start() -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let state = Arc::new(Mutex::new(State::default()));
        let s = state.clone();
        thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let s = s.clone();
                thread::spawn(move || serve(conn, &s));
            }
        });
        Mock { base, state }
    }

    /// Queues `reply` for `/v1/customer-portal/license-keys/<action>`.
    pub fn on(&self, action: &str, reply: Reply) -> &Mock {
        self.state.lock().unwrap().replies.entry(action.to_owned()).or_default().push_back(reply);
        self
    }

    /// Requests so far: (action, body).
    pub fn seen(&self) -> Vec<(String, serde_json::Value)> {
        self.state.lock().unwrap().seen.clone()
    }

    pub fn count(&self, action: &str) -> usize {
        self.seen().iter().filter(|(a, _)| a == action).count()
    }
}

/// A base URL nothing listens on (connection refused: "offline").
pub fn dead_base() -> String {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = l.local_addr().expect("addr");
    drop(l);
    format!("http://{addr}")
}

fn serve(conn: TcpStream, state: &Mutex<State>) {
    let mut reader = BufReader::new(conn.try_clone().expect("clone"));
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':')
            && k.eq_ignore_ascii_case("content-length")
        {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; len];
    let _ = reader.read_exact(&mut body);
    let action = path.rsplit('/').next().unwrap_or("").to_owned();
    let reply = {
        let mut s = state.lock().unwrap();
        s.seen.push((action.clone(), serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null)));
        s.replies.get_mut(&action).and_then(VecDeque::pop_front)
    };
    let (status, ctype, text) = match reply {
        Some(Reply::Json(c, b)) => (c, "application/json", b),
        Some(Reply::Text(c, b)) => (c, "text/html", b),
        Some(Reply::Hang(d)) => {
            thread::sleep(d);
            return;
        }
        None => (500, "text/plain", "no reply scripted"),
    };
    let mut conn = conn;
    let head = format!("HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", text.len());
    let _ = conn.write_all(head.as_bytes());
    let _ = conn.write_all(text.as_bytes());
}
