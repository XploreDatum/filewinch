//! Delete and shared items (core request E-1): a shared folder is left or refused, never
//! emptied, unless the job carries the user's explicit `delete_shared_contents`.

mod common;

use common::*;
use filewinch_core::{Caps, Entry, Error, ErrorKind, RPath, Remote, Result, Shared, WriteSink};
use filewinch_engine::*;
use std::collections::{BTreeMap, VecDeque};
use std::io::Read;
use std::sync::{Arc, Mutex};

/// Wraps a remote: marks some paths shared and implements `leave_shared` by hiding the item
/// (it stays in the backing store, as it would for the share's other members).
struct Shares {
    inner: Arc<Mem>,
    marks: Mutex<BTreeMap<RPath, Shared>>,
    left: Mutex<Vec<RPath>>,
    /// Scripted `leave_shared` failures: the first one for the path is taken per call.
    leave_faults: Mutex<VecDeque<(RPath, Error)>>,
}

impl Shares {
    fn new() -> (Arc<Mem>, Arc<Shares>) {
        let m = Mem::new(full_caps());
        let s = Arc::new(Shares { inner: m.clone(), marks: Mutex::default(), left: Mutex::default(), leave_faults: Mutex::default() });
        (m, s)
    }
    fn mark(&self, path: &str, owned_by_me: bool, can_leave: bool) {
        self.marks.lock().unwrap().insert(RPath::new(path), Shared { owned_by_me, can_leave });
    }
    fn hidden(&self, p: &RPath) -> bool {
        self.left.lock().unwrap().iter().any(|l| p.starts_with(l))
    }
    fn decorate(&self, mut e: Entry) -> Entry {
        e.shared = self.marks.lock().unwrap().get(&e.path).copied();
        e
    }
    fn left(&self) -> Vec<String> {
        self.left.lock().unwrap().iter().map(|p| p.to_string()).collect()
    }
    fn mem(&self) -> MemRemote {
        MemRemote(self.inner.clone())
    }
}

impl Remote for Shares {
    fn caps(&self) -> Caps {
        self.inner.caps
    }
    fn home(&self) -> Result<RPath> {
        Ok(RPath::root())
    }
    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        Ok(self.mem().list(dir)?.into_iter().filter(|e| !self.hidden(&e.path)).map(|e| self.decorate(e)).collect())
    }
    fn stat(&self, path: &RPath) -> Result<Entry> {
        if self.hidden(path) {
            return Err(Error::new(ErrorKind::NotFound, "gone"));
        }
        Ok(self.decorate(self.mem().stat(path)?))
    }
    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        self.mem().read(path, offset)
    }
    fn write(&self, path: &RPath, offset: u64, size: Option<u64>) -> Result<Box<dyn WriteSink>> {
        self.mem().write(path, offset, size)
    }
    fn mkdir(&self, path: &RPath) -> Result<()> {
        self.mem().mkdir(path)
    }
    fn rename(&self, from: &RPath, to: &RPath) -> Result<()> {
        self.mem().rename(from, to)
    }
    fn remove_file(&self, path: &RPath) -> Result<()> {
        self.mem().remove_file(path)
    }
    fn remove_dir(&self, path: &RPath) -> Result<()> {
        self.mem().remove_dir(path)
    }
    fn leave_shared(&self, path: &RPath) -> Result<()> {
        let mut faults = self.leave_faults.lock().unwrap();
        if let Some(i) = faults.iter().position(|(p, _)| p == path) {
            return Err(faults.remove(i).unwrap().1);
        }
        drop(faults);
        self.left.lock().unwrap().push(path.clone());
        Ok(())
    }
}

fn delete(src: &Arc<Shares>, paths: &[&str], delete_shared_contents: bool) -> Job {
    let src: Arc<dyn Remote> = src.clone();
    Job {
        op: Op::Delete,
        src,
        src_paths: paths.iter().map(|p| RPath::new(p)).collect(),
        dst: None,
        dst_dir: None,
        conflict: ConflictPolicy::Skip,
        label: "test".into(),
        delete_shared_contents,
    }
}

