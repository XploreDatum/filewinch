//! The Inspector: following the selection, summaries, latest-wins previews, folder sizes,
//! staged permissions, preferences and the keyboard.

use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use filewinch_core::{Caps, Entry, ErrorKind, Protocol, RPath, Remote, Result, WriteSink};
use rune::{Env, Key, KeyEvent, Mods, Runtime};

use super::load::{self, Cancel, Fetched, Tally};
use super::perms::{self, Staged, Tri};
use super::{Preview, Size};
use crate::fake::FakeRemote;
use crate::pane::Source;
use crate::ui::{Session, Ui};

/// A window with the fake web server in the right pane, active, and the Inspector open.
fn with_web() -> Ui {
    with_remote(Arc::new(FakeRemote::web_server()))
}

fn with_remote(remote: Arc<dyn Remote>) -> Ui {
    let mut ui = Ui::new();
    let mut site = filewinch_core::Site::new("w", "web", Protocol::Sftp);
    site.host = "web1.example.com".into();
    site.user = "deploy".into();
    ui.sessions.push(Session { id: 9, site, remote: remote.clone() });
    ui.panes[1].switch_to(Source { session: 9, label: "web".into(), protocol: Protocol::Sftp, remote }, RPath::new("/var/www/site"), false);
    ui.active = 1;
    ui.prefs.inspector_open = true;
    ui
}

fn select(ui: &mut Ui, names: &[&str]) {
    let pane = &mut ui.panes[1];
    pane.sel.names = names.iter().map(|n| (*n).to_owned()).collect();
    ui.inspector_follow();
}

fn drain(ui: &mut Ui) {
    for _ in 0..4 {
        for f in ui.inbox.drain() {
            f(ui);
        }
    }
}

/// Polls (landing finished work) until `done`, for up to 10 s.
fn until(ui: &mut Ui, mut done: impl FnMut(&Ui) -> bool) -> bool {
    let end = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < end {
        ui.inspector_follow();
        ui.inspector_land();
        drain(ui);
        if done(ui) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

/// A remote whose reads and listings of gated paths wait until `open` is called, counting them.
struct Gated {
    inner: FakeRemote,
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
    gated: Vec<RPath>,
    reads: Arc<AtomicUsize>,
}

impl Gated {
    fn new(inner: FakeRemote, gated: &[&str]) -> Gated {
        Gated { inner, gate: Arc::default(), gated: gated.iter().map(|p| RPath::new(p)).collect(), reads: Arc::default() }
    }
    fn wait(&self, path: &RPath) {
        if self.gated.contains(path) {
            let (m, cv) = &*self.gate;
            let mut open = m.lock().unwrap();
            while !*open {
                open = cv.wait(open).unwrap();
            }
        }
    }
    fn open(gate: &Arc<(Mutex<bool>, std::sync::Condvar)>) {
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
    }
}

impl Remote for Gated {
    fn caps(&self) -> Caps {
        self.inner.caps()
    }
    fn home(&self) -> Result<RPath> {
        self.inner.home()
    }
    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        self.wait(dir);
        self.inner.list(dir)
    }
    fn stat(&self, path: &RPath) -> Result<Entry> {
        self.inner.stat(path)
    }
    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.wait(path);
        self.inner.read(path, offset)
    }
    fn write(&self, path: &RPath, offset: u64, size: Option<u64>) -> Result<Box<dyn WriteSink>> {
        self.inner.write(path, offset, size)
    }
    fn mkdir(&self, path: &RPath) -> Result<()> {
        self.inner.mkdir(path)
    }
    fn rename(&self, from: &RPath, to: &RPath) -> Result<()> {
        self.inner.rename(from, to)
    }
    fn remove_file(&self, path: &RPath) -> Result<()> {
        self.inner.remove_file(path)
    }
    fn remove_dir(&self, path: &RPath) -> Result<()> {
        self.inner.remove_dir(path)
    }
    fn set_mode(&self, path: &RPath, mode: u32) -> Result<()> {
        self.inner.set_mode(path, mode)
    }
}

fn put(remote: &dyn Remote, path: &str, body: &str) {
    let mut w = remote.write(&RPath::new(path), 0, Some(body.len() as u64)).unwrap();
    std::io::Write::write_all(&mut w, body.as_bytes()).unwrap();
    w.finish().unwrap();
}

