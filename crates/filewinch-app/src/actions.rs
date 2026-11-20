//! File operations from menus, keys and drags: transfers, delete, rename, new items,
//! duplicate, open, and what happens when a job ends.

use std::sync::Arc;

use filewinch_core::{EntryKind, RPath, Remote};
use filewinch_store::ConflictDefault;
use rune::{TextEdit, Tx};

use crate::hub::hub;
use crate::pane::model::{invalid_name, unique_name};
use crate::pane::{Rename, Source};
use crate::transfers::engine::{ConflictPolicy, Event, Job, JobId, Op};
use crate::transfers::model::{Direction, JobMeta, Purpose};
use crate::ui::{Sheet, Tone, Ui};

pub fn policy(c: ConflictDefault) -> ConflictPolicy {
    match c {
        ConflictDefault::Ask => ConflictPolicy::Ask,
        ConflictDefault::Overwrite => ConflictPolicy::Overwrite,
        ConflictDefault::OverwriteIfNewer => ConflictPolicy::OverwriteIfNewer,
        ConflictDefault::Resume => ConflictPolicy::ResumeOrOverwrite,
        ConflictDefault::Rename => ConflictPolicy::Rename,
        ConflictDefault::Skip => ConflictPolicy::Skip,
    }
}

/// "“index.html”" or "3 items".
pub fn items_label(paths: &[RPath]) -> String {
    match paths {
        [one] => format!("“{}”", one.name()),
        many => format!("{} items", many.len()),
    }
}

impl Ui {
    /// Queues a job and remembers why. Returns its id.
    pub fn enqueue(&mut self, job: Job, purpose: Purpose, touched: Vec<(u64, RPath)>) -> JobId {
        let label = job.label.clone();
        // This Mac is whatever isn't one of the window's server sessions.
        let local = |r: &Arc<dyn Remote>| !self.sessions.iter().any(|s| Arc::ptr_eq(&s.remote, r));
        let direction = Direction::of(&job.op, local(&job.src), job.dst.as_ref().map(local));
        let visible = matches!(purpose, Purpose::Plain);
        let id = hub().engine.enqueue(job);
        self.jobs.insert(id, JobMeta { purpose, touched });
        self.transfers.apply(&Event::Added(id));
        self.transfers.set_label(id, &label);
        self.transfers.set_direction(id, direction);
        if visible {
            self.prefs.transfers_open = true;
        }
        id
    }

    /// Copies (or moves) `paths` from `from` into `to_dir` on pane `to`'s source.
    pub fn transfer(&mut self, from: &Source, paths: Vec<RPath>, to: &Source, to_dir: RPath, op: Op) {
        if paths.is_empty() {
            return;
        }
        // Dropping items onto the folder they're already in does nothing.
        if from.session == to.session && paths.iter().all(|p| p.parent().as_ref() == Some(&to_dir)) {
            return;
        }
        if from.session == to.session && paths.iter().any(|p| to_dir.starts_with(p)) {
            self.notify(Tone::Warning, "A folder can't be copied or moved into itself.", None);
            return;
        }
        let items = items_label(&paths);
        let verb = match (op, from.is_local(), to.is_local()) {
            (Op::Move, _, _) => "Moving",
            (_, true, false) => "Uploading",
            (_, false, true) => "Downloading",
            _ => "Copying",
        };
        let label = format!("{verb} {items} to {}", if to_dir.is_root() { to.label.clone() } else { format!("{} on {}", to_dir.name(), to.label) });
        let mut touched = vec![(to.session, to_dir.clone())];
        if op == Op::Move {
            touched.extend(paths.iter().filter_map(|p| p.parent()).map(|d| (from.session, d)));
        }
        let reveal = (to.is_local() && !from.is_local()).then(|| filewinch_local::local_path(&to_dir.join(paths[0].name())));
        let job = Job {
            op,
            src: from.remote.clone(),
            src_paths: paths,
            dst: Some(to.remote.clone()),
            dst_dir: Some(to_dir),
            conflict: policy(self.prefs.conflict),
            label,
            delete_shared_contents: false,
        };
        let id = self.enqueue(job, Purpose::Plain, touched);
        if let Some(row) = self.transfers.row_mut(id) {
            row.reveal = reveal;
        }
    }

    /// The selection of pane `p` to the other pane's folder.
    pub fn send_to_other(&mut self, p: usize) {
        let paths: Vec<RPath> = self.panes[p].selected().into_iter().map(|e| e.path).collect();
        // Nothing goes to (or from) a pane that shows nothing.
        let (Some(from), Some(to)) = (self.panes[p].src.clone(), self.panes[self.other(p)].src.clone()) else { return };
        let to_dir = self.panes[self.other(p)].dir.clone();
        self.transfer(&from, paths, &to, to_dir, Op::Copy);
    }

