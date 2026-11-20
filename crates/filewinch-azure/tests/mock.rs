//! Against a scripted mock of the Blob service: SAS handling, throttling, copy polling,
//! listing markers.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use common::{DEV_KEY, EMPTY_CONTAINERS, Mock, Reply, connect, site};
use filewinch_core::{Auth, ErrorKind, Protocol, RPath, Site};

fn p(s: &str) -> RPath {
    RPath::new(s)
}

const SIG: &str = "S3CR3TSIGNATURE%2Babc%3D";

fn sas_site(port: u16, container_scoped: bool) -> Site {
    let mut s = Site::new("sas", "sas", Protocol::AzureBlob);
    s.host = if container_scoped {
        format!("http://127.0.0.1:{port}/acct/box?sv=2021-08-06&sr=c&sp=racwdl&se=2099-01-01&sig={SIG}")
    } else {
        format!("http://127.0.0.1:{port}/acct?sv=2021-08-06&ss=b&srt=sco&sp=racwdl&sig={SIG}")
    };
    s.auth = Auth::None;
    s
}

fn blobs_xml(names: &[&str], prefixes: &[&str], next: Option<&str>) -> String {
    let mut x = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><EnumerationResults><Blobs>");
    for n in names {
        x.push_str(&format!("<Blob><Name>{n}</Name><Properties><Content-Length>3</Content-Length></Properties></Blob>"));
    }
    for n in prefixes {
        x.push_str(&format!("<BlobPrefix><Name>{n}</Name></BlobPrefix>"));
    }
    x.push_str(&format!("</Blobs><NextMarker>{}</NextMarker></EnumerationResults>", next.unwrap_or("")));
    x
}

#[test]
fn sas_is_on_every_request_and_never_in_errors() {
    let mock = Mock::start(|req| {
        if req.method == "GET" && req.url.contains("restype=container") {
            return Reply::new(200, &blobs_xml(&["a.txt"], &["sub/"], None));
        }
        // Everything else: an error that echoes the request URL, as Azure does.
        Reply::new(
            403,
            &format!(
                "<?xml version=\"1.0\"?><Error><Code>AuthorizationPermissionMismatch</Code><Message>no</Message>\
                 <Url>http://x{}</Url><Detail>sig={SIG}</Detail></Error>",
                req.url.replace('&', "&amp;")
            ),
        )
        .h("x-ms-error-code", "AuthorizationPermissionMismatch")
    });
    let r = connect(&sas_site(mock.port, true), "").unwrap();
    assert_eq!(r.home().unwrap(), p("/box"));
    let root: Vec<String> = r.list(&RPath::root()).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(root, ["box"], "a container SAS shows only its container");
    let names: Vec<String> = r.list(&p("/box")).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["sub", "a.txt"]);
    assert_eq!(r.list(&p("/other")).unwrap_err().kind, ErrorKind::PermissionDenied);

    let errors = [
        r.remove_file(&p("/box/a.txt")).unwrap_err(),
        r.read(&p("/box/a.txt"), 5).err().unwrap(),
        r.copy(&p("/box/a.txt"), &p("/box/b.txt")).unwrap_err(),
    ];
    for e in &errors {
        assert_eq!(e.kind, ErrorKind::PermissionDenied, "{e}");
        let all = format!("{} {:?} {e:?}", e.message, e.detail);
        assert!(!all.contains("S3CR3T"), "SAS leaked: {all}");
    }
    let seen = mock.seen();
    assert!(seen.len() >= 5);
    for s in &seen {
        assert!(s.url.contains(&format!("sig={SIG}")), "no SAS on {} {}", s.method, s.url);
        assert!(s.header("authorization").is_none(), "SAS requests aren't signed");
        assert_eq!(s.header("x-ms-version"), Some("2021-08-06"));
    }
    // The copy source carries the SAS too (the service needs it to read the source).
    let copy = seen.iter().find(|s| s.header("x-ms-copy-source").is_some()).unwrap();
    assert!(copy.header("x-ms-copy-source").unwrap().contains(SIG));
    // Ranged read header.
    assert!(seen.iter().any(|s| s.header("x-ms-range") == Some("bytes=5-")));
}

