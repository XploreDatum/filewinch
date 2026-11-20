//! "Sign in with …" for cloud drives: the browser flow lives in the cloud backend crate; this
//! file is the app's one link to it.

use filewinch_core::{Error, ErrorKind, Protocol, Result};

/// Whether this build can sign in to `p` through the browser.
pub fn sign_in_available(p: Protocol) -> bool {
    p.uses_oauth() && cfg!(feature = "cloud") && crate::registry::available(p)
}

#[cfg(feature = "cloud")]
static CANCEL: std::sync::Mutex<Option<filewinch_cloud::CancelToken>> = std::sync::Mutex::new(None);

/// Runs the browser sign-in (blocking; call off the UI thread). Returns the refresh token and
/// the account's display name.
pub fn sign_in(p: Protocol) -> Result<(String, String)> {
    #[cfg(feature = "cloud")]
    {
        let provider = filewinch_cloud::Provider::ALL
            .into_iter()
            .find(|x| x.protocol() == p)
            .ok_or_else(|| Error::new(ErrorKind::Unsupported, format!("{} doesn't sign in through the browser.", p.label())))?;
        let client = filewinch_cloud::ClientConfig::load(provider)?;
        let cancel = filewinch_cloud::CancelToken::new();
        if let Ok(mut c) = CANCEL.lock() {
            *c = Some(cancel.clone());
        }
        let signed = filewinch_cloud::sign_in(provider, &client, cancel)?;
        Ok((signed.refresh_token, signed.account_label))
    }
    #[cfg(not(feature = "cloud"))]
    {
        Err(Error::new(ErrorKind::Unsupported, format!("Signing in to {} isn't available in this build yet.", p.label())))
    }
}

/// Stops a sign-in waiting on the browser (the sheet closed).
pub fn cancel_sign_in() {
    #[cfg(feature = "cloud")]
    if let Ok(mut c) = CANCEL.lock()
        && let Some(t) = c.take()
    {
        t.cancel();
    }
}
