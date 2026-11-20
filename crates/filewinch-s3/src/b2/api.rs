//! B2 native API (v2): account authorization, JSON calls with transparent re-authorization,
//! and error mapping.

use std::collections::HashMap;
use std::io::Read;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine;
use filewinch_core::{Error, ErrorKind, Result};
use filewinch_http::Http;
use filewinch_http::ureq::{Body, http::HeaderMap, http::Response};
use serde_json::{Value, json};

use crate::s3::lock;

/// Default API host for `b2_authorize_account`.
pub(crate) const DEFAULT_API: &str = "https://api.backblazeb2.com";
const MAX_JSON: u64 = 32 * 1024 * 1024;
/// Longest server-requested wait honoured inside one call before giving up to the engine.
const MAX_INLINE_WAIT: Duration = Duration::from_secs(2);

/// What `b2_authorize_account` returned. `Debug` never shows the token.
#[derive(Clone)]
pub(crate) struct Account {
    pub account_id: String,
    pub token: String,
    pub api_url: String,
    pub download_url: String,
    pub recommended_part: u64,
    pub min_part: u64,
    /// A key restricted to one bucket: (id, name).
    pub bucket: Option<(String, String)>,
}

/// Part sizes and copy limits. Fields so tests can shrink them.
#[derive(Clone, Debug)]
pub(crate) struct Limits {
    /// Most memory one upload buffers for a part (each part is hashed before sending).
    pub max_buffered_part: u64,
    /// Files at least this big (or the recommended part size, if smaller) use the
    /// large-file API.
    pub large_from: u64,
    pub max_parts: u64,
    /// Largest file `b2_copy_file` copies in one call.
    pub copy_max: u64,
    pub copy_part: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_buffered_part: 32 * 1024 * 1024,
            large_from: 100 * 1000 * 1000,
            max_parts: 10_000,
            copy_max: 5 * 1000 * 1000 * 1000,
            copy_part: 1024 * 1024 * 1024,
        }
    }
}

pub(crate) struct Session {
    pub http: Http,
    auth_base: String,
    key_id: String,
    app_key: String,
    account: Mutex<Account>,
    /// Bucket name → id.
    buckets: Mutex<HashMap<String, String>>,
    /// Reusable upload URLs per bucket id: (upload URL, upload token).
    upload_urls: Mutex<Vec<(String, String, String)>>,
    closed: AtomicBool,
    pub limits: Limits,
}

impl Session {
    pub fn open(http: Http, auth_base: String, key_id: String, app_key: String, limits: Limits) -> Result<Session> {
        let account = authorize(&http, &auth_base, &key_id, &app_key)?;
        let mut buckets = HashMap::new();
        if let Some((id, name)) = &account.bucket {
            buckets.insert(name.clone(), id.clone());
        }
        Ok(Session {
            http,
            auth_base,
            key_id,
            app_key,
            account: Mutex::new(account),
            buckets: Mutex::new(buckets),
            upload_urls: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            limits,
        })
    }

