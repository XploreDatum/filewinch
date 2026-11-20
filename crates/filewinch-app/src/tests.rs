//! App-level tests: forms, the connection flow, the transfers model, and the window driven
//! through Rune's headless runtime (keys, clicks, the accessibility tree).

use std::sync::Arc;
use std::time::{Duration, Instant};

use filewinch_core::{Error, ErrorKind, Protocol, RPath, Remote};
use rune::{Env, Key, KeyEvent, Mods, Runtime};

use crate::connect::flow::{Conn, parse_trust};
use crate::connect::form::{AuthChoice, Field, Form};
use crate::fake::FakeRemote;
use crate::pane::Source;
use crate::transfers::engine::{ConflictAsk, Event, JobState, Progress};
use crate::transfers::model::Transfers;
use crate::ui::{Session, Sheet, Ui};

fn drain(ui: &mut Ui) {
    for _ in 0..4 {
        for f in ui.inbox.drain() {
            f(ui);
        }
    }
}

/// A window with the fake web server in the right pane.
fn with_fake() -> Ui {
    let mut ui = Ui::new();
    let remote: Arc<dyn Remote> = Arc::new(FakeRemote::web_server());
    ui.sessions.push(Session { id: 9, site: filewinch_core::Site::new("w", "web", Protocol::Sftp), remote: remote.clone() });
    ui.panes[1].switch_to(Source { session: 9, label: "web".into(), protocol: Protocol::Sftp, remote }, RPath::new("/var/www/site"), false);
    ui
}

fn runtime(ui: Ui) -> Runtime<Ui> {
    let mut rt = Runtime::new(ui, crate::view::view, (1320.0, 820.0), Env::new(false, 1.0));
    rt.frame(0.0);
    rt
}

fn key(rt: &mut Runtime<Ui>, key: Key, mods: Mods) {
    rt.key(KeyEvent { key, mods });
    rt.frame(1.0 / 60.0);
    drain(&mut rt.state);
    rt.frame(1.0 / 60.0);
}

#[test]
fn an_empty_sftp_form_explains_what_is_missing() {
    let mut f = Form::new(Protocol::Sftp);
    assert!(!f.validate());
    assert_eq!(f.errors.get(&Field::Host).map(String::as_str), Some("Enter the server's name or address."));
    assert_eq!(f.errors.get(&Field::User).map(String::as_str), Some("Enter the user."));
    f.host.text = "web1.example.com".into();
    f.user.text = "deploy".into();
    f.port.text = "99999".into();
    assert!(!f.validate());
    assert!(f.errors.contains_key(&Field::Port));
    f.port.text = "2222".into();
    assert!(f.validate(), "{:?}", f.errors);
}

#[test]
fn form_to_site_keeps_secrets_out_of_the_site() {
    let mut f = Form::new(Protocol::Sftp);
    f.host.text = " web1 ".into();
    f.user.text = "deploy".into();
    f.auth = AuthChoice::KeyFile;
    f.key_path.text = "/k/id".into();
    f.password.text = "throwaway-passphrase".into();
    let (site, secrets) = f.to_site(Some("abc"));
    assert_eq!(site.host, "web1");
    assert_eq!(site.name, "web1", "a nameless site is named after its host");
    assert!(matches!(&site.auth, filewinch_core::Auth::KeyFile { path, passphrase: Some(r) } if path == "/k/id" && r.0 == "abc/passphrase"));
    assert_eq!(secrets, vec![("abc/passphrase".to_owned(), "throwaway-passphrase".to_owned())]);

    let mut s3 = Form::new(Protocol::S3);
    s3.user.text = "AKIA".into();
    assert!(!s3.validate(), "the secret key is required");
    assert!(s3.errors.contains_key(&Field::Password));
    assert!(!s3.errors.contains_key(&Field::Host), "the endpoint is optional for AWS");
}

#[test]
fn cloud_forms_need_a_sign_in() {
    let mut f = Form::new(Protocol::Dropbox);
    assert!(!f.validate());
    assert!(f.errors.contains_key(&Field::SignIn));
}

