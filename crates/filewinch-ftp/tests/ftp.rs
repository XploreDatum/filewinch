//! Integration tests against `rclone serve ftp` (plain and explicit TLS).

use base64::Engine as _;
use filewinch_core::{Auth, Connector, ErrorKind, Protocol, RPath, Remote, SecretRef, Site, TlsMode};
use filewinch_engine::{ConflictPolicy, Engine, EngineConfig, Event, Job, JobState, Op};
use filewinch_ftp::{FtpConnector, set_trusted_certs_path, trust_certificate};
use filewinch_local::{LocalRemote, rpath};
use filewinch_testkit::{FaultProxy, Kind, Server, scratch_dir};
use sha2::Digest;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

fn base() -> &'static Path {
    static B: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    B.get_or_init(filewinch_testkit::test_base)
}

fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = scratch_dir(base(), "ftp-trust");
        set_trusted_certs_path(Some(dir.join("trusted_certs")));
    });
}

fn site(port: u16, user: &str, protocol: Protocol) -> Site {
    let mut s = Site::new("t", "test", protocol);
    s.host = "127.0.0.1".into();
    s.port = Some(port);
    s.user = user.into();
    s.auth = Auth::Password { secret: SecretRef("pw".into()) };
    // rclone's FTPS is implicit TLS; explicit (AUTH TLS) is exercised by the "plain server"
    // case below, which must refuse cleanly.
    s.tls = if protocol == Protocol::Ftps { TlsMode::Implicit } else { TlsMode::Explicit };
    s
}

fn connect(s: &Site, pass: &str) -> filewinch_core::Result<Box<dyn Remote>> {
    init();
    let pass = pass.to_string();
    FtpConnector.connect(s, std::sync::Arc::new(filewinch_core::FnSecrets(move |_: &filewinch_core::SecretRef| Ok(Some(pass.clone())))))
}

fn data(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

fn run_job(job: Job) -> (JobState, Vec<Event>) {
    let (tx, rx) = channel();
    let tx = Mutex::new(tx);
    let engine = Engine::new(
        EngineConfig { retry_delay: Duration::from_millis(200), chunk: 64 * 1024, ..EngineConfig::default() },
        move |e| {
            let _ = tx.lock().unwrap().send(e);
        },
    );
    let id = engine.enqueue(job);
    let mut log = vec![];
    loop {
        let e = rx.recv_timeout(Duration::from_secs(120)).expect("engine event");
        log.push(e.clone());
        if let Event::State(j, s) = e
            && j == id && s.is_finished() {
                return (s, log);
            }
    }
}

#[test]
fn login_and_wrong_password() {
    let srv = Server::start(Kind::Ftp, base());
    let r = connect(&site(srv.port, &srv.user, Protocol::Ftp), &srv.pass).unwrap();
    assert!(r.list(&RPath::root()).unwrap().is_empty());
    let err = connect(&site(srv.port, &srv.user, Protocol::Ftp), "wrong").err().unwrap();
    assert_eq!(err.kind, ErrorKind::Auth, "{err}");
    assert_eq!(err.message, format!("The server refused the password for {}@127.0.0.1.", srv.user));
}

#[test]
fn unreachable_server_is_a_network_error() {
    let port = filewinch_testkit::free_port();
    let err = connect(&site(port, "u", Protocol::Ftp), "p").err().unwrap();
    assert_eq!(err.kind, ErrorKind::Network);
    assert!(err.message.contains("refused the connection"), "{}", err.message);
}

#[test]
fn file_operations() {
    let srv = Server::start(Kind::Ftp, base());
    let r = connect(&site(srv.port, &srv.user, Protocol::Ftp), &srv.pass).unwrap();
    let d = RPath::new("/dir one");
    r.mkdir(&d).unwrap();
    if let Err(e) = r.mkdir(&d) {
        assert_eq!(e.kind, ErrorKind::AlreadyExists, "{e}");
    }
    let f = d.join("a b.txt");
    let mut w = r.write(&f, 0, Some(5)).unwrap();
    w.write_all(b"hello").unwrap();
    w.finish().unwrap();
    assert_eq!(std::fs::read(srv.root.join("dir one/a b.txt")).unwrap(), b"hello");
    let e = r.stat(&f).unwrap();
    assert_eq!((e.name.as_str(), e.size, e.is_dir()), ("a b.txt", Some(5), false));
    assert!(e.modified.is_some());
    assert!(r.stat(&d).unwrap().is_dir());
    let list = r.list(&d).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].path, f);
    assert!(r.remove_dir(&d).is_err(), "non-empty folder");
    let g = d.join("b.txt");
    r.rename(&f, &g).unwrap();
    assert_eq!(r.stat(&f).unwrap_err().kind, ErrorKind::NotFound);
    let mut s = String::new();
    r.read(&g, 0).unwrap().read_to_string(&mut s).unwrap();
    assert_eq!(s, "hello");
    if r.caps().resume_read {
        let mut s = String::new();
        r.read(&g, 2).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "llo");
    }
    r.remove_file(&g).unwrap();
    r.remove_dir(&d).unwrap();
    assert_eq!(r.stat(&d).unwrap_err().kind, ErrorKind::NotFound);
    let e = r.read(&RPath::new("/missing"), 0).err().unwrap();
    assert_eq!(e.kind, ErrorKind::NotFound, "{e}");
    // a failed command leaves the pooled connection usable
    assert!(r.list(&RPath::root()).unwrap().is_empty());
    r.close();
    assert_eq!(r.list(&RPath::root()).unwrap_err().kind, ErrorKind::Closed);
}

