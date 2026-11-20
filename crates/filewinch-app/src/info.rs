//! Get Info: what an item is, where, how big, when changed, and a permissions editor (octal
//! and checkboxes kept in step).

use std::sync::Arc;

use filewinch_core::{Entry, EntryKind, Remote};
use rune::machine::Attempt;
use rune::{Align, El, TextEdit, Tx, col, icon, row, text};
use rune_kit::{Button, ButtonVariant, Checkbox, ControlSize, Dialog, Kit, Spinner, TextInput};

use crate::fmt;
use crate::pane::model::file_kind;
use crate::theme::Paint;
use crate::ui::{Sheet, Ui};

pub enum FolderSize {
    NotAsked,
    Counting(Attempt),
    Done { bytes: u64, files: u64 },
    Failed(String),
}

pub struct Info {
    pub pane: usize,
    pub session: u64,
    pub site: String,
    pub entry: Entry,
    pub remote: Arc<dyn Remote>,
    pub can_chmod: bool,
    pub mode: Option<u32>,
    pub octal: TextEdit,
    pub octal_error: Option<String>,
    pub saving: bool,
    pub error: Option<String>,
    pub size: FolderSize,
}

impl Info {
    pub fn dirty(&self) -> bool {
        self.mode.is_some() && self.mode != self.entry.mode
    }
}

/// Bytes and files under `dir`, walking iteratively (no recursion limit).
fn folder_size(remote: &dyn Remote, dir: &filewinch_core::RPath) -> filewinch_core::Result<(u64, u64)> {
    let (mut bytes, mut files) = (0u64, 0u64);
    let mut stack = vec![dir.clone()];
    while let Some(d) = stack.pop() {
        for e in remote.list(&d)? {
            match e.kind {
                EntryKind::Dir => stack.push(e.path),
                _ => {
                    bytes += e.size.unwrap_or(0);
                    files += 1;
                }
            }
        }
    }
    Ok((bytes, files))
}

impl Ui {
    pub fn open_info(&mut self, p: usize) {
        let pane = &self.panes[p];
        let Some(src) = &pane.src else { return };
        let entry = match pane.selected().into_iter().next() {
            Some(e) => e,
            None => return,
        };
        let caps = src.caps();
        let mode = entry.mode;
        self.sheet = Some(Sheet::Info(Box::new(Info {
            pane: p,
            session: src.session,
            site: src.label.clone(),
            octal: TextEdit::new(mode.map(|m| format!("{:03o}", m & 0o7777)).unwrap_or_default()),
            can_chmod: caps.permissions && mode.is_some(),
            mode,
            remote: src.remote.clone(),
            entry,
            octal_error: None,
            saving: false,
            error: None,
            size: FolderSize::NotAsked,
        })));
    }

    fn info_mut(&mut self) -> Option<&mut Info> {
        match &mut self.sheet {
            Some(Sheet::Info(i)) => Some(i),
            _ => None,
        }
    }

    fn toggle_bit(&mut self, bit: u32) {
        if let Some(i) = self.info_mut() {
            let m = i.mode.unwrap_or(0) ^ bit;
            i.mode = Some(m);
            i.octal = TextEdit::new(format!("{:03o}", m));
            i.octal_error = None;
        }
    }

    fn octal_changed(&mut self) {
        if let Some(i) = self.info_mut() {
            match fmt::parse_octal(&i.octal.text) {
                Some(m) => {
                    i.mode = Some(m);
                    i.octal_error = None;
                }
                None => i.octal_error = Some("Use three or four digits from 0 to 7, like 644.".into()),
            }
        }
    }

    fn apply_mode(&mut self) {
        let Some(i) = self.info_mut() else { return };
        let Some(mode) = i.mode.filter(|_| i.octal_error.is_none()) else { return };
        i.saving = true;
        i.error = None;
        let (remote, path) = (i.remote.clone(), i.entry.path.clone());
        let (session, dir) = (i.session, path.parent().unwrap_or_else(filewinch_core::RPath::root));
        self.run(move || remote.set_mode(&path, mode), move |ui, res| {
            if let Some(i) = ui.info_mut() {
                i.saving = false;
                match &res {
                    Ok(()) => i.entry.mode = Some(mode),
                    Err(e) => i.error = Some(e.message.clone()),
                }
            }
            if res.is_ok() {
                ui.refresh_where(session, &dir);
            }
        });
    }

