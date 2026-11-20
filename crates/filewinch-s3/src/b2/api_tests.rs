use super::*;
use filewinch_http::ureq::http::HeaderValue;

fn kind(status: u16, code: &str) -> (ErrorKind, bool) {
    let body = format!("{{\"status\":{status},\"code\":\"{code}\",\"message\":\"m\"}}");
    match classify(status, &HeaderMap::new(), &body, "list /b") {
        Failure::TokenExpired(e) => (e.kind, true),
        Failure::Error(e) => (e.kind, false),
    }
}

#[test]
fn b2_codes() {
    assert_eq!(kind(401, "expired_auth_token"), (ErrorKind::Auth, true));
    assert_eq!(kind(401, "bad_auth_token"), (ErrorKind::Auth, true));
    assert_eq!(kind(401, "unauthorized"), (ErrorKind::PermissionDenied, false));
    assert_eq!(kind(403, "storage_cap_exceeded"), (ErrorKind::NoSpace, false));
    assert_eq!(kind(403, "transaction_cap_exceeded"), (ErrorKind::Other, false));
    assert_eq!(kind(404, "not_found"), (ErrorKind::NotFound, false));
    assert_eq!(kind(400, "file_not_present"), (ErrorKind::NotFound, false));
    assert_eq!(kind(400, "duplicate_bucket_name"), (ErrorKind::AlreadyExists, false));
    assert_eq!(kind(400, "cannot_delete_non_empty_bucket"), (ErrorKind::InvalidPath, false));
    assert_eq!(kind(429, "too_many_requests"), (ErrorKind::RateLimited, false));
    assert_eq!(kind(503, "service_unavailable"), (ErrorKind::RateLimited, false));
    assert_eq!(kind(500, "internal_error"), (ErrorKind::Network, false));
    assert_eq!(kind(408, "request_timeout"), (ErrorKind::Network, false));
}

#[test]
fn retry_after_and_message() {
    let mut h = HeaderMap::new();
    h.insert("retry-after", HeaderValue::from_static("9"));
    let Failure::Error(e) = classify(429, &h, "{}", "upload x") else { panic!() };
    assert_eq!(e.retry_after, Some(Duration::from_secs(9)));
    let body = r#"{"status":400,"code":"bad_request","message":"File names must not end with a slash"}"#;
    let Failure::Error(e) = classify(400, &HeaderMap::new(), body, "upload x/") else { panic!() };
    assert_eq!(e.message, "Couldn't upload x/: File names must not end with a slash.");
}

#[test]
fn host_names() {
    assert_eq!(host_of("https://api005.backblazeb2.com/b2api"), "api005.backblazeb2.com");
    assert_eq!(host_of("http://127.0.0.1:9/"), "127.0.0.1:9");
}
