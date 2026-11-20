//! The licence rules against a mock Polar: activate (valid, wrong key, limit reached, not
//! active, another product's key), the daily check (confirmed, revoked, 5xx, timeout, offline,
//! a proxy's 404, throttle), deactivate, persistence, and the key helpers.

mod mock;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use filewinch_core::{Error, ErrorKind, Result};
use filewinch_license::{Check, Failure, Licensing, Polar, Status, clean_key, mask_key};
use filewinch_store::{MemorySecrets, SecretStore};
use mock::{Mock, Reply, dead_base};

const ORG: &str = "f2434cea-855e-43e8-937e-f1cf6947bd04";
const BENEFIT: &str = "7123bf4e-0ddc-4b48-a8bb-cf1816b48a19";
const KEY: &str = "FW-1C285B2D-6CE6-4BC7-B8BE-ADB6A7E304DA";
const DAY: u64 = 24 * 60 * 60;
const T0: u64 = 1_795_000_000;

const ACTIVATED: &str = r#"{"id":"b6724bc8-7ad9-4ca0-b143-7c896fcbb6fe","license_key_id":"508176f7","label":"Filewinch on test","meta":{},"created_at":"2026-11-20T04:00:00Z","modified_at":null,"license_key":{"id":"508176f7","status":"granted"}}"#;
const VALID: &str = r#"{"id":"508176f7","status":"granted","benefit_id":"7123bf4e-0ddc-4b48-a8bb-cf1816b48a19","display_key":"****-E304DA","customer":{"name":"Alex Morgan","email":"alex@example.com"},"unknown_future_field":[1,2]}"#;
const VALID_RENAMED: &str = r#"{"status":"granted","benefit_id":"7123bf4e-0ddc-4b48-a8bb-cf1816b48a19","customer":{"name":"Alex M. Morgan","email":"alex@example.com"}}"#;
const NOT_FOUND: &str = r#"{"error":"ResourceNotFound","detail":"Not found"}"#;
const LIMIT: &str = r#"{"error":"NotPermitted","detail":"License key activation limit already reached"}"#;
const INACTIVE: &str = r#"{"error":"NotPermitted","detail":"License key is no longer active. This license key can not be activated."}"#;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("license-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn polar(base: &str) -> Polar {
    Polar::new(base, ORG, BENEFIT).with_deadline(Duration::from_millis(800))
}

fn licensing(base: &str, dir: &Scratch) -> (Licensing, Arc<MemorySecrets>) {
    let secrets = Arc::new(MemorySecrets::default());
    (Licensing::new(polar(base), secrets.clone(), Some(dir.0.clone())), secrets)
}

/// A device registered at T0 against `m`.
fn registered(m: &Mock, dir: &Scratch) -> (Licensing, Arc<MemorySecrets>) {
    m.on("activate", Reply::Json(200, ACTIVATED)).on("validate", Reply::Json(200, VALID));
    let (l, s) = licensing(&m.base, dir);
    l.activate(KEY, "Filewinch on test", T0).expect("activates");
    (l, s)
}

#[test]
fn a_valid_key_registers_this_device() {
    let m = Mock::start();
    let dir = Scratch::new("valid");
    let (l, secrets) = registered(&m, &dir);
    let Status::Registered(r) = l.status() else { panic!("registered") };
    assert_eq!(r.holder(), Some("Alex Morgan"));
    assert_eq!(r.email.as_deref(), Some("alex@example.com"));
    assert_eq!(r.masked_key, "FW-••••-E304DA");
    assert_eq!(r.label, "Filewinch on test");
    assert_eq!((r.activated_at, r.last_validated, r.last_checked), (T0, T0, T0));
    // The key and activation are secrets; the status file never holds the key.
    assert_eq!(secrets.get("license/key").unwrap().as_deref(), Some(KEY));
    assert_eq!(secrets.get("license/activation").unwrap().as_deref(), Some("b6724bc8-7ad9-4ca0-b143-7c896fcbb6fe"));
    let file = fs::read_to_string(dir.0.join("license.json")).unwrap();
    assert!(!file.contains(KEY) && file.contains("Alex Morgan"), "{file}");
    // What was sent: the organisation everywhere, the label on activate, the benefit on validate.
    let seen = m.seen();
    assert_eq!(seen[0].0, "activate");
    assert_eq!(seen[0].1["organization_id"], ORG);
    assert_eq!(seen[0].1["label"], "Filewinch on test");
    assert_eq!(seen[0].1["key"], KEY);
    assert_eq!(seen[1].0, "validate");
    assert_eq!(seen[1].1["benefit_id"], BENEFIT);
    assert_eq!(seen[1].1["activation_id"], "b6724bc8-7ad9-4ca0-b143-7c896fcbb6fe");
    // A fresh start reads the same status from disk.
    let (again, _) = licensing(&m.base, &dir);
    assert_eq!(again.status(), l.status());
}

