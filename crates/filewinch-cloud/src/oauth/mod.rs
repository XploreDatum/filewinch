//! Browser sign-in: OAuth 2 Authorization Code with PKCE (S256) on a loopback redirect.
//!
//! [`sign_in`] binds `127.0.0.1` (Dropbox and Box: a fixed, pre-registered port range; Google
//! and Microsoft: any free port), opens the system browser on the provider's consent page,
//! waits for the redirect, checks `state`, answers the tab with a short "you can close this
//! tab" page, swaps the code for tokens and looks up an account label for the sidebar. The app
//! stores [`SignedIn::refresh_token`] in the system secret store and saves
//! `Auth::OAuth { refresh_token: SecretRef }` on the site.

mod client;
mod loopback;
mod pkce;
pub(crate) mod token;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use filewinch_core::{Error, ErrorKind, Protocol, Result};

use crate::setup::Endpoints;

pub use client::ClientConfig;
pub use loopback::FIXED_PORTS;

/// The four cloud drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    GoogleDrive,
    Dropbox,
    OneDrive,
    Box,
}

impl Provider {
    pub const ALL: [Provider; 4] = [
        Provider::GoogleDrive,
        Provider::Dropbox,
        Provider::OneDrive,
        Provider::Box,
    ];

    /// Name shown to people, e.g. in error messages.
    pub fn label(self) -> &'static str {
        match self {
            Provider::GoogleDrive => "Google Drive",
            Provider::Dropbox => "Dropbox",
            Provider::OneDrive => "OneDrive",
            Provider::Box => "Box",
        }
    }

    /// Key in `oauth-clients.json` and the `<PROVIDER>` part of `FILEWINCH_<PROVIDER>_CLIENT_ID`.
    pub fn key(self) -> &'static str {
        match self {
            Provider::GoogleDrive => "google",
            Provider::Dropbox => "dropbox",
            Provider::OneDrive => "onedrive",
            Provider::Box => "box",
        }
    }

    pub fn protocol(self) -> Protocol {
        match self {
            Provider::GoogleDrive => Protocol::GoogleDrive,
            Provider::Dropbox => Protocol::Dropbox,
            Provider::OneDrive => Protocol::OneDrive,
            Provider::Box => Protocol::Box,
        }
    }

    pub fn from_protocol(p: Protocol) -> Option<Provider> {
        Provider::ALL.into_iter().find(|x| x.protocol() == p)
    }

    /// Google and Box refuse the code exchange without the client secret, even for desktop
    /// apps; Dropbox and Microsoft accept PKCE alone.
    pub fn needs_secret(self) -> bool {
        matches!(self, Provider::GoogleDrive | Provider::Box)
    }

    /// Dropbox and Box only accept redirect URIs registered with their exact port.
    pub fn needs_fixed_port(self) -> bool {
        matches!(self, Provider::Dropbox | Provider::Box)
    }

    /// Host in the redirect URI. Microsoft's app registration accepts `http://localhost` with
    /// any port; the others take the loopback IP.
    fn redirect_host(self) -> &'static str {
        match self {
            Provider::OneDrive => "localhost",
            _ => "127.0.0.1",
        }
    }

    fn scopes(self) -> &'static str {
        match self {
            Provider::GoogleDrive => "https://www.googleapis.com/auth/drive",
            Provider::Dropbox => {
                "account_info.read files.metadata.read files.metadata.write \
                 files.content.read files.content.write"
            }
            Provider::OneDrive => "offline_access Files.ReadWrite.All User.Read",
            // Box scopes are set on the app in the Developer Console, not per request.
            Provider::Box => "",
        }
    }

    /// Provider-specific query parameters that make the provider issue a refresh token.
    fn extra_authorize_params(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Provider::GoogleDrive => &[("access_type", "offline"), ("prompt", "consent")],
            Provider::Dropbox => &[("token_access_type", "offline")],
            Provider::OneDrive => &[("prompt", "select_account")],
            Provider::Box => &[],
        }
    }
}

/// Lets the UI stop a sign-in (or anything else that polls it). Cloning shares the flag.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// The result of a successful sign-in.
#[derive(Clone, PartialEq, Eq)]
pub struct SignedIn {
    /// Store in the system secret store; never log it.
    pub refresh_token: String,
    /// The account's email or login, for the sidebar ("ana@example.com"). Falls back to the
    /// provider name if the provider wouldn't say.
    pub account_label: String,
}

