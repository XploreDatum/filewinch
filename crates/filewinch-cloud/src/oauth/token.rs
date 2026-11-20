//! Token endpoint calls (code exchange, refresh) and the per-session access-token cache.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use filewinch_core::{Error, ErrorKind, Result, SecretRef, Secrets};
use serde::Deserialize;
use serde_json::Value;
use ureq::Agent;

use super::{ClientConfig, Provider, form_encode};
use crate::http::{read_body, retry_after, transport_error};
use crate::setup::Endpoints;

#[derive(Deserialize)]
pub(crate) struct Grant {
    pub access_token: String,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

pub(crate) fn exchange_code(
    agent: &Agent,
    provider: Provider,
    client: &ClientConfig,
    token_url: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<Grant> {
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("redirect_uri", redirect_uri.to_string()),
        ("code_verifier", verifier.to_string()),
    ];
    add_client(&mut form, client);
    token_request(agent, provider, token_url, &form)
}

pub(crate) fn refresh(
    agent: &Agent,
    provider: Provider,
    client: &ClientConfig,
    token_url: &str,
    refresh_token: &str,
) -> Result<Grant> {
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.to_string()),
    ];
    add_client(&mut form, client);
    token_request(agent, provider, token_url, &form)
}

fn add_client(form: &mut Vec<(&'static str, String)>, client: &ClientConfig) {
    form.push(("client_id", client.client_id.clone()));
    if let Some(s) = client.client_secret.as_ref().filter(|s| !s.is_empty()) {
        form.push(("client_secret", s.clone()));
    }
}

fn token_request(
    agent: &Agent,
    provider: Provider,
    url: &str,
    form: &[(&str, String)],
) -> Result<Grant> {
    let service = provider.label();
    let mut resp = agent
        .post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(form_encode(form))
        .map_err(|e| transport_error(e, service))?;
    let status = resp.status().as_u16();
    let wait = retry_after(resp.headers());
    let body = read_body(&mut resp, service)?;
    if (200..300).contains(&status) {
        return serde_json::from_slice::<Grant>(&body).map_err(|e| {
            Error::new(
                ErrorKind::Auth,
                format!("{service} sent an unreadable sign-in reply."),
            )
            .with_detail(e)
        });
    }
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let code = v["error"].as_str().unwrap_or("");
    let desc = v["error_description"].as_str().unwrap_or(code);
    let detail = format!("HTTP {status} {code}: {desc}");
    let err = match (status, code) {
        (429, _) => {
            let e = Error::new(
                ErrorKind::RateLimited,
                format!(
                    "{service} is limiting sign-in requests. Filewinch will try again shortly."
                ),
            );
            match wait {
                Some(w) => e.with_retry_after(w),
                None => e,
            }
        }
        (500..=599, _) => Error::new(
            ErrorKind::Network,
            format!("{service}'s sign-in service isn't responding properly. Filewinch will retry."),
        ),
        (_, "invalid_grant") => Error::new(
            ErrorKind::Auth,
            format!("Your {service} sign-in has expired or was revoked. Sign in again."),
        ),
        (_, "invalid_client" | "unauthorized_client") => Error::new(
            ErrorKind::Auth,
            format!(
                "{service} rejected Filewinch's client ID. Check oauth-clients.json \
                 (see docs/oauth-setup.md)."
            ),
        ),
        _ => Error::new(
            ErrorKind::Auth,
            format!("{service} refused the sign-in: {desc}"),
        ),
    };
    Err(err.with_detail(detail))
}

/// Looks up the signed-in account's email/login for the sidebar. `None` if the provider
/// wouldn't say; the caller falls back to the provider name.
pub(crate) fn account_label(
    agent: &Agent,
    provider: Provider,
    endpoints: &Endpoints,
    access: &str,
) -> Option<String> {
    let bearer = format!("Bearer {access}");
    let api = &endpoints.api;
    let resp = match provider {
        Provider::GoogleDrive => agent
            .get(format!("{api}/about"))
            .query("fields", "user(displayName,emailAddress)")
            .header("Authorization", &bearer)
            .call(),
        Provider::Dropbox => agent
            .post(format!("{api}/users/get_current_account"))
            .header("Authorization", &bearer)
            .header("Content-Type", "application/json")
            .send("null"),
        Provider::OneDrive => agent
            .get(format!("{api}/me"))
            .header("Authorization", &bearer)
            .call(),
        Provider::Box => agent
            .get(format!("{api}/users/me"))
            .header("Authorization", &bearer)
            .call(),
    };
    let mut resp = resp.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&read_body(&mut resp, provider.label()).ok()?).ok()?;
    let pick = |ptrs: &[&str]| {
        ptrs.iter()
            .find_map(|p| {
                v.pointer(p)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
            })
            .map(str::to_string)
    };
    match provider {
        Provider::GoogleDrive => pick(&["/user/emailAddress", "/user/displayName"]),
        Provider::Dropbox => pick(&["/email", "/name/display_name"]),
        Provider::OneDrive => pick(&["/userPrincipalName", "/mail", "/displayName"]),
        Provider::Box => pick(&["/login", "/name"]),
    }
}