#[test]
fn trust_details_split_into_fingerprint_and_reason() {
    let fp = "SHA256:nThbg6kXUpJWGl7E1IGOCspRomTxdCARLviKw6E5SY8";
    let e = Error::host_trust("Unknown certificate", fp, "self-signed");
    assert_eq!(parse_trust(&e), (fp.into(), "self-signed".into()));
    let bare = Error::new(ErrorKind::HostTrust, "x").with_detail(fp);
    assert_eq!(parse_trust(&bare), (fp.into(), String::new()));
    // Older hex form from a TLS backend: same canonical fingerprint.
    let hex = "sha256:9D:38:5B:83:A9:17:52:92:56:1A:5E:C4:D4:81:8E:0A:CA:51:A2:64:F1:74:20:11:2E:F8:8A:C3:A1:39:49:8F; x";
    assert_eq!(parse_trust(&Error::new(ErrorKind::HostTrust, "x").with_detail(hex)).0, fp);
    let junk = Error::new(ErrorKind::HostTrust, "x").with_detail("not a fingerprint");
    assert_eq!(parse_trust(&junk), (String::new(), "not a fingerprint".into()));
}

#[test]
fn a_refused_connection_fails_with_a_message_and_retry() {
    let mut ui = Ui::new();
    // Nothing listens on port 1: the backend fails fast with a network error.
    let mut site = filewinch_core::Site::new("x", "Nowhere", Protocol::Sftp);
    site.host = "127.0.0.1".into();
    site.port = Some(1);
    site.user = "me".into();
    ui.connect(crate::connect::flow::Target { site, pane: 1, secrets: vec![], save: false, folder: None, trusted_once: vec![] });
    drain(&mut ui);
    match &ui.conn {
        Conn::Failed { error, .. } => assert!(!error.message.is_empty()),
        Conn::Password { .. } | Conn::Trust { .. } => panic!("a refused connection isn't an auth or trust question"),
        Conn::Idle | Conn::Connecting { .. } => panic!("expected a failure"),
    }
    ui.retry_connect();
    drain(&mut ui);
    assert!(matches!(ui.conn, Conn::Failed { .. }));
    ui.cancel_connect();
    assert!(ui.conn.is_idle());
}

#[test]
fn a_job_is_drawn_by_the_way_it_goes() {
    use crate::transfers::engine::Op;
    use crate::transfers::model::Direction;
    assert_eq!(Direction::of(&Op::Copy, true, Some(false)), Direction::Upload);
    assert_eq!(Direction::of(&Op::Move, true, Some(false)), Direction::Upload);
    assert_eq!(Direction::of(&Op::Copy, false, Some(true)), Direction::Download);
    assert_eq!(Direction::of(&Op::Copy, false, Some(false)), Direction::Across);
    assert_eq!(Direction::of(&Op::Copy, true, Some(true)), Direction::Across);
    assert_eq!(Direction::of(&Op::Delete, false, None), Direction::Delete);
    let mut t = Transfers::default();
    t.apply(&Event::Added(3));
    assert_eq!(t.row(3).map(|r| r.direction), Some(Direction::Across));
    t.set_direction(3, Direction::Download);
    assert_eq!(t.row(3).map(|r| r.direction), Some(Direction::Download));
}

#[test]
fn transfers_model_follows_engine_events() {
    let mut t = Transfers::default();
    assert_eq!(t.apply(&Event::Added(1)), None);
    t.set_label(1, "Uploading");
    t.apply(&Event::Progress(1, Progress { bytes_done: 50, bytes_total: Some(200), ..Progress::default() }));
    assert_eq!(t.row(1).and_then(|r| r.fraction()), Some(0.25));
    let entry = |n: &str| filewinch_core::Entry {
        name: n.into(),
        path: RPath::new("/").join(n),
        kind: filewinch_core::EntryKind::File,
        size: Some(1),
        modified: None,
        mode: None,
        owner: None,
        group: None,
        link_target: None,
        extra: vec![], checksums: Vec::new(), shared: None,
    };
    t.apply(&Event::Conflict { job: 1, ask: ConflictAsk { src: entry("a"), dst: entry("a"), can_resume: false } });
    assert!(t.has_question());
    t.apply(&Event::State(1, JobState::Running));
    assert!(!t.has_question(), "a running job has no open question");
    t.apply(&Event::ItemFailed { job: 1, path: RPath::new("/a"), error: Error::new(ErrorKind::PermissionDenied, "no") });
    let done = t.apply(&Event::State(1, JobState::Failed(Error::new(ErrorKind::Other, "1 of 2 failed"))));
    assert_eq!(done, Some((1, false)));
    assert!(t.row(1).is_some_and(|r| r.expanded && r.failures.len() == 1));
    assert_eq!(t.apply(&Event::State(1, JobState::Failed(Error::new(ErrorKind::Other, "again")))), None, "finishes once");
    assert!(t.clear_completed().is_empty(), "failed jobs stay until dismissed");
    t.apply(&Event::Added(2));
    t.apply(&Event::State(2, JobState::Done));
    assert_eq!(t.clear_completed(), vec![2]);
}

