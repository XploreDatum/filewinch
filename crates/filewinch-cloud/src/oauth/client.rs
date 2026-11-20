//! Per-provider OAuth client IDs. Nothing secret lives in the source: IDs come from
//! `oauth-clients.json` in Filewinch's config folder (`~/Library/Application Support/Filewinch`
//! on macOS, `%APPDATA%\Filewinch` on Windows, `~/.config/filewinch` on Linux), falling back to
//! `FILEWINCH_<PROVIDER>_CLIENT_ID` / `FILEWINCH_<PROVIDER>_CLIENT_SECRET`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use filewinch_core::{Error, ErrorKind, Result};
use serde::Deserialize;

use super::Provider;

/// A registered OAuth app. Desktop apps can't keep a secret truly secret; Google and Box
/// still require one in the token request, so it is loaded from the same file.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct ClientConfig {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
}

impl std::fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientConfig")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl ClientConfig {
    pub fn new(client_id: impl Into<String>, client_secret: Option<String>) -> Self {
        ClientConfig {
            client_id: client_id.into(),
            client_secret,
        }
    }

    /// `oauth-clients.json` in Filewinch's config folder (see `filewinch_os::dirs`).
    pub fn default_path() -> Option<PathBuf> {
        filewinch_os::dirs::Dirs::current().map(|d| d.config.join("oauth-clients.json"))
    }

    /// Loads `provider`'s client from the default file, then the environment.
    pub fn load(provider: Provider) -> Result<ClientConfig> {
        let path = Self::default_path();
        Self::load_from(provider, path.as_deref(), &|k| std::env::var(k).ok())
    }

    /// [`ClientConfig::load`] with an explicit file and environment (tests).
    pub fn load_from(
        provider: Provider,
        file: Option<&Path>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<ClientConfig> {
        let prefix = format!("FILEWINCH_{}", provider.key().to_ascii_uppercase());
        let env_get =
            |suffix: &str| env(&format!("{prefix}_{suffix}")).filter(|v| !v.trim().is_empty());

        let mut from_file = None;
        if let Some(path) = file.filter(|p| p.exists()) {
            let text = std::fs::read_to_string(path).map_err(|e| {
                Error::new(
                    ErrorKind::Auth,
                    format!("Filewinch couldn't read {}.", path.display()),
                )
                .with_detail(e)
            })?;
            let mut all: HashMap<String, ClientConfig> =
                serde_json::from_str(&text).map_err(|e| {
                    Error::new(
                        ErrorKind::Auth,
                        format!(
                            "{} isn't valid: fix it as shown in docs/oauth-clients.example.json.",
                            path.display()
                        ),
                    )
                    .with_detail(e)
                })?;
            from_file = all
                .remove(provider.key())
                .filter(|c| !c.client_id.trim().is_empty());
        }

        let client = match from_file {
            Some(mut c) => {
                if c.client_secret
                    .as_deref()
                    .is_none_or(|s| s.trim().is_empty())
                {
                    c.client_secret = env_get("CLIENT_SECRET");
                }
                c
            }
            None => ClientConfig {
                client_id: env_get("CLIENT_ID").unwrap_or_default(),
                client_secret: env_get("CLIENT_SECRET"),
            },
        };
        client.check(provider)?;
        Ok(client)
    }

    /// Fails with a clear "isn't set up yet" error if the ID (or a required secret) is missing.
    pub(crate) fn check(&self, provider: Provider) -> Result<()> {
        let missing = if self.client_id.trim().is_empty() {
            "a client ID"
        } else if provider.needs_secret()
            && self
                .client_secret
                .as_deref()
                .is_none_or(|s| s.trim().is_empty())
        {
            "a client secret"
        } else {
            return Ok(());
        };
        Err(Error::new(
            ErrorKind::Auth,
            format!(
                "{} isn't set up yet: add {missing} (see docs/oauth-setup.md)",
                provider.label()
            ),
        ))
    }
}
