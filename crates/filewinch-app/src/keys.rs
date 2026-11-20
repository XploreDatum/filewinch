//! Every window shortcut in one table, per platform: ⌘ on macOS, Ctrl on Windows and Linux,
//! plus each desktop's own conventions (Alt+Left/Right/Up to navigate, F5 to refresh, Alt+Enter
//! for Properties on Windows, Ctrl+H for hidden files on Linux). The same [`Keybind`] gives the
//! window's key handling, the menus' key equivalents and the labels buttons and hints show
//! ("⌘K", "Ctrl+K"), so they never disagree.

use filewinch_os::Os;
use rune::{KeyEvent, KeyPlatform, Keybind, Shortcut, Tx};

/// Something a shortcut or a menu item does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    NewWindow,
    Connect,
    NewFolder,
    Info,
    Delete,
    Refresh,
    Hidden,
    Transfers,
    Sidebar,
    Back,
    Forward,
    Up,
    FocusLeft,
    FocusRight,
    Open,
    Settings,
    Quit,
}

impl Act {
    /// The shortcuts the window handles, in matching order (New Folder before New Window, so
    /// the chord with Shift wins).
    pub const WINDOW: [Act; 16] = [
        Act::NewFolder,
        Act::NewWindow,
        Act::Connect,
        Act::Info,
        Act::Delete,
        Act::Refresh,
        Act::Hidden,
        Act::Transfers,
        Act::Sidebar,
        Act::Back,
        Act::Forward,
        Act::Up,
        Act::FocusLeft,
        Act::FocusRight,
        Act::Settings,
        Act::Quit,
    ];

    /// The chords for this action on every platform.
    pub fn keybind(self) -> Keybind {
        use KeyPlatform::{Linux, Mac, Windows};
        match self {
            Act::NewWindow => Keybind::new("mod+n"),
            Act::Connect => Keybind::new("mod+k"),
            Act::NewFolder => Keybind::new("mod+shift+n"),
            // Alt+Enter is Properties in File Explorer; Files takes both.
            Act::Info => Keybind::new("mod+i").on(Windows, "alt+enter, mod+i").on(Linux, "mod+i, alt+enter"),
            // A bare Delete only acts in a focused pane (see `window_key`).
            Act::Delete => Keybind::new("delete").on(Mac, "mod+backspace"),
            Act::Refresh => Keybind::new("f5, mod+r").on(Mac, "mod+r"),
            Act::Hidden => Keybind::new("mod+shift+period").on(Linux, "mod+h, mod+shift+period"),
            // Ctrl+Alt+T opens a terminal on most Linux desktops.
            Act::Transfers => Keybind::new("mod+shift+t").on(Mac, "mod+alt+t"),
            Act::Sidebar => Keybind::new("f9").on(Mac, "mod+ctrl+s"),
            Act::Back => Keybind::new("alt+left").on(Mac, "mod+["),
            Act::Forward => Keybind::new("alt+right").on(Mac, "mod+]"),
            Act::Up => Keybind::new("alt+up").on(Mac, "mod+up"),
            Act::FocusLeft => Keybind::new("mod+1"),
            Act::FocusRight => Keybind::new("mod+2"),
            Act::Open => Keybind::new("enter").on(Mac, "mod+o"),
            Act::Settings => Keybind::new("mod+comma"),
            // macOS's Quit (⌘Q) lives in the system's app menu; Windows apps exit with Alt+F4,
            // which closes the window, so Exit has no chord of its own there.
            Act::Quit => Keybind::new("mod+q").on(Mac, "none").on(Windows, "none"),
        }
    }

    /// The shortcut as the platform the app words things for writes it ("⇧⌘N", "Ctrl+Shift+N");
    /// empty when unbound. Follows `--look` in snapshots (see `theme::ui_os`).
    pub fn label(self) -> String {
        self.label_on(crate::theme::key_platform())
    }

    pub fn label_on(self, platform: KeyPlatform) -> String {
        self.keybind().label_on(platform)
    }

    /// The first chord as a menu key equivalent.
    pub fn shortcut(self) -> Option<Shortcut> {
        let platform = KeyPlatform::current();
        let chord = self.keybind().chords_on(platform).first()?.to_string();
        Shortcut::parse_on(&chord, platform).ok()
    }

    /// The menu title, where platforms name it differently (follows `theme::ui_os`).
    pub fn title(self) -> &'static str {
        title_on(self, crate::theme::ui_os())
    }

    /// The action `ev` asks for, if it's one of the window's shortcuts.
    pub fn for_key(ev: &KeyEvent, tx: &Tx) -> Option<Act> {
        Act::WINDOW.into_iter().find(|a| a.keybind().matches_tx(ev, tx))
    }
}