    fn count_folder(&mut self) {
        let attempt = self.seq.next();
        let Some(i) = self.info_mut() else { return };
        i.size = FolderSize::Counting(attempt);
        let (remote, path) = (i.remote.clone(), i.entry.path.clone());
        self.run(move || folder_size(&*remote, &path), move |ui, res| {
            if let Some(i) = ui.info_mut()
                && matches!(i.size, FolderSize::Counting(a) if a == attempt)
            {
                i.size = match res {
                    Ok((bytes, files)) => FolderSize::Done { bytes, files },
                    Err(e) => FolderSize::Failed(e.message),
                };
            }
        });
    }
}

fn fact(k: &Kit, key: &str, label: &str, value: String) -> El {
    row().key(key.to_owned()).w_full().gap(12.0).items_start().label(format!("{label}: {value}"))
        .child(text(label.to_owned()).font(k.ty.text_small).color(k.c.text_muted).w(96.0).align(Align::End).a11y_hidden())
        .child(text(value).font(k.ty.text_small).color(k.c.text_base).wrap().grow(1.0).min_w(0.0).selectable(true).a11y_hidden())
}

fn perms_grid(i: &Info, k: &Kit) -> El {
    let mode = i.mode.unwrap_or(0);
    let mut grid = col().key("perm-grid").gap(4.0);
    let mut head = row().gap(4.0).child(text("").w(64.0));
    for h in ["Read", "Write", "Execute"] {
        head = head.child(text(h).font(k.ty.text_xsmall_medium).color(k.c.text_muted).w(64.0).a11y_hidden());
    }
    grid = grid.child(head);
    for (who, shift) in [("Owner", 6), ("Group", 3), ("Everyone", 0)] {
        let mut r = row().gap(4.0).center_y().child(text(who).font(k.ty.text_small).color(k.c.text_base).w(64.0).a11y_hidden());
        for (what, bit) in [("read", 4u32), ("write", 2), ("execute", 1)] {
            let b = bit << shift;
            let cb = Checkbox::new(mode & b != 0)
                .label(format!("{who} can {what}"))
                .hide_label()
                .disabled(!i.can_chmod || i.saving)
                .on_toggle(move |s: &mut Ui| s.toggle_bit(b))
                .build(k)
                .key(format!("perm-{}-{what}", who.to_lowercase()));
            r = r.child(row().w(64.0).child(cb));
        }
        grid = grid.child(r);
    }
    grid
}