    pub fn account(&self) -> Account {
        lock(&self.account).clone()
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    pub fn check_open(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Error::new(ErrorKind::Closed, "The connection was closed."));
        }
        Ok(())
    }

    /// Re-authorizes unless another thread already replaced `failed_token`.
    fn reauthorize(&self, failed_token: &str) -> Result<()> {
        let mut account = lock(&self.account);
        if account.token != failed_token {
            return Ok(());
        }
        *account = authorize(&self.http, &self.auth_base, &self.key_id, &self.app_key)?;
        lock(&self.upload_urls).clear();
        Ok(())
    }

    /// Runs `send(account)`; on an expired or bad auth token, re-authorizes once and runs
    /// it again. A second auth failure is an `Auth` error.
    pub fn with_auth<T>(&self, what: &str, mut send: impl FnMut(&Account) -> std::result::Result<T, Failure>) -> Result<T> {
        self.check_open()?;
        let mut reauthed = false;
        loop {
            let account = self.account();
            match send(&account) {
                Ok(v) => return Ok(v),
                Err(Failure::TokenExpired(e)) if !reauthed => {
                    reauthed = true;
                    self.reauthorize(&account.token)?;
                    let _ = e;
                }
                Err(Failure::TokenExpired(e)) => {
                    let mut e = e;
                    e.kind = ErrorKind::Auth;
                    e.message = format!("Couldn't {what}: Backblaze B2 keeps rejecting the session. Sign in again.");
                    return Err(e);
                }
                Err(Failure::Error(e)) => return Err(e),
            }
        }
    }

    /// POSTs a JSON call to `{apiUrl}/b2api/v2/{api}`. Retries once after a dropped
    /// connection or a short-lived 5xx unless `idempotent` is false.
    pub fn call(&self, api: &str, body: &Value, what: &str, idempotent: bool) -> Result<Value> {
        let text = body.to_string();
        let mut retried = false;
        loop {
            let result = self.with_auth(what, |acct| {
                let url = format!("{}/b2api/v2/{api}", acct.api_url);
                let resp = self
                    .http
                    .agent()
                    .post(&url)
                    .header("authorization", acct.token.as_str())
                    .header("content-type", "application/json")
                    .send(text.as_str())
                    .map_err(|e| Failure::Error(self.http.transport_error(e, &host_of(&acct.api_url))))?;
                json_or_failure(resp, what, &host_of(&acct.api_url))
            });
            match result {
                Err(e) if idempotent && !retried && is_quick_retry(&e) => {
                    retried = true;
                    std::thread::sleep(e.retry_after.unwrap_or(Duration::from_millis(300)));
                }
                other => return other,
            }
        }
    }

    pub fn bucket_id(&self, name: &str) -> Result<String> {
        if let Some(id) = lock(&self.buckets).get(name) {
            return Ok(id.clone());
        }
        let acct = self.account();
        let what = format!("open the bucket “{name}”");
        let v = self.call("b2_list_buckets", &json!({"accountId": acct.account_id, "bucketName": name}), &what, true)?;
        let id = v["buckets"]
            .as_array()
            .and_then(|b| b.iter().find(|b| b["bucketName"] == name))
            .and_then(|b| b["bucketId"].as_str())
            .ok_or_else(|| Error::new(ErrorKind::NotFound, format!("The bucket “{name}” doesn't exist.")))?
            .to_string();
        lock(&self.buckets).insert(name.to_string(), id.clone());
        Ok(id)
    }

    pub fn remember_bucket(&self, name: &str, id: &str) {
        lock(&self.buckets).insert(name.to_string(), id.to_string());
    }

    pub fn forget_bucket(&self, name: &str) {
        lock(&self.buckets).remove(name);
    }

    /// An upload URL for `bucket_id`: a pooled one, else a fresh `b2_get_upload_url`.
    pub fn take_upload_url(&self, bucket_id: &str, what: &str) -> Result<(String, String)> {
        {
            let mut pool = lock(&self.upload_urls);
            if let Some(i) = pool.iter().position(|(b, _, _)| b == bucket_id) {
                let (_, url, token) = pool.swap_remove(i);
                return Ok((url, token));
            }
        }
        let v = self.call("b2_get_upload_url", &json!({"bucketId": bucket_id}), what, true)?;
        upload_target(&v, what)
    }

    /// Returns an upload URL that just worked, for reuse.
    pub fn give_back_upload_url(&self, bucket_id: &str, url: String, token: String) {
        let mut pool = lock(&self.upload_urls);
        if pool.len() < 16 {
            pool.push((bucket_id.to_string(), url, token));
        }
    }
}

pub(crate) fn upload_target(v: &Value, what: &str) -> Result<(String, String)> {
    match (v["uploadUrl"].as_str(), v["authorizationToken"].as_str()) {
        (Some(u), Some(t)) => Ok((u.to_string(), t.to_string())),
        _ => Err(Error::new(ErrorKind::Other, format!("Couldn't {what}: Backblaze B2 didn't hand out an upload address."))),
    }
}

fn is_quick_retry(e: &Error) -> bool {
    e.kind == ErrorKind::Network || (e.kind == ErrorKind::RateLimited && e.retry_after.is_some_and(|w| w <= MAX_INLINE_WAIT))
}

/// A failed attempt: either the auth token needs renewing, or a final error.
pub(crate) enum Failure {
    TokenExpired(Error),
    Error(Error),
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        Failure::Error(e)
    }
}

/// "api.backblazeb2.com" from a URL, for messages.
pub(crate) fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split('/').next().unwrap_or(rest).to_string()
}

