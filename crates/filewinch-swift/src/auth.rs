//! Signing in: Keystone v3, Keystone v2 (incl. Rackspace API keys) and v1 (TempAuth).
//! Each returns a token, the storage URL from the catalog, and the expiry when known.

use std::time::SystemTime;

use filewinch_core::{Error, ErrorKind, Result};
use filewinch_http::ureq::Body;
use filewinch_http::ureq::http::Response;
use filewinch_http::{Http, status_error};
use serde::Deserialize;
use serde_json::json;

use crate::config::{AuthConfig, Version};
use crate::time::parse_iso;

/// A signed-in session. No `Debug`: it holds the token.
pub(crate) struct Session {
    pub token: String,
    /// Account URL, no trailing slash.
    pub storage_url: String,
    /// Storage host name, for messages.
    pub host: String,
    pub expires: Option<SystemTime>,
}

/// Largest auth response read (big catalogs are a few hundred KiB).
/// One sign-in method, called with the versioned auth URL.
type SignIn = fn(&Http, &AuthConfig, &str) -> Result<Session>;

const MAX_AUTH_BODY: u64 = 16 * 1024 * 1024;

pub(crate) fn authenticate(http: &Http, cfg: &AuthConfig) -> Result<Session> {
    match cfg.version {
        Version::V3 => v3(http, cfg, &cfg.base),
        Version::V2 => v2(http, cfg, &cfg.base),
        Version::V1 => v1(http, cfg, &cfg.base),
        Version::Detect => {
            let attempts: [(SignIn, &str); 3] = [(v3, "/v3"), (v2, "/v2.0"), (v1, "/auth/v1.0")];
            for (f, suffix) in attempts {
                match f(http, cfg, &format!("{}{suffix}", cfg.base)) {
                    Err(e) if e.kind == ErrorKind::NotFound => continue,
                    r => return r,
                }
            }
            Err(Error::new(
                ErrorKind::NotFound,
                format!(
                    "Couldn't find a Keystone or Swift sign-in service at {}. Check the auth URL \
                     (it usually ends in /v3, /v2.0 or /auth/v1.0).",
                    cfg.host
                ),
            ))
        }
    }
}

fn v3(http: &Http, cfg: &AuthConfig, base: &str) -> Result<Session> {
    let domain = json!({ "name": cfg.domain });
    let mut body = json!({
        "auth": { "identity": { "methods": ["password"], "password": { "user": {
            "name": cfg.user_name, "domain": domain, "password": cfg.secret,
        }}}}
    });
    if let Some(project) = &cfg.project {
        body["auth"]["scope"] = json!({ "project": { "name": project, "domain": domain } });
    }
    let mut resp = post_json(http, cfg, &format!("{base}/auth/tokens"), &body.to_string())?;
    check(resp.status().as_u16(), cfg)?;
    if !resp.status().is_success() {
        return Err(status_error(resp, &format!("sign in to {}", cfg.host)));
    }
    let token = resp
        .headers()
        .get("x-subject-token")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .ok_or_else(|| bad_reply(cfg, "no X-Subject-Token header"))?;
    #[derive(Deserialize)]
    struct Reply {
        token: Token,
    }
    #[derive(Deserialize)]
    struct Token {
        expires_at: Option<String>,
        #[serde(default)]
        catalog: Vec<Service>,
    }
    #[derive(Deserialize)]
    struct Service {
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        endpoints: Vec<Endpoint>,
    }
    #[derive(Deserialize)]
    struct Endpoint {
        interface: String,
        region: Option<String>,
        region_id: Option<String>,
        url: String,
    }
    let reply: Reply = read_json(&mut resp, cfg)?;
    if reply.token.catalog.is_empty() {
        return Err(no_catalog(cfg));
    }
    let endpoints = reply
        .token
        .catalog
        .into_iter()
        .filter(|s| s.kind == "object-store")
        .flat_map(|s| s.endpoints)
        .filter(|e| e.interface.eq_ignore_ascii_case("public"))
        .map(|e| (e.region.or(e.region_id), e.url))
        .collect();
    let url = pick_endpoint(endpoints, cfg.region.as_deref())?;
    session(token, &url, reply.token.expires_at.as_deref().and_then(parse_iso), cfg)
}

fn v2(http: &Http, cfg: &AuthConfig, base: &str) -> Result<Session> {
    let mut auth = if cfg.use_api_key() {
        json!({ "RAX-KSKEY:apiKeyCredentials": { "username": cfg.user_name, "apiKey": cfg.secret } })
    } else {
        json!({ "passwordCredentials": { "username": cfg.user_name, "password": cfg.secret } })
    };
    if let Some(project) = &cfg.project {
        auth["tenantName"] = json!(project);
    }
    let body = json!({ "auth": auth }).to_string();
    let mut resp = post_json(http, cfg, &format!("{base}/tokens"), &body)?;
    check(resp.status().as_u16(), cfg)?;
    if !resp.status().is_success() {
        return Err(status_error(resp, &format!("sign in to {}", cfg.host)));
    }
    #[derive(Deserialize)]
    struct Reply {
        access: Access,
    }
    #[derive(Deserialize)]
    struct Access {
        token: Token,
        #[serde(default, rename = "serviceCatalog")]
        catalog: Vec<Service>,
    }
    #[derive(Deserialize)]
    struct Token {
        id: String,
        expires: Option<String>,
    }
    #[derive(Deserialize)]
    struct Service {
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        endpoints: Vec<Endpoint>,
    }
    #[derive(Deserialize)]
    struct Endpoint {
        region: Option<String>,
        #[serde(rename = "publicURL")]
        public_url: Option<String>,
    }
    let reply: Reply = read_json(&mut resp, cfg)?;
    if reply.access.catalog.is_empty() {
        return Err(no_catalog(cfg));
    }
    let endpoints = reply
        .access
        .catalog
        .into_iter()
        .filter(|s| s.kind == "object-store")
        .flat_map(|s| s.endpoints)
        .filter_map(|e| Some((e.region, e.public_url?)))
        .collect();
    let url = pick_endpoint(endpoints, cfg.region.as_deref())?;
    let expires = reply.access.token.expires.as_deref().and_then(parse_iso);
    session(reply.access.token.id, &url, expires, cfg)
}

