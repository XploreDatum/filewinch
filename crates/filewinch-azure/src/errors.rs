//! Mapping Azure error responses (`x-ms-error-code`, `<Error><Code>`) to core errors.

use std::time::Duration;

use filewinch_core::{Error, ErrorKind};
use filewinch_http::status_error_parts;
use filewinch_http::ureq::http::HeaderMap;

/// What the error is about, for the message.
pub(crate) struct Ctx<'a> {
    /// A phrase like `list “photos”` or `upload “a.txt”`.
    pub what: &'a str,
    pub account: &'a str,
    pub sas: bool,
}

/// Builds the error for a failed response. `body` may be empty (HEAD).
pub(crate) fn azure_error(status: u16, headers: &HeaderMap, body: &str, ctx: &Ctx) -> Error {
    let header_code = headers.get("x-ms-error-code").and_then(|v| v.to_str().ok()).map(str::to_string);
    let (body_code, body_message) = crate::xml::error_body(body);
    let code = header_code.or(body_code).unwrap_or_default();
    // The message plus AuthenticationErrorDetail ("Request date header too old", ...).
    let lower = format!("{} {body}", body_message.unwrap_or_default()).to_ascii_lowercase();

    // The generic mapping supplies the scrubbed detail and Retry-After.
    let mut err = status_error_parts(status, headers, &redact_signature(body), ctx.what);
    if !code.is_empty() {
        let detail = err.detail.take().unwrap_or_default();
        err.detail = Some(format!("{code}; {detail}"));
    }
    let what = ctx.what;
    let mut set = |kind: ErrorKind, message: String| {
        err.kind = kind;
        err.message = message;
    };
    // Azurite (and some proxies) report a bad signature as AuthorizationFailure.
    let code = if code == "AuthorizationFailure" && lower.contains("failed to authenticate") {
        "AuthenticationFailed".to_string()
    } else {
        code
    };
    match code.as_str() {
        "AuthenticationFailed" | "InvalidAuthenticationInfo" => {
            let clock = lower.contains("too old")
                || lower.contains("time frame")
                || lower.contains("skew")
                || lower.contains("expir");
            let base = if ctx.sas {
                format!("The SAS token for {} was rejected", ctx.account)
            } else {
                format!("The account key for {} was rejected", ctx.account)
            };
            let tail = match (clock, ctx.sas) {
                (true, true) => ": it has expired or isn't valid yet. Check the token's dates and that your Mac's clock is set correctly.",
                (true, false) => ": the request time was off. Check that your Mac's clock is set correctly (System Settings › General › Date & Time).",
                (false, true) => ". Check that the whole token was copied and hasn't been revoked.",
                (false, false) => ". Check that you copied key1 or key2 of this storage account.",
            };
            set(ErrorKind::Auth, format!("{base}{tail}"));
        }
        "NoAuthenticationInformation" => set(
            ErrorKind::Auth,
            format!("{} needs an account key or a SAS token for this.", ctx.account),
        ),
        "AuthorizationPermissionMismatch"
        | "AuthorizationFailure"
        | "AuthorizationResourceTypeMismatch"
        | "AuthorizationServiceMismatch"
        | "AuthorizationSourceIPMismatch"
        | "AuthorizationProtocolMismatch"
        | "InsufficientAccountPermissions"
        | "PublicAccessNotPermitted"
        | "AccountIsDisabled"
        | "ContainerDisabled"
        | "KeyBasedAuthenticationNotPermitted" => {
            let hint = if ctx.sas { " The SAS token doesn't allow it." } else { "" };
            set(ErrorKind::PermissionDenied, format!("You don't have permission to {what}.{hint}"));
        }
        "ContainerNotFound" => set(ErrorKind::NotFound, format!("Couldn't {what}: the container doesn't exist.")),
        "BlobNotFound" | "ResourceNotFound" | "CannotVerifyCopySource" if status == 404 => {
            set(ErrorKind::NotFound, format!("Couldn't {what}: it wasn't found."))
        }
        "ContainerAlreadyExists" => {
            set(ErrorKind::AlreadyExists, format!("Couldn't {what}: a container with that name already exists."))
        }
        "BlobAlreadyExists" => set(ErrorKind::AlreadyExists, format!("Couldn't {what}: it already exists.")),
        "ContainerBeingDeleted" => {
            set(
                ErrorKind::RateLimited,
                format!("Couldn't {what}: a container with that name is still being deleted. Try again in a minute."),
            );
            err.retry_after.get_or_insert(Duration::from_secs(30));
        }
        "ServerBusy" | "OperationTimedOut" | "InternalError" if status == 503 || status == 500 => {
            if code == "ServerBusy" || err.retry_after.is_some() {
                set(ErrorKind::RateLimited, format!("Couldn't {what}: Azure is busy. Filewinch will try again shortly."));
            } else {
                set(ErrorKind::Network, format!("Couldn't {what}: Azure had a temporary problem."));
            }
        }
        "InvalidResourceName" | "InvalidUri" | "InvalidBlobOrBlock" | "OutOfRangeInput" | "InvalidBlobType" => {
            set(ErrorKind::InvalidPath, format!("Couldn't {what}: Azure doesn't allow that name or path."))
        }
        "InvalidRange" => set(ErrorKind::InvalidPath, format!("Couldn't {what}: the range is past the end of the file.")),
        "RequestBodyTooLarge" | "BlockCountExceedsLimit" | "BlockListTooLong" => {
            set(ErrorKind::NoSpace, format!("Couldn't {what}: the file is too large for Azure."))
        }
        "LeaseIdMissing" | "LeaseAlreadyPresent" | "LeaseIdMismatchWithBlobOperation"
        | "LeaseIdMismatchWithContainerOperation" => set(
            ErrorKind::PermissionDenied,
            format!("Couldn't {what}: it is leased (locked) by another program."),
        ),
        "ContainerQuotaExceeded" | "AccountLimitExceeded" => {
            set(ErrorKind::NoSpace, format!("Couldn't {what}: the storage account is full."))
        }
        _ => {
            if status == 503 || status == 429 {
                set(ErrorKind::RateLimited, format!("Couldn't {what}: Azure is busy. Filewinch will try again shortly."));
            } else if status == 401 {
                // HEAD responses carry no code when the header is stripped by a proxy.
                let who = if ctx.sas { "SAS token" } else { "account key" };
                set(ErrorKind::Auth, format!("The {who} for {} was rejected.", ctx.account));
            }
        }
    }
    err
}

/// Azure echoes the request's computed signature in AuthenticationFailed details ("The MAC
/// signature found in the HTTP request '...'"): cut it out. SAS `sig=` values are scrubbed
/// by `filewinch_http` already.
pub(crate) fn redact_signature(body: &str) -> String {
    const MARK: &str = "HTTP request '";
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(i) = rest.find(MARK) {
        out.push_str(&rest[..i + MARK.len()]);
        rest = &rest[i + MARK.len()..];
        match rest.find('\'') {
            Some(end) => {
                out.push_str("REDACTED");
                rest = &rest[end..];
            }
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}
