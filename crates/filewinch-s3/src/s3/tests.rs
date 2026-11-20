//! S3 unit tests and mock-server tests for paths rclone's S3 server doesn't exercise
//! (region redirects, UploadPartCopy, 200-with-error bodies, abort on drop).

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use filewinch_core::{Auth, ErrorKind, Protocol, RPath, Remote, SecretRef, Site};
use filewinch_http::Http;

use super::endpoint::{Endpoint, dns_compatible, infer_region};
use super::listing::parse_page;
use super::multipart::{part_size_for, part_size_unknown};
use super::*;
use crate::mock::{Mock, Reply, Seen};
use crate::xml;

const MIB: u64 = 1024 * 1024;

fn site(host: &str) -> Site {
    let mut s = Site::new("t", "t", Protocol::S3);
    s.host = host.into();
    s.user = "AKIDTEST".into();
    s.auth = Auth::Password { secret: SecretRef("k".into()) };
    s
}

fn secret(_: &SecretRef) -> filewinch_core::Result<Option<String>> {
    Ok(Some("test-secret".into()))
}

fn mock_remote(mock: &Mock, limits: Limits) -> S3Remote {
    S3Remote::open(&site(&mock.url()), &secret, Http::new(), limits).unwrap()
}

#[test]
fn endpoint_aws_default_and_regional() {
    let (ep, region) = Endpoint::from_site(&site("")).unwrap();
    assert_eq!(region, "us-east-1");
    assert_eq!(ep.host, "s3.us-east-1.amazonaws.com");
    assert!(ep.https);
    assert_eq!(ep.host_for("eu-west-1"), "s3.eu-west-1.amazonaws.com");
    let (_, _, url) = ep.locate("eu-west-1", Some("photos"), "a b/c.jpg");
    assert_eq!(url, "https://photos.s3.eu-west-1.amazonaws.com/a%20b/c.jpg");
    // Dotted bucket over https: path style (the wildcard certificate wouldn't match).
    let (_, path, url) = ep.locate("us-east-1", Some("my.bucket"), "k");
    assert_eq!(path, "/my.bucket/k");
    assert_eq!(url, "https://s3.us-east-1.amazonaws.com/my.bucket/k");

    let (ep, region) = Endpoint::from_site(&site("s3.ap-south-1.amazonaws.com")).unwrap();
    assert_eq!(region, "ap-south-1");
    assert_eq!(ep.host, "s3.ap-south-1.amazonaws.com");
}

#[test]
fn endpoint_compatible_services() {
    let (ep, region) = Endpoint::from_site(&site("http://127.0.0.1:9000")).unwrap();
    assert_eq!((ep.https, ep.port, region.as_str()), (false, Some(9000), "us-east-1"));
    let (authority, path, url) = ep.locate(&region, Some("bucket"), "k/x");
    assert_eq!((authority.as_str(), path.as_str()), ("127.0.0.1:9000", "/bucket/k/x"));
    assert_eq!(url, "http://127.0.0.1:9000/bucket/k/x");

    let (ep, region) = Endpoint::from_site(&site("https://acct123.r2.cloudflarestorage.com/")).unwrap();
    assert_eq!(region, "auto");
    assert_eq!(ep.port, None);
    let (_, _, url) = ep.locate(&region, Some("b1"), "");
    assert_eq!(url, "https://acct123.r2.cloudflarestorage.com/b1"); // "b1" is too short for DNS style

    assert_eq!(infer_region("s3.us-west-004.backblazeb2.com").as_deref(), Some("us-west-004"));
    assert_eq!(infer_region("s3.eu-central-1.wasabisys.com").as_deref(), Some("eu-central-1"));
    assert_eq!(infer_region("nyc3.digitaloceanspaces.com").as_deref(), Some("nyc3"));
    assert_eq!(infer_region("minio.internal"), None);

    let mut s = site("https://minio.example.com:8443");
    s.path_style = true;
    s.region = Some("eu-1".into());
    let (ep, region) = Endpoint::from_site(&s).unwrap();
    assert_eq!(region, "eu-1");
    let (_, _, url) = ep.locate(&region, Some("bucket"), "k");
    assert_eq!(url, "https://minio.example.com:8443/bucket/k");

    assert!(Endpoint::from_site(&site("host:notaport")).is_err());
}

#[test]
fn bucket_name_rules() {
    assert!(dns_compatible("my-bucket.logs"));
    assert!(!dns_compatible("My_Bucket"));
    assert!(!dns_compatible("ab"));
    assert!(!dns_compatible("192.168.1.1"));
    assert!(!dns_compatible("a..b"));
}

