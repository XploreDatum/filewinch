//! compare() planning and apply() end to end.

mod common;

use common::*;
use filewinch_core::{RPath, Remote};
use filewinch_engine::*;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

type Pair = (Arc<Mem>, Arc<dyn Remote>, Arc<Mem>, Arc<dyn Remote>);

fn pair() -> Pair {
    let a = Mem::new(full_caps());
    let b = Mem::new(full_caps());
    let ra: Arc<dyn Remote> = Arc::new(MemRemote(a.clone()));
    let rb: Arc<dyn Remote> = Arc::new(MemRemote(b.clone()));
    a.mkdirs("/A");
    b.mkdirs("/B");
    (a, ra, b, rb)
}

fn kinds(plan: &SyncPlan) -> Vec<(SyncActionKind, String)> {
    plan.actions.iter().map(|a| (a.kind, a.rel.clone())).collect()
}

fn opts(direction: SyncDirection) -> SyncOptions {
    SyncOptions { direction, ..SyncOptions::default() }
}

#[test]
fn a_to_b_plans_new_changed_and_orphans() {
    let (a, ra, b, rb) = pair();
    let t = SystemTime::now() - Duration::from_secs(1000);
    a.put_at("/A/same.txt", b"same", t);
    b.put_at("/B/same.txt", b"same", t + Duration::from_millis(900)); // within tolerance
    a.put_at("/A/changed.txt", b"v2", t + Duration::from_secs(60));
    b.put_at("/B/changed.txt", b"v1", t);
    a.put_at("/A/sized.txt", b"longer", t);
    b.put_at("/B/sized.txt", b"short", t);
    a.put("/A/newdir/x/y.txt", b"y");
    b.put("/B/orphan.txt", b"o");
    b.put("/B/orphandir/z", b"z");
    a.put("/A/sub/inner.txt", b"i");
    b.put_at("/B/sub/inner.txt", b"i", SystemTime::now() + Duration::from_secs(3600));

    let plan = compare(&*ra, &RPath::new("/A"), &*rb, &RPath::new("/B"), &opts(SyncDirection::AToB)).unwrap();
    use SyncActionKind::*;
    assert_eq!(
        kinds(&plan),
        vec![
            (CopyAToB, "changed.txt".into()),
            (CopyAToB, "newdir".into()),
            (CopyAToB, "sized.txt".into()),
            (CopyAToB, "sub/inner.txt".into()),
        ]
    );
    assert!(plan.actions.iter().find(|x| x.rel == "newdir").unwrap().is_dir);

    let with_delete = SyncOptions { delete_orphans: true, ..opts(SyncDirection::AToB) };
    let plan = compare(&*ra, &RPath::new("/A"), &*rb, &RPath::new("/B"), &with_delete).unwrap();
    let deletes: Vec<_> = kinds(&plan).into_iter().filter(|k| k.0 == DeleteInB).map(|k| k.1).collect();
    assert_eq!(deletes, vec!["orphan.txt".to_string(), "orphandir".to_string()]);

    // size-only ignores the mtime-only change of changed.txt? no: sizes equal (2 bytes) → skipped
    let size_only = SyncOptions { compare: CompareBy::SizeOnly, ..opts(SyncDirection::AToB) };
    let plan = compare(&*ra, &RPath::new("/A"), &*rb, &RPath::new("/B"), &size_only).unwrap();
    assert!(!kinds(&plan).iter().any(|k| k.1 == "changed.txt"));
    assert!(kinds(&plan).iter().any(|k| k.1 == "sized.txt"));
}

#[test]
fn minute_tolerance_for_coarse_servers() {
    let (a, ra, b, rb) = pair();
    let t = SystemTime::now() - Duration::from_secs(1000);
    a.put_at("/A/f", b"x", t);
    b.put_at("/B/f", b"x", t - Duration::from_secs(45));
    let strict = compare(&*ra, &RPath::new("/A"), &*rb, &RPath::new("/B"), &opts(SyncDirection::AToB)).unwrap();
    assert_eq!(strict.actions.len(), 1);
    let loose = SyncOptions { compare: CompareBy::SizeAndMtime { tolerance: Duration::from_secs(60) }, ..opts(SyncDirection::AToB) };
    assert!(compare(&*ra, &RPath::new("/A"), &*rb, &RPath::new("/B"), &loose).unwrap().is_empty());
}

