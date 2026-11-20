//! The per-file events the app's live folders follow: `ItemStarted` then `ItemDone` for each
//! file written, moved or deleted (with the destination path), and no `ItemDone` for a skip.

mod common;

use common::*;
use filewinch_core::{RPath, Remote};
use filewinch_engine::*;
use std::sync::Arc;

fn mem() -> (Arc<Mem>, Arc<dyn Remote>) {
    let m = Mem::new(full_caps());
    let r: Arc<dyn Remote> = Arc::new(MemRemote(m.clone()));
    (m, r)
}

fn job(op: Op, src: &Arc<dyn Remote>, paths: &[&str], dst: Option<(&Arc<dyn Remote>, &str)>, conflict: ConflictPolicy) -> Job {
    Job {
        op,
        src: src.clone(),
        src_paths: paths.iter().map(|p| RPath::new(p)).collect(),
        dst: dst.map(|d| d.0.clone()),
        dst_dir: dst.map(|d| RPath::new(d.1)),
        conflict,
        label: "test".into(),
        delete_shared_contents: false,
    }
}

/// (started, done) paths for job `id`, in order, and each done's `from`.
fn items(h: &Harness, id: JobId) -> (Vec<String>, Vec<(String, Option<String>)>) {
    let mut started = Vec::new();
    let mut done = Vec::new();
    for e in &h.log {
        match e {
            Event::ItemStarted { job, path, .. } if *job == id => started.push(path.to_string()),
            Event::ItemDone { job, path, from } if *job == id => done.push((path.to_string(), from.as_ref().map(|f| f.to_string()))),
            _ => {}
        }
    }
    started.sort();
    done.sort();
    (started, done)
}

#[test]
fn a_copy_reports_each_file_at_its_destination() {
    let (m, src) = mem();
    let (dm, dst) = mem();
    m.put("/src/a.txt", &bytes(1000, 1));
    m.put("/src/sub/b.bin", &bytes(300_000, 2));
    dm.mkdirs("/dl");
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(job(Op::Copy, &src, &["/src/a.txt", "/src/sub"], Some((&dst, "/dl")), ConflictPolicy::Overwrite));
    assert_done(&h.wait(id));
    let (started, done) = items(&h, id);
    assert_eq!(started, ["/dl/a.txt", "/dl/sub/b.bin"]);
    assert_eq!(done, [("/dl/a.txt".to_owned(), None), ("/dl/sub/b.bin".to_owned(), None)]);
}

#[test]
fn a_move_across_remotes_names_its_source() {
    let (m, src) = mem();
    let (dm, dst) = mem();
    m.put("/src/b.txt", &bytes(10, 2));
    dm.mkdirs("/in");
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(job(Op::Move, &src, &["/src/b.txt"], Some((&dst, "/in")), ConflictPolicy::Overwrite));
    assert_done(&h.wait(id));
    assert_eq!(items(&h, id).1, [("/in/b.txt".to_owned(), Some("/src/b.txt".to_owned()))]);
}

#[test]
fn a_skipped_file_has_no_item_done() {
    let (m, r) = mem();
    m.put("/src/a.txt", &bytes(10, 1));
    m.put("/src/new.txt", &bytes(10, 3));
    m.put("/dl/a.txt", &bytes(10, 2));
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(job(Op::Copy, &r, &["/src/a.txt", "/src/new.txt"], Some((&r, "/dl")), ConflictPolicy::Skip));
    assert_done(&h.wait(id));
    let (started, done) = items(&h, id);
    assert_eq!(started, ["/dl/a.txt", "/dl/new.txt"], "both were taken up");
    assert_eq!(done, [("/dl/new.txt".to_owned(), None)], "only the written one is done");
    assert_eq!(m.get("/dl/a.txt"), Some(bytes(10, 2)), "the skipped file is untouched");
}

#[test]
fn a_delete_reports_the_removed_files() {
    let (m, r) = mem();
    m.put("/trash/a.txt", &bytes(10, 1));
    m.put("/trash/in/b.txt", &bytes(10, 2));
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(job(Op::Delete, &r, &["/trash"], None, ConflictPolicy::Overwrite));
    assert_done(&h.wait(id));
    let (started, done) = items(&h, id);
    assert_eq!(started, ["/trash/a.txt", "/trash/in/b.txt"]);
    assert_eq!(done.iter().map(|d| d.0.as_str()).collect::<Vec<_>>(), ["/trash/a.txt", "/trash/in/b.txt"]);
}

#[test]
fn a_failed_file_is_done_and_failed() {
    let (m, r) = mem();
    m.put("/src/a.txt", &bytes(10, 1));
    m.put("/dl/a.txt/inside", &bytes(1, 1)); // a folder is in the way
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(job(Op::Copy, &r, &["/src/a.txt"], Some((&r, "/dl")), ConflictPolicy::Overwrite));
    let _ = h.wait(id);
    let (_, done) = items(&h, id);
    assert_eq!(done, [("/dl/a.txt".to_owned(), None)], "its row can go: ItemFailed says why");
    assert!(h.log.iter().any(|e| matches!(e, Event::ItemFailed { job, .. } if *job == id)));
}