#[test]
fn account_sas_in_password_field() {
    let mock = Mock::start(|_| Reply::new(200, EMPTY_CONTAINERS));
    let mut s = site(mock.port, "acct");
    s.user = String::new();
    let r = connect(&s, &format!("?sv=2021-08-06&ss=b&srt=sco&sp=rl&sig={SIG}")).unwrap();
    assert!(r.list(&RPath::root()).unwrap().is_empty());
    for req in mock.seen() {
        assert!(req.has_query("comp=list"), "{}", req.url);
        assert!(req.url.contains(SIG));
    }
    // The same account SAS as a URL: unscoped, the top level lists containers.
    let r = connect(&sas_site(mock.port, false), "").unwrap();
    assert_eq!(r.home().unwrap(), RPath::root());
    assert!(r.list(&RPath::root()).unwrap().is_empty());
}

#[test]
fn server_busy_is_rate_limited_with_retry_after() {
    let mock = Mock::start(|req| {
        if req.url.contains("maxresults=1") && !req.url.contains("restype") {
            return Reply::new(200, EMPTY_CONTAINERS);
        }
        Reply::new(503, "<?xml version=\"1.0\"?><Error><Code>ServerBusy</Code><Message>Slow down</Message></Error>")
            .h("x-ms-error-code", "ServerBusy")
            .h("Retry-After", "7")
    });
    let r = connect(&site(mock.port, "acct"), DEV_KEY).unwrap();
    let e = r.list(&p("/box")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::RateLimited, "{e}");
    assert_eq!(e.retry_after, Some(Duration::from_secs(7)));
    assert!(e.kind.is_transient());
    // Shared Key requests are signed with the account from the path-style URL.
    let seen = mock.seen();
    assert!(seen.iter().all(|s| s.header("authorization").is_some_and(|a| a.starts_with("SharedKey acct:"))));
}

#[test]
fn copy_polls_until_success() {
    let polls = Arc::new(AtomicU32::new(0));
    let n = polls.clone();
    let mock = Mock::start(move |req| match req.method.as_str() {
        "GET" => Reply::new(200, EMPTY_CONTAINERS),
        "PUT" => Reply::new(202, "").h("x-ms-copy-id", "cid-1").h("x-ms-copy-status", "pending"),
        "HEAD" => {
            let i = n.fetch_add(1, Ordering::SeqCst);
            let status = if i < 2 { "pending" } else { "success" };
            Reply::new(200, "")
                .h("x-ms-copy-id", "cid-1")
                .h("x-ms-copy-status", status)
                .h("x-ms-copy-progress", &format!("{}/100", i * 40))
        }
        _ => Reply::new(400, ""),
    });
    let r = connect(&site(mock.port, "acct"), DEV_KEY).unwrap();
    r.copy(&p("/box/src.bin"), &p("/box/dst.bin")).unwrap();
    assert_eq!(polls.load(Ordering::SeqCst), 3);
    let put = mock.seen().into_iter().find(|s| s.method == "PUT").unwrap();
    assert!(put.url.ends_with("/acct/box/dst.bin"), "{}", put.url);
    assert_eq!(put.header("x-ms-copy-source"), Some(format!("http://127.0.0.1:{}/acct/box/src.bin", mock.port).as_str()));
}