pub fn view(i: &Info, k: &Kit, paint: &Paint, layer: &mut Vec<El>) {
    let e = &i.entry;
    let kind = file_kind(e);
    let now = crate::fmt::now();
    let title = row().gap(10.0).center_y()
        .child(icon(kind.icon(), 32.0).color(kind.tint(paint.folder, k.c.icon_muted, paint.dark)).a11y_hidden())
        .child(col().gap(2.0).min_w(0.0)
            .child(text(e.name.clone()).font(k.ty.ui_medium).color(k.c.text_strong).truncate())
            .child(text(kind.describe(&e.name)).font(k.ty.text_small).color(k.c.text_muted)));
    let mut facts = col().key("facts").gap(6.0).w_full()
        .child(fact(k, "where", "Where", format!("{} on {}", e.path.parent().map_or("/".into(), |p| p.to_string()), i.site)));
    let size = match (e.kind, &i.size) {
        (EntryKind::Dir, FolderSize::Done { bytes, files }) => format!("{} ({}) in {files} files", fmt::size(*bytes), fmt::exact_bytes(*bytes)),
        (EntryKind::Dir, FolderSize::Counting(_)) => "Counting…".into(),
        (EntryKind::Dir, FolderSize::Failed(m)) => m.clone(),
        (EntryKind::Dir, FolderSize::NotAsked) => "—".into(),
        _ => e.size.map(|s| format!("{} ({})", fmt::size(s), fmt::exact_bytes(s))).unwrap_or_else(|| "Unknown".into()),
    };
    let mut size_row = row().w_full().gap(8.0).center_y().child(fact(k, "size", "Size", size).grow(1.0));
    if e.kind == EntryKind::Dir {
        match i.size {
            FolderSize::Counting(_) => size_row = size_row.child(Spinner::new().size(14.0).build(k)),
            _ => {
                size_row = size_row.child(Button::new("Calculate").control_size(ControlSize::Small).variant(ButtonVariant::Ghost)
                    .on_press(|s: &mut Ui| s.count_folder()).build(k).key("calculate"));
            }
        }
    }
    facts = facts.child(size_row);
    facts = facts.child(fact(k, "modified", "Modified", e.modified.map(|m| fmt::date(m, now)).unwrap_or_else(|| "Unknown".into())));
    if let (Some(o), g) = (&e.owner, &e.group) {
        facts = facts.child(fact(k, "owner", "Owner", match g { Some(g) => format!("{o} (group {g})"), None => o.clone() }));
    }
    if let Some(t) = &e.link_target {
        facts = facts.child(fact(k, "target", "Points to", t.clone()));
    }
    for (name, value) in &e.extra {
        facts = facts.child(fact(k, &format!("extra-{name}"), name, value.clone()));
    }
    let mut perms = col().key("permissions").gap(10.0).w_full().pt(12.0).border_sides([1.0, 0.0, 0.0, 0.0], paint.hair)
        .child(text("Permissions").font(k.ty.text_small_medium).color(k.c.text_base).role("heading").level(3));
    if i.mode.is_none() {
        perms = perms.child(text(format!("{} doesn't report permissions.", i.site)).font(k.ty.text_small).color(k.c.text_muted));
    } else {
        let octal = TextInput::new(&i.octal)
            .label("Permissions in octal")
            .width(Some(72.0))
            .invalid(i.octal_error.is_some())
            .disabled(!i.can_chmod || i.saving)
            .on_edit(|s: &mut Ui| match &mut s.sheet {
                Some(Sheet::Info(i)) => &mut i.octal,
                _ => &mut s.sidebar_search,
            })
            .on_change(|s: &mut Ui, _tx: &mut Tx| s.octal_changed())
            .on_submit(|s: &mut Ui, _tx: &mut Tx| s.apply_mode())
            .text_style(k.ty.mono)
            .build(k)
            .key("octal");
        let apply = Button::new(if i.saving { "Applying…" } else { "Apply" }).variant(ButtonVariant::Contrast).control_size(ControlSize::Small)
            .disabled(!i.dirty() || i.octal_error.is_some() || i.saving || !i.can_chmod)
            .on_press(|s: &mut Ui| s.apply_mode()).build(&crate::theme::primary(k)).key("apply-mode");
        let mut line = row().gap(12.0).center_y().child(perms_grid(i, k)).child(col().gap(6.0)
            .child(octal)
            .child(text(fmt::mode_string(i.mode.unwrap_or(0))).font(k.ty.mono).color(k.c.text_muted).a11y_hidden())
            .child(apply));
        if !i.can_chmod {
            line = line.child(text("This server doesn't let Filewinch change permissions.").font(k.ty.text_xsmall).color(k.c.text_muted).wrap().max_w(160.0));
        }
        perms = perms.child(line);
        if let Some(err) = i.octal_error.as_ref().or(i.error.as_ref()) {
            perms = perms.child(text(err.clone()).key("perm-error").font(k.ty.text_small).color(paint.danger).wrap().live_region(rune::LiveRegion::Polite));
        }
    }
    let body = col().key("info-body").gap(14.0).px(16.0).pb(16.0).w_full().child(title).child(facts).child(perms);
    Dialog::new("info", format!("{} Info", e.name)).open(true).width(460.0).fit().child(body)
        .on_close(|s: &mut Ui| s.sheet = None)
        .build(k, layer);
}
