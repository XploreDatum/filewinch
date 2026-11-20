//! The empty pane: the right pane at launch and after Disconnect. It does no IO, takes no
//! drops or file keys, sends connections its way, and turns into This Mac on request.

use std::path::PathBuf;
use std::sync::Arc;

use filewinch_core::{Protocol, RPath, Remote};
use rune::{Env, Key, KeyEvent, Mods, Runtime};

use crate::connect::flow::Conn;
use crate::fake::FakeRemote;
use crate::pane::{NOT_CONNECTED, Source};
use crate::ui::{DragSrc, Session, Sheet, Ui};

fn drain(ui: &mut Ui) {
    for _ in 0..4 {
        for f in ui.inbox.drain() {
            f(ui);
        }
    }
}

fn runtime(ui: Ui) -> Runtime<Ui> {
    let mut rt = Runtime::new(ui, crate::view::view, (1320.0, 820.0), Env::new(false, 1.0));
    rt.frame(0.0);
    rt
}

fn step(rt: &mut Runtime<Ui>) {
    rt.frame(1.0 / 60.0);
    drain(&mut rt.state);
    rt.frame(1.0 / 60.0);
}

fn click(rt: &mut Runtime<Ui>, key: &str) {
    let (x, y) = rt.find(key).unwrap_or_else(|| panic!("{key} is on screen"));
    rt.pointer_move(x, y);
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    step(rt);
}

fn cmd(c: char) -> KeyEvent {
    KeyEvent { key: Key::Command(c), mods: Mods { cmd: true, ..Mods::default() } }
}

/// A fake server session (id 21) in pane `p`.
fn connect_fake(ui: &mut Ui, p: usize) {
    let remote: Arc<dyn Remote> = Arc::new(FakeRemote::web_server());
    ui.sessions.push(Session { id: 21, site: filewinch_core::Site::new("w", "web", Protocol::Sftp), remote: remote.clone() });
    ui.panes[p].switch_to(Source { session: 21, label: "web".into(), protocol: Protocol::Sftp, remote }, RPath::new("/var/www/site"), false);
}

/// Nothing queued, no sheet, the pane still empty.
fn inert(ui: &Ui) {
    assert!(ui.panes[1].is_empty(), "the pane stays empty");
    // `jobs` holds what this window queued (Transfers rows also hear other windows' engine events).
    assert!(ui.jobs.is_empty(), "nothing was queued");
    assert!(ui.sheet.is_none(), "no sheet opened");
}

#[test]
fn the_right_pane_starts_empty_with_connect_and_this_mac() {
    let ui = Ui::new();
    assert!(ui.panes[0].is_local(), "the left pane is This Mac");
    assert!(ui.panes[1].is_empty() && ui.panes[1].entries.is_empty());
    assert_eq!(ui.panes[1].status(), NOT_CONNECTED);
    assert_eq!(ui.panes[1].label(), NOT_CONNECTED);
    assert_eq!(ui.empty_pane(), Some(1));
    let rt = runtime(ui);
    for shown in ["empty-connect-1", "empty-local-1", "empty-title", "source-1", "status-1"] {
        assert!(rt.find(shown).is_some(), "{shown} shows");
    }
    for hidden in ["pathbar-1", "filter-1", "back-1", "forward-1", "up-1", "refresh-1", "disconnect-1", "pane-1/header"] {
        assert!(rt.find(hidden).is_none(), "{hidden} is hidden on an empty pane");
    }
    // Both buttons are named buttons, and the pane's focus key is the empty state.
    let els = rt.elements();
    let named = |key: &str| els.iter().find(|e| e.key.as_deref() == Some(key)).map(|e| (e.role, e.label.clone().or(e.text.clone()).unwrap_or_default()));
    let (role, name) = named("empty-connect-1").expect("connect");
    assert_eq!(role, "button");
    assert!(name.contains("Connect"), "{name}");
    let (role, name) = named("empty-local-1").expect("this mac");
    assert_eq!(role, "button");
    assert!(name.contains(crate::theme::ui_os().this_computer()), "{name}");
    assert!(els.iter().any(|e| e.key.as_deref() == Some("pane-1") && e.focusable), "Cmd-2 and Tab reach the empty pane");
}

