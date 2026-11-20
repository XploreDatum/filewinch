//! The OS the app words things for (`theme::ui_os`): `--look` makes snapshots of the Windows and
//! Linux looks say what those platforms say; without it the build's own OS decides.

use filewinch_os::Os;
use rune::{Env, KeyPlatform, Runtime};

use crate::keys::Act;
use crate::theme::{self, Look, set_test_ui_os, ui_os};
use crate::ui::Ui;

/// The main window drawn as `os` words it: the empty right pane and an empty Transfers drawer.
fn window_text(os: Option<Os>) -> Runtime<Ui> {
    set_test_ui_os(os);
    let mut ui = Ui::new();
    ui.prefs.transfers_open = true;
    ui.transfers.rows.clear();
    let mut rt = Runtime::new(ui, crate::view::view, (1320.0, 820.0), Env::new(false, 1.0));
    rt.frame(0.0);
    rt
}

fn text_of(rt: &Runtime<Ui>, key: &str) -> String {
    let els = rt.elements();
    let e = els.iter().find(|e| e.key.as_deref() == Some(key)).unwrap_or_else(|| panic!("{key} is on screen"));
    e.label.clone().or(e.text.clone()).unwrap_or_default()
}

#[test]
fn each_look_words_things_for_its_platform() {
    assert_eq!(Look::MacOs.os(), Os::Mac);
    assert_eq!(Look::Windows.os(), Os::Windows);
    assert_eq!(Look::Gnome.os(), Os::Linux);
    assert_eq!(Look::Kde.os(), Os::Linux);
    // Absent `--look`, the native look's OS is the build's own, KDE or not.
    assert_eq!(Look::native(std::env::consts::OS, None).os(), Os::CURRENT);
    assert_eq!(Look::native(std::env::consts::OS, Some("KDE")).os(), Os::CURRENT);
}

#[test]
fn without_an_override_the_build_os_wins() {
    set_test_ui_os(None);
    assert_eq!(ui_os(), Os::CURRENT);
    assert_eq!(theme::key_platform(), KeyPlatform::current());
    assert_eq!(Act::Connect.label(), Act::Connect.label_on(KeyPlatform::current()));
    assert_eq!(Act::Settings.title(), crate::keys::title_on(Act::Settings, Os::CURRENT));
    assert_eq!(crate::settings::spec().title, crate::settings::window_title(Os::CURRENT));
    let rt = window_text(None);
    assert!(text_of(&rt, "empty-local-1").contains(Os::CURRENT.this_computer()));
    assert!(text_of(&rt, "transfers-empty").contains(Os::CURRENT.file_manager()));
    assert_eq!(rt.find("tb-menu").is_some(), Os::CURRENT != Os::Mac, "the toolbar menu stands in for a menu bar off macOS");
}

#[test]
fn a_windows_look_says_this_computer_file_explorer_and_ctrl() {
    let rt = window_text(Some(Os::Windows));
    assert_eq!(text_of(&rt, "empty-local-1"), "This Computer");
    let hint = text_of(&rt, "transfers-empty");
    assert!(hint.contains("File Explorer") && !hint.contains("Finder"), "{hint}");
    assert!(rt.find("tb-menu").is_some(), "Windows has no menu bar: the toolbar's menu stands in");
    assert_eq!(crate::settings::spec().title, "Filewinch Settings");
    assert_eq!(Act::Settings.title(), "Settings");
    assert_eq!(Act::Quit.title(), "Exit");
    assert_eq!(Act::Connect.label(), "Ctrl+K");
    assert_eq!(Act::NewFolder.label(), "Ctrl+Shift+N");
    let help = Act::Connect.label();
    assert!(!help.contains('⌘'));
    set_test_ui_os(None);
}

#[test]
fn a_linux_look_says_this_computer_files_preferences_and_ctrl() {
    let rt = window_text(Some(Os::Linux));
    assert_eq!(text_of(&rt, "empty-local-1"), "This Computer");
    let hint = text_of(&rt, "transfers-empty");
    assert!(hint.contains("in from Files,"), "{hint}");
    assert!(rt.find("tb-menu").is_some());
    assert_eq!(crate::settings::spec().title, "Filewinch Preferences");
    assert_eq!(Act::Settings.title(), "Preferences");
    assert_eq!(Act::Connect.label(), "Ctrl+K");
    assert_eq!(Act::Hidden.label(), "Ctrl+H");
    set_test_ui_os(None);
}

#[test]
fn a_mac_look_says_this_mac_finder_and_command() {
    let rt = window_text(Some(Os::Mac));
    assert_eq!(text_of(&rt, "empty-local-1"), "This Mac");
    assert!(text_of(&rt, "transfers-empty").contains("Finder"));
    assert!(rt.find("tb-menu").is_none());
    assert_eq!(crate::settings::spec().title, "Filewinch Settings");
    assert_eq!(Act::Connect.label(), "⌘K");
    set_test_ui_os(None);
}
