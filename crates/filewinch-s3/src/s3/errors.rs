//! S3 error responses (`<Error><Code>…</Code><Message>…</Message></Error>`) to core errors.

use std::time::Duration;

use filewinch_core::{Error, ErrorKind};
use filewinch_http::ureq::http::HeaderMap;

use crate::xml;

/// The parsed `<Error>` document, if the body is one.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct S3Error {
    pub code: String,
    pub message: String,
    /// `<Region>` (AuthorizationHeaderMalformed) or `<Endpoint>`-implied region hints.
    pub region: Option<String>,
}

pub(crate) fn parse_error(body: &str) -> Option<S3Error> {
    let root = xml::parse(body).ok()?;
    if root.name != "Error" {
        return None;
    }
    Some(S3Error {
        code: root.text_of("Code").unwrap_or_default().trim().to_string(),
        message: root.text_of("Message").unwrap_or_default().trim().to_string(),
        region: root.text_of("Region").map(|r| r.trim().to_string()).filter(|r| !r.is_empty()),
    })
}

/// Maps a failed S3 response. `what` is a phrase like "list /photos"; `host` names the
/// endpoint in messages. HEAD responses have no body, so `body` may be empty: the status
/// decides then.
pub(crate) fn map_error(status: u16, headers: &HeaderMap, body: &str, what: &str, host: &str) -> Error {
    let base = filewinch_http::status_error_parts(status, headers, body, what);
    let parsed = parse_error(body);
    let code = parsed.as_ref().map_or("", |p| p.code.as_str());
    let server_says = parsed.as_ref().map(|p| p.message.clone()).filter(|m| !m.is_empty());
    let with = |kind: ErrorKind, message: String| {
        let mut e = Error::new(kind, message);
        e.detail = base.detail.clone();
        e.retry_after = base.retry_after;
        e
    };
    match code {
        "AccessDenied" | "AllAccessDisabled" | "AccountProblem" => with(
            ErrorKind::PermissionDenied,
            format!("Couldn't {what}: the access key doesn't have permission."),
        ),
        "InvalidAccessKeyId" | "SignatureDoesNotMatch" | "InvalidToken" | "ExpiredToken"
        | "TokenRefreshRequired" | "InvalidSecurity" => with(
            ErrorKind::Auth,
            format!("The server rejected the access key or secret for {host}."),
        ),
        "NoSuchBucket" => with(ErrorKind::NotFound, format!("Couldn't {what}: the bucket doesn't exist.")),
        "NoSuchKey" | "NoSuchUpload" | "NotFound" => {
            with(ErrorKind::NotFound, format!("Couldn't {what}: the item doesn't exist."))
        }
        "BucketAlreadyOwnedByYou" => {
            with(ErrorKind::AlreadyExists, format!("Couldn't {what}: you already own a bucket with that name."))
        }
        "BucketAlreadyExists" => with(
            ErrorKind::AlreadyExists,
            format!("Couldn't {what}: that bucket name is taken by someone else. Bucket names are shared by every account."),
        ),
        "BucketNotEmpty" => with(ErrorKind::InvalidPath, format!("Couldn't {what}: the bucket isn't empty.")),
        "InvalidBucketName" => with(ErrorKind::InvalidPath, format!("Couldn't {what}: that isn't a valid bucket name.")),
        "KeyTooLongError" => with(ErrorKind::InvalidPath, format!("Couldn't {what}: the name is too long.")),
        "SlowDown" | "RequestLimitExceeded" | "TooManyRequests" | "Throttling" => {
            let e = with(ErrorKind::RateLimited, format!("Couldn't {what}: {host} is limiting requests, trying again shortly."));
            let after = e.retry_after.unwrap_or(SLOW_DOWN_DEFAULT);
            e.with_retry_after(after)
        }
        "RequestTimeTooSkewed" => with(
            ErrorKind::Other,
            format!("Couldn't {what}: this Mac's clock is too far off for {host} to accept the request. Set the date and time automatically in System Settings."),
        ),
        "EntityTooLarge" => with(
            ErrorKind::NoSpace,
            format!("Couldn't {what}: the file is larger than {host} allows for one object."),
        ),
        "QuotaExceeded" | "StorageQuotaExceeded" => {
            with(ErrorKind::NoSpace, format!("Couldn't {what}: the storage quota is used up."))
        }
        "XAmzContentSHA256Mismatch" | "BadDigest" | "IncompleteBody" | "RequestTimeout" => with(
            ErrorKind::Network,
            format!("Couldn't {what}: the data arrived damaged or incomplete at {host}."),
        ),
        "InternalError" | "ServiceUnavailable" => {
            let kind = if base.retry_after.is_some() || status == 503 { ErrorKind::RateLimited } else { ErrorKind::Network };
            with(kind, format!("Couldn't {what}: {host} had a temporary problem."))
        }
        "PermanentRedirect" | "AuthorizationHeaderMalformed" | "TemporaryRedirect" | "IllegalLocationConstraintException" => with(
            ErrorKind::Other,
            format!("Couldn't {what}: the bucket is in a different region than {host} expected."),
        ),
        "InvalidRange" => with(ErrorKind::InvalidPath, format!("Couldn't {what}: the file is shorter than the requested position.")),
        "MethodNotAllowed" | "NotImplemented" => {
            with(ErrorKind::Unsupported, format!("Couldn't {what}: {host} doesn't support this operation."))
        }
        "EntityTooSmall" | "InvalidPart" | "InvalidPartOrder" => with(
            ErrorKind::Other,
            format!("Couldn't {what}: {host} rejected the uploaded parts."),
        ),
        _ => {
            // No (known) code: the status decides, as filewinch-http maps it.
            let mut e = base;
            match status {
                401 => e.message = format!("The server rejected the access key or secret for {host}."),
                403 if code.is_empty() => {
                    e.message = format!("Couldn't {what}: the access key doesn't have permission.")
                }
                503 => {
                    e.kind = ErrorKind::RateLimited;
                    e.message = format!("Couldn't {what}: {host} is busy, trying again shortly.");
                }
                _ => {}
            }
            if let (Some(says), ErrorKind::Other) = (&server_says, e.kind) {
                e.message = format!("Couldn't {what}: {}", sentence(says));
            }
            e
        }
    }
}

fn sentence(s: &str) -> String {
    let s = s.trim();
    if s.ends_with('.') { s.to_string() } else { format!("{s}.") }
}

/// A default wait when S3 says SlowDown without Retry-After.
pub(crate) const SLOW_DOWN_DEFAULT: Duration = Duration::from_secs(1);

#[cfg(test)]
#[path = "errors_tests.rs"]
mod tests;