#[test]
fn this_mac_turns_the_empty_pane_into_the_home_folder() {
    let mut rt = runtime(Ui::new());
    click(&mut rt, "empty-local-1");
    let home = Source::local().remote.home().expect("home");
    let pane = &rt.state.panes[1];
    assert!(pane.is_local() && !pane.is_empty());
    assert_eq!(pane.dir, home);
    assert_eq!(rt.state.active, 1);
    assert!(rt.find("pathbar-1").is_some() && rt.find("empty-connect-1").is_none(), "the listing chrome is back");
    assert_eq!(rt.state.empty_pane(), None);
}

#[test]
fn connections_go_to_the_empty_pane() {
    let mut rt = runtime(Ui::new());
    // The empty state's Connect… opens the sheet for that pane.
    click(&mut rt, "empty-connect-1");
    match &rt.state.sheet {
        Some(Sheet::Connect(f)) => assert_eq!(f.pane, Some(1)),
        _ => panic!("Connect… opens the connect sheet"),
    }
    rt.state.sheet = None;
    // Return on the focused empty pane connects too.
    rt.update(|_, tx| tx.focus("pane-1", true));
    rt.frame(1.0 / 60.0);
    rt.key(KeyEvent { key: Key::Enter, mods: Mods::default() });
    step(&mut rt);
    assert!(matches!(&rt.state.sheet, Some(Sheet::Connect(f)) if f.pane == Some(1)), "Return presses Connect…");
    rt.state.sheet = None;

    // Cmd-K and favorites from the left pane land in the empty one.
    let ui = &mut rt.state;
    ui.active = 0;
    assert_eq!(ui.connect_pane(0), 1);
    rt.update(|s, tx| {
        s.window_key(&cmd('k'), tx);
    });
    assert!(matches!(&rt.state.sheet, Some(Sheet::Connect(f)) if f.pane.is_none()), "Cmd-K leaves the pane to the submit");
    rt.state.sheet = None;
    let ui = &mut rt.state;
    let mut site = filewinch_core::Site::new("nowhere", "Nowhere", Protocol::Sftp);
    site.host = "127.0.0.1".into();
    site.port = Some(1); // nothing listens: fails fast, keeping its target
    site.user = "me".into();
    ui.sites.upsert(filewinch_store::Favorite::new(site, None));
    let to = ui.connect_pane(ui.active);
    ui.connect_favorite("nowhere", to);
    drain(ui);
    match &ui.conn {
        Conn::Failed { target, .. } => assert_eq!(target.pane, 1, "the favorite connects into the empty pane"),
        _ => panic!("expected the refused attempt"),
    }
    ui.cancel_connect();

    // With no empty pane, the active pane keeps its old role.
    ui.show_local(1);
    ui.active = 0;
    assert_eq!(ui.connect_pane(0), 0);
}

#[test]
fn disconnect_returns_the_pane_to_empty_and_closes_the_session() {
    let mut ui = Ui::new();
    connect_fake(&mut ui, 1);
    assert!(!ui.panes[1].is_empty() && !ui.panes[1].is_local());
    ui.panes[1].sel.only("index.html");
    ui.disconnect(1);
    assert!(ui.panes[1].is_empty(), "Disconnect empties the pane rather than showing This Mac");
    assert!(ui.panes[1].sel.names.is_empty() && ui.panes[1].back.is_empty());
    assert!(ui.sessions.is_empty(), "the session closes once no pane shows it");
    // Disconnecting an empty or local pane does nothing.
    ui.disconnect(1);
    ui.disconnect(0);
    assert!(ui.panes[0].is_local() && ui.panes[1].is_empty());

    // The source menu's This Mac on a server pane shows home and closes the session.
    connect_fake(&mut ui, 1);
    ui.show_local(1);
    assert!(ui.panes[1].is_local() && ui.sessions.is_empty());
}

#[test]
fn a_late_listing_for_the_old_site_does_not_fill_an_emptied_pane() {
    let mut ui = Ui::new();
    connect_fake(&mut ui, 1);
    ui.panes[1].clear(false);
    let landed = ui.panes[1].poll(false);
    assert!(!landed || ui.panes[1].entries.is_empty());
    ui.panes[1].refresh(false);
    ui.panes[1].go(RPath::new("/var/www"), false);
    ui.panes[1].up(false);
    assert!(ui.panes[1].is_empty() && ui.panes[1].entries.is_empty() && ui.panes[1].back.is_empty());
    assert_eq!(ui.panes[1].dir, RPath::root());
}

