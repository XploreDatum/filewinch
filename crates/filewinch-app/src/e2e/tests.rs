//! One round trip and one set of unhappy paths per protocol, against `rclone serve`.

use std::sync::Arc;
use std::time::Duration;

use filewinch_core::{ErrorKind, Remote};
use filewinch_local::LocalRemote;
use filewinch_testkit::{Kind, free_port};

use super::flow::{job, round_trip};
use super::harness::{Harness, Stop};
use super::tree::pseudo_random;
use super::{Live, LocalDir, connect, isolate_known_hosts};
use crate::transfers::engine::{ConflictPolicy, EngineConfig, JobState, Op};

fn skip() -> bool {
    if super::rclone_available() {
        return false;
    }
    eprintln!("SKIPPED: rclone isn't installed (expected at /usr/local/bin/rclone or on PATH)");
    true
}

/// Wrong password, refused connection, a missing file, and cancelling a slow upload.
fn unhappy(kind: Kind) {
    let live = Live::start(kind);
    let name = format!("{kind:?}");

    let err = connect(&live.site, "not-the-password").err().unwrap_or_else(|| panic!("{name}: a wrong password connected"));
    assert_eq!(err.kind, ErrorKind::Auth, "{name}: wrong password: {err:?}");
    assert!(!err.message.contains("not-the-password") && !format!("{:?}", err.detail).contains("not-the-password"), "{name}: the error repeats the password");

    let mut nobody = live.site.clone();
    nobody.port = Some(free_port());
    let err = connect(&nobody, &live.srv.pass).err().unwrap_or_else(|| panic!("{name}: connected to a closed port"));
    assert_eq!(err.kind, ErrorKind::Network, "{name}: closed port: {err:?}");

    let local: Arc<dyn Remote> = Arc::new(LocalRemote::new());
    let down = LocalDir::new("e2e-missing");
    let h = Harness::new();
    let out = h.run(job(Op::Copy, &live.remote, vec![live.home.join("no such file.txt")], Some((&local, down.rpath())), ConflictPolicy::Ask, "download missing"));
    assert!(matches!(out.state, JobState::Failed(_)), "{name}: downloading a missing file ended {:?}", out.state);
    assert!(out.failures.iter().any(|(_, e)| e.kind == ErrorKind::NotFound), "{name}: missing file failure: {:?}", out.failures);
    assert!(!down.0.join("no such file.txt").exists(), "{name}: nothing lands locally");

    // Cancel a throttled 8 MB upload after its first bytes: the job ends Cancelled and the
    // server never shows a complete copy.
    let up = LocalDir::new("e2e-cancel");
    std::fs::write(up.0.join("slow.bin"), pseudo_random(8 << 20, 3)).expect("write");
    let h = Harness::with_config(EngineConfig { bandwidth_limit: Some(1 << 20), ..EngineConfig::default() });
    let id = h.engine.enqueue(job(Op::Copy, &local, vec![up.rpath().join("slow.bin")], Some((&live.remote, live.home.clone())), ConflictPolicy::Ask, "slow upload"));
    h.wait_bytes(id, "slow upload");
    h.engine.cancel(id);
    match h.wait_for(id, "cancelled upload", Duration::from_secs(30)) {
        Stop::Finished(o) => assert!(matches!(o.state, JobState::Cancelled), "{name}: cancelled upload ended {:?}", o.state),
        Stop::Conflict(_) => panic!("{name}: cancelled upload asked a question"),
    }
    if let Ok(e) = live.remote.stat(&live.home.join("slow.bin")) {
        assert_ne!(e.size, Some(8 << 20), "{name}: a cancelled upload left a complete file");
    }
}

macro_rules! protocol_tests {
    ($($kind:ident: $round:ident, $bad:ident;)*) => {$(
        #[test]
        fn $round() {
            if skip() { return; }
            round_trip(&Live::start(Kind::$kind));
        }

        #[test]
        fn $bad() {
            if skip() { return; }
            unhappy(Kind::$kind);
        }
    )*};
}

protocol_tests! {
    Sftp: sftp_round_trip, sftp_unhappy_paths;
    Ftp: ftp_round_trip, ftp_unhappy_paths;
    WebDav: webdav_round_trip, webdav_unhappy_paths;
    S3: s3_round_trip, s3_unhappy_paths;
}

/// A fingerprint the site trusts that isn't the server's still stops the connection, with
/// the real fingerprint in the error.
#[test]
fn sftp_wrong_trusted_fingerprint_asks_again() {
    if skip() {
        return;
    }
    isolate_known_hosts();
    let live = Live::start(Kind::Sftp);
    let real = live.site.trusted_fingerprints[0].clone();
    let mut site = live.site.clone();
    site.trusted_fingerprints = vec!["SHA256:nThbg6kXUpJWGl7E1IGOCspRomTxdCARLviKw6E5SY8".into()];
    let err = connect(&site, &live.srv.pass).err().expect("a wrong pinned fingerprint must not connect");
    assert_eq!(err.kind, ErrorKind::HostTrust, "{err:?}");
    assert_eq!(err.fingerprint().as_deref(), Some(real.as_str()));
}
