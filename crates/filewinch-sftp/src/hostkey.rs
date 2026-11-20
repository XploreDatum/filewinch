//! Host key verification against Filewinch's and the user's known_hosts files.

use filewinch_core::{Error, ErrorKind, Result};
use russh::keys::{HashAlg, PublicKey, check_known_hosts_path, known_hosts::learn_known_hosts_path};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

static OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Keys servers presented that weren't trusted yet, by (host, port), so the app can trust
/// exactly the key the user was shown.
fn seen() -> &'static Mutex<HashMap<(String, u16), PublicKey>> {
    static SEEN: OnceLock<Mutex<HashMap<(String, u16), PublicKey>>> = OnceLock::new();
    SEEN.get_or_init(Default::default)
}

/// `known_hosts` in Filewinch's config folder (`~/Library/Application Support/Filewinch` on
/// macOS, `%APPDATA%\Filewinch` on Windows, `~/.config/filewinch` on Linux), or the override.
pub fn filewinch_known_hosts_path() -> PathBuf {
    if let Some(p) = OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return p;
    }
    filewinch_os::dirs::Dirs::current().map(|d| d.config).unwrap_or_default().join("known_hosts")
}

/// Points Filewinch's own known_hosts somewhere else (tests; a sandboxed container path).
pub fn set_known_hosts_path(path: Option<PathBuf>) {
    *OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()) = path;
}

fn user_known_hosts() -> Option<PathBuf> {
    filewinch_os::dirs::Dirs::current().map(|d| d.ssh().join("known_hosts"))
}

pub(crate) fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// Ok when the site trusts `key`'s fingerprint (`Site::trusted_fingerprints`) or a known_hosts
/// file vouches for it; otherwise the HostTrust error to show.
pub(crate) fn verify(host: &str, port: u16, key: &PublicKey, trusted: &[String]) -> std::result::Result<(), Error> {
    if filewinch_core::is_trusted(trusted, &fingerprint(key)) {
        return Ok(());
    }
    let fw = filewinch_known_hosts_path();
    let mut changed = false;
    // Filewinch's file first: a key the user trusted in-app wins over a stale ~/.ssh entry.
    for path in std::iter::once(fw).chain(user_known_hosts()) {
        if !path.exists() {
            continue;
        }
        match check_known_hosts_path(host, port, key, &path) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(russh::keys::Error::KeyChanged { .. }) => changed = true,
            Err(_) => {} // unreadable file: treat as no entry
        }
    }
    let fp = fingerprint(key);
    seen().lock().unwrap_or_else(|e| e.into_inner()).insert((host.to_string(), port), key.clone());
    let who = if port == 22 { host.to_string() } else { format!("{host}:{port}") };
    let message = if changed {
        format!(
            "The identity of {who} has changed since you last connected. This could mean someone is intercepting the connection, or the server was reinstalled. Only trust the new key if you know why it changed."
        )
    } else {
        format!("Filewinch hasn't seen {who} before. Check that its key fingerprint matches what the server's administrator gave you.")
    };
    Err(Error::host_trust(message, &fp, if changed { "key changed" } else { "unknown host" }))
}

/// Records the key a server presented in a failed connect, if its fingerprint is
/// `fingerprint` (the `detail` of the HostTrust error), in Filewinch's known_hosts, replacing
/// any older key Filewinch had for that host. `host` may be the site's address or the name it
/// resolved to through `~/.ssh/config`.
pub fn trust_host_key(host: &str, port: u16, fingerprint_: &str) -> Result<()> {
    // Accept the whole `detail` ("SHA256:…; reason") or any form core can normalise.
    let raw = fingerprint_.split(';').next().unwrap_or_default().trim();
    let fingerprint_ = filewinch_core::normalize_fingerprint(raw).unwrap_or_else(|| raw.to_string());
    let fingerprint_ = fingerprint_.as_str();
    let found = {
        let seen = seen().lock().unwrap_or_else(|e| e.into_inner());
        seen.get(&(host.to_string(), port))
            .map(|k| (host.to_string(), k.clone()))
            .or_else(|| seen.iter().find(|((_, p), k)| *p == port && fingerprint(k) == fingerprint_).map(|((h, _), k)| (h.clone(), k.clone())))
    };
    let Some((host, key)) = found else {
        return Err(Error::new(ErrorKind::NotFound, format!("Connect to {host} again to see its key before trusting it.")));
    };
    let host = host.as_str();
    if fingerprint(&key) != fingerprint_ {
        return Err(Error::new(ErrorKind::HostTrust, format!("The key {host} presents no longer matches the one you were shown. Connect again to review it.")));
    }
    let path = filewinch_known_hosts_path();
    let label = if port == 22 { host.to_string() } else { format!("[{host}]:{port}") };
    if let Ok(text) = std::fs::read_to_string(&path) {
        let kept: Vec<&str> = text
            .lines()
            .filter(|l| !l.split_whitespace().next().is_some_and(|hosts| hosts.split(',').any(|h| h == label)))
            .collect();
        let mut out = kept.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        std::fs::write(&path, out).map_err(|e| Error::from(e).with_detail(path.display()))?;
    }
    learn_known_hosts_path(host, port, &key, &path)
        .map_err(|e| Error::new(ErrorKind::Other, format!("Couldn't save the key for {host}.")).with_detail(e))?;
    seen().lock().unwrap_or_else(|e| e.into_inner()).remove(&(host.to_string(), port));
    Ok(())
}