/// Menu titles per OS: macOS says Settings… and has Quit in the app menu; Windows says Settings
/// and Exit; GNOME and KDE say Preferences and Quit.
pub fn title_on(a: Act, os: Os) -> &'static str {
    match (a, os) {
        (Act::Settings, Os::Mac) => "Settings…",
        (Act::Settings, Os::Windows) => "Settings",
        (Act::Settings, Os::Linux) => "Preferences",
        (Act::Quit, Os::Mac) => "Quit Filewinch",
        (Act::Quit, Os::Windows) => "Exit",
        (Act::Quit, Os::Linux) => "Quit",
        (Act::NewWindow, _) => "New Window",
        (Act::Connect, _) => "Connect…",
        (Act::NewFolder, _) => "New Folder",
        (Act::Info, Os::Windows) => "Properties",
        (Act::Info, _) => "Get Info",
        (Act::Delete, _) => "Delete…",
        (Act::Refresh, _) => "Refresh",
        (Act::Hidden, _) => "Show Hidden Files",
        (Act::Transfers, _) => "Show Transfers",
        (Act::Sidebar, _) => "Show Sidebar",
        (Act::Back, _) => "Back",
        (Act::Forward, _) => "Forward",
        (Act::Up, Os::Mac) => "Enclosing Folder",
        (Act::Up, _) => "Up One Level",
        (Act::FocusLeft, _) => "Left Pane",
        (Act::FocusRight, _) => "Right Pane",
        (Act::Open, _) => "Open",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::{Key, Mods};

    #[test]
    fn every_binding_parses_on_every_platform() {
        for p in [KeyPlatform::Mac, KeyPlatform::Windows, KeyPlatform::Linux] {
            for a in Act::WINDOW.into_iter().chain([Act::Open]) {
                let kb = a.keybind();
                for c in kb.chords_on(p) {
                    assert!(Shortcut::parse_on(&c.to_string(), p).is_ok(), "{a:?} {c} on {p:?}");
                }
            }
        }
    }

    #[test]
    fn labels_per_platform() {
        use KeyPlatform::{Linux, Mac, Windows};
        assert_eq!(Act::Connect.label_on(Mac), "⌘K");
        assert_eq!(Act::Connect.label_on(Windows), "Ctrl+K");
        assert_eq!(Act::Connect.label_on(Linux), "Ctrl+K");
        assert_eq!(Act::NewFolder.label_on(Mac), "⇧⌘N");
        assert_eq!(Act::NewFolder.label_on(Windows), "Ctrl+Shift+N");
        assert_eq!(Act::Delete.label_on(Mac), "⌘⌫");
        assert_eq!(Act::Delete.label_on(Windows), "Delete");
        assert_eq!(Act::Refresh.label_on(Windows), "F5");
        assert_eq!(Act::Info.label_on(Windows), "Alt+Enter");
        assert_eq!(Act::Info.label_on(Linux), "Ctrl+I");
        assert_eq!(Act::Hidden.label_on(Linux), "Ctrl+H");
        assert_eq!(Act::Back.label_on(Windows), "Alt+←");
        assert_eq!(Act::Up.label_on(Linux), "Alt+↑");
        assert_eq!(Act::Sidebar.label_on(Mac), "⌃⌘S");
        assert_eq!(Act::Transfers.label_on(Mac), "⌥⌘T");
        assert_eq!(Act::Settings.label_on(Mac), "⌘,");
        assert_eq!(Act::Settings.label_on(Linux), "Ctrl+,");
        assert_eq!(Act::Quit.label_on(Linux), "Ctrl+Q");
        assert_eq!(Act::Quit.label_on(Windows), "");
        assert_eq!(Act::Quit.label_on(Mac), "");
    }

    #[test]
    fn titles_per_os() {
        assert_eq!(title_on(Act::Settings, Os::Mac), "Settings…");
        assert_eq!(title_on(Act::Settings, Os::Windows), "Settings");
        assert_eq!(title_on(Act::Settings, Os::Linux), "Preferences");
        assert_eq!(title_on(Act::Quit, Os::Windows), "Exit");
        assert_eq!(title_on(Act::Quit, Os::Linux), "Quit");
        assert_eq!(title_on(Act::Info, Os::Windows), "Properties");
        assert_eq!(title_on(Act::Up, Os::Mac), "Enclosing Folder");
    }

    #[test]
    fn chords_match_the_platforms_keys() {
        // A bare `cmd` is the platform's primary key (⌘ on macOS, Ctrl elsewhere), as in
        // Rune's own scripts; naming the physical key would follow the build's OS instead.
        let cmd = Mods { cmd: true, ..Mods::default() };
        let ctrl = cmd;
        let ev = |key, mods| KeyEvent { key, mods };
        assert!(Act::Connect.keybind().matches_on(&ev(Key::Command('k'), cmd), KeyPlatform::Mac));
        assert!(Act::Connect.keybind().matches_on(&ev(Key::Command('k'), ctrl), KeyPlatform::Windows));
        // ⌘K's Super is not Ctrl off macOS.
        let sup = Mods { logo: true, ..Mods::default() };
        assert!(!Act::Connect.keybind().matches_on(&ev(Key::Text("k".into()), sup), KeyPlatform::Linux));
        let shift_ctrl = Mods { shift: true, ..ctrl };
        assert!(Act::NewFolder.keybind().matches_on(&ev(Key::Command('n'), shift_ctrl), KeyPlatform::Windows));
        assert!(!Act::NewWindow.keybind().matches_on(&ev(Key::Command('n'), shift_ctrl), KeyPlatform::Windows));
        let alt = Mods { alt: true, ..Mods::default() };
        assert!(Act::Back.keybind().matches_on(&ev(Key::Left, alt), KeyPlatform::Windows));
        assert!(Act::Refresh.keybind().matches_on(&ev(Key::Function(5), Mods::default()), KeyPlatform::Linux));
    }
}