#[test]
fn copy_reports_failure_and_abort() {
    for (status, kind) in [("failed", ErrorKind::Other), ("aborted", ErrorKind::Other)] {
        let mock = Mock::start(move |req| match req.method.as_str() {
            "GET" => Reply::new(200, EMPTY_CONTAINERS),
            "PUT" => Reply::new(202, "").h("x-ms-copy-id", "cid-2").h("x-ms-copy-status", "pending"),
            _ => Reply::new(200, "")
                .h("x-ms-copy-id", "cid-2")
                .h("x-ms-copy-status", status)
                .h("x-ms-copy-status-description", "500 InternalError \"Copy failed.\""),
        });
        let r = connect(&site(mock.port, "acct"), DEV_KEY).unwrap();
        let e = r.copy(&p("/box/a"), &p("/box/b")).unwrap_err();
        assert_eq!(e.kind, kind, "{e}");
        assert!(e.message.contains("copy"), "{}", e.message);
        assert!(e.detail.unwrap_or_default().contains("Copy failed"));
    }
}

#[test]
fn closing_stops_a_pending_copy_and_aborts_it() {
    let mock = Arc::new(Mock::start(|req| match req.method.as_str() {
        "GET" => Reply::new(200, EMPTY_CONTAINERS),
        "PUT" if req.has_query("comp=copy") => Reply::new(204, ""),
        "PUT" => Reply::new(202, "").h("x-ms-copy-id", "cid-3").h("x-ms-copy-status", "pending"),
        _ => Reply::new(200, "").h("x-ms-copy-id", "cid-3").h("x-ms-copy-status", "pending"),
    }));
    let r: Arc<dyn filewinch_core::Remote> = Arc::from(connect(&site(mock.port, "acct"), DEV_KEY).unwrap());
    let r2 = r.clone();
    let t = std::thread::spawn(move || r2.copy(&p("/box/a"), &p("/box/b")));
    std::thread::sleep(Duration::from_millis(700));
    r.close();
    let e = t.join().unwrap().unwrap_err();
    assert_eq!(e.kind, ErrorKind::Closed, "{e}");
    let abort = mock.seen().into_iter().find(|s| s.has_query("comp=copy")).expect("abort sent");
    assert!(abort.has_query("copyid=cid-3"));
    assert_eq!(abort.header("x-ms-copy-action"), Some("abort"));
}

#[test]
fn blob_listing_follows_next_marker() {
    let mock = Mock::start(|req| {
        if !req.url.contains("restype=container") {
            return Reply::new(200, EMPTY_CONTAINERS);
        }
        if req.has_query("marker=m%2F2") {
            Reply::new(200, &blobs_xml(&["dir/c"], &[], None))
        } else {
            Reply::new(200, &blobs_xml(&["dir/", "dir/a", "dir/b"], &["dir/sub/"], Some("m/2")))
        }
    });
    let r = connect(&site(mock.port, "acct"), DEV_KEY).unwrap();
    let mut names: Vec<String> = r.list(&p("/box/dir")).unwrap().into_iter().map(|e| e.name).collect();
    names.sort();
    assert_eq!(names, ["a", "b", "c", "sub"]);
    let lists: Vec<_> = mock.seen().into_iter().filter(|s| s.url.contains("restype=container")).collect();
    assert_eq!(lists.len(), 2);
    assert!(lists[0].has_query("prefix=dir%2F") && lists[0].has_query("delimiter=%2F"));
    assert!(lists[1].has_query("marker=m%2F2"));
}

#[test]
fn container_listing_follows_next_marker() {
    let mock = Mock::start(|req| {
        let page = |names: &[&str], next: &str| {
            let items: String = names.iter().map(|n| format!("<Container><Name>{n}</Name></Container>")).collect();
            format!("<EnumerationResults><Containers>{items}</Containers><NextMarker>{next}</NextMarker></EnumerationResults>")
        };
        if req.has_query("marker=next") {
            Reply::new(200, &page(&["three"], ""))
        } else {
            Reply::new(200, &page(&["one", "two"], "next"))
        }
    });
    let r = connect(&site(mock.port, "acct"), DEV_KEY).unwrap();
    let names: Vec<String> = r.list(&RPath::root()).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["one", "two", "three"]);
}

