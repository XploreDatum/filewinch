//! Settings shared by the four connectors: where each provider lives and upload tuning.

use std::sync::Arc;
use std::time::Duration;

use filewinch_core::{Auth, Error, ErrorKind, Result, Secrets, Site};

use crate::http::Session;
use crate::oauth::token::TokenSource;
use crate::oauth::{ClientConfig, Provider};

/// The provider's URLs. Real defaults come from [`Endpoints::for_provider`]; tests point every
/// URL at a local mock server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// Browser sign-in page.
    pub authorize: String,
    /// Token endpoint (code exchange and refresh).
    pub token: String,
    /// Metadata API base, no trailing slash.
    pub api: String,
    /// Upload/content API base, no trailing slash (same as `api` where the provider has one host).
    pub upload: String,
}

impl Endpoints {
    pub fn for_provider(p: Provider) -> Self {
        let (authorize, token, api, upload) = match p {
            Provider::GoogleDrive => (
                "https://accounts.google.com/o/oauth2/v2/auth",
                "https://oauth2.googleapis.com/token",
                "https://www.googleapis.com/drive/v3",
                "https://www.googleapis.com/upload/drive/v3",
            ),
            Provider::Dropbox => (
                "https://www.dropbox.com/oauth2/authorize",
                "https://api.dropboxapi.com/oauth2/token",
                "https://api.dropboxapi.com/2",
                "https://content.dropboxapi.com/2",
            ),
            Provider::OneDrive => (
                "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
                "https://login.microsoftonline.com/common/oauth2/v2.0/token",
                "https://graph.microsoft.com/v1.0",
                "https://graph.microsoft.com/v1.0",
            ),
            Provider::Box => (
                "https://account.box.com/api/oauth2/authorize",
                "https://api.box.com/oauth2/token",
                "https://api.box.com/2.0",
                "https://upload.box.com/api/2.0",
            ),
        };
        Endpoints {
            authorize: authorize.into(),
            token: token.into(),
            api: api.into(),
            upload: upload.into(),
        }
    }

    /// Every URL under one local base (`http://127.0.0.1:port`): `/oauth/authorize`,
    /// `/oauth/token`, `/api`, `/upload`. Used by the mock-server tests.
    pub fn local(base: &str) -> Self {
        let base = base.trim_end_matches('/');
        Endpoints {
            authorize: format!("{base}/oauth/authorize"),
            token: format!("{base}/oauth/token"),
            api: format!("{base}/api"),
            upload: format!("{base}/upload"),
        }
    }
}

/// Upload and cache sizing. Defaults suit real providers; tests shrink them to exercise the
/// session paths with small files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tuning {
    /// Files up to this many bytes go up in one request (buffered in memory); larger files use
    /// the provider's resumable/session upload.
    pub simple_max: u64,
    /// Session chunk size in bytes (Box ignores it: the server dictates the part size).
    pub chunk: usize,
    /// Most path → item entries kept in memory.
    pub cache_capacity: usize,
    /// How long a cached path → item lookup is trusted before asking the server again.
    pub cache_ttl: Duration,
    /// Items requested per listing page.
    pub page_size: u32,
}

impl Tuning {
    pub fn for_provider(p: Provider) -> Self {
        const MIB: usize = 1024 * 1024;
        let (simple_max, chunk) = match p {
            // Drive: multipart upload is for files up to 5 MB; chunks are multiples of 256 KiB.
            Provider::GoogleDrive => (5 * MIB as u64, 8 * MIB),
            // Dropbox: single upload allows 150 MB, but buffering that much is wasteful; any
            // file over 8 MiB streams through an upload session.
            Provider::Dropbox => (8 * MIB as u64, 8 * MIB),
            // Graph: simple PUT up to 4 MB; session fragments are multiples of 320 KiB.
            Provider::OneDrive => (4 * MIB as u64, 32 * 320 * 1024),
            // Box: chunked sessions need at least 20,000,000 bytes; the part size comes from Box.
            Provider::Box => (20_000_000, 8 * MIB),
        };
        Tuning {
            simple_max,
            chunk,
            cache_capacity: 20_000,
            cache_ttl: Duration::from_secs(60),
            page_size: match p {
                Provider::GoogleDrive => 1000,
                Provider::Dropbox => 2000,
                Provider::OneDrive => 999,
                Provider::Box => 1000,
            },
        }
    }
}

/// What a connector needs beyond the [`Site`]. `Default` loads client IDs from
/// `oauth-clients.json` / the environment and uses the real provider URLs.
#[derive(Clone, Default)]
pub struct ConnectorOptions {
    /// Client ID (and secret). `None` loads it with [`ClientConfig::load`] at connect time.
    pub client: Option<ClientConfig>,
    /// `None` uses [`Endpoints::for_provider`].
    pub endpoints: Option<Endpoints>,
    /// `None` uses [`Tuning::for_provider`].
    pub tuning: Option<Tuning>,
}

impl std::fmt::Debug for ConnectorOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectorOptions")
            .field("client", &self.client)
            .field("endpoints", &self.endpoints)
            .field("tuning", &self.tuning)
            .finish()
    }
}

impl ConnectorOptions {
    pub(crate) fn endpoints(&self, p: Provider) -> Endpoints {
        self.endpoints
            .clone()
            .unwrap_or_else(|| Endpoints::for_provider(p))
    }

    pub(crate) fn tuning(&self, p: Provider) -> Tuning {
        self.tuning.unwrap_or_else(|| Tuning::for_provider(p))
    }

    /// Reads the site's refresh token, loads the client ID, and proves the token works by
    /// fetching a first access token, so a revoked sign-in fails at connect time. The session
    /// keeps `secrets` to save the new refresh token when the provider rotates it (Box on every
    /// refresh, Microsoft sometimes), so the next launch doesn't sign in with a retired one.
    pub(crate) fn open_session(
        &self,
        p: Provider,
        site: &Site,
        secrets: Arc<dyn Secrets>,
    ) -> Result<Arc<Session>> {
        let service = p.label();
        let Auth::OAuth { refresh_token } = &site.auth else {
            return Err(Error::new(
                ErrorKind::Auth,
                format!(
                    "“{}” isn't signed in to {service}. Sign in again.",
                    site.name
                ),
            ));
        };
        let token = secrets.get(refresh_token)?
            .filter(|t| !t.trim().is_empty())
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::Auth,
                    format!(
                        "Filewinch doesn't have a saved {service} sign-in for “{}”. Sign in again.",
                        site.name
                    ),
                )
            })?;
        let client = match &self.client {
            Some(c) => c.clone(),
            None => ClientConfig::load(p)?,
        };
        let endpoints = self.endpoints(p);
        let agent = crate::http::agent();
        let tokens = TokenSource::new(
            agent.clone(),
            p,
            client,
            endpoints.token.clone(),
            refresh_token.clone(),
            token,
            secrets,
        );
        tokens.access()?;
        Ok(Arc::new(Session::new(agent, p, tokens)))
    }
}