#[test]
fn splits_paths() {
    assert_eq!(split(&RPath::root()), (None, ""));
    assert_eq!(split(&RPath::new("/b")), (Some("b"), ""));
    assert_eq!(split(&RPath::new("/b/x/y z")), (Some("b"), "x/y z"));
}

#[test]
fn part_sizes() {
    assert_eq!(part_size_for(20 * MIB, 8 * MIB, 10_000), 8 * MIB);
    // 1 TiB needs > 100 MiB parts to fit in 10 000.
    let p = part_size_for(1 << 40, 8 * MIB, 10_000);
    assert!(p * 10_000 >= 1 << 40 && p.is_multiple_of(MIB), "{p}");
    assert_eq!(part_size_unknown(1, 8 * MIB), 8 * MIB);
    assert_eq!(part_size_unknown(1001, 8 * MIB), 16 * MIB);
    let total: u64 = (1..=10_000).map(|n| part_size_unknown(n, 8 * MIB)).sum();
    assert!(total > 400 * 1024 * MIB);
}

#[test]
fn parses_url_encoded_listing() {
    let doc = r#"<ListBucketResult><EncodingType>url</EncodingType><IsTruncated>true</IsTruncated>
        <NextContinuationToken>tok/1=</NextContinuationToken>
        <Contents><Key>dir/a+b%2Bc.txt</Key><Size>12</Size><LastModified>2009-10-12T17:50:30.000Z</LastModified><ETag>"e"</ETag></Contents>
        <CommonPrefixes><Prefix>dir/sub%20x/</Prefix></CommonPrefixes></ListBucketResult>"#;
    let page = parse_page(&xml::parse(doc).unwrap());
    assert_eq!(page.objects[0].key, "dir/a b+c.txt");
    assert_eq!(page.objects[0].size, 12);
    assert!(page.objects[0].modified.is_some());
    assert_eq!(page.prefixes, ["dir/sub x/"]);
    assert_eq!(page.next.as_deref(), Some("tok/1="));
}

fn creds_region(s: &Seen) -> String {
    let auth = s.header("authorization").unwrap_or_default();
    // Credential=AK/date/REGION/s3/aws4_request
    auth.split("Credential=").nth(1).and_then(|c| c.split('/').nth(2)).unwrap_or_default().to_string()
}

#[test]
fn region_redirect_switches_and_retries_once() {
    let mock = Mock::start(|s| {
        if creds_region(s) != "eu-west-1" {
            return Reply::new(301, "<Error><Code>PermanentRedirect</Code><Message>m</Message></Error>")
                .header("x-amz-bucket-region", "eu-west-1");
        }
        Reply::new(200, "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>f.txt</Key><Size>3</Size></Contents></ListBucketResult>")
    });
    let r = mock_remote(&mock, Limits::default());
    let entries = r.list(&RPath::new("/eu-bucket")).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(r.client.region(Some("eu-bucket")), "eu-west-1");
    // Remembered: the next call goes straight to the right region.
    r.list(&RPath::new("/eu-bucket")).unwrap();
    let seen = mock.seen();
    assert_eq!(seen.len(), 3);
    assert_eq!(creds_region(&seen[2]), "eu-west-1");
}

#[test]
fn authorization_header_malformed_body_region() {
    let mock = Mock::start(|s| {
        if creds_region(s) != "us-west-2" {
            return Reply::new(
                400,
                "<Error><Code>AuthorizationHeaderMalformed</Code><Message>m</Message><Region>us-west-2</Region></Error>",
            );
        }
        Reply::new(200, "<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>")
    });
    let r = mock_remote(&mock, Limits::default());
    assert!(r.list(&RPath::new("/b")).unwrap().is_empty());
    assert_eq!(r.client.region(Some("b")), "us-west-2");
}

#[test]
fn region_loop_stops_after_one_switch() {
    let mock = Mock::start(|_| {
        Reply::new(301, "<Error><Code>PermanentRedirect</Code></Error>").header("x-amz-bucket-region", "elsewhere-1")
    });
    let r = mock_remote(&mock, Limits::default());
    let e = r.list(&RPath::new("/b")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Other);
    assert!(mock.seen().len() <= 3);
}