    /// Asks before deleting (unless turned off), then deletes through the engine.
    pub fn ask_delete(&mut self, p: usize) {
        let sel = self.panes[p].selected();
        if sel.is_empty() {
            return;
        }
        // Shared items always get their own confirm, whatever the preference: what Delete
        // does to them (leave, keep) differs from what the person may expect.
        let site = self.panes[p].label().to_owned();
        if let Some(d) = crate::delete_sheet::SharedDelete::from_selection(p, site, sel.clone()) {
            self.sheet = Some(Sheet::DeleteShared(Box::new(d)));
            return;
        }
        let paths: Vec<RPath> = sel.iter().map(|e| e.path.clone()).collect();
        if self.prefs.confirm_deletes {
            let names = sel.into_iter().map(|e| e.name).collect();
            self.sheet = Some(Sheet::ConfirmDelete { pane: p, paths, names });
        } else {
            self.delete(p, paths);
        }
    }

    pub fn delete(&mut self, p: usize, paths: Vec<RPath>) {
        self.delete_with(p, paths, false);
    }

    /// Deletes through the engine. `delete_shared_contents` is the person's explicit "delete
    /// for everyone" on shared items; otherwise the engine leaves or keeps them.
    pub fn delete_with(&mut self, p: usize, paths: Vec<RPath>, delete_shared_contents: bool) {
        let Some(src) = self.panes[p].src.clone() else { return };
        let dir = self.panes[p].dir.clone();
        let label = format!("Deleting {} from {}", items_label(&paths), src.label);
        let job = Job { op: Op::Delete, src: src.remote.clone(), src_paths: paths, dst: None, dst_dir: None, conflict: ConflictPolicy::Skip, label, delete_shared_contents };
        self.enqueue(job, Purpose::Plain, vec![(src.session, dir)]);
        self.panes[p].sel.clear();
    }

    /// Copies the selection beside itself ("name 2.txt").
    pub fn duplicate(&mut self, p: usize) {
        let paths: Vec<RPath> = self.panes[p].selected().into_iter().map(|e| e.path).collect();
        if paths.is_empty() {
            return;
        }
        let Some(src) = self.panes[p].src.clone() else { return };
        let dir = self.panes[p].dir.clone();
        let label = format!("Duplicating {}", items_label(&paths));
        let job = Job {
            op: Op::Copy,
            src: src.remote.clone(),
            src_paths: paths,
            dst: Some(src.remote.clone()),
            dst_dir: Some(dir.clone()),
            conflict: ConflictPolicy::Rename,
            label,
            delete_shared_contents: false,
        };
        self.enqueue(job, Purpose::Plain, vec![(src.session, dir)]);
    }

    /// Starts renaming the cursor's row in place.
    pub fn begin_rename(&mut self, p: usize, tx: &mut Tx) {
        let pane = &mut self.panes[p];
        let name = pane.sel.cursor.clone().filter(|c| pane.sel.is_selected(c)).or_else(|| pane.selected().first().map(|e| e.name.clone()));
        let (Some(name), Some(src)) = (name, &pane.src) else { return };
        if !src.caps().server_rename {
            let msg = format!("{} can't rename items on the server.", src.label);
            self.notify(Tone::Warning, msg, None);
            return;
        }
        let mut edit = TextEdit::new(name.clone());
        // Select the name without its extension, as Finder does.
        let stem = name.rfind('.').filter(|&i| i > 0).unwrap_or(name.len());
        edit.anchor = 0;
        edit.focus = stem;
        pane.rename = Some(Rename { original: name, edit, error: None });
        tx.focus(format!("rename-{p}"), true);
    }

    pub fn cancel_rename(&mut self, p: usize, tx: &mut Tx) {
        self.panes[p].rename = None;
        tx.focus(format!("pane-{p}"), true);
    }

    pub fn commit_rename(&mut self, p: usize, tx: &mut Tx) {
        let taken = self.panes[p].names().into_iter().map(str::to_owned).collect::<std::collections::HashSet<String>>();
        let pane = &mut self.panes[p];
        let (Some(r), Some(src)) = (&mut pane.rename, &pane.src) else { return };
        let new = r.edit.text.trim().to_owned();
        if new == r.original {
            pane.rename = None;
            tx.focus(format!("pane-{p}"), true);
            return;
        }
        if let Some(why) = invalid_name(&new, src.is_local()) {
            r.error = Some(why);
            return;
        }
        if taken.contains(&new) {
            r.error = Some(format!("“{new}” is already taken. Please choose a different name."));
            return;
        }
        let (from, to) = (pane.dir.join(&r.original), pane.dir.join(&new));
        let remote = src.remote.clone();
        let (session, dir) = (src.session, pane.dir.clone());
        pane.rename = None;
        tx.focus(format!("pane-{p}"), true);
        self.run(move || remote.rename(&from, &to), move |ui, res| match res {
            Ok(()) => {
                for pane in &mut ui.panes {
                    if pane.session() == Some(session) && pane.dir == dir {
                        pane.reveal = Some(new.clone());
                    }
                }
                ui.refresh_where(session, &dir);
            }
            Err(e) => ui.notify_error(&e),
        });
    }

