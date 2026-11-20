//! Filewinch's optional paid licence, sold through Polar.
//!
//! Filewinch is free to use with every feature; a licence only removes the "free to use"
//! banner. So this crate errs, always, on the side of the person who paid:
//!
//! - [`Licensing::activate`] activates a key on this device with Polar's public
//!   customer-portal endpoints (no secret is involved: the app holds no Polar token), confirms
//!   it belongs to Filewinch's benefit, then keeps the key and activation id in the system's
//!   secret store and the non-secret status ([`Registration`]) in `license.json`.
//! - [`Licensing::check`] validates in the background at most once a day
//!   ([`CHECK_EVERY`]). Network failures, timeouts, 5xx and anything unexpected leave the
//!   app registered, offline, indefinitely. Only Polar's explicit `404 ResourceNotFound` for
//!   this key and activation (revoked, disabled, or the device removed in the customer portal)
//!   returns it to unregistered: [`Check::Revoked`].
//! - [`Licensing::deactivate`] frees this device's slot.
//!
//! Human messages for every failure are in [`Failure::message`].

mod licensing;
mod polar;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use licensing::{Check, Licensing, Registration, Status};
pub use polar::{Failure, Holder, Polar};

/// Polar's API.
pub const API_BASE: &str = "https://api.polar.sh";
/// XploreDatum Technologies LLP's Polar organisation.
pub const ORGANIZATION_ID: &str = "f2434cea-855e-43e8-937e-f1cf6947bd04";
/// The "Filewinch licence key" License Keys benefit: keys of any other product don't count.
pub const BENEFIT_ID: &str = "7123bf4e-0ddc-4b48-a8bb-cf1816b48a19";
/// Where "Buy a Licence…" goes: the checkout link for the one-time product.
pub const CHECKOUT_URL: &str = "https://buy.polar.sh/polar_cl_dRKrImCCpKaozJf802qEERMHHKyTdVOxCOhMz3jOPd7";
/// Where buyers see their key and free devices.
pub const PORTAL_URL: &str = "https://polar.sh/xploredatum/portal";
/// The price shown in the app, before tax (Polar adds tax at checkout).
pub const PRICE: &str = "$14";
/// Devices one key can be active on (the benefit's activation limit).
pub const DEVICE_LIMIT: u32 = 5;
/// Every key starts with this (the benefit's prefix), then `-` and a UUID.
pub const KEY_PREFIX: &str = "FW";
/// How often a registered app re-validates, at most.
pub const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// Seconds since 1970, the clock the status file uses.
pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The pasted key without surrounding whitespace, or why it can't be a Filewinch key. A
/// quick local check so a typo doesn't need the network; Polar has the final word.
pub fn clean_key(raw: &str) -> Result<String, &'static str> {
    // Pasting from an email can bring quotes, a trailing full stop or a line break.
    let key = raw.trim().trim_matches(|c: char| matches!(c, '"' | '\'' | '“' | '”' | '.' | ',')).trim();
    if key.is_empty() {
        return Err("Paste the licence key from your receipt email.");
    }
    if key.chars().any(char::is_whitespace) {
        return Err("A licence key has no spaces. Copy it again from your receipt email.");
    }
    let prefixed = key.get(..KEY_PREFIX.len()).is_some_and(|p| p.eq_ignore_ascii_case(KEY_PREFIX));
    if !prefixed || key.len() < 20 {
        return Err("That doesn't look like a Filewinch licence key. Keys start with FW- and are about 40 characters long.");
    }
    Ok(key.to_owned())
}

/// A key shown in Settings: the prefix and last six characters, the rest hidden.
pub fn mask_key(key: &str) -> String {
    let n = key.chars().count();
    if n <= 10 {
        return "•".repeat(n);
    }
    let tail: String = key.chars().skip(n - 6).collect();
    format!("{}-••••-{tail}", KEY_PREFIX)
}

/// The activation's label in Polar's customer portal: "Filewinch on <host name>".
pub fn device_label() -> String {
    match host_name() {
        Some(h) => format!("Filewinch on {h}"),
        None => "Filewinch".to_owned(),
    }
}

/// This computer's name without a `.local` or other domain, if the OS says.
pub fn host_name() -> Option<String> {
    #[cfg(unix)]
    let raw = {
        let mut buf = [0u8; 256];
        // SAFETY: the buffer is valid for its length; gethostname NUL-terminates on success
        // (and the last byte stays 0 if it truncates).
        let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) } == 0;
        let end = buf.iter().position(|b| *b == 0).unwrap_or(0);
        ok.then(|| String::from_utf8_lossy(&buf[..end]).into_owned())
    };
    #[cfg(windows)]
    let raw = std::env::var("COMPUTERNAME").ok();
    #[cfg(not(any(unix, windows)))]
    let raw: Option<String> = None;
    let name = raw?.trim().to_owned();
    let short = name.split('.').next().unwrap_or("").trim().to_owned();
    // Polar's labels are free text; keep them short.
    let short: String = short.chars().take(60).collect();
    (!short.is_empty()).then_some(short)
}
