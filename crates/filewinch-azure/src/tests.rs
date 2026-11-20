//! Unit tests: site parsing, XML, error mapping, block sizing, the held-back-byte reader.

use std::io::Read;

use filewinch_core::{Auth, ErrorKind, Protocol, RPath, Result, SecretRef, Site};
use filewinch_http::ureq::http::HeaderMap;

use crate::config::{self, Credential};
use crate::errors::{Ctx, azure_error, redact_signature};
use crate::ops::valid_container_name;
use crate::upload::{BLOCK_MAX, BLOCK_MIN, HoldLast, MAX_BLOCKS, block_id, block_size, growing_block_size};
use crate::xml;

const KEY: &str = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";

fn site(host: &str, user: &str, auth: Auth, initial: Option<&str>) -> Site {
    let mut s = Site::new("t", "t", Protocol::AzureBlob);
    s.host = host.into();
    s.user = user.into();
    s.auth = auth;
    s.initial_path = initial.map(str::to_string);
    s
}

fn pw() -> Auth {
    Auth::Password { secret: SecretRef("k".into()) }
}

fn parse_with(s: &Site, secret: &str) -> Result<config::Config> {
    let secret = secret.to_string();
    config::parse(s, &move |_| Ok(Some(secret.clone())))
}

#[test]
fn bare_account_name() {
    let c = parse_with(&site("myaccount", "", pw(), None), KEY).unwrap();
    assert_eq!(c.origin, "https://myaccount.blob.core.windows.net");
    assert_eq!(c.account, "myaccount");
    assert_eq!(c.path_prefix, "");
    assert!(matches!(c.credential, Credential::SharedKey(ref k) if k.len() == 64));
    assert_eq!(c.home, RPath::root());
    assert!(c.scoped.is_none());
}

#[test]
fn azurite_path_style_url() {
    let c = parse_with(&site("http://127.0.0.1:10000/devstoreaccount1", "", pw(), Some("photos/2024")), KEY).unwrap();
    assert_eq!(c.origin, "http://127.0.0.1:10000");
    assert_eq!(c.account, "devstoreaccount1");
    assert_eq!(c.path_prefix, "/devstoreaccount1");
    assert_eq!(c.home.as_str(), "/photos/2024");
    // Account from `user` when the URL has no path; the port can come from the site.
    let mut s = site("http://localhost", "devstoreaccount1", pw(), None);
    s.port = Some(10001);
    let c = parse_with(&s, KEY).unwrap();
    assert_eq!(c.origin, "http://localhost:10001");
    assert_eq!(c.path_prefix, "/devstoreaccount1");
}

#[test]
fn host_name_and_url_with_container() {
    let c = parse_with(&site("https://acct.blob.core.windows.net/box/sub", "", pw(), None), KEY).unwrap();
    assert_eq!(c.account, "acct");
    assert_eq!(c.home.as_str(), "/box/sub");
    assert!(c.scoped.is_none(), "Shared Key is never scoped");
    let c = parse_with(&site("acct.blob.core.windows.net", "", pw(), Some("/box/")), KEY).unwrap();
    assert_eq!(c.origin, "https://acct.blob.core.windows.net");
    assert_eq!(c.home.as_str(), "/box");
}

#[test]
fn container_sas_url_is_scoped() {
    let url = "https://acct.blob.core.windows.net/box?sv=2021-08-06&sr=c&sp=rl&sig=abc%2Bdef%3D";
    let c = parse_with(&site(url, "", Auth::None, None), "").unwrap();
    assert!(matches!(c.credential, Credential::Sas(ref q) if q.ends_with("sig=abc%2Bdef%3D")));
    assert_eq!(c.scoped.as_deref(), Some("box"));
    assert_eq!(c.home.as_str(), "/box");
    // An initial folder outside the scoped container falls back to the container.
    let c = parse_with(&site(url, "", Auth::None, Some("other/x")), "").unwrap();
    assert_eq!(c.home.as_str(), "/box");
}

