//! Engine behaviour against in-memory and local-disk remotes.

mod common;

use common::*;
use filewinch_core::{Caps, ErrorKind, RPath, Remote};
use filewinch_engine::*;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

fn mem() -> (Arc<Mem>, Arc<dyn Remote>) {
    let m = Mem::new(full_caps());
    let r: Arc<dyn Remote> = Arc::new(MemRemote(m.clone()));
    (m, r)
}

fn job(op: Op, src: &Arc<dyn Remote>, paths: &[&str], dst: Option<&Arc<dyn Remote>>, dst_dir: Option<&str>, conflict: ConflictPolicy) -> Job {
    Job {
        op,
        src: src.clone(),
        src_paths: paths.iter().map(|p| RPath::new(p)).collect(),
        dst: dst.cloned(),
        dst_dir: dst_dir.map(RPath::new),
        conflict,
        label: "test".into(),
        delete_shared_contents: false,
    }
}

fn copy(src: &Arc<dyn Remote>, paths: &[&str], dst: &Arc<dyn Remote>, dir: &str, c: ConflictPolicy) -> Job {
    job(Op::Copy, src, paths, Some(dst), Some(dir), c)
}

fn sample_tree(m: &Mem) {
    m.put("/src/a.txt", &bytes(1000, 1));
    m.put("/src/empty.bin", b"");
    m.put("/src/sub/b.bin", &bytes(200_000, 2));
    m.put("/src/sub/deeper/c.bin", &bytes(70_001, 3));
    m.mkdirs("/src/emptydir");
}