#[test]
fn the_panel_follows_the_selection() {
    let mut ui = with_web();
    ui.inspector_follow();
    let s = ui.inspector.subject.as_ref().expect("the folder itself");
    assert!(s.items.is_empty());
    assert_eq!(ui.inspector.folder().map(|f| f.path.clone()), Some(RPath::new("/var/www/site")));
    select(&mut ui, &["robots.txt"]);
    let s = ui.inspector.subject.as_ref().unwrap();
    assert_eq!(s.items.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["robots.txt"]);
    assert!(matches!(&ui.inspector.preview, Preview::Text { text, .. } if text.starts_with('x')), "a text file's first bytes");
    select(&mut ui, &["css"]);
    assert_eq!(ui.inspector.subject.as_ref().unwrap().items[0].name, "css");
    assert!(matches!(ui.inspector.preview, Preview::None), "no preview for a folder");
    assert!(matches!(ui.inspector.size, Size::NotAsked), "remote folders are never sized unasked");
    // A new listing of the same selection updates the copy without resetting staged edits.
    ui.toggle_perm_bit(0o002);
    let hidden = ui.prefs.show_hidden;
    ui.panes[1].refresh(hidden);
    ui.inspector_follow();
    assert!(ui.inspector.dirty(), "staged changes survive a refresh");
}

#[test]
fn closing_the_panel_stops_following() {
    let mut ui = with_web();
    select(&mut ui, &["robots.txt"]);
    ui.toggle_inspector();
    assert!(!ui.prefs.inspector_open);
    assert!(ui.inspector.subject.is_none());
    ui.toggle_inspector();
    assert!(ui.inspector.subject.as_ref().is_some_and(|s| s.items.len() == 1), "opening again shows the selection");
}

#[test]
fn several_items_are_summed_by_kind() {
    let mut ui = with_web();
    select(&mut ui, &["index.html", "about.html", "robots.txt", "css", "img"]);
    let s = ui.inspector.subject.clone().unwrap();
    assert_eq!(s.items.len(), 5);
    let kinds = super::view::kinds(&s.items);
    assert_eq!(kinds[0], ("Folder".to_owned(), 2));
    assert_eq!(kinds[1], ("HTML source".to_owned(), 2));
    let text = super::summary(&s, None, &ui.inspector.size, crate::fmt::now());
    assert!(text.starts_with("5 items\nFiles: "), "{text}");
    assert!(text.contains("/var/www/site/css"));
    ui.calculate_size(false);
    // css/site.css; img holds hero.jpg, logo.svg and ten team photos.
    assert!(matches!(ui.inspector.size, Size::Done { files: 13, folders: 0, bytes } if bytes == 34_210 + 604_330 + 6_204 + 10 * 60_000));
    assert!(ui.inspector.modes().is_some(), "bulk permissions: every item has a mode");
}

#[test]
fn rapid_selection_changes_drop_stale_previews() {
    let fake = FakeRemote::web_server();
    put(&fake, "/var/www/site/a.txt", "first file");
    put(&fake, "/var/www/site/b.txt", "second file");
    let remote = Gated::new(fake, &["/var/www/site/a.txt"]);
    let (gate, reads) = (remote.gate.clone(), remote.reads.clone());
    let mut ui = with_remote(Arc::new(remote));
    ui.inspector.sync_io = false;
    select(&mut ui, &["a.txt"]);
    assert!(matches!(ui.inspector.preview, Preview::Loading { .. }));
    let a_cancel = ui.inspector.preview_cancel.clone();
    assert!(until(&mut ui, |_| reads.load(Ordering::SeqCst) >= 1), "a.txt's read started");
    // Quick changes: c is never read (superseded before it starts or cancelled), b wins.
    select(&mut ui, &["robots.txt"]);
    select(&mut ui, &["b.txt"]);
    assert!(a_cancel.is_cancelled(), "the old preview's download was told to stop");
    assert!(until(&mut ui, |ui| matches!(&ui.inspector.preview, Preview::Text { text, .. } if text == "second file")));
    Gated::open(&gate);
    std::thread::sleep(Duration::from_millis(100));
    until(&mut ui, |_| true);
    assert!(matches!(&ui.inspector.preview, Preview::Text { text, .. } if text == "second file"), "a.txt's late answer never lands");
}

