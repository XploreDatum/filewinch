//! Streaming uploads through `upload_pipe` + `send_reader` against a local mini server.

mod common;

use std::io::{BufRead, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::{pattern, read_body, read_request, reader, respond, serve};
use filewinch_core::{ErrorKind, Result};
use filewinch_http::{Http, UploadPipe, from_io, send_reader, status_error, upload_pipe};

const SIZE: usize = 20 * 1024 * 1024;

/// What the server saw: framing and body (or the read error).
struct Seen {
    chunked: bool,
    content_length: Option<u64>,
    expect: bool,
    body: std::io::Result<Vec<u8>>,
}

/// Server that honours `Expect: 100-continue`, reads the body and answers `status`.
fn recording_server(status: &'static str) -> (u16, mpsc::Receiver<Seen>) {
    let (tx, rx) = mpsc::channel();
    let tx = std::sync::Mutex::new(tx);
    let port = serve(move |mut stream: TcpStream| {
        let mut r = reader(&stream);
        let Ok(req) = read_request(&mut r) else {
            return;
        };
        let expect = req.header("expect") == Some("100-continue");
        if expect {
            let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
        }
        let body = read_body(&mut r, &req);
        if body.is_ok() {
            respond(&mut stream, status, "", b"");
        }
        let seen = Seen {
            chunked: req.is_chunked(),
            content_length: req.content_length(),
            expect,
            body,
        };
        let _ = tx.lock().unwrap().send(seen);
    });
    (port, rx)
}

fn start_upload(http: &Http, port: u16, len: Option<u64>) -> UploadPipe<u16> {
    let http = http.clone();
    upload_pipe(move |body| -> Result<u16> {
        let url = format!("http://127.0.0.1:{port}/big.bin");
        let resp = send_reader(http.agent().put(&url), body, len)
            .map_err(|e| http.transport_error(e, "127.0.0.1"))?;
        if !resp.status().is_success() {
            return Err(status_error(resp, "upload big.bin"));
        }
        Ok(resp.status().as_u16())
    })
}

fn upload_20mb(len: Option<u64>) -> Seen {
    let (port, seen) = recording_server("201 Created");
    let http = Http::new();
    let data = pattern(SIZE);
    let mut pipe = start_upload(&http, port, len);
    // Odd-sized writes so chunk boundaries don't line up with writes.
    for piece in data.chunks(100_003) {
        pipe.write_all(piece).unwrap();
    }
    assert_eq!(pipe.finish().unwrap(), 201);
    let seen = seen.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(
        seen.body.as_ref().unwrap() == &data,
        "server received different bytes"
    );
    seen
}

#[test]
fn streams_20mb_chunked_byte_equal() {
    let seen = upload_20mb(None);
    assert!(seen.chunked);
    assert_eq!(seen.content_length, None);
    assert!(seen.expect);
}

#[test]
fn streams_20mb_with_content_length_byte_equal() {
    let seen = upload_20mb(Some(SIZE as u64));
    assert!(
        !seen.chunked,
        "must not be chunked when the length is known"
    );
    assert_eq!(seen.content_length, Some(SIZE as u64));
}

#[test]
fn early_401_surfaces_as_auth_from_write_and_finish() {
    // Rejects on the request head (as S3/Azure/nginx do with Expect: 100-continue) and
    // never reads the body.
    let port = serve(|mut stream: TcpStream| {
        let mut r = reader(&stream);
        if read_request(&mut r).is_ok() {
            respond(
                &mut stream,
                "401 Unauthorized",
                "",
                b"<Error>InvalidAccessKeyId</Error>",
            );
        }
    });
    let http = Http::new();
    let mut pipe = start_upload(&http, port, Some(SIZE as u64));
    let data = pattern(SIZE);
    let write_err = data
        .chunks(65_536)
        .find_map(|piece| pipe.write_all(piece).err());
    let write_err = write_err.expect("a write must fail once the worker has ended");
    assert_eq!(write_err.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(from_io(write_err).kind, ErrorKind::Auth);
    let err = pipe.finish().unwrap_err();
    assert_eq!(err.kind, ErrorKind::Auth, "{err}");
    assert!(err.message.contains("upload big.bin"), "{}", err.message);
}

fn drop_aborts(len: Option<u64>) {
    let (port, seen) = recording_server("201 Created");
    let http = Http::new();
    let mut pipe = start_upload(&http, port, len);
    let data = pattern(3 * 1024 * 1024);
    pipe.write_all(&data).unwrap();
    let started = Instant::now();
    drop(pipe);
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "Drop must not block"
    );
    let seen = seen
        .recv_timeout(Duration::from_secs(10))
        .expect("server saw nothing");
    assert!(seen.body.is_err(), "server must not see a complete body");
}

#[test]
fn drop_without_finish_aborts_content_length_upload() {
    drop_aborts(Some(SIZE as u64));
}

#[test]
fn drop_without_finish_aborts_chunked_upload() {
    drop_aborts(None);
}

#[test]
fn short_reader_with_content_length_aborts_with_clear_error() {
    let (port, seen) = recording_server("201 Created");
    let http = Http::new();
    let url = format!("http://127.0.0.1:{port}/short");
    let err = send_reader(http.agent().put(&url), &pattern(50)[..], Some(100)).unwrap_err();
    let err = http.transport_error(err, "127.0.0.1");
    assert_eq!(err.kind, ErrorKind::Other);
    assert!(
        err.message.contains("ended before its declared size"),
        "{}",
        err.message
    );
    let seen = seen.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(seen.body.is_err());
}

#[test]
fn custom_methods_and_ranged_get_with_static_reader() {
    let port = serve(|mut stream: TcpStream| {
        let mut r = reader(&stream);
        while let Ok(req) = read_request(&mut r) {
            let body = read_body(&mut r, &req).unwrap();
            let reply = format!(
                "{} {} depth={:?} range={:?} body={}",
                req.method,
                req.path,
                req.header("depth"),
                req.header("range"),
                String::from_utf8_lossy(&body)
            );
            let status = if req.header("range").is_some() {
                "206 Partial Content"
            } else {
                "207 Multi-Status"
            };
            respond(&mut stream, status, "", reply.as_bytes());
            if r.fill_buf().map(|b| b.is_empty()).unwrap_or(true) {
                break;
            }
        }
    });
    let http = Http::new();
    let base = format!("http://127.0.0.1:{port}");

    let req = ureq::http::Request::builder()
        .method("PROPFIND")
        .uri(format!("{base}/dav/"))
        .header("Depth", "1")
        .body("<propfind/>")
        .unwrap();
    let mut resp = http.agent().run(req).unwrap();
    assert_eq!(resp.status().as_u16(), 207);
    let text = resp
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_string()
        .unwrap();
    assert_eq!(
        text,
        "PROPFIND /dav/ depth=Some(\"1\") range=None body=<propfind/>"
    );

    let req = ureq::http::Request::builder()
        .method("MKCOL")
        .uri(format!("{base}/new%20folder/"))
        .body(())
        .unwrap();
    let mut resp = http.agent().run(req).unwrap();
    assert!(
        resp.body_mut()
            .read_to_string()
            .unwrap()
            .starts_with("MKCOL /new%20folder/ ")
    );

    let resp = http
        .agent()
        .get(format!("{base}/file"))
        .header("Range", "bytes=100-")
        .call()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 206);
    let content_range_free_len = resp.body().content_length();
    let mut body = resp.into_body().into_reader(); // BodyReader<'static>: Read + Send
    let text = std::thread::spawn(move || {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut body, &mut s).map(|_| s)
    })
    .join()
    .unwrap()
    .unwrap();
    assert_eq!(Some(text.len() as u64), content_range_free_len);
    assert!(text.contains("range=Some(\"bytes=100-\")"), "{text}");
}