#[test]
fn copies_a_tree_between_memory_and_disk_and_back() {
    let (a, ra) = mem();
    sample_tree(&a);
    let dir = scratch("roundtrip");
    let disk: Arc<dyn Remote> = Arc::new(Disk::new(&dir));
    let (b, rb) = mem();
    b.mkdirs("/back");

    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(copy(&ra, &["/src"], &disk, "/", ConflictPolicy::Ask));
    assert_done(&h.wait(id));
    assert_eq!(std::fs::read(dir.join("src/sub/b.bin")).unwrap(), bytes(200_000, 2));
    assert!(dir.join("src/emptydir").is_dir());

    let id = h.engine.enqueue(copy(&disk, &["/src"], &rb, "/back", ConflictPolicy::Ask));
    assert_done(&h.wait(id));
    assert_eq!(a.tree("/src"), b.tree("/back/src"));

    // mtime carried over (disk keeps sub-second precision on APFS; allow a second)
    let ma = a.mtime("/src/a.txt").unwrap();
    let mb = b.mtime("/back/src/a.txt").unwrap();
    let diff = ma.duration_since(mb).unwrap_or_else(|e| e.duration());
    assert!(diff < Duration::from_secs(1), "mtime drift {diff:?}");

    // final progress adds up
    let snap = h.engine.snapshot();
    let p = &snap.iter().find(|s| s.0 == id).unwrap().3;
    assert_eq!(p.files_done, 4);
    assert_eq!(p.files_total, Some(4));
    assert_eq!(p.bytes_done, 271_001);
    assert_eq!(p.bytes_total, Some(271_001));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn scan_states_and_events_in_order() {
    let (a, ra) = mem();
    sample_tree(&a);
    let (_b, rb) = mem();
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(copy(&ra, &["/src"], &rb, "/", ConflictPolicy::Ask));
    h.wait(id);
    assert!(matches!(h.log[0], Event::Added(i) if i == id));
    let states = h.states(id);
    assert!(matches!(states[0], JobState::Queued));
    assert!(matches!(states[1], JobState::Scanning));
    assert!(states.iter().any(|s| matches!(s, JobState::Running)));
    // Progress arrives before the final state.
    let last_progress = h.log.iter().rposition(|e| matches!(e, Event::Progress(j, _) if *j == id)).unwrap();
    let done = h.log.iter().rposition(|e| matches!(e, Event::State(j, JobState::Done) if *j == id)).unwrap();
    assert!(last_progress < done);
}

#[test]
fn move_across_remotes_removes_sources() {
    let (a, ra) = mem();
    sample_tree(&a);
    let (b, rb) = mem();
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(job(Op::Move, &ra, &["/src"], Some(&rb), Some("/"), ConflictPolicy::Ask));
    assert_done(&h.wait(id));
    assert!(!a.exists("/src"), "source folder should be gone: {:?}", a.tree("/"));
    assert_eq!(b.get("/src/sub/deeper/c.bin").unwrap(), bytes(70_001, 3));
}

#[test]
fn move_within_one_remote_uses_server_rename() {
    let (a, ra) = mem();
    sample_tree(&a);
    a.mkdirs("/dest");
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(job(Op::Move, &ra, &["/src", "/src/../src/a.txt"], Some(&ra), Some("/dest"), ConflictPolicy::Ask));
    let st = h.wait(id);
    // a.txt moved with the folder first, so its own item is gone (NotFound) or moved: either
    // way the tree under /dest/src is complete.
    let _ = st;
    assert!(a.exists("/dest/src/sub/deeper/c.bin"));
    assert!(!a.exists("/src"));
    assert!(a.renames.load(Ordering::SeqCst) >= 1);
}

#[test]
fn move_merging_into_existing_folder_and_keeping_skipped_sources() {
    let (a, ra) = mem();
    a.put("/src/x.txt", b"new x");
    a.put("/src/y.txt", b"new y");
    a.put("/dest/src/x.txt", b"old x");
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(job(Op::Move, &ra, &["/src"], Some(&ra), Some("/dest"), ConflictPolicy::Skip));
    assert_done(&h.wait(id));
    assert_eq!(a.get("/dest/src/x.txt").unwrap(), b"old x");
    assert_eq!(a.get("/dest/src/y.txt").unwrap(), b"new y");
    // skipped x.txt stays, so its folder stays too
    assert_eq!(a.get("/src/x.txt").unwrap(), b"new x");
    assert!(!a.exists("/src/y.txt"));
}

#[test]
fn delete_is_recursive_and_bottom_up() {
    let (a, ra) = mem();
    sample_tree(&a);
    a.put("/keep.txt", b"k");
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(job(Op::Delete, &ra, &["/src"], None, None, ConflictPolicy::Ask));
    assert_done(&h.wait(id));
    assert_eq!(a.tree("/"), vec![("keep.txt".to_string(), Some(b"k".to_vec()))]);
}

#[test]
fn delete_refuses_root_and_ignores_missing() {
    let (a, ra) = mem();
    a.put("/f", b"1");
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(job(Op::Delete, &ra, &["/", "/missing", "/f"], None, None, ConflictPolicy::Ask));
    let st = h.wait(id);
    assert!(matches!(st, JobState::Failed(ref e) if e.kind == ErrorKind::InvalidPath), "{st:?}");
    assert!(!a.exists("/f"));
}

#[test]
fn conflict_policies() {
    let old = SystemTime::now() - Duration::from_secs(3600);
    for (policy, expect_f, expect_extra) in [
        (ConflictPolicy::Overwrite, b"NEW-CONTENT".to_vec(), None),
        (ConflictPolicy::Skip, b"old".to_vec(), None),
        (ConflictPolicy::Rename, b"old".to_vec(), Some(("/d/f 2.txt", b"NEW-CONTENT".to_vec()))),
        (ConflictPolicy::OverwriteIfNewer, b"NEW-CONTENT".to_vec(), None),
    ] {
        let (a, ra) = mem();
        let (b, rb) = mem();
        a.put("/f.txt", b"NEW-CONTENT");
        b.put_at("/d/f.txt", b"old", old);
        let mut h = Harness::new(cfg());
        let id = h.engine.enqueue(copy(&ra, &["/f.txt"], &rb, "/d", policy));
        assert_done(&h.wait(id));
        assert_eq!(b.get("/d/f.txt").unwrap(), expect_f, "{policy:?}");
        if let Some((p, d)) = expect_extra {
            assert_eq!(b.get(p).unwrap(), d);
        }
    }
    // OverwriteIfNewer keeps a newer destination
    let (a, ra) = mem();
    let (b, rb) = mem();
    a.put_at("/f.txt", b"older source", old);
    b.put("/d/f.txt", b"newer dst");
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(copy(&ra, &["/f.txt"], &rb, "/d", ConflictPolicy::OverwriteIfNewer));
    assert_done(&h.wait(id));
    assert_eq!(b.get("/d/f.txt").unwrap(), b"newer dst");
}

#[test]
fn rename_numbers_names_sensibly() {
    let (a, ra) = mem();
    let (b, rb) = mem();
    a.put("/x.tar.gz", b"1");
    a.put("/README", b"2");
    b.put("/d/x.tar.gz", b"o");
    b.put("/d/x 2.tar.gz", b"o");
    b.put("/d/README", b"o");
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(copy(&ra, &["/x.tar.gz", "/README"], &rb, "/d", ConflictPolicy::Rename));
    assert_done(&h.wait(id));
    assert_eq!(b.get("/d/x 3.tar.gz").unwrap(), b"1");
    assert_eq!(b.get("/d/README 2").unwrap(), b"2");
}

#[test]
fn resume_or_overwrite_continues_a_partial_file() {
    let (a, ra) = mem();
    let (b, rb) = mem();
    let data = bytes(500_000, 7);
    a.put("/big.bin", &data);
    b.put("/d/big.bin", &data[..123_457]);
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(copy(&ra, &["/big.bin"], &rb, "/d", ConflictPolicy::ResumeOrOverwrite));
    assert_done(&h.wait(id));
    assert_eq!(b.get("/d/big.bin").unwrap(), data);

    // without resume caps on the destination it overwrites
    let b2 = Mem::new(Caps { resume_write: false, ..full_caps() });
    let rb2: Arc<dyn Remote> = Arc::new(MemRemote(b2.clone()));
    b2.put("/d/big.bin", b"garbage-prefix");
    let id = h.engine.enqueue(copy(&ra, &["/big.bin"], &rb2, "/d", ConflictPolicy::ResumeOrOverwrite));
    assert_done(&h.wait(id));
    assert_eq!(b2.get("/d/big.bin").unwrap(), data);
}

#[test]
fn ask_flow_parks_job_and_honours_apply_to_all() {
    let (a, ra) = mem();
    let (b, rb) = mem();
    for i in 0..4 {
        a.put(&format!("/s/f{i}"), b"new");
        b.put(&format!("/d/s/f{i}"), b"old-old");
    }
    let mut h = Harness::new(EngineConfig { max_files_per_job: 2, ..cfg() });
    let id = h.engine.enqueue(copy(&ra, &["/s"], &rb, "/d", ConflictPolicy::Ask));
    let ask = h.until(Duration::from_secs(10), |e| matches!(e, Event::Conflict { .. })).expect("conflict asked");
    let Event::Conflict { job, ask } = ask else { unreachable!() };
    assert_eq!(job, id);
    assert_eq!(ask.dst.size, Some(7));
    assert!(!ask.can_resume, "dst is longer than src");
    assert!(h.states(id).iter().any(|s| matches!(s, JobState::WaitingForAnswer)));
    // first: skip just this one
    h.engine.answer(id, ConflictAnswer { choice: ConflictChoice::Skip, apply_to_all: false });
    let first = ask.src.path.clone();
    h.until(Duration::from_secs(10), |e| matches!(e, Event::Conflict { .. })).expect("second conflict asked");
    h.engine.answer(id, ConflictAnswer { choice: ConflictChoice::Overwrite, apply_to_all: true });
    assert_done(&h.wait(id));
    let conflicts = h.log.iter().filter(|e| matches!(e, Event::Conflict { .. })).count();
    assert_eq!(conflicts, 2, "apply_to_all stops further questions");
    for i in 0..4 {
        let p = format!("/s/f{i}");
        let got = b.get(&format!("/d{p}")).unwrap();
        if RPath::new(&p) == first {
            assert_eq!(got, b"old-old");
        } else {
            assert_eq!(got, b"new");
        }
    }
}

#[test]
fn ask_offers_resume_for_partial_files_and_cancel_unblocks() {
    let (a, ra) = mem();
    let (b, rb) = mem();
    a.put("/f", &bytes(1000, 1));
    b.put("/d/f", &bytes(10, 1));
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(copy(&ra, &["/f"], &rb, "/d", ConflictPolicy::Ask));
    let Some(Event::Conflict { ask, .. }) = h.until(Duration::from_secs(10), |e| matches!(e, Event::Conflict { .. })) else {
        panic!("no conflict")
    };
    assert!(ask.can_resume);
    h.engine.cancel(id);
    assert!(matches!(h.wait(id), JobState::Cancelled));
}

#[test]
fn transient_read_faults_resume_byte_for_byte() {
    let (a, ra) = mem();
    let (b, rb) = mem();
    let data = bytes(300_000, 9);
    a.put("/f.bin", &data);
    let faulty = Faulty::new(ra.clone());
    faulty.read_faults.lock().unwrap().extend([50_000, 120_001, 299_999]);
    let rf: Arc<dyn Remote> = Arc::new(faulty);
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(copy(&rf, &["/f.bin"], &rb, "/", ConflictPolicy::Ask));
    assert_done(&h.wait(id));
    assert_eq!(b.get("/f.bin").unwrap(), data);
    let retries: Vec<u32> = h.states(id).iter().filter_map(|s| match s {
        JobState::Retrying { attempt, .. } => Some(*attempt),
        _ => None,
    }).collect();
    assert_eq!(retries, vec![1, 2, 3]);
    let p = h.engine.snapshot()[0].3.clone();
    assert_eq!(p.bytes_done, 300_000, "resumed bytes aren't double counted");
}

/// A sink failing with a wrapped RateLimited error: the kind survives the io::Error, so the
/// item retries, and the wait honours retry_after (300 ms) rather than retry_delay (10 ms).
#[test]
fn rate_limited_write_honours_retry_after() {
    let (a, ra) = mem();
    let (b, rb) = mem();
    let data = bytes(100_000, 6);
    a.put("/f.bin", &data);
    let mut faulty = Faulty::new(rb.clone());
    faulty.rate_limited = Some(Duration::from_millis(300));
    faulty.write_faults.lock().unwrap().push_back(30_000);
    let rf: Arc<dyn Remote> = Arc::new(faulty);
    let mut h = Harness::new(cfg());
    let t = Instant::now();
    let id = h.engine.enqueue(copy(&ra, &["/f.bin"], &rf, "/", ConflictPolicy::Ask));
    assert_done(&h.wait(id));
    assert!(t.elapsed() >= Duration::from_millis(300), "retry_after honoured: {:?}", t.elapsed());
    assert_eq!(b.get("/f.bin").unwrap(), data);
    let at = h.log.iter().find_map(|e| match e {
        Event::State(_, JobState::Retrying { attempt: 1, at }) => Some(*at),
        _ => None,
    });
    let wait = at.expect("Retrying state").saturating_duration_since(t);
    assert!(wait >= Duration::from_millis(290), "next attempt scheduled after retry_after: {wait:?}");
    assert!(h.failures(id).is_empty());
}

#[test]
fn resume_refused_by_server_restarts_the_file() {
    let (a, ra) = mem();
    let (b, rb) = mem();
    b.refuse_offset_writes.store(true, Ordering::SeqCst);
    let data = bytes(200_000, 8);
    a.put("/f.bin", &data);
    let faulty = Faulty::new(ra.clone());
    faulty.read_faults.lock().unwrap().push_back(90_000);
    let rf: Arc<dyn Remote> = Arc::new(faulty);
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(copy(&rf, &["/f.bin"], &rb, "/", ConflictPolicy::Ask));
    assert_done(&h.wait(id));
    assert_eq!(b.get("/f.bin").unwrap(), data);
    assert_eq!(h.engine.snapshot()[0].3.bytes_done, 200_000);
    // and a partial file chosen for "resume" also restarts
    b.put("/g.bin", &data[..1000]);
    a.put("/g.bin", &data);
    let id = h.engine.enqueue(copy(&ra, &["/g.bin"], &rb, "/", ConflictPolicy::ResumeOrOverwrite));
    assert_done(&h.wait(id));
    assert_eq!(b.get("/g.bin").unwrap(), data);
}

#[test]
fn transient_write_faults_restart_when_resume_unsupported() {
    let a = Mem::new(Caps { resume_read: false, ..full_caps() });
    let ra: Arc<dyn Remote> = Arc::new(MemRemote(a.clone()));
    let (b, rb) = mem();
    let data = bytes(100_000, 4);
    a.put("/f.bin", &data);
    let faulty = Faulty::new(rb.clone());
    faulty.write_faults.lock().unwrap().extend([40_000, 99_000]);
    let rf: Arc<dyn Remote> = Arc::new(faulty);
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(copy(&ra, &["/f.bin"], &rf, "/", ConflictPolicy::Ask));
    assert_done(&h.wait(id));
    assert_eq!(b.get("/f.bin").unwrap(), data);
}

#[test]
fn exhausted_retries_and_hard_errors_fail_items_but_job_continues() {
    let (a, ra) = mem();
    let (b, rb) = mem();
    a.put("/s/bad.bin", &bytes(50_000, 1));
    a.put("/s/denied.bin", &bytes(10, 1));
    a.put("/s/ok.bin", &bytes(10, 2));
    a.deny.lock().unwrap().push(RPath::new("/s/denied.bin"));
    let faulty = Faulty::new(ra.clone());
    // bad.bin is the only file reaching byte 1000; fail it more times than retries allow
    faulty.read_faults.lock().unwrap().extend([1000; 3]);
    let rf: Arc<dyn Remote> = Arc::new(faulty);
    let mut h = Harness::new(EngineConfig { retries: 2, max_files_per_job: 1, ..cfg() });
    let id = h.engine.enqueue(copy(&rf, &["/s"], &rb, "/", ConflictPolicy::Ask));
    let st = h.wait(id);
    let JobState::Failed(err) = st else { panic!("expected Failed, got {st:?}") };
    assert!(err.message.contains("2 of 3"), "{}", err.message);
    assert!(err.detail.as_deref().unwrap_or("").contains("denied.bin"));
    let fails = h.failures(id);
    assert_eq!(fails.len(), 2);
    assert!(fails.iter().any(|(_, e)| e.kind == ErrorKind::PermissionDenied));
    assert!(fails.iter().any(|(_, e)| e.kind == ErrorKind::Network));
    assert_eq!(b.get("/s/ok.bin").unwrap(), bytes(10, 2));
}

#[test]
fn retry_reruns_only_unfinished_files() {
    let (a, ra) = mem();
    let (b, rb) = mem();
    a.put("/s/one", &bytes(5000, 1));
    a.put("/s/two", &bytes(5000, 2));
    a.deny.lock().unwrap().push(RPath::new("/s/two"));
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(copy(&ra, &["/s"], &rb, "/", ConflictPolicy::Ask));
    assert!(matches!(h.wait(id), JobState::Failed(_)));
    // change "one" on the destination: a retry must not touch it again
    b.put("/s/one", b"marker");
    a.deny.lock().unwrap().clear();
    h.engine.retry(id);
    assert_done(&h.wait(id));
    assert_eq!(b.get("/s/one").unwrap(), b"marker");
    assert_eq!(b.get("/s/two").unwrap(), bytes(5000, 2));
}

#[test]
fn cancel_mid_transfer_aborts_the_sink() {
    let a = Mem::with_chunk(full_caps(), 4096);
    *a.read_delay.lock().unwrap() = Duration::from_millis(5);
    let ra: Arc<dyn Remote> = Arc::new(MemRemote(a.clone()));
    let (b, rb) = mem();
    a.put("/big", &bytes(2_000_000, 1));
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(copy(&ra, &["/big"], &rb, "/", ConflictPolicy::Ask));
    h.until(Duration::from_secs(10), |e| matches!(e, Event::Progress(_, p) if p.bytes_done > 20_000)).unwrap();
    h.engine.cancel(id);
    // Cancel takes effect at the next chunk: at most the one 4 KiB read already under way lands,
    // however slowly the runner goes (a byte count, not a time budget).
    let at_cancel = b.get("/big").unwrap().len();
    assert!(matches!(h.wait(id), JobState::Cancelled));
    let landed = b.get("/big").unwrap().len() - at_cancel;
    assert!(landed <= 4096, "{landed} bytes landed after the cancel");
    assert_eq!(b.aborted_sinks.load(Ordering::SeqCst), 1);
    assert!(b.get("/big").unwrap().len() < 2_000_000);
    assert!(h.failures(id).is_empty(), "cancel isn't a failure");
}

#[test]
fn cancel_queued_job_and_retry_it() {
    let a = Mem::with_chunk(full_caps(), 4096);
    *a.read_delay.lock().unwrap() = Duration::from_millis(5);
    let ra: Arc<dyn Remote> = Arc::new(MemRemote(a.clone()));
    let (b, rb) = mem();
    a.put("/slow", &bytes(400_000, 1));
    a.put("/q", b"queued");
    let mut h = Harness::new(EngineConfig { max_jobs: 1, ..cfg() });
    let first = h.engine.enqueue(copy(&ra, &["/slow"], &rb, "/", ConflictPolicy::Ask));
    let queued = h.engine.enqueue(copy(&ra, &["/q"], &rb, "/", ConflictPolicy::Ask));
    h.engine.cancel(queued);
    assert!(matches!(h.wait(queued), JobState::Cancelled));
    *a.read_delay.lock().unwrap() = Duration::ZERO;
    assert_done(&h.wait(first));
    h.engine.retry(queued);
    assert_done(&h.wait(queued));
    assert_eq!(b.get("/q").unwrap(), b"queued");
}

#[test]
fn pause_stops_bytes_and_resume_finishes() {
    let a = Mem::with_chunk(full_caps(), 4096);
    *a.read_delay.lock().unwrap() = Duration::from_millis(2);
    let ra: Arc<dyn Remote> = Arc::new(MemRemote(a.clone()));
    let (b, rb) = mem();
    let data = bytes(600_000, 5);
    a.put("/big", &data);
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(copy(&ra, &["/big"], &rb, "/", ConflictPolicy::Ask));
    h.until(Duration::from_secs(10), |e| matches!(e, Event::Progress(_, p) if p.bytes_done > 10_000)).unwrap();
    h.engine.pause(id);
    // The one stream stops at its next chunk: from the pause on, at most the 4 KiB read it had
    // already begun lands, however slowly; then a fixed window shows nothing more moves.
    let before = b.get("/big").unwrap().len();
    h.until(Duration::from_secs(30), |e| matches!(e, Event::State(_, JobState::Paused))).expect("Paused state");
    std::thread::sleep(Duration::from_millis(400));
    let after = b.get("/big").unwrap().len();
    assert!(after - before <= 4096, "no bytes while paused beyond the in-flight chunk: {before} -> {after}");
    *a.read_delay.lock().unwrap() = Duration::ZERO;
    h.engine.resume(id);
    assert_done(&h.wait(id));
    assert_eq!(b.get("/big").unwrap(), data);
}

#[test]
fn concurrency_limits_are_respected() {
    let a = Mem::with_chunk(full_caps(), 4096);
    *a.read_delay.lock().unwrap() = Duration::from_millis(1);
    let ra: Arc<dyn Remote> = Arc::new(MemRemote(a.clone()));
    let (_b, rb) = mem();
    for i in 0..12 {
        a.put(&format!("/s/f{i}"), &bytes(40_000, i as u8));
    }
    a.put("/other", &bytes(40_000, 1));
    let mut h = Harness::new(EngineConfig { max_jobs: 1, max_files_per_job: 3, ..cfg() });
    let j1 = h.engine.enqueue(copy(&ra, &["/s"], &rb, "/", ConflictPolicy::Ask));
    let j2 = h.engine.enqueue(copy(&ra, &["/other"], &rb, "/", ConflictPolicy::Ask));
    assert_done(&h.wait(j1));
    assert_done(&h.wait(j2));
    assert_eq!(a.max_open_readers.load(Ordering::SeqCst), 3);
    // j2 never left Queued before j1 finished
    let j1_done = h.log.iter().position(|e| matches!(e, Event::State(j, JobState::Done) if *j == j1)).unwrap();
    let j2_start = h.log.iter().position(|e| matches!(e, Event::State(j, JobState::Scanning) if *j == j2)).unwrap();
    assert!(j2_start > j1_done);
}

#[test]
fn bandwidth_limit_is_roughly_respected() {
    let (a, ra) = mem();
    let (b, rb) = mem();
    a.put("/f1", &bytes(150_000, 1));
    a.put("/f2", &bytes(150_000, 2));
    let mut h = Harness::new(EngineConfig { bandwidth_limit: Some(200_000), ..cfg() });
    let t = Instant::now();
    let j1 = h.engine.enqueue(copy(&ra, &["/f1"], &rb, "/", ConflictPolicy::Ask));
    let j2 = h.engine.enqueue(copy(&ra, &["/f2"], &rb, "/", ConflictPolicy::Ask));
    assert_done(&h.wait(j1));
    assert_done(&h.wait(j2));
    let el = t.elapsed().as_secs_f64();
    // 300 kB at 200 kB/s shared: ~1.5 s (minus a 50 kB initial burst allowance)
    assert!(el > 1.1 && el < 3.5, "took {el}s");
    assert_eq!(b.get("/f2").unwrap().len(), 150_000);
}

#[test]
fn progress_is_throttled_and_has_speed_and_eta() {
    let a = Mem::with_chunk(full_caps(), 1024);
    *a.read_delay.lock().unwrap() = Duration::from_micros(500);
    let ra: Arc<dyn Remote> = Arc::new(MemRemote(a.clone()));
    let (_b, rb) = mem();
    a.put("/f", &bytes(1_000_000, 1));
    let mut h = Harness::new(EngineConfig { chunk: 1024, ..cfg() });
    let t = Instant::now();
    let id = h.engine.enqueue(copy(&ra, &["/f"], &rb, "/", ConflictPolicy::Ask));
    assert_done(&h.wait(id));
    let secs = t.elapsed().as_secs_f64();
    let progress: Vec<&Progress> = h.log.iter().filter_map(|e| match e {
        Event::Progress(_, p) => Some(p),
        _ => None,
    }).collect();
    assert!((progress.len() as f64) < secs * 12.0 + 4.0, "{} events in {secs}s", progress.len());
    assert!(progress.iter().any(|p| p.bytes_per_sec > 0.0 && p.eta.is_some()));
    let last = progress.last().unwrap();
    assert_eq!(last.bytes_done, 1_000_000);
}

#[test]
fn guards_against_copying_into_itself_and_duplicates_in_place() {
    let (a, ra) = mem();
    a.put("/dir/f", b"x");
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(copy(&ra, &["/dir"], &ra, "/dir", ConflictPolicy::Ask));
    assert!(matches!(h.wait(id), JobState::Failed(ref e) if e.kind == ErrorKind::InvalidPath));
    let id = h.engine.enqueue(copy(&ra, &["/dir/f"], &ra, "/dir", ConflictPolicy::Overwrite));
    assert_done(&h.wait(id));
    assert_eq!(a.get("/dir/f").unwrap(), b"x");
    assert_eq!(a.get("/dir/f 2").unwrap(), b"x");
}

#[test]
fn missing_destination_folder_fails_clearly() {
    let (a, ra) = mem();
    let (_b, rb) = mem();
    a.put("/f", b"x");
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(copy(&ra, &["/f"], &rb, "/nope", ConflictPolicy::Ask));
    let st = h.wait(id);
    assert!(matches!(st, JobState::Failed(ref e) if e.kind == ErrorKind::NotFound && e.message.contains("/nope")), "{st:?}");
}

#[test]
fn file_in_the_way_of_a_folder() {
    let (a, ra) = mem();
    let (b, rb) = mem();
    a.put("/s/inner/f", b"x");
    b.put("/d/s/inner", b"i am a file");
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(copy(&ra, &["/s"], &rb, "/d", ConflictPolicy::Overwrite));
    let st = h.wait(id);
    assert!(matches!(st, JobState::Failed(ref e) if e.kind == ErrorKind::AlreadyExists), "{st:?}");
}

#[test]
fn forget_and_snapshot() {
    let (a, ra) = mem();
    let (_b, rb) = mem();
    a.put("/f", b"x");
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(copy(&ra, &["/f"], &rb, "/", ConflictPolicy::Ask));
    h.wait(id);
    assert_eq!(h.engine.snapshot().len(), 1);
    assert!(h.engine.forget(id));
    assert!(h.engine.snapshot().is_empty());
}