#[test]
fn sas_token_as_secret() {
    // Account SAS in the password field: not scoped.
    let c = parse_with(&site("acct", "", pw(), None), "?sv=2021-08-06&ss=b&srt=sco&sp=rl&sig=x").unwrap();
    assert!(matches!(c.credential, Credential::Sas(ref q) if q.starts_with("sv=")));
    assert!(c.scoped.is_none());
    // Container SAS in the password field: scoped to the initial folder's container.
    let c = parse_with(&site("acct", "acct", pw(), Some("box/a")), "sv=1&sr=c&sig=x").unwrap();
    assert_eq!(c.scoped.as_deref(), Some("box"));
    assert_eq!(c.home.as_str(), "/box/a");
}

#[test]
fn bad_credentials_are_explained() {
    let e = parse_with(&site("acct", "", pw(), None), "not base64 !!").err().unwrap();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.message.contains("account key for acct isn't valid"), "{}", e.message);
    let e = parse_with(&site("acct", "", pw(), None), "").err().unwrap();
    assert_eq!(e.kind, ErrorKind::Auth);
    let e = parse_with(&site("acct", "", pw(), None), "?sv=1&sp=r").err().unwrap();
    assert!(e.message.contains("sig="), "{}", e.message);
    let e = parse_with(&site("acct", "", pw(), None), "sv=1&sr=c&sig=x").err().unwrap();
    assert!(e.message.contains("container"), "{}", e.message);
    let e = parse_with(&site("acct", "", pw(), Some("b")), "sv=1&sr=b&sig=x").err().unwrap();
    assert_eq!(e.kind, ErrorKind::Unsupported);
    let e = parse_with(&site("acct", "", Auth::None, None), "").err().unwrap();
    assert_eq!(e.kind, ErrorKind::Auth);
    let e = parse_with(&site("ftp://x", "", pw(), None), KEY).err().unwrap();
    assert_eq!(e.kind, ErrorKind::InvalidPath);
    let e = parse_with(&site("", "", pw(), None), KEY).err().unwrap();
    assert_eq!(e.kind, ErrorKind::InvalidPath);
}

#[test]
fn anonymous_is_scoped_to_the_initial_container() {
    let c = parse_with(&site("acct", "", Auth::None, Some("public/x")), "").unwrap();
    assert!(matches!(c.credential, Credential::Anonymous));
    assert_eq!(c.scoped.as_deref(), Some("public"));
}

#[test]
fn list_blobs_xml() {
    let body = "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?>\
        <EnumerationResults ServiceEndpoint=\"x\" ContainerName=\"c\"><Prefix>a/</Prefix>\
        <Blobs><Blob><Name>a/</Name><Properties><Content-Length>0</Content-Length></Properties></Blob>\
        <Blob><Name>a/R&amp;D.txt</Name><Properties><Last-Modified>Fri, 20 Nov 2026 10:00:00 GMT</Last-Modified>\
        <Etag>0x1</Etag><Content-Length>12</Content-Length><Content-Type>text/plain</Content-Type>\
        <AccessTier>Hot</AccessTier></Properties></Blob>\
        <BlobPrefix><Name>a/sub/</Name></BlobPrefix></Blobs><NextMarker>2!abc</NextMarker></EnumerationResults>";
    let page = xml::blobs(body).unwrap();
    assert_eq!(page.blobs.len(), 2);
    assert_eq!(page.blobs[1].name, "a/R&D.txt");
    assert_eq!(page.blobs[1].size, Some(12));
    assert!(page.blobs[1].modified.is_some());
    assert_eq!(page.blobs[1].tier.as_deref(), Some("Hot"));
    assert_eq!(page.prefixes, vec!["a/sub/".to_string()]);
    assert_eq!(page.next_marker.as_deref(), Some("2!abc"));
    let empty = xml::blobs("<EnumerationResults><Blobs/><NextMarker/></EnumerationResults>").unwrap();
    assert!(empty.blobs.is_empty() && empty.next_marker.is_none());
    assert!(xml::blobs("<oops").is_err());
}