#[test]
fn listing_pages_with_continuation_tokens() {
    let mock = Mock::start(|s| {
        let page = match s.query("continuation-token").as_deref() {
            None => "<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>p2</NextContinuationToken>\
                     <Contents><Key>d/</Key><Size>0</Size></Contents><Contents><Key>d/a</Key><Size>1</Size></Contents>\
                     <CommonPrefixes><Prefix>d/sub/</Prefix></CommonPrefixes></ListBucketResult>",
            Some("p2") => "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>d/b</Key><Size>2</Size></Contents>\
                           <CommonPrefixes><Prefix>d/sub/</Prefix></CommonPrefixes></ListBucketResult>",
            _ => return Reply::new(400, "<Error><Code>InvalidArgument</Code></Error>"),
        };
        Reply::new(200, page)
    });
    let r = mock_remote(&mock, Limits::default());
    let names: Vec<(String, bool)> =
        r.list(&RPath::new("/b/d")).unwrap().into_iter().map(|e| (e.name.clone(), e.is_dir())).collect();
    assert_eq!(names, [("a".to_string(), false), ("b".into(), false), ("sub".into(), true)]);
    let seen = mock.seen();
    assert_eq!(seen[0].query("prefix").as_deref(), Some("d/"));
    assert_eq!(seen[0].query("delimiter").as_deref(), Some("/"));
}

/// A fake large object: HEAD says 25 MiB; UploadPartCopy answers per part.
#[test]
fn large_copy_uses_upload_part_copy() {
    let mock = Mock::start(|s| match (s.method.as_str(), s.query("uploadId"), s.query("partNumber")) {
        ("HEAD", _, _) => Reply::new(200, "").header("Content-Length", &(25 * MIB).to_string()),
        ("POST", None, _) => {
            Reply::new(200, "<InitiateMultipartUploadResult><UploadId>up-1</UploadId></InitiateMultipartUploadResult>")
        }
        ("PUT", Some(_), Some(n)) => {
            Reply::new(200, format!("<CopyPartResult><ETag>\"etag-{n}\"</ETag></CopyPartResult>"))
        }
        ("POST", Some(_), _) => Reply::new(200, "<CompleteMultipartUploadResult><Key>k</Key></CompleteMultipartUploadResult>"),
        _ => Reply::new(500, ""),
    });
    let limits = Limits { copy_max: 10 * MIB, copy_part: 10 * MIB, ..Limits::default() };
    let r = mock_remote(&mock, limits);
    r.copy(&RPath::new("/src/big file.bin"), &RPath::new("/dst/copy.bin")).unwrap();
    let seen = mock.seen();
    let parts: Vec<&Seen> = seen.iter().filter(|s| s.query("partNumber").is_some()).collect();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0].header("x-amz-copy-source"), Some("/src/big%20file.bin"));
    assert_eq!(parts[0].header("x-amz-copy-source-range"), Some("bytes=0-10485759"));
    assert_eq!(parts[2].header("x-amz-copy-source-range"), Some("bytes=20971520-26214399"));
    let complete = seen.last().unwrap();
    let body = String::from_utf8_lossy(&complete.body);
    assert!(body.contains("<PartNumber>3</PartNumber><ETag>&quot;etag-3&quot;</ETag>"), "{body}");
}

#[test]
fn copy_200_with_error_body_fails() {
    let mock = Mock::start(|s| match s.method.as_str() {
        "HEAD" => Reply::new(200, "").header("Content-Length", "5"),
        _ => Reply::new(200, "<Error><Code>InternalError</Code><Message>try again</Message></Error>"),
    });
    let r = mock_remote(&mock, Limits::default());
    let e = r.copy(&RPath::new("/b/a"), &RPath::new("/b/c")).unwrap_err();
    assert!(e.kind.is_transient(), "{e:?}");
}

/// Multipart flow against the mock: parts sized by the limits, complete with ETags,
/// 200-with-error on complete fails and aborts.
fn multipart_mock(complete_fails: bool) -> (Mock, Arc<AtomicU32>) {
    let parts = Arc::new(AtomicU32::new(0));
    let p2 = parts.clone();
    let mock = Mock::start(move |s| match (s.method.as_str(), s.query("uploadId"), s.query("partNumber")) {
        ("HEAD", _, _) => Reply::new(200, ""),
        ("POST", None, _) => {
            Reply::new(200, "<InitiateMultipartUploadResult><UploadId>up-9</UploadId></InitiateMultipartUploadResult>")
        }
        ("PUT", Some(_), Some(n)) => {
            p2.fetch_add(1, Ordering::SeqCst);
            Reply::new(200, "").header("ETag", &format!("\"p{n}-{}\"", s.body.len()))
        }
        ("POST", Some(_), _) if complete_fails => {
            Reply::new(200, "<Error><Code>InvalidPart</Code><Message>bad</Message></Error>")
        }
        ("POST", Some(_), _) => Reply::new(200, "<CompleteMultipartUploadResult/>"),
        ("DELETE", Some(_), _) => Reply::new(204, ""),
        ("PUT", None, _) => Reply::new(200, "").header("ETag", "\"single\""),
        _ => Reply::new(500, ""),
    });
    (mock, parts)
}

