//! The desktop's accent colour where Rune doesn't read it yet: Windows (`UISettings`, then the
//! DWM registry value, then Fluent's default blue) and Linux (the XDG desktop portal's
//! `org.freedesktop.appearance` `accent-color`). macOS needs nothing here: Rune's
//! `system_accent` already follows it.
//!
//! [`start`] spawns one reader thread per process, which reads the colour once and then follows
//! the platform's change notification, calling the app back on each change. Nothing here blocks
//! the caller: until the first read lands, [`current`] is `None` and the app draws its look's
//! default accent. The readers compile only on their own platform; the fallback chains are pure
//! functions, tested on every host.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(test)]
mod tests;
#[cfg(target_os = "windows")]
mod windows;

/// An sRGB colour, 8 bits a channel.
pub type Rgb = [u8; 3];

/// The desktop's accent as the platform states it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Accent {
    pub base: Rgb,
    /// Windows' `AccentLight2`, the accent fill on dark backgrounds. `None` when only the base
    /// colour is known (the DWM registry value, the portal); the app derives it.
    pub light2: Option<Rgb>,
    /// Windows' `AccentDark1`, the accent fill on light backgrounds.
    pub dark1: Option<Rgb>,
}

/// Windows 11's default accent and the shades WinUI's default theme resources use.
pub const FLUENT_DEFAULT: Accent =
    Accent { base: [0x00, 0x78, 0xd4], light2: Some([0x60, 0xcd, 0xff]), dark1: Some([0x00, 0x5f, 0xb8]) };

/// Windows' fallback chain: `UISettings`' three colours when it answers, else the DWM
/// `AccentColor` registry value (`0xAABBGGRR`), else Fluent's default.
pub fn windows_accent(ui: Option<[Rgb; 3]>, dwm: Option<u32>) -> Accent {
    if let Some([base, light2, dark1]) = ui {
        return Accent { base, light2: Some(light2), dark1: Some(dark1) };
    }
    match dwm {
        Some(abgr) => Accent { base: dwm_rgb(abgr), light2: None, dark1: None },
        None => FLUENT_DEFAULT,
    }
}

/// The DWM registry's `AccentColor` DWORD, `0xAABBGGRR`, as sRGB.
pub fn dwm_rgb(abgr: u32) -> Rgb {
    [abgr as u8, (abgr >> 8) as u8, (abgr >> 16) as u8]
}

/// The portal's `accent-color`, an `(ddd)` of sRGB channels in 0–1. A channel out of range
/// (or not a number) means the user picked no accent.
pub fn portal_accent(r: f64, g: f64, b: f64) -> Option<Rgb> {
    let ch = |v: f64| (0.0..=1.0).contains(&v).then(|| (v * 255.0).round() as u8);
    Some([ch(r)?, ch(g)?, ch(b)?])
}

/// Whether `XDG_CURRENT_DESKTOP` (a colon-separated list, e.g. `KDE` or `ubuntu:GNOME`) names KDE.
pub fn is_kde(xdg_current_desktop: &str) -> bool {
    xdg_current_desktop.split(':').any(|d| d.trim().eq_ignore_ascii_case("kde"))
}

static CURRENT: Mutex<Option<Accent>> = Mutex::new(None);
static VERSION: AtomicU64 = AtomicU64::new(0);
type Callback = Box<dyn Fn() + Send + Sync>;
static ON_CHANGE: OnceLock<Callback> = OnceLock::new();

/// The latest accent the platform reported; `None` before the first read, when the platform
/// has no reader here (macOS), or when the desktop has no accent setting.
pub fn current() -> Option<Accent> {
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner())
}

/// Bumped on every change [`current`] reports, for caches keyed on the accent.
pub fn version() -> u64 {
    VERSION.load(Ordering::Relaxed)
}

/// Starts the platform's reader (once per process; later calls do nothing). `on_change` runs on
/// the reader's thread after each change, so it must only hand work to the UI (wake it), never
/// draw. Returns at once.
pub fn start(on_change: impl Fn() + Send + Sync + 'static) {
    let first = ON_CHANGE.set(Box::new(on_change)).is_ok();
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    let _ = first;
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    if first {
        #[cfg(target_os = "windows")]
        let run = windows::run;
        #[cfg(target_os = "linux")]
        let run = linux::run;
        // A failed spawn leaves the look's default accent, which is what the app draws anyway.
        let _ = std::thread::Builder::new().name("filewinch-accent".into()).spawn(run);
    }
}

/// Records a read; calls the app back when it differs from the last one.
#[cfg_attr(not(any(target_os = "windows", target_os = "linux", test)), allow(dead_code))]
fn set(accent: Option<Accent>) {
    {
        let mut current = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
        if *current == accent {
            return;
        }
        *current = accent;
        VERSION.fetch_add(1, Ordering::Relaxed);
    }
    if let Some(f) = ON_CHANGE.get() {
        f();
    }
}
