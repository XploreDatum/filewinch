//! HTTP status → core error mapping, Retry-After, detail truncation and scrubbing, and the
//! io::Error round trip.

use std::time::{Duration, SystemTime};

use filewinch_core::{Error, ErrorKind};
use filewinch_http::{MAX_DETAIL, from_io, retry_after, status_error, status_error_parts, to_io};
use ureq::http::{HeaderMap, HeaderValue, Response};

fn resp(status: u16, headers: &[(&'static str, &str)], body: &[u8]) -> Response<ureq::Body> {
    let mut b = Response::builder().status(status);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(ureq::Body::builder().data(body.to_vec())).unwrap()
}

#[test]
fn status_table() {
    use ErrorKind::*;
    let table: &[(u16, ErrorKind)] = &[
        (400, Other),
        (401, Auth),
        (403, PermissionDenied),
        (404, NotFound),
        (410, NotFound),
        (405, Unsupported),
        (408, Network),
        (409, InvalidPath),
        (412, AlreadyExists),
        (413, NoSpace),
        (507, NoSpace),
        (416, InvalidPath),
        (423, PermissionDenied),
        (429, RateLimited),
        (500, Network),
        (502, Network),
        (504, Network),
        (503, Network),
        (599, Network),
        (418, Other),
        (302, Other),
    ];
    for &(code, kind) in table {
        let err = status_error(resp(code, &[], b""), "list /photos");
        assert_eq!(err.kind, kind, "HTTP {code}");
        assert!(
            err.message.starts_with("Couldn't list /photos: "),
            "{}",
            err.message
        );
        assert!(err.message.ends_with('.'), "{}", err.message);
        assert_eq!(err.detail.as_deref(), Some(format!("HTTP {code}").as_str()));
        assert_eq!(err.retry_after, None);
    }
    let not_found = status_error(resp(404, &[], b""), "download a.txt");
    assert_eq!(
        not_found.message,
        "Couldn't download a.txt: the server says the file or folder wasn't found."
    );
    assert!(
        status_error(resp(413, &[], b""), "upload x")
            .message
            .contains("too large")
    );
    assert!(
        status_error(resp(423, &[], b""), "delete x")
            .message
            .contains("locked")
    );
}

#[test]
fn bad_request_mentioning_invalid_name_is_invalid_path() {
    let body = b"<Error><Code>InvalidObjectName</Code></Error>";
    assert_eq!(
        status_error(resp(400, &[], body), "upload x").kind,
        ErrorKind::InvalidPath
    );
    let body = b"{\"error\":\"Invalid key: too long\"}";
    assert_eq!(
        status_error(resp(400, &[], body), "upload x").kind,
        ErrorKind::InvalidPath
    );
}

#[test]
fn retry_after_seconds_and_http_date() {
    let err = status_error(resp(429, &[("Retry-After", "120")], b""), "list /");
    assert_eq!(err.retry_after, Some(Duration::from_secs(120)));

    let when = SystemTime::now() + Duration::from_secs(90);
    let date = httpdate::fmt_http_date(when);
    let err = status_error(resp(503, &[("Retry-After", &date)], b""), "list /");
    assert_eq!(err.kind, ErrorKind::RateLimited);
    let after = err.retry_after.unwrap();
    assert!(
        after > Duration::from_secs(85) && after <= Duration::from_secs(90),
        "{after:?}"
    );

    // Present on a non-rate-limit status too.
    let err = status_error(resp(500, &[("Retry-After", "5")], b""), "list /");
    assert_eq!(err.kind, ErrorKind::Network);
    assert_eq!(err.retry_after, Some(Duration::from_secs(5)));

    let mut h = HeaderMap::new();
    h.insert(
        "retry-after",
        HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
    );
    assert_eq!(
        retry_after(&h),
        Some(Duration::ZERO),
        "a past date means now"
    );
    h.insert("retry-after", HeaderValue::from_static("soon"));
    assert_eq!(retry_after(&h), None);
    assert_eq!(retry_after(&HeaderMap::new()), None);
}

#[test]
fn detail_is_truncated_at_4_kib() {
    let body = "x".repeat(100_000);
    let err = status_error(resp(500, &[], body.as_bytes()), "list /");
    let detail = err.detail.unwrap();
    let text = detail.strip_prefix("HTTP 500: ").unwrap();
    assert_eq!(text.len(), MAX_DETAIL);

    // Multi-byte characters are never split.
    let body = "é".repeat(5000);
    let err = status_error_parts(500, &HeaderMap::new(), &body, "list /");
    let text = err
        .detail
        .unwrap()
        .strip_prefix("HTTP 500: ")
        .unwrap()
        .to_string();
    assert!(text.len() <= MAX_DETAIL && text.chars().all(|c| c == 'é'));
}

#[test]
fn detail_is_trimmed_and_scrubbed_of_signatures_and_tokens() {
    let body = "  <Error><Code>AuthenticationFailed</Code>\
        <Message>see https://acct.blob.core.windows.net/c/b?sv=2024&sig=SECRETSIG&se=x</Message>\
        <SignatureProvided>SECRETPROVIDED</SignatureProvided>\
        <X-Amz-Security-Token>SECRETTOKEN</X-Amz-Security-Token>\
        X-Amz-Signature=SECRETAMZ&X-Amz-Credential=SECRETCRED access_token=SECRETBEARER</Error>\n";
    let err = status_error(resp(403, &[], body.as_bytes()), "list /");
    let detail = err.detail.unwrap();
    assert!(!detail.contains("SECRET"), "{detail}");
    assert!(
        detail.contains("AuthenticationFailed") && detail.contains("sv=2024"),
        "{detail}"
    );
    assert!(
        detail.starts_with("HTTP 403: <Error>") && detail.ends_with("</Error>"),
        "{detail}"
    );
}

#[test]
fn io_round_trip_keeps_kind_and_message() {
    let cases = [
        (ErrorKind::Network, std::io::ErrorKind::ConnectionReset),
        (ErrorKind::NotFound, std::io::ErrorKind::NotFound),
        (
            ErrorKind::PermissionDenied,
            std::io::ErrorKind::PermissionDenied,
        ),
        (ErrorKind::Auth, std::io::ErrorKind::PermissionDenied),
        (ErrorKind::NoSpace, std::io::ErrorKind::StorageFull),
        (ErrorKind::Cancelled, std::io::ErrorKind::Other),
        (ErrorKind::HostTrust, std::io::ErrorKind::Other),
    ];
    for (kind, io_kind) in cases {
        let original = Error::new(kind, "Couldn't upload a.txt: something.")
            .with_detail("HTTP 999")
            .with_retry_after(Duration::from_secs(3));
        let io = to_io(original.clone());
        assert_eq!(io.kind(), io_kind, "{kind:?}");
        let back = from_io(io);
        assert_eq!(back.kind, kind);
        assert_eq!(back.message, original.message);
        assert_eq!(back.detail, original.detail);
        assert_eq!(back.retry_after, original.retry_after);
    }
    let timed_out = to_io(Error::new(
        ErrorKind::Network,
        "The connection to h timed out.",
    ));
    assert_eq!(timed_out.kind(), std::io::ErrorKind::TimedOut);

    // A plain io::Error falls back to core's conversion.
    let plain = from_io(std::io::Error::new(
        std::io::ErrorKind::ConnectionReset,
        "reset",
    ));
    assert_eq!(plain.kind, ErrorKind::Network);
}