#[test]
fn transient_block_failures_are_retried() {
    let fails = Arc::new(AtomicU32::new(0));
    let f = fails.clone();
    let mock = Mock::start(move |req| {
        if req.method == "GET" {
            return Reply::new(200, EMPTY_CONTAINERS);
        }
        if req.has_query("comp=block") && f.fetch_add(1, Ordering::SeqCst) == 0 {
            return Reply::new(500, "<Error><Code>InternalError</Code></Error>").h("x-ms-error-code", "InternalError");
        }
        Reply::new(201, "")
    });
    let r = connect(&site(mock.port, "acct"), DEV_KEY).unwrap();
    let mut sink = r.write(&p("/box/f"), 0, None).unwrap();
    std::io::Write::write_all(&mut sink, b"hello blocks").unwrap();
    sink.finish().unwrap();
    let seen = mock.seen();
    let blocks: Vec<_> = seen.iter().filter(|s| s.has_query("comp=block")).collect();
    assert_eq!(blocks.len(), 2, "one failure, one retry");
    let list = seen.iter().find(|s| s.has_query("comp=blocklist")).expect("block list committed");
    assert_eq!(list.header("content-type"), Some("application/xml"));
}

/// MD5 of "hello": base64 and hex.
const HELLO_B64: &str = "XUFAKrxLKna5cZ2REBfFkg==";
const HELLO_HEX: &str = "5d41402abc4b2a76b9719d911017c592";

#[test]
fn content_md5_becomes_a_checksum_on_list_and_stat() {
    let mock = Mock::start(|req| {
        if req.method == "GET" && req.url.contains("restype=container") {
            let blob = |n: &str, md5: Option<&str>| {
                let md5 = md5.map(|m| format!("<Content-MD5>{m}</Content-MD5>")).unwrap_or_default();
                format!("<Blob><Name>{n}</Name><Properties><Content-Length>5</Content-Length>{md5}</Properties></Blob>")
            };
            let xml = format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?><EnumerationResults><Blobs>{}{}{}{}{}\
                 <BlobPrefix><Name>sub/</Name></BlobPrefix></Blobs><NextMarker/></EnumerationResults>",
                blob("good", Some(HELLO_B64)),
                blob("missing", None),
                blob("empty", Some("")),
                blob("short", Some("XUFAKrxLKna5cZ2REBfF")),
                blob("garbage", Some("not base64!")),
            );
            return Reply::new(200, &xml);
        }
        if req.method == "HEAD" {
            let r = Reply::new(200, "").h("Last-Modified", "Fri, 20 Nov 2026 10:00:00 GMT");
            return match req.url.split('?').next().unwrap_or_default() {
                "/acct/box/good" => r.h("Content-MD5", HELLO_B64),
                "/acct/box/short" => r.h("Content-MD5", "XUFAKrxLKna5cZ2REBfF"),
                "/acct/box/missing" => r,
                _ => Reply::new(404, "").h("x-ms-error-code", "BlobNotFound"),
            };
        }
        Reply::new(200, EMPTY_CONTAINERS)
    });
    let r = connect(&site(mock.port, "acct"), DEV_KEY).unwrap();
    let md5 = filewinch_core::Checksum { algo: filewinch_core::HashAlgo::Md5, hex: HELLO_HEX.into() };
    let list = r.list(&p("/box")).unwrap();
    let sums = |n: &str| list.iter().find(|e| e.name == n).unwrap().checksums.clone();
    assert_eq!(sums("good"), std::slice::from_ref(&md5));
    for n in ["missing", "empty", "short", "garbage", "sub"] {
        assert!(sums(n).is_empty(), "{n}: {:?}", sums(n));
    }
    assert_eq!(r.stat(&p("/box/good")).unwrap().checksums, [md5]);
    assert!(r.stat(&p("/box/short")).unwrap().checksums.is_empty());
    assert!(r.stat(&p("/box/missing")).unwrap().checksums.is_empty());
}
