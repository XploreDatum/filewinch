//! PKCE (RFC 7636, S256) and the `state` value.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

fn random_b64(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    // macOS's getentropy never fails for buffers this small; a failure here means the OS
    // can't provide randomness at all, and signing in without it would be unsafe.
    getrandom::getrandom(&mut buf).expect("the system random number generator failed");
    URL_SAFE_NO_PAD.encode(buf)
}

/// 43-character code verifier (32 random bytes, base64url), within RFC 7636's 43–128 range.
pub(crate) fn verifier() -> String {
    random_b64(32)
}

/// `BASE64URL(SHA256(verifier))` without padding.
pub(crate) fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Unguessable `state` for CSRF protection.
pub(crate) fn random_token() -> String {
    random_b64(24)
}
