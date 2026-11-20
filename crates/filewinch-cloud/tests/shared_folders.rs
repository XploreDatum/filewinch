//! Shared folders on every cloud drive (core request E-1): each backend marks shares with
//! `Entry::shared`, and the engine's Delete leaves or refuses them instead of emptying them for
//! everyone they're shared with.

mod mock_box;
#[allow(dead_code)]
mod mock_dropbox;
#[allow(dead_code)]
mod mock_gdrive;
#[allow(dead_code)]
mod mock_onedrive;
mod support;

use std::sync::Arc;
use std::sync::mpsc::channel;
use std::time::Duration;

use filewinch_cloud::{BoxConnector, DropboxConnector, GoogleDriveConnector, OneDriveConnector, Provider, Tuning};
use filewinch_core::{Connector, Error, ErrorKind, Protocol, RPath, Remote, Shared};
use filewinch_engine::{ConflictPolicy, Engine, EngineConfig, Event, Job, JobState, Op};

const LEAVABLE: Option<Shared> = Some(Shared { owned_by_me: false, can_leave: true });
const MINE: Option<Shared> = Some(Shared { owned_by_me: true, can_leave: false });
const STUCK: Option<Shared> = Some(Shared { owned_by_me: false, can_leave: false });

fn p(s: &str) -> RPath {
    RPath::new(s)
}

fn tuning(provider: Provider) -> Tuning {
    Tuning { page_size: 2, ..Tuning::for_provider(provider) }
}

fn connect(c: &dyn Connector, protocol: Protocol) -> Arc<dyn Remote> {
    Arc::from(c.connect(&support::site(protocol), support::secrets()).unwrap())
}

