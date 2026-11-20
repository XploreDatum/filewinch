//! What a `keyring` failure means for the person: an [`ErrorKind`] and one sentence naming
//! this OS's secret store. The technical text goes in `detail`; a secret never does
//! (`keyring::Error`'s `Display` doesn't include one).

use std::error::Error as StdError;

use filewinch_core::{Error, ErrorKind};
use filewinch_os::Os;

/// Shown on Linux when no Secret Service answers on the session bus (headless boxes, minimal
/// window managers). Filewinch never falls back to keeping passwords in a file.
pub const NO_SECRET_SERVICE: &str = "No password store is running. Install and unlock GNOME Keyring or KWallet.";

/// What Filewinch was doing with the secret store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Read,
    Save,
    Delete,
}

impl Op {
    fn phrase(self) -> &'static str {
        match self {
            Op::Read => "read a saved password",
            Op::Save => "save the password",
            Op::Delete => "delete the saved password",
        }
    }
}

/// What a platform failure inside a `keyring::Error` comes down to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// Nothing provides the store (Linux: no D-Bus session or no `org.freedesktop.secrets`).
    NoService,
    /// The person dismissed the unlock prompt.
    Cancelled,
    /// The store is locked or refused access.
    Denied,
    Unknown,
}

/// `e` as Filewinch's error for `op` on `os`. `NoEntry` is handled by callers (a missing
/// secret isn't an error for get or delete); here it becomes `NotFound`.
pub fn to_error(e: &keyring::Error, op: Op, os: Os) -> Error {
    let store = os.secret_store();
    let store_cap = capitalize(store);
    let doing = op.phrase();
    let (kind, message) = match e {
        keyring::Error::NoEntry => (ErrorKind::NotFound, format!("There's no saved password in {store}.")),
        keyring::Error::BadEncoding(_) => (ErrorKind::Other, format!("A saved password in {store} isn't valid text.")),
        keyring::Error::TooLong(attr, limit) if attr == "password" || attr == "secret" => (
            ErrorKind::Other,
            format!("This password or token is too long to save in {store} (the limit is {limit} bytes)."),
        ),
        keyring::Error::TooLong(attr, limit) => {
            (ErrorKind::Other, format!("{store_cap} couldn't {doing}: its {attr} is longer than the limit of {limit}."))
        }
        keyring::Error::Ambiguous(_) => (
            ErrorKind::Other,
            format!("There's more than one saved password for this site in {store}. Enter the password again to replace them."),
        ),
        keyring::Error::PlatformFailure(p) | keyring::Error::NoStorageAccess(p) => {
            let cause = match cause(&**p, os) {
                Cause::Unknown if matches!(e, keyring::Error::NoStorageAccess(_)) => Cause::Denied,
                c => c,
            };
            match cause {
                Cause::NoService if os == Os::Linux => (ErrorKind::Unsupported, NO_SECRET_SERVICE.to_owned()),
                Cause::NoService => (ErrorKind::Unsupported, format!("{store_cap} isn't available, so Filewinch couldn't {doing}.")),
                Cause::Cancelled => (ErrorKind::Cancelled, format!("Unlocking {store} was cancelled, so Filewinch couldn't {doing}.")),
                Cause::Denied => (ErrorKind::PermissionDenied, format!("{store_cap} is locked or didn't allow Filewinch to {doing}.")),
                Cause::Unknown => (ErrorKind::Other, format!("{store_cap} couldn't {doing}.")),
            }
        }
        _ => (ErrorKind::Other, format!("{store_cap} couldn't {doing}.")),
    };
    Error::new(kind, message).with_detail(e)
}

/// The platform error's meaning: the OS-specific code first, then (on Linux) the D-Bus error
/// names anywhere in its chain.
pub fn cause(p: &(dyn StdError + 'static), os: Os) -> Cause {
    match platform_cause(p) {
        Cause::Unknown if os == Os::Linux && chain_means_no_service(p) => Cause::NoService,
        c => c,
    }
}

/// True when the error chain says no Secret Service is reachable: no session bus, nothing
/// owning `org.freedesktop.secrets`, or its activation failed.
pub fn chain_means_no_service(p: &(dyn StdError + 'static)) -> bool {
    let mut next = Some(p);
    while let Some(e) = next {
        if let Some(io) = e.downcast_ref::<std::io::Error>()
            && matches!(io.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused)
        {
            return true;
        }
        if text_means_no_service(&e.to_string()) {
            return true;
        }
        next = e.source();
    }
    false
}

/// D-Bus error names (they're the protocol's API, so stable) and the `secret-service` crate's
/// own "unavailable" text.
pub fn text_means_no_service(text: &str) -> bool {
    [
        "org.freedesktop.DBus.Error.ServiceUnknown",
        "org.freedesktop.DBus.Error.NameHasNoOwner",
        "org.freedesktop.DBus.Error.Spawn.",
        "org.freedesktop.DBus.Error.NoServer",
        "no secret service provider or dbus session found",
    ]
    .iter()
    .any(|name| text.contains(name))
}

#[cfg(target_os = "macos")]
fn platform_cause(p: &(dyn StdError + 'static)) -> Cause {
    match p.downcast_ref::<security_framework::base::Error>().map(|e| e.code()) {
        // errSecUserCanceled
        Some(-128) => Cause::Cancelled,
        // errSecAuthFailed, errSecInteractionNotAllowed (locked, no UI), errSecNotAvailable
        Some(-25293 | -25308 | -25291) => Cause::Denied,
        _ => Cause::Unknown,
    }
}

#[cfg(windows)]
fn platform_cause(p: &(dyn StdError + 'static)) -> Cause {
    match p.downcast_ref::<keyring::windows::Error>().map(|e| e.0) {
        // ERROR_CANCELLED
        Some(1223) => Cause::Cancelled,
        // ERROR_ACCESS_DENIED
        Some(5) => Cause::Denied,
        // ERROR_NO_SUCH_LOGON_SESSION: no credential set for this sign-in (services, some RDP).
        Some(1312) => Cause::NoService,
        _ => Cause::Unknown,
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd"))]
fn platform_cause(p: &(dyn StdError + 'static)) -> Cause {
    use secret_service::Error as Ss;
    match p.downcast_ref::<Ss>() {
        Some(Ss::Unavailable) => Cause::NoService,
        Some(Ss::Prompt) => Cause::Cancelled,
        Some(Ss::Locked) => Cause::Denied,
        _ => Cause::Unknown,
    }
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux", target_os = "freebsd", target_os = "openbsd")))]
fn platform_cause(_: &(dyn StdError + 'static)) -> Cause {
    Cause::Unknown
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
}