#[test]
fn content_md5_needs_16_bytes() {
    let want = filewinch_core::Checksum {
        algo: filewinch_core::HashAlgo::Md5,
        hex: "5d41402abc4b2a76b9719d911017c592".into(),
    };
    assert_eq!(xml::content_md5(Some("XUFAKrxLKna5cZ2REBfFkg==")), Some(want.clone()));
    assert_eq!(xml::content_md5(Some(" XUFAKrxLKna5cZ2REBfFkg== ")), Some(want));
    assert_eq!(xml::content_md5(Some("XUFAKrxLKna5cZ2REBfF")), None, "15 bytes");
    assert_eq!(xml::content_md5(Some("XUFAKrxLKna5cZ2REBfFkgAA")), None, "17 bytes");
    assert_eq!(xml::content_md5(Some("")), None);
    assert_eq!(xml::content_md5(Some("%%%")), None);
    assert_eq!(xml::content_md5(None), None);
}

#[test]
fn list_containers_xml() {
    let body = "<EnumerationResults><Containers><Container><Name>one</Name><Properties>\
        <Last-Modified>Fri, 20 Nov 2026 10:00:00 GMT</Last-Modified><Etag>\"0x2\"</Etag></Properties></Container>\
        <Container><Name>two</Name></Container></Containers><NextMarker /></EnumerationResults>";
    let (items, next) = xml::containers(body).unwrap();
    assert_eq!(items.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["one", "two"]);
    assert!(items[0].modified.is_some());
    assert!(next.is_none());
}

fn err(status: u16, code: Option<&str>, body: &str, sas: bool) -> filewinch_core::Error {
    let mut h = HeaderMap::new();
    if let Some(c) = code {
        h.insert("x-ms-error-code", c.parse().unwrap());
    }
    azure_error(status, &h, body, &Ctx { what: "list “box”", account: "acct", sas })
}

#[test]
fn error_codes_map_to_kinds() {
    let e = err(403, Some("AuthenticationFailed"), "", false);
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.message.starts_with("The account key for acct was rejected"), "{}", e.message);
    let skew = "<Error><Code>AuthenticationFailed</Code><Message>Server failed</Message>\
        <AuthenticationErrorDetail>Request date header too old: 'Fri, 20 Nov 2026'</AuthenticationErrorDetail></Error>";
    assert!(err(403, None, skew, false).message.contains("clock"));
    let e = err(403, Some("AuthenticationFailed"), "", true);
    assert!(e.message.contains("SAS token for acct"), "{}", e.message);
    assert_eq!(err(403, Some("AuthorizationPermissionMismatch"), "", false).kind, ErrorKind::PermissionDenied);
    assert_eq!(err(403, Some("AuthorizationFailure"), "", true).kind, ErrorKind::PermissionDenied);
    // Azurite's spelling of a bad signature.
    let azurite = "<Error><Code>AuthorizationFailure</Code><Message>Server failed to authenticate the request.</Message></Error>";
    assert_eq!(err(403, Some("AuthorizationFailure"), azurite, false).kind, ErrorKind::Auth);
    assert_eq!(err(404, Some("ContainerNotFound"), "", false).kind, ErrorKind::NotFound);
    assert_eq!(err(404, Some("BlobNotFound"), "", false).kind, ErrorKind::NotFound);
    assert_eq!(err(409, Some("ContainerAlreadyExists"), "", false).kind, ErrorKind::AlreadyExists);
    assert_eq!(err(409, Some("BlobAlreadyExists"), "", false).kind, ErrorKind::AlreadyExists);
    assert_eq!(err(400, Some("InvalidResourceName"), "", false).kind, ErrorKind::InvalidPath);
    let busy = err(503, Some("ServerBusy"), "", false);
    assert_eq!(busy.kind, ErrorKind::RateLimited);
    assert_eq!(err(429, None, "", false).kind, ErrorKind::RateLimited);
    assert_eq!(err(500, Some("InternalError"), "", false).kind, ErrorKind::Network);
    assert_eq!(err(409, Some("ContainerBeingDeleted"), "", false).kind, ErrorKind::RateLimited);
    // Code from the body when the header is missing.
    let body = "<?xml version=\"1.0\"?><Error><Code>ContainerNotFound</Code><Message>no</Message></Error>";
    assert_eq!(err(404, None, body, false).kind, ErrorKind::NotFound);
    // HEAD: no body, no code.
    assert_eq!(err(404, None, "", false).kind, ErrorKind::NotFound);
    assert_eq!(err(401, None, "", false).kind, ErrorKind::Auth);
}

