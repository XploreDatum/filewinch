//! Host-key and certificate fingerprints, in one canonical form for every protocol.
//!
//! Canonical form: `SHA256:<base64, no padding>` — the OpenSSH style (`ssh-keygen -l`), used for
//! SSH host keys and TLS certificates alike (the SHA-256 of the key blob / DER certificate).
//! [`normalize_fingerprint`] also accepts the hex form browsers show (`sha256:AB:CD:…`, colons
//! optional, any case) and a lower-case `sha256:` prefix, so fingerprints saved by older builds
//! keep matching.
//!
//! A `HostTrust` error's `detail` starts with the canonical fingerprint, optionally followed by
//! `"; <reason>"` (see [`Error::host_trust`] and [`Error::fingerprint`]).

use crate::{Error, ErrorKind};

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// The canonical fingerprint for a SHA-256 digest.
pub fn fingerprint_from_sha256(digest: &[u8; 32]) -> String {
    format!("SHA256:{}", base64_nopad(digest))
}

/// The canonical form of `fp`, or `None` if it isn't a SHA-256 fingerprint in a form we know.
pub fn normalize_fingerprint(fp: &str) -> Option<String> {
    let fp = fp.trim();
    let (prefix, body) = fp.split_once(':')?;
    if !prefix.eq_ignore_ascii_case("sha256") {
        return None;
    }
    let hex: String = body.chars().filter(|c| *c != ':').collect();
    if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        let mut d = [0u8; 32];
        for (i, b) in d.iter_mut().enumerate() {
            *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
        }
        return Some(fingerprint_from_sha256(&d));
    }
    let raw = base64_decode(body.trim_end_matches('='))?;
    let d: [u8; 32] = raw.try_into().ok()?;
    Some(fingerprint_from_sha256(&d))
}

/// True when `fp` (any accepted form) equals one of `trusted` (any accepted forms).
pub fn is_trusted(trusted: &[String], fp: &str) -> bool {
    let Some(fp) = normalize_fingerprint(fp) else { return false };
    trusted.iter().any(|t| normalize_fingerprint(t).as_deref() == Some(fp.as_str()))
}

impl Error {
    /// A `HostTrust` error whose `detail` is `"<canonical fingerprint>; <reason>"` (or just the
    /// fingerprint when `reason` is empty).
    pub fn host_trust(message: impl Into<String>, fingerprint: &str, reason: &str) -> Error {
        let fp = normalize_fingerprint(fingerprint).unwrap_or_else(|| fingerprint.to_string());
        let detail = if reason.is_empty() { fp } else { format!("{fp}; {reason}") };
        Error::new(ErrorKind::HostTrust, message).with_detail(detail)
    }

    /// The canonical fingerprint at the start of a `HostTrust` error's `detail`.
    pub fn fingerprint(&self) -> Option<String> {
        if self.kind != ErrorKind::HostTrust {
            return None;
        }
        let d = self.detail.as_deref()?;
        normalize_fingerprint(d.split(';').next()?)
    }
}

fn base64_nopad(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = B64.iter().position(|b| *b == c)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "sha256:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:aa:bb:cc:dd:ee:ff";

    #[test]
    fn hex_and_base64_forms_normalise_to_one() {
        let canon = normalize_fingerprint(HEX).unwrap();
        assert!(canon.starts_with("SHA256:") && !canon.ends_with('='));
        assert_eq!(normalize_fingerprint(&canon).as_deref(), Some(canon.as_str()));
        assert_eq!(normalize_fingerprint(&canon.replace("SHA256", "sha256")).as_deref(), Some(canon.as_str()));
        assert_eq!(normalize_fingerprint(&format!("{canon}=")).as_deref(), Some(canon.as_str()));
        assert_eq!(normalize_fingerprint(&HEX.replace(':', "").replacen("sha256", "sha256:", 1)).as_deref(), Some(canon.as_str()));
        assert!(is_trusted(&[HEX.to_string()], &canon));
        assert!(!is_trusted(&[HEX.to_string()], "SHA256:AAAA"));
    }

    #[test]
    fn known_openssh_fingerprint_round_trips() {
        // ssh-keygen prints digests as 43 base64 characters.
        let fp = "SHA256:nThbg6kXUpJWGl7E1IGOCspRomTxdCARLviKw6E5SY8";
        assert_eq!(normalize_fingerprint(fp).as_deref(), Some(fp));
        assert_eq!(normalize_fingerprint("md5:aa:bb"), None);
        assert_eq!(normalize_fingerprint("SHA256:not base64!"), None);
    }

    #[test]
    fn host_trust_detail_starts_with_canonical_fingerprint() {
        let e = Error::host_trust("Unknown certificate", HEX, "self-signed");
        let canon = normalize_fingerprint(HEX).unwrap();
        assert_eq!(e.detail.as_deref(), Some(format!("{canon}; self-signed").as_str()));
        assert_eq!(e.fingerprint(), Some(canon));
        assert_eq!(Error::new(ErrorKind::Network, "x").with_detail(HEX).fingerprint(), None);
    }
}
