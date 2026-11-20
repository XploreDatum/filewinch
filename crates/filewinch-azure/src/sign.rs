//! Shared Key authorization: the string-to-sign and its HMAC-SHA256.
//!
//! Follows Microsoft's "Authorize with Shared Key" for the Blob service (version 2009-09-19
//! and later): twelve standard header lines (Content-Length empty when zero), then the
//! canonicalized `x-ms-*` headers, then the canonicalized resource.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// The standard headers, in string-to-sign order.
const STANDARD_HEADERS: [&str; 11] = [
    "content-encoding",
    "content-language",
    "content-length",
    "content-md5",
    "content-type",
    "date",
    "if-modified-since",
    "if-match",
    "if-none-match",
    "if-unmodified-since",
    "range",
];

/// Builds the string-to-sign.
///
/// - `path` is the URI path exactly as sent (percent-encoded), e.g. `/photos/a%20b.jpg`
///   (for path-style endpoints it includes the account: `/devstoreaccount1/photos`).
/// - `query` holds decoded parameter names and values.
/// - `headers` holds the request headers that matter for signing (names any case).
pub(crate) fn string_to_sign(
    method: &str,
    account: &str,
    path: &str,
    query: &[(String, String)],
    headers: &[(String, String)],
) -> String {
    let find = |name: &str| {
        headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map_or("", |(_, v)| v.as_str())
    };
    let mut out = String::with_capacity(256);
    out.push_str(&method.to_ascii_uppercase());
    out.push('\n');
    for name in STANDARD_HEADERS {
        let value = find(name);
        // From version 2015-02-21 a zero Content-Length is signed as an empty string.
        let value = if name == "content-length" && value == "0" { "" } else { value };
        out.push_str(value);
        out.push('\n');
    }
    out.push_str(&canonical_headers(headers));
    out.push_str(&canonical_resource(account, path, query));
    out
}

/// `name:value\n` for every `x-ms-*` header: names lowercased and sorted, values trimmed
/// with runs of whitespace collapsed to one space.
pub(crate) fn canonical_headers(headers: &[(String, String)]) -> String {
    let mut ms: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v))
        .filter(|(k, _)| k.starts_with("x-ms-"))
        .map(|(k, v)| (k, v.split_whitespace().collect::<Vec<_>>().join(" ")))
        .collect();
    ms.sort();
    ms.into_iter().map(|(k, v)| format!("{k}:{v}\n")).collect()
}

/// `/account/path` then `\nname:v1,v2` for each query parameter: names lowercased and
/// sorted, values decoded and sorted, repeated names joined with commas.
pub(crate) fn canonical_resource(account: &str, path: &str, query: &[(String, String)]) -> String {
    let path = if path.is_empty() { "/" } else { path };
    let mut out = format!("/{account}{path}");
    let mut params: Vec<(String, &str)> =
        query.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.as_str())).collect();
    params.sort();
    let mut i = 0;
    while i < params.len() {
        let name = params[i].0.clone();
        let mut values = Vec::new();
        while i < params.len() && params[i].0 == name {
            values.push(params[i].1);
            i += 1;
        }
        out.push('\n');
        out.push_str(&name);
        out.push(':');
        out.push_str(&values.join(","));
    }
    out
}

/// Base64 HMAC-SHA256 of `string_to_sign` with the decoded account key.
pub(crate) fn signature(key: &[u8], string_to_sign: &str) -> String {
    // HMAC accepts keys of any length, so this never fails.
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(string_to_sign.as_bytes());
    STANDARD.encode(mac.finalize().into_bytes())
}

#[cfg(test)]
#[path = "sign_tests.rs"]
mod tests;