/// Runs one Delete job through the real engine; returns its final state and the item errors.
fn delete(r: &Arc<dyn Remote>, paths: &[&str], delete_shared_contents: bool) -> (JobState, Vec<Error>) {
    let (tx, rx) = channel();
    let cfg = EngineConfig { retry_delay: Duration::from_millis(10), ..EngineConfig::default() };
    let engine = Engine::new(cfg, move |e| {
        let _ = tx.send(e);
    });
    let id = engine.enqueue(Job {
        op: Op::Delete,
        src: r.clone(),
        src_paths: paths.iter().map(|s| p(s)).collect(),
        dst: None,
        dst_dir: None,
        conflict: ConflictPolicy::Skip,
        label: "Delete".into(),
        delete_shared_contents,
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

fn shared_of(r: &dyn Remote, dir: &str, name: &str) -> Option<Shared> {
    r.list(&p(dir)).unwrap().into_iter().find(|e| e.name == name).unwrap_or_else(|| panic!("{name} listed")).shared
}

// ---- Dropbox ---------------------------------------------------------------------------

fn dropbox() -> (mock_dropbox::FakeDropbox, Arc<dyn Remote>) {
    let fake = mock_dropbox::FakeDropbox::start(false);
    {
        let mut st = fake.state.lock().unwrap();
        st.put_file("/Design/logo.svg", b"<svg/>");
        st.put_file("/Design/Drafts/v1.svg", b"v1");
        st.put_file("/Mine/plan.txt", b"plan");
        st.put_file("/Projects/a.txt", b"a");
        st.put_file("/Projects/Team/notes.txt", b"n");
        st.put_file("/Plain/x.txt", b"x");
        st.shares.insert("/design".into(), ("sf-1".into(), "editor".into()));
        st.shares.insert("/mine".into(), ("sf-2".into(), "owner".into()));
        st.shares.insert("/projects/team".into(), ("sf-3".into(), "editor".into()));
    }
    let r = connect(
        &DropboxConnector::with_options(support::options(&fake.mock, tuning(Provider::Dropbox))),
        Protocol::Dropbox,
    );
    (fake, r)
}

#[test]
fn dropbox_marks_share_roots_with_ownership() {
    let (_fake, r) = dropbox();
    assert_eq!(shared_of(&*r, "/", "Design"), LEAVABLE);
    assert_eq!(shared_of(&*r, "/", "Mine"), MINE);
    assert_eq!(shared_of(&*r, "/", "Plain"), None);
    // Inside a share, items are ordinary: only the root is the share.
    assert_eq!(shared_of(&*r, "/Design", "Drafts"), None);
    assert_eq!(shared_of(&*r, "/Design", "logo.svg"), None);
    assert_eq!(r.stat(&p("/Design")).unwrap().shared, LEAVABLE);
}

#[test]
fn dropbox_delete_of_a_joined_share_leaves_it_and_deletes_nothing() {
    let (fake, r) = dropbox();
    let (state, failures) = delete(&r, &["/Design"], false);
    assert!(matches!(state, JobState::Done), "{state:?} {failures:?}");
    assert_eq!(fake.state.lock().unwrap().left, [("sf-1".to_string(), false)]);
    assert_eq!(fake.mock.count("POST", "/api/files/delete_v2"), 0, "nothing deleted for the other members");
}

#[test]
fn dropbox_delete_of_an_owned_share_is_refused_unless_confirmed() {
    let (fake, r) = dropbox();
    let (state, failures) = delete(&r, &["/Mine"], false);
    assert!(matches!(state, JobState::Failed(_)), "{state:?}");
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].kind, ErrorKind::PermissionDenied);
    assert_eq!(
        failures[0].message,
        "“Mine” is shared with others. Filewinch won't delete files inside a shared folder. Remove it from the provider's website if you own it."
    );
    assert_eq!(fake.mock.count("POST", "/api/files/delete_v2"), 0);
    assert_eq!(fake.mock.count("POST", "/api/sharing/relinquish_folder_membership"), 0);
    assert!(fake.state.lock().unwrap().file("/Mine/plan.txt").is_some());

    // The user ticked "delete the contents for everyone": now it goes.
    let (state, failures) = delete(&r, &["/Mine"], true);
    assert!(matches!(state, JobState::Done), "{state:?} {failures:?}");
    assert!(fake.state.lock().unwrap().file("/Mine/plan.txt").is_none());
}

#[test]
fn dropbox_share_nested_in_a_deleted_folder_is_left_whole() {
    let (fake, r) = dropbox();
    // Even with the opt-in: it covers the shares the user selected, not ones found inside.
    let (state, failures) = delete(&r, &["/Projects"], true);
    assert!(matches!(state, JobState::Failed(_)), "{state:?}");
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].message.starts_with("“Team” in “Projects” is shared with others"), "{}", failures[0].message);
    let st = fake.state.lock().unwrap();
    assert!(st.file("/Projects/a.txt").is_none(), "the ordinary file went");
    assert!(st.file("/Projects/Team/notes.txt").is_some(), "the share wasn't emptied");
    assert!(st.nodes.contains_key("/projects"), "its parent stays to hold it");
    assert!(st.left.is_empty(), "a nested share isn't left either");
}

#[test]
fn dropbox_leave_waits_for_an_async_job_and_explains_owner_refusal() {
    let (fake, r) = dropbox();
    fake.state.lock().unwrap().leave_async_polls = Some(2);
    r.leave_shared(&p("/Design")).unwrap();
    assert_eq!(fake.mock.count("POST", "/api/sharing/check_job_status"), 3);

    let e = r.leave_shared(&p("/Mine")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::PermissionDenied);
    assert!(e.message.contains("Owners can't leave their own shared folder"), "{}", e.message);
    let e = r.leave_shared(&p("/Plain")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidPath);
}

// ---- Box -------------------------------------------------------------------------------