#[test]
fn keyboard_moves_selection_and_opens_folders() {
    let mut rt = runtime(with_fake());
    rt.state.active = 1;
    let order: Vec<String> = rt.state.panes[1].order().into_iter().map(str::to_owned).collect();
    assert_eq!(order[..3], ["css", "img", "js"], "folders first");
    let (x, y) = rt.find("pane-1/viewport/rows/css").expect("the css row is on screen");
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(1.0 / 60.0);
    assert!(rt.state.panes[1].sel.is_selected("css"));
    key(&mut rt, Key::Down, Mods::default());
    assert!(rt.state.panes[1].sel.is_selected("img"));
    key(&mut rt, Key::Down, Mods { shift: true, ..Mods::default() });
    assert_eq!(rt.state.panes[1].sel.names.len(), 2);
    key(&mut rt, Key::Up, Mods::default());
    key(&mut rt, Key::Enter, Mods::default());
    assert_eq!(rt.state.panes[1].dir, RPath::new("/var/www/site/img"));
    key(&mut rt, Key::Up, Mods { cmd: true, ..Mods::default() });
    assert_eq!(rt.state.panes[1].dir, RPath::new("/var/www/site"));
    assert!(rt.state.panes[1].sel.is_selected("img"), "back on the folder we came out of");
    key(&mut rt, Key::Text("j".into()), Mods::default());
    assert!(rt.state.panes[1].sel.is_selected("js"), "type to select");
}

#[test]
fn rename_in_place_and_reject_taken_names() {
    let mut rt = runtime(with_fake());
    rt.state.active = 1;
    let (x, y) = rt.find("pane-1/viewport/rows/about.html").expect("row");
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(1.0 / 60.0);
    key(&mut rt, Key::Function(2), Mods::default());
    assert!(rt.state.panes[1].rename.is_some());
    if let Some(r) = &mut rt.state.panes[1].rename {
        r.edit.text = "index.html".into();
    }
    rt.update(|s, tx| s.commit_rename(1, tx));
    rt.frame(1.0 / 60.0);
    let err = rt.state.panes[1].rename.as_ref().and_then(|r| r.error.clone());
    assert!(err.is_some_and(|e| e.contains("already taken")));
    if let Some(r) = &mut rt.state.panes[1].rename {
        r.edit.text = "team.html".into();
    }
    rt.update(|s, tx| s.commit_rename(1, tx));
    rt.frame(1.0 / 60.0);
    drain(&mut rt.state);
    assert!(rt.state.panes[1].entry("team.html").is_some());
    assert!(rt.state.panes[1].entry("about.html").is_none());
    assert!(rt.state.panes[1].sel.is_selected("team.html"));
}

#[test]
fn delete_asks_first_and_cmd_k_validates_the_connect_sheet() {
    let mut rt = runtime(with_fake());
    rt.state.active = 1;
    rt.state.panes[1].sel.only("robots.txt");
    rt.update(|s, tx| {
        s.pane_key(1, &KeyEvent { key: Key::Backspace, mods: Mods { cmd: true, ..Mods::default() } }, tx);
    });
    rt.frame(1.0 / 60.0);
    assert!(matches!(rt.state.sheet, Some(Sheet::ConfirmDelete { ref names, .. }) if names == &["robots.txt"]));
    assert!(rt.find("confirm-ok").is_some(), "the confirmation shows");
    rt.state.sheet = None;
    rt.frame(1.0 / 60.0);
    // The window's own shortcut handler (Runtime::new has no AppSpec, so call it as the shell does).
    rt.update(|s, tx| {
        s.window_key(&KeyEvent { key: Key::Command('k'), mods: Mods { cmd: true, ..Mods::default() } }, tx);
    });
    rt.frame(1.0 / 60.0);
    assert!(matches!(rt.state.sheet, Some(Sheet::Connect(_))));
    rt.update(|s, _| s.connect_from_form());
    rt.frame(1.0 / 60.0);
    let tree = rt.accessibility_json().to_string();
    assert!(tree.contains("Enter the server's name or address."), "inline error is in the accessibility tree");
}

