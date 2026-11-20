//! OneDrive shortcuts ("Add shortcut to My files") and file hashes, against a mock Graph.
//! A shortcut is the user's own item pointing at a folder in the sharer's drive: Filewinch
//! must show it as a link, browse through it, and never delete the sharer's folder.

#[allow(dead_code)] // shared with onedrive.rs, which uses the rest
mod mock_onedrive;
mod support;

use std::sync::Arc;
use std::sync::mpsc::channel;
use std::time::Duration;

use filewinch_cloud::{OneDriveConnector, Provider, Tuning};
use filewinch_core::{Checksum, Connector, EntryKind, ErrorKind, HashAlgo, Protocol, RPath, Remote};
use filewinch_engine::{ConflictPolicy, Engine, EngineConfig, Event, Job, JobState, Op};
use mock_onedrive::{Graph, MY_DRIVE, OTHER_DRIVE};
use serde_json::json;

fn connect(g: &Graph) -> Arc<dyn Remote> {
    let tuning = Tuning { page_size: 2, ..Tuning::for_provider(Provider::OneDrive) };
    let c = OneDriveConnector::with_options(support::options(&g.mock, tuning));
    Arc::from(c.connect(&support::site(Protocol::OneDrive), support::secrets()).unwrap())
}

fn p(s: &str) -> RPath {
    RPath::new(s)
}

struct Seeded {
    team: String,
    shortcut: String,
}

/// The sharer's drive: Team/{plan.txt, Sub/{deep.txt}}. My files: Mine/, and a shortcut
/// "Team" → their Team folder.
fn seed(g: &Graph) -> Seeded {
    let mut st = g.state.lock().unwrap();
    let team = st.add(OTHER_DRIVE, Some("other-root"), "Team", true, &[]);
    st.add(OTHER_DRIVE, Some(&team), "plan.txt", false, b"the plan");
    let sub = st.add(OTHER_DRIVE, Some(&team), "Sub", true, &[]);
    st.add(OTHER_DRIVE, Some(&sub), "deep.txt", false, b"deep");
    st.add(MY_DRIVE, Some("root"), "Mine", true, &[]);
    let shortcut = st.add_shortcut("root", "Team", &team);
    Seeded { team, shortcut }
}

/// Runs one Delete job through the real engine and returns its final state.
fn engine_delete(r: Arc<dyn Remote>, path: &str) -> JobState {
    run_job(r.clone(), Op::Delete, path, None).0
}

/// Runs one job through the real engine; returns its final state and the item errors.
fn run_job(r: Arc<dyn Remote>, op: Op, path: &str, dst_dir: Option<&str>) -> (JobState, Vec<filewinch_core::Error>) {
    let (tx, rx) = channel();
    let cfg = EngineConfig { retry_delay: Duration::from_millis(10), ..EngineConfig::default() };
    let engine = Engine::new(cfg, move |e| {
        let _ = tx.send(e);
    });
    let id = engine.enqueue(Job {
        op,
        src: r.clone(),
        src_paths: vec![p(path)],
        dst: dst_dir.map(|_| r),
        dst_dir: dst_dir.map(p),
        conflict: ConflictPolicy::Overwrite,
        label: "Test job".into(),
        delete_shared_contents: false,
    });
    let mut failures = Vec::new();
    loop {
        match rx.recv_timeout(Duration::from_secs(30)).expect("the job finished") {
            Event::State(j, s) if j == id && s.is_finished() => return (s, failures),
            Event::ItemFailed { job, error, .. } if job == id => failures.push(error),
            _ => {}
        }
    }
}

fn sharer_tree_intact(g: &Graph, s: &Seeded) {
    let st = g.state.lock().unwrap();
    assert!(st.items.contains_key(&s.team), "the sharer's folder must survive");
    let kids: Vec<String> = st.children(&s.team).into_iter().map(|i| i.name).collect();
    assert_eq!(kids, ["Sub", "plan.txt"], "the sharer's files must survive");
    let sub = st.find(&s.team, "Sub").unwrap();
    assert_eq!(st.children(&sub.id).len(), 1);
}

#[test]
fn shortcut_is_a_link_that_still_browses() {
    let g = Graph::start(false);
    let s = seed(&g);
    let r = connect(&g);
    let listed = r.list(&p("/My files")).unwrap();
    let kinds: Vec<_> = listed.iter().map(|e| (e.name.as_str(), e.kind)).collect();
    assert_eq!(kinds, [("Mine", EntryKind::Dir), ("Team", EntryKind::Symlink)]);
    let team = &listed[1];
    assert_eq!(team.size, None);
    assert!(team.checksums.is_empty());
    assert!(team.extra.iter().any(|(k, v)| k == "OneDrive ID" && *v == s.shortcut), "{:?}", team.extra);

    // stat doesn't follow: still a link.
    assert_eq!(r.stat(&p("/My files/Team")).unwrap().kind, EntryKind::Symlink);
    // The pane opens a link by listing it: that lists the sharer's folder.
    let inside: Vec<_> = r.list(&p("/My files/Team")).unwrap().into_iter().map(|e| (e.name, e.kind)).collect();
    assert_eq!(inside, [("Sub".to_string(), EntryKind::Dir), ("plan.txt".to_string(), EntryKind::File)]);
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut r.read(&p("/My files/Team/plan.txt"), 0).unwrap(), &mut buf).unwrap();
    assert_eq!(buf, b"the plan");
    // The listing went to the sharer's drive.
    assert!(g.mock.requests().iter().any(|q| q.path == format!("/api/drives/{OTHER_DRIVE}/items/{}/children", s.team)));
}

