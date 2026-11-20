//! Idle timeouts: a stalled server fails fast, a slow-but-steady one never times out.

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use common::{read_request, reader, serve};
use filewinch_core::ErrorKind;
use filewinch_http::Http;

fn short() -> Http {
    Http::with_timeouts(Duration::from_secs(2), Duration::from_secs(1))
}

#[test]
fn stall_after_headers_is_network_error_within_idle_time() {
    let port = serve(|mut stream: TcpStream| {
        let mut r = reader(&stream);
        if read_request(&mut r).is_ok() {
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n0123456789");
            std::thread::sleep(Duration::from_secs(6));
        }
    });
    let resp = short()
        .agent()
        .get(format!("http://127.0.0.1:{port}/"))
        .call()
        .unwrap();
    let started = Instant::now();
    let mut body = Vec::new();
    let err = filewinch_http::body_reader(resp)
        .read_to_end(&mut body)
        .unwrap_err();
    let elapsed = started.elapsed();
    // The engine's path: core's plain `From<io::Error>` must still see Network.
    let err = filewinch_core::Error::from(err);
    assert_eq!(err.kind, ErrorKind::Network, "{err}");
    assert!(
        elapsed >= Duration::from_millis(800) && elapsed < Duration::from_secs(3),
        "{elapsed:?}"
    );
}

#[test]
fn stall_before_headers_is_network_error_within_idle_time() {
    let port = serve(|stream: TcpStream| {
        let mut r = reader(&stream);
        if read_request(&mut r).is_ok() {
            std::thread::sleep(Duration::from_secs(6));
        }
    });
    let http = short();
    let started = Instant::now();
    let err = http
        .agent()
        .get(format!("http://127.0.0.1:{port}/"))
        .call()
        .unwrap_err();
    let elapsed = started.elapsed();
    let err = http.transport_error(err, "127.0.0.1");
    assert_eq!(err.kind, ErrorKind::Network, "{err}");
    assert!(
        err.message.contains("127.0.0.1") && err.message.contains("timed out"),
        "{err}"
    );
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
}

#[test]
fn slow_but_steady_body_succeeds_past_idle_timeout() {
    let port = serve(|mut stream: TcpStream| {
        let mut r = reader(&stream);
        if read_request(&mut r).is_ok() {
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n");
            for i in 0..10u8 {
                std::thread::sleep(Duration::from_millis(300));
                let _ = stream.write_all(&[b'0' + i]);
            }
        }
    });
    let started = Instant::now();
    let resp = short()
        .agent()
        .get(format!("http://127.0.0.1:{port}/"))
        .call()
        .unwrap();
    let mut body = String::new();
    resp.into_body()
        .into_reader()
        .read_to_string(&mut body)
        .unwrap();
    assert_eq!(body, "0123456789");
    assert!(
        started.elapsed() >= Duration::from_secs(3),
        "the body took 3 s in total"
    );
}

#[test]
fn slow_but_steady_upload_succeeds_past_idle_timeout() {
    let port = serve(|mut stream: TcpStream| {
        let mut r = reader(&stream);
        if let Ok(req) = read_request(&mut r) {
            let body = common::read_body(&mut r, &req).unwrap();
            common::respond(&mut stream, "200 OK", "", &body);
        }
    });
    // A body source that dribbles 10 bytes over 3 s: total time > idle, gaps < idle.
    struct Dribble(u8);
    impl Read for Dribble {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0 == 10 || buf.is_empty() {
                return Ok(0);
            }
            std::thread::sleep(Duration::from_millis(300));
            buf[0] = b'a' + self.0;
            self.0 += 1;
            Ok(1)
        }
    }
    let http = short();
    let url = format!("http://127.0.0.1:{port}/up");
    let mut resp =
        filewinch_http::send_reader(http.agent().put(&url), Dribble(0), Some(10)).unwrap();
    assert_eq!(resp.body_mut().read_to_string().unwrap(), "abcdefghij");
}

#[test]
fn refused_connection_is_network_error_naming_host() {
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let http = short();
    let err = http
        .agent()
        .get(format!("http://127.0.0.1:{port}/?sig=SECRET"))
        .call()
        .unwrap_err();
    let err = http.transport_error(err, "127.0.0.1");
    assert_eq!(err.kind, ErrorKind::Network, "{err}");
    assert!(err.message.contains("127.0.0.1"));
    assert!(!err.to_string().contains("SECRET"));
}

#[test]
fn unknown_host_is_network_error() {
    let http = short();
    let err = http
        .agent()
        .get("http://nonexistent.invalid/")
        .call()
        .unwrap_err();
    let err = http.transport_error(err, "nonexistent.invalid");
    assert_eq!(err.kind, ErrorKind::Network, "{err}");
    assert!(err.message.contains("nonexistent.invalid"));
}
