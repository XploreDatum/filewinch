//! Saved Servers in the connect sheet: hidden without favorites, order, filter, connecting into
//! the sheet's pane, Edit… then Save in place, and the keyboard.

use filewinch_core::{Auth, Protocol, SecretRef, Site};
use filewinch_store::{Favorite, Recent};
use rune::{Env, Key, KeyEvent, Mods, Runtime};

use super::flow::Conn;
use super::saved::{detail, order};
use crate::ui::{Sheet, Ui};

/// A server on a closed local port: connecting fails at once, without the network.
fn site(id: &str, name: &str, p: Protocol, host: &str, user: &str) -> Site {
    let mut s = Site::new(id, name, p);
    s.host = host.into();
    s.user = user.into();
    s.port = Some(9);
    s.auth = Auth::Password { secret: SecretRef(format!("{id}/password")) };
    s
}

fn ui_with(n: usize) -> Ui {
    let mut ui = Ui::new();
    let clients = ui.sites.add_folder("Clients");
    let names = ["web1 production", "Staging", "Assets bucket", "Home NAS", "Team Drive", "Old box", "mail", "db 2", "db 10", "Backups", "CDN", "Docs"];
    for (i, name) in names.iter().take(n).enumerate() {
        let id = format!("s{i}");
        let (p, host, user) = match i {
            2 => (Protocol::S3, "", "AKIAEXAMPLE"),
            4 => (Protocol::GoogleDrive, "", "me@example.com"),
            _ => (Protocol::Sftp, "127.0.0.1", "deploy"),
        };
        let folder = (i == 0).then(|| clients.clone());
        ui.sites.upsert(Favorite::new(site(&id, name, p, host, user), folder));
    }
    ui
}

fn runtime(ui: Ui) -> Runtime<Ui> {
    let mut rt = Runtime::new(ui, crate::view::view, (1320.0, 820.0), Env::new(false, 1.0));
    rt.frame(0.0);
    rt
}

/// Frames with workers' results landing in between, as the app's `poll` lands them: through
/// `update`, so the view rebuilds (a result applied straight to `rt.state` wouldn't show).
fn step(rt: &mut Runtime<Ui>) {
    for _ in 0..3 {
        rt.update(|s, _| {
            for f in s.inbox.drain() {
                f(s);
            }
        });
        rt.frame(1.0 / 60.0);
    }
}

fn click(rt: &mut Runtime<Ui>, key: &str) {
    let (x, y) = rt.find(key).unwrap_or_else(|| panic!("{key} is on screen"));
    rt.pointer_move(x, y);
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    step(rt);
}

fn key(rt: &mut Runtime<Ui>, k: Key) {
    rt.key(KeyEvent { key: k, mods: Mods::default() });
    step(rt);
}

/// The focused element's key, or for a text field (focus sits on its inner input) the field's.
fn focused(rt: &Runtime<Ui>) -> Option<String> {
    let e = rt.elements().into_iter().find(|e| e.focused && !e.exiting)?;
    if let Some(field) = e.path.split('/').find(|c| c.starts_with("field-")) {
        return Some(field.to_owned());
    }
    e.key
}

fn label_of(rt: &Runtime<Ui>, key: &str) -> Option<String> {
    rt.elements().into_iter().find(|e| !e.exiting && e.key.as_deref() == Some(key)).and_then(|e| e.label)
}

fn rows(rt: &Runtime<Ui>) -> usize {
    (0..20).take_while(|i| rt.find(&format!("saved-{i}")).is_some()).count()
}

/// The pane and site of the attempt the sheet started, whatever state it reached.
fn attempt(ui: &Ui) -> Option<(usize, String)> {
    match &ui.conn {
        Conn::Connecting { target, .. } | Conn::Failed { target, .. } | Conn::Password { target, .. } | Conn::Trust { target, .. } => {
            Some((target.pane, target.site.id.clone()))
        }
        Conn::Idle => None,
    }
}

fn form(ui: &Ui) -> &super::form::Form {
    match &ui.sheet {
        Some(Sheet::Connect(f)) => f,
        _ => panic!("the connect sheet is open"),
    }
}

