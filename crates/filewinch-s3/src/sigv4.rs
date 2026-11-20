//! AWS Signature Version 4 header signing, as S3 uses it.
//!
//! S3 differs from other AWS services in one way that matters here: the canonical URI is the
//! request path encoded once (each segment percent-encoded, `/` kept), not encoded twice.

use hmac::{Hmac, KeyInit, Mac};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use sha2::{Digest, Sha256};

/// Everything except the SigV4 unreserved characters `A-Z a-z 0-9 - _ . ~`.
pub(crate) const STRICT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');

/// The payload hash for streamed bodies whose hash isn't known up front.
pub(crate) const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

/// Access key pair. `Debug` never shows the secret.
#[derive(Clone)]
pub(crate) struct Credentials {
    pub access_key: String,
    pub secret_key: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials").field("access_key", &self.access_key).finish_non_exhaustive()
    }
}

/// Percent-encodes with the SigV4 rules (everything but unreserved characters).
pub(crate) fn encode(s: &str) -> String {
    utf8_percent_encode(s, STRICT).to_string()
}

/// Encodes an S3 key or path for the URL and canonical URI: each `/`-separated segment once.
pub(crate) fn encode_path(path: &str) -> String {
    path.split('/').map(encode).collect::<Vec<_>>().join("/")
}

/// The canonical (and URL) query string: names and values encoded, sorted by name then value.
pub(crate) fn canonical_query(query: &[(String, String)]) -> String {
    let mut pairs: Vec<(String, String)> = query.iter().map(|(k, v)| (encode(k), encode(v))).collect();
    pairs.sort();
    pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    // HMAC accepts keys of any length, so `new_from_slice` can't fail.
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// One request to sign. `headers` must include `host` and `x-amz-date` (and, for S3,
/// `x-amz-content-sha256`); names are matched case-insensitively. Every header given is signed.
pub(crate) struct Request<'a> {
    pub method: &'a str,
    /// Already-encoded path (see [`encode_path`]), starting with `/`.
    pub canonical_uri: &'a str,
    pub query: &'a [(String, String)],
    pub headers: &'a [(String, String)],
    pub payload_hash: &'a str,
}

/// The `Authorization` header value for `req`. `amz_date` is `YYYYMMDDTHHMMSSZ`.
pub(crate) fn authorization(
    creds: &Credentials,
    region: &str,
    service: &str,
    amz_date: &str,
    req: &Request<'_>,
) -> String {
    let (signed_headers, signature) = signature(creds, region, service, amz_date, req);
    let scope = scope(amz_date, region, service);
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key
    )
}

fn scope(amz_date: &str, region: &str, service: &str) -> String {
    format!("{}/{region}/{service}/aws4_request", &amz_date[..8.min(amz_date.len())])
}

/// (signed header names, hex signature).
pub(crate) fn signature(
    creds: &Credentials,
    region: &str,
    service: &str,
    amz_date: &str,
    req: &Request<'_>,
) -> (String, String) {
    let canonical = canonical_request(req);
    let (signed_headers, canonical) = canonical;
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{}\n{}",
        scope(amz_date, region, service),
        sha256_hex(canonical.as_bytes())
    );
    let date = &amz_date[..8.min(amz_date.len())];
    let k_date = hmac(format!("AWS4{}", creds.secret_key).as_bytes(), date);
    let k_region = hmac(&k_date, region);
    let k_service = hmac(&k_region, service);
    let k_signing = hmac(&k_service, "aws4_request");
    (signed_headers, hex::encode(hmac(&k_signing, &string_to_sign)))
}

/// (signed header names, canonical request).
pub(crate) fn canonical_request(req: &Request<'_>) -> (String, String) {
    let mut headers: Vec<(String, String)> = req
        .headers
        .iter()
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.split_whitespace().collect::<Vec<_>>().join(" ")))
        .collect();
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    // Repeated names are joined with commas, in the order given.
    let mut merged: Vec<(String, String)> = Vec::new();
    for (k, v) in headers {
        match merged.last_mut() {
            Some(last) if last.0 == k => {
                last.1.push(',');
                last.1.push_str(&v);
            }
            _ => merged.push((k, v)),
        }
    }
    let canonical_headers: String = merged.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed: String = merged.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let canonical = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed}\n{}",
        req.method,
        req.canonical_uri,
        canonical_query(req.query),
        req.payload_hash
    );
    (signed, canonical)
}

#[cfg(test)]
#[path = "sigv4_tests.rs"]
mod tests;