/// Access tokens for one session: cached in memory, refreshed shortly before expiry and once
/// more when the API answers 401. A refresh that returns a new refresh token (rotation) updates
/// the session and saves it through [`Secrets::set`] (the system secret store in the app).
pub(crate) struct TokenSource {
    agent: Agent,
    provider: Provider,
    client: ClientConfig,
    token_url: String,
    secret: SecretRef,
    secrets: Arc<dyn Secrets>,
    state: Mutex<State>,
}

struct State {
    refresh: String,
    access: Option<String>,
    renew_at: Instant,
}

impl TokenSource {
    pub(crate) fn new(
        agent: Agent,
        provider: Provider,
        client: ClientConfig,
        token_url: String,
        secret: SecretRef,
        refresh: String,
        secrets: Arc<dyn Secrets>,
    ) -> Self {
        TokenSource {
            agent,
            provider,
            client,
            token_url,
            secret,
            secrets,
            state: Mutex::new(State {
                refresh,
                access: None,
                renew_at: Instant::now(),
            }),
        }
    }

    /// A valid access token, refreshing first if the cached one is missing or about to expire.
    pub(crate) fn access(&self) -> Result<String> {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        match &st.access {
            Some(a) if Instant::now() < st.renew_at => Ok(a.clone()),
            _ => self.renew(&mut st),
        }
    }

    /// The API rejected `rejected` with 401: refresh unless another thread already did.
    pub(crate) fn after_unauthorized(&self, rejected: &str) -> Result<String> {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        match &st.access {
            Some(a) if a != rejected && Instant::now() < st.renew_at => Ok(a.clone()),
            _ => self.renew(&mut st),
        }
    }

    fn renew(&self, st: &mut State) -> Result<String> {
        let grant = refresh(
            &self.agent,
            self.provider,
            &self.client,
            &self.token_url,
            &st.refresh,
        )?;
        let lifetime = Duration::from_secs(grant.expires_in.unwrap_or(3600).max(1));
        // Renew a minute early (or halfway, for very short-lived tokens).
        let margin = Duration::from_secs(60).min(lifetime / 2);
        st.renew_at = Instant::now() + lifetime - margin;
        st.access = Some(grant.access_token.clone());
        if let Some(new) = grant
            .refresh_token
            .filter(|t| !t.is_empty() && *t != st.refresh)
        {
            st.refresh = new;
            // The session carries on with the new token either way; a failed save only means
            // the next launch has to sign in again, so say so where the log can show it.
            if let Err(e) = self.secrets.set(&self.secret, &st.refresh) {
                eprintln!("filewinch: couldn't save the renewed {} sign-in ({}): {}", self.provider.label(), self.secret.0, e.message);
            }
        }
        Ok(grant.access_token)
    }
}
