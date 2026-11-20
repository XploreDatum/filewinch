use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

/// What went wrong, as far as retry policy and the UI care.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// Login rejected or token expired and couldn't refresh. Ask for credentials again.
    Auth,
    /// The server's host key or TLS certificate is unknown or changed. Ask the user to trust it.
    HostTrust,
    NotFound,
    PermissionDenied,
    AlreadyExists,
    /// Directory not empty, path is a file not a dir, and similar.
    InvalidPath,
    /// Out of quota or disk space.
    NoSpace,
    /// Rate limited; `Error::retry_after` may say when.
    RateLimited,
    /// Connection dropped, timed out, DNS failed, 5xx. Safe to retry with backoff.
    Network,
    Unsupported,
    Cancelled,
    /// Session was closed.
    Closed,
    Other,
}

impl ErrorKind {
    /// The transfer engine retries these with backoff; everything else fails the item.
    pub fn is_transient(self) -> bool {
        matches!(self, ErrorKind::Network | ErrorKind::RateLimited)
    }
}

#[derive(Clone, Debug)]
pub struct Error {
    pub kind: ErrorKind,
    /// One sentence a person can read, e.g. "The server refused the password for deploy@web1".
    pub message: String,
    /// Raw protocol detail for the log and the "Details" disclosure.
    pub detail: Option<String>,
    pub retry_after: Option<std::time::Duration>,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), detail: None, retry_after: None }
    }
    pub fn with_detail(mut self, detail: impl fmt::Display) -> Self {
        self.detail = Some(detail.to_string());
        self
    }
    pub fn with_retry_after(mut self, after: std::time::Duration) -> Self {
        self.retry_after = Some(after);
        self
    }
    pub fn unsupported(what: &str) -> Self {
        Self::new(ErrorKind::Unsupported, format!("This server doesn't support {what}."))
    }
    pub fn cancelled() -> Self {
        Self::new(ErrorKind::Cancelled, "Cancelled.")
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)?;
        if let Some(d) = &self.detail {
            write!(f, " ({d})")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        // Backends report failures through `Read`/`Write` by wrapping a core `Error` in an
        // `io::Error`; unwrap it so kind, detail and retry_after survive the round trip.
        if let Some(inner) = e.get_ref().and_then(|i| i.downcast_ref::<Error>()) {
            return inner.clone();
        }
        use std::io::ErrorKind as K;
        let kind = match e.kind() {
            K::NotFound => ErrorKind::NotFound,
            K::PermissionDenied => ErrorKind::PermissionDenied,
            K::AlreadyExists => ErrorKind::AlreadyExists,
            K::DirectoryNotEmpty | K::NotADirectory | K::IsADirectory | K::InvalidFilename => {
                ErrorKind::InvalidPath
            }
            K::StorageFull | K::QuotaExceeded => ErrorKind::NoSpace,
            K::Unsupported => ErrorKind::Unsupported,
            K::ConnectionRefused
            | K::ConnectionReset
            | K::ConnectionAborted
            | K::NotConnected
            | K::BrokenPipe
            | K::TimedOut
            | K::UnexpectedEof
            | K::HostUnreachable
            | K::NetworkUnreachable
            | K::NetworkDown => ErrorKind::Network,
            _ => ErrorKind::Other,
        };
        Error::new(kind, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{Error, ErrorKind};
    use std::time::Duration;

    #[test]
    fn io_round_trip_keeps_core_error() {
        let core = Error::new(ErrorKind::RateLimited, "Slow down.")
            .with_detail("429")
            .with_retry_after(Duration::from_secs(7));
        let io = std::io::Error::other(core);
        let back: Error = io.into();
        assert_eq!(back.kind, ErrorKind::RateLimited);
        assert_eq!(back.retry_after, Some(Duration::from_secs(7)));
        assert_eq!(back.detail.as_deref(), Some("429"));
    }

    #[test]
    fn plain_io_errors_map_by_kind() {
        let e: Error = std::io::Error::from(std::io::ErrorKind::ConnectionReset).into();
        assert_eq!(e.kind, ErrorKind::Network);
        assert!(e.kind.is_transient());
    }
}
