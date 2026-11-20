use super::*;

fn site(protocol: Protocol, host: &str, port: Option<u16>) -> Site {
    let mut s = Site::new("id", "name", protocol);
    s.host = host.into();
    s.port = port;
    s
}

#[test]
fn bare_host_uses_protocol_scheme_and_default_port() {
    let t = Target::from_site(&site(Protocol::WebDavs, "Cloud.Example.com", None)).unwrap();
    assert_eq!(t.origin, "https://cloud.example.com");
    assert_eq!(t.port, 443);
    assert!(t.base.is_empty());
    assert_eq!(t.request_path(&RPath::root(), true), "/");
    let t = Target::from_site(&site(Protocol::WebDav, "host", Some(8080))).unwrap();
    assert_eq!(t.origin, "http://host:8080");
}

#[test]
fn full_url_with_base_path() {
    let t = Target::from_site(&site(
        Protocol::WebDavs,
        "https://cloud.example.com/remote.php/dav/files/alice%40corp/",
        None,
    ))
    .unwrap();
    assert_eq!(t.base, ["remote.php", "dav", "files", "alice@corp"]);
    assert_eq!(
        t.request_path(&RPath::new("/a b/é#1%?.txt"), false),
        "/remote.php/dav/files/alice%40corp/a%20b/%C3%A9%231%25%3F.txt"
    );
    assert_eq!(t.request_path(&RPath::new("/dir"), true), "/remote.php/dav/files/alice%40corp/dir/");
    assert_eq!(href_segments("/remote.php/dav/files/alice%40corp/a%20b/"), t.segments(&RPath::new("/a b")));
}

#[test]
fn url_port_used_unless_site_port_set() {
    let t = Target::from_site(&site(Protocol::WebDav, "http://h:8443/dav", None)).unwrap();
    assert_eq!((t.port, t.base.as_slice()), (8443, ["dav".to_string()].as_slice()));
    let t = Target::from_site(&site(Protocol::WebDav, "h:8443", Some(9000))).unwrap();
    assert_eq!(t.port, 9000);
    let t = Target::from_site(&site(Protocol::WebDav, "[::1]:81", None)).unwrap();
    assert_eq!((t.host.as_str(), t.host_name(), t.port), ("[::1]", "::1", 81));
}

#[test]
fn bad_addresses_are_rejected() {
    assert!(Target::from_site(&site(Protocol::WebDav, "  ", None)).is_err());
    assert!(Target::from_site(&site(Protocol::WebDav, "host:abc", None)).is_err());
    assert!(Target::from_site(&site(Protocol::WebDav, "bad host", None)).is_err());
}

#[test]
fn hrefs_decode_from_urls_and_paths() {
    assert_eq!(href_segments("http://h:80/a/b%20c/"), ["a", "b c"]);
    assert_eq!(href_segments("/a/%E2%9C%93/"), ["a", "✓"]);
    assert_eq!(href_segments("https://h"), Vec::<String>::new());
    assert_eq!(href_segments("/a//./b"), ["a", "b"]);
}

#[test]
fn redirects_only_to_same_origin() {
    let t = Target::from_site(&site(Protocol::WebDav, "h", Some(8080))).unwrap();
    assert_eq!(t.resolve_location("/x/", "/x").unwrap(), "/x/");
    assert_eq!(t.resolve_location("http://H:8080/x/", "/x").unwrap(), "/x/");
    assert_eq!(t.resolve_location("y/", "/x/z").unwrap(), "/x/y/");
    let e = t.resolve_location("https://h/x/", "/x").unwrap_err();
    assert!(e.message.contains("WebDAV (HTTPS)"), "{e}");
    assert!(t.resolve_location("http://evil/x", "/x").is_err());
}