#[test]
fn signatures_never_reach_details() {
    let body = "<Error><Code>AuthenticationFailed</Code><AuthenticationErrorDetail>The MAC signature found in the \
        HTTP request 'c2VjcmV0c2lnbmF0dXJl' is not the same as any computed signature.</AuthenticationErrorDetail>\
        <Url>https://a/b?sv=1&amp;sig=TOPSECRET&amp;se=2</Url></Error>";
    let e = err(403, None, body, false);
    let all = format!("{} {}", e.message, e.detail.clone().unwrap_or_default());
    assert!(!all.contains("c2VjcmV0c2lnbmF0dXJl"), "{all}");
    assert!(!all.contains("TOPSECRET"), "{all}");
    assert_eq!(redact_signature("x HTTP request 'abc' y"), "x HTTP request 'REDACTED' y");
}

#[test]
fn block_sizes() {
    assert_eq!(block_size(0).unwrap(), BLOCK_MIN);
    assert_eq!(block_size(100 * 1024 * 1024 * 1024).unwrap(), BLOCK_MIN);
    let tib = 1024u64.pow(4);
    let b = block_size(tib).unwrap();
    assert!(b > BLOCK_MIN && b.is_multiple_of(1024 * 1024) && tib.div_ceil(b) <= MAX_BLOCKS);
    assert!(block_size(BLOCK_MAX * MAX_BLOCKS).is_ok());
    assert_eq!(block_size(BLOCK_MAX * MAX_BLOCKS + 1).unwrap_err().kind, ErrorKind::NoSpace);
    assert_eq!(growing_block_size(0), BLOCK_MIN);
    assert_eq!(growing_block_size(10_000), 2 * BLOCK_MIN);
    assert!(growing_block_size(49_999) <= BLOCK_MAX);
}

#[test]
fn block_ids_have_equal_length() {
    let a = block_id(u64::MAX, 0);
    let b = block_id(1, 49_999);
    assert_eq!(a.len(), b.len());
    assert_ne!(block_id(1, 1), block_id(2, 1));
}

#[test]
fn container_names() {
    for ok in ["abc", "a-b-c", "photos2024", &"a".repeat(63)] {
        assert!(valid_container_name(ok), "{ok}");
    }
    for bad in ["ab", "Abc", "-ab", "ab-", "a--b", "a_b", &"a".repeat(64)] {
        assert!(!valid_container_name(bad), "{bad}");
    }
}

/// A reader handing out fixed-size chunks, to exercise HoldLast with odd buffer sizes.
struct Chunks(Vec<u8>, usize);

impl Read for Chunks {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = out.len().min(self.0.len()).min(self.1);
        out[..n].copy_from_slice(&self.0[..n]);
        self.0.drain(..n);
        Ok(n)
    }
}

#[test]
fn hold_last_passes_every_byte_through() {
    let data: Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
    for chunk in [1, 2, 3, 64, 2000] {
        for buf_len in [1, 2, 5, 4096] {
            let mut r = HoldLast::new(Chunks(data.clone(), chunk));
            let mut out = Vec::new();
            let mut buf = vec![0u8; buf_len];
            loop {
                let n = r.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
            assert_eq!(out, data, "chunk {chunk} buf {buf_len}");
        }
    }
}

/// The last byte is only released once the source reports EOF; an error instead of EOF
/// means the body never completes.
#[test]
fn hold_last_withholds_until_eof() {
    struct ThenFail(bool);
    impl Read for ThenFail {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.0 {
                return Err(std::io::Error::other("dropped"));
            }
            self.0 = true;
            out[..3].copy_from_slice(b"abc");
            Ok(3)
        }
    }
    let mut r = HoldLast::new(ThenFail(false));
    let mut buf = [0u8; 16];
    assert_eq!(r.read(&mut buf).unwrap(), 2);
    assert!(r.read(&mut buf).is_err());
}
