//! Integration tests against `rclone serve sftp` (see filewinch-testkit).

use filewinch_core::{Auth, Connector, ErrorKind, Protocol, RPath, Remote, SecretRef, Site};
use filewinch_engine::{ConflictPolicy, Engine, EngineConfig, Event, Job, JobState, Op};
use filewinch_local::{LocalRemote, rpath};
use filewinch_sftp::{SftpConnector, set_known_hosts_path, trust_host_key};
use filewinch_testkit::{FaultProxy, Kind, Options, Server, scratch_dir, ssh_keygen};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn base() -> &'static Path {
    static B: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    B.get_or_init(filewinch_testkit::test_base)
}

/// One known_hosts file for this test process; trust changes are serialised.
static TRUST: Mutex<()> = Mutex::new(());

fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = scratch_dir(base(), "sftp-known-hosts");
        set_known_hosts_path(Some(dir.join("known_hosts")));
    });
}

fn site(port: u16, user: &str, auth: Auth) -> Site {
    let mut s = Site::new("t", "test", Protocol::Sftp);
    s.host = "127.0.0.1".into();
    s.port = Some(port);
    s.user = user.into();
    s.auth = auth;
    s
}

fn pw_auth() -> Auth {
    Auth::Password { secret: SecretRef("pw".into()) }
}

fn connect_raw(s: &Site, secret: Option<String>) -> filewinch_core::Result<Box<dyn Remote>> {
    SftpConnector.connect(s, std::sync::Arc::new(filewinch_core::FnSecrets(move |_: &filewinch_core::SecretRef| Ok(secret.clone()))))
}

