//! Signing in (Keystone v3/v2, TempAuth), region choice, token renewal and auth failures.

mod mock;

use std::time::Duration;

use filewinch_core::{Connector, ErrorKind, RPath, Remote};
use filewinch_swift::{SwiftConnector, Tuning};
use mock::{Mock, PASS, connect};

fn storage_hits(m: &Mock, prefix: &str) -> usize {
    m.st().log.iter().filter(|l| l.split(' ').nth(1).is_some_and(|p| p.starts_with(prefix))).count()
}

#[test]
fn keystone_v3_default_region_is_first_public_endpoint() {
    let m = Mock::start();
    m.put("photos", "a.jpg", b"x");
    let r = m.connect(Tuning::default());
    let names: Vec<String> = r.list(&RPath::root()).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["photos"]);
    assert!(storage_hits(&m, "/v1/AUTH_test") >= 2);
    assert_eq!(storage_hits(&m, "/r2/"), 0);
}

#[test]
fn keystone_v3_region_selection() {
    let m = Mock::start();
    m.put("photos", "a.jpg", b"x");
    let mut site = m.site("/v3", "test:tester");
    site.region = Some("regiontwo".into());
    let r = connect(&site, PASS, Tuning::default()).unwrap();
    assert_eq!(r.list(&RPath::root()).unwrap().len(), 1);
    assert_eq!(storage_hits(&m, "/v1/"), 0);
    assert!(storage_hits(&m, "/r2/v1/AUTH_test") >= 2);
}

#[test]
fn keystone_v3_unknown_region_lists_available_ones() {
    let m = Mock::start();
    let mut site = m.site("/v3", "test:tester");
    site.region = Some("Mars".into());
    let e = connect(&site, PASS, Tuning::default()).err().unwrap();
    assert_eq!(e.kind, ErrorKind::NotFound);
    assert!(e.message.contains("Mars") && e.message.contains("RegionOne") && e.message.contains("RegionTwo"), "{}", e.message);
}

#[test]
fn keystone_v3_domain_prefix_and_missing_project() {
    let m = Mock::start();
    connect(&m.site("/v3", "Default/test:tester"), PASS, Tuning::default()).unwrap();
    let e = connect(&m.site("/v3", "Elsewhere/test:tester"), PASS, Tuning::default()).err().unwrap();
    assert_eq!(e.kind, ErrorKind::Auth);
    let e = connect(&m.site("/v3", "tester"), PASS, Tuning::default()).err().unwrap();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.message.contains("Enter a project"), "{}", e.message);
}

#[test]
fn keystone_v2_and_tempauth() {
    let m = Mock::start();
    m.put("c", "f", b"hello");
    let r = connect(&m.site("/v2.0", "test:tester"), PASS, Tuning::default()).unwrap();
    assert_eq!(r.stat(&RPath::new("/c/f")).unwrap().size, Some(5));
    let r = connect(&m.site("/auth/v1.0", "test:tester"), PASS, Tuning::default()).unwrap();
    assert_eq!(r.stat(&RPath::new("/c/f")).unwrap().size, Some(5));
    // Through the Connector trait too.
    let r = SwiftConnector.connect(&m.site("/v2.0", "test:tester"), std::sync::Arc::new(filewinch_core::FnSecrets(move |_: &filewinch_core::SecretRef| Ok(Some(PASS.into()))))).unwrap();
    assert!(r.caps().server_copy && r.caps().resume_read && !r.caps().real_dirs && !r.caps().server_rename);
}

#[test]
fn unversioned_url_falls_back_v3_v2_v1() {
    let m = Mock::start();
    m.st().disabled.insert("v3");
    connect(&m.site("", "test:tester"), PASS, Tuning::default()).unwrap();
    m.st().disabled.insert("v2");
    connect(&m.site("/", "test:tester"), PASS, Tuning::default()).unwrap();
    m.st().disabled.insert("v1");
    let e = connect(&m.site("", "test:tester"), PASS, Tuning::default()).err().unwrap();
    assert_eq!(e.kind, ErrorKind::NotFound);
    assert!(e.message.contains("auth URL"), "{}", e.message);
}

