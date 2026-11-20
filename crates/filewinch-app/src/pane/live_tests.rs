//! Live folders: re-lists after the engine's per-file events (debounced, only where they
//! happened), in-flight rows, hidden temp files, and a real folder watch.

use std::sync::Arc;

use filewinch_core::{RPath, Remote};
use rune::{Env, Runtime};

use super::live::DEBOUNCE;
use crate::fake::{FakeRemote, ist};
use crate::pane::{Listing, Source};
use crate::transfers::engine::{Event, JobState};
use crate::transfers::model::{Direction, JobMeta, Purpose};
use crate::ui::Ui;

const JOB: u64 = 9001;

/// This Mac (in memory): the left pane in `/dl`, where a download lands; the right in `/other`.
fn setup() -> (Runtime<Ui>, Arc<dyn Remote>) {
    let at = ist(20, 9, 0);
    let remote: Arc<dyn Remote> = Arc::new(FakeRemote::from_files(&[("/dl/b.txt", 5, at), ("/other/x.txt", 5, at)], &[]).with_home("/dl"));
    let mut ui = Ui::new();
    ui.panes[0].switch_to(Source { remote: remote.clone(), ..Source::local() }, RPath::new("/dl"), false);
    ui.panes[1].switch_to(Source { remote: remote.clone(), ..Source::local() }, RPath::new("/other"), false);
    ui.jobs.insert(JOB, JobMeta { purpose: Purpose::Plain, touched: Vec::new(), writes: Some(0), reads: None, direction: Direction::Download });
    let mut rt = Runtime::new(ui, crate::view::view, (1320.0, 820.0), Env::new(false, 1.0));
    rt.frame(0.0);
    (rt, remote)
}

/// A frame at `t` seconds, as the app runs one: `poll` first.
fn at(rt: &mut Runtime<Ui>, t: f64) {
    rt.update(|s, tx| {
        s.poll(t, tx);
    });
    rt.frame(t);
}

fn event(rt: &mut Runtime<Ui>, ev: Event) {
    rt.update(|s, _| s.on_engine_event(ev));
}

fn put(remote: &Arc<dyn Remote>, path: &str) {
    let mut w = remote.write(&RPath::new(path), 0, Some(3)).unwrap();
    std::io::Write::write_all(&mut w, b"abc").unwrap();
    w.finish().unwrap();
}

fn started(path: &str) -> Event {
    Event::ItemStarted { job: JOB, path: RPath::new(path), size: Some(3) }
}

fn done(path: &str) -> Event {
    Event::ItemDone { job: JOB, path: RPath::new(path), from: None }
}

#[test]
fn a_written_file_relists_its_folder_only() {
    let (mut rt, remote) = setup();
    at(&mut rt, 1.0);
    put(&remote, "/dl/a.txt");
    put(&remote, "/other/y.txt");
    event(&mut rt, done("/dl/a.txt"));
    at(&mut rt, 1.1);
    assert_eq!(rt.state.panes[0].refresh_at, Some(1.0 + DEBOUNCE), "the download's folder lists again soon");
    assert_eq!(rt.state.panes[1].refresh_at, None, "a folder it didn't touch doesn't");
    assert!(rt.state.panes[0].entry("a.txt").is_none(), "not before the debounce");
    at(&mut rt, 1.0 + DEBOUNCE + 0.01);
    assert!(rt.state.panes[0].entry("a.txt").is_some(), "the new file is listed");
    assert!(rt.state.panes[1].entry("y.txt").is_none(), "the other pane wasn't re-listed");
}

#[test]
fn a_file_deep_inside_relists_the_folder_above() {
    let (mut rt, remote) = setup();
    remote.mkdir(&RPath::new("/dl/new")).unwrap();
    put(&remote, "/dl/new/c.txt");
    event(&mut rt, done("/dl/new/c.txt"));
    at(&mut rt, 0.1);
    assert!(rt.state.panes[0].refresh_at.is_some(), "a new subfolder shows in the folder above");
    at(&mut rt, 1.0);
    assert!(rt.state.panes[0].entry("new").is_some());
}