#[test]
fn engine_delete_of_shortcut_removes_only_the_shortcut() {
    let g = Graph::start(false);
    let s = seed(&g);
    let r = connect(&g);
    let state = engine_delete(r.clone(), "/My files/Team");
    assert!(matches!(state, JobState::Done), "{state:?}");
    {
        let st = g.state.lock().unwrap();
        assert!(!st.items.contains_key(&s.shortcut), "the shortcut itself is gone");
    }
    sharer_tree_intact(&g, &s);
    // No DELETE ever touched the sharer's drive.
    let deletes: Vec<_> = g.mock.requests().into_iter().filter(|q| q.method == "DELETE").map(|q| q.path).collect();
    assert_eq!(deletes, [format!("/api/drives/{MY_DRIVE}/items/{}", s.shortcut)]);
    let names: Vec<_> = r.list(&p("/My files")).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["Mine"]);
}

#[test]
fn shortcut_remove_dir_rename_and_mtime_never_touch_the_target() {
    let g = Graph::start(false);
    let s = seed(&g);
    let r = connect(&g);
    // A link isn't a folder: remove_dir refuses instead of deleting the sharer's folder.
    let e = r.remove_dir(&p("/My files/Team")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidPath);
    sharer_tree_intact(&g, &s);

    // Renaming or moving the shortcut changes the shortcut, not the sharer's folder.
    r.rename(&p("/My files/Team"), &p("/My files/Mine/Team (shared)")).unwrap();
    {
        let st = g.state.lock().unwrap();
        assert_eq!(st.items[&s.team].name, "Team");
        assert_eq!(st.items[&s.team].parent.as_deref(), Some("other-root"));
        assert_eq!(st.items[&s.shortcut].name, "Team (shared)");
    }
    assert_eq!(r.stat(&p("/My files/Mine/Team (shared)")).unwrap().kind, EntryKind::Symlink);
    let patched: Vec<_> = g.mock.requests().into_iter().filter(|q| q.method == "PATCH").map(|q| q.path).collect();
    assert!(patched.iter().all(|x| x.ends_with(&format!("/items/{}", s.shortcut))), "{patched:?}");

    // Deleting it by path deletes only the shortcut.
    r.remove_file(&p("/My files/Mine/Team (shared)")).unwrap();
    sharer_tree_intact(&g, &s);
    assert_eq!(r.stat(&p("/My files/Mine/Team (shared)")).unwrap_err().kind, ErrorKind::NotFound);
}

#[test]
fn deleting_inside_a_shortcut_is_an_ordinary_delete_in_the_shared_folder() {
    // Browsing into the shortcut and deleting a file there is the user's explicit choice and
    // goes to the sharer's drive (Graph enforces their permissions).
    let g = Graph::start(false);
    let s = seed(&g);
    let r = connect(&g);
    r.remove_file(&p("/My files/Team/plan.txt")).unwrap();
    let st = g.state.lock().unwrap();
    assert!(st.find(&s.team, "plan.txt").is_none());
    assert!(st.items.contains_key(&s.team));
}

#[test]
fn file_hashes_come_through() {
    let g = Graph::start(false);
    {
        let mut st = g.state.lock().unwrap();
        let both = st.add(MY_DRIVE, Some("root"), "both.txt", false, b"Hello, World!");
        st.items.get_mut(&both).unwrap().hashes = Some(json!({
            "sha1Hash": "0A0A9F2A6772942557AB5355D76AF442F8F65E01",
            "quickXorHash": "SCgDG9jwBhaA4ApvnQMbyBACAAA=",
        }));
        // Business drives send only quickXorHash; a malformed one must be dropped.
        let bad = st.add(MY_DRIVE, Some("root"), "bad.txt", false, b"x");
        st.items.get_mut(&bad).unwrap().hashes = Some(json!({"sha1Hash": "abc", "quickXorHash": "not base64!"}));
        st.add(MY_DRIVE, Some("root"), "none.txt", false, b"y");
    }
    let r = connect(&g);
    let want = vec![
        Checksum::new(HashAlgo::Sha1, "0a0a9f2a6772942557ab5355d76af442f8f65e01").unwrap(),
        Checksum::new(HashAlgo::QuickXor, "4828031bd8f0061680e00a6f9d031bc810020000").unwrap(),
    ];
    let listed = r.list(&p("/My files")).unwrap();
    let by = |n: &str| listed.iter().find(|e| e.name == n).unwrap().checksums.clone();
    assert_eq!(by("both.txt"), want);
    assert!(by("bad.txt").is_empty());
    assert!(by("none.txt").is_empty());
    assert_eq!(r.stat(&p("/My files/both.txt")).unwrap().checksums, want);
    assert!(r.stat(&p("/My files")).unwrap().checksums.is_empty());
}

#[test]
fn engine_copy_of_a_shortcut_explains_itself_and_changes_nothing() {
    let g = Graph::start(false);
    let s = seed(&g);
    let r = connect(&g);
    let (state, failures) = run_job(r.clone(), Op::Copy, "/My files/Team", Some("/My files/Mine"));
    assert!(matches!(state, JobState::Failed(_)), "{state:?}");
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].kind, ErrorKind::Unsupported);
    assert!(failures[0].message.contains("Open it and copy what's inside"), "{}", failures[0].message);
    sharer_tree_intact(&g, &s);
    assert!(r.list(&p("/My files/Mine")).unwrap().is_empty());
}
