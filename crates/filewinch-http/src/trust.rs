//! Process-wide pinned TLS certificates, keyed by `host:port` — the fallback trust path.
//!
//! When the platform verifier rejects a server certificate (self-signed, private CA not in
//! the Keychain, expired, wrong name), the connection is still accepted if the SHA-256
//! fingerprint of the server's end-entity certificate is in the site's own
//! `Site::trusted_fingerprints` (see [`crate::Http::trust_fingerprints`], the main path), or has
//! been pinned here for that `host:port` (for callers without a saved site).
//!
//! Fingerprints use core's canonical form (`SHA256:<base64>`); every function here also accepts
//! the hex forms (`sha256:AB:CD:…`, `AB:CD:…`, `abcd…`).

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

static PINNED: LazyLock<Mutex<HashMap<String, HashSet<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn key(host: &str, port: u16) -> String {
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    format!("{host}:{port}")
}

/// The canonical form of any accepted fingerprint form; `None` if it isn't SHA-256.
pub(crate) fn normalize(sha256: &str) -> Option<String> {
    let s = sha256.trim();
    filewinch_core::normalize_fingerprint(s)
        .or_else(|| filewinch_core::normalize_fingerprint(&format!("sha256:{s}")))
}

fn pinned() -> std::sync::MutexGuard<'static, HashMap<String, HashSet<String>>> {
    PINNED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Trusts the certificate with this SHA-256 fingerprint for `host:port`, in addition to any
/// already trusted there (load-balanced hosts can present several certificates).
/// Accepts "sha256:AB:CD:..", "AB:CD:.." and "abcd.." forms; anything else is ignored.
pub fn trust_fingerprint(host: &str, port: u16, sha256: &str) {
    if let Some(fp) = normalize(sha256) {
        pinned().entry(key(host, port)).or_default().insert(fp);
    }
}

/// Forgets every pinned certificate for `host:port`.
pub fn untrust(host: &str, port: u16) {
    pinned().remove(&key(host, port));
}

/// Whether this fingerprint (any accepted form) is pinned for `host:port`.
pub fn is_trusted(host: &str, port: u16, sha256: &str) -> bool {
    let Some(fp) = normalize(sha256) else {
        return false;
    };
    pinned()
        .get(&key(host, port))
        .is_some_and(|set| set.contains(&fp))
}

/// SHA-256 of a DER certificate in core's canonical form (`SHA256:<base64>`).
pub fn fingerprint(der: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, der);
    let digest: [u8; 32] = digest.as_ref().try_into().expect("SHA-256 is 32 bytes");
    filewinch_core::fingerprint_from_sha256(&digest)
}