fn tree(m: &Mem) {
    m.put("/Design/logo.svg", b"<svg/>");
    m.put("/Design/Drafts/v1.svg", b"v1");
    m.put("/Mine/plan.txt", b"plan");
    m.put("/Projects/a.txt", b"a");
    m.put("/Projects/Team/notes.txt", b"n");
    m.put("/Projects/link.txt", b"l");
    m.put("/Plain/x.txt", b"x");
}

#[test]
fn a_leavable_share_is_left_and_nothing_inside_is_deleted() {
    let (m, s) = Shares::new();
    tree(&m);
    s.mark("/Design", false, true);
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(delete(&s, &["/Design", "/Plain"], false));
    assert_done(&h.wait(id));
    assert_eq!(s.left(), ["/Design"]);
    assert!(m.exists("/Design/logo.svg") && m.exists("/Design/Drafts/v1.svg"), "contents survive for the members");
    assert!(!m.exists("/Plain"), "the ordinary folder is deleted as usual");
}

#[test]
fn a_share_that_cant_be_left_is_refused_with_a_clear_message() {
    let (m, s) = Shares::new();
    tree(&m);
    s.mark("/Design", true, false);
    s.mark("/Plain/x.txt", false, false);
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(delete(&s, &["/Design", "/Plain/x.txt"], false));
    let st = h.wait(id);
    assert!(matches!(st, JobState::Failed(ref e) if e.kind == ErrorKind::PermissionDenied), "{st:?}");
    let f = h.failures(id);
    let msgs: Vec<_> = f.iter().map(|(p, e)| (p.to_string(), e.message.clone())).collect();
    assert_eq!(msgs, [
        ("/Design".to_string(), "“Design” is shared with others. Filewinch won't delete files inside a shared folder. Remove it from the provider's website if you own it.".to_string()),
        ("/Plain/x.txt".to_string(), "“x.txt” is shared with others. Filewinch won't delete a shared file. Remove it from the provider's website if you own it.".to_string()),
    ]);
    assert!(m.exists("/Design/logo.svg") && m.exists("/Plain/x.txt"));
    assert!(s.left().is_empty());
}

#[test]
fn the_explicit_opt_in_deletes_the_contents() {
    let (m, s) = Shares::new();
    tree(&m);
    s.mark("/Design", true, false);
    s.mark("/Mine", false, true);
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(delete(&s, &["/Design", "/Mine"], true));
    assert_done(&h.wait(id));
    assert!(!m.exists("/Design") && !m.exists("/Mine"));
    assert!(s.left().is_empty(), "the opt-in deletes rather than leaves");
}

#[test]
fn shares_nested_in_a_deleted_folder_are_left_whole() {
    for opt_in in [false, true] {
        let (m, s) = Shares::new();
        tree(&m);
        s.mark("/Projects/Team", false, true);
        s.mark("/Projects/link.txt", false, true);
        let mut h = Harness::new(cfg());
        let id = h.engine.enqueue(delete(&s, &["/Projects"], opt_in));
        let st = h.wait(id);
        assert!(matches!(st, JobState::Failed(_)), "{st:?}");
        let mut f: Vec<_> = h.failures(id).into_iter().map(|(p, e)| (p.to_string(), e.message)).collect();
        f.sort();
        assert_eq!(f.len(), 2, "{f:?}");
        assert_eq!(f[0].0, "/Projects/Team");
        assert_eq!(
            f[0].1,
            "“Team” in “Projects” is shared with others, so Filewinch left it (and the folders around it) in place. Select it on its own to remove it."
        );
        assert_eq!(f[1].0, "/Projects/link.txt");
        assert!(!m.exists("/Projects/a.txt"), "ordinary items still go");
        assert!(m.exists("/Projects/Team/notes.txt") && m.exists("/Projects/link.txt"), "shares are neither emptied");
        assert!(s.left().is_empty(), "nor left");
        assert!(m.exists("/Projects"), "the folder stays to hold them");
    }
}

