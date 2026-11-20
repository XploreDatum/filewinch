//! A TCP forwarder in front of tiny_http that makes its `100 Continue` look like a real
//! server's. tiny_http sends `HTTP/1.1 100 Continue` with `Server`/`Date` headers, which
//! ureq (rightly) rejects; Swift's eventlet server sends the bare status line. tiny_http
//! writes the interim response as one flushed write before anything else for that request,
//! so it is matched at the start of a read chunk.

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};

const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n";

/// Starts the forwarder; returns its address. It lives as long as the test process.
pub fn start(target: SocketAddr) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind shim");
    let addr = listener.local_addr().expect("shim addr");
    std::thread::spawn(move || {
        for client in listener.incoming() {
            let Ok(client) = client else { continue };
            let Ok(server) = TcpStream::connect(target) else { continue };
            let _ = client.set_nodelay(true);
            let _ = server.set_nodelay(true);
            let (c2, s2) = (client.try_clone().expect("clone"), server.try_clone().expect("clone"));
            std::thread::spawn(move || pump(c2, s2, false));
            std::thread::spawn(move || pump(server, client, true));
        }
    });
    addr
}

fn pump(mut from: TcpStream, mut to: TcpStream, fix_continue: bool) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let mut chunk = &buf[..n];
        if fix_continue
            && chunk.starts_with(CONTINUE)
            && let Some(end) = chunk.windows(4).position(|w| w == b"\r\n\r\n")
        {
            if to.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").is_err() {
                break;
            }
            chunk = &chunk[end + 4..];
        }
        if to.write_all(chunk).is_err() {
            break;
        }
    }
    let _ = to.shutdown(Shutdown::Both);
    let _ = from.shutdown(Shutdown::Both);
}
