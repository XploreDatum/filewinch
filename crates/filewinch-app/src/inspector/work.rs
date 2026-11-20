//! The panel's work: starting loads for a new selection, landing their answers, and the
//! actions its buttons take (Calculate, Download to preview, Retry, Rename, Apply).

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use filewinch_core::{Error, ErrorKind, RPath};
use rune::{ImageCache, TextEdit, Tx};

use super::load::{self, AUTO_SIZE_ITEMS, Cancel, Dates, Fetched, PREVIEW_MAX, PreviewKind, Sized, Tally};
use super::perms::{self, Applying, Confirm, PermsEdit};
use super::{Details, Load, Preview, Rename, Size};
use crate::pane::model::invalid_name;
use crate::transfers::engine::{ConflictPolicy, Job};
use crate::transfers::model::Purpose;
use crate::ui::{Tone, Ui};

impl Ui {
    /// The loads a new selection starts: the dates and (with nothing selected) the folder's own
    /// entry, a single file's preview, and a local folder's size when it's small. Remote folders
    /// are only sized when asked.
    pub(super) fn inspector_start(&mut self) {
        let Some(s) = &self.inspector.subject else { return };
        let (local, items) = (s.local, s.items.len());
        let one_dir = s.items.first().is_some_and(|e| e.is_dir()) && items == 1;
        if items == 0 || (items == 1 && local) {
            self.inspector_details();
        }
        if items == 1 {
            self.inspector_preview(false);
        }
        if local && (items == 0 || one_dir) {
            self.calculate_size(true);
        }
    }

    pub fn inspector_details(&mut self) {
        let Some(s) = &self.inspector.subject else { return };
        let (remote, local, dir) = (s.remote.clone(), s.local, s.dir.clone());
        let item = s.items.first().map(|e| e.path.clone());
        let work = move |()| -> Result<Details, Error> {
            let dates = |p: &RPath| if local { load::local_dates(p) } else { Dates::default() };
            match item {
                Some(path) => Ok(Details { dates: dates(&path), folder: None }),
                None => {
                    let folder = load::retrying(&Cancel::default(), || remote.stat(&dir))?;
                    Ok(Details { dates: dates(&dir), folder: Some(folder) })
                }
            }
        };
        self.inspector.details = Load::Loading;
        if self.inspector.sync_io {
            self.land_details(work(()));
        } else {
            self.inspector.details_req.request((), work);
        }
    }

    fn land_details(&mut self, r: Result<Details, Error>) {
        let i = &mut self.inspector;
        let folder_now = r.as_ref().is_ok_and(|d| d.folder.is_some());
        i.details = match r {
            Ok(d) => Load::Ready(d),
            Err(e) => Load::Failed(e),
        };
        if folder_now && let Some(m) = i.modes() {
            i.perms = PermsEdit::new(&m);
        }
    }

    /// The single file's preview: an image (fetched to the cache when remote, never over 20 MB
    /// unless `force`), or the first 64 KB of text. Supersedes any preview still loading.
    pub fn inspector_preview(&mut self, force: bool) {
        let i = &mut self.inspector;
        i.preview_cancel.cancel();
        i.preview_req.cancel();
        i.image_seen = false;
        let Some(s) = &i.subject else { return };
        let ([e], Some(kind)) = (s.items.as_slice(), s.items.first().and_then(load::preview_kind)) else {
            i.preview = Preview::None;
            return;
        };
        if kind == PreviewKind::Image && !force && e.size.is_some_and(|n| n > PREVIEW_MAX) {
            i.preview = Preview::TooLarge;
            return;
        }
        let (remote, e) = (s.remote.clone(), e.clone());
        let cache = (!s.local && kind == PreviewKind::Image).then(|| load::cache_file(s.session, &e.path));
        let cancel = Cancel::default();
        i.preview_cancel = cancel.clone();
        let got = Arc::new(AtomicU64::new(0));
        let total = if kind == PreviewKind::Text { e.size.map(|n| n.min(load::TEXT_MAX as u64)) } else { e.size };
        i.preview = Preview::Loading { got: got.clone(), total };
        let work = move |()| load::fetch(&*remote, &e, kind, cache, force, &cancel, &got);
        if i.sync_io {
            let r = work(());
            self.land_preview(r);
        } else {
            i.preview_req.request((), work);
        }
    }

    fn land_preview(&mut self, r: Result<Fetched, Error>) {
        let i = &mut self.inspector;
        i.preview = match r {
            Ok(Fetched::Image(bytes)) => {
                let images = i.images.get_or_insert_with(|| ImageCache::new(Arc::new(|| {})));
                let img = images.load(bytes);
                if i.sync_io {
                    // Headless: decode now, so the snapshot shows the picture.
                    let _ = img.wait();
                }
                Preview::Image(img)
            }
            Ok(Fetched::Text { text, truncated }) => Preview::Text { text, truncated },
            Ok(Fetched::Binary) => Preview::Binary,
            Err(e) if e.kind == ErrorKind::Cancelled => Preview::None,
            Err(e) if e.kind == ErrorKind::Unsupported => Preview::TooLarge,
            Err(e) => Preview::Failed(e),
        };
    }