fn authorize(http: &Http, base: &str, key_id: &str, app_key: &str) -> Result<Account> {
    let host = host_of(base);
    let what = "sign in to Backblaze B2";
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{key_id}:{app_key}"));
    let resp = http
        .agent()
        .get(format!("{}/b2api/v2/b2_authorize_account", base.trim_end_matches('/')))
        .header("authorization", format!("Basic {basic}"))
        .call()
        .map_err(|e| http.transport_error(e, &host))?;
    let v = match json_or_failure(resp, what, &host) {
        Ok(v) => v,
        Err(Failure::TokenExpired(e)) | Err(Failure::Error(e)) if matches!(e.kind, ErrorKind::Auth | ErrorKind::PermissionDenied) => {
            let mut e = e;
            e.kind = ErrorKind::Auth;
            e.message = "Backblaze B2 rejected the key ID or application key.".into();
            return Err(e);
        }
        Err(Failure::TokenExpired(e)) | Err(Failure::Error(e)) => return Err(e),
    };
    let s = |k: &str| v[k].as_str().map(str::to_string);
    let bad = || Error::new(ErrorKind::Other, "Backblaze B2 sent a sign-in reply Filewinch couldn't read.");
    let allowed = &v["allowed"];
    let bucket = match (allowed["bucketId"].as_str(), allowed["bucketName"].as_str()) {
        (Some(id), Some(name)) => Some((id.to_string(), name.to_string())),
        _ => None,
    };
    Ok(Account {
        account_id: s("accountId").ok_or_else(bad)?,
        token: s("authorizationToken").ok_or_else(bad)?,
        api_url: s("apiUrl").ok_or_else(bad)?.trim_end_matches('/').to_string(),
        download_url: s("downloadUrl").ok_or_else(bad)?.trim_end_matches('/').to_string(),
        recommended_part: v["recommendedPartSize"].as_u64().unwrap_or(100_000_000),
        min_part: v["absoluteMinimumPartSize"].as_u64().unwrap_or(5_000_000),
        bucket,
    })
}

/// Reads a JSON success body, or classifies the failure.
pub(crate) fn json_or_failure(resp: Response<Body>, what: &str, host: &str) -> std::result::Result<Value, Failure> {
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let mut buf = Vec::new();
    let read = resp.into_body().into_reader().take(MAX_JSON).read_to_end(&mut buf);
    if (200..300).contains(&status) {
        read.map_err(|e| crate::body::body_error(e, what, host))?;
        return serde_json::from_slice(&buf).map_err(|e| {
            Failure::Error(
                Error::new(ErrorKind::Other, format!("Couldn't {what}: Backblaze B2 sent a reply Filewinch couldn't read."))
                    .with_detail(e),
            )
        });
    }
    Err(classify(status, &headers, &String::from_utf8_lossy(&buf), what))
}

/// Maps a B2 error (`{"status":…, "code":"…", "message":"…"}`).
pub(crate) fn classify(status: u16, headers: &HeaderMap, body: &str, what: &str) -> Failure {
    let base = filewinch_http::status_error_parts(status, headers, body, what);
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let code = parsed["code"].as_str().unwrap_or("");
    let says = parsed["message"].as_str().unwrap_or("").trim();
    let with = |kind: ErrorKind, message: String| {
        let mut e = Error::new(kind, message);
        e.detail = base.detail.clone();
        e.retry_after = base.retry_after;
        e
    };
    let e = match (status, code) {
        (401, "expired_auth_token" | "bad_auth_token") => {
            return Failure::TokenExpired(with(ErrorKind::Auth, format!("Couldn't {what}: the B2 session expired.")));
        }
        (401, _) => with(ErrorKind::PermissionDenied, format!("Couldn't {what}: the application key doesn't allow this.")),
        (403, "storage_cap_exceeded" | "cap_exceeded") => {
            with(ErrorKind::NoSpace, format!("Couldn't {what}: the account's storage cap is reached."))
        }
        (403, "transaction_cap_exceeded" | "download_cap_exceeded") => with(
            ErrorKind::Other,
            format!("Couldn't {what}: the account's daily B2 cap is reached. Raise it in the Backblaze web site."),
        ),
        (403, _) => with(ErrorKind::PermissionDenied, format!("Couldn't {what}: the application key doesn't allow this.")),
        (_, "not_found" | "file_not_present" | "no_such_file" | "bad_bucket_id") => {
            with(ErrorKind::NotFound, format!("Couldn't {what}: it doesn't exist."))
        }
        (_, "duplicate_bucket_name") => {
            with(ErrorKind::AlreadyExists, format!("Couldn't {what}: that bucket name is already taken."))
        }
        (_, "cannot_delete_non_empty_bucket") => {
            with(ErrorKind::InvalidPath, format!("Couldn't {what}: the bucket isn't empty."))
        }
        (_, "range_not_satisfiable") | (416, _) => {
            with(ErrorKind::InvalidPath, format!("Couldn't {what}: the file is shorter than the requested position."))
        }
        (429, _) | (503, _) => {
            with(ErrorKind::RateLimited, format!("Couldn't {what}: Backblaze B2 is busy, trying again shortly."))
        }
        (408, _) | (500..=599, _) => with(ErrorKind::Network, format!("Couldn't {what}: Backblaze B2 had a temporary problem.")),
        (400, _) if !says.is_empty() => {
            let mut e = base;
            e.message = format!("Couldn't {what}: {}", if says.ends_with('.') { says.to_string() } else { format!("{says}.") });
            e
        }
        _ => base,
    };
    Failure::Error(e)
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod tests;
