//! Secrets: the system's secret store (macOS Keychain, Windows Credential Manager, Secret
//! Service on Linux) through `keyring`, or memory for tests and headless runs.
//!
//! Every secret is a generic password with service [`crate::KEYCHAIN_SERVICE`] and account
//! [`secret_account`]. On macOS these are the same Keychain items Filewinch always wrote;
//! on Windows the credential's target name is `<account>.app.xpdt.filewinch`; on Linux the
//! item's attributes are `service` and `username` in the default collection.

use std::collections::HashMap;
use std::sync::Mutex;

use filewinch_core::{Error, ErrorKind, Result};

/// The store account for one of a site's secrets: `<site id>/<what>` (`password`,
/// `passphrase`, `oauth`).
pub fn secret_account(site_id: &str, what: &str) -> String {
    format!("{site_id}/{what}")
}

/// Where secrets live. Values are never logged.
pub trait SecretStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<String>>;
    fn set(&self, account: &str, secret: &str) -> Result<()>;
    /// Deleting a missing secret succeeds.
    fn delete(&self, account: &str) -> Result<()>;
}

/// The system's secret store, service [`crate::KEYCHAIN_SERVICE`]. Errors name the store the
/// way the person knows it ("your Keychain", "Windows Credential Manager", "your keyring").
/// It never falls back to a file or memory: on Linux with no Secret Service running every
/// call fails with [`crate::NO_SECRET_SERVICE`].
pub struct SystemSecrets;

#[cfg(any(target_os = "macos", windows, target_os = "linux", target_os = "freebsd", target_os = "openbsd"))]
mod system {
    use filewinch_os::Os;

    use super::*;
    use crate::secret_errors::{Op, to_error};

    fn entry(account: &str, op: Op) -> Result<keyring::Entry> {
        keyring::Entry::new(crate::KEYCHAIN_SERVICE, account).map_err(|e| to_error(&e, op, Os::CURRENT))
    }

    /// Deletes every credential of an ambiguous match (Linux can hold duplicates).
    fn delete_all(found: Vec<Box<keyring::Credential>>, op: Op) -> Result<()> {
        for c in found {
            match c.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => {}
                Err(e) => return Err(to_error(&e, op, Os::CURRENT)),
            }
        }
        Ok(())
    }

    impl SecretStore for SystemSecrets {
        fn get(&self, account: &str) -> Result<Option<String>> {
            on_store_thread(|| match entry(account, Op::Read)?.get_password() {
                Ok(secret) => Ok(Some(secret)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => Err(to_error(&e, Op::Read, Os::CURRENT)),
            })
        }

        fn set(&self, account: &str, secret: &str) -> Result<()> {
            on_store_thread(|| {
                let entry = entry(account, Op::Save)?;
                match entry.set_password(secret) {
                    Err(keyring::Error::Ambiguous(found)) => {
                        delete_all(found, Op::Save)?;
                        entry.set_password(secret).map_err(|e| to_error(&e, Op::Save, Os::CURRENT))
                    }
                    r => r.map_err(|e| to_error(&e, Op::Save, Os::CURRENT)),
                }
            })
        }

        fn delete(&self, account: &str) -> Result<()> {
            on_store_thread(|| match entry(account, Op::Delete)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(keyring::Error::Ambiguous(found)) => delete_all(found, Op::Delete),
                Err(e) => Err(to_error(&e, Op::Delete, Os::CURRENT)),
            })
        }
    }

    /// Keychain and Credential Manager calls are plain blocking calls.
    #[cfg(any(target_os = "macos", windows))]
    fn on_store_thread<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
        f()
    }

    /// Secret Service calls block on zbus's own single-threaded tokio runtime, which panics
    /// when entered from a thread already inside a runtime (the SFTP backend's) and can
    /// deadlock on one that drives another. So each call runs on a fresh thread with no
    /// runtime, one at a time (keyring warns that concurrent D-Bus calls can fail).
    #[cfg(not(any(target_os = "macos", windows)))]
    fn on_store_thread<T: Send>(f: impl FnOnce() -> Result<T> + Send) -> Result<T> {
        static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
        let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let store = Os::CURRENT.secret_store();
        std::thread::scope(|s| {
            let worker = std::thread::Builder::new().name("filewinch-secrets".into()).spawn_scoped(s, f).map_err(|e| {
                Error::new(ErrorKind::Other, format!("Filewinch couldn't reach {store}.")).with_detail(e)
            })?;
            worker
                .join()
                .unwrap_or_else(|_| Err(Error::new(ErrorKind::Other, format!("Filewinch couldn't reach {store}."))))
        })
    }
}

/// Other systems have no store Filewinch knows; it never keeps passwords in a file instead.
#[cfg(not(any(target_os = "macos", windows, target_os = "linux", target_os = "freebsd", target_os = "openbsd")))]
impl SecretStore for SystemSecrets {
    fn get(&self, _: &str) -> Result<Option<String>> {
        Err(no_store())
    }
    fn set(&self, _: &str, _: &str) -> Result<()> {
        Err(no_store())
    }
    fn delete(&self, _: &str) -> Result<()> {
        Err(no_store())
    }
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux", target_os = "freebsd", target_os = "openbsd")))]
fn no_store() -> Error {
    Error::new(ErrorKind::Unsupported, "Filewinch can't save passwords on this system.")
}

/// Secrets held in memory: tests, headless screenshots, and "don't save" connections.
#[derive(Default)]
pub struct MemorySecrets(Mutex<HashMap<String, String>>);

impl SecretStore for MemorySecrets {
    fn get(&self, account: &str) -> Result<Option<String>> {
        Ok(self.0.lock().map_err(|_| Error::new(ErrorKind::Other, "Secret store lock poisoned."))?.get(account).cloned())
    }
    fn set(&self, account: &str, secret: &str) -> Result<()> {
        self.0.lock().map_err(|_| Error::new(ErrorKind::Other, "Secret store lock poisoned."))?.insert(account.into(), secret.into());
        Ok(())
    }
    fn delete(&self, account: &str) -> Result<()> {
        self.0.lock().map_err(|_| Error::new(ErrorKind::Other, "Secret store lock poisoned."))?.remove(account);
        Ok(())
    }
}
