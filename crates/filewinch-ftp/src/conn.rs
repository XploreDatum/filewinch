//! Opening and logging in one FTP control connection, feature detection, error mapping.

use crate::tls;
use filewinch_core::{Auth, Error, ErrorKind, Protocol, Result, SecretRef, Site, TlsMode};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;
use suppaftp::types::FileType;
use suppaftp::{FtpError, RustlsConnector, RustlsFtpStream, Status};

pub(crate) type Ftp = RustlsFtpStream;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Read/write timeout on control and data sockets: a stalled server becomes a Network error.
const IO_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub(crate) struct Params {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub pass: String,
    pub tls: Option<TlsMode>,
    pub passive: bool,
    /// The site's "always trust" certificate fingerprints.
    pub trusted: Vec<String>,
}

impl Params {
    pub fn resolve(site: &Site, secret: &dyn Fn(&SecretRef) -> Result<Option<String>>) -> Result<Params> {
        let host = site.host.trim().to_string();
        if host.is_empty() {
            return Err(Error::new(ErrorKind::Other, "Enter a server address."));
        }
        let tls = (site.protocol == Protocol::Ftps).then_some(site.tls);
        let default_port = if tls == Some(TlsMode::Implicit) { 990 } else { 21 };
        let (user, pass) = match &site.auth {
            Auth::Password { secret: s } => {
                let user = site.user.trim().to_string();
                match secret(s)? {
                    Some(p) => (user, p),
                    None => return Err(Error::new(ErrorKind::Auth, format!("There's no saved password for {user}@{host}."))),
                }
            }
            _ if site.user.trim().is_empty() || site.user.trim() == "anonymous" => ("anonymous".into(), "filewinch@".into()),
            _ => (site.user.trim().to_string(), String::new()),
        };
        Ok(Params { port: site.port.unwrap_or(default_port), host, user, pass, tls, passive: site.passive, trusted: site.trusted_fingerprints.clone() })
    }

