//! Test helpers shared by Filewinch's protocol crates: throwaway local servers and a TCP
//! fault-injection proxy. Test-only; never a dependency of the app.
//!
//! # Local servers (`rclone serve`)
//!
//! [`Server::start`] runs `rclone serve sftp|ftp|webdav|s3` on `127.0.0.1` with a free port,
//! serving a fresh scratch folder ([`Server::root`]) with a throwaway user and password. It
//! waits until the port accepts connections and kills the server (and deletes the scratch
//! folder) on drop.
//!
//! ```no_run
//! use filewinch_testkit::{Kind, Server, test_base};
//! let srv = Server::start(Kind::WebDav, &test_base());
//! // srv.port, srv.user, srv.pass, srv.root (files placed here show up on the server)
//! ```
//!
//! What each kind gives you:
//! - [`Kind::Sftp`]: password auth (`user`/`pass`) and public-key auth with
//!   [`Server::client_key`] (an unencrypted ed25519 key whose public half is authorised).
//!   [`Server::host_key_pub`] is the server's host key line (`ssh-ed25519 AAAA...`).
//!   Use [`Server::start_with`] and [`Options::authorized_keys`] to authorise more keys
//!   (see [`ssh_keygen`]).
//! - [`Kind::Ftp`]: plain FTP; passive ports come from a private range so parallel servers
//!   don't collide.
//! - [`Kind::Ftps`]: FTP with **implicit** TLS (TLS from the first byte; that is what
//!   `rclone serve ftp --cert` does) using a fresh self-signed certificate for `127.0.0.1`
//!   ([`Server::cert_pem`]). rclone has no explicit (`AUTH TLS`) mode.
//! - [`Kind::WebDav`]: HTTP Basic auth with `user`/`pass`. [`Kind::WebDavs`] adds TLS with a
//!   self-signed certificate.
//! - [`Kind::S3`]: path-style S3 with access key `user` and secret `pass`, region
//!   `us-east-1`. Top-level folders under `root` are buckets: `srv.mkdir_bucket("b")`.
//!
//! The server log goes to `<scratch>/server.log`; [`Server::log`] reads it for assertions
//! and failure messages.
//!
//! Use [`test_base`] for scratch folders rather than `CARGO_TARGET_TMPDIR`: on this machine the
//! build directory is on an exFAT drive, where macOS litters folders with AppleDouble `._name`
//! files (they show up in server listings) and modification times are coarse.
//!
//! # Fault injection
//!
//! [`FaultProxy::start`] forwards a local port to a server and can cut connections after a
//! number of bytes (once, then it disarms), add latency per chunk, or cut everything now.
//! Point a client at `proxy.port` instead of the server's port. Note that FTP data
//! connections go straight to the server (PASV hands out the server's address), so for FTP
//! the proxy only affects the control connection.

mod proxy;

pub use proxy::FaultProxy;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Sftp,
    Ftp,
    Ftps,
    WebDav,
    WebDavs,
    S3,
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Extra `authorized_keys` lines for SFTP (the built-in client key is always included).
    pub authorized_keys: Vec<String>,
    /// Extra arguments passed to `rclone serve`.
    pub extra_args: Vec<String>,
}