fn small_limits() -> Limits {
    Limits { single_put_below: 64, part_size: 40, ..Limits::default() }
}

#[test]
fn known_size_multipart_streams_exact_parts() {
    let (mock, _) = multipart_mock(false);
    let r = mock_remote(&mock, small_limits());
    let data: Vec<u8> = (0..100u8).collect();
    let mut sink = r.write(&RPath::new("/b/k.bin"), 0, Some(100)).unwrap();
    for chunk in data.chunks(7) {
        sink.write_all(chunk).unwrap();
    }
    sink.finish().unwrap();
    let seen = mock.seen();
    let parts: Vec<&Seen> = seen.iter().filter(|s| s.query("partNumber").is_some()).collect();
    let lens: Vec<usize> = parts.iter().map(|p| p.body.len()).collect();
    assert_eq!(lens, [40, 40, 20]);
    assert_eq!(parts[0].header("x-amz-content-sha256"), Some("UNSIGNED-PAYLOAD"));
    let joined: Vec<u8> = parts.iter().flat_map(|p| p.body.clone()).collect();
    assert_eq!(joined, data);
}

#[test]
fn unknown_size_small_is_single_put_and_large_is_multipart() {
    let (mock, parts) = multipart_mock(false);
    let r = mock_remote(&mock, small_limits());
    let mut sink = r.write(&RPath::new("/b/small"), 0, None).unwrap();
    sink.write_all(&[1u8; 40]).unwrap(); // exactly one part's worth: still a single PUT
    sink.finish().unwrap();
    assert_eq!(parts.load(Ordering::SeqCst), 0);
    let single = mock.seen().into_iter().find(|s| s.method == "PUT").unwrap();
    assert_eq!(single.body.len(), 40);

    let mut sink = r.write(&RPath::new("/b/big"), 0, None).unwrap();
    sink.write_all(&[2u8; 95]).unwrap();
    sink.finish().unwrap();
    assert_eq!(parts.load(Ordering::SeqCst), 3);
}

#[test]
fn complete_with_error_body_fails_and_aborts() {
    let (mock, _) = multipart_mock(true);
    let r = mock_remote(&mock, small_limits());
    let mut sink = r.write(&RPath::new("/b/k"), 0, Some(80)).unwrap();
    sink.write_all(&[0u8; 80]).unwrap();
    let e = sink.finish().unwrap_err();
    assert!(e.message.contains("rejected the uploaded parts"), "{e:?}");
    wait_for(|| mock.seen().iter().any(|s| s.method == "DELETE" && s.query("uploadId").as_deref() == Some("up-9")));
}

#[test]
fn dropped_sink_aborts_multipart() {
    let (mock, _) = multipart_mock(false);
    let r = mock_remote(&mock, small_limits());
    let mut sink = r.write(&RPath::new("/b/k"), 0, Some(100)).unwrap();
    sink.write_all(&[0u8; 50]).unwrap();
    drop(sink);
    wait_for(|| mock.seen().iter().any(|s| s.method == "DELETE" && s.query("uploadId").as_deref() == Some("up-9")));
    assert!(!mock.seen().iter().any(|s| s.method == "POST" && s.query("uploadId").is_some()));
}

#[test]
fn single_put_holds_last_byte_until_finish() {
    let (mock, _) = multipart_mock(false);
    let r = mock_remote(&mock, small_limits());
    let mut sink = r.write(&RPath::new("/b/k"), 0, Some(30)).unwrap();
    sink.write_all(&[9u8; 30]).unwrap();
    drop(sink);
    std::thread::sleep(Duration::from_millis(300));
    assert!(!mock.seen().iter().any(|s| s.method == "PUT"), "a dropped sink must not complete the PUT");
    let mut sink = r.write(&RPath::new("/b/k"), 0, Some(30)).unwrap();
    sink.write_all(&[9u8; 30]).unwrap();
    sink.finish().unwrap();
    let put = mock.seen().into_iter().find(|s| s.method == "PUT").unwrap();
    assert_eq!(put.body, vec![9u8; 30]);
}

