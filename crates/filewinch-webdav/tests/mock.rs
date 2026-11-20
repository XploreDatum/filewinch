//! Behaviour that rclone can't produce: Digest sign-in and stale nonces, redirects, servers
//! that ignore `Range`, truncated bodies, and WebDAV-specific statuses (405, 409, 423, 507).

mod common;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use common::*;
use filewinch_core::{ErrorKind, Protocol, RPath};
use sha2::{Digest, Sha256};

fn connect_mock(port: u16, pass: &str) -> filewinch_core::Result<Box<dyn filewinch_core::Remote>> {
    connect_with(&site(Protocol::WebDav, "127.0.0.1", port, "alice"), pass)
}

fn hex256(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// `key=value` / `key="value"` pairs of an Authorization header (no escapes needed here).
fn auth_params(h: &str) -> HashMap<String, String> {
    let rest = h.split_once(' ').map_or("", |(_, r)| r);
    let mut out = HashMap::new();
    let mut s = rest;
    while let Some(eq) = s.find('=') {
        let key = s[..eq].trim().trim_start_matches(',').trim().to_string();
        s = &s[eq + 1..];
        let value;
        if let Some(q) = s.strip_prefix('"') {
            let end = q.find('"').unwrap();
            value = q[..end].to_string();
            s = &q[end + 1..];
        } else {
            let end = s.find(',').unwrap_or(s.len());
            value = s[..end].trim().to_string();
            s = &s[end..];
        }
        out.insert(key, value);
    }
    out
}

#[derive(Default)]
struct DigestState {
    nonce: String,
    /// Nonces the server no longer accepts (answered with stale=true).
    retired: Vec<String>,
    last_nc: u32,
    basic_seen: bool,
    ok_requests: u32,
    stale_replies: u32,
}

fn challenge(nonce: &str, stale: bool) -> Vec<u8> {
    let digest = format!(
        r#"Digest realm="dav", qop="auth", algorithm=SHA-256, nonce="{nonce}", opaque="op"{}"#,
        if stale { ", stale=true" } else { "" }
    );
    resp("401 Unauthorized", &[("WWW-Authenticate", r#"Basic realm="dav""#), ("WWW-Authenticate", &digest)], b"")
}

/// `Ok` if the request carries a valid Digest for the current nonce; else the 401 to send.
fn verify(req: &common::Request, st: &mut DigestState) -> Result<(), Vec<u8>> {
    let Some(h) = req.header("authorization") else { return Err(challenge(&st.nonce, false)) };
    if h.starts_with("Basic") {
        st.basic_seen = true;
        return Err(challenge(&st.nonce, false));
    }
    let p = auth_params(h);
    assert_eq!(p["uri"], req.path, "digest uri must be the request target");
    assert_eq!(p["algorithm"], "SHA-256");
    assert_eq!(p["opaque"], "op");
    assert_eq!(p["username"], "alice");
    if p["nonce"] != st.nonce {
        assert!(st.retired.contains(&p["nonce"]), "unknown nonce");
        st.stale_replies += 1;
        return Err(challenge(&st.nonce, true));
    }
    let ha1 = hex256("alice:dav:s3cret");
    let ha2 = hex256(&format!("{}:{}", req.method, req.path));
    let expected = hex256(&format!("{ha1}:{}:{}:{}:auth:{ha2}", st.nonce, p["nc"], p["cnonce"]));
    if p["response"] != expected {
        return Err(challenge(&st.nonce, false));
    }
    let nc = u32::from_str_radix(&p["nc"], 16).unwrap();
    assert!(nc > st.last_nc, "nonce count must increase");
    st.last_nc = nc;
    st.ok_requests += 1;
    Ok(())
}

#[test]
fn digest_sign_in_stale_nonce_and_wrong_password() {
    let state = Arc::new(Mutex::new(DigestState { nonce: "n1".into(), ..Default::default() }));
    let files = Arc::new(Mutex::new(HashMap::<String, Vec<u8>>::new()));
    let (st, fs) = (state.clone(), files.clone());
    let port = mock(move |req, r| {
        let verdict = verify(req, &mut st.lock().unwrap());
        if let Err(reply) = verdict {
            // Answer before reading any upload body (as with Expect: 100-continue).
            if req.method != "PUT" {
                let _ = read_body(req, r);
            }
            return if req.method == "PUT" { Reply::SendAndClose(reply) } else { Reply::Send(reply) };
        }
        match req.method.as_str() {
            "PROPFIND" => {
                let _ = read_body(req, r);
                propfind_ok(&[("/", true, 0), ("/a.txt", false, 3)])
            }
            "PUT" => {
                if req.header("expect").is_some() {
                    let _ = r.get_mut().write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
                }
                let body = read_body(req, r).unwrap();
                fs.lock().unwrap().insert(req.path.clone(), body);
                Reply::Send(resp("201 Created", &[], b""))
            }
            _ => Reply::Send(resp("405 Method Not Allowed", &[], b"")),
        }
    });

    let err = connect_mock(port, "wrong").err().expect("wrong password");
    assert_eq!(err.kind, ErrorKind::Auth, "{err}");
    assert!(err.message.contains("alice@127.0.0.1"), "{err}");

    let r = connect_mock(port, "s3cret").unwrap();
    assert_eq!(r.list(&RPath::root()).unwrap().len(), 1);

    // The server rotates its nonce: the next request gets stale=true and is re-sent.
    {
        let mut s = state.lock().unwrap();
        s.retired.push("n1".into());
        s.nonce = "n2".into();
        s.last_nc = 0;
    }
    assert_eq!(r.list(&RPath::root()).unwrap().len(), 1);
    assert_eq!(state.lock().unwrap().stale_replies, 1);

    // Again, but on an upload: the 401 arrives before any body byte, so it's re-sent too.
    {
        let mut s = state.lock().unwrap();
        s.retired.push("n2".into());
        s.nonce = "n3".into();
        s.last_nc = 0;
    }
    let mut sink = r.write(&RPath::new("/up load.txt"), 0, Some(5)).unwrap();
    sink.write_all(b"hello").unwrap();
    sink.finish().unwrap();
    assert_eq!(files.lock().unwrap()["/up%20load.txt"], b"hello");

    let s = state.lock().unwrap();
    assert_eq!(s.stale_replies, 2);
    assert!(!s.basic_seen, "Basic must not be used when Digest is offered");
    assert!(s.ok_requests >= 4);
}

#[test]
fn basic_only_server_and_unsupported_scheme() {
    let port = mock(|req, r| {
        let _ = read_body(req, r);
        let ok = req.header("authorization") == Some("Basic YWxpY2U6czNjcmV0");
        if !ok {
            return Reply::Send(resp("401 Unauthorized", &[("WWW-Authenticate", r#"Basic realm="x", charset="UTF-8""#)], b""));
        }
        propfind_ok(&[("/", true, 0)])
    });
    connect_mock(port, "s3cret").unwrap();
    assert_eq!(connect_mock(port, "nope").err().unwrap().kind, ErrorKind::Auth);

    let port = mock(|req, r| {
        let _ = read_body(req, r);
        Reply::Send(resp("401 Unauthorized", &[("WWW-Authenticate", "NTLM"), ("WWW-Authenticate", "Negotiate")], b""))
    });
    let err = connect_mock(port, "x").err().unwrap();
    assert_eq!(err.kind, ErrorKind::Unsupported, "{err}");
    assert!(err.detail.unwrap().contains("ntlm"));
}

#[test]
fn redirects() {
    let port = Arc::new(Mutex::new(0u16));
    let p = port.clone();
    let srv = mock(move |req, r| {
        let me = *p.lock().unwrap();
        match req.path.as_str() {
            "/" => {
                let _ = read_body(req, r);
                propfind_ok(&[("/", true, 0)])
            }
            "/dir" => {
                let _ = read_body(req, r);
                let loc = format!("http://127.0.0.1:{me}/dir/");
                Reply::Send(resp("301 Moved Permanently", &[("Location", &loc)], b""))
            }
            "/dir/" => {
                let _ = read_body(req, r);
                propfind_ok(&[("/dir/", true, 0), ("/dir/f", false, 1)])
            }
            "/away" => {
                let _ = read_body(req, r);
                let loc = format!("https://127.0.0.1:{me}/away");
                Reply::Send(resp("302 Found", &[("Location", &loc)], b""))
            }
            "/loop" | "/loop2" => {
                let _ = read_body(req, r);
                let to = if req.path == "/loop" { "/loop2" } else { "/loop" };
                Reply::Send(resp("307 Temporary Redirect", &[("Location", to)], b""))
            }
            _ => {
                let _ = read_body(req, r);
                Reply::Send(resp("404 Not Found", &[], b""))
            }
        }
    });
    *port.lock().unwrap() = srv;
    let r = connect_mock(srv, "x").unwrap();
    assert!(r.stat(&RPath::new("/dir")).unwrap().is_dir());
    let err = r.stat(&RPath::new("/away")).unwrap_err();
    assert!(err.message.contains("WebDAV (HTTPS)"), "{err}");
    let err = r.stat(&RPath::new("/loop")).unwrap_err();
    assert!(err.message.contains("redirected the request more than once"), "{err}");
}

#[test]
fn ranges_ignored_wrong_or_past_end_and_truncated_bodies() {
    let data: Vec<u8> = (0..5000u32).map(|i| (i % 253) as u8).collect();
    let d = data.clone();
    let port = mock(move |req, r| {
        let _ = read_body(req, r);
        match (req.method.as_str(), req.path.as_str()) {
            ("PROPFIND", _) => propfind_ok(&[("/", true, 0)]),
            ("GET", "/plain.bin") => Reply::Send(resp("200 OK", &[], &d)),
            ("GET", "/wrong.bin") => {
                Reply::Send(resp("206 Partial Content", &[("Content-Range", "bytes 0-4999/5000")], &d))
            }
            ("GET", "/end.bin") => Reply::Send(resp("416 Range Not Satisfiable", &[("Content-Range", "bytes */5000")], b"")),
            ("GET", "/shrunk.bin") => Reply::Send(resp("416 Range Not Satisfiable", &[("Content-Range", "bytes */10")], b"")),
            ("GET", "/short.bin") => {
                let mut bytes = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", d.len()).into_bytes();
                bytes.extend_from_slice(&d[..1000]);
                Reply::SendAndClose(bytes)
            }
            _ => Reply::Send(resp("404 Not Found", &[], b"")),
        }
    });
    let r = connect_mock(port, "x").unwrap();
    let mut out = Vec::new();
    r.read(&RPath::new("/plain.bin"), 1234).unwrap().read_to_end(&mut out).unwrap();
    assert_eq!(out, data[1234..], "a server ignoring Range still resumes correctly");

    let err = r.read(&RPath::new("/wrong.bin"), 100).err().unwrap();
    assert!(err.message.contains("wrong part"), "{err}");

    out.clear();
    r.read(&RPath::new("/end.bin"), 5000).unwrap().read_to_end(&mut out).unwrap();
    assert!(out.is_empty());
    assert_eq!(r.read(&RPath::new("/shrunk.bin"), 5000).err().unwrap().kind, ErrorKind::InvalidPath);

    out.clear();
    let err = r.read(&RPath::new("/short.bin"), 0).unwrap().read_to_end(&mut out).unwrap_err();
    assert_eq!(filewinch_http::from_io(err).kind, ErrorKind::Network);
}

#[test]
fn webdav_statuses_map_to_clear_errors() {
    let port = mock(|req, r| {
        if req.method == "PUT" {
            // Reject before reading the body, then hang up.
            let status = if req.path.starts_with("/full/") { "507 Insufficient Storage" } else { "409 Conflict" };
            return Reply::SendAndClose(resp(status, &[("Connection", "close")], b""));
        }
        let _ = read_body(req, r);
        let reply = match (req.method.as_str(), req.path.as_str()) {
            ("PROPFIND", "/") => return propfind_ok(&[("/", true, 0)]),
            ("PROPFIND", "/locked.txt") => return propfind_ok(&[("/locked.txt", false, 1)]),
            ("PROPFIND", "/secret") => resp("403 Forbidden", &[], b"no"),
            ("MKCOL", "/exists/") => resp("405 Method Not Allowed", &[], b""),
            ("MKCOL", "/a/b/") => resp("409 Conflict", &[], b""),
            ("DELETE", "/locked.txt") => resp("423 Locked", &[], b""),
            ("GET", "/busy") => resp("503 Service Unavailable", &[("Retry-After", "7")], b""),
            _ => resp("404 Not Found", &[], b""),
        };
        Reply::Send(reply)
    });
    let r = connect_mock(port, "x").unwrap();

    let mut sink = r.write(&RPath::new("/full/report.pdf"), 0, Some(1 << 20)).unwrap();
    let write_err = sink.write_all(&vec![0u8; 1 << 20]).err();
    let err = match write_err {
        Some(e) => filewinch_http::from_io(e),
        None => sink.finish().unwrap_err(),
    };
    assert_eq!(err.kind, ErrorKind::NoSpace, "{err}");
    assert_eq!(err.message, "Couldn't upload report.pdf: the server is out of space.");

    let mut sink = r.write(&RPath::new("/nowhere/x.txt"), 0, None).unwrap();
    let _ = sink.write_all(b"abc");
    let err = sink.finish().unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotFound, "{err}");

    assert_eq!(r.mkdir(&RPath::new("/exists")).unwrap_err().kind, ErrorKind::AlreadyExists);
    let err = r.mkdir(&RPath::new("/a/b")).unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotFound);
    assert!(err.message.contains("parent folder doesn't exist"), "{err}");
    let err = r.remove_file(&RPath::new("/locked.txt")).unwrap_err();
    assert_eq!(err.kind, ErrorKind::PermissionDenied);
    assert!(err.message.contains("locked"), "{err}");
    assert_eq!(r.stat(&RPath::new("/secret")).unwrap_err().kind, ErrorKind::PermissionDenied);
    let err = r.read(&RPath::new("/busy"), 0).err().unwrap();
    assert_eq!(err.kind, ErrorKind::RateLimited);
    assert_eq!(err.retry_after, Some(std::time::Duration::from_secs(7)));
    assert_eq!(r.write(&RPath::new("/x"), 5, None).err().unwrap().kind, ErrorKind::Unsupported);
}

#[test]
fn servers_that_arent_webdav() {
    let html = mock(|req, r| {
        let _ = read_body(req, r);
        Reply::Send(resp("200 OK", &[("Content-Type", "text/html")], b"<html><body>Hi</body></html>"))
    });
    let err = connect_mock(html, "x").err().unwrap();
    assert_eq!(err.kind, ErrorKind::Other, "{err}");
    assert!(err.message.contains("couldn't read"), "{err}");

    let plain = mock(|req, r| {
        let _ = read_body(req, r);
        Reply::Send(resp("405 Method Not Allowed", &[("Allow", "GET, HEAD")], b""))
    });
    let err = connect_mock(plain, "x").err().unwrap();
    assert_eq!(err.kind, ErrorKind::Unsupported, "{err}");
}

/// Segmented downloads' ranged reads: a 206 with the right Content-Range gives exactly the
/// range; a server that ignores Range (200), a wrong range, a 416 and a short body are typed
/// errors, never wrong bytes.
#[test]
fn read_range_needs_the_exact_partial_reply() {
    let data: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
    let d = data.clone();
    let port = mock(move |req, r| {
        let _ = read_body(req, r);
        let range = req.header("range").and_then(|h| h.strip_prefix("bytes=")).map(|h| {
            let (a, b) = h.split_once('-').unwrap();
            (a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap())
        });
        match (req.method.as_str(), req.path.as_str(), range) {
            ("PROPFIND", _, _) => propfind_ok(&[("/", true, 0)]),
            ("GET", "/ranged.bin", Some((a, b))) => {
                let b = b.min(d.len() - 1);
                let cr = format!("bytes {a}-{b}/{}", d.len());
                Reply::Send(resp("206 Partial Content", &[("Content-Range", &cr)], &d[a..=b]))
            }
            ("GET", "/plain.bin", _) => Reply::Send(resp("200 OK", &[], &d)),
            ("GET", "/wrong.bin", _) => Reply::Send(resp("206 Partial Content", &[("Content-Range", "bytes 0-99/5000")], &d[..100])),
            ("GET", "/end.bin", _) => Reply::Send(resp("416 Range Not Satisfiable", &[("Content-Range", "bytes */5000")], b"")),
            ("GET", "/short.bin", Some((a, b))) => {
                let mut bytes = format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {a}-{b}/5000\r\nContent-Length: {}\r\n\r\n", b - a + 1).into_bytes();
                bytes.extend_from_slice(&d[a..a + 10]);
                Reply::SendAndClose(bytes)
            }
            _ => Reply::Send(resp("404 Not Found", &[], b"")),
        }
    });
    let r = connect_mock(port, "x").unwrap();
    let mut out = Vec::new();
    r.read_range(&RPath::new("/ranged.bin"), 1000, 1500).unwrap().read_to_end(&mut out).unwrap();
    assert_eq!(out, data[1000..2500]);
    out.clear();
    // The last piece: the server trims the range at the end of the file.
    r.read_range(&RPath::new("/ranged.bin"), 4900, 500).unwrap().read_to_end(&mut out).unwrap();
    assert_eq!(out, data[4900..]);

    for p in ["/plain.bin", "/wrong.bin"] {
        let e = r.read_range(&RPath::new(p), 100, 100).err().unwrap();
        assert_eq!(e.kind, ErrorKind::Unsupported, "{p}: {e}");
        assert!(e.message.contains("whole file"), "{p}: {e}");
    }
    assert_eq!(r.read_range(&RPath::new("/end.bin"), 6000, 10).err().unwrap().kind, ErrorKind::InvalidPath);
    out.clear();
    let err = r.read_range(&RPath::new("/short.bin"), 100, 100).unwrap().read_to_end(&mut out).unwrap_err();
    assert_eq!(filewinch_core::Error::from(err).kind, ErrorKind::Network);
}
