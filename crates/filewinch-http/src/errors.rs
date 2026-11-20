//! Mapping transport-level failures to core errors, and carrying core errors through
//! `std::io` (`Read`/`Write` impls) without losing them.

use std::io;

use filewinch_core::{Error, ErrorKind};

use crate::status::scrub;
use crate::tls::Rejection;

/// Converts a core Error into an io::Error for use inside `Read`/`Write` impls. The core
/// Error is boxed as the inner error so core's `From<io::Error>` (and [`from_io`]) recover it
/// exactly.
///
/// `Cancelled` maps to `io::ErrorKind::Other`, not `Interrupted`: std's `write_all`,
/// `read_exact` and `io::copy` silently retry `Interrupted`, which would swallow a cancel.
pub fn to_io(e: Error) -> io::Error {
    let kind = match e.kind {
        ErrorKind::Network if is_timeout_message(&e.message) => io::ErrorKind::TimedOut,
        ErrorKind::Network => io::ErrorKind::ConnectionReset,
        ErrorKind::NotFound => io::ErrorKind::NotFound,
        ErrorKind::PermissionDenied | ErrorKind::Auth => io::ErrorKind::PermissionDenied,
        ErrorKind::AlreadyExists => io::ErrorKind::AlreadyExists,
        ErrorKind::NoSpace => io::ErrorKind::StorageFull,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, e)
}

/// Like core's `From<io::Error>` (which already unwraps a wrapped core Error), except that an
/// io::Error wrapping a ureq error (as ureq's body readers produce for timeouts and protocol
/// errors) is mapped like a transport error, so a stalled download is `Network`, not `Other`.
pub fn from_io(e: io::Error) -> Error {
    if e.get_ref().is_some_and(|inner| inner.is::<ureq::Error>()) {
        return map_transport(ureq::Error::from(e), "the server", None);
    }
    Error::from(e)
}

fn is_timeout_message(message: &str) -> bool {
    message.contains("timed out")
}

/// The body of `Http::transport_error`. `rejection` is the recorded certificate failure for
/// `host`, if the TLS verifier recorded one.
pub(crate) fn map_transport(e: ureq::Error, host: &str, rejection: Option<Rejection>) -> Error {
    use ureq::Error as U;
    let net = |message: String| Error::new(ErrorKind::Network, message);
    match e {
        U::Rustls(tls) => tls_error(&tls.to_string(), host, rejection),
        U::Tls(reason) => tls_error(reason, host, rejection),
        U::Io(io) => {
            if io.get_ref().is_some_and(|inner| inner.is::<Error>()) {
                return from_io(io);
            }
            if let Some(tls) = io.get_ref().and_then(|i| i.downcast_ref::<rustls::Error>()) {
                return tls_error(&tls.to_string(), host, rejection);
            }
            use io::ErrorKind as K;
            let detail = scrub(&io.to_string());
            // std reports getaddrinfo failures as an uncategorized io error: by text on Unix,
            // and on Windows by Winsock code (WSAHOST_NOT_FOUND, WSATRY_AGAIN, WSANO_RECOVERY,
            // WSANO_DATA: "No such host is known.").
            let windows_dns = cfg!(windows) && matches!(io.raw_os_error(), Some(11001..=11004));
            if windows_dns || detail.contains("failed to lookup address") {
                return net(not_found_message(host)).with_detail(detail);
            }
            let message = match io.kind() {
                K::TimedOut | K::WouldBlock => format!("The connection to {host} timed out."),
                K::ConnectionRefused => format!("{host} refused the connection."),
                K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe => {
                    format!("The connection to {host} was lost.")
                }
                K::UnexpectedEof => format!("{host} closed the connection unexpectedly."),
                K::NotConnected
                | K::HostUnreachable
                | K::NetworkUnreachable
                | K::NetworkDown
                | K::AddrNotAvailable => {
                    format!("Couldn't reach {host}. Check your internet connection.")
                }
                _ => {
                    return Error::new(ErrorKind::Other, format!("Couldn't talk to {host}."))
                        .with_detail(detail);
                }
            };
            net(message).with_detail(detail)
        }
        U::Timeout(which) => net(format!("The connection to {host} timed out."))
            .with_detail(format!("timeout: {which:?}")),
        U::HostNotFound => net(not_found_message(host)),
        U::ConnectionFailed => net(format!("Couldn't connect to {host}.")),
        // Never echo the URI: it may carry a SAS token or presigned query.
        U::BadUri(_) | U::Http(_) => Error::new(
            ErrorKind::Other,
            format!("The address for {host} isn't valid."),
        ),
        // Legal HTTP that ureq rejects: a `100 Continue` with headers. `send_reader` stops
        // sending `Expect` to this server after it happens once, so a retry goes through.
        U::Protocol(ureq_proto::Error::HeadersWith100) => net(format!(
            "{host} answered the upload in an unusual way; trying again will work."
        ))
        .with_detail("100 Continue with headers"),
        U::Protocol(p) => Error::new(
            ErrorKind::Other,
            format!("{host} sent a response Filewinch couldn't understand."),
        )
        .with_detail(scrub(&p.to_string())),
        other => Error::new(ErrorKind::Other, format!("Couldn't talk to {host}."))
            .with_detail(scrub(&other.to_string())),
    }
}

fn not_found_message(host: &str) -> String {
    format!("Couldn't find the server {host}. Check the address and your internet connection.")
}

fn tls_error(reason: &str, host: &str, rejection: Option<Rejection>) -> Error {
    match rejection {
        Some(r) => Error::host_trust(
            format!("The certificate for {host} isn't trusted."),
            &r.fingerprint,
            &r.reason,
        ),
        None => Error::new(
            ErrorKind::Other,
            format!("Couldn't make a secure connection to {host}."),
        )
        .with_detail(reason),
    }
}