#[test]
fn a_wrong_key_says_so_and_saves_nothing() {
    let m = Mock::start();
    m.on("activate", Reply::Json(404, NOT_FOUND));
    let dir = Scratch::new("wrong");
    let (l, secrets) = licensing(&m.base, &dir);
    let e = l.activate(KEY, "x", T0).unwrap_err();
    assert_eq!(e, Failure::WrongKey);
    assert!(e.message().contains("wasn't recognised"));
    assert_eq!(l.status(), Status::Unregistered);
    assert_eq!(secrets.get("license/key").unwrap(), None);
    assert!(!dir.0.join("license.json").exists());
    // A malformed key Polar rejects (422) is also just a wrong key.
    m.on("activate", Reply::Json(422, r#"{"detail":[{"type":"x","input":"FW-1C285B2D-6CE6-4BC7-B8BE-ADB6A7E304DA"}]}"#));
    assert_eq!(l.activate(KEY, "x", T0).unwrap_err(), Failure::WrongKey);
}

#[test]
fn the_activation_limit_points_to_the_portal() {
    let m = Mock::start();
    m.on("activate", Reply::Json(403, LIMIT));
    let dir = Scratch::new("limit");
    let (l, _) = licensing(&m.base, &dir);
    let e = l.activate(KEY, "x", T0).unwrap_err();
    assert_eq!(e, Failure::LimitReached);
    assert!(e.wants_portal());
    assert!(e.message().contains("5 devices"), "{}", e.message());
    assert_eq!(l.status(), Status::Unregistered);
}

#[test]
fn a_revoked_key_cant_be_activated() {
    let m = Mock::start();
    m.on("activate", Reply::Json(403, INACTIVE));
    let dir = Scratch::new("inactive");
    let (l, _) = licensing(&m.base, &dir);
    let e = l.activate(KEY, "x", T0).unwrap_err();
    assert!(matches!(&e, Failure::NotActive(d) if d.contains("no longer active")), "{e:?}");
    assert!(e.wants_portal());
}

#[test]
fn another_products_key_is_released_and_refused() {
    let m = Mock::start();
    m.on("activate", Reply::Json(200, ACTIVATED)).on("validate", Reply::Json(404, NOT_FOUND)).on("deactivate", Reply::Json(204, ""));
    let dir = Scratch::new("other");
    let (l, secrets) = licensing(&m.base, &dir);
    assert_eq!(l.activate(KEY, "x", T0).unwrap_err(), Failure::WrongKey);
    assert_eq!(m.count("deactivate"), 1, "the slot it took is given back");
    assert_eq!(secrets.get("license/key").unwrap(), None);
    assert_eq!(l.status(), Status::Unregistered);
}

#[test]
fn activation_survives_a_failed_confirmation() {
    // Activated, then the confirming validate times out: registered all the same.
    let m = Mock::start();
    m.on("activate", Reply::Json(200, ACTIVATED)).on("validate", Reply::Json(503, "{}"));
    let dir = Scratch::new("half");
    let (l, _) = licensing(&m.base, &dir);
    let r = l.activate(KEY, "x", T0).expect("registered");
    assert_eq!((r.holder(), r.last_validated), (None, 0));
    assert!(l.status().is_registered());
}

#[test]
fn activating_offline_says_try_later() {
    let dir = Scratch::new("offline-activate");
    let (l, _) = licensing(&dead_base(), &dir);
    let e = l.activate(KEY, "x", T0).unwrap_err();
    assert!(matches!(e, Failure::Offline(_)), "{e:?}");
    assert_eq!(e.message(), "Couldn't reach the licence server. Try again later.");
    let m = Mock::start();
    m.on("activate", Reply::Json(502, "<html>bad gateway</html>"));
    let (l, _) = licensing(&m.base, &dir);
    let e = l.activate(KEY, "x", T0).unwrap_err();
    assert!(matches!(e, Failure::Server(_)), "{e:?}");
    assert_eq!(e.message(), "Couldn't reach the licence server. Try again later.");
}

struct BrokenSecrets;

impl SecretStore for BrokenSecrets {
    fn get(&self, _: &str) -> Result<Option<String>> {
        Err(Error::new(ErrorKind::PermissionDenied, "Filewinch can't use your Keychain."))
    }
    fn set(&self, _: &str, _: &str) -> Result<()> {
        Err(Error::new(ErrorKind::PermissionDenied, "Filewinch can't use your Keychain."))
    }
    fn delete(&self, _: &str) -> Result<()> {
        Ok(())
    }
}

#[test]
fn a_secret_store_failure_releases_the_activation() {
    let m = Mock::start();
    m.on("activate", Reply::Json(200, ACTIVATED)).on("validate", Reply::Json(200, VALID)).on("deactivate", Reply::Json(204, ""));
    let dir = Scratch::new("keychain");
    let l = Licensing::new(polar(&m.base), Arc::new(BrokenSecrets), Some(dir.0.clone()));
    let e = l.activate(KEY, "x", T0).unwrap_err();
    assert!(matches!(&e, Failure::Storage(m) if m.starts_with("Filewinch can't use your Keychain.")), "{e:?}");
    assert_eq!(m.count("deactivate"), 1);
    assert_eq!(l.status(), Status::Unregistered);
}

#[test]
fn the_daily_check_runs_at_most_once_a_day() {
    let m = Mock::start();
    let dir = Scratch::new("throttle");
    let (l, _) = registered(&m, &dir);
    let before = m.count("validate");
    assert_eq!(l.check(T0 + 60), Check::NotDue);
    assert_eq!(l.check(T0 + DAY - 1), Check::NotDue);
    assert_eq!(m.count("validate"), before, "no request inside the day");
    m.on("validate", Reply::Json(200, VALID_RENAMED));
    let Check::Confirmed(r) = l.check(T0 + DAY) else { panic!("checked") };
    assert_eq!(r.holder(), Some("Alex M. Morgan"), "the name follows Polar");
    assert_eq!((r.last_validated, r.last_checked), (T0 + DAY, T0 + DAY));
    assert_eq!(l.check(T0 + DAY + 5), Check::NotDue);
    // A clock set back is due at once (else a wrong date could stop checks for years).
    m.on("validate", Reply::Json(200, VALID));
    assert!(matches!(l.check(T0), Check::Confirmed(_)));
    // The throttle survives a restart.
    let (again, _) = licensing(&m.base, &dir);
    assert_eq!(again.check(T0 + 10), Check::NotDue);
}

#[test]
fn a_revoked_key_returns_to_unregistered() {
    let m = Mock::start();
    let dir = Scratch::new("revoked");
    let (l, secrets) = registered(&m, &dir);
    m.on("validate", Reply::Json(404, NOT_FOUND));
    assert_eq!(l.check(T0 + DAY), Check::Revoked);
    assert_eq!(l.status(), Status::Unregistered);
    assert_eq!(secrets.get("license/key").unwrap(), None);
    assert_eq!(secrets.get("license/activation").unwrap(), None);
    assert!(!dir.0.join("license.json").exists());
    assert_eq!(l.check(T0 + 3 * DAY), Check::Unregistered);
}

#[test]
fn server_errors_and_timeouts_never_unregister() {
    let m = Mock::start();
    let dir = Scratch::new("5xx");
    let (l, _) = registered(&m, &dir);
    m.on("validate", Reply::Json(500, r#"{"error":"InternalServerError"}"#));
    assert!(matches!(l.check(T0 + DAY), Check::Skipped(_)));
    assert!(l.status().is_registered());
    // A 404 that isn't Polar's own answer (a proxy, a captive portal) isn't a revocation.
    m.on("validate", Reply::Text(404, "<html>Not Found</html>"));
    assert!(matches!(l.check(T0 + 2 * DAY), Check::Skipped(_)));
    // Too many requests, or an odd 200 from a changed contract: still registered.
    m.on("validate", Reply::Json(429, "{}"));
    assert!(matches!(l.check(T0 + 3 * DAY), Check::Skipped(_)));
    m.on("validate", Reply::Json(200, r#"{"status":"revoked"}"#));
    assert!(matches!(l.check(T0 + 4 * DAY), Check::Skipped(_)));
    m.on("validate", Reply::Json(200, "not json"));
    assert!(matches!(l.check(T0 + 5 * DAY), Check::Skipped(_)));
    // A server that never answers: the call gives up on its deadline.
    m.on("validate", Reply::Hang(Duration::from_secs(5)));
    let started = Instant::now();
    assert!(matches!(l.check(T0 + 6 * DAY), Check::Skipped(_)));
    assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
    let Status::Registered(r) = l.status() else { panic!("still registered") };
    assert_eq!(r.holder(), Some("Alex Morgan"));
    assert_eq!(r.last_validated, T0, "only a real confirmation counts");
    assert_eq!(r.last_checked, T0 + 6 * DAY, "but every attempt is throttled");
}

#[test]
fn offline_stays_registered_indefinitely() {
    let m = Mock::start();
    let dir = Scratch::new("offline");
    let (_, secrets) = registered(&m, &dir);
    // Months later, with no network at all.
    let l = Licensing::new(polar(&dead_base()), secrets.clone(), Some(dir.0.clone()));
    for day in 1..=400u64 {
        if day % 50 == 0 {
            assert!(matches!(l.check(T0 + day * DAY), Check::Skipped(_)));
        }
    }
    assert!(l.status().is_registered());
    assert_eq!(secrets.get("license/key").unwrap().as_deref(), Some(KEY));
}

#[test]
fn a_locked_secret_store_skips_the_check_but_stays_registered() {
    let m = Mock::start();
    let dir = Scratch::new("locked");
    let _ = registered(&m, &dir);
    let l = Licensing::new(polar(&m.base), Arc::new(BrokenSecrets), Some(dir.0.clone()));
    assert!(matches!(l.check(T0 + DAY), Check::Skipped(_)));
    assert!(l.status().is_registered());
}

#[test]
fn deactivating_frees_the_slot() {
    let m = Mock::start();
    let dir = Scratch::new("deactivate");
    let (l, secrets) = registered(&m, &dir);
    // Offline: nothing changes, try again later.
    let offline = Licensing::new(polar(&dead_base()), secrets.clone(), Some(dir.0.clone()));
    assert!(matches!(offline.deactivate(), Err(Failure::Offline(_))));
    assert!(offline.status().is_registered());
    m.on("deactivate", Reply::Json(204, ""));
    l.deactivate().expect("deactivates");
    let (_, body) = m.seen().into_iter().rfind(|(a, _)| a == "deactivate").unwrap();
    assert_eq!(body["activation_id"], "b6724bc8-7ad9-4ca0-b143-7c896fcbb6fe");
    assert_eq!(body["organization_id"], ORG);
    assert_eq!(l.status(), Status::Unregistered);
    assert_eq!(secrets.get("license/key").unwrap(), None);
    assert!(!dir.0.join("license.json").exists());
}

#[test]
fn deactivating_a_slot_polar_already_freed_succeeds() {
    let m = Mock::start();
    let dir = Scratch::new("deactivate-gone");
    let (l, _) = registered(&m, &dir);
    m.on("deactivate", Reply::Json(404, NOT_FOUND));
    l.deactivate().expect("already free counts as freed");
    assert_eq!(l.status(), Status::Unregistered);
}

#[test]
fn the_key_never_appears_in_error_details() {
    let m = Mock::start();
    let dir = Scratch::new("redact");
    let (l, _) = registered(&m, &dir);
    m.on("deactivate", Reply::Json(422, r#"{"detail":[{"msg":"bad","input":"FW-1C285B2D-6CE6-4BC7-B8BE-ADB6A7E304DA"}]}"#));
    let e = l.deactivate().unwrap_err();
    let detail = e.detail().unwrap_or_default().to_owned();
    assert!(detail.contains("422") && !detail.contains(KEY), "{detail}");
}

#[test]
fn a_damaged_status_file_reads_as_unregistered() {
    let dir = Scratch::new("damaged");
    fs::create_dir_all(&dir.0).unwrap();
    fs::write(dir.0.join("license.json"), "{not json").unwrap();
    let (l, _) = licensing(&dead_base(), &dir);
    assert_eq!(l.status(), Status::Unregistered);
}

#[test]
fn pasted_keys_are_cleaned_and_checked() {
    assert_eq!(clean_key(&format!("  {KEY}\n")).as_deref(), Ok(KEY));
    assert_eq!(clean_key(&format!("“{KEY}”.")).as_deref(), Ok(KEY));
    assert_eq!(clean_key(&KEY.to_lowercase()).as_deref(), Ok(KEY.to_lowercase().as_str()));
    assert!(clean_key("").unwrap_err().contains("Paste"));
    assert!(clean_key("FW-1C28 5B2D-6CE6").unwrap_err().contains("no spaces"));
    assert!(clean_key("1C285B2D-6CE6-4BC7-B8BE-ADB6A7E304DA").unwrap_err().contains("start with FW-"));
    assert!(clean_key("FW-123").is_err());
    assert_eq!(mask_key(KEY), "FW-••••-E304DA");
    assert_eq!(mask_key("short"), "•••••");
}

#[test]
fn the_device_label_names_this_computer() {
    let label = filewinch_license::device_label();
    assert!(label.starts_with("Filewinch"), "{label}");
    assert!(!label.ends_with(".local"), "{label}");
}