#[test]
fn wrong_password_is_auth_for_every_version() {
    let m = Mock::start();
    for path in ["/v3", "/v2.0", "/auth/v1.0", ""] {
        let e = connect(&m.site(path, "test:tester"), "nope", Tuning::default()).err().unwrap();
        assert_eq!(e.kind, ErrorKind::Auth, "{path}: {e}");
        assert!(!e.to_string().contains("nope"), "secret leaked: {e}");
    }
    let e = connect(&m.site("/v3", "test:tester"), "", Tuning::default()).err().unwrap();
    assert_eq!(e.kind, ErrorKind::Auth);
}

#[test]
fn unreachable_auth_server_is_network() {
    let port = filewinch_testkit::free_port();
    let m = Mock::start();
    let mut site = m.site("/v3", "test:tester");
    site.host = format!("http://127.0.0.1:{port}/v3");
    let e = connect(&site, PASS, Tuning::default()).err().unwrap();
    assert_eq!(e.kind, ErrorKind::Network, "{e}");
}

#[test]
fn expired_token_signs_in_again_transparently() {
    let m = Mock::start();
    m.put("c", "f", b"hello");
    let r = m.connect(Tuning::default());
    assert_eq!(m.st().auths, 1);
    m.st().tokens.clear();
    assert_eq!(r.list(&RPath::new("/c")).unwrap().len(), 1);
    assert_eq!(m.st().auths, 2);
    // Downloads too.
    m.st().tokens.clear();
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut r.read(&RPath::new("/c/f"), 0).unwrap(), &mut buf).unwrap();
    assert_eq!(buf, b"hello");
    assert_eq!(m.st().auths, 3);
}

#[test]
fn token_near_expiry_is_renewed_before_use() {
    let m = Mock::start();
    m.st().expires = "2000-01-01T00:00:00Z".into();
    let r = m.connect(Tuning::default());
    r.list(&RPath::root()).unwrap();
    assert!(m.st().auths >= 3, "renewed before each use: {}", m.st().auths);
}

#[test]
fn persistent_401_is_auth_after_one_retry() {
    let m = Mock::start();
    let r = m.connect(Tuning::default());
    m.st().reject_tokens = true;
    let before = m.st().auths;
    let e = r.list(&RPath::root()).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth, "{e}");
    assert_eq!(m.st().auths, before + 1, "exactly one re-sign-in");
}

#[test]
fn rate_limits_and_busy() {
    let m = Mock::start();
    let r = m.connect(Tuning::default());
    m.st().fail.push_back((503, vec![("Retry-After", "7".into())], String::new()));
    let e = r.list(&RPath::root()).unwrap_err();
    assert_eq!(e.kind, ErrorKind::RateLimited);
    assert_eq!(e.retry_after, Some(Duration::from_secs(7)));
    m.st().fail.push_back((498, vec![], "Slow down".into()));
    assert_eq!(r.list(&RPath::root()).unwrap_err().kind, ErrorKind::RateLimited);
    m.st().fail.push_back((429, vec![], String::new()));
    assert_eq!(r.list(&RPath::root()).unwrap_err().kind, ErrorKind::RateLimited);
    m.st().fail.push_back((500, vec![], String::new()));
    assert_eq!(r.list(&RPath::root()).unwrap_err().kind, ErrorKind::Network);
    assert!(r.list(&RPath::root()).is_ok());
}

#[test]
fn closed_session_refuses_calls() {
    let m = Mock::start();
    let r = m.connect(Tuning::default());
    r.close();
    assert_eq!(r.list(&RPath::root()).unwrap_err().kind, ErrorKind::Closed);
    assert_eq!(r.write(&RPath::new("/c/f"), 0, Some(1)).err().unwrap().kind, ErrorKind::Closed);
}
