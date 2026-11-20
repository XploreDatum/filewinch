#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! Filewinch: a two-pane file transfer app for macOS, Windows and Linux (app id
//! `app.xpdt.filewinch`: the macOS bundle id, the Windows AppUserModelID, the Wayland app id).
//!
//! `cargo run -p filewinch-app` opens the window. Headless (agent tooling, no window):
//! `filewinch --snap out.png --scene local`, `--tree tree.json`, `--script s.json`,
//! `--window settings --snap settings.png`. Headless runs never touch the secret store or the
//! saved-sites files. They, the scenes and their demo data come with the `dev-tools` feature
//! (on by default; release builds leave it out: `dev_tools.rs`).

mod about;
mod actions;
mod connect;
mod delete_sheet;
mod dev_tools;
#[cfg(test)]
mod dev_tools_tests;
mod drag;
mod drag_app;
#[cfg(all(test, feature = "sftp", feature = "ftp", feature = "webdav", feature = "s3"))]
mod e2e;
mod edit_watch;
#[cfg(any(test, feature = "dev-tools"))]
mod fake;
mod fmt;
mod hub;
mod icons;
mod info;
mod inspector;
mod keys;
mod license_ui;
#[cfg(any(test, feature = "dev-tools"))]
mod live;
mod menus;
mod oauth;
mod pane;
mod registry;
#[cfg(any(test, feature = "dev-tools"))]
mod scenes;
mod settings;
mod sidebar;
mod sync_sheet;
#[cfg(test)]
mod tests;
mod theme;
mod transfers;
mod ui;
#[cfg(test)]
mod ui_os_tests;
mod urls;
mod view;
mod volumes;
mod watch;

use std::sync::atomic::{AtomicU32, Ordering};

use rune::{App, AppSpec, KeyEvent, Tx, Waker, WindowOptions, WindowSpec};

use ui::Ui;

fn init() -> Ui {
    Ui::new()
}

fn setup(s: &mut Ui, waker: Waker) {
    s.setup(waker);
}

fn poll(s: &mut Ui, now: f64, tx: &mut Tx) -> bool {
    s.poll(now, tx)
}

fn on_key(s: &mut Ui, ev: &KeyEvent, tx: &mut Tx) -> bool {
    s.window_key(ev, tx)
}

pub fn main_spec() -> AppSpec<Ui> {
    let mut spec = AppSpec {
        #[cfg(any(test, feature = "dev-tools"))]
        scenes: scenes::SCENES,
        setup: Some(setup),
        poll: Some(poll),
        on_key: Some(on_key),
        min_size: Some((960.0, 580.0)),
        ..AppSpec::new("Filewinch", (1320.0, 820.0), init, view::view)
    };
    if let Some(families) = theme::families() {
        spec.families = families;
    }
    spec
}

/// File ▸ New Window: another browser window with its own panes (transfers are shared).
pub fn open_new_window() {
    static N: AtomicU32 = AtomicU32::new(1);
    let n = N.fetch_add(1, Ordering::Relaxed);
    rune::open_window(WindowSpec::new(format!("main-{n}"), main_spec()));
}

fn main() {
    // A release build asked for --snap, --scene, --look, ... says they aren't there and exits.
    #[cfg(not(feature = "dev-tools"))]
    dev_tools::refuse_flags();
    // Before any window: `--look macos|windows|linux|kde` renders another platform's look.
    theme::init_from_args(std::env::args().skip(1));
    // One id everywhere: Wayland app_id and X11 WM_CLASS (matching the .desktop file's
    // StartupWMClass), and the Windows AppUserModelID (matching the Start Menu shortcut).
    rune::set_app_id(filewinch_os::APP_ID);
    filewinch_os::register_app_id();
    // Windows: window, taskbar and Alt+Tab icons from the .ico embedded by build.rs.
    #[cfg(windows)]
    rune::set_app_icon_resource(1);
    rune::prewarm_gpu();
    // The Windows and Linux accent readers start after the first frame, off the UI thread.
    let mut app = App::new(main_spec()).on_ready(|_| {
        theme::start_watching();
        // The licence's daily check waits for the first frame and runs off the UI thread.
        license_ui::start_checks();
        // Filewinch ▸ About opens Filewinch's own About window, not AppKit's standard panel.
        #[cfg(target_os = "macos")]
        about::hide_standard_about_item();
    });
    // Windows and Linux have no menu bar for Rune to fill: the toolbar's menu button stands in.
    if cfg!(target_os = "macos") {
        for item in menus::menu_bar() {
            app = app.menu_item(item);
        }
    }
    let hidden = WindowOptions { visible: false, ..WindowOptions::default() };
    let (settings, window) = app.window(WindowSpec::new("settings", settings::spec()).options(hidden.clone()));
    settings::install(settings, window);
    let (about_app, about_window) = app.window(WindowSpec::new("about", about::spec()).options(hidden));
    about::install(about_app, about_window);
    app.run();
}