    /// Makes "untitled folder" (or "untitled file") and starts renaming it.
    pub fn new_item(&mut self, p: usize, folder: bool) {
        let pane = &self.panes[p];
        let Some(src) = &pane.src else { return };
        let name = unique_name(if folder { "untitled folder" } else { "untitled.txt" }, &pane.names());
        let path = pane.dir.join(&name);
        let remote = src.remote.clone();
        let (session, dir) = (src.session, pane.dir.clone());
        self.run(
            move || {
                if folder {
                    remote.mkdir(&path)
                } else {
                    let w = remote.write(&path, 0, Some(0))?;
                    w.finish()
                }
            },
            move |ui, res| match res {
                Ok(()) => {
                    if let Some(i) = ui.panes.iter().position(|x| x.session() == Some(session) && x.dir == dir) {
                        ui.panes[i].reveal = Some(name.clone());
                        ui.pending_rename = Some((i, name));
                    }
                    ui.refresh_where(session, &dir);
                }
                Err(e) => ui.notify_error(&e),
            },
        );
    }

    /// Opens the cursor's row: folders in the pane, files in their app.
    pub fn open_entry(&mut self, p: usize, name: &str) {
        let hidden = self.prefs.show_hidden;
        let Some(e) = self.panes[p].entry(name).cloned() else { return };
        match e.kind {
            EntryKind::Dir => self.panes[p].go(e.path, hidden),
            EntryKind::Symlink => self.panes[p].go_link(e.path, hidden),
            _ => self.open_file(p, &e.path, false),
        }
    }

    /// Opens the selection (Return, Cmd-Down, Open in the menu).
    pub fn open_selection(&mut self, p: usize) {
        let sel = self.panes[p].selected();
        match sel.as_slice() {
            [one] => self.open_entry(p, &one.name.clone()),
            many => {
                for e in many.iter().filter(|e| e.kind == EntryKind::File) {
                    self.open_file(p, &e.path, false);
                }
            }
        }
    }

    /// A file on the Mac opens in its app; a remote file is downloaded to the cache first
    /// (and with `watch`, saves are uploaded back).
    pub fn open_file(&mut self, p: usize, path: &RPath, watch: bool) {
        let Some(src) = self.panes[p].src.clone() else { return };
        if src.is_local() {
            let file = filewinch_local::local_path(path);
            let editor = if watch { self.prefs.editor.clone() } else { None };
            self.run(move || crate::edit_watch::open_with(&file, editor.as_deref()), |ui, res| {
                if let Err(e) = res {
                    ui.notify_error(&e);
                }
            });
            return;
        }
        self.start_remote_open(&src, path, watch);
    }

    /// Runs `work` on a worker thread and hands its result to `done` on the UI thread.
    pub fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
        done: impl FnOnce(&mut Ui, T) + Send + 'static,
    ) {
        if self.sync_io {
            let inbox = self.inbox.clone();
            let out = work();
            inbox.post(move |ui| done(ui, out));
            return;
        }
        let inbox = self.inbox.clone();
        std::thread::Builder::new()
            .name("filewinch-op".into())
            .spawn(move || {
                let out = work();
                inbox.post(move |ui| done(ui, out));
            })
            .ok();
    }

    /// Follows up on a finished job.
    pub fn job_finished(&mut self, id: JobId, ok: bool) {
        let Some(meta) = self.jobs.get(&id).cloned() else { return };
        for (session, dir) in &meta.touched {
            self.refresh_where(*session, dir);
        }
        let background = !matches!(meta.purpose, Purpose::Plain);
        // Names Windows refuses arrive under a substitute (`12:30.txt` as `12：30.txt`): say so.
        let renamed = filewinch_local::take_renamed();
        if let Some(first) = renamed.first() {
            let more = if renamed.len() > 1 { format!(" and {} more", renamed.len() - 1) } else { String::new() };
            let detail = renamed.iter().take(20).map(|r| format!("{} → {}", r.from, r.to)).collect::<Vec<_>>().join("\n");
            self.notify(
                Tone::Info,
                format!("Saved “{}” as “{}”{more}: Windows doesn't allow some characters in names.", first.from, first.to),
                Some(detail),
            );
        }
        match meta.purpose {
            Purpose::Plain => {}
            Purpose::Staging { files } => self.staging_finished(files, ok),
            Purpose::Open { edit } => self.edit_downloaded(edit, ok),
            Purpose::EditUpload { edit } => self.edit_uploaded(edit, ok),
        }
        if ok && background {
            // Background jobs leave no row behind when they succeed.
            self.transfers.rows.retain(|r| r.id != id);
            hub().engine.forget(id);
        }
        self.jobs.remove(&id);
    }
}