impl std::fmt::Debug for SignedIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignedIn")
            .field("refresh_token", &"<redacted>")
            .field("account_label", &self.account_label)
            .finish()
    }
}

/// Opens a URL in a browser.
pub type OpenBrowser = Box<dyn Fn(&str) -> Result<()> + Send + Sync>;

/// Knobs for [`sign_in_with`]. `Default` is what [`sign_in`] uses.
pub struct SignInOptions {
    /// How long to wait for the browser to come back. Default: 5 minutes.
    pub timeout: Duration,
    /// `None` uses the real provider URLs.
    pub endpoints: Option<Endpoints>,
    /// Opens the consent page. Default: the system browser (`open` on macOS, `ShellExecuteW`
    /// on Windows, `xdg-open` on Linux).
    pub open_browser: OpenBrowser,
    /// Ports to try for providers with fixed redirect ports. Default: [`FIXED_PORTS`].
    pub fixed_ports: Vec<u16>,
}

impl Default for SignInOptions {
    fn default() -> Self {
        SignInOptions {
            timeout: Duration::from_secs(300),
            endpoints: None,
            open_browser: Box::new(open_in_browser),
            fixed_ports: FIXED_PORTS.to_vec(),
        }
    }
}

fn open_in_browser(url: &str) -> Result<()> {
    filewinch_os::launch::open_url(url).map_err(|e| {
        Error::new(
            ErrorKind::Other,
            "Filewinch couldn't open your web browser. Set a default browser in your system settings, then try again.",
        )
        .with_detail(e)
    })
}

/// Signs in to `provider` through the system browser. Blocks until the browser comes back,
/// the 5-minute timeout passes, or `cancel` fires. Run it on a worker thread.
pub fn sign_in(provider: Provider, client: &ClientConfig, cancel: CancelToken) -> Result<SignedIn> {
    sign_in_with(provider, client, cancel, SignInOptions::default())
}

/// [`sign_in`] with explicit options.
pub fn sign_in_with(
    provider: Provider,
    client: &ClientConfig,
    cancel: CancelToken,
    opts: SignInOptions,
) -> Result<SignedIn> {
    client.check(provider)?;
    let endpoints = opts
        .endpoints
        .clone()
        .unwrap_or_else(|| Endpoints::for_provider(provider));
    let listener = loopback::Listener::bind(provider, &opts.fixed_ports)?;
    let redirect_uri = format!(
        "http://{}:{}/callback",
        provider.redirect_host(),
        listener.port()
    );
    let verifier = pkce::verifier();
    let state = pkce::random_token();

    let mut params: Vec<(&str, String)> = vec![
        ("response_type", "code".into()),
        ("client_id", client.client_id.clone()),
        ("redirect_uri", redirect_uri.clone()),
        ("code_challenge", pkce::challenge(&verifier)),
        ("code_challenge_method", "S256".into()),
        ("state", state.clone()),
    ];
    if !provider.scopes().is_empty() {
        params.push(("scope", provider.scopes().into()));
    }
    for (k, v) in provider.extra_authorize_params() {
        params.push((k, (*v).into()));
    }
    let url = format!("{}?{}", endpoints.authorize, form_encode(&params));
    (opts.open_browser)(&url)?;

    let deadline = Instant::now() + opts.timeout;
    let code = listener.wait_for_code(provider, &state, &cancel, deadline)?;
    if cancel.is_cancelled() {
        return Err(Error::cancelled());
    }

    let agent = crate::http::agent();
    let grant = token::exchange_code(
        &agent,
        provider,
        client,
        &endpoints.token,
        &code,
        &redirect_uri,
        &verifier,
    )?;
    let refresh_token = grant.refresh_token.clone().ok_or_else(|| {
        Error::new(
            ErrorKind::Auth,
            format!(
                "{} signed you in but didn't allow offline access. Try again.",
                provider.label()
            ),
        )
    })?;
    let account_label = token::account_label(&agent, provider, &endpoints, &grant.access_token)
        .unwrap_or_else(|| provider.label().to_string());
    Ok(SignedIn {
        refresh_token,
        account_label,
    })
}

/// `application/x-www-form-urlencoded` (RFC 3986 unreserved characters pass through).
pub(crate) fn form_encode(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", pct(k), pct(v)))
        .collect::<Vec<_>>()
        .join("&")
}

pub(crate) fn pct(s: &str) -> String {
    use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
    const SET: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    utf8_percent_encode(s, SET).to_string()
}

#[cfg(test)]
mod tests;