#[test]
fn folder_sizes_calculate_and_cancel() {
    let remote = Gated::new(FakeRemote::web_server(), &["/var/www/site/img"]);
    let gate = remote.gate.clone();
    let mut ui = with_remote(Arc::new(remote));
    ui.inspector.sync_io = false;
    select(&mut ui, &["img"]);
    ui.calculate_size(false);
    assert!(matches!(ui.inspector.size, Size::Counting(_)));
    ui.cancel_size();
    assert!(matches!(ui.inspector.size, Size::NotAsked));
    assert!(ui.inspector.size_cancel.is_cancelled());
    Gated::open(&gate);
    std::thread::sleep(Duration::from_millis(100));
    until(&mut ui, |_| true);
    assert!(matches!(ui.inspector.size, Size::NotAsked), "a cancelled count never lands");
    ui.calculate_size(false);
    assert!(until(&mut ui, |ui| matches!(ui.inspector.size, Size::Done { files: 12, .. })));
}

#[test]
fn folder_walks_stop_when_cancelled_and_cap_automatic_counts() {
    let fake = FakeRemote::web_server();
    let (cancel, tally) = (Cancel::default(), Tally::default());
    cancel.cancel();
    let err = load::folder_size(&fake, &[RPath::new("/var/www")], &cancel, &tally, None).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    let capped = load::folder_size(&fake, &[RPath::new("/var/www")], &Cancel::default(), &Tally::default(), Some(3)).unwrap();
    assert_eq!(capped, load::Sized::TooMany);
}

