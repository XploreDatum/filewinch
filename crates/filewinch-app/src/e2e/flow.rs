//! The round trip every protocol runs: list, upload a tree, download it back, compare bytes,
//! rename and move, delete recursively, and answer conflict questions (Skip, Overwrite, Rename).

use std::sync::Arc;

use filewinch_core::{ErrorKind, RPath, Remote};
use filewinch_local::LocalRemote;

use super::harness::Harness;
use super::tree::{self, assert_same, expected_listing, list_remote, read_remote, snapshot_local};
use super::{Live, LocalDir, SEEDED, SERVER_TREE};
use crate::transfers::engine::{ConflictAnswer, ConflictChoice, ConflictPolicy, Job, JobState, Op};

pub fn job(op: Op, src: &Arc<dyn Remote>, paths: Vec<RPath>, dst: Option<(&Arc<dyn Remote>, RPath)>, conflict: ConflictPolicy, label: &str) -> Job {
    Job {
        op,
        src: src.clone(),
        src_paths: paths,
        dst: dst.as_ref().map(|(r, _)| (*r).clone()),
        dst_dir: dst.map(|(_, d)| d),
        conflict,
        label: label.into(),
        delete_shared_contents: false,
    }
}

#[track_caller]
fn assert_gone(remote: &dyn Remote, path: &RPath, what: &str) {
    match remote.stat(path) {
        Err(e) if e.kind == ErrorKind::NotFound => {}
        other => panic!("{what}: {path} should be gone, stat gave {other:?}"),
    }
}

/// Runs the whole round trip against a connected server.
pub fn round_trip(live: &Live) {
    let name = format!("{:?}", live.site.protocol);
    let (remote, home) = (&live.remote, live.home.clone());
    let local: Arc<dyn Remote> = Arc::new(LocalRemote::new());
    let h = Harness::new();

    // Listing: the file put on the server's disk shows up with its size.
    let listed = remote.list(&home).unwrap_or_else(|e| panic!("{name}: list home: {e:?}"));
    let seeded = listed.iter().find(|e| e.name == SEEDED.0).unwrap_or_else(|| panic!("{name}: {} not listed: {listed:?}", SEEDED.0));
    assert_eq!(seeded.size, Some(SEEDED.1.len() as u64), "{name}: seeded file size");
    assert!(!seeded.is_dir(), "{name}: the seeded file is a file");

    // Download a nested folder made on the server and compare every byte.
    let down = LocalDir::new("e2e-down");
    let label = format!("Downloading “{SERVER_TREE}” from {name}");
    h.run(job(Op::Copy, remote, vec![home.join(SERVER_TREE)], Some((&local, down.rpath())), ConflictPolicy::Ask, &label)).assert_done(&label);
    let on_server = snapshot_local(&live.server_dir.join(SERVER_TREE));
    assert_same(&format!("{name}: downloaded {SERVER_TREE}"), &on_server, &snapshot_local(&down.0.join(SERVER_TREE)));

    // Upload a tree. Where the server can't make folders (S3 test server), a flat folder
    // becomes a new bucket beside the site's one.
    let nested = live.folder_markers;
    let up = LocalDir::new("e2e-up");
    let src_root = up.0.join("tree");
    tree::write_fixture(&src_root, nested);
    let want = snapshot_local(&src_root);
    let parent = if nested { home.clone() } else { RPath::root() };
    let remote_tree = parent.join("tree");
    let label = format!("Uploading “tree” to {name}");
    h.run(job(Op::Copy, &local, vec![up.rpath().join("tree")], Some((remote, parent.clone())), ConflictPolicy::Ask, &label)).assert_done(&label);
    assert_eq!(list_remote(&**remote, &remote_tree), expected_listing(&want), "{name}: the server's listing after upload");

    // Download it back into a fresh folder and compare every byte.
    let label = format!("Downloading “tree” from {name}");
    h.run(job(Op::Copy, remote, vec![remote_tree.clone()], Some((&local, down.rpath())), ConflictPolicy::Ask, &label)).assert_done(&label);
    assert_same(&format!("{name}: downloaded tree"), &want, &snapshot_local(&down.0.join("tree")));

    // Rename, where the app offers it (server-side rename), then an engine Move into a folder.
    if remote.caps().server_rename {
        let (from, to) = (remote_tree.join("readme.md"), remote_tree.join("README renamed.md"));
        remote.rename(&from, &to).unwrap_or_else(|e| panic!("{name}: rename file: {e:?}"));
        assert_gone(&**remote, &from, &name);
        assert_eq!(read_remote(&**remote, &to), want["readme.md"].clone().expect("file"), "{name}: renamed file content");
        let (from, to) = (home.join(SERVER_TREE), home.join("renamed folder"));
        remote.rename(&from, &to).unwrap_or_else(|e| panic!("{name}: rename folder: {e:?}"));
        assert_gone(&**remote, &from, &name);
        assert_eq!(read_remote(&**remote, &to.join("site/assets/css/style.css")), b"body { margin: 0 }\n", "{name}: file inside renamed folder");
        remote.rename(&to, &from).unwrap_or_else(|e| panic!("{name}: rename folder back: {e:?}"));
    }
    let unicode = remote_tree.join(tree::UNICODE_NAME);
    let move_to = if nested { remote_tree.join("media") } else { home.clone() };
    let label = format!("Moving “{}” on {name}", tree::UNICODE_NAME);
    h.run(job(Op::Move, remote, vec![unicode.clone()], Some((remote, move_to.clone())), ConflictPolicy::Ask, &label)).assert_done(&label);
    assert_gone(&**remote, &unicode, &name);
    let moved = move_to.join(tree::UNICODE_NAME);
    assert_eq!(read_remote(&**remote, &moved), want[tree::UNICODE_NAME].clone().expect("file"), "{name}: moved file content");

    // Delete recursively: the uploaded tree, the server's nested folder, the moved file.
    let mut gone = vec![remote_tree.clone(), home.join(SERVER_TREE)];
    if !nested {
        gone.push(moved);
    }
    let label = format!("Deleting from {name}");
    h.run(job(Op::Delete, remote, gone.clone(), None, ConflictPolicy::Skip, &label)).assert_done(&label);
    for p in &gone {
        assert_gone(&**remote, p, &name);
    }
    let names: Vec<String> = remote.list(&home).expect("list home after delete").into_iter().map(|e| e.name).collect();
    assert_eq!(names, vec![SEEDED.0.to_string()], "{name}: only the seeded file is left");

    conflicts(&h, &name, &local, remote, &home);
}

