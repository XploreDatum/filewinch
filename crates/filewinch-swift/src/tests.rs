use std::time::{Duration, UNIX_EPOCH};

use filewinch_core::{ErrorKind, RPath};

use crate::auth::pick_endpoint;
use crate::client::{Loc, url_path};
use crate::config::{AuthConfig, Version, parse_auth_url, uses_rackspace_api_key};
use crate::time::{parse_http, parse_iso};

#[test]
fn auth_url_versions() {
    let v = |u: &str| parse_auth_url(u, None).unwrap();
    assert_eq!(v("https://k.example.com:5000/v3").1, Version::V3);
    assert_eq!(v("https://k.example.com:5000/v3/").0, "https://k.example.com:5000/v3");
    assert_eq!(v("https://k.example.com/identity/v3/auth/tokens").0, "https://k.example.com/identity/v3");
    assert_eq!(v("https://identity.api.rackspacecloud.com/v2.0").1, Version::V2);
    assert_eq!(v("https://identity.api.rackspacecloud.com/v2.0/tokens").0, "https://identity.api.rackspacecloud.com/v2.0");
    assert_eq!(v("http://127.0.0.1:8080/auth/v1.0").1, Version::V1);
    let (base, ver, host) = v("keystone.example.com");
    assert_eq!((base.as_str(), ver, host.as_str()), ("https://keystone.example.com", Version::Detect, "keystone.example.com"));
    // Port from the site when the URL has none; the URL's own port wins.
    assert_eq!(parse_auth_url("http://h/v3", Some(5000)).unwrap().0, "http://h:5000/v3");
    assert_eq!(parse_auth_url("http://h:1/v3", Some(5000)).unwrap().0, "http://h:1/v3");
}

#[test]
fn auth_url_rejects_garbage() {
    for bad in ["", "   ", "ftp://h/v3", "http://"] {
        let e = parse_auth_url(bad, None).err().unwrap_or_else(|| panic!("{bad:?} accepted"));
        assert_eq!(e.kind, ErrorKind::InvalidPath);
    }
}

#[test]
fn user_project_and_domain_come_from_the_site() {
    let mut site = filewinch_core::Site::new("s", "s", filewinch_core::Protocol::Swift);
    site.host = "https://keystone.example.com/v3".into();
    site.user = " alice ".into();
    let c = AuthConfig::from_site(&site, "pw".into()).unwrap();
    assert_eq!((c.user_name.as_str(), c.project.as_deref(), c.domain.as_str(), c.user.as_str()), ("alice", None, "Default", "alice"));
    site.project = Some("proj".into());
    site.domain = Some("corp".into());
    let c = AuthConfig::from_site(&site, "pw".into()).unwrap();
    assert_eq!((c.user_name.as_str(), c.project.as_deref(), c.domain.as_str(), c.user.as_str()), ("alice", Some("proj"), "corp", "proj:alice"));
    site.project = Some("  ".into());
    site.domain = Some("".into());
    let c = AuthConfig::from_site(&site, "pw".into()).unwrap();
    assert_eq!((c.project, c.domain.as_str()), (None, "Default"));
}

#[test]
fn rackspace_api_key_rule() {
    let key = "0123456789abcdef0123456789ABCDEF";
    assert!(uses_rackspace_api_key("identity.api.rackspacecloud.com", key));
    assert!(!uses_rackspace_api_key("identity.api.rackspacecloud.com", "hunter2"));
    assert!(!uses_rackspace_api_key("keystone.example.com", key));
    assert!(!uses_rackspace_api_key("evilrackspacecloud.com", key));
}

#[test]
fn endpoint_choice() {
    let eps = || vec![(Some("DFW".to_string()), "https://dfw/v1/a".to_string()), (Some("ORD".into()), "https://ord/v1/a".into())];
    assert_eq!(pick_endpoint(eps(), None).unwrap(), "https://dfw/v1/a");
    assert_eq!(pick_endpoint(eps(), Some("ord")).unwrap(), "https://ord/v1/a");
    let e = pick_endpoint(eps(), Some("SYD")).unwrap_err();
    assert!(e.message.contains("DFW") && e.message.contains("ORD") && e.message.contains("SYD"), "{}", e.message);
    assert_eq!(pick_endpoint(Vec::new(), None).unwrap_err().kind, ErrorKind::Unsupported);
}

#[test]
fn locations_and_encoding() {
    assert_eq!(Loc::of(&RPath::root()), Loc::Root);
    assert_eq!(Loc::of(&RPath::new("/c")), Loc::Container("c".into()));
    assert_eq!(Loc::of(&RPath::new("/c/a/b.txt")), Loc::Object { container: "c".into(), key: "a/b.txt".into() });
    assert_eq!(url_path("my c", Some("a b/ü?#.txt")), "/my%20c/a%20b/%C3%BC%3F%23.txt");
    assert_eq!(url_path("a/b", None), "/a%2Fb");
}

#[test]
fn timestamps() {
    let t = |s| parse_iso(s).unwrap().duration_since(UNIX_EPOCH).unwrap();
    assert_eq!(t("2024-01-02T03:04:05.250000"), Duration::new(1_704_164_645, 250_000_000));
    assert_eq!(t("2024-01-02T03:04:05Z"), Duration::from_secs(1_704_164_645));
    assert_eq!(t("2024-01-01T21:04:05.000-06:00"), Duration::from_secs(1_704_164_645));
    assert!(parse_iso("2024-13-02T03:04:05").is_none());
    assert!(parse_iso("yesterday").is_none());
    assert_eq!(
        parse_http("Tue, 02 Jan 2024 03:04:05 GMT").unwrap().duration_since(UNIX_EPOCH).unwrap(),
        Duration::from_secs(1_704_164_645)
    );
}

#[test]
fn etag_md5_only_for_32_hex() {
    use crate::list::{has_swift_bytes, md5_etag};
    use filewinch_core::{Checksum, HashAlgo};
    let hex = "5d41402abc4b2a76b9719d911017c592";
    let want = Some(Checksum { algo: HashAlgo::Md5, hex: hex.into() });
    assert_eq!(md5_etag(hex), want);
    assert_eq!(md5_etag(&format!("\"{}\"", hex.to_uppercase())), want);
    assert_eq!(md5_etag(&hex[..31]), None);
    assert_eq!(md5_etag(&format!("{hex}-2")), None);
    assert_eq!(md5_etag(&format!("\"{hex}")), None);
    assert_eq!(md5_etag(""), None);
    assert!(has_swift_bytes("application/octet-stream;swift_bytes=1048576"));
    assert!(has_swift_bytes("text/plain; charset=utf-8; Swift_Bytes=3"));
    assert!(!has_swift_bytes("application/octet-stream"));
    assert!(!has_swift_bytes("swift_bytes=3"));
}