#[test]
fn drops_and_drags_onto_the_empty_pane_are_refused() {
    let mut rt = runtime(Ui::new());
    let at = rt.find("empty-title").expect("empty state");
    let file = vec![PathBuf::from("/Users/alex/Projects/acme-site/index.html")];
    rt.drag_files(file.clone(), Some(at));
    rt.frame(1.0 / 60.0);
    assert_eq!(rt.drop_hovered(), None, "no drop target under the pointer");
    rt.drop_files(file.clone(), Some(at));
    step(&mut rt);
    inert(&rt.state);
    // A row dragged from the left pane: refused the same way, and a stray drop call is inert.
    let src = rt.state.panes[0].src.clone().expect("left pane");
    rt.state.drag = Some(DragSrc { pane: 0, source: src, paths: vec![RPath::new("/Users/alex/a.txt")], handed: file.clone() });
    rt.drag_files(file.clone(), Some(at));
    rt.drop_files(file.clone(), Some(at));
    step(&mut rt);
    inert(&rt.state);
    rt.update(|s, tx| s.dropped(1, None, &file, tx));
    inert(&rt.state);
    // Nor can anything be sent there from the left pane.
    rt.state.panes[0].sel.only("anything");
    rt.state.send_to_other(0);
    inert(&rt.state);
}

#[test]
fn file_shortcuts_do_nothing_on_an_empty_pane() {
    let mut rt = runtime(Ui::new());
    rt.state.active = 1;
    let shift_cmd_n = KeyEvent { key: Key::Command('n'), mods: Mods { cmd: true, shift: true, ..Mods::default() } };
    let cmd_back = KeyEvent { key: Key::Backspace, mods: Mods { cmd: true, ..Mods::default() } };
    let cmd_up = KeyEvent { key: Key::Up, mods: Mods { cmd: true, ..Mods::default() } };
    for ev in [cmd('r'), cmd('i'), cmd('['), cmd(']'), shift_cmd_n, cmd_back.clone(), cmd_up] {
        rt.update(|s, tx| {
            s.window_key(&ev, tx);
        });
        step(&mut rt);
        inert(&rt.state);
    }
    let plain = |key| KeyEvent { key, mods: Mods::default() };
    for ev in [cmd('a'), cmd_back, plain(Key::Delete), plain(Key::Function(2)), plain(Key::Text("a".into())), plain(Key::Down)] {
        let taken = rt.update_if(|s, tx| s.pane_key(1, &ev, tx));
        assert!(!taken, "{:?} isn't taken by an empty pane", ev.key);
        step(&mut rt);
        inert(&rt.state);
        assert!(rt.state.panes[1].sel.names.is_empty() && rt.state.panes[1].rename.is_none());
    }
    // Menu-bar style calls straight in.
    let ui = &mut rt.state;
    ui.refresh_pane(1);
    ui.new_item(1, true);
    ui.duplicate(1);
    ui.delete(1, vec![RPath::new("/etc/hosts")]);
    ui.open_info(1);
    ui.open_file(1, &RPath::new("/etc/hosts"), false);
    drain(ui);
    inert(ui);
}

#[test]
fn sync_is_unavailable_while_a_pane_is_empty() {
    let mut rt = runtime(Ui::new());
    let disabled = |rt: &Runtime<Ui>| {
        fn find(v: &serde_json::Value) -> Option<&serde_json::Value> {
            if v.get("name").and_then(|n| n.as_str()) == Some("Synchronize folders") {
                return Some(v);
            }
            match v {
                serde_json::Value::Object(o) => o.values().find_map(find),
                serde_json::Value::Array(a) => a.iter().find_map(find),
                _ => None,
            }
        }
        let tree = rt.accessibility_json();
        let node = find(&tree).expect("the toolbar's Sync button");
        node.get("states").is_some_and(|s| s.to_string().contains("disabled"))
    };
    assert!(disabled(&rt), "the toolbar's Sync is disabled");
    rt.state.open_sync();
    assert!(rt.state.sheet.is_none(), "the menu's Synchronize does nothing but explain");
    assert!(rt.state.notices.iter().any(|n| n.text.contains("both panes")));
    rt.update(|s, _| s.show_local(1));
    step(&mut rt);
    assert!(!disabled(&rt), "with both panes showing folders, Sync is back");
    rt.state.open_sync();
    assert!(matches!(rt.state.sheet, Some(Sheet::Sync(_))));
}
