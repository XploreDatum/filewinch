//! Maps russh and russh-sftp errors to Filewinch errors with readable messages.

use filewinch_core::{Error, ErrorKind};
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::StatusCode;

pub(crate) fn ssh(e: russh::Error, who: &str) -> Error {
    use russh::Error as E;
    let (kind, msg) = match &e {
        E::IO(io) => return io_err(io, who),
        E::ConnectionTimeout | E::KeepaliveTimeout | E::InactivityTimeout => {
            (ErrorKind::Network, format!("The connection to {who} timed out."))
        }
        E::HUP | E::Disconnect | E::SendError | E::RecvError => (ErrorKind::Network, format!("The connection to {who} was closed.")),
        E::NoCommonAlgo { .. } | E::UnknownAlgo | E::Version => {
            (ErrorKind::Unsupported, format!("{who} uses an SSH version or algorithms Filewinch doesn't support."))
        }
        E::NotAuthenticated | E::NoAuthMethod | E::UnsupportedAuthMethod => (ErrorKind::Auth, format!("{who} didn't accept any sign-in method.")),
        E::ChannelOpenFailure(_) | E::RequestDenied => (ErrorKind::PermissionDenied, format!("{who} refused to start SFTP. It may only allow a shell, or none.")),
        _ => (ErrorKind::Network, format!("The SSH connection to {who} failed.")),
    };
    Error::new(kind, msg).with_detail(e)
}

pub(crate) fn io_err(e: &std::io::Error, who: &str) -> Error {
    use std::io::ErrorKind as K;
    let msg = match e.kind() {
        K::ConnectionRefused => format!("{who} refused the connection. Is SFTP running on that port?"),
        K::TimedOut => format!("{who} didn't answer in time."),
        K::HostUnreachable | K::NetworkUnreachable | K::NetworkDown => format!("{who} can't be reached from this network."),
        _ if e.to_string().contains("lookup") || e.to_string().contains("nodename") => {
            format!("Couldn't find the server {who}. Check the address.")
        }
        _ => format!("Couldn't connect to {who}."),
    };
    let kind = match Error::from(std::io::Error::new(e.kind(), "")).kind {
        ErrorKind::Other => ErrorKind::Network,
        k => k,
    };
    Error::new(kind, msg).with_detail(e)
}

/// `what` describes the operation and path, e.g. "open “/var/www/index.html”".
pub(crate) fn sftp(e: SftpError, what: &str) -> Error {
    match &e {
        SftpError::Status(s) => {
            let (kind, msg) = match s.status_code {
                StatusCode::NoSuchFile => (ErrorKind::NotFound, format!("Couldn't {what}: it doesn't exist.")),
                StatusCode::PermissionDenied => (ErrorKind::PermissionDenied, format!("Couldn't {what}: permission denied.")),
                StatusCode::OpUnsupported => (ErrorKind::Unsupported, format!("The server doesn't support this: couldn't {what}.")),
                StatusCode::NoConnection | StatusCode::ConnectionLost => (ErrorKind::Network, format!("The connection dropped while trying to {what}.")),
                StatusCode::Eof => (ErrorKind::Other, format!("Couldn't {what}: unexpected end of file.")),
                _ => (ErrorKind::Other, format!("The server couldn't {what}.")),
            };
            let detail = if s.error_message.is_empty() { format!("{}", s.status_code) } else { s.error_message.clone() };
            Error::new(kind, msg).with_detail(detail)
        }
        SftpError::Timeout => Error::new(ErrorKind::Network, format!("The server took too long to {what}.")),
        SftpError::IO(_) | SftpError::UnexpectedBehavior(_) => {
            Error::new(ErrorKind::Network, format!("The connection dropped while trying to {what}.")).with_detail(e)
        }
        _ => Error::new(ErrorKind::Other, format!("Couldn't {what}.")).with_detail(e),
    }
}

/// An I/O error from a russh-sftp file handle. Status replies arrive as plain messages, so a
/// dead session (checked by the caller) is what tells a dropped connection apart.
pub(crate) fn file_io(e: std::io::Error, session_dead: bool, what: &str) -> Error {
    if session_dead || matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::UnexpectedEof) {
        return Error::new(ErrorKind::Network, format!("The connection dropped while trying to {what}.")).with_detail(e);
    }
    let text = e.to_string().to_ascii_lowercase();
    let kind = if text.contains("illegal seek") || text.contains("not supported") || text.contains("unsupported") {
        ErrorKind::Unsupported
    } else if text.contains("permission") {
        ErrorKind::PermissionDenied
    } else if text.contains("no space") || text.contains("quota") {
        ErrorKind::NoSpace
    } else if text.contains("no such file") {
        ErrorKind::NotFound
    } else if text.contains("i/o:") || text.contains("timeout") || text.contains("sending") || text.contains("closed") {
        ErrorKind::Network
    } else {
        ErrorKind::Other
    };
    Error::new(kind, format!("Couldn't {what}.")).with_detail(e)
}