/// Connects, trusting the host key on first sight (what the app does after the user agrees).
fn connect(s: &Site, secret: Option<String>) -> Box<dyn Remote> {
    init();
    match connect_raw(s, secret.clone()) {
        Ok(r) => r,
        Err(e) if e.kind == ErrorKind::HostTrust => {
            let _g = TRUST.lock().unwrap();
            trust_host_key("127.0.0.1", s.port.unwrap(), e.detail.as_deref().unwrap()).unwrap();
            connect_raw(s, secret).unwrap()
        }
        Err(e) => panic!("connect failed: {e}"),
    }
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

fn fingerprint_of(line: &str) -> String {
    let b64 = line.split_whitespace().nth(1).unwrap();
    let k = russh::keys::parse_public_key_base64(b64).unwrap();
    k.fingerprint(russh::keys::HashAlg::Sha256).to_string()
}

#[test]
fn unknown_host_key_is_reported_then_trusted() {
    init();
    let srv = Server::start(Kind::Sftp, base());
    let s = site(srv.port, &srv.user, pw_auth());
    let err = connect_raw(&s, Some(srv.pass.clone())).err().expect("unknown key must fail");
    assert_eq!(err.kind, ErrorKind::HostTrust, "{err}");
    let fp = err.fingerprint().expect("detail starts with the fingerprint");
    assert!(err.detail.as_deref().unwrap().starts_with(&fp));
    assert_eq!(fp, fingerprint_of(srv.host_key_pub.as_ref().unwrap()));
    assert!(err.message.contains("hasn't seen"), "{}", err.message);

    // The site's own trusted fingerprints are enough, without touching known_hosts.
    let mut pinned = s.clone();
    pinned.trusted_fingerprints = vec![fp.clone()];
    let r = connect_raw(&pinned, Some(srv.pass.clone())).expect("site-trusted key connects");
    r.close();
    pinned.trusted_fingerprints = vec!["SHA256:nThbg6kXUpJWGl7E1IGOCspRomTxdCARLviKw6E5SY8".into()];
    assert_eq!(connect_raw(&pinned, Some(srv.pass.clone())).err().unwrap().kind, ErrorKind::HostTrust);

    let _g = TRUST.lock().unwrap();
    let wrong = trust_host_key("127.0.0.1", srv.port, "SHA256:nope").unwrap_err();
    assert_eq!(wrong.kind, ErrorKind::HostTrust);
    trust_host_key("127.0.0.1", srv.port, &fp).unwrap();
    drop(_g);
    let r = connect_raw(&s, Some(srv.pass.clone())).expect("trusted now");
    assert!(r.list(&RPath::root()).unwrap().is_empty());
}

#[test]
fn changed_host_key_is_reported() {
    init();
    let srv = Server::start(Kind::Sftp, base());
    let s = site(srv.port, &srv.user, pw_auth());
    drop(connect(&s, Some(srv.pass.clone())));
    // Record a different ed25519 key for this host:port, as if the server was reinstalled.
    let other = ssh_keygen(&srv.scratch, "other_host", "ed25519", None);
    let line = std::fs::read_to_string(other.with_extension("pub")).unwrap();
    let path = filewinch_sftp::filewinch_known_hosts_path();
    {
        let _g = TRUST.lock().unwrap();
        let old = std::fs::read_to_string(&path).unwrap();
        let label = format!("[127.0.0.1]:{}", srv.port);
        let mut kept: Vec<String> = old.lines().filter(|l| !l.starts_with(&label)).map(String::from).collect();
        kept.push(format!("{label} {}", line.trim()));
        std::fs::write(&path, kept.join("\n") + "\n").unwrap();
    }
    let err = connect_raw(&s, Some(srv.pass.clone())).err().expect("changed key must fail");
    assert_eq!(err.kind, ErrorKind::HostTrust);
    assert!(err.message.contains("changed"), "{}", err.message);
    // Trusting the new key replaces the old line.
    {
        let _g = TRUST.lock().unwrap();
        trust_host_key("127.0.0.1", srv.port, err.detail.as_deref().unwrap()).unwrap();
    }
    connect_raw(&s, Some(srv.pass.clone())).expect("re-trusted");
}

#[test]
fn password_auth_and_wrong_password() {
    let srv = Server::start(Kind::Sftp, base());
    let s = site(srv.port, &srv.user, pw_auth());
    let r = connect(&s, Some(srv.pass.clone()));
    assert!(r.home().unwrap().is_root() || !r.home().unwrap().as_str().is_empty());
    let err = connect_raw(&s, Some("wrong".into())).err().unwrap();
    assert_eq!(err.kind, ErrorKind::Auth);
    assert_eq!(err.message, format!("The server refused the password for {}@127.0.0.1.", srv.user));
    let err = connect_raw(&s, None).err().unwrap();
    assert_eq!(err.kind, ErrorKind::Auth, "no saved password");
}

#[test]
fn key_auth_ed25519_rsa_ecdsa_with_passphrases() {
    let keys = scratch_dir(base(), "client-keys");
    let rsa = ssh_keygen(&keys, "id_rsa", "rsa", Some("rsa pass"));
    let ecdsa = ssh_keygen(&keys, "id_ecdsa", "ecdsa", Some("ec pass"));
    let pubs = [&rsa, &ecdsa].iter().map(|k| std::fs::read_to_string(k.with_extension("pub")).unwrap()).collect();
    let srv = Server::start_with(Kind::Sftp, base(), Options { authorized_keys: pubs, ..Default::default() });
    let key_auth = |p: &PathBuf, pass: bool| Auth::KeyFile {
        path: p.display().to_string(),
        passphrase: pass.then(|| SecretRef("kp".into())),
    };
    let ed = srv.client_key.clone().unwrap();
    let r = connect(&site(srv.port, &srv.user, key_auth(&ed, false)), None);
    r.list(&RPath::root()).unwrap();
    connect(&site(srv.port, &srv.user, key_auth(&rsa, true)), Some("rsa pass".into()));
    connect(&site(srv.port, &srv.user, key_auth(&ecdsa, true)), Some("ec pass".into()));

    let err = connect_raw(&site(srv.port, &srv.user, key_auth(&rsa, true)), Some("bad".into())).err().unwrap();
    assert_eq!(err.kind, ErrorKind::Auth);
    assert!(err.message.contains("passphrase"), "{}", err.message);
    let err = connect_raw(&site(srv.port, &srv.user, key_auth(&rsa, false)), None).err().unwrap();
    assert_eq!(err.kind, ErrorKind::Auth);
    assert!(err.message.contains("protected by a passphrase"), "{}", err.message);
    // A key the server doesn't know.
    let stranger = ssh_keygen(&keys, "id_stranger", "ed25519", None);
    let err = connect_raw(&site(srv.port, &srv.user, key_auth(&stranger, false)), None).err().unwrap();
    assert_eq!(err.kind, ErrorKind::Auth);
    assert!(err.message.contains("didn't accept the key"), "{}", err.message);
    let _ = std::fs::remove_dir_all(keys);
}

#[test]
fn file_operations() {
    let srv = Server::start(Kind::Sftp, base());
    let r = connect(&site(srv.port, &srv.user, pw_auth()), Some(srv.pass.clone()));
    let d = RPath::new("/dir one");
    r.mkdir(&d).unwrap();
    // OpenSSH fails a second mkdir (AlreadyExists); rclone's server treats it as done.
    if let Err(e) = r.mkdir(&d) {
        assert_eq!(e.kind, ErrorKind::AlreadyExists);
    }
    let f = d.join("a.txt");
    let mut w = r.write(&f, 0, Some(5)).unwrap();
    w.write_all(b"hello").unwrap();
    w.finish().unwrap();
    assert_eq!(std::fs::read(srv.root.join("dir one/a.txt")).unwrap(), b"hello");

    let e = r.stat(&f).unwrap();
    assert_eq!((e.name.as_str(), e.size, e.is_dir()), ("a.txt", Some(5), false));
    assert!(r.stat(&d).unwrap().is_dir());
    let list = r.list(&d).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].path, f);

    let when = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    r.set_mtime(&f, when).unwrap();
    assert_eq!(r.stat(&f).unwrap().modified, Some(when));

    assert_eq!(r.remove_dir(&d).unwrap_err().kind, ErrorKind::InvalidPath);
    let g = d.join("b.txt");
    r.rename(&f, &g).unwrap();
    assert_eq!(r.stat(&f).unwrap_err().kind, ErrorKind::NotFound);
    let mut s = String::new();
    r.read(&g, 1).unwrap().read_to_string(&mut s).unwrap();
    assert_eq!(s, "ello");
    r.remove_file(&g).unwrap();
    r.remove_dir(&d).unwrap();
    assert_eq!(r.list(&d).unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(r.read(&RPath::new("/missing"), 0).err().unwrap().kind, ErrorKind::NotFound);
    assert_eq!(r.remove_file(&RPath::new("/missing")).unwrap_err().kind, ErrorKind::NotFound);
    r.close();
    assert_eq!(r.list(&RPath::root()).unwrap_err().kind, ErrorKind::Closed);
}

