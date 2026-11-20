//! Dragging rows: between panes (copy, Option to move), out to Finder, and files in from
//! Finder (upload). Out-to-Finder is macOS only (Rune's `drag_out`); on Windows and Linux rows
//! drag between panes through [`crate::drag_app`], and Download to… replaces the drag out.
//!
//! A system drag carries file paths. Rows on the Mac hand their real paths. Remote rows hand
//! copies staged in the cache, downloaded when the drag starts: Rune has no file promises, so a
//! drop in Finder before the copy is ready finds nothing, and the window says so. Drops on
//! Filewinch's own panes never use the staged copies; they copy from the source directly.

use std::collections::HashSet;
use std::path::PathBuf;

use filewinch_core::RPath;
use rune::{DragItem, DragOutcome, FileDialogOptions, Tx};

use crate::edit_watch::cache_dir;
use crate::pane::Source;
use crate::transfers::engine::{ConflictPolicy, Job, Op};
use crate::transfers::model::Purpose;
use crate::ui::{DragSrc, Tone, Ui};

/// Whether rows can be dragged out to the file manager (Rune's `drag_out` is macOS only).
pub const DRAG_OUT: bool = cfg!(target_os = "macos");

/// Where a remote item is staged for a drag out.
pub fn staged_path(session: u64, path: &RPath) -> PathBuf {
    cache_dir("drag").join(session.to_string()).join(path.as_str().trim_start_matches('/'))
}

impl Ui {
    /// The rows a drag starting on `name` carries: the selection if `name` is in it.
    pub(crate) fn drag_rows(&self, p: usize, name: &str) -> Vec<RPath> {
        let pane = &self.panes[p];
        if pane.sel.is_selected(name) {
            pane.selected().into_iter().map(|e| e.path).collect()
        } else {
            pane.entry(name).map(|e| vec![e.path.clone()]).unwrap_or_default()
        }
    }

    fn handed(&self, src: &Source, paths: &[RPath]) -> Vec<PathBuf> {
        paths
            .iter()
            .map(|p| if src.is_local() { filewinch_local::local_path(p) } else { staged_path(src.session, p) })
            .collect()
    }

    /// What the system drag of row `name` carries.
    pub fn drag_item(&self, p: usize, name: &str) -> Option<DragItem> {
        if self.panes[p].rename.is_some() {
            return None;
        }
        let paths = self.drag_rows(p, name);
        if paths.is_empty() {
            return None;
        }
        let src = self.panes[p].src.as_ref()?;
        let mut item = DragItem::new().within_app();
        for f in self.handed(src, &paths) {
            item = item.file(f);
        }
        Some(item)
    }

    pub fn drag_started(&mut self, p: usize, name: &str) {
        let Some(src) = self.panes[p].src.clone() else { return };
        let paths = self.drag_rows(p, name);
        let handed = self.handed(&src, &paths);
        if !src.is_local() {
            self.stage(&src, &paths);
        }
        self.drag = Some(DragSrc { pane: p, source: src, paths, handed });
    }

    pub fn drag_ended(&mut self, outcome: DragOutcome) {
        // A drop on one of our panes takes `drag`; anything left went elsewhere.
        let Some(d) = self.drag.as_ref() else { return };
        if outcome == DragOutcome::Cancelled || d.source.is_local() {
            return;
        }
        let missing: Vec<String> = d
            .handed
            .iter()
            .zip(&d.paths)
            .filter(|(h, _)| !h.exists())
            .map(|(_, p)| p.name().to_owned())
            .collect();
        if let Some(first) = missing.first() {
            let what = if missing.len() == 1 { format!("“{first}”") } else { format!("{} items", missing.len()) };
            self.notify(
                Tone::Info,
                format!("Filewinch is still downloading {what} for {}. Drag again once it's ready.", crate::theme::ui_os().file_manager()),
                None,
            );
        }
    }

