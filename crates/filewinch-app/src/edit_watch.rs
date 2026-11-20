//! Opening remote files: download to the cache, open in their app (or the chosen editor),
//! and with "Edit", watch the copy and upload each save.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use filewinch_core::{Error, ErrorKind, RPath, Remote};

use crate::hub::Inbox;
use crate::pane::Source;
use crate::transfers::engine::{ConflictPolicy, Job, Op};
use crate::transfers::model::Purpose;
use crate::ui::{Tone, Ui};

#[derive(Clone, Debug, PartialEq)]
pub enum EditState {
    Downloading,
    /// Open in its app; saves upload.
    Watching,
    Uploading,
    /// Opened for viewing only.
    Opened,
    Failed(String),
}

pub struct Edit {
    pub id: u64,
    pub session: u64,
    pub label: String,
    pub remote: Arc<dyn Remote>,
    pub path: RPath,
    pub local: PathBuf,
    pub watch: bool,
    pub state: EditState,
    /// A save landed while an upload was running: upload again after it.
    pub dirty: bool,
    stop: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct Edits {
    pub list: Vec<Edit>,
}

impl Edits {
    pub fn get_mut(&mut self, id: u64) -> Option<&mut Edit> {
        self.list.iter_mut().find(|e| e.id == id)
    }
    pub fn watching(&self) -> impl Iterator<Item = &Edit> {
        self.list.iter().filter(|e| e.watch)
    }
}

impl Drop for Edit {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// `<what>` in Filewinch's cache folder: `~/Library/Caches/app.xpdt.filewinch` on macOS,
/// `%LOCALAPPDATA%\Filewinch\Cache` on Windows, `$XDG_CACHE_HOME/filewinch` on Linux (the
/// system temp folder if there's no home folder at all).
pub fn cache_dir(what: &str) -> PathBuf {
    filewinch_os::dirs::Dirs::current().map_or_else(|| std::env::temp_dir().join(filewinch_os::APP_ID), |d| d.cache).join(what)
}

/// Opens `file` in its default app, or in `editor` (the external-editor setting: an app name
/// on macOS, a command or program path on Windows and Linux).
pub fn open_with(file: &Path, editor: Option<&str>) -> filewinch_core::Result<()> {
    filewinch_os::launch::open_file(file, editor).map_err(|e| {
        let name = file.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        Error::new(ErrorKind::Other, format!("Couldn't open “{name}”.")).with_detail(e)
    })
}

fn stamp(p: &Path) -> Option<(SystemTime, u64)> {
    let md = std::fs::metadata(p).ok()?;
    Some((md.modified().ok()?, md.len()))
}

/// Polls `file` once a second; posts `changed(id)` when a save settles (same stamp twice).
fn watch(file: PathBuf, id: u64, inbox: Inbox, stop: Arc<AtomicBool>) {
    std::thread::Builder::new()
        .name("filewinch-edit-watch".into())
        .spawn(move || {
            let mut last = stamp(&file);
            let mut pending: Option<(SystemTime, u64)> = None;
            while !stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(800));
                let now = stamp(&file);
                if now.is_none() {
                    continue; // editors that save by replacing the file leave a gap
                }
                match pending {
                    Some(p) if Some(p) == now => {
                        pending = None;
                        last = now;
                        inbox.post(move |ui| ui.edit_saved(id));
                    }
                    _ if now != last => pending = now,
                    _ => pending = None,
                }
            }
        })
        .ok();
}

impl Ui {
    pub fn start_remote_open(&mut self, src: &Source, path: &RPath, watch: bool) {
        let id = self.next_id();
        let dir = cache_dir("open").join(format!("{}-{id}", src.session));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.notify(Tone::Error, "Couldn't make room in the cache to open the file.", Some(format!("{}: {e}", dir.display())));
            return;
        }
        // Where the local backend saves it: on Windows a name it refuses gets its substitute.
        let local = filewinch_local::local_path(&filewinch_local::rpath(&dir).join(path.name()));
        self.edits.list.push(Edit {
            id,
            session: src.session,
            label: src.label.clone(),
            remote: src.remote.clone(),
            path: path.clone(),
            local,
            watch,
            state: EditState::Downloading,
            dirty: false,
            stop: Arc::new(AtomicBool::new(false)),
        });
        let job = Job {
            op: Op::Copy,
            src: src.remote.clone(),
            src_paths: vec![path.clone()],
            dst: Some(Source::local().remote),
            dst_dir: Some(filewinch_local::rpath(&dir)),
            conflict: ConflictPolicy::Overwrite,
            label: format!("Opening “{}”", path.name()),
            delete_shared_contents: false,
        };
        self.enqueue(job, Purpose::Open { edit: id }, Vec::new());
    }

    pub fn edit_downloaded(&mut self, id: u64, ok: bool) {
        let editor = self.prefs.editor.clone();
        let inbox = self.inbox.clone();
        let Some(e) = self.edits.get_mut(id) else { return };
        if !ok {
            e.state = EditState::Failed("The download failed; see Transfers.".into());
            return;
        }
        e.state = if e.watch { EditState::Watching } else { EditState::Opened };
        let (file, watch_it, stop) = (e.local.clone(), e.watch, e.stop.clone());
        let editor = if watch_it { editor } else { None };
        let opened = file.clone();
        self.run(move || open_with(&opened, editor.as_deref()), move |ui, res| match res {
            Ok(()) if watch_it => watch(file, id, inbox, stop),
            Ok(()) => {}
            Err(err) => {
                if let Some(e) = ui.edits.get_mut(id) {
                    e.state = EditState::Failed(err.message.clone());
                }
                ui.notify_error(&err);
            }
        });
    }

    /// The watched copy was saved: upload it (or queue another upload after the running one).
    pub fn edit_saved(&mut self, id: u64) {
        let Some(e) = self.edits.get_mut(id) else { return };
        match e.state {
            EditState::Uploading => {
                e.dirty = true;
                return;
            }
            EditState::Watching | EditState::Failed(_) => {}
            _ => return,
        }
        e.state = EditState::Uploading;
        e.dirty = false;
        let (remote, path, local, label) = (e.remote.clone(), e.path.clone(), e.local.clone(), e.label.clone());
        let Some(dir) = path.parent() else { return };
        let session = e.session;
        let job = Job {
            op: Op::Copy,
            src: Source::local().remote,
            src_paths: vec![filewinch_local::rpath(&local)],
            dst: Some(remote),
            dst_dir: Some(dir.clone()),
            conflict: ConflictPolicy::Overwrite,
            label: format!("Saving “{}” to {label}", path.name()),
            delete_shared_contents: false,
        };
        self.enqueue(job, Purpose::EditUpload { edit: id }, vec![(session, dir)]);
    }

    pub fn edit_uploaded(&mut self, id: u64, ok: bool) {
        let Some(e) = self.edits.get_mut(id) else { return };
        let name = e.path.name().to_owned();
        if ok {
            e.state = EditState::Watching;
            if std::mem::take(&mut e.dirty) {
                self.edit_saved(id);
            }
        } else {
            e.state = EditState::Failed(format!("“{name}” wasn't saved to the server. Save again to retry."));
            self.notify(Tone::Error, format!("Your changes to “{name}” weren't uploaded."), Some("Save the file again to retry, or see Transfers.".into()));
        }
    }

    pub fn stop_edit(&mut self, id: u64) {
        self.edits.list.retain(|e| e.id != id);
    }
}
