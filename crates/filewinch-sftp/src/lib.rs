//! Filewinch's SFTP backend.
//!
//! # Library choice
//!
//! Built on [`russh`] + [`russh_sftp`] (pure Rust), driven from a private multi-threaded tokio
//! runtime so the blocking [`Remote`] contract can sit on top. The alternative, `ssh2`
//! (libssh2), needs a C toolchain plus OpenSSL (system or vendored), which is the build pain
//! we want to avoid on macOS arm64; russh uses `ring` for crypto here, the same backend the
//! rest of the workspace's rustls stack uses. russh-sftp pipelines reads and writes (16
//! requests in flight per file), so a single file streams at close to link speed.
//!
//! # What it does
//!
//! - Auth: password (falling back to keyboard-interactive with the same password when the
//!   server asks for it), private key files (ed25519, RSA with SHA-2 signatures, ECDSA;
//!   passphrase from the system's secret store), ssh-agent (`SSH_AUTH_SOCK` on macOS and
//!   Linux; on Windows the OpenSSH agent pipe `\\.\pipe\openssh-ssh-agent`, then Pageant), and
//!   for [`Auth::None`] the agent then the default/configured identity files.
//! - `~/.ssh/config` (`%USERPROFILE%\.ssh\config` on Windows):
//!   Host/HostName/User/Port/IdentityFile/IdentitiesOnly ([`config`]).
//! - Host keys are checked against Filewinch's own known_hosts (in its config folder, see
//!   `filewinch_os::dirs`) and then `~/.ssh/known_hosts`
//!   (read only). An unknown or changed key fails with [`ErrorKind::HostTrust`] and the key's
//!   SHA-256 fingerprint (`SHA256:base64`) in `detail`; after the user agrees,
//!   [`trust_host_key`] records it and the next connect succeeds.
//! - A small pool of SSH connections (up to [`MAX_CONNECTIONS`]): each file transfer gets a
//!   connection of its own while one is free; listings share. Keepalives every 15 s; dead
//!   connections are dropped and replaced on the next call (metadata calls retry once on a
//!   fresh connection).
//! - Ranged reads for segmented downloads ([`MAX_SEGMENT_CONNECTIONS`]).
//! - Resume in both directions, chmod, mtime, symlinks (`stat` doesn't follow links and fills
//!   `link_target`), server-side rename, the home folder.

pub mod config;
mod errors;
mod hostkey;
mod remote;
mod session;

pub use hostkey::{filewinch_known_hosts_path, set_known_hosts_path, trust_host_key};

use std::sync::Arc;

use filewinch_core::{Connector, Protocol, Remote, Result, SecretRef, Secrets, Site};
#[cfg(doc)]
use filewinch_core::{Auth, ErrorKind};

/// SSH connections one SFTP site opens at most for ordinary transfers.
pub const MAX_CONNECTIONS: usize = 4;

/// SSH connections a site may grow to for segmented downloads; beyond it (or once the server
/// refuses another), pieces share connections, each with its own file handle.
pub const MAX_SEGMENT_CONNECTIONS: usize = 8;

/// Opens SFTP sessions for [`Protocol::Sftp`] sites.
pub struct SftpConnector;

impl Connector for SftpConnector {
    fn protocols(&self) -> &'static [Protocol] {
        &[Protocol::Sftp]
    }

    fn connect(&self, site: &Site, secrets: Arc<dyn Secrets>) -> Result<Box<dyn Remote>> {
        let secret = |r: &SecretRef| secrets.get(r);
        let secret = &secret;
        let params = session::Params::resolve(site, secret)?;
        Ok(Box::new(remote::SftpRemote::connect(params)?))
    }
}

pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("filewinch-sftp")
            .enable_all()
            .build()
            .expect("start the SFTP runtime")
    })
}