/// Waits for rclone's VFS to write a closed file back to its root.
fn wait_for_file(p: &Path, len: usize) -> Vec<u8> {
    let end = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < end {
        if let Ok(b) = std::fs::read(p)
            && b.len() == len {
                return b;
            }
        std::thread::sleep(Duration::from_millis(50));
    }
    std::fs::read(p).unwrap_or_default()
}

/// Resuming an upload needs the server to keep the partial file. rclone's server truncates on
/// every write-open, so the backend must report Unsupported (never write after a hole); reads
/// at an offset work.
#[test]
fn resume_write_is_refused_safely_and_read_at_offset_works() {
    let srv = Server::start(Kind::Sftp, base());
    let r = connect(&site(srv.port, &srv.user, pw_auth()), Some(srv.pass.clone()));
    let body = data(300_000, 3);
    let p = RPath::new("/r.bin");
    let mut w = r.write(&p, 0, None).unwrap();
    w.write_all(&body[..100_000]).unwrap();
    w.finish().unwrap();
    match r.write(&p, 100_000, None) {
        Ok(mut w) => {
            // A server that keeps partial files (OpenSSH): the resumed file must be whole. One
            // that can't write past the start (rclone) fails the upload as Unsupported.
            let done = w.write_all(&body[100_000..]).map_err(|e| e.to_string()).and_then(|_| w.finish().map_err(|e| {
                assert_eq!(e.kind, ErrorKind::Unsupported, "{e}");
                e.to_string()
            }));
            if done.is_ok() {
                assert!(wait_for_file(&srv.root.join("r.bin"), body.len()) == body);
            }
        }
        Err(e) => assert_eq!(e.kind, ErrorKind::Unsupported, "{e}"),
    }
    // Written through the server: rclone caches folder listings, so a file dropped into its
    // root behind its back may not be visible yet.
    let mut w = r.write(&RPath::new("/whole.bin"), 0, None).unwrap();
    w.write_all(&body).unwrap();
    w.finish().unwrap();
    let mut tail = Vec::new();
    r.read(&RPath::new("/whole.bin"), 250_000).unwrap().read_to_end(&mut tail).unwrap();
    assert!(tail == body[250_000..]);
}

