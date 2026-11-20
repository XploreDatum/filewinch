//! Polar's public customer-portal licence-key endpoints (no token): activate, validate and
//! deactivate. Paths and bodies as in Polar's API reference (2026-10 and 2027-01 agree):
//!
//! - `POST /v1/customer-portal/license-keys/activate` `{key, organization_id, label, meta}` →
//!   200 activation `{id, ...}`; 403 `NotPermitted` (limit reached, key not active, no
//!   activations); 404 `ResourceNotFound` (no such key).
//! - `POST /v1/customer-portal/license-keys/validate` `{key, organization_id, activation_id,
//!   benefit_id}` → 200 key `{status, display_key, customer {name, email}, ...}`; 404
//!   `ResourceNotFound` (not found, revoked, disabled, expired, activation gone, other benefit).
//! - `POST /v1/customer-portal/license-keys/deactivate` `{key, organization_id,
//!   activation_id}` → 204; 404 (key or activation not found).
//!
//! No `Polar-Version` header: Polar removes a pinned version after about nine months, and a
//! desktop app outlives that; the default (Current) version keeps these few fields. Responses
//! are read leniently (unknown fields ignored, missing optional ones tolerated).

use std::time::Duration;

use filewinch_http::{Http, ureq};
use serde::Deserialize;
use serde_json::{Value, json};

/// Why a licence call didn't succeed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// No such key (or a key for another product).
    WrongKey,
    /// The key is active on as many devices as it allows.
    LimitReached,
    /// The key exists but can't be activated: revoked, disabled or expired. Polar's words.
    NotActive(String),
    /// Validate said 404 `ResourceNotFound`: revoked, disabled, or this device's activation
    /// was removed. The only answer that un-registers.
    Revoked,
    /// No answer: offline, DNS, TLS, timeout. Never un-registers.
    Offline(String),
    /// An answer that isn't one of the above (5xx, 429, odd 4xx, unreadable body). Never
    /// un-registers.
    Server(String),
    /// The system's secret store or the status file failed; the message says which.
    Storage(String),
}

impl Failure {
    /// What to tell the person.
    pub fn message(&self) -> String {
        match self {
            Failure::WrongKey => "That licence key wasn't recognised. Check it against your receipt email and try again.".into(),
            Failure::LimitReached => format!(
                "This key is already active on {} devices, the most it allows. Free a device in your customer portal, then try again.",
                crate::DEVICE_LIMIT
            ),
            Failure::NotActive(_) => "This licence key is no longer active, so it can't be used on another device. Your customer portal has the details.".into(),
            Failure::Revoked => "This licence is no longer active.".into(),
            Failure::Offline(_) | Failure::Server(_) => "Couldn't reach the licence server. Try again later.".into(),
            Failure::Storage(m) => m.clone(),
        }
    }

    /// The technical detail, for a "Details" disclosure. Never holds the key.
    pub fn detail(&self) -> Option<&str> {
        match self {
            Failure::NotActive(d) | Failure::Offline(d) | Failure::Server(d) => Some(d),
            _ => None,
        }
    }

    /// Whether the customer portal is the place to fix it (shows a link next to the error).
    pub fn wants_portal(&self) -> bool {
        matches!(self, Failure::LimitReached | Failure::NotActive(_))
    }
}

/// Who a key belongs to, from a successful validate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Holder {
    pub name: Option<String>,
    pub email: Option<String>,
    /// Polar's masked form of the key (`****-E304DA`).
    pub display_key: Option<String>,
}

/// Talks to Polar's customer-portal licence-key API for one organisation and benefit.
#[derive(Clone)]
pub struct Polar {
    http: Http,
    base: String,
    organization_id: String,
    benefit_id: String,
    deadline: Duration,
}

/// Longest any one licence call may take, start to finish: it runs in the background, and a
/// person waiting on Activate deserves an answer.
pub const CALL_DEADLINE: Duration = Duration::from_secs(20);

impl Default for Polar {
    fn default() -> Self {
        Polar::new(crate::API_BASE, crate::ORGANIZATION_ID, crate::BENEFIT_ID)
    }
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    error: String,
    #[serde(default)]
    detail: Value,
}

#[derive(Deserialize)]
struct Activation {
    id: String,
}

#[derive(Deserialize)]
struct Customer {
    name: Option<String>,
    email: Option<String>,
}

#[derive(Deserialize)]
struct Validated {
    status: Option<String>,
    benefit_id: Option<String>,
    display_key: Option<String>,
    customer: Option<Customer>,
}

impl Polar {
    pub fn new(base: &str, organization_id: &str, benefit_id: &str) -> Polar {
        Polar {
            http: Http::with_timeouts(Duration::from_secs(10), Duration::from_secs(15)),
            base: base.trim_end_matches('/').to_owned(),
            organization_id: organization_id.to_owned(),
            benefit_id: benefit_id.to_owned(),
            deadline: CALL_DEADLINE,
        }
    }

    /// The same client with a shorter deadline per call (tests).
    pub fn with_deadline(mut self, deadline: Duration) -> Polar {
        self.deadline = deadline;
        self
    }

