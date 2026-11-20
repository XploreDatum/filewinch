//! Filewinch's per-OS glue, kept pure where it can be so every OS's rules are tested on any OS:
//!
//! - [`dirs`]: where settings, sites, known hosts and caches live (from an injected environment).
//! - [`names`]: which file names Windows refuses, and the substitution downloads use for them.
//! - [`launch`]: opening a URL in the browser, revealing a file, and opening a file in the
//!   default app or the person's external editor.

pub mod dirs;
pub mod launch;
pub mod names;
#[cfg(test)]
mod tests;

/// The desktop OS a rule is for. [`Os::CURRENT`] is the one this build runs on; tests pass
/// the others explicitly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Os {
    Mac,
    Windows,
    /// Linux and other freedesktop systems (XDG folders, `xdg-open`).
    Linux,
}

impl Os {
    #[cfg(target_os = "macos")]
    pub const CURRENT: Os = Os::Mac;
    #[cfg(windows)]
    pub const CURRENT: Os = Os::Windows;
    #[cfg(not(any(target_os = "macos", windows)))]
    pub const CURRENT: Os = Os::Linux;

    /// What the app calls this computer in pane titles and menus ("Download to This Mac").
    pub fn this_computer(self) -> &'static str {
        match self {
            Os::Mac => "This Mac",
            Os::Windows | Os::Linux => "This Computer",
        }
    }

    /// Where saved passwords go, for hints like "Stored in your Keychain".
    pub fn secret_store(self) -> &'static str {
        match self {
            Os::Mac => "your Keychain",
            Os::Windows => "Windows Credential Manager",
            Os::Linux => "your keyring",
        }
    }

    /// The file manager, for "Show in Finder" and messages.
    pub fn file_manager(self) -> &'static str {
        match self {
            Os::Mac => "Finder",
            Os::Windows => "File Explorer",
            Os::Linux => "Files",
        }
    }
}

/// The app id on every OS: macOS bundle identifier, Wayland app id / X11 WM class,
/// Windows AppUserModelID, and the secret store's service name.
pub const APP_ID: &str = "app.xpdt.filewinch";

/// Tells Windows this process is [`APP_ID`], so the taskbar groups its windows under the
/// Start Menu shortcut's icon. Elsewhere does nothing (Rune sets the Wayland app id).
pub fn register_app_id() {
    #[cfg(windows)]
    {
        let wide: Vec<u16> = APP_ID.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        unsafe {
            windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(wide.as_ptr());
        }
    }
}