/// A `100 Continue` with headers is legal HTTP but ureq rejects it. The first upload must
/// fail as a retryable Network error, and the retry must go out without `Expect` and succeed.
#[test]
fn continue_with_headers_is_retryable_and_retry_skips_expect() {
    let (tx, rx) = mpsc::channel();
    let tx = std::sync::Mutex::new(tx);
    let port = serve(move |mut stream: TcpStream| {
        let mut r = reader(&stream);
        let Ok(req) = read_request(&mut r) else {
            return;
        };
        let expect = req.header("expect").is_some();
        let _ = tx.lock().unwrap().send(expect);
        if expect {
            let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\nServer: odd\r\n\r\n");
            return;
        }
        if read_body(&mut r, &req).is_ok() {
            respond(&mut stream, "201 Created", "", b"");
        }
    });
    let http = Http::new();
    let data = pattern(1000);
    let put = |http: &Http| {
        send_reader(http.agent().put(format!("http://127.0.0.1:{port}/a")), &data[..], Some(1000))
            .map_err(|e| http.transport_error(e, "127.0.0.1"))
    };
    let err = put(&http).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Network, "{err}");
    assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), "first try sends Expect");
    let resp = put(&http).unwrap();
    assert_eq!(resp.status().as_u16(), 201);
    assert!(!rx.recv_timeout(Duration::from_secs(5)).unwrap(), "retry skips Expect");
}