    /// Sizes the folder (nothing selected), the selected folder, or every selected folder. `auto`
    /// (local folders only) gives up past a couple of thousand items and leaves it to a click.
    pub fn calculate_size(&mut self, auto: bool) {
        let i = &mut self.inspector;
        i.size_cancel.cancel();
        i.size_req.cancel();
        let Some(s) = &i.subject else { return };
        let roots: Vec<RPath> = if s.items.is_empty() { vec![s.dir.clone()] } else { s.items.iter().filter(|e| e.is_dir()).map(|e| e.path.clone()).collect() };
        if roots.is_empty() {
            return;
        }
        let remote = s.remote.clone();
        let (cancel, tally) = (Cancel::default(), Arc::new(Tally::default()));
        i.size_cancel = cancel.clone();
        i.size = Size::Counting(tally.clone());
        let cap = auto.then_some(AUTO_SIZE_ITEMS);
        let work = move |()| load::folder_size(&*remote, &roots, &cancel, &tally, cap);
        if i.sync_io {
            let r = work(());
            self.land_size(r);
        } else {
            i.size_req.request((), work);
        }
    }

    pub fn cancel_size(&mut self) {
        let i = &mut self.inspector;
        i.size_cancel.cancel();
        i.size_req.cancel();
        i.size = Size::NotAsked;
    }

    fn land_size(&mut self, r: Result<Sized, Error>) {
        self.inspector.size = match r {
            Ok(Sized::Done { bytes, files, folders }) => Size::Done { bytes, files, folders },
            Ok(Sized::TooMany) => Size::TooMany,
            Err(e) if e.kind == ErrorKind::Cancelled => Size::NotAsked,
            Err(e) => Size::Failed(e),
        };
    }

    /// Lands whatever finished since the last frame. True when something did.
    pub(super) fn inspector_land(&mut self) -> bool {
        let mut changed = false;
        if let Some(r) = self.inspector.details_req.take() {
            self.land_details(r);
            changed = true;
        }
        if let Some(r) = self.inspector.preview_req.take() {
            self.land_preview(r);
            changed = true;
        }
        if let Some(r) = self.inspector.size_req.take() {
            self.land_size(r);
            changed = true;
        }
        let i = &mut self.inspector;
        if let Preview::Image(img) = &i.preview
            && img.is_ready()
            && !i.image_seen
        {
            i.image_seen = true;
            changed = true;
        }
        changed
    }

    /// Starts renaming the single item in the panel (servers that can rename only).
    pub fn inspector_begin_rename(&mut self, tx: &mut Tx) {
        let Some(s) = &self.inspector.subject else { return };
        let [e] = s.items.as_slice() else { return };
        if !s.caps.server_rename {
            return;
        }
        let name = e.name.clone();
        let mut edit = TextEdit::new(name.clone());
        // The name without its extension, as the pane selects it.
        edit.anchor = 0;
        edit.focus = name.rfind('.').filter(|&i| i > 0).unwrap_or(name.len());
        self.inspector.rename = Some(Rename { edit, error: None, busy: false });
        tx.focus("inspector-rename", true);
    }

    /// The rename field's text, for its `on_edit`.
    pub fn inspector_rename_edit(&mut self) -> &mut TextEdit {
        match &mut self.inspector.rename {
            Some(r) => &mut r.edit,
            None => &mut self.sidebar_search,
        }
    }

    /// Renames on the server with the pane's rules (the OS's name rules on this computer, no
    /// name already taken); the pane then selects the new name, and the panel follows it.
    pub fn inspector_commit_rename(&mut self) {
        let Some(s) = &self.inspector.subject else { return };
        let ([e], Some(r)) = (s.items.as_slice(), &mut self.inspector.rename) else { return };
        let new = r.edit.text.trim().to_owned();
        if new == e.name {
            self.inspector.rename = None;
            return;
        }
        if let Some(why) = invalid_name(&new, s.local) {
            r.error = Some(why);
            return;
        }
        if self.panes[s.pane].entry(&new).is_some() {
            r.error = Some(format!("“{new}” is already taken. Please choose a different name."));
            return;
        }
        r.busy = true;
        r.error = None;
        let (remote, from, to) = (s.remote.clone(), e.path.clone(), s.dir.join(&new));
        let (session, dir) = (s.session, s.dir.clone());
        self.run(move || remote.rename(&from, &to), move |ui, res| match res {
            Ok(()) => {
                ui.inspector.rename = None;
                for pane in &mut ui.panes {
                    if pane.session() == Some(session) && pane.dir == dir {
                        pane.reveal = Some(new.clone());
                    }
                }
                ui.refresh_where(session, &dir);
            }
            Err(e) => {
                if let Some(r) = &mut ui.inspector.rename {
                    r.busy = false;
                    r.error = Some(e.message);
                }
            }
        });
    }