#[test]
fn every_control_in_the_main_window_has_a_name() {
    let rt = runtime(with_fake());
    let tree: serde_json::Value = rt.accessibility_json();
    let mut unnamed = Vec::new();
    fn walk(v: &serde_json::Value, out: &mut Vec<String>) {
        if let Some(obj) = v.as_object() {
            let role = obj.get("role").and_then(|r| r.as_str()).unwrap_or("");
            let named = obj.get("name").and_then(|n| n.as_str()).is_some_and(|n| !n.trim().is_empty());
            if matches!(role, "button" | "check-box" | "switch" | "text-input" | "combo-box" | "link" | "row" | "tree-item" | "column-header" | "grid") && !named {
                out.push(format!("{role} {:?}", obj.get("key")));
            }
            for v in obj.values() {
                walk(v, out);
            }
        } else if let Some(a) = v.as_array() {
            for v in a {
                walk(v, out);
            }
        }
    }
    walk(&tree, &mut unnamed);
    assert!(unnamed.is_empty(), "unnamed controls: {unnamed:?}");
}

fn drag(rt: &mut Runtime<Ui>, from: (f32, f32), dx: f32) {
    rt.pointer_down(from.0, from.1);
    rt.frame(1.0 / 60.0);
    for i in 1..=6 {
        rt.pointer_move(from.0 + dx * i as f32 / 6.0, from.1);
        rt.frame(1.0 / 60.0);
    }
    rt.pointer_up(from.0 + dx, from.1);
    rt.frame(1.0 / 60.0);
    rt.frame(1.0 / 60.0);
}

#[test]
fn header_dividers_resize_and_headers_move_without_sorting() {
    use filewinch_store::ColumnId;
    let mut rt = runtime(with_fake());
    let width = |rt: &Runtime<Ui>, id: ColumnId| rt.state.prefs.columns.iter().find(|c| c.id == id).and_then(|c| c.width);
    // The divider at Size's right edge (Name, Size, Modified, Kind): drag it 30 pt right.
    let (x, y) = rt.find("pane-1/header/size/divider-1").expect("Size's divider");
    drag(&mut rt, (x, y), 30.0);
    assert_eq!(width(&rt, ColumnId::Size), Some(110));
    // Name's divider resizes Size from its left edge: dragging left widens it.
    let (x, y) = rt.find("pane-1/header/name/divider-0").expect("Name's divider");
    drag(&mut rt, (x, y), -20.0);
    assert_eq!(width(&rt, ColumnId::Size), Some(130));
    // Both panes follow the shared layout.
    let size_cell = |rt: &Runtime<Ui>, p: usize| rt.state.panes[p].header_drag.is_none() && rt.find(&format!("pane-{p}/header/size")).is_some();
    assert!(size_cell(&rt, 0) && size_cell(&rt, 1));
    // Drag Kind's header to the front: it moves there, and the release doesn't sort by Kind.
    let sort_before = rt.state.panes[1].sort;
    let (x, y) = rt.find("pane-1/header/kind").expect("Kind header");
    drag(&mut rt, (x, y), -2000.0);
    let order: Vec<ColumnId> = rt.state.shown_columns().iter().map(|c| c.id).collect();
    assert_eq!(order, [ColumnId::Kind, ColumnId::Name, ColumnId::Size, ColumnId::Modified]);
    assert_eq!(rt.state.panes[1].sort, sort_before, "a drag isn't a click");
    // A plain click still sorts.
    let (x, y) = rt.find("pane-1/header/modified").expect("Modified header");
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(1.0 / 60.0);
    assert_eq!(rt.state.panes[1].sort.key, crate::pane::model::SortKey::Modified);
    // Cells keep their accessible names.
    let tree = rt.accessibility_json().to_string();
    for name in ["\"Name\"", "\"Size\"", "\"Modified\"", "\"Kind\""] {
        assert!(tree.contains(name), "{name} header is labelled");
    }
}