#[test]
fn box_marks_collaborations_and_leaves_by_dropping_its_own() {
    let (mock, fake) = mock_box::FakeBox::start();
    let (design, mine, logo) = {
        let mut st = fake.st();
        let design = st.add("0", "Design", true, &[]);
        let logo = st.add(&design, "logo.svg", false, b"<svg/>");
        st.owners.insert(logo.clone(), "u-ann".into());
        let mine = st.add("0", "Mine", true, &[]);
        st.add(&mine, "plan.txt", false, b"plan");
        st.add("0", "Plain", true, &[]);
        st.collaborate(&design, "u-ann", mock_box::ME);
        st.collaborate(&design, "u-ann", "u-cho");
        st.collaborate(&mine, mock_box::ME, "u-bob");
        (design, mine, logo)
    };
    let r = connect(&BoxConnector::with_options(support::options(&mock, tuning(Provider::Box))), Protocol::Box);
    assert_eq!(shared_of(&*r, "/", "Design"), LEAVABLE);
    assert_eq!(shared_of(&*r, "/", "Mine"), MINE);
    assert_eq!(shared_of(&*r, "/", "Plain"), None);
    assert_eq!(shared_of(&*r, "/Design", "logo.svg"), None);

    let (state, failures) = delete(&r, &["/Design", "/Mine"], false);
    assert!(matches!(state, JobState::Failed(_)), "{state:?}");
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].message.starts_with("“Mine” is shared with others."), "{}", failures[0].message);
    let st = fake.st();
    assert!(st.nodes.values().all(|n| !n.trashed), "nothing was trashed");
    // Only the user's own collaboration went; Cho still has Design, and Ann still owns it.
    let left: Vec<_> = st.collabs.iter().map(|c| (c.1.clone(), c.2.clone())).collect();
    assert_eq!(left, [(design.clone(), "u-cho".to_string()), (mine, "u-bob".to_string())]);
    assert_ne!(st.nodes[&design].parent, "0", "Design left All Files");
    assert_eq!(st.nodes[&logo].parent, design);
    assert_eq!(mock.count("GET", "/api/users/me"), 1, "who-am-I is asked once");
}

// ---- OneDrive --------------------------------------------------------------------------

#[test]
fn onedrive_marks_shares_and_never_empties_them() {
    use mock_onedrive::{Graph, MY_DRIVE, OTHER_DRIVE};
    let g = Graph::start(false);
    {
        let mut st = g.state.lock().unwrap();
        let mine = st.add(MY_DRIVE, Some("root"), "Mine", true, &[]);
        st.add(MY_DRIVE, Some(&mine), "plan.txt", false, b"plan");
        st.shared_out.push(mine);
        let proj = st.add(MY_DRIVE, Some("root"), "Projects", true, &[]);
        st.add(MY_DRIVE, Some(&proj), "a.txt", false, b"a");
        let nested = st.add(MY_DRIVE, Some(&proj), "Team", true, &[]);
        st.add(MY_DRIVE, Some(&nested), "notes.txt", false, b"n");
        st.shared_out.push(nested);
        let theirs = st.add(OTHER_DRIVE, Some("other-root"), "Theirs", true, &[]);
        let inner = st.add(OTHER_DRIVE, Some(&theirs), "Inner", true, &[]);
        st.shared_out.push(inner);
        st.shared.push(theirs.clone());
        st.add_shortcut("root", "Theirs link", &theirs);
    }
    let r = connect(
        &OneDriveConnector::with_options(support::options(&g.mock, tuning(Provider::OneDrive))),
        Protocol::OneDrive,
    );
    assert_eq!(shared_of(&*r, "/My files", "Mine"), MINE);
    assert_eq!(shared_of(&*r, "/My files", "Projects"), None);
    assert_eq!(shared_of(&*r, "/My files", "Theirs link"), LEAVABLE);
    assert_eq!(shared_of(&*r, "/Shared with me", "Theirs"), STUCK);
    // Someone else's shared item inside their folder isn't the user's.
    assert_eq!(shared_of(&*r, "/Shared with me/Theirs", "Inner"), Some(Shared { owned_by_me: false, can_leave: false }));

    let (state, failures) = delete(&r, &["/My files/Mine", "/My files/Projects"], false);
    assert!(matches!(state, JobState::Failed(_)), "{state:?}");
    let msgs: Vec<_> = failures.iter().map(|e| e.message.as_str()).collect();
    assert_eq!(msgs.len(), 2, "{msgs:?}");
    assert!(msgs.iter().any(|m| m.starts_with("“Mine” is shared with others.")), "{msgs:?}");
    assert!(msgs.iter().any(|m| m.starts_with("“Team” in “Projects” is shared with others")), "{msgs:?}");
    {
        let st = g.state.lock().unwrap();
        assert!(st.at("Mine/plan.txt").is_some());
        assert!(st.at("Projects/Team/notes.txt").is_some());
        assert!(st.at("Projects/a.txt").is_none(), "the ordinary file went");
    }

    let e = r.leave_shared(&p("/Shared with me/Theirs")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Unsupported);
    assert!(e.message.contains("Remove it there on onedrive.com"), "{}", e.message);
    // Leaving a shortcut deletes only the shortcut.
    let (state, _) = delete(&r, &["/My files/Theirs link"], false);
    assert!(matches!(state, JobState::Done), "{state:?}");
    let st = g.state.lock().unwrap();
    assert!(st.at("Theirs link").is_none());
    assert!(st.items.values().any(|i| i.name == "Theirs" && i.drive == OTHER_DRIVE));
}