#[test]
fn text_previews_stop_at_64_kb_and_refuse_binaries() {
    let mut long = "é".repeat(load::TEXT_MAX); // two bytes each: cut mid-character at the limit
    long.push('z');
    match load::as_text(long.as_bytes(), Some(long.len() as u64)) {
        Fetched::Text { text, truncated } => {
            assert!(truncated);
            assert!(text.len() <= load::TEXT_MAX && text.chars().all(|c| c == 'é'));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(load::as_text(b"PK\x03\x04\0\0binary", None), Fetched::Binary);
    assert_eq!(load::as_text(b"short", Some(5)), Fetched::Text { text: "short".into(), truncated: false });
}

#[test]
fn big_images_wait_for_download_to_preview() {
    let fake = FakeRemote::web_server();
    put(&fake, "/var/www/site/huge.png", "");
    let mut ui = with_remote(Arc::new(fake));
    select(&mut ui, &["huge.png"]);
    // An empty PNG isn't decodable, but it's small: it's fetched and fails to decode.
    assert!(matches!(ui.inspector.preview, Preview::Image(_)));
    if let Some(s) = &mut ui.inspector.subject {
        s.items[0].size = Some(load::PREVIEW_MAX + 1);
    }
    ui.inspector_preview(false);
    assert!(matches!(ui.inspector.preview, Preview::TooLarge), "nothing over 20 MB unasked");
    ui.inspector_preview(true);
    assert!(matches!(ui.inspector.preview, Preview::Image(_)), "Download to preview fetches it");
}

#[test]
fn a_picture_is_decoded_for_the_preview() {
    let fake = FakeRemote::web_server();
    let png = super::scenes::banner_png(32, 18);
    let mut w = fake.write(&RPath::new("/var/www/site/banner.png"), 0, None).unwrap();
    std::io::Write::write_all(&mut w, &png).unwrap();
    w.finish().unwrap();
    let mut ui = with_remote(Arc::new(fake));
    select(&mut ui, &["banner.png"]);
    match &ui.inspector.preview {
        Preview::Image(img) => assert_eq!(img.size(), Some([32, 18])),
        _ => panic!("expected an image preview"),
    }
}

#[test]
fn staged_bits_toggle_across_items_and_apply_x_to_folders() {
    let modes = [0o644, 0o664];
    let mut s = Staged::default();
    assert_eq!(perms::tri(&modes, s, 0o020), Tri::Mixed);
    s.toggle(0o020, Tri::Mixed);
    assert_eq!(perms::tri(&modes, s, 0o020), Tri::On);
    assert_eq!(perms::uniform(&modes, s), Some(0o664));
    s.toggle(0o004, Tri::On);
    assert_eq!(s.on(0o644), 0o660);
    assert_eq!(perms::octal(0o770), "0770");
    // Typing 644 for a whole tree: with chmod's X (the engine's rule) folders stay openable.
    let typed = Staged::exact(0o644);
    assert_eq!(typed.enclosed(0o755, true, true), 0o755);
    assert_eq!(typed.enclosed(0o755, false, true), 0o644);
    assert_eq!(typed.enclosed(0o755, true, false), 0o644);
    // Only the bits the person changed are in the engine's mask; the rest stay per item.
    let world_write = Staged { set: 0o002, clear: 0 };
    assert_eq!(world_write.enclosed(0o750, true, true), 0o752);
    assert!(matches!(world_write.op(true), crate::transfers::engine::Op::SetMode { mode: 0o002, mask: 0o002, dirs_get_x_where_r: true }));
    assert_eq!(Staged::exact(0o4750).on(0o777), 0o4750);
}

#[test]
fn octal_edits_are_checked_and_apply_changes_the_server() {
    let mut ui = with_web();
    select(&mut ui, &["contact.php"]);
    assert_eq!(ui.inspector.perms.octal.text, "0644");
    ui.inspector.perms.octal = rune::TextEdit::new("98");
    ui.inspector.perms.typed();
    assert!(ui.inspector.perms.octal_error.is_some(), "invalid octal is explained inline");
    ui.inspector.perms.octal = rune::TextEdit::new("0640");
    ui.inspector.perms.typed();
    assert!(ui.inspector.perms.octal_error.is_none() && ui.inspector.dirty());
    ui.request_apply(false);
    drain(&mut ui);
    assert!(ui.inspector.perms.error.is_none(), "{:?}", ui.inspector.perms.error);
    let remote = ui.inspector.subject.as_ref().unwrap().remote.clone();
    assert_eq!(remote.stat(&RPath::new("/var/www/site/contact.php")).unwrap().mode, Some(0o640));
    ui.inspector_follow();
    assert!(!ui.inspector.dirty(), "applied: nothing staged");
}

#[test]
fn apply_to_enclosed_is_an_engine_job_in_transfers() {
    let mut ui = with_web();
    ui.prefs.transfers_open = false;
    select(&mut ui, &["img"]);
    ui.toggle_perm_bit(0o002); // world may write
    ui.request_apply(true);
    assert_eq!(ui.inspector.perms.confirm, Some(perms::Confirm::Enclosed), "asks first");
    ui.apply_perms(true);
    assert!(ui.prefs.transfers_open, "the drawer opens to show it");
    assert!(ui.transfers.rows.iter().any(|r| r.label == "Changing permissions of “img”"), "a row in Transfers");
    assert!(!ui.inspector.dirty(), "handed over: nothing left staged");
    let remote = ui.inspector.subject.as_ref().unwrap().remote.clone();
    let mode = |p: &str| remote.stat(&RPath::new(p)).unwrap().mode;
    assert!(until(&mut ui, |_| mode("/var/www/site/img/hero.jpg") == Some(0o646)), "the engine walked the folder");
    assert_eq!(mode("/var/www/site/img"), Some(0o757));
    assert_eq!(mode("/var/www/site/css/site.css"), Some(0o644), "outside the folder: untouched");
}

#[test]
fn a_new_selection_asks_about_staged_changes() {
    let mut ui = with_web();
    select(&mut ui, &["contact.php"]);
    ui.toggle_perm_bit(0o020);
    select(&mut ui, &["about.html"]);
    assert!(ui.inspector.prompt, "asks before following");
    assert_eq!(ui.inspector.subject.as_ref().unwrap().items[0].name, "contact.php", "still showing the edited item");
    ui.keep_editing();
    assert!(ui.panes[1].sel.is_selected("contact.php"), "Esc goes back to it");
    select(&mut ui, &["about.html"]);
    ui.answer_prompt(false);
    assert_eq!(ui.inspector.subject.as_ref().unwrap().items[0].name, "about.html");
    assert!(!ui.inspector.dirty());
    let remote = ui.inspector.subject.as_ref().unwrap().remote.clone();
    assert_eq!(remote.stat(&RPath::new("/var/www/site/contact.php")).unwrap().mode, Some(0o644), "discarded");
}

#[test]
fn backends_without_modes_hide_the_editor() {
    let mut ui = with_web();
    select(&mut ui, &["contact.php"]);
    let mut subj = ui.inspector.subject.clone().unwrap();
    assert!(subj.modes(None).is_some());
    subj.caps.permissions = false; // S3, Azure, cloud drives
    assert!(subj.modes(None).is_none());
    subj.caps.permissions = true;
    subj.items[0].mode = None; // a server that doesn't report them
    assert!(subj.modes(None).is_none());
    subj.items[0].mode = Some(0o777);
    subj.items[0].kind = filewinch_core::EntryKind::Symlink; // a link's mode is its target's
    assert!(subj.modes(None).is_none());
}

#[test]
fn width_and_open_state_are_remembered() {
    let dir = filewinch_testkit::scratch_dir(&filewinch_testkit::test_base(), "inspector-prefs");
    let store = filewinch_store::Store::new(&dir);
    let mut ui = with_web();
    ui.set_inspector_width(999.0, false);
    assert_eq!(ui.prefs.inspector_width, 420, "clamped to the widest");
    ui.set_inspector_width(333.4, false);
    ui.toggle_section("preview");
    store.save_prefs(&ui.prefs).unwrap();
    let back = store.load_prefs().value.clamped();
    assert!(back.inspector_open);
    assert_eq!(back.inspector_width, 333);
    assert_eq!(back.inspector_collapsed, ["preview"]);
    let narrow = filewinch_store::Prefs { inspector_width: 10, ..filewinch_store::Prefs::default() }.clamped();
    assert_eq!(narrow.inspector_width, 240);
    assert!(!filewinch_store::Prefs::default().inspector_open, "closed until asked for");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn narrow_windows_float_the_panel() {
    use super::view::floats;
    assert!(!floats(1320.0, 280.0, true));
    assert!(floats(880.0, 280.0, false), "below 900 pt");
    assert!(floats(1000.0, 280.0, true), "the panes would be squeezed under their minimum");
    assert!(!floats(1000.0, 280.0, false));
}

fn runtime(ui: Ui) -> Runtime<Ui> {
    let mut rt = Runtime::new(ui, crate::view::view, (1320.0, 820.0), Env::new(false, 1.0));
    rt.frame(0.0);
    rt
}

#[test]
fn the_shortcut_toggles_the_panel_and_esc_leaves_it() {
    let mut ui = with_web();
    ui.prefs.inspector_open = false;
    let mut rt = runtime(ui);
    assert!(rt.find(super::FOCUS_KEY).is_none());
    let chord = KeyEvent { key: Key::Command('i'), mods: Mods { cmd: true, alt: true, ..Mods::default() } };
    rt.update(|s, tx| {
        assert!(s.window_key(&chord, tx), "⌥⌘I / Ctrl+Alt+I is the window's");
    });
    rt.frame(1.0 / 60.0);
    rt.frame(1.0 / 60.0);
    assert!(rt.state.prefs.inspector_open);
    assert!(rt.find(super::FOCUS_KEY).is_some(), "the panel is in the window");
    // Tab from the right pane goes to the panel; Esc there returns to the pane.
    rt.update(|s, tx| {
        s.pane_key(1, &KeyEvent { key: Key::Tab, mods: Mods::default() }, tx);
    });
    rt.frame(1.0 / 60.0);
    rt.update(|s, tx| s.inspector_escape(tx));
    rt.frame(1.0 / 60.0);
    rt.update(|s, tx| {
        s.window_key(&chord, tx);
    });
    rt.frame(1.0 / 60.0);
    assert!(!rt.state.prefs.inspector_open);
    assert!(rt.find(super::FOCUS_KEY).is_none());
}

#[test]
fn every_control_in_the_panel_has_a_name() {
    let mut ui = with_web();
    select(&mut ui, &["contact.php"]);
    let rt = runtime(ui);
    let tree = rt.accessibility_json().to_string();
    for want in ["Inspector", "Copy info", "Close Inspector", "User can write", "SUID (set user ID)", "Permissions in octal", "Copy Path", "Copy URL"] {
        assert!(tree.contains(want), "{want} is in the accessibility tree");
    }
    assert!(tree.contains("sftp://deploy@web1.example.com/var/www/site/contact.php"));
}
