//! End-to-end tests: the app's own connector registry and the real transfer engine against
//! local `rclone serve` servers (SFTP, FTP, WebDAV, S3), with This Mac (`filewinch_local`) as
//! the other side. One shared body ([`flow::round_trip`]) runs for every protocol.
//!
//! Sites are built the way the connect sheet builds them, secrets come from
//! [`MemorySecrets`] (never the Keychain), and SFTP's unknown host key goes through the trust
//! path: the first connect fails with `HostTrust`, its fingerprint goes into
//! `Site::trusted_fingerprints`, and the second connect succeeds. Filewinch's own known_hosts
//! is redirected to the test's scratch folder so nothing under `~/Library` is read or written.

mod flow;
mod harness;
mod tests;
mod tree;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use filewinch_core::{Auth, Error, ErrorKind, MemorySecrets, Protocol, RPath, Remote, SecretRef, Site};
use filewinch_testkit::{Kind, Server, scratch_dir, test_base};

use crate::registry;

const SECRET: &str = "e2e/password";

/// A file placed on the server's disk before connecting (servers cache listings, so it goes
/// there first); the round trip expects to see it listed.
pub const SEEDED: (&str, &[u8]) = ("seeded.txt", b"placed on the server's disk\n");
/// A nested folder placed on the server's disk the same way ([`tree::write_server_tree`]).
pub const SERVER_TREE: &str = "from-server";

/// True when `rclone` can run; tests skip (with a message) when it can't.
pub fn rclone_available() -> bool {
    let bin = if Path::new("/usr/local/bin/rclone").exists() { "/usr/local/bin/rclone" } else { "rclone" };
    std::process::Command::new(bin).arg("version").output().is_ok_and(|o| o.status.success())
}

pub fn protocol(kind: Kind) -> Protocol {
    match kind {
        Kind::Sftp => Protocol::Sftp,
        Kind::Ftp => Protocol::Ftp,
        Kind::Ftps => Protocol::Ftps,
        Kind::WebDav => Protocol::WebDav,
        Kind::WebDavs => Protocol::WebDavs,
        Kind::S3 => Protocol::S3,
    }
}

/// The site the connect sheet would save for this server.
pub fn site_for(srv: &Server, bucket: Option<&str>) -> Site {
    let p = protocol(srv.kind);
    let mut site = Site::new(format!("e2e-{p:?}").to_lowercase(), format!("rclone {}", p.label()), p);
    site.host = if p == Protocol::S3 { "http://127.0.0.1".into() } else { "127.0.0.1".into() };
    site.port = Some(srv.port);
    site.user = srv.user.clone();
    site.auth = Auth::Password { secret: SecretRef(SECRET.into()) };
    if p == Protocol::S3 {
        site.path_style = true;
        site.region = Some("us-east-1".into());
        site.initial_path = bucket.map(str::to_owned);
    }
    site
}

pub fn secrets(pass: &str) -> Arc<MemorySecrets> {
    Arc::new(MemorySecrets::new().with(SECRET, pass))
}

/// Connects through the app's registry, as `connect::flow` does.
pub fn connect(site: &Site, pass: &str) -> Result<Arc<dyn Remote>, Error> {
    let connector = registry::connector(site.protocol).expect("this build has a connector for the protocol");
    connector.connect(site, secrets(pass)).map(Arc::from)
}

/// Where the session opens: `initial_path` if it is a folder, else home (as `connect::flow`).
pub fn start_dir(remote: &dyn Remote, site: &Site) -> RPath {
    let home = remote.home().expect("home folder");
    match site.initial_path.as_deref().filter(|p| !p.trim().is_empty()) {
        Some(p) => {
            let p = if p.starts_with('/') { RPath::new(p) } else { home.join(p) };
            match remote.stat(&p) {
                Ok(e) if e.is_dir() => p,
                _ => home,
            }
        }
        None => home,
    }
}

/// Points Filewinch's own known_hosts at a scratch file, once per test process.
pub fn isolate_known_hosts() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let dir = scratch_dir(&test_base(), "e2e-known-hosts");
        filewinch_sftp::set_known_hosts_path(Some(dir.join("known_hosts")));
    });
}

/// A started server with a connected session, after the trust prompt where there is one.
pub struct Live {
    pub srv: Server,
    /// The folder on the server's disk that `home` shows.
    pub server_dir: PathBuf,
    pub site: Site,
    pub remote: Arc<dyn Remote>,
    pub home: RPath,
    /// False for `rclone serve s3`: it stores a folder marker (`name/`) as a zero-byte *file*
    /// `name`, so uploads into that folder then fail (its docs: "you can't create empty
    /// folders under other folders not in the root"). Real S3 and MinIO keep markers. The S3
    /// round trip therefore uploads a flat folder as a new bucket, and covers nested folders
    /// by downloading and deleting [`SERVER_TREE`].
    pub folder_markers: bool,
}

impl Live {
    pub fn start(kind: Kind) -> Live {
        isolate_known_hosts();
        let srv = Server::start(kind, &test_base());
        let bucket = (kind == Kind::S3).then(|| {
            srv.mkdir_bucket("e2e-bucket");
            "e2e-bucket"
        });
        let server_dir = bucket.map_or(srv.root.clone(), |b| srv.root.join(b));
        std::fs::write(server_dir.join(SEEDED.0), SEEDED.1).expect("seed a file on the server");
        tree::write_server_tree(&server_dir.join(SERVER_TREE));
        let mut site = site_for(&srv, bucket);
        let remote = match connect(&site, &srv.pass) {
            Ok(r) => {
                assert_ne!(kind, Kind::Sftp, "SFTP must not trust an unknown host key without asking");
                r
            }
            Err(e) if e.kind == ErrorKind::HostTrust => {
                let fp = e.fingerprint().unwrap_or_else(|| panic!("HostTrust error without a fingerprint: {e:?}"));
                assert!(fp.starts_with("SHA256:"), "fingerprint {fp:?} isn't canonical");
                site.trusted_fingerprints.push(fp);
                connect(&site, &srv.pass).unwrap_or_else(|e| panic!("connect after trusting the host key: {e:?}\n{}", srv.log()))
            }
            Err(e) => panic!("connect to rclone {kind:?}: {e:?}\n{}", srv.log()),
        };
        let home = start_dir(&*remote, &site);
        if let Some(b) = bucket {
            assert_eq!(home, RPath::new(b), "an S3 site with a bucket opens in the bucket");
        }
        Live { srv, server_dir, site, remote, home, folder_markers: kind != Kind::S3 }
    }
}

/// A fresh local scratch folder, removed on drop (kept when the test fails, for a look).
pub struct LocalDir(pub PathBuf);

impl LocalDir {
    pub fn new(prefix: &str) -> LocalDir {
        LocalDir(scratch_dir(&test_base(), prefix))
    }

    pub fn rpath(&self) -> RPath {
        filewinch_local::rpath(&self.0)
    }
}

impl Drop for LocalDir {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