    /// Apply or Apply to Enclosed pressed: asks first when it reaches more than one item.
    pub fn request_apply(&mut self, enclosed: bool) {
        let n = self.inspector.subject.as_ref().map_or(0, |s| s.items.len());
        self.inspector.perms.confirm = if enclosed {
            Some(Confirm::Enclosed)
        } else if n > 1 {
            Some(Confirm::Bulk)
        } else {
            return self.apply_perms(false);
        };
    }

    /// Applies the staged change. Plain Apply sets each selected item's mode in the background,
    /// with progress and Stop in the panel. Apply to Enclosed queues the engine's `SetMode` job,
    /// which walks the selected folders and shows in the Transfers drawer (pause, cancel, skip,
    /// "N of M"); the panes showing those folders refresh when it ends.
    pub fn apply_perms(&mut self, enclosed: bool) {
        let i = &mut self.inspector;
        i.perms.confirm = None;
        let (Some(s), Some(_)) = (i.subject.clone(), i.modes()) else { return };
        if i.perms.octal_error.is_some() || i.perms.applying.is_some() {
            return;
        }
        let items: Vec<(RPath, u32, bool)> = if s.items.is_empty() {
            i.folder().and_then(|f| f.mode.map(|m| (f.path.clone(), m, true))).into_iter().collect()
        } else {
            s.items.iter().filter_map(|e| e.mode.map(|m| (e.path.clone(), m, e.is_dir()))).collect()
        };
        let (staged, folders_x) = (i.perms.staged, i.perms.folders_x);
        i.perms.error = None;
        i.perms.note = None;
        if enclosed {
            return self.apply_enclosed(&s, items, staged, folders_x);
        }
        let (cancel, tally) = (Cancel::default(), Arc::new(perms::Tally::default()));
        i.perms.applying = Some(Applying { cancel: cancel.clone(), tally: tally.clone() });
        let key = i.key.clone();
        let remote = s.remote.clone();
        let targets: Vec<(RPath, u32)> = items.into_iter().map(|(p, m, _)| (p, m)).collect();
        let (session, dir) = (s.session, s.dir.clone());
        self.run(move || perms::apply(&*remote, &targets, staged, &cancel, &tally), move |ui, applied| {
            ui.refresh_where(session, &dir);
            let (err, note) = perms::outcome(&applied);
            if ui.inspector.key != key {
                // The panel moved on (Apply from "Apply changes?"): say how it went here.
                if let Some(e) = err {
                    ui.notify(Tone::Error, e, None);
                }
                return;
            }
            let i = &mut ui.inspector;
            i.perms.applying = None;
            i.perms.error = err;
            i.perms.note = note;
            if applied.failures.is_empty() && !applied.cancelled {
                if let Some(s) = &mut i.subject {
                    for e in &mut s.items {
                        e.mode = e.mode.map(|m| staged.on(m));
                    }
                }
                i.perms.staged = perms::Staged::default();
                if let Some(m) = i.modes() {
                    i.perms.sync_octal(&m);
                }
            }
        });
    }

    /// Apply to Enclosed: one engine job over the selection (folders walked, links left alone).
    fn apply_enclosed(&mut self, s: &super::Subject, items: Vec<(RPath, u32, bool)>, staged: perms::Staged, folders_x: bool) {
        let paths: Vec<RPath> = items.iter().map(|t| t.0.clone()).collect();
        let label = format!("Changing permissions of {}", crate::actions::items_label(&paths));
        // The folder the pane shows, and every selected folder (a pane may be inside one).
        let mut touched = vec![(s.session, s.dir.clone())];
        touched.extend(items.iter().filter(|t| t.2).map(|t| (s.session, t.0.clone())));
        let job = Job {
            op: staged.op(folders_x),
            src: s.remote.clone(),
            src_paths: paths,
            dst: None,
            dst_dir: None,
            conflict: ConflictPolicy::Skip,
            label,
            delete_shared_contents: false,
        };
        self.enqueue(job, Purpose::Plain, touched);
        self.prefs.transfers_open = true;
        let i = &mut self.inspector;
        i.perms.staged = perms::Staged::default();
        if let Some(m) = i.modes() {
            i.perms.sync_octal(&m);
        }
        i.perms.note = Some("Changing permissions: progress is in Transfers.".into());
    }

    pub fn cancel_apply(&mut self) {
        if let Some(a) = &self.inspector.perms.applying {
            a.cancel.cancel();
        }
    }

    pub fn toggle_perm_bit(&mut self, bit: u32) {
        if let Some(m) = self.inspector.modes() {
            self.inspector.perms.toggle(&m, bit);
        }
    }

    pub fn revert_perms(&mut self) {
        if let Some(m) = self.inspector.modes() {
            self.inspector.perms.revert(&m);
        }
    }
}