    pub fn who(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Features {
    pub mlsd: bool,
    pub mfmt: bool,
    pub mdtm: bool,
    pub rest: bool,
    pub unix: bool,
}

/// Maps an FTP error. `what` is the attempted action ("delete “/a.txt”").
pub(crate) fn ftp_err(e: FtpError, what: &str, host: &str) -> Error {
    match e {
        FtpError::ConnectionError(io) => {
            let kind = match Error::from(std::io::Error::new(io.kind(), "")).kind {
                ErrorKind::Other => ErrorKind::Network,
                k => k,
            };
            Error::new(kind, format!("The connection to {host} dropped while trying to {what}.")).with_detail(io)
        }
        FtpError::SecureError(s) => Error::new(ErrorKind::Network, format!("The secure connection to {host} failed.")).with_detail(s),
        FtpError::UnexpectedResponse(r) => {
            let text = r.as_string().unwrap_or_default();
            let lower = text.to_ascii_lowercase();
            let code = r.status.code();
            let (kind, msg) = match code {
                530 | 430 | 332 | 532 => (ErrorKind::Auth, format!("{host} refused the login.")),
                421 | 425 | 426 | 434 | 450 | 451 => (ErrorKind::Network, format!("{host} couldn't {what} right now.")),
                452 | 552 => (ErrorKind::NoSpace, format!("{host} is out of space: couldn't {what}.")),
                553 => (ErrorKind::InvalidPath, format!("{host} doesn't allow that name: couldn't {what}.")),
                500 | 502 | 504 => (ErrorKind::Unsupported, format!("{host} doesn't support this: couldn't {what}.")),
                550 if lower.contains("permission") || lower.contains("denied") || lower.contains("not allowed") => {
                    (ErrorKind::PermissionDenied, format!("Couldn't {what}: permission denied."))
                }
                550 if lower.contains("exist") && !lower.contains("not exist") && !lower.contains("no such") => {
                    (ErrorKind::AlreadyExists, format!("Couldn't {what}: it already exists."))
                }
                550 if lower.contains("not empty") => (ErrorKind::InvalidPath, format!("Couldn't {what}: the folder isn't empty.")),
                550 | 551 => (ErrorKind::NotFound, format!("Couldn't {what}: it doesn't exist or isn't available.")),
                _ => (ErrorKind::Other, format!("{host} couldn't {what}.")),
            };
            Error::new(kind, msg).with_detail(format!("{code} {}", text.trim()))
        }
        FtpError::BadResponse => Error::new(ErrorKind::Network, format!("{host} sent a reply Filewinch couldn't read while trying to {what}.")),
        FtpError::InvalidAddress(a) => Error::new(ErrorKind::Network, format!("{host} gave an invalid data address.")).with_detail(a),
        FtpError::DataConnectionAlreadyOpen => Error::new(ErrorKind::Other, format!("Couldn't {what}: a transfer is already running on this connection.")),
    }
}

fn data_stream(addr: SocketAddr) -> suppaftp::FtpResult<TcpStream> {
    let s = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(FtpError::ConnectionError)?;
    let _ = s.set_read_timeout(Some(IO_TIMEOUT));
    let _ = s.set_write_timeout(Some(IO_TIMEOUT));
    let _ = s.set_nodelay(true);
    Ok(s)
}

/// Opens, secures and logs in one control connection.
pub(crate) fn open(p: &Params) -> Result<(Ftp, Features)> {
    let who = if p.port == 21 { p.host.clone() } else { format!("{}:{}", p.host, p.port) };
    let addr = (p.host.as_str(), p.port)
        .to_socket_addrs()
        .map_err(|e| Error::new(ErrorKind::Network, format!("Couldn't find the server {}. Check the address.", p.host)).with_detail(e))?
        .next()
        .ok_or_else(|| Error::new(ErrorKind::Network, format!("Couldn't find the server {}.", p.host)))?;
    let connect_err = |e: FtpError| match e {
        FtpError::ConnectionError(io) if io.kind() == std::io::ErrorKind::ConnectionRefused => {
            Error::new(ErrorKind::Network, format!("{who} refused the connection. Is FTP running on that port?")).with_detail(io)
        }
        FtpError::ConnectionError(io) if io.kind() == std::io::ErrorKind::TimedOut => {
            Error::new(ErrorKind::Network, format!("{who} didn't answer in time.")).with_detail(io)
        }
        e => ftp_err(e, "connect", &who),
    };

    let tls_cfg = match p.tls {
        Some(_) => Some(tls::client_config(&p.host, p.port, &p.trusted)?),
        None => None,
    };
    let trust = |e: FtpError, verdict: &std::sync::Mutex<Option<Error>>| {
        verdict.lock().unwrap_or_else(|x| x.into_inner()).take().unwrap_or_else(|| connect_err(e))
    };

    let mut ftp = match (&p.tls, &tls_cfg) {
        (Some(TlsMode::Implicit), Some((cfg, verdict))) => {
            Ftp::connect_secure_implicit(addr, RustlsConnector::from(cfg.clone()), &p.host).map_err(|e| trust(e, verdict))?
        }
        _ => {
            let s = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| connect_err(FtpError::ConnectionError(e)))?;
            let _ = s.set_read_timeout(Some(IO_TIMEOUT));
            let _ = s.set_write_timeout(Some(IO_TIMEOUT));
            let ftp = Ftp::connect_with_stream(s).map_err(connect_err)?;
            match &tls_cfg {
                Some((cfg, verdict)) => ftp.into_secure(RustlsConnector::from(cfg.clone()), &p.host).map_err(|e| match e {
                    FtpError::UnexpectedResponse(_) if verdict.lock().unwrap_or_else(|x| x.into_inner()).is_none() => {
                        Error::new(ErrorKind::Unsupported, format!("{who} doesn't offer FTP over TLS (AUTH TLS was refused).")).with_detail(e)
                    }
                    e => trust(e, verdict),
                })?,
                None => ftp,
            }
        }
    };
    ftp = if p.passive { ftp.passive_stream_builder(data_stream) } else { ftp.active_mode(Duration::from_secs(30)) };
    ftp.set_passive_nat_workaround(true);

    ftp.login(p.user.as_str(), p.pass.as_str()).map_err(|e| match e {
        FtpError::UnexpectedResponse(r) if matches!(r.status.code(), 530 | 430 | 331 | 332) => {
            let msg = if p.user == "anonymous" {
                format!("{} doesn't allow anonymous login.", p.host)
            } else {
                format!("The server refused the password for {}.", p.who())
            };
            Error::new(ErrorKind::Auth, msg).with_detail(r.as_string().unwrap_or_default())
        }
        e => ftp_err(e, "log in", &who),
    })?;

    let mut f = Features::default();
    if let Ok(feat) = ftp.feat() {
        for (k, v) in &feat {
            match k.to_ascii_uppercase().as_str() {
                "MLST" | "MLSD" => f.mlsd = true,
                "MFMT" => f.mfmt = true,
                "MDTM" => f.mdtm = true,
                "REST" => f.rest = v.as_deref().is_none_or(|v| v.to_ascii_uppercase().contains("STREAM")),
                "UTF8" => {
                    let _ = ftp.opts("UTF8", Some("ON"));
                }
                _ => {}
            }
        }
    }
    if let Ok(r) = ftp.custom_command("SYST", &[Status::Name]) {
        f.unix = r.as_string().unwrap_or_default().to_ascii_uppercase().contains("UNIX");
    }
    ftp.transfer_type(FileType::Binary).map_err(|e| ftp_err(e, "switch to binary mode", &who))?;
    Ok((ftp, f))
}
