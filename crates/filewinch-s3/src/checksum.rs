//! Turning what S3 and B2 report into [`Checksum`]s, only when the value is certainly a
//! hash of the object's bytes (sync skips a copy on a match, so a wrong hash is worse than
//! none).

use filewinch_core::{Checksum, HashAlgo};

/// `hex` as a checksum only if it is exactly `len` hex digits.
fn exact(algo: HashAlgo, hex: &str, len: usize) -> Option<Checksum> {
    let hex = hex.trim();
    if hex.len() != len {
        return None;
    }
    Checksum::new(algo, hex)
}

/// An S3 ETag as an MD5: only a plain, single-part ETag (`"<32 hex>"`). Multipart ETags
/// (`<hex>-<parts>`), weak ETags and anything else aren't the MD5 of the bytes.
pub(crate) fn s3_etag_md5(etag: &str) -> Option<Checksum> {
    let t = etag.trim();
    let t = t.strip_prefix('"').and_then(|t| t.strip_suffix('"')).unwrap_or(t);
    exact(HashAlgo::Md5, t, 32)
}

/// HEAD headers that make the ETag something other than the MD5: SSE-KMS (and its dual-layer
/// variant) and SSE-C objects have opaque ETags. SSE-S3 (`AES256`) ETags are still the MD5.
pub(crate) fn s3_etag_is_opaque(sse: Option<&str>, sse_customer_algorithm: Option<&str>) -> bool {
    let kms = sse.is_some_and(|v| {
        let v = v.trim();
        v.eq_ignore_ascii_case("aws:kms") || v.eq_ignore_ascii_case("aws:kms:dsse")
    });
    kms || sse_customer_algorithm.is_some_and(|v| !v.trim().is_empty())
}

/// B2's `contentSha1` as a SHA-1: only a plain 40-hex value that B2 verified itself.
///
/// - `"none"` (large files with no SHA-1 on record) → none.
/// - `"unverified:<hex>"` → none. B2 reports this for large files whose SHA-1 the uploading
///   client claimed in `fileInfo.large_file_sha1`; B2 checked each part, never the whole. A
///   client that hashed one version of a file and uploaded another (a file changed during
///   upload, a buggy tool) leaves a value that matches the local file but not the stored
///   bytes, and sync would then skip a copy it needed. We report only what B2 vouches for.
///   `fileInfo.large_file_sha1` is the same claim and is ignored for the same reason.
pub(crate) fn b2_content_sha1(v: Option<&str>) -> Option<Checksum> {
    exact(HashAlgo::Sha1, v?, 40)
}

#[cfg(test)]
#[path = "checksum_tests.rs"]
mod tests;