#[test]
fn a_burst_of_events_is_one_relist() {
    let (mut rt, remote) = setup();
    at(&mut rt, 2.0);
    let before = rt.state.panes[0].version;
    for i in 0..500 {
        let p = format!("/dl/f{i:03}.bin");
        put(&remote, &p);
        event(&mut rt, started(&p));
        event(&mut rt, done(&p));
    }
    at(&mut rt, 2.05);
    assert_eq!(rt.state.panes[0].refresh_at, Some(2.0 + DEBOUNCE), "every event joined the first re-list");
    at(&mut rt, 2.0 + DEBOUNCE + 0.01);
    assert_eq!(rt.state.panes[0].refresh_at, None);
    assert_eq!(rt.state.panes[0].entries.len(), 501);
    // A few rebuilds (placeholder rows in, the listing, rows out), not one per file.
    assert!(rt.state.panes[0].version - before < 10, "{} rebuilds", rt.state.panes[0].version - before);
}

#[test]
fn a_relist_waits_for_the_listing_in_flight() {
    let (mut rt, _) = setup();
    let pane = &mut rt.state.panes[0];
    pane.refresh_soon(0.0);
    pane.busy = true;
    assert!(!pane.run_due(10.0, false), "one listing at a time");
    assert!(pane.refresh_at.is_some(), "still due");
    pane.busy = false;
    assert!(pane.run_due(10.0, false));
    assert!(pane.refresh_at.is_none());
}

#[test]
fn selection_rename_and_rows_survive_a_relist() {
    let (mut rt, remote) = setup();
    put(&remote, "/dl/c.txt");
    rt.update(|s, _| {
        s.panes[0].refresh(false);
        s.panes[0].sel.only("b.txt");
    });
    at(&mut rt, 1.0);
    rt.update(|s, tx| {
        s.active = 0;
        s.begin_rename(0, tx);
    });
    at(&mut rt, 1.1);
    assert!(rt.state.panes[0].rename.is_some());
    put(&remote, "/dl/d.txt");
    event(&mut rt, done("/dl/d.txt"));
    at(&mut rt, 1.2);
    // A quiet re-list: the table never falls back to "Loading…" (which would lose the scroll).
    assert!(matches!(rt.state.panes[0].listing, Listing::Ready));
    at(&mut rt, 1.0 + DEBOUNCE + 0.5);
    let pane = &rt.state.panes[0];
    assert!(pane.entry("d.txt").is_some());
    assert!(pane.sel.is_selected("b.txt"), "the selection stays");
    assert_eq!(pane.sel.cursor.as_deref(), Some("b.txt"), "so does the cursor");
    assert_eq!(pane.rename.as_ref().map(|r| r.original.as_str()), Some("b.txt"), "and the rename in progress");
    assert!(rt.find("viewport").is_some());
}

#[test]
fn in_flight_rows_show_then_become_real_rows() {
    let (mut rt, remote) = setup();
    at(&mut rt, 1.0);
    // The engine writes under a temporary name first: the listing doesn't have "a.txt" yet.
    put(&remote, "/dl/.a.txt.filewinch-part");
    event(&mut rt, started("/dl/a.txt"));
    at(&mut rt, 1.1);
    let pane = &rt.state.panes[0];
    assert!(pane.is_placeholder("a.txt"), "a row before the file exists");
    assert_eq!(pane.incoming.get("a.txt").copied(), Some("Downloading…"));
    assert_eq!(pane.order(), ["a.txt", "b.txt"], "the folder doesn't look empty, and no temp file shows");
    let texts: Vec<String> = rt.elements().into_iter().filter_map(|e| e.text).collect();
    assert!(texts.iter().any(|t| t == "Downloading…"), "the Kind column says so");
    assert!(rt.find("incoming").is_some(), "with a spinner");
    assert!(rt.elements().iter().any(|e| e.label.as_deref() == Some("a.txt, downloading")));
    // It lands under its real name.
    remote.rename(&RPath::new("/dl/.a.txt.filewinch-part"), &RPath::new("/dl/a.txt")).unwrap();
    event(&mut rt, done("/dl/a.txt"));
    at(&mut rt, 1.2);
    let pane = &rt.state.panes[0];
    assert!(pane.incoming.is_empty(), "no spinner once written");
    assert_eq!(pane.order(), ["a.txt", "b.txt"], "the row stays put until the listing lands");
    at(&mut rt, 1.1 + DEBOUNCE + 0.5);
    let pane = &rt.state.panes[0];
    assert!(!pane.is_placeholder("a.txt"), "now the listing's own row");
    assert_eq!(pane.entry("a.txt").and_then(|e| e.size), Some(3));
    assert_eq!(pane.order(), ["a.txt", "b.txt"], "same place, no jump");
    assert!(rt.state.flights.is_empty());
}

