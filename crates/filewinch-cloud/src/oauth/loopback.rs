//! The one-shot loopback HTTP listener that receives the provider's redirect.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use filewinch_core::{Error, ErrorKind, Result};
use percent_encoding::percent_decode_str;

use super::{CancelToken, Provider};

/// Redirect ports registered with Dropbox and Box, which only accept exact redirect URIs:
/// `http://127.0.0.1:<port>/callback` for each. The first free one is used.
pub const FIXED_PORTS: [u16; 5] = [47811, 47812, 47813, 47814, 47815];

pub(crate) struct Listener {
    sockets: Vec<TcpListener>,
    port: u16,
}

impl Listener {
    pub(crate) fn bind(provider: Provider, fixed: &[u16]) -> Result<Listener> {
        let v4 = |port| TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
        let first = if provider.needs_fixed_port() {
            fixed.iter().find_map(|&p| v4(p).ok()).ok_or_else(|| {
                Error::new(
                    ErrorKind::Other,
                    format!(
                        "Filewinch couldn't start the {} sign-in: the sign-in ports {} are all in use. \
                         Quit apps that might be using them and try again.",
                        provider.label(),
                        port_list(fixed)
                    ),
                )
            })?
        } else {
            v4(0).map_err(|e| {
                Error::new(
                    ErrorKind::Other,
                    "Filewinch couldn't start the sign-in listener.",
                )
                .with_detail(e)
            })?
        };
        let port = first.local_addr()?.port();
        let mut sockets = vec![first];
        // `localhost` may resolve to ::1 first; listen there too so the redirect lands either way.
        if provider.redirect_host() == "localhost"
            && let Ok(v6) = TcpListener::bind(SocketAddr::from((Ipv6Addr::LOCALHOST, port)))
        {
            sockets.push(v6);
        }
        for s in &sockets {
            s.set_nonblocking(true)?;
        }
        Ok(Listener { sockets, port })
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    /// Waits for `GET /callback?code=…&state=…`. Stray requests (favicon, other paths) get a
    /// 404 and the wait goes on; a provider error, a wrong `state`, the deadline or `cancel`
    /// end it.
    pub(crate) fn wait_for_code(
        &self,
        provider: Provider,
        state: &str,
        cancel: &CancelToken,
        deadline: Instant,
    ) -> Result<String> {
        loop {
            if cancel.is_cancelled() {
                return Err(Error::cancelled());
            }
            if Instant::now() >= deadline {
                return Err(Error::new(
                    ErrorKind::Auth,
                    format!(
                        "The {} sign-in timed out before the browser came back. Try again.",
                        provider.label()
                    ),
                ));
            }
            let mut idle = true;
            for socket in &self.sockets {
                match socket.accept() {
                    Ok((stream, _)) => {
                        idle = false;
                        if let Some(code) = handle(stream, provider, state)? {
                            return Ok(code);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        return Err(Error::new(
                            ErrorKind::Other,
                            "The sign-in listener stopped unexpectedly. Try again.",
                        )
                        .with_detail(e));
                    }
                }
            }
            if idle {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn port_list(ports: &[u16]) -> String {
    match (ports.first(), ports.last()) {
        (Some(a), Some(b)) if a != b => format!("{a}–{b}"),
        (Some(a), _) => a.to_string(),
        _ => "(none)".into(),
    }
}

/// `Ok(Some(code))` on success, `Ok(None)` to keep waiting, `Err` to end the sign-in.
fn handle(mut stream: TcpStream, provider: Provider, state: &str) -> Result<Option<String>> {
    // A broken or slow connection is the browser's problem, not a reason to stop waiting.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let Some(head) = read_head(&mut stream) else {
        return Ok(None);
    };
    let mut words = head.lines().next().unwrap_or("").split_whitespace();
    let (method, target) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if method != "GET" || path != "/callback" {
        respond(
            &mut stream,
            "404 Not Found",
            "Not found",
            "There's nothing here.",
        );
        return Ok(None);
    }
    let params = parse_query(query);
    let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let service = provider.label();

    if let Some(err) = get("error") {
        let (title, msg) = if err == "access_denied" {
            (
                "Access not allowed".to_string(),
                format!("You didn't give Filewinch access to {service}."),
            )
        } else {
            let what = get("error_description").unwrap_or(err);
            (
                "Sign-in failed".to_string(),
                format!("{service} couldn't sign you in: {what}"),
            )
        };
        respond(
            &mut stream,
            "200 OK",
            &title,
            &format!("{msg} You can close this tab."),
        );
        return Err(Error::new(ErrorKind::Auth, msg).with_detail(format!("error={err}")));
    }
    if get("state") != Some(state) {
        respond(
            &mut stream,
            "400 Bad Request",
            "Sign-in not accepted",
            "This sign-in response doesn't match the one Filewinch started. \
             Close this tab and sign in again from Filewinch.",
        );
        return Err(Error::new(
            ErrorKind::Auth,
            format!(
                "The {service} sign-in response didn't match the request Filewinch made, \
                 so it was rejected. Try again."
            ),
        ));
    }
    let Some(code) = get("code").filter(|c| !c.is_empty()) else {
        respond(
            &mut stream,
            "400 Bad Request",
            "Sign-in failed",
            "No authorization code arrived.",
        );
        return Err(Error::new(
            ErrorKind::Auth,
            format!("{service} came back without an authorization code. Try again."),
        ));
    };
    respond(
        &mut stream,
        "200 OK",
        "You're signed in",
        "You can close this tab and return to Filewinch.",
    );
    Ok(Some(code.to_string()))
}

fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        if buf.len() > 16 * 1024 {
            return None;
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return None,
        }
    }
    String::from_utf8(buf).ok()
}

fn parse_query(q: &str) -> Vec<(String, String)> {
    let decode = |s: &str| {
        percent_decode_str(&s.replace('+', " "))
            .decode_utf8_lossy()
            .into_owned()
    };
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect()
}

fn respond(stream: &mut TcpStream, status: &str, title: &str, message: &str) {
    let body = page(title, message);
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

fn page(title: &str, message: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Filewinch</title>
<style>
:root {{ color-scheme: light dark; --bg: #f6f5f2; --card: #ffffff; --ink: #1d1d1f; --muted: #5f6368; }}
@media (prefers-color-scheme: dark) {{ :root {{ --bg: #1b1b1d; --card: #262629; --ink: #f2f2f2; --muted: #a8a8ad; }} }}
body {{ margin: 0; min-height: 100vh; display: grid; place-items: center; background: var(--bg);
  font: 15px/1.5 -apple-system, BlinkMacSystemFont, "Helvetica Neue", sans-serif; color: var(--ink); }}
main {{ background: var(--card); padding: 32px 40px; border-radius: 14px; max-width: 420px; margin: 16px;
  box-shadow: 0 1px 3px rgba(0,0,0,.12); text-align: center; }}
h1 {{ font-size: 19px; margin: 0 0 8px; }}
p {{ margin: 0; color: var(--muted); }}
</style></head>
<body><main><h1>{}</h1><p>{}</p></main></body></html>
"#,
        escape(title),
        escape(message)
    )
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
