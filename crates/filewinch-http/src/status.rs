//! Mapping non-success HTTP statuses to core errors.

use std::io::Read;
use std::time::{Duration, SystemTime};

use filewinch_core::{Error, ErrorKind};
use ureq::http::{HeaderMap, Response};

/// Most of an error body that goes into `Error::detail`.
pub const MAX_DETAIL: usize = 4096;

/// Maps a non-success HTTP status to a core Error, reading at most 4 KiB of the body for
/// `detail`. `what` is a human phrase like "list /photos" or "upload report.pdf".
///
/// Backends that need to parse the error body themselves (S3 `<Code>`, Azure `x-ms-error-code`)
/// read it first and call [`status_error_parts`] instead.
pub fn status_error(resp: Response<ureq::Body>, what: &str) -> Error {
    let (parts, body) = resp.into_parts();
    let mut buf = Vec::new();
    // A failed body read only loses the detail; the status alone decides the error.
    let _ = body
        .into_reader()
        .take(MAX_DETAIL as u64)
        .read_to_end(&mut buf);
    status_error_parts(
        parts.status.as_u16(),
        &parts.headers,
        &String::from_utf8_lossy(&buf),
        what,
    )
}

/// [`status_error`] for a response whose body the caller has already read.
pub fn status_error_parts(status: u16, headers: &HeaderMap, body: &str, what: &str) -> Error {
    let retry = retry_after(headers);
    let lower = body.to_ascii_lowercase();
    let (kind, says) = match status {
        400 if mentions_invalid_name(&lower) => (
            ErrorKind::InvalidPath,
            "the server says the name isn't allowed".to_string(),
        ),
        400 => (ErrorKind::Other, "the server rejected the request".into()),
        401 => (ErrorKind::Auth, "the server didn't accept the login".into()),
        403 => (
            ErrorKind::PermissionDenied,
            "the server says you don't have permission".into(),
        ),
        404 | 410 => (
            ErrorKind::NotFound,
            "the server says the file or folder wasn't found".into(),
        ),
        405 => (
            ErrorKind::Unsupported,
            "the server doesn't allow this operation".into(),
        ),
        408 => (
            ErrorKind::Network,
            "the server timed out waiting for the request".into(),
        ),
        409 => (
            ErrorKind::InvalidPath,
            "the server reported a conflict with what's already there".into(),
        ),
        412 => (
            ErrorKind::AlreadyExists,
            "the item already exists or changed on the server".into(),
        ),
        413 => (
            ErrorKind::NoSpace,
            "the server says the file is too large".into(),
        ),
        507 => (ErrorKind::NoSpace, "the server is out of space".into()),
        416 => (
            ErrorKind::InvalidPath,
            "the requested range isn't valid for this file".into(),
        ),
        423 => (
            ErrorKind::PermissionDenied,
            "the item is locked on the server".into(),
        ),
        429 => (
            ErrorKind::RateLimited,
            "the server is limiting requests, try again shortly".into(),
        ),
        503 if retry.is_some() => (
            ErrorKind::RateLimited,
            "the server is busy, try again shortly".into(),
        ),
        503 => (
            ErrorKind::Network,
            "the server is temporarily unavailable".into(),
        ),
        500..=599 => (
            ErrorKind::Network,
            format!("the server had a problem (HTTP {status})"),
        ),
        _ => (
            ErrorKind::Other,
            format!("the server replied with HTTP {status}"),
        ),
    };
    let mut err =
        Error::new(kind, format!("Couldn't {what}: {says}.")).with_detail(detail(status, body));
    if let Some(after) = retry {
        err = err.with_retry_after(after);
    }
    err
}

fn mentions_invalid_name(lower: &str) -> bool {
    const HINTS: [&str; 9] = [
        "invalid name",
        "invalidname",
        "invalid key",
        "invalidkey",
        "invalid object name",
        "invalidobjectname",
        "invalid file name",
        "invalidresourcename",
        "keytoolong",
    ];
    HINTS.iter().any(|h| lower.contains(h))
}

fn detail(status: u16, body: &str) -> String {
    let body = scrub(body.trim());
    let mut end = body.len().min(MAX_DETAIL);
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    let body = body[..end].trim_end();
    if body.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {body}")
    }
}

/// Parses `Retry-After` as delta-seconds or an HTTP-date (a past date means "now").
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get("retry-after")?.to_str().ok()?.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let when = httpdate::parse_http_date(value).ok()?;
    Some(
        when.duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

/// Query-parameter names (matched as a case-insensitive suffix) whose values are secrets or
/// signatures: SAS `sig`, `X-Amz-Signature`, `X-Amz-Security-Token`, `access_token`, ...
const SECRET_NAMES: [&str; 9] = [
    "sig",
    "signature",
    "signatureprovided",
    "token",
    "credential",
    "secret",
    "password",
    "apikey",
    "api_key",
];

/// Redacts `name=value` pairs and `<Name>value</Name>` XML elements whose name looks like a
/// signature, token or credential. Used on every detail that might echo a request URL.
pub(crate) fn scrub(text: &str) -> String {
    let text = scrub_query_params(text);
    scrub_xml_elements(&text)
}

fn is_secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    SECRET_NAMES.iter().any(|s| name.ends_with(s))
}

fn scrub_query_params(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(eq) = rest.find('=') {
        let (before, after) = rest.split_at(eq);
        let name_start = before
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .map_or(0, |i| i + 1);
        out.push_str(before);
        out.push('=');
        let after = &after[1..];
        let value_end = after
            .find(|c: char| c == '&' || c == '"' || c == '\'' || c == '<' || c.is_whitespace())
            .unwrap_or(after.len());
        if is_secret_name(&before[name_start..]) && value_end > 0 {
            out.push_str("REDACTED");
        } else {
            out.push_str(&after[..value_end]);
        }
        rest = &after[value_end..];
    }
    out.push_str(rest);
    out
}

fn scrub_xml_elements(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        rest = &rest[open..];
        let Some(close) = rest.find('>') else { break };
        let tag = &rest[1..close];
        out.push_str(&rest[..=close]);
        rest = &rest[close + 1..];
        let is_plain_open = !tag.is_empty()
            && tag
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if is_plain_open && is_secret_name(tag) {
            let end_tag = format!("</{tag}>");
            if let Some(end) = rest.find(&end_tag) {
                out.push_str("REDACTED");
                rest = &rest[end..];
            }
        }
    }
    out.push_str(rest);
    out
}