/// Uploading over an existing file with ConflictPolicy::Ask: Skip, Overwrite, then Rename.
fn conflicts(h: &Harness, name: &str, local: &Arc<dyn Remote>, remote: &Arc<dyn Remote>, home: &RPath) {
    let dir = LocalDir::new("e2e-conflict");
    let file = dir.0.join("conflict.txt");
    let src = vec![dir.rpath().join("conflict.txt")];
    let target = home.join("conflict.txt");
    let upload = |label: &str| job(Op::Copy, local, src.clone(), Some((remote, home.clone())), ConflictPolicy::Ask, label);
    let answer = |choice| ConflictAnswer { choice, apply_to_all: false };

    std::fs::write(&file, b"version one\n").expect("write");
    h.run(upload("first upload")).assert_done(&format!("{name}: first upload"));
    assert_eq!(read_remote(&**remote, &target), b"version one\n");

    std::fs::write(&file, b"version two, longer\n").expect("write");
    let (id, ask) = h.run_until_conflict(upload("upload over, skip"));
    assert_eq!(ask.dst.name, "conflict.txt", "{name}: the question names the file");
    assert_eq!(ask.dst.size, Some(12), "{name}: the question shows the server's size");
    assert_eq!(ask.src.size, Some(20), "{name}: the question shows the new size");
    assert!(matches!(h.engine.snapshot().iter().find(|j| j.0 == id).map(|j| &j.2), Some(JobState::WaitingForAnswer)), "{name}: job waits for the answer");
    h.answer(id, answer(ConflictChoice::Skip)).assert_done(&format!("{name}: skip"));
    assert_eq!(read_remote(&**remote, &target), b"version one\n", "{name}: Skip leaves the server's copy");

    let (id, _) = h.run_until_conflict(upload("upload over, overwrite"));
    h.answer(id, answer(ConflictChoice::Overwrite)).assert_done(&format!("{name}: overwrite"));
    assert_eq!(read_remote(&**remote, &target), b"version two, longer\n", "{name}: Overwrite replaces it");

    std::fs::write(&file, b"version three\n").expect("write");
    let (id, _) = h.run_until_conflict(upload("upload over, keep both"));
    h.answer(id, answer(ConflictChoice::Rename)).assert_done(&format!("{name}: rename"));
    assert_eq!(read_remote(&**remote, &target), b"version two, longer\n", "{name}: Rename keeps the original");
    assert_eq!(read_remote(&**remote, &home.join("conflict 2.txt")), b"version three\n", "{name}: Rename adds “conflict 2.txt”");

    let label = format!("{name}: clean up");
    h.run(job(Op::Delete, remote, vec![target, home.join("conflict 2.txt")], None, ConflictPolicy::Skip, &label)).assert_done(&label);
}