#[test]
fn the_permissions_column_shows_modes_as_ls_writes_them() {
    let mut rt = runtime(with_fake());
    rt.update(|s, _| s.prefs.show_permissions = true);
    rt.frame(1.0 / 60.0);
    assert!(rt.find("pane-1/header/permissions").is_some());
    let tree = rt.accessibility_json().to_string();
    assert!(tree.contains("rw-r--r--"), "a file's mode shows as ls writes it");
}

/// A window with the fake cloud drive (a shared `Design`, an owned shared `Budget 2027`) on
/// the right; returns its remote too.
fn with_cloud() -> (Ui, Arc<dyn Remote>) {
    let mut ui = Ui::new();
    let remote: Arc<dyn Remote> = Arc::new(FakeRemote::cloud_drive());
    ui.sessions.push(Session { id: 11, site: filewinch_core::Site::new("d", "Drive", Protocol::Dropbox), remote: remote.clone() });
    ui.panes[1].switch_to(Source { session: 11, label: "Drive".into(), protocol: Protocol::Dropbox, remote: remote.clone() }, RPath::new("/Acme"), false);
    ui.active = 1;
    (ui, remote)
}

/// Waits (up to 30 s) for the engine to make `done` true.
fn eventually(mut done: impl FnMut() -> bool) -> bool {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < end {
        if done() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

#[test]
fn deleting_a_shared_folder_offers_to_leave_it() {
    let (mut ui, remote) = with_cloud();
    // Even with delete confirmations turned off, shared items get their own question.
    ui.prefs.confirm_deletes = false;
    ui.panes[1].sel.only("Design");
    let mut rt = runtime(ui);
    assert!(rt.find("pane-1/viewport/rows/Design/shared-badge").is_some() || rt.accessibility_json().to_string().contains("Design, shared"), "the row says it's shared");
    rt.update(|s, _| s.ask_delete(1));
    rt.frame(1.0 / 60.0);
    let Some(Sheet::DeleteShared(d)) = &rt.state.sheet else { panic!("the shared-delete confirm opens") };
    assert_eq!(d.title(), "Leave “Design”?");
    let tree = rt.accessibility_json().to_string();
    assert!(tree.contains("It stays for the people it's shared with."), "{tree}");
    assert!(rt.find("delete-leave").is_some() && rt.find("delete-everyone").is_some());
    rt.update(|s, _| s.delete_shared_default());
    assert!(rt.state.sheet.is_none());
    assert!(eventually(|| remote.stat(&RPath::new("/Acme/Design")).is_err()), "left");
    assert!(remote.stat(&RPath::new("/Acme/Budget 2027/forecast.xlsx")).is_ok(), "nothing else touched");
}

#[test]
fn deleting_for_everyone_needs_the_acknowledgement() {
    let (mut ui, remote) = with_cloud();
    ui.panes[1].sel.only("Budget 2027");
    ui.panes[1].sel.names.insert("meeting-notes.md".into());
    let mut rt = runtime(ui);
    rt.update(|s, _| s.ask_delete(1));
    rt.frame(1.0 / 60.0);
    {
        let Some(Sheet::DeleteShared(d)) = &rt.state.sheet else { panic!("confirm opens") };
        assert_eq!(d.primary_label(), Some("Delete Others"), "the owned share can't be left: kept by default");
    }
    rt.update(|s, _| if let Some(Sheet::DeleteShared(d)) = &mut s.sheet { d.everyone = Some(false) });
    rt.frame(1.0 / 60.0);
    assert!(rt.find("everyone-ack").is_some() && rt.find("everyone-ok").is_some());
    // Unticked, the destructive action does nothing.
    rt.update(|s, _| s.delete_shared_everyone());
    assert!(matches!(rt.state.sheet, Some(Sheet::DeleteShared(_))));
    rt.update(|s, _| if let Some(Sheet::DeleteShared(d)) = &mut s.sheet { d.everyone = Some(true) });
    rt.update(|s, _| s.delete_shared_everyone());
    assert!(rt.state.sheet.is_none());
    assert!(eventually(|| remote.stat(&RPath::new("/Acme/Budget 2027")).is_err()), "its contents went too");
    assert!(remote.stat(&RPath::new("/Acme/meeting-notes.md")).is_err());
    assert!(remote.stat(&RPath::new("/Acme/Design/logo-mark.svg")).is_ok());
}

#[test]
fn sync_compare_reports_progress_and_can_be_stopped() {
    let mut ui = with_fake();
    ui.open_sync();
    ui.sync_compare();
    drain(&mut ui);
    let Some(Sheet::Sync(s)) = &ui.sheet else { panic!("sync sheet") };
    assert!(matches!(s.phase, crate::sync_sheet::Phase::Plan { .. }), "compare ran to a plan");

    // Stopping drops the in-flight compare, which cancels its worker's token.
    let cancel = crate::transfers::engine::CancelToken::new();
    if let Some(Sheet::Sync(s)) = &mut ui.sheet {
        let progress = crate::transfers::engine::CompareProgress { entries: 3, bytes_hashed: 2048, current: None };
        s.phase = crate::sync_sheet::Phase::Comparing(crate::sync_sheet::Comparing { attempt: ui.seq.next(), cancel: cancel.clone(), progress });
    }
    ui.sync_cancel_compare();
    assert!(cancel.is_cancelled());
    let Some(Sheet::Sync(s)) = &ui.sheet else { panic!("sync sheet") };
    assert!(matches!(s.phase, crate::sync_sheet::Phase::Stopped));
    let p = crate::transfers::engine::CompareProgress { entries: 1_204, bytes_hashed: 312_400_000, current: None };
    assert!(crate::sync_sheet::progress_text(&p).starts_with("Comparing… 1,204 items, "), "{}", crate::sync_sheet::progress_text(&p));
    // Closing the sheet mid-compare cancels too.
    let cancel = crate::transfers::engine::CancelToken::new();
    if let Some(Sheet::Sync(s)) = &mut ui.sheet {
        s.phase = crate::sync_sheet::Phase::Comparing(crate::sync_sheet::Comparing { attempt: ui.seq.next(), cancel: cancel.clone(), progress: Default::default() });
    }
    ui.sheet = None;
    assert!(cancel.is_cancelled());
}

#[test]
fn rows_rebuild_in_the_dark_kit_when_the_appearance_flips() {
    let mut ui = with_fake();
    ui.prefs.appearance = filewinch_store::Appearance::Light;
    let mut rt = runtime(ui);
    rt.settle_all();
    let name_color = |rt: &Runtime<Ui>| {
        rt.elements().into_iter().find(|e| e.text.as_deref() == Some("about.html")).and_then(|e| e.text_color).expect("the about.html row shows")
    };
    assert_eq!(name_color(&rt), crate::theme::kit(false).c.text_base);
    rt.update(|s, _| s.prefs.appearance = filewinch_store::Appearance::Dark);
    rt.frame(1.0 / 60.0);
    rt.settle_all();
    rt.frame(1.0 / 60.0);
    assert_eq!(name_color(&rt), crate::theme::kit(true).c.text_base, "cached rows rebuild with the dark kit's text colour");
}

fn scene_ui(name: &str) -> Ui {
    let mut ui = Ui::new();
    let (_, f) = crate::scenes::SCENES.iter().find(|(n, _)| *n == name).expect("scene");
    f(&mut ui);
    ui
}

#[test]
fn files_at_once_control_shows_for_multi_file_jobs_only() {
    let mut rt = runtime(scene_ui("transfers-folder"));
    rt.frame(0.1);
    let tree = rt.accessibility_json().to_string();
    // Counting (3) and known totals (6): both have it; the single large file doesn't.
    let shown = rt.elements().into_iter().filter(|e| !e.exiting && e.key.as_deref() == Some("files-at-once")).count();
    assert_eq!(shown, 2, "on the counting and running jobs, not the single-file job");
    if rune::popup_menu_supported() {
        // A menu button named with its value.
        assert!(tree.contains("Files at a time, 3"), "counting job");
        assert!(tree.contains("Files at a time, 6"), "running job");
    } else {
        // − and + around a spin button whose value is the count (Linux).
        assert_eq!(tree.matches("More files at a time").count(), 2, "{tree}");
        let counts: Vec<String> = rt.elements().into_iter().filter(|e| !e.exiting).filter_map(|e| e.text).filter(|t| t.ends_with("at a time")).collect();
        assert!(counts.iter().any(|t| t.starts_with('3')) && counts.iter().any(|t| t.starts_with('6')), "{counts:?}");
    }
}

#[test]
fn progress_text_while_counting_and_with_totals() {
    use crate::transfers::model::Row;
    use crate::transfers::view::{detail_line, header_line};
    let ui = scene_ui("transfers-folder");
    let rows: Vec<&Row> = ui.transfers.rows.iter().collect();
    let (known, counting, single) = (rows[0], rows[1], rows[2]);
    assert_eq!(header_line(counting), "Downloading 11 of 12+ to “untitled folder”");
    assert_eq!(detail_line(counting), "245 MB · counting… · 6.3 MB/s");
    assert!(counting.counting() && counting.fraction().is_none(), "an indeterminate bar");
    assert_eq!(header_line(known), "Downloading 9 of 24 to “Downloads”");
    assert_eq!(detail_line(known), "88 MB of 22 GB · 25 min left · 13 MB/s");
    assert!(!known.counting());
    assert_eq!(header_line(single), single.label, "a single file keeps its label");
    assert!(!single.many_files() && counting.many_files() && known.many_files());
}

#[test]
fn inline_rows_show_moving_files_with_skip_labels_and_not_for_single_files() {
    let mut ui = scene_ui("transfers-folder");
    // The scene's retry is 4.4 s out on the real clock, which a slow runner ticks down before
    // the first frame. Pin it an hour out so the label's form, not the clock, is under test.
    set_retry_at(&mut ui, Instant::now() + Duration::from_secs(3600));
    let mut rt = runtime(ui);
    rt.frame(0.1);
    let tree = rt.accessibility_json().to_string();
    assert!(tree.contains("Skip Friends.S01E05.The.One.With.The.East.German.Laundry.Detergent.1080p.BluRay.x264.mkv, 45% downloaded"), "pie label");
    assert!(tree.contains("connecting…"), "a file still connecting says so");
    let left = tree.split_once("retrying in ").and_then(|(_, t)| t.split_once(" seconds")).and_then(|(n, _)| n.parse::<u64>().ok());
    assert!(left.is_some_and(|n| (1..=3600).contains(&n)), "a retrying file says when: {left:?}");
    let inline = rt.elements().into_iter().filter_map(|e| e.text).any(|t| t.starts_with("Retrying in ") && t.ends_with(" s…"));
    assert!(inline, "the inline row says when too");
    assert!(tree.contains("Downloading now (4)") && tree.contains("Queued (3)") && tree.contains("Skipped (1)"), "grouped list");
    assert!(tree.contains("Done (4)"), "done is listed (collapsed)");
    assert!(!tree.contains("site-2026-11.tar, "), "no inline row for the single file");
}

/// Moves every retrying item of the scene's running job to retry at `at`.
fn set_retry_at(ui: &mut Ui, at: Instant) {
    use crate::transfers::engine::ActivePhase;
    let items = &mut ui.transfers.rows[0].items;
    for v in items.active.iter_mut().chain(items.pages.values_mut().flatten()) {
        if let Some(ActivePhase::Retrying { at: a }) = &mut v.phase {
            *a = at;
        }
    }
}

#[test]
fn a_retry_whose_moment_has_passed_says_now_not_zero() {
    let mut ui = scene_ui("transfers-folder");
    set_retry_at(&mut ui, Instant::now().checked_sub(Duration::from_secs(1)).unwrap_or_else(Instant::now));
    let mut rt = runtime(ui);
    rt.frame(0.1);
    let tree = rt.accessibility_json().to_string();
    assert!(tree.contains("retrying now") && !tree.contains("retrying in 0"), "{tree}");
    assert!(rt.elements().into_iter().filter_map(|e| e.text).any(|t| t == "Retrying now…"), "the inline row too");
}

#[test]
fn finished_jobs_sum_up_their_items() {
    use crate::transfers::engine::{ItemCounts, JobState};
    use crate::transfers::view::summary_line;
    let mut ui = scene_ui("transfers-folder");
    let r = &mut ui.transfers.rows[0];
    r.state = JobState::Done;
    r.items.counts = ItemCounts { done: 84, skipped: 2, failed: 1, ..ItemCounts::default() };
    assert_eq!(summary_line(r).as_deref(), Some("84 downloaded, 2 skipped, 1 failed"));
}