/// A running `rclone serve`. Killed, and its scratch folder deleted, on drop.
pub struct Server {
    pub kind: Kind,
    pub port: u16,
    pub addr: SocketAddr,
    /// The folder the server serves.
    pub root: PathBuf,
    /// Scratch folder holding `root`, keys, certificates and `server.log`.
    pub scratch: PathBuf,
    pub user: String,
    pub pass: String,
    /// SFTP: unencrypted ed25519 private key authorised for `user`.
    pub client_key: Option<PathBuf>,
    /// SFTP: the server's public host key, as an OpenSSH line (`ssh-ed25519 AAAA...`).
    pub host_key_pub: Option<String>,
    /// TLS kinds: the self-signed certificate (PEM) the server presents.
    pub cert_pem: Option<PathBuf>,
    child: Child,
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A scratch base on the system temp volume (APFS), created if needed. Everything a test
/// puts here should be removed by the test (servers delete their scratch on drop).
pub fn test_base() -> PathBuf {
    let p = std::env::temp_dir().join("filewinch-tests");
    std::fs::create_dir_all(&p).expect("create test base");
    p
}

/// Creates a fresh, uniquely named folder under `base`.
pub fn scratch_dir(base: &Path, prefix: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let dir = base.join(format!("{prefix}-{}-{n}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// A port nobody is listening on right now.
pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0").local_addr().expect("local addr").port()
}

/// Generates an OpenSSH key pair with `ssh-keygen` (`kind`: "ed25519", "rsa", "ecdsa").
/// Returns the private key path; the public key is beside it with `.pub`.
pub fn ssh_keygen(dir: &Path, name: &str, kind: &str, passphrase: Option<&str>) -> PathBuf {
    let path = dir.join(name);
    let mut cmd = Command::new("ssh-keygen");
    cmd.args(["-q", "-t", kind, "-N", passphrase.unwrap_or(""), "-C", "filewinch-test", "-f"]).arg(&path);
    if kind == "rsa" {
        cmd.args(["-b", "2048"]);
    }
    let out = cmd.output().expect("run ssh-keygen");
    assert!(out.status.success(), "ssh-keygen failed: {}", String::from_utf8_lossy(&out.stderr));
    path
}

/// Writes a self-signed certificate and key for 127.0.0.1/localhost with `openssl`.
/// Returns (cert.pem, key.pem).
pub fn self_signed_cert(dir: &Path) -> (PathBuf, PathBuf) {
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    let out = Command::new("openssl")
        .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2", "-subj", "/CN=127.0.0.1"])
        .args(["-addext", "subjectAltName=IP:127.0.0.1,DNS:localhost", "-keyout"])
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .output()
        .expect("run openssl");
    assert!(out.status.success(), "openssl failed: {}", String::from_utf8_lossy(&out.stderr));
    (cert, key)
}

/// A run of `n` consecutive free ports for FTP passive mode. Ranges handed out in this process
/// never overlap (rclone binds them lazily, so a bind check alone can't keep two servers apart).
fn free_port_range(n: u16) -> (u16, u16) {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let base = 40000 + (std::process::id() % 64) as u32 * 320;
    for _ in 0..200 {
        let slot = NEXT.fetch_add(1, Ordering::Relaxed) % 20;
        let start = (base + slot * n as u32) as u16;
        if (start..start + n).all(|p| TcpListener::bind(("127.0.0.1", p)).is_ok()) {
            return (start, start + n - 1);
        }
    }
    panic!("no free passive port range");
}

impl Server {
    pub fn start(kind: Kind, base: &Path) -> Server {
        Self::start_with(kind, base, Options::default())
    }

    pub fn start_with(kind: Kind, base: &Path, opts: Options) -> Server {
        let scratch = scratch_dir(base, &format!("{kind:?}").to_lowercase());
        let root = scratch.join("root");
        std::fs::create_dir_all(&root).expect("create server root");
        let user = "fwtest".to_string();
        let pass = format!("pw-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed));
        let mut client_key = None;
        let mut host_key_pub = None;
        let mut cert_pem = None;
        let mut args: Vec<String> = vec!["serve".into()];
        let tls = |args: &mut Vec<String>, cert_pem: &mut Option<PathBuf>| {
            let (c, k) = self_signed_cert(&scratch);
            args.extend(["--cert".into(), c.display().to_string(), "--key".into(), k.display().to_string()]);
            *cert_pem = Some(c);
        };

        for attempt in 0..5 {
            let port = free_port();
            args.truncate(1);
            match kind {
                Kind::Sftp => {
                    let keys = scratch.join("keys");
                    std::fs::create_dir_all(&keys).expect("keys dir");
                    let host = keys.join("host_ed25519");
                    if !host.exists() {
                        ssh_keygen(&keys, "host_ed25519", "ed25519", None);
                        let ck = ssh_keygen(&keys, "client_ed25519", "ed25519", None);
                        let mut lines = vec![read_to_string(&ck.with_extension("pub"))];
                        lines.extend(opts.authorized_keys.iter().cloned());
                        std::fs::write(keys.join("authorized_keys"), lines.join("\n") + "\n").expect("write authorized_keys");
                        client_key = Some(ck);
                        host_key_pub = Some(read_to_string(&host.with_extension("pub")).trim().to_string());
                    }
                    args.extend(["sftp".into(), root.display().to_string()]);
                    args.extend(["--key".into(), host.display().to_string()]);
                    args.extend(["--authorized-keys".into(), keys.join("authorized_keys").display().to_string()]);
                }
                Kind::Ftp | Kind::Ftps => {
                    let (lo, hi) = free_port_range(16);
                    args.extend(["ftp".into(), root.display().to_string()]);
                    args.extend(["--passive-port".into(), format!("{lo}-{hi}"), "--public-ip".into(), "127.0.0.1".into()]);
                    if kind == Kind::Ftps && cert_pem.is_none() {
                        tls(&mut args, &mut cert_pem);
                    } else if let Some(c) = &cert_pem {
                        args.extend(["--cert".into(), c.display().to_string(), "--key".into(), scratch.join("key.pem").display().to_string()]);
                    }
                }
                Kind::WebDav | Kind::WebDavs => {
                    args.extend(["webdav".into(), root.display().to_string()]);
                    if kind == Kind::WebDavs && cert_pem.is_none() {
                        tls(&mut args, &mut cert_pem);
                    } else if let Some(c) = &cert_pem {
                        args.extend(["--cert".into(), c.display().to_string(), "--key".into(), scratch.join("key.pem").display().to_string()]);
                    }
                }
                Kind::S3 => {
                    args.extend(["s3".into(), root.display().to_string(), "--force-path-style".into()]);
                    args.extend(["--auth-key".into(), format!("{user},{pass}")]);
                }
            }
            args.extend(["--addr".into(), format!("127.0.0.1:{port}")]);
            if kind != Kind::S3 {
                args.extend(["--user".into(), user.clone(), "--pass".into(), pass.clone()]);
            }
            args.extend(opts.extra_args.iter().cloned());
            args.extend(["--log-file".into(), scratch.join("server.log").display().to_string(), "-v".into()]);

            let mut child = Command::new(rclone())
                .args(&args)
                .env("RCLONE_CONFIG", scratch.join("rclone.conf"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn rclone (is it installed at /usr/local/bin/rclone?)");
            let addr: SocketAddr = format!("127.0.0.1:{port}").parse().expect("valid addr");
            if wait_ready(&mut child, addr, kind, &scratch.join("server.log")) {
                return Server { kind, port, addr, root, scratch, user, pass, client_key, host_key_pub, cert_pem, child };
            }
            let _ = child.kill();
            let _ = child.wait();
            if attempt == 4 {
                let log = std::fs::read_to_string(scratch.join("server.log")).unwrap_or_default();
                panic!("rclone serve {kind:?} didn't start:\n{log}");
            }
        }
        unreachable!()
    }

    /// S3: creates a bucket (a top-level folder under `root`).
    pub fn mkdir_bucket(&self, name: &str) -> PathBuf {
        let p = self.root.join(name);
        std::fs::create_dir_all(&p).expect("create bucket dir");
        p
    }

    /// The server's log so far.
    pub fn log(&self) -> String {
        std::fs::read_to_string(self.scratch.join("server.log")).unwrap_or_default()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.scratch);
        }
    }
}

fn rclone() -> &'static str {
    if Path::new("/usr/local/bin/rclone").exists() { "/usr/local/bin/rclone" } else { "rclone" }
}

fn read_to_string(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// Waits until *this* server answers on `addr` (for SSH and FTP, until it sends its greeting).
///
/// Answering isn't enough on its own: `free_port` releases the port before rclone binds it, so a
/// server started at the same moment by another test or process can take it, ours then exits
/// with "address already in use", and the connection we just made reached the other server.
/// So we also require our child to be alive and its own log to name the port it bound.
fn wait_ready(child: &mut Child, addr: SocketAddr, kind: Kind, log: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    let port = format!(":{}", addr.port());
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return false; // exited: port taken or bad flags
        }
        let ours = std::fs::read_to_string(log).is_ok_and(|l| l.contains(&port) && !l.contains("address already in use"));
        if !ours {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        if let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
            match kind {
                Kind::Sftp | Kind::Ftp => {
                    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut buf = [0u8; 16];
                    if matches!(s.read(&mut buf), Ok(n) if n > 0) {
                        if kind == Kind::Sftp {
                            let _ = s.write_all(b"SSH-2.0-filewinch_probe\r\n");
                        }
                        return true;
                    }
                }
                _ => return true,
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}