#[test]
fn write_more_than_declared_fails() {
    let (mock, _) = multipart_mock(false);
    let r = mock_remote(&mock, small_limits());
    let mut sink = r.write(&RPath::new("/b/k"), 0, Some(10)).unwrap();
    assert!(sink.write_all(&[0u8; 11]).is_err());
    let mut sink = r.write(&RPath::new("/b/k"), 0, Some(10)).unwrap();
    sink.write_all(&[0u8; 5]).unwrap();
    assert!(sink.finish().is_err());
}

#[test]
fn ranged_reads_206_200_with_range_ignored_range_and_eof() {
    let mock = Mock::start(|s| match (s.method.as_str(), s.header("range")) {
        ("GET", Some("bytes=3-")) => Reply::new(206, "lo").header("Content-Range", "bytes 3-4/5"),
        ("GET", Some("bytes=5-")) => Reply::new(416, "<Error><Code>InvalidRange</Code></Error>"),
        ("GET", Some("bytes=2-")) => Reply::new(200, "llo").header("Content-Range", "bytes 2-4/5"),
        ("GET", Some(_)) => Reply::new(200, "hello"),
        ("HEAD", _) => Reply::new(200, "").header("Content-Length", "5"),
        _ => Reply::new(200, "hello"),
    });
    let r = mock_remote(&mock, Limits::default());
    let mut s = String::new();
    r.read(&RPath::new("/b/f"), 3).unwrap().read_to_string(&mut s).unwrap();
    assert_eq!(s, "lo");
    let mut v = Vec::new();
    r.read(&RPath::new("/b/f"), 5).unwrap().read_to_end(&mut v).unwrap();
    assert!(v.is_empty());
    let mut s = String::new();
    r.read(&RPath::new("/b/f"), 2).unwrap().read_to_string(&mut s).unwrap();
    assert_eq!(s, "llo", "200 with a matching Content-Range is the range");
    let mut s = String::new();
    r.read(&RPath::new("/b/f"), 1).unwrap().read_to_string(&mut s).unwrap();
    assert_eq!(s, "ello", "a server ignoring Range sends everything; the start is skipped");
}

/// Folder markers (`name/`), which rclone's S3 server can't store under a bucket.
fn marker_mock(has_child: bool) -> Mock {
    Mock::start(move |s| match (s.method.as_str(), s.path(), s.query("prefix").as_deref()) {
        ("HEAD", "/b/docs", _) => Reply::new(404, ""),
        ("GET", "/b", Some("docs/")) if s.query("delimiter").is_some() => Reply::new(
            200,
            "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>docs/</Key><Size>0</Size></Contents></ListBucketResult>",
        ),
        ("GET", "/b", Some("docs/")) => Reply::new(
            200,
            if has_child {
                "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>docs/</Key><Size>0</Size></Contents>\
                 <Contents><Key>docs/a</Key><Size>1</Size></Contents></ListBucketResult>"
            } else {
                "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>docs/</Key><Size>0</Size></Contents></ListBucketResult>"
            },
        ),
        ("PUT", "/b/new/", _) | ("DELETE", "/b/docs/", _) => Reply::new(200, ""),
        _ => Reply::new(500, ""),
    })
}

#[test]
fn folder_markers() {
    let mock = marker_mock(false);
    let r = mock_remote(&mock, Limits::default());
    assert!(r.stat(&RPath::new("/b/docs")).unwrap().is_dir());
    assert!(r.list(&RPath::new("/b/docs")).unwrap().is_empty(), "own marker isn't a child");
    r.mkdir(&RPath::new("/b/new")).unwrap();
    r.remove_dir(&RPath::new("/b/docs")).unwrap();
    let seen = mock.seen();
    assert!(seen.iter().any(|s| s.method == "PUT" && s.path() == "/b/new/" && s.body.is_empty()));
    assert!(seen.iter().any(|s| s.method == "DELETE" && s.path() == "/b/docs/"));

    let mock = marker_mock(true);
    let r = mock_remote(&mock, Limits::default());
    assert_eq!(r.remove_dir(&RPath::new("/b/docs")).unwrap_err().kind, ErrorKind::InvalidPath);
    assert!(!mock.seen().iter().any(|s| s.method == "DELETE"));
}