#[test]
fn resume_write_at_offset() {
    let srv = Server::start(Kind::Ftp, base());
    let r = connect(&site(srv.port, &srv.user, Protocol::Ftp), &srv.pass).unwrap();
    let body = data(300_000, 3);
    let p = RPath::new("/r.bin");
    let mut w = r.write(&p, 0, None).unwrap();
    w.write_all(&body[..100_000]).unwrap();
    w.finish().unwrap();
    // A server that appends ends with the whole file; one that doesn't (rclone starts the file
    // over) must be reported as Unsupported so the engine resends it, never left corrupt.
    let res = r.write(&p, 100_000, None).and_then(|mut w| {
        w.write_all(&body[100_000..]).map_err(filewinch_core::Error::from)?;
        w.finish()
    });
    match res {
        Ok(()) => assert!(std::fs::read(srv.root.join("r.bin")).unwrap() == body),
        Err(e) => assert_eq!(e.kind, ErrorKind::Unsupported, "{e}"),
    }
    // Through the engine a resume that the server can't honour still ends byte-equal.
    let remote: Arc<dyn Remote> = Arc::from(r);
    let dir = scratch_dir(base(), "ftp-resume");
    std::fs::write(dir.join("r.bin"), &body).unwrap();
    std::fs::write(srv.root.join("r.bin"), &body[..100_000]).unwrap();
    let (st, _) = run_job(Job {
        op: Op::Copy,
        src: Arc::new(LocalRemote::new()),
        src_paths: vec![rpath(&dir.join("r.bin"))],
        dst: Some(remote),
        dst_dir: Some(RPath::root()),
        conflict: ConflictPolicy::ResumeOrOverwrite,
        label: "resume".into(),
        delete_shared_contents: false,
    });
    assert!(matches!(st, JobState::Done), "{st:?}");
    assert!(std::fs::read(srv.root.join("r.bin")).unwrap() == body);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn abandoned_download_keeps_pool_healthy() {
    let srv = Server::start(Kind::Ftp, base());
    std::fs::write(srv.root.join("big.bin"), data(8 << 20, 1)).unwrap();
    let r = connect(&site(srv.port, &srv.user, Protocol::Ftp), &srv.pass).unwrap();
    for _ in 0..6 {
        let mut rd = r.read(&RPath::new("/big.bin"), 0).unwrap();
        let mut buf = [0u8; 4096];
        rd.read_exact(&mut buf).unwrap();
        drop(rd); // abort mid-transfer
    }
    assert_eq!(r.list(&RPath::root()).unwrap().len(), 1);
}

#[test]
fn upload_and_download_1mb_and_50mb_byte_equal() {
    let srv = Server::start(Kind::Ftp, base());
    let remote: Arc<dyn Remote> = Arc::from(connect(&site(srv.port, &srv.user, Protocol::Ftp), &srv.pass).unwrap());
    let local: Arc<dyn Remote> = Arc::new(LocalRemote::new());
    let dir = scratch_dir(base(), "ftp-xfer");
    std::fs::create_dir_all(dir.join("up/sub")).unwrap();
    std::fs::write(dir.join("up/one.bin"), data(1 << 20, 1)).unwrap();
    std::fs::write(dir.join("up/sub/fifty.bin"), data(50 << 20, 2)).unwrap();
    let up = Job {
        op: Op::Copy,
        src: local.clone(),
        src_paths: vec![rpath(&dir.join("up"))],
        dst: Some(remote.clone()),
        dst_dir: Some(RPath::root()),
        conflict: ConflictPolicy::Overwrite,
        label: "up".into(),
        delete_shared_contents: false,
    };
    let (st, _) = run_job(up);
    assert!(matches!(st, JobState::Done), "{st:?}");
    for f in ["one.bin", "sub/fifty.bin"] {
        assert!(std::fs::read(dir.join("up").join(f)).unwrap() == std::fs::read(srv.root.join("up").join(f)).unwrap(), "{f}");
    }
    std::fs::create_dir_all(dir.join("down")).unwrap();
    let down = Job {
        op: Op::Copy,
        src: remote,
        src_paths: vec![RPath::new("/up")],
        dst: Some(local),
        dst_dir: Some(rpath(&dir.join("down"))),
        conflict: ConflictPolicy::Overwrite,
        label: "down".into(),
        delete_shared_contents: false,
    };
    let (st, _) = run_job(down);
    assert!(matches!(st, JobState::Done), "{st:?}");
    for f in ["one.bin", "sub/fifty.bin"] {
        assert!(std::fs::read(dir.join("up").join(f)).unwrap() == std::fs::read(dir.join("down/up").join(f)).unwrap(), "{f}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn download_survives_control_connection_drop() {
    let srv = Server::start(Kind::Ftp, base());
    let body = data(16 << 20, 4);
    std::fs::write(srv.root.join("d.bin"), &body).unwrap();
    let proxy = FaultProxy::start(srv.addr);
    // Slow the control connection a little so the cut lands mid-transfer.
    let remote: Arc<dyn Remote> = Arc::from(connect(&site(proxy.port, &srv.user, Protocol::Ftp), &srv.pass).unwrap());
    let local: Arc<dyn Remote> = Arc::new(LocalRemote::new());
    let dir = scratch_dir(base(), "ftp-drop");
    let (tx, rx) = channel();
    let tx = Mutex::new(tx);
    let engine = Engine::new(
        EngineConfig { retry_delay: Duration::from_millis(200), chunk: 32 * 1024, bandwidth_limit: Some(16 << 20), ..EngineConfig::default() },
        move |e| {
            let _ = tx.lock().unwrap().send(e);
        },
    );
    let id = engine.enqueue(Job {
        op: Op::Copy,
        src: remote,
        src_paths: vec![RPath::new("/d.bin")],
        dst: Some(local),
        dst_dir: Some(rpath(&dir)),
        conflict: ConflictPolicy::Overwrite,
        label: "drop".into(),
        delete_shared_contents: false,
    });
    let mut cut = false;
    let st = loop {
        match rx.recv_timeout(Duration::from_secs(120)).unwrap() {
            Event::Progress(_, p) if !cut && p.bytes_done > 4 << 20 => {
                proxy.cut_all();
                cut = true;
            }
            Event::State(j, s) if j == id && s.is_finished() => break s,
            _ => {}
        }
    };
    assert!(cut);
    assert!(matches!(st, JobState::Done), "{st:?}");
    assert!(std::fs::read(dir.join("d.bin")).unwrap() == body);
    let _ = std::fs::remove_dir_all(dir);
}

fn pem_fingerprint(pem: &Path) -> String {
    let text = std::fs::read_to_string(pem).unwrap();
    let b64: String = text.lines().skip_while(|l| !l.starts_with("-----BEGIN CERT")).skip(1).take_while(|l| !l.starts_with("-----END")).collect();
    let der = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
    format!("SHA256:{}", base64::engine::general_purpose::STANDARD_NO_PAD.encode(sha2::Sha256::digest(der)))
}

#[test]
fn ftps_self_signed_is_host_trust_then_trusted() {
    let srv = Server::start(Kind::Ftps, base());
    let s = site(srv.port, &srv.user, Protocol::Ftps);
    let err = connect(&s, &srv.pass).err().expect("self-signed must fail");
    assert_eq!(err.kind, ErrorKind::HostTrust, "{err}");
    let fp = err.fingerprint().expect("detail starts with the fingerprint");
    assert!(err.detail.as_deref().unwrap().starts_with(&fp));
    assert_eq!(fp, pem_fingerprint(srv.cert_pem.as_ref().unwrap()));
    // The site's own trusted fingerprints (any accepted form) are enough on their own.
    let mut pinned = s.clone();
    pinned.trusted_fingerprints = vec![fp.replace("SHA256:", "sha256:")];
    connect(&pinned, &srv.pass).expect("site-trusted certificate connects").close();
    pinned.trusted_fingerprints = vec!["SHA256:nThbg6kXUpJWGl7E1IGOCspRomTxdCARLviKw6E5SY8".into()];
    assert_eq!(connect(&pinned, &srv.pass).err().unwrap().kind, ErrorKind::HostTrust);
    // The process-wide fallback still works for callers without a saved site.
    trust_certificate("127.0.0.1", srv.port, err.detail.as_deref().unwrap()).unwrap();
    let r = connect(&s, &srv.pass).unwrap();
    let p = RPath::new("/tls.bin");
    let body = data(2 << 20, 6);
    let mut w = r.write(&p, 0, None).unwrap();
    w.write_all(&body).unwrap();
    w.finish().unwrap();
    let mut back = Vec::new();
    r.read(&p, 0).unwrap().read_to_end(&mut back).unwrap();
    assert!(back == body);
    assert_eq!(r.list(&RPath::root()).unwrap().len(), 1);
    // Explicit FTPS against a server without TLS: AUTH TLS refused, a clear error.
    let plain = Server::start(Kind::Ftp, base());
    let mut explicit = site(plain.port, &plain.user, Protocol::Ftps);
    explicit.tls = TlsMode::Explicit;
    let err = connect(&explicit, &plain.pass).err().unwrap();
    assert!(matches!(err.kind, ErrorKind::Unsupported | ErrorKind::Network), "{err}");
}
