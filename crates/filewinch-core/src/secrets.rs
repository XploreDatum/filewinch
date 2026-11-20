//! Access to the user's secrets (the macOS Keychain in the app).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::{Error, ErrorKind, Result, SecretRef};

/// Reads and (for rotating OAuth refresh tokens) replaces secrets. The app implements it over
/// the Keychain; backends get it as `Arc<dyn Secrets>` and may keep it for the session's life.
pub trait Secrets: Send + Sync {
    fn get(&self, r: &SecretRef) -> Result<Option<String>>;
    fn set(&self, r: &SecretRef, value: &str) -> Result<()>;
}

/// Secrets held in memory: tests, headless runs, and connections the user chose not to save.
#[derive(Default)]
pub struct MemorySecrets(Mutex<HashMap<SecretRef, String>>);

impl MemorySecrets {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a secret (builder style, for tests).
    pub fn with(self, r: &str, value: &str) -> Self {
        self.lock().insert(SecretRef(r.into()), value.into());
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SecretRef, String>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl Secrets for MemorySecrets {
    fn get(&self, r: &SecretRef) -> Result<Option<String>> {
        Ok(self.lock().get(r).cloned())
    }
    fn set(&self, r: &SecretRef, value: &str) -> Result<()> {
        if r.0.is_empty() {
            return Err(Error::new(ErrorKind::Other, "A secret needs a name."));
        }
        self.lock().insert(r.clone(), value.into());
        Ok(())
    }
}

/// Read-only secrets from a closure (tests and one-off connections). `set` fails, so a session
/// that rotates its token keeps the new one in memory only.
pub struct FnSecrets<F>(pub F);

impl<F> Secrets for FnSecrets<F>
where
    F: Fn(&SecretRef) -> Result<Option<String>> + Send + Sync,
{
    fn get(&self, r: &SecretRef) -> Result<Option<String>> {
        (self.0)(r)
    }
    fn set(&self, _r: &SecretRef, _value: &str) -> Result<()> {
        Err(Error::new(ErrorKind::Unsupported, "These secrets are read-only."))
    }
}