#[test]
fn slow_down_is_rate_limited_with_retry_after() {
    let mock = Mock::start(|_| {
        Reply::new(503, "<Error><Code>SlowDown</Code><Message>Please reduce your request rate.</Message></Error>")
            .header("Retry-After", "3")
    });
    let r = mock_remote(&mock, Limits::default());
    let e = r.list(&RPath::new("/b")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::RateLimited);
    assert_eq!(e.retry_after, Some(Duration::from_secs(3)));
}

#[test]
fn clock_skew_message() {
    let mock = Mock::start(|_| Reply::new(403, "<Error><Code>RequestTimeTooSkewed</Code></Error>"));
    let r = mock_remote(&mock, Limits::default());
    let e = r.list(&RPath::new("/b")).unwrap_err();
    assert!(e.message.contains("clock"), "{}", e.message);
}

#[test]
fn closed_remote_refuses_calls() {
    let mock = Mock::start(|_| Reply::new(200, ""));
    let r = mock_remote(&mock, Limits::default());
    r.close();
    assert_eq!(r.list(&RPath::root()).unwrap_err().kind, ErrorKind::Closed);
}

#[test]
fn missing_secret_is_auth() {
    let none = |_: &SecretRef| -> filewinch_core::Result<Option<String>> { Ok(None) };
    let e = S3Remote::open(&site(""), &none, Http::new(), Limits::default()).err().unwrap();
    assert_eq!(e.kind, ErrorKind::Auth);
}

fn wait_for(cond: impl Fn() -> bool) {
    let end = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < end {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("condition not met in 30 s");
}

const MD5_HELLO: &str = "5d41402abc4b2a76b9719d911017c592";

#[test]
fn listing_reports_md5_for_single_part_etags_only() {
    let mock = Mock::start(|_| {
        Reply::new(
            200,
            format!(
                "<ListBucketResult><IsTruncated>false</IsTruncated>\
                 <Contents><Key>plain</Key><Size>5</Size><ETag>&quot;{}&quot;</ETag></Contents>\
                 <Contents><Key>multi</Key><Size>5</Size><ETag>\"{MD5_HELLO}-3\"</ETag></Contents>\
                 <Contents><Key>odd</Key><Size>5</Size><ETag>\"abc\"</ETag></Contents>\
                 <Contents><Key>none</Key><Size>5</Size></Contents>\
                 <CommonPrefixes><Prefix>sub/</Prefix></CommonPrefixes></ListBucketResult>",
                MD5_HELLO.to_uppercase()
            ),
        )
    });
    let r = mock_remote(&mock, Limits::default());
    let list = r.list(&RPath::new("/b")).unwrap();
    let sums = |n: &str| list.iter().find(|e| e.name == n).unwrap().checksums.clone();
    let md5 = filewinch_core::Checksum { algo: filewinch_core::HashAlgo::Md5, hex: MD5_HELLO.into() };
    assert_eq!(sums("plain"), [md5]);
    for n in ["multi", "odd", "none", "sub"] {
        assert!(sums(n).is_empty(), "{n}: {:?}", sums(n));
    }
}

#[test]
fn head_reports_md5_unless_multipart_or_kms_or_sse_c() {
    let mock = Mock::start(|s| {
        let ok = |etag: &str| Reply::new(200, "").header("Content-Length", "5").header("ETag", etag);
        let plain = format!("\"{MD5_HELLO}\"");
        match s.path() {
            "/b/plain" => ok(&plain),
            "/b/sse-s3" => ok(&plain).header("x-amz-server-side-encryption", "AES256"),
            "/b/multi" => ok(&format!("\"{MD5_HELLO}-12\"")),
            "/b/kms" => ok(&plain).header("x-amz-server-side-encryption", "aws:kms"),
            "/b/dsse" => ok(&plain).header("x-amz-server-side-encryption", "aws:kms:dsse"),
            "/b/sse-c" => ok(&plain).header("x-amz-server-side-encryption-customer-algorithm", "AES256"),
            "/b/no-etag" => Reply::new(200, "").header("Content-Length", "5"),
            _ => Reply::new(404, ""),
        }
    });
    let r = mock_remote(&mock, Limits::default());
    let stat = |k: &str| r.stat(&RPath::new(&format!("/b/{k}"))).unwrap().checksums;
    let md5 = filewinch_core::Checksum { algo: filewinch_core::HashAlgo::Md5, hex: MD5_HELLO.into() };
    assert_eq!(stat("plain"), std::slice::from_ref(&md5));
    assert_eq!(stat("sse-s3"), [md5]);
    for k in ["multi", "kms", "dsse", "sse-c", "no-etag"] {
        assert!(stat(k).is_empty(), "{k}: {:?}", stat(k));
    }
}