#[test]
fn a_finished_job_clears_its_rows() {
    let (mut rt, _) = setup();
    event(&mut rt, started("/dl/gone.txt"));
    at(&mut rt, 1.0);
    assert!(rt.state.panes[0].is_placeholder("gone.txt"));
    event(&mut rt, Event::State(JOB, JobState::Cancelled));
    at(&mut rt, 1.1);
    assert!(rt.state.flights.is_empty(), "a cancelled file's row goes (no pane re-list is pending)");
    assert!(rt.state.panes[0].entry("gone.txt").is_none());
}

#[test]
fn transfer_temp_files_never_show() {
    use crate::pane::model::is_transfer_temp;
    for n in [".big.iso.filewinch-part", ".big.iso.filewinch-ranges", ".big.iso.filewinch-old", ".a.txt.filewinch-4242-3"] {
        assert!(is_transfer_temp(n), "{n}");
    }
    for n in ["filewinch-notes.txt", ".hidden", "a.filewinch-part"] {
        assert!(!is_transfer_temp(n), "{n}");
    }
    let (mut rt, remote) = setup();
    put(&remote, "/dl/.c.bin.filewinch-part");
    put(&remote, "/dl/.profile");
    rt.update(|s, _| {
        s.set_show_hidden(true);
        s.panes[0].refresh(true);
    });
    at(&mut rt, 1.0);
    assert_eq!(rt.state.panes[0].order(), [".profile", "b.txt"], "hidden files show; the transfer's own don't");
}

#[test]
fn a_watched_folder_relists_when_another_app_adds_a_file() {
    let dir = std::env::temp_dir().join(format!("filewinch-watch-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())));
    std::fs::create_dir_all(&dir).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(dir.clone());
    let dir = dir.canonicalize().unwrap();
    let mut ui = Ui::new();
    ui.watches.enabled = true;
    ui.panes[0].switch_to(Source::local(), filewinch_local::rpath(&dir), false);
    let mut rt = Runtime::new(ui, crate::view::view, (1320.0, 820.0), Env::new(false, 1.0));
    at(&mut rt, 0.0);
    // Arming runs through the inbox.
    for f in rt.state.inbox.drain() {
        f(&mut rt.state);
    }
    assert!(rt.state.watches.is_armed(0), "the shown folder is watched");
    assert!(!rt.state.watches.is_armed(1), "an empty pane watches nothing");
    // FSEvents can take a moment to start delivering.
    std::thread::sleep(std::time::Duration::from_millis(300));
    std::fs::write(dir.join("from-another-app.txt"), b"hi").unwrap();
    let start = std::time::Instant::now();
    let mut t = 1.0;
    while rt.state.panes[0].entry("from-another-app.txt").is_none() && start.elapsed() < std::time::Duration::from_secs(10) {
        std::thread::sleep(std::time::Duration::from_millis(50));
        t += 0.1;
        at(&mut rt, t);
    }
    assert!(rt.state.panes[0].entry("from-another-app.txt").is_some(), "listed without a manual refresh");
    // Leaving This Mac drops the watch.
    rt.update(|s, _| s.panes[0].clear(false));
    at(&mut rt, t + 0.1);
    assert!(!rt.state.watches.is_armed(0));
}
