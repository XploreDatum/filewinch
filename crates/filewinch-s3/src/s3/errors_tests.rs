use super::*;
use filewinch_http::ureq::http::{HeaderMap, HeaderValue};

fn err(status: u16, code: &str) -> Error {
    let body = format!("<?xml version=\"1.0\"?><Error><Code>{code}</Code><Message>m</Message></Error>");
    map_error(status, &HeaderMap::new(), &body, "list /b", "s3.example.com")
}

#[test]
fn codes_map_to_kinds() {
    assert_eq!(err(403, "AccessDenied").kind, ErrorKind::PermissionDenied);
    let auth = err(403, "SignatureDoesNotMatch");
    assert_eq!(auth.kind, ErrorKind::Auth);
    assert_eq!(auth.message, "The server rejected the access key or secret for s3.example.com.");
    assert_eq!(err(403, "InvalidAccessKeyId").kind, ErrorKind::Auth);
    assert_eq!(err(404, "NoSuchBucket").kind, ErrorKind::NotFound);
    assert_eq!(err(404, "NoSuchKey").kind, ErrorKind::NotFound);
    assert_eq!(err(409, "BucketAlreadyOwnedByYou").kind, ErrorKind::AlreadyExists);
    assert_eq!(err(409, "BucketAlreadyExists").kind, ErrorKind::AlreadyExists);
    assert_eq!(err(409, "BucketNotEmpty").kind, ErrorKind::InvalidPath);
    assert_eq!(err(400, "EntityTooLarge").kind, ErrorKind::NoSpace);
    assert_eq!(err(403, "QuotaExceeded").kind, ErrorKind::NoSpace);
    let skew = err(403, "RequestTimeTooSkewed");
    assert_eq!(skew.kind, ErrorKind::Other);
    assert!(skew.message.contains("clock"), "{}", skew.message);
    let slow = err(503, "SlowDown");
    assert_eq!(slow.kind, ErrorKind::RateLimited);
    assert_eq!(slow.retry_after, Some(SLOW_DOWN_DEFAULT));
}

#[test]
fn retry_after_header_is_kept() {
    let mut h = HeaderMap::new();
    h.insert("retry-after", HeaderValue::from_static("7"));
    let e = map_error(503, &h, "<Error><Code>SlowDown</Code></Error>", "upload /b/k", "h");
    assert_eq!(e.kind, ErrorKind::RateLimited);
    assert_eq!(e.retry_after, Some(Duration::from_secs(7)));
    let e = map_error(503, &h, "", "upload /b/k", "h");
    assert_eq!(e.kind, ErrorKind::RateLimited);
}

#[test]
fn head_responses_map_by_status() {
    let h = HeaderMap::new();
    assert_eq!(map_error(404, &h, "", "get info for /b/k", "h").kind, ErrorKind::NotFound);
    assert_eq!(map_error(403, &h, "", "get info for /b/k", "h").kind, ErrorKind::PermissionDenied);
    assert_eq!(map_error(401, &h, "", "x", "h").kind, ErrorKind::Auth);
    assert_eq!(map_error(500, &h, "", "x", "h").kind, ErrorKind::Network);
}

#[test]
fn error_body_parse_and_secrets_not_echoed() {
    let body = "<Error><Code>AuthorizationHeaderMalformed</Code><Message>wrong</Message><Region>eu-west-1</Region>\
                <SignatureProvided>abcdef0123</SignatureProvided></Error>";
    let p = parse_error(body).unwrap();
    assert_eq!(p.code, "AuthorizationHeaderMalformed");
    assert_eq!(p.region.as_deref(), Some("eu-west-1"));
    let e = map_error(403, &HeaderMap::new(), body, "x", "h");
    assert!(!e.detail.unwrap_or_default().contains("abcdef0123"));
    assert!(parse_error("<ListBucketResult/>").is_none());
    assert!(parse_error("not xml").is_none());
}