#[test]
fn hidden_without_favorites() {
    let mut ui = Ui::new();
    ui.open_connect_in(1);
    let rt = runtime(ui);
    assert!(rt.find("connect-body").is_some());
    assert!(rt.find("saved").is_none(), "no Saved Servers section");
    assert!(rt.find("protocol").is_some(), "the form is as before");
}

#[test]
fn recently_used_first_then_by_name() {
    let mut ui = ui_with(10);
    ui.recents.items.push(Recent { site: ui.sites.get("s3").unwrap().site.clone(), at: 300 });
    ui.recents.items.push(Recent { site: ui.sites.get("s1").unwrap().site.clone(), at: 500 });
    let names: Vec<&str> = order(&ui.sites, &ui.recents, "").iter().map(|f| f.site.name.as_str()).collect();
    assert_eq!(names, ["Staging", "Home NAS", "Assets bucket", "Backups", "db 2", "db 10", "mail", "Old box", "Team Drive", "web1 production"]);
    // The sidebar's matching: name, host, user or protocol, any case.
    let hits: Vec<&str> = order(&ui.sites, &ui.recents, "DB").iter().map(|f| f.site.name.as_str()).collect();
    assert_eq!(hits, ["db 2", "db 10"]);
    assert_eq!(order(&ui.sites, &ui.recents, "google drive").len(), 1);
    assert_eq!(order(&ui.sites, &ui.recents, "deploy").len(), 8);
}

#[test]
fn rows_say_where_each_server_goes() {
    let ui = ui_with(5);
    assert_eq!(detail(&ui.sites.get("s0").unwrap().site), "deploy@127.0.0.1");
    assert_eq!(detail(&ui.sites.get("s4").unwrap().site), "me@example.com", "a cloud drive shows its account");
    let mut s3 = ui.sites.get("s2").unwrap().site.clone();
    assert_eq!(detail(&s3), "Amazon S3", "never the access key");
    s3.initial_path = Some("/acme-assets/img".into());
    assert_eq!(detail(&s3), "acme-assets", "a bucket when there is one");
    let mut rt = runtime(ui);
    rt.update(|s, _| s.open_connect_in(1));
    step(&mut rt);
    assert_eq!(rows(&rt), 5);
    assert_eq!(label_of(&rt, "saved-0").as_deref(), Some("Assets bucket, Amazon S3, Amazon S3, connect"));
    let web1 = (0..5).find_map(|i| label_of(&rt, &format!("saved-{i}")).filter(|l| l.starts_with("web1"))).unwrap();
    assert_eq!(web1, "web1 production, SFTP, deploy@127.0.0.1, connect");
    let shown: Vec<String> = rt.elements().into_iter().filter_map(|e| e.text).collect();
    assert!(shown.iter().any(|t| t == "Clients"), "the group shows");
    assert!(rt.find("saved-filter").is_none(), "no filter for five");
}

#[test]
fn the_filter_appears_past_six_and_narrows_the_list() {
    let mut rt = runtime(ui_with(12));
    rt.update(|s, _| s.open_connect_in(0));
    step(&mut rt);
    assert!(rt.find("saved-filter").is_some());
    assert_eq!(rows(&rt), 12, "every row is built; the list scrolls past five");
    let list = rt.elements().into_iter().find(|e| e.key.as_deref() == Some("saved-list")).unwrap();
    assert!(list.rect[3] < 7.0 * 34.0, "about five rows tall: {}", list.rect[3]);
    rt.update(|s, _| {
        if let Some(Sheet::Connect(f)) = &mut s.sheet {
            f.saved_filter = rune::TextEdit::new("db");
        }
    });
    step(&mut rt);
    assert_eq!(rows(&rt), 2);
    rt.update(|s, _| {
        if let Some(Sheet::Connect(f)) = &mut s.sheet {
            f.saved_filter = rune::TextEdit::new("zzz");
        }
    });
    step(&mut rt);
    assert_eq!(rows(&rt), 0);
    assert!(rt.find("saved-none").is_some(), "says nothing matches");
}