#[test]
fn leaving_retries_transient_errors_and_reports_others() {
    let (m, s) = Shares::new();
    tree(&m);
    s.mark("/Design", false, true);
    s.mark("/Mine", false, true);
    s.leave_faults.lock().unwrap().extend([
        (RPath::new("/Design"), Error::new(ErrorKind::Network, "dropped")),
        (RPath::new("/Mine"), Error::new(ErrorKind::PermissionDenied, "Couldn't leave “Mine”: it's a team folder.")),
    ]);
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(delete(&s, &["/Design", "/Mine"], false));
    let st = h.wait(id);
    // Design: Network, then retried and left. Mine: refused by the server.
    assert!(matches!(st, JobState::Failed(ref e) if e.message.starts_with("Mine couldn't be deleted")), "{st:?}");
    assert_eq!(s.left(), ["/Design"]);
    assert!(m.exists("/Mine/plan.txt"));
    // Retrying the job only redoes what failed.
    s.leave_faults.lock().unwrap().clear();
    h.engine.retry(id);
    let st = {
        h.until(std::time::Duration::from_secs(10), |e| matches!(e, Event::State(j, JobState::Running | JobState::Scanning) if *j == id));
        h.until(std::time::Duration::from_secs(10), |e| matches!(e, Event::State(j, s) if *j == id && s.is_finished()))
    };
    assert!(matches!(st, Some(Event::State(_, JobState::Done))), "{st:?}");
    assert_eq!(s.left(), ["/Design", "/Mine"]);
}

#[test]
fn moving_out_of_a_shared_folder_is_unaffected() {
    let (m, s) = Shares::new();
    tree(&m);
    s.mark("/Design", false, true);
    let r: Arc<dyn Remote> = s.clone();
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(Job {
        op: Op::Move,
        src: r.clone(),
        src_paths: vec![RPath::new("/Design/logo.svg")],
        dst: Some(r),
        dst_dir: Some(RPath::new("/Plain")),
        conflict: ConflictPolicy::Skip,
        label: "move".into(),
        delete_shared_contents: false,
    });
    assert_done(&h.wait(id));
    assert!(!m.exists("/Design/logo.svg") && m.exists("/Plain/logo.svg"));
}

#[test]
fn moving_a_shared_folder_to_another_remote_copies_it_and_keeps_the_original() {
    let (m, s) = Shares::new();
    tree(&m);
    s.mark("/Design", false, true);
    s.mark("/Projects/Team", false, true);
    let src: Arc<dyn Remote> = s.clone();
    let out = Mem::new(full_caps());
    out.mkdirs("/out");
    let dst: Arc<dyn Remote> = Arc::new(MemRemote(out.clone()));
    let mut h = Harness::new(cfg());
    let id = h.engine.enqueue(Job {
        op: Op::Move,
        src,
        src_paths: vec![RPath::new("/Design"), RPath::new("/Projects")],
        dst: Some(dst),
        dst_dir: Some(RPath::new("/out")),
        conflict: ConflictPolicy::Skip,
        label: "move".into(),
        delete_shared_contents: false,
    });
    let st = h.wait(id);
    assert!(matches!(st, JobState::Failed(_)), "{st:?}");
    let mut f: Vec<_> = h.failures(id).into_iter().map(|(p, e)| (p.to_string(), e.message)).collect();
    f.sort();
    assert_eq!(f.len(), 2, "{f:?}");
    assert_eq!(f[0].1, "“Design” is shared with others, so Filewinch copied it and left the original in place. Moving it would delete it for everyone.");
    assert_eq!(f[1].0, "/Projects/Team");
    // Everything arrived; the shares' originals stayed, the ordinary file moved.
    assert!(out.exists("/out/Design/Drafts/v1.svg") && out.exists("/out/Projects/Team/notes.txt") && out.exists("/out/Projects/a.txt"));
    assert!(m.exists("/Design/logo.svg") && m.exists("/Design/Drafts/v1.svg") && m.exists("/Projects/Team/notes.txt"));
    assert!(!m.exists("/Projects/a.txt"));
}