    /// Activates `key` on this device; returns the activation id.
    pub fn activate(&self, key: &str, label: &str) -> Result<String, Failure> {
        let body = json!({
            "key": key,
            "organization_id": self.organization_id,
            "label": label,
            "meta": { "app": "Filewinch", "version": env!("CARGO_PKG_VERSION"), "os": std::env::consts::OS },
        });
        let (status, text) = self.post("activate", &body)?;
        match status {
            200 | 201 => serde_json::from_str::<Activation>(&text).map(|a| a.id).map_err(|e| Failure::Server(format!("Unreadable activation from Polar: {e}"))),
            403 => {
                let detail = error_detail(&text);
                let lower = detail.to_ascii_lowercase();
                Err(if lower.contains("limit") {
                    Failure::LimitReached
                } else if lower.contains("does not support activations") {
                    // A key of another product of the organisation (one without devices).
                    Failure::WrongKey
                } else {
                    Failure::NotActive(detail)
                })
            }
            404 if is_not_found(&text) => Err(Failure::WrongKey),
            // A malformed key or organisation id: as far as the person is concerned, a wrong key.
            422 => Err(Failure::WrongKey),
            _ => Err(unexpected(status, &text)),
        }
    }

    /// Checks `key` and this device's activation against Filewinch's benefit.
    pub fn validate(&self, key: &str, activation_id: &str) -> Result<Holder, Failure> {
        let body = json!({
            "key": key,
            "organization_id": self.organization_id,
            "activation_id": activation_id,
            "benefit_id": self.benefit_id,
        });
        let (status, text) = self.post("validate", &body)?;
        match status {
            200 => {
                let v: Validated = serde_json::from_str(&text).map_err(|e| Failure::Server(format!("Unreadable validation from Polar: {e}")))?;
                // Polar only answers 200 for a granted key of the asked benefit; anything else
                // here is a contract change, and a contract change never un-registers.
                if v.status.as_deref().is_some_and(|s| s != "granted") || v.benefit_id.as_deref().is_some_and(|b| b != self.benefit_id) {
                    return Err(Failure::Server(format!("Unexpected validation from Polar (status {:?}).", v.status)));
                }
                let customer = v.customer.unwrap_or(Customer { name: None, email: None });
                Ok(Holder { name: nonempty(customer.name), email: nonempty(customer.email), display_key: nonempty(v.display_key) })
            }
            404 if is_not_found(&text) => Err(Failure::Revoked),
            _ => Err(unexpected(status, &text)),
        }
    }

    /// Frees this device's activation. A 404 (already gone) is `Failure::Revoked`.
    pub fn deactivate(&self, key: &str, activation_id: &str) -> Result<(), Failure> {
        let body = json!({ "key": key, "organization_id": self.organization_id, "activation_id": activation_id });
        let (status, text) = self.post("deactivate", &body)?;
        match status {
            200 | 204 => Ok(()),
            404 if is_not_found(&text) => Err(Failure::Revoked),
            _ => Err(unexpected(status, &text)),
        }
    }

    /// POSTs JSON to `/v1/customer-portal/license-keys/<action>`; the status and body text.
    fn post(&self, action: &str, body: &Value) -> Result<(u16, String), Failure> {
        let url = format!("{}/v1/customer-portal/license-keys/{action}", self.base);
        let resp = self.http.agent().post(&url)
            .config().timeout_global(Some(self.deadline)).build()
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .send(body.to_string());
        let mut resp = match resp {
            Ok(r) => r,
            Err(e) => return Err(Failure::Offline(transport(&e))),
        };
        let status = resp.status().as_u16();
        // Bodies here are small; cap what's read so a broken proxy can't feed us gigabytes.
        let text = resp.body_mut().with_config().limit(1 << 20).read_to_string().map_err(|e| Failure::Offline(transport(&e)))?;
        // A validation error echoes its input; the key never reaches a message or a log.
        let key = body["key"].as_str().unwrap_or_default();
        let text = if key.is_empty() { text } else { text.replace(key, "FW-…") };
        Ok((status, text))
    }
}

fn transport(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Timeout(_) => "The licence server didn't answer in time.".into(),
        e => e.to_string(),
    }
}

fn nonempty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

/// Polar's own "not found" answer, as opposed to a 404 from a proxy or captive portal.
fn is_not_found(text: &str) -> bool {
    serde_json::from_str::<ErrorBody>(text).is_ok_and(|b| b.error == "ResourceNotFound")
}

fn error_detail(text: &str) -> String {
    match serde_json::from_str::<ErrorBody>(text) {
        Ok(ErrorBody { detail: Value::String(s), .. }) => s,
        Ok(ErrorBody { detail, error }) if !error.is_empty() => format!("{error}: {detail}"),
        _ => short(text),
    }
}

fn unexpected(status: u16, text: &str) -> Failure {
    Failure::Server(format!("HTTP {status} from the licence server: {}", short(&error_detail(text))))
}

fn short(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() > 300 { format!("{}…", s.chars().take(300).collect::<String>()) } else { s.to_owned() }
}