#[test]
fn a_click_connects_in_the_sheets_pane_and_closes_it() {
    let mut rt = runtime(ui_with(3));
    rt.update(|s, _| {
        s.active = 0;
        s.open_connect_in(1);
    });
    step(&mut rt);
    let web1 = (0..3).find(|i| label_of(&rt, &format!("saved-{i}")).is_some_and(|l| l.starts_with("web1"))).unwrap();
    click(&mut rt, &format!("saved-{web1}"));
    assert!(rt.state.sheet.is_none(), "the sheet closes once the connection starts");
    assert_eq!(attempt(&rt.state), Some((1, "s0".to_owned())), "into the right pane, through the usual flow");
    // The flow's own dialog takes over: here the password re-prompt (none is saved), which
    // connects again from the same target.
    assert!(matches!(rt.state.conn, Conn::Password { .. } | Conn::Failed { .. }));
    let shown: Vec<String> = rt.elements().into_iter().filter(|e| !e.exiting).filter_map(|e| e.text).collect();
    assert!(shown.iter().any(|t| t == "Log in to web1 production" || t == "Couldn't connect to web1 production"), "{shown:?}");
}

#[test]
fn edit_fills_the_form_and_save_updates_in_place() {
    let mut rt = runtime(ui_with(3));
    rt.update(|s, _| s.open_connect_in(1));
    step(&mut rt);
    let i = (0..3).find(|i| label_of(&rt, &format!("saved-{i}")).is_some_and(|l| l.starts_with("Staging"))).unwrap();
    click(&mut rt, &format!("saved-edit-{i}"));
    let f = form(&rt.state);
    assert_eq!(f.editing.as_deref(), Some("s1"));
    assert_eq!((f.name.text.as_str(), f.host.text.as_str(), f.user.text.as_str(), f.port.text.as_str()), ("Staging", "127.0.0.1", "deploy", "9"));
    assert_eq!(f.pane, Some(1), "still for the same pane");
    assert!(f.save, "Keep in Favorites");
    assert!(rt.state.conn.is_idle(), "Edit doesn't connect");
    let shown: Vec<String> = rt.elements().into_iter().filter_map(|e| e.text).collect();
    assert!(shown.iter().any(|t| t == "Edit Server"), "the sheet's title says so");
    assert!(shown.iter().any(|t| t == "Keep in Favorites"));
    assert_eq!(focused(&rt).as_deref(), Some("field-Name"), "focus moves to the form");
    rt.update(|s, _| {
        if let Some(Sheet::Connect(f)) = &mut s.sheet {
            f.name = rune::TextEdit::new("Staging (new)");
        }
    });
    click(&mut rt, "save");
    assert!(rt.state.sheet.is_none());
    assert_eq!(rt.state.sites.favorites.len(), 3, "updated, not duplicated");
    assert_eq!(rt.state.sites.get("s1").unwrap().site.name, "Staging (new)");
}

#[test]
fn arrows_move_return_connects_and_tab_reaches_edit_then_the_form() {
    let mut rt = runtime(ui_with(4));
    rt.update(|s, tx| {
        s.open_connect_in(0);
        tx.focus("saved-0", true);
    });
    step(&mut rt);
    assert_eq!(focused(&rt).as_deref(), Some("saved-0"));
    key(&mut rt, Key::Down);
    assert_eq!(focused(&rt).as_deref(), Some("saved-1"));
    assert_eq!(form(&rt.state).saved_cursor, 1);
    key(&mut rt, Key::Down);
    key(&mut rt, Key::Up);
    assert_eq!(focused(&rt).as_deref(), Some("saved-1"));
    key(&mut rt, Key::Tab);
    assert_eq!(focused(&rt).as_deref(), Some("saved-edit-1"), "Tab reaches the row's Edit…");
    key(&mut rt, Key::Tab);
    let next = focused(&rt).unwrap_or_default();
    assert!(!next.starts_with("saved"), "then leaves the list for the form: {next}");
    rt.update(|_, tx| tx.focus("saved-1", true));
    step(&mut rt);
    let expect = label_of(&rt, "saved-1").unwrap();
    key(&mut rt, Key::Enter);
    let (pane, id) = attempt(&rt.state).expect("Return connects");
    assert_eq!(pane, 0);
    assert!(expect.starts_with(&rt.state.sites.get(&id).unwrap().site.name));
}
