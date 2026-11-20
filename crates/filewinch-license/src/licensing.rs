//! This device's licence: what's stored where, and the activate / check / deactivate rules.
//!
//! The key and activation id are secrets (system secret store, accounts `license/key` and
//! `license/activation`). Whether the app is registered, and to whom, is in `license.json`
//! in Filewinch's config folder (never the key). The status file alone decides "registered":
//! if the secret store is locked or unreadable the app stays registered and simply skips the
//! check.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use filewinch_store::SecretStore;
use serde::{Deserialize, Serialize};

use crate::polar::{Failure, Holder, Polar};

const KEY_ACCOUNT: &str = "license/key";
const ACTIVATION_ACCOUNT: &str = "license/activation";
const FILE: &str = "license.json";

/// A registered device's non-secret status.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Registration {
    /// The buyer's name from Polar, if they gave one.
    pub name: Option<String>,
    pub email: Option<String>,
    /// The key with all but its ends hidden, for Settings.
    pub masked_key: String,
    /// This device's label in the customer portal.
    pub label: String,
    /// Seconds since 1970.
    pub activated_at: u64,
    /// The last time Polar confirmed the key (0: not yet).
    pub last_validated: u64,
    /// The last time a check was tried, answered or not (the daily throttle).
    pub last_checked: u64,
}

impl Registration {
    /// "Registered to …": the name, else the email, else nothing to say.
    pub fn holder(&self) -> Option<&str> {
        self.name.as_deref().or(self.email.as_deref())
    }

    fn take(&mut self, h: Holder, now: u64) {
        // Keep what we had if Polar's answer leaves a field out.
        self.name = h.name.or(self.name.take());
        self.email = h.email.or(self.email.take());
        self.last_validated = now;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Unregistered,
    Registered(Registration),
}

impl Status {
    pub fn is_registered(&self) -> bool {
        matches!(self, Status::Registered(_))
    }
}

/// What a background [`Licensing::check`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    /// Not registered: nothing to check.
    Unregistered,
    /// Checked less than [`crate::CHECK_EVERY`] ago.
    NotDue,
    /// Polar confirmed the key; the status is refreshed.
    Confirmed(Registration),
    /// Polar said the key or this activation is gone: the app is unregistered now.
    Revoked,
    /// No usable answer (offline, 5xx, timeout) or no key to send: still registered.
    Skipped(String),
}

/// This device's licence, on disk and with Polar. `Send + Sync`; every method blocks on the
/// network or the secret store, so call them off the UI thread.
pub struct Licensing {
    polar: Polar,
    secrets: Arc<dyn SecretStore>,
    /// `None`: keep the status in memory only (headless runs, tests).
    dir: Option<PathBuf>,
    status: Mutex<Option<Status>>,
    /// Held through each activate, check and deactivate, so a check that lands after a
    /// deactivation can't register the device again.
    op: Mutex<()>,
}

impl Licensing {
    pub fn new(polar: Polar, secrets: Arc<dyn SecretStore>, dir: Option<PathBuf>) -> Licensing {
        Licensing { polar, secrets, dir, status: Mutex::new(None), op: Mutex::new(()) }
    }

    /// The current status (read from disk once). An unreadable file reads as unregistered:
    /// the person can enter their key again; nothing is lost from Polar.
    pub fn status(&self) -> Status {
        let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        s.get_or_insert_with(|| match &self.dir {
            Some(dir) => read(&dir.join(FILE)).map_or(Status::Unregistered, Status::Registered),
            None => Status::Unregistered,
        })
        .clone()
    }

    /// Activates `key` (already through [`crate::clean_key`]) on this device as `label`,
    /// confirms it is a Filewinch key, and saves it. On any failure after Polar activated it,
    /// the activation is released again so it doesn't use up a device.
    pub fn activate(&self, key: &str, label: &str, now: u64) -> Result<Registration, Failure> {
        let _op = self.op.lock().unwrap_or_else(|e| e.into_inner());
        let activation = self.polar.activate(key, label)?;
        let holder = match self.polar.validate(key, &activation) {
            Ok(h) => h,
            // Another product's key, or Polar changed its mind: give the slot back.
            Err(Failure::Revoked) => {
                let _ = self.polar.deactivate(key, &activation);
                return Err(Failure::WrongKey);
            }
            // Activated but the confirming call didn't get through: registered all the same
            // (activation proved the key); the daily check fills in the name.
            Err(_) => Holder::default(),
        };
        let mut reg = Registration { masked_key: crate::mask_key(key), label: label.to_owned(), activated_at: now, last_checked: now, ..Registration::default() };
        if holder != Holder::default() {
            reg.take(holder, now);
        }
        let saved = self.secrets.set(KEY_ACCOUNT, key)
            .and_then(|()| self.secrets.set(ACTIVATION_ACCOUNT, &activation))
            .map_err(|e| Failure::Storage(format!("{} Your licence wasn't saved; try again.", e.message)));
        let saved = saved.and_then(|()| self.save(Some(&reg)));
        if let Err(f) = saved {
            let _ = self.polar.deactivate(key, &activation);
            let _ = self.secrets.delete(KEY_ACCOUNT);
            let _ = self.secrets.delete(ACTIVATION_ACCOUNT);
            return Err(f);
        }
        Ok(reg)
    }