// ---- Google Drive ----------------------------------------------------------------------

#[test]
fn gdrive_marks_share_roots_and_refuses_to_trash_them() {
    use mock_gdrive::{FakeDrive, ROOT};
    let fake = FakeDrive::start();
    let mine = fake.add_folder("Mine", ROOT);
    let plan = fake.add_file("plan.txt", &mine, b"plan");
    fake.set_shared_out(&mine);
    fake.set_shared_out(&plan); // Drive reports inherited sharing on children too
    let theirs = fake.add_file("theirs.txt", ROOT, b"t");
    fake.set_not_mine(&theirs);
    fake.add_file("plain.txt", ROOT, b"p");
    let from = fake.add_file("from-bo.txt", "someone-else", b"hi");
    fake.set_shared(&from);
    fake.add_drive("drive-1", "Team");
    let spec = fake.add_folder("Specs", "drive-1");
    fake.set_shared_out(&spec);
    let r = connect(
        &GoogleDriveConnector::with_options(support::options(&fake.mock, tuning(Provider::GoogleDrive))),
        Protocol::GoogleDrive,
    );
    assert_eq!(shared_of(&*r, "/My Drive", "Mine"), MINE);
    assert_eq!(shared_of(&*r, "/My Drive/Mine", "plan.txt"), None, "inherited, not a share of its own");
    assert_eq!(shared_of(&*r, "/My Drive", "theirs.txt"), STUCK);
    assert_eq!(shared_of(&*r, "/My Drive", "plain.txt"), None);
    assert_eq!(shared_of(&*r, "/Shared with me", "from-bo.txt"), STUCK);
    assert_eq!(shared_of(&*r, "/Shared drives/Team", "Specs"), None, "shared drives belong to the team");

    let (state, failures) = delete(&r, &["/My Drive/Mine", "/My Drive/theirs.txt"], false);
    assert!(matches!(state, JobState::Failed(_)), "{state:?}");
    let msgs: Vec<_> = failures.iter().map(|e| e.message.as_str()).collect();
    assert_eq!(msgs.len(), 2, "{msgs:?}");
    assert!(msgs.contains(&"“theirs.txt” is shared with others. Filewinch won't delete a shared file. Remove it from the provider's website if you own it."), "{msgs:?}");
    assert!(fake.named("plan.txt").iter().all(|f| !f.trashed));
    assert!(fake.named("theirs.txt").iter().all(|f| !f.trashed));
    let e = r.leave_shared(&p("/Shared with me/from-bo.txt")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Unsupported);
    assert!(e.message.contains("drive.google.com"), "{}", e.message);
}