    /// Downloads `paths` into the drag staging area (skipping ones already there).
    fn stage(&mut self, src: &Source, paths: &[RPath]) {
        let todo: Vec<RPath> = paths.iter().filter(|p| !staged_path(src.session, p).exists()).cloned().collect();
        let mut by_dir: std::collections::BTreeMap<RPath, Vec<RPath>> = Default::default();
        for p in todo {
            by_dir.entry(p.parent().unwrap_or_else(RPath::root)).or_default().push(p);
        }
        for (dir, paths) in by_dir {
            // Staged into a hidden incoming folder, then renamed into place whole, so Finder
            // never copies half a file.
            let incoming = cache_dir("drag").join(".incoming").join(format!("{}-{}", src.session, self.next_id()));
            if let Err(e) = std::fs::create_dir_all(&incoming) {
                self.notify(Tone::Error, "Couldn't prepare the drag: the cache folder isn't writable.", Some(e.to_string()));
                return;
            }
            let files = paths.iter().map(|p| (src.session, p.clone(), incoming.join(p.name()))).collect();
            let job = Job {
                op: Op::Copy,
                src: src.remote.clone(),
                src_paths: paths,
                dst: Some(Source::local().remote),
                dst_dir: Some(filewinch_local::rpath(&incoming)),
                conflict: ConflictPolicy::Overwrite,
                label: format!("Preparing items from {} for {}", dir.name(), crate::theme::ui_os().file_manager()),
                delete_shared_contents: false,
            };
            self.enqueue(job, Purpose::Staging { files }, Vec::new());
        }
    }

    pub fn staging_finished(&mut self, files: Vec<(u64, RPath, PathBuf)>, ok: bool) {
        if !ok {
            return; // the failed job shows in Transfers
        }
        for (session, path, incoming) in files {
            let dest = staged_path(session, &path);
            if let Some(parent) = dest.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::remove_dir_all(&dest).or_else(|_| std::fs::remove_file(&dest));
            if std::fs::rename(&incoming, &dest).is_err() {
                self.notify(Tone::Warning, format!("Couldn't stage “{}” for dragging.", path.name()), None);
            }
        }
    }

    /// Files dropped on pane `p` (on folder row `folder`, or the pane itself).
    pub fn dropped(&mut self, p: usize, folder: Option<&str>, files: &[PathBuf], tx: &mut Tx) {
        let pane = &self.panes[p];
        // An empty pane takes no drops (its view offers none); this keeps a stray one inert.
        let Some(to) = pane.src.clone() else {
            self.panes[p].drop_target = None;
            return;
        };
        let to_dir = folder.map_or_else(|| pane.dir.clone(), |f| pane.dir.join(f));
        self.panes[p].drop_target = None;
        let alt = tx.mods().alt;
        let dropped: HashSet<&PathBuf> = files.iter().collect();
        let ours = self.drag.take_if(|d| d.handed.iter().collect::<HashSet<_>>() == dropped);
        match ours {
            Some(d) => {
                // Within one site a drop on a folder moves, as in Finder; across sites it copies.
                let same = d.source.session == to.session;
                let op = if alt || (same && folder.is_some()) { Op::Move } else { Op::Copy };
                self.transfer(&d.source, d.paths, &to, to_dir, op);
            }
            None => {
                let paths = files.iter().map(|f| filewinch_local::rpath(f)).collect();
                self.transfer(&Source::local(), paths, &to, to_dir, Op::Copy);
            }
        }
        self.active = p;
    }

    /// Asks for a folder on this computer, then downloads pane `p`'s selection into it (the
    /// stand-in for dragging out to the file manager where Rune can't).
    pub fn download_to(&mut self, p: usize, tx: &mut Tx) {
        let Some(src) = self.panes[p].src.clone() else { return };
        let paths: Vec<RPath> = self.panes[p].selected().into_iter().map(|e| e.path).collect();
        if paths.is_empty() {
            return;
        }
        let options = FileDialogOptions { title: "Download to…".into(), directory: filewinch_os::dirs::home(), ..Default::default() };
        tx.open_folder(options, move |s: &mut Ui, result, _tx: &mut Tx| match result {
            Ok(Some(dir)) => s.transfer(&src, paths, &Source::local(), filewinch_local::rpath(&dir), crate::transfers::engine::Op::Copy),
            Ok(None) => {}
            Err(e) => s.notify(Tone::Error, "Couldn't show the folder picker.", Some(e.to_string())),
        });
    }

    /// Shows pane `p`'s first selected item (on this computer) in the file manager.
    pub fn reveal_selection(&mut self, p: usize) {
        let Some(e) = self.panes[p].selected().into_iter().next() else { return };
        self.reveal_local(filewinch_local::local_path(&e.path));
    }

    /// Shows `file` selected in the file manager; its folder if it's gone (renamed by a
    /// conflict, moved since).
    pub fn reveal_local(&mut self, file: PathBuf) {
        self.run(
            move || {
                let target = if file.exists() { file } else { file.parent().map(PathBuf::from).unwrap_or(file) };
                filewinch_os::launch::reveal(&target)
            },
            |ui, res| {
                if let Err(e) = res {
                    ui.notify(Tone::Error, format!("Couldn't show the item in {}.", crate::theme::ui_os().file_manager()), Some(e));
                }
            },
        );
    }
}