#[test]
fn b_to_a_and_mirror() {
    let (a, ra, b, rb) = pair();
    let t = SystemTime::now() - Duration::from_secs(1000);
    a.put("/A/only_a", b"a");
    b.put("/B/only_b", b"b");
    a.put_at("/A/newer_in_a", b"aa", t + Duration::from_secs(100));
    b.put_at("/B/newer_in_a", b"b", t);
    a.put_at("/A/newer_in_b", b"a", t);
    b.put_at("/B/newer_in_b", b"bb", t + Duration::from_secs(100));
    a.put_at("/A/tie", b"one", t);
    b.put_at("/B/tie", b"three", t);
    a.put("/A/clash", b"file");
    b.mkdirs("/B/clash");

    use SyncActionKind::*;
    let plan = compare(&*ra, &RPath::new("/A"), &*rb, &RPath::new("/B"), &opts(SyncDirection::Mirror)).unwrap();
    assert_eq!(
        kinds(&plan),
        vec![
            (CopyAToB, "newer_in_a".into()),
            (CopyAToB, "only_a".into()),
            (CopyBToA, "newer_in_b".into()),
            (CopyBToA, "only_b".into()),
        ]
    );
    let conflicts: Vec<_> = plan.conflicts.iter().map(|c| c.rel.as_str()).collect();
    assert_eq!(conflicts.len(), 2);
    assert!(conflicts.contains(&"tie") && conflicts.contains(&"clash"));

    let del = SyncOptions { delete_orphans: true, ..opts(SyncDirection::BToA) };
    let plan = compare(&*ra, &RPath::new("/A"), &*rb, &RPath::new("/B"), &del).unwrap();
    assert!(kinds(&plan).contains(&(DeleteInA, "only_a".into())));
    assert!(kinds(&plan).contains(&(CopyBToA, "only_b".into())));
}

#[test]
fn excludes_match_names_and_paths() {
    let (a, ra, _b, rb) = pair();
    a.put("/A/keep.txt", b"k");
    a.put("/A/x.tmp", b"t");
    a.put("/A/.git/config", b"g");
    a.put("/A/build/out/bin", b"b");
    a.put("/A/src/build/keep", b"k");
    let o = SyncOptions { exclude: vec!["*.tmp".into(), ".git".into(), "build/out".into()], ..opts(SyncDirection::AToB) };
    let plan = compare(&*ra, &RPath::new("/A"), &*rb, &RPath::new("/B"), &o).unwrap();
    let rels: Vec<_> = plan.actions.iter().map(|x| x.rel.as_str()).collect();
    assert!(rels.contains(&"keep.txt"));
    assert!(rels.contains(&"src"));
    assert!(!rels.iter().any(|r| r.contains("tmp") || r.contains(".git")));
    // "build" itself isn't excluded (only build/out), so it is copied (and its scan copies out/ too:
    // excludes shape the plan; a whole-folder copy takes the folder as it is).
    assert!(rels.contains(&"build"));

    let bad = SyncOptions { exclude: vec!["[".into()], ..opts(SyncDirection::AToB) };
    let err = compare(&*ra, &RPath::new("/A"), &*rb, &RPath::new("/B"), &bad).unwrap_err();
    assert!(err.message.contains("isn't a valid exclude pattern"));
}

#[cfg(unix)]
#[test]
fn symlinks_followed_only_when_asked() {
    let dir = scratch("sync-links");
    std::fs::create_dir_all(dir.join("A/real")).unwrap();
    std::fs::write(dir.join("A/real/f"), b"f").unwrap();
    std::os::unix::fs::symlink("real", dir.join("A/link")).unwrap();
    std::fs::create_dir_all(dir.join("B")).unwrap();
    let disk: Arc<dyn Remote> = Arc::new(Disk::new(&dir));
    let plan = compare(&*disk, &RPath::new("/A"), &*disk, &RPath::new("/B"), &opts(SyncDirection::AToB)).unwrap();
    assert_eq!(plan.actions.iter().map(|a| a.rel.as_str()).collect::<Vec<_>>(), vec!["real"]);
    let follow = SyncOptions { follow_symlinks: true, ..opts(SyncDirection::AToB) };
    let plan = compare(&*disk, &RPath::new("/A"), &*disk, &RPath::new("/B"), &follow).unwrap();
    assert_eq!(plan.actions.len(), 2);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn apply_runs_to_convergence() {
    let (a, ra, b, rb) = pair();
    a.put("/A/one.txt", b"1");
    a.put("/A/d/two.txt", b"2");
    a.put("/A/d/e/three.txt", b"3");
    b.put("/B/d/two.txt", b"old two, longer");
    b.mkdirs("/B/d/e");
    b.put("/B/stale.txt", b"s");
    b.put("/B/stale_dir/x", b"x");
    let o = SyncOptions { delete_orphans: true, ..opts(SyncDirection::AToB) };
    let plan = compare(&*ra, &RPath::new("/A"), &*rb, &RPath::new("/B"), &o).unwrap();
    assert_eq!(plan.bytes_to_copy(), 3);
    let jobs = apply(&plan, ra.clone(), rb.clone());
    assert_eq!(jobs.last().unwrap().op, Op::Delete, "deletes come last");
    let mut h = Harness::new(cfg());
    let ids: Vec<_> = jobs.into_iter().map(|j| h.engine.enqueue(j)).collect();
    for id in ids {
        assert_done(&h.wait(id));
    }
    assert_eq!(a.tree("/A"), b.tree("/B"));
    let again = compare(&*ra, &RPath::new("/A"), &*rb, &RPath::new("/B"), &o).unwrap();
    assert!(again.is_empty(), "converged: {:?}", again.actions);
}