fn run_job(engine_job: Job) -> (JobState, Vec<Event>) {
    let (tx, rx) = channel();
    let tx = Mutex::new(tx);
    let engine = Engine::new(
        EngineConfig { retry_delay: Duration::from_millis(200), chunk: 64 * 1024, ..EngineConfig::default() },
        move |e| {
            let _ = tx.lock().unwrap().send(e);
        },
    );
    let id = engine.enqueue(engine_job);
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
fn upload_and_download_1mb_and_50mb_byte_equal() {
    let srv = Server::start(Kind::Sftp, base());
    let remote: Arc<dyn Remote> = Arc::from(connect(&site(srv.port, &srv.user, pw_auth()), Some(srv.pass.clone())));
    let local: Arc<dyn Remote> = Arc::new(LocalRemote::new());
    let dir = scratch_dir(base(), "sftp-xfer");
    std::fs::create_dir_all(dir.join("up")).unwrap();
    std::fs::write(dir.join("up/one.bin"), data(1 << 20, 1)).unwrap();
    std::fs::write(dir.join("up/fifty.bin"), data(50 << 20, 2)).unwrap();

    let job = Job {
        op: Op::Copy,
        src: local.clone(),
        src_paths: vec![rpath(&dir.join("up"))],
        dst: Some(remote.clone()),
        dst_dir: Some(RPath::root()),
        conflict: ConflictPolicy::Overwrite,
        label: "up".into(),
        delete_shared_contents: false,
    };
    let (st, _) = run_job(job);
    assert!(matches!(st, JobState::Done), "{st:?}");
    for f in ["one.bin", "fifty.bin"] {
        assert!(std::fs::read(dir.join("up").join(f)).unwrap() == std::fs::read(srv.root.join("up").join(f)).unwrap(), "{f} differs");
    }

    std::fs::create_dir_all(dir.join("down")).unwrap();
    let job = Job {
        op: Op::Copy,
        src: remote.clone(),
        src_paths: vec![RPath::new("/up")],
        dst: Some(local.clone()),
        dst_dir: Some(rpath(&dir.join("down"))),
        conflict: ConflictPolicy::Overwrite,
        label: "down".into(),
        delete_shared_contents: false,
    };
    let (st, _) = run_job(job);
    assert!(matches!(st, JobState::Done), "{st:?}");
    for f in ["one.bin", "fifty.bin"] {
        assert!(std::fs::read(dir.join("up").join(f)).unwrap() == std::fs::read(dir.join("down/up").join(f)).unwrap(), "{f} differs");
    }
    // mtime carried to the server (whole seconds)
    let lm = std::fs::metadata(dir.join("up/one.bin")).unwrap().modified().unwrap();
    let rm = remote.stat(&RPath::new("/up/one.bin")).unwrap().modified.unwrap();
    let diff = lm.duration_since(rm).unwrap_or_else(|e| e.duration());
    assert!(diff < Duration::from_secs(2), "{diff:?}");
    let _ = std::fs::remove_dir_all(dir);
}

/// The connection drops mid-upload; the engine reconnects and, since rclone can't keep partial
/// uploads, sends the file again. (Resume itself is covered by the engine's tests and the
/// download test below.)
#[test]
fn upload_survives_connection_drop() {
    upload_after_drop(Server::start(Kind::Sftp, base()), false);
}

fn upload_after_drop(srv: Server, resumes: bool) {
    let proxy = FaultProxy::start(srv.addr);
    let remote: Arc<dyn Remote> = Arc::from(connect(&site(proxy.port, &srv.user, pw_auth()), Some(srv.pass.clone())));
    let local: Arc<dyn Remote> = Arc::new(LocalRemote::new());
    let dir = scratch_dir(base(), "sftp-resume");
    let body = data(20 << 20, 9);
    std::fs::write(dir.join("big.bin"), &body).unwrap();
    proxy.cut_after(6 << 20);
    let started = SystemTime::now();
    let job = Job {
        op: Op::Copy,
        src: local,
        src_paths: vec![rpath(&dir.join("big.bin"))],
        dst: Some(remote),
        dst_dir: Some(RPath::root()),
        conflict: ConflictPolicy::Overwrite,
        label: "resume".into(),
        delete_shared_contents: false,
    };
    let (st, log) = run_job(job);
    assert!(matches!(st, JobState::Done), "{st:?}");
    assert_eq!(proxy.cuts(), 1);
    assert!(log.iter().any(|e| matches!(e, Event::State(_, JobState::Retrying { .. }))), "should have retried");
    assert!(proxy.accepted() >= 2, "reconnected");
    assert!(wait_for_file(&srv.root.join("big.bin"), body.len()) == body, "byte-equal after the drop");
    let progress: Vec<u64> = log.iter().filter_map(|e| match e {
        Event::Progress(_, p) => Some(p.bytes_done),
        _ => None,
    }).collect();
    assert_eq!(*progress.last().unwrap(), body.len() as u64);
    // Resuming keeps the progress counter above where the drop happened; restarting rewinds it.
    let rewound = progress.windows(2).any(|w| w[1] + (1 << 20) < w[0]);
    assert_eq!(rewound, !resumes, "progress: {progress:?}");
    let _ = started;
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn download_resumes_after_connection_drop() {
    let srv = Server::start(Kind::Sftp, base());
    let body = data(12 << 20, 5);
    std::fs::write(srv.root.join("d.bin"), &body).unwrap();
    let proxy = FaultProxy::start(srv.addr);
    let remote: Arc<dyn Remote> = Arc::from(connect(&site(proxy.port, &srv.user, pw_auth()), Some(srv.pass.clone())));
    let local: Arc<dyn Remote> = Arc::new(LocalRemote::new());
    let dir = scratch_dir(base(), "sftp-dresume");
    proxy.cut_after(5 << 20);
    let job = Job {
        op: Op::Copy,
        src: remote,
        src_paths: vec![RPath::new("/d.bin")],
        dst: Some(local),
        dst_dir: Some(rpath(&dir)),
        conflict: ConflictPolicy::Overwrite,
        label: "dresume".into(),
        delete_shared_contents: false,
    };
    let (st, _) = run_job(job);
    assert!(matches!(st, JobState::Done), "{st:?}");
    assert_eq!(proxy.cuts(), 1);
    assert!(std::fs::read(dir.join("d.bin")).unwrap() == body);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn unreachable_server_is_a_network_error() {
    let port = filewinch_testkit::refused_port();
    let err = connect_raw(&site(port, "x", pw_auth()), Some("p".into())).err().unwrap();
    assert_eq!(err.kind, ErrorKind::Network);
    assert!(err.message.contains("refused the connection"), "{}", err.message);
}


/// A 64 MB download over 4 SFTP connections lands byte-equal, without falling back.
#[test]
fn segmented_download_64mb_over_4_connections() {
    let srv = Server::start(Kind::Sftp, base());
    let want = data(64 << 20, 9);
    std::fs::write(srv.root.join("big.bin"), &want).unwrap();
    let remote: Arc<dyn Remote> = Arc::from(connect(&site(srv.port, &srv.user, pw_auth()), Some(srv.pass.clone())));
    let dir = scratch_dir(base(), "sftp-seg");
    let o = filewinch_testkit::segmented::download(remote, &RPath::new("/big.bin"), &dir, 4);
    assert!(matches!(o.state, JobState::Done), "{o:?}");
    assert!(std::fs::read(dir.join("big.bin")).unwrap() == want, "bytes differ");
    assert!(o.max_connections >= 2 && o.ranges >= 4 && o.notes.is_empty(), "{o:?}");
    let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
    assert_eq!(left.len(), 1, "no temp files left: {left:?}");
    let _ = std::fs::remove_dir_all(dir);
}

/// Throughput with 1, 3 and 8 connections, direct and through a proxy adding latency.
/// `cargo test --release -p filewinch-sftp --test sftp -- --ignored --nocapture measure`
#[test]
#[ignore = "measurement; writes 512 MB of scratch"]
fn measure_segmented_256mb() {
    let base = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let srv = Server::start(Kind::Sftp, base);
    std::fs::write(srv.root.join("big.bin"), data(256 << 20, 3)).unwrap();
    let proxy = FaultProxy::start(([127, 0, 0, 1], srv.port).into());
    proxy.set_latency(Duration::from_millis(2));
    let dir = scratch_dir(base, "sftp-measure");
    for (label, port) in [("direct", srv.port), ("2 ms per 16 KB proxy", proxy.port)] {
        for n in [1, 3, 8] {
            let remote: Arc<dyn Remote> = Arc::from(connect(&site(port, &srv.user, pw_auth()), Some(srv.pass.clone())));
            let o = filewinch_testkit::segmented::download(remote, &RPath::new("/big.bin"), &dir, n);
            assert!(matches!(o.state, JobState::Done), "{o:?}");
            eprintln!("MEASURE sftp {label}: {n} connection(s): {:.2} s, {:.1} MB/s (max {} at once)", o.secs, 256.0 * 1.048_576 / o.secs, o.max_connections);
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// A folder of 2,000 small files and 4 large ones: time to the first byte (transfers start
/// while the walk is still counting) and in total.
/// `cargo test --release -p filewinch-sftp --test sftp -- --ignored --nocapture measure_tree`
#[test]
#[ignore = "measurement; writes about 300 MB of scratch"]
fn measure_tree_first_byte() {
    let base = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let srv = Server::start(Kind::Sftp, base);
    let tree = srv.root.join("tree");
    for d in 0..40 {
        let dir = tree.join(format!("d{d:02}"));
        std::fs::create_dir_all(&dir).unwrap();
        for f in 0..50 {
            std::fs::write(dir.join(format!("f{f:02}.txt")), data(2_000 + f * 37, (d * 50 + f) as u32)).unwrap();
        }
    }
    for i in 0..4 {
        std::fs::write(tree.join(format!("big{i}.bin")), data(64 << 20, 100 + i)).unwrap();
    }
    let dir = scratch_dir(base, "sftp-tree");
    let remote: Arc<dyn Remote> = Arc::from(connect(&site(srv.port, &srv.user, pw_auth()), Some(srv.pass.clone())));
    let o = filewinch_testkit::segmented::download(remote, &RPath::new("/tree"), &dir, 3);
    assert!(matches!(o.state, JobState::Done), "{o:?}");
    eprintln!(
        "MEASURE sftp tree (2,000 small + 4 × 64 MB): first byte {:.2} s, totals known {:.2} s, done {:.2} s",
        o.first_byte.unwrap_or(f64::NAN),
        o.counted.unwrap_or(f64::NAN),
        o.secs
    );
    let _ = std::fs::remove_dir_all(dir);
}