fn v1(http: &Http, cfg: &AuthConfig, url: &str) -> Result<Session> {
    let resp = http
        .agent()
        .get(url)
        .header("x-auth-user", &cfg.user)
        .header("x-auth-key", &cfg.secret)
        .call()
        .map_err(|e| http.transport_error(e, &cfg.host))?;
    check(resp.status().as_u16(), cfg)?;
    if !resp.status().is_success() {
        return Err(status_error(resp, &format!("sign in to {}", cfg.host)));
    }
    let header = |name: &str| resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    let token = header("x-auth-token")
        .or_else(|| header("x-storage-token"))
        .ok_or_else(|| bad_reply(cfg, "no X-Auth-Token header"))?;
    let storage = header("x-storage-url").ok_or_else(|| bad_reply(cfg, "no X-Storage-Url header"))?;
    let expires = header("x-auth-token-expires")
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|secs| SystemTime::now() + std::time::Duration::from_secs(secs));
    session(token, &storage, expires, cfg)
}

fn post_json(http: &Http, cfg: &AuthConfig, url: &str, body: &str) -> Result<Response<Body>> {
    http.agent()
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .send(body)
        .map_err(|e| http.transport_error(e, &cfg.host))
}

/// 401 → Auth with a clear message; 404/405 → NotFound (no such service, for detection).
fn check(status: u16, cfg: &AuthConfig) -> Result<()> {
    match status {
        401 => Err(Error::new(
            ErrorKind::Auth,
            format!("{} didn't accept the user name or password for {}.", cfg.host, cfg.user),
        )
        .with_detail(format!("HTTP {status}"))),
        404 | 405 => Err(Error::new(
            ErrorKind::NotFound,
            format!("There's no sign-in service at that address on {}.", cfg.host),
        )
        .with_detail(format!("HTTP {status}"))),
        _ => Ok(()),
    }
}

fn read_json<T: serde::de::DeserializeOwned>(resp: &mut Response<Body>, cfg: &AuthConfig) -> Result<T> {
    let text = resp
        .body_mut()
        .with_config()
        .limit(MAX_AUTH_BODY)
        .read_to_string()
        .map_err(|e| Error::new(ErrorKind::Network, format!("The sign-in reply from {} was cut off.", cfg.host)).with_detail(e))?;
    serde_json::from_str(&text).map_err(|e| bad_reply(cfg, e))
}

fn bad_reply(cfg: &AuthConfig, why: impl std::fmt::Display) -> Error {
    Error::new(ErrorKind::Other, format!("{} sent a sign-in reply Filewinch couldn't understand.", cfg.host))
        .with_detail(why)
}

fn no_catalog(cfg: &AuthConfig) -> Error {
    Error::new(
        ErrorKind::Auth,
        format!(
            "{} signed you in but listed no services. Enter a project (and its domain, if it \
             isn't Default) so Filewinch signs in to a project.",
            cfg.host
        ),
    )
}

/// Picks the object-store endpoint for `region` (case-insensitive) or the first one.
pub(crate) fn pick_endpoint(endpoints: Vec<(Option<String>, String)>, region: Option<&str>) -> Result<String> {
    if endpoints.is_empty() {
        return Err(Error::new(
            ErrorKind::Unsupported,
            "This account has no object storage (Swift) service in its catalog.",
        ));
    }
    let Some(region) = region else { return Ok(endpoints.into_iter().next().map(|e| e.1).unwrap_or_default()) };
    if let Some((_, url)) =
        endpoints.iter().find(|(r, _)| r.as_deref().is_some_and(|r| r.eq_ignore_ascii_case(region)))
    {
        return Ok(url.clone());
    }
    let mut regions: Vec<&str> = endpoints.iter().filter_map(|(r, _)| r.as_deref()).collect();
    regions.dedup();
    Err(Error::new(
        ErrorKind::NotFound,
        format!(
            "This account has no object storage in the region “{region}”. Available regions: {}.",
            if regions.is_empty() { "none named".to_string() } else { regions.join(", ") }
        ),
    ))
}

fn session(token: String, url: &str, expires: Option<SystemTime>, cfg: &AuthConfig) -> Result<Session> {
    let storage_url = url.trim_end_matches('/').to_string();
    let uri: filewinch_http::ureq::http::Uri =
        storage_url.parse().map_err(|_| bad_reply(cfg, "storage URL isn't valid"))?;
    let host = uri.host().ok_or_else(|| bad_reply(cfg, "storage URL has no host"))?.to_string();
    if token.is_empty() {
        return Err(bad_reply(cfg, "empty token"));
    }
    Ok(Session { token, storage_url, host, expires })
}