    /// Validates with Polar if registered and the last check was [`crate::CHECK_EVERY`] ago
    /// or more (or the clock went backwards). Only Polar's "not found" un-registers.
    pub fn check(&self, now: u64) -> Check {
        let _op = self.op.lock().unwrap_or_else(|e| e.into_inner());
        let Status::Registered(mut reg) = self.status() else { return Check::Unregistered };
        let every = crate::CHECK_EVERY.as_secs();
        if now >= reg.last_checked && now - reg.last_checked < every {
            return Check::NotDue;
        }
        // Count the attempt first, so an app that crashes or quits mid-check still waits a day.
        reg.last_checked = now;
        let _ = self.save(Some(&reg));
        let (key, activation) = match self.secrets_pair() {
            Ok(Some(pair)) => pair,
            Ok(None) => return Check::Skipped("The licence key isn't in the secret store.".into()),
            Err(e) => return Check::Skipped(e),
        };
        match self.polar.validate(&key, &activation) {
            Ok(h) => {
                reg.take(h, now);
                let _ = self.save(Some(&reg));
                Check::Confirmed(reg)
            }
            Err(Failure::Revoked) => {
                self.forget();
                Check::Revoked
            }
            Err(f) => Check::Skipped(f.detail().unwrap_or("").to_owned()),
        }
    }

    /// Frees this device's slot with Polar, then forgets the licence here. If Polar can't be
    /// reached this fails and nothing changes (try again later). A slot Polar no longer has
    /// counts as freed.
    pub fn deactivate(&self) -> Result<(), Failure> {
        let _op = self.op.lock().unwrap_or_else(|e| e.into_inner());
        match self.secrets_pair() {
            Ok(Some((key, activation))) => match self.polar.deactivate(&key, &activation) {
                Ok(()) | Err(Failure::Revoked) => {}
                Err(f) => return Err(f),
            },
            // Nothing to free with Polar (the key was removed from the secret store by hand).
            Ok(None) => {}
            Err(e) => return Err(Failure::Storage(e)),
        }
        self.forget();
        Ok(())
    }

    fn secrets_pair(&self) -> Result<Option<(String, String)>, String> {
        let key = self.secrets.get(KEY_ACCOUNT).map_err(|e| e.message)?;
        let activation = self.secrets.get(ACTIVATION_ACCOUNT).map_err(|e| e.message)?;
        Ok(key.zip(activation))
    }

    fn forget(&self) {
        let _ = self.secrets.delete(KEY_ACCOUNT);
        let _ = self.secrets.delete(ACTIVATION_ACCOUNT);
        let _ = self.save(None);
    }

    /// Writes (or, with `None`, removes) the status file, and the cached status.
    fn save(&self, reg: Option<&Registration>) -> Result<(), Failure> {
        if let Some(dir) = &self.dir {
            let path = dir.join(FILE);
            let written = match reg {
                Some(r) => write(dir, r),
                None => match fs::remove_file(&path) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                    _ => Ok(()),
                },
            };
            written.map_err(|e| Failure::Storage(format!("Filewinch couldn't save its licence status in {} ({e}).", dir.display())))?;
        }
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = Some(reg.cloned().map_or(Status::Unregistered, Status::Registered));
        Ok(())
    }
}

fn read(path: &Path) -> Option<Registration> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// Atomic write, owner-only on Unix, like the other config files.
fn write(dir: &Path, reg: &Registration) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let json = serde_json::to_vec_pretty(reg).map_err(std::io::Error::other)?;
    let tmp = dir.join(format!(".{FILE}.tmp-{}", std::process::id()));
    let result = (|| {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut f = opts.open(&tmp)?;
        f.write_all(&json)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        fs::rename(&tmp, dir.join(FILE))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}
