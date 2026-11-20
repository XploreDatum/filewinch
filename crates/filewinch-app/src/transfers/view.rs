//! The Transfers drawer: a row per job with progress, speed and time left, its controls,
//! failed items, and the "already exists" question.

use rune::{El, ScrollSpec, col, icon, row, text};
use rune_kit::{Button, ButtonVariant, Checkbox, ControlSize, IconButton, IconButtonVariant, Kit, Progress as Bar};

use super::engine::{ConflictAnswer, ConflictChoice, JobId, JobState};
use super::model::{Direction, Row};
use crate::edit_watch::EditState;
use crate::fmt;
use crate::hub::hub;
use crate::icons;
use crate::theme::Paint;
use crate::ui::Ui;

impl Ui {
    pub fn answer(&mut self, id: JobId, choice: ConflictChoice) {
        let apply_to_all = self.transfers.row(id).is_some_and(|r| r.apply_to_all);
        if let Some(r) = self.transfers.row_mut(id) {
            r.question = None;
        }
        hub().engine.answer(id, ConflictAnswer { choice, apply_to_all });
    }

    pub fn clear_completed(&mut self) {
        for id in self.transfers.clear_completed() {
            hub().engine.forget(id);
        }
    }

    pub fn dismiss_job(&mut self, id: JobId) {
        self.transfers.rows.retain(|r| r.id != id);
        hub().engine.forget(id);
    }
}

fn ctl(k: &Kit, key: String, glyph: &'static rune::Icon, label: &str, f: impl Fn(&mut Ui) + 'static) -> El {
    IconButton::new(glyph).variant(IconButtonVariant::Ghost).control_size(ControlSize::Small).label(label).help(label).on_press(f).build(k).key(key)
}

fn detail_line(r: &Row) -> String {
    let p = &r.progress;
    let mut parts = Vec::new();
    // Deleting moves no bytes: count the items.
    if r.direction == Direction::Delete {
        if let Some(n) = p.files_total {
            parts.push(if n == 1 { format!("{} of 1 item", p.files_done) } else { format!("{} of {n} items", p.files_done) });
        }
        return parts.join(" · ");
    }
    match p.bytes_total {
        Some(t) => parts.push(format!("{} of {}", fmt::size(p.bytes_done), fmt::size(t))),
        None if p.bytes_done > 0 => parts.push(fmt::size(p.bytes_done)),
        None => {}
    }
    if let Some(n) = p.files_total.filter(|n| *n > 1) {
        parts.push(format!("{} of {n} files", p.files_done));
    }
    if matches!(r.state, JobState::Running) {
        if p.bytes_per_sec >= 1.0 {
            parts.push(fmt::speed(p.bytes_per_sec));
        }
        if let Some(eta) = p.eta {
            parts.push(format!("{} left", fmt::duration(eta)));
        }
    }
    parts.join(" · ")
}

fn question(r: &Row, k: &Kit, paint: &Paint) -> Option<El> {
    let ask = r.question.as_ref()?;
    let id = r.id;
    let now = crate::fmt::now();
    let describe = |e: &filewinch_core::Entry| {
        let size = e.size.map(fmt::size).unwrap_or_else(|| "—".into());
        let date = e.modified.map(|m| fmt::date(m, now)).unwrap_or_else(|| "unknown date".into());
        format!("{size}, {date}")
    };
    let newer = match (ask.src.modified, ask.dst.modified) {
        (Some(a), Some(b)) if a > b => " (the one you're copying is newer)",
        (Some(a), Some(b)) if a < b => " (the existing one is newer)",
        _ => "",
    };
    let primary = crate::theme::primary(k);
    let b = |label: &str, key: &str, choice: ConflictChoice, v: ButtonVariant| {
        // The default action (Contrast) takes the accent fill.
        let kit = if v == ButtonVariant::Contrast { &primary } else { k };
        Button::new(label).variant(v).control_size(ControlSize::Small).on_press(move |s: &mut Ui| s.answer(id, choice)).build(kit).key(key.to_owned())
    };
    let mut buttons = row().gap(6.0).center_y()
        .child(b("Skip", "skip", ConflictChoice::Skip, ButtonVariant::Ghost))
        .child(b("Keep Both", "keep-both", ConflictChoice::Rename, ButtonVariant::Neutral));
    if ask.can_resume {
        buttons = buttons.child(b("Resume", "resume", ConflictChoice::Resume, ButtonVariant::Neutral));
    }
    buttons = buttons.child(b("Replace", "replace", ConflictChoice::Overwrite, ButtonVariant::Contrast));
    let all = Checkbox::new(r.apply_to_all).label("Do this for all remaining items")
        .on_toggle(move |s: &mut Ui| {
            if let Some(r) = s.transfers.row_mut(id) {
                r.apply_to_all = !r.apply_to_all;
            }
        })
        .build(k).key("apply-all");
    Some(col().key("question").role("alertdialog").label(format!("“{}” already exists", ask.dst.name)).gap(8.0).w_full().padding(10.0).radius(6.0)
        .fill(k.alpha(paint.warning, 0.10)).border(1.0, k.alpha(paint.warning, 0.4))
        .child(row().gap(8.0).center_y()
            .child(icon(&icons::WARNING, 16.0).color(paint.warning).a11y_hidden())
            .child(text(format!("“{}” already exists here{newer}.", ask.dst.name)).font(k.ty.text_small_medium).color(k.c.text_base).wrap()))
        .child(text(format!("Existing: {}   ·   Incoming: {}", describe(&ask.dst), describe(&ask.src))).font(k.ty.text_xsmall).color(k.c.text_muted))
        .child(row().w_full().center_y().gap(8.0).child(all.grow(1.0)).child(buttons)))
}

fn job_row(r: &Row, k: &Kit, paint: &Paint) -> El {
    let id = r.id;
    let failed = matches!(r.state, JobState::Failed(_));
    let tone = match &r.state {
        JobState::Failed(_) => paint.danger,
        JobState::Done => paint.success,
        JobState::Retrying { .. } | JobState::WaitingForAnswer => paint.warning,
        _ => k.c.text_muted,
    };
    // Which way the job goes; its colour says how it's doing.
    let glyph = match r.direction {
        Direction::Upload => &icons::UPLOAD,
        Direction::Download => &icons::DOWNLOAD,
        Direction::Across => &icons::ACROSS,
        Direction::Delete => &icons::TRASH,
    };
    let mut controls = row().gap(2.0).center_y();
    match &r.state {
        JobState::Running | JobState::Scanning | JobState::Queued | JobState::Retrying { .. } | JobState::WaitingForAnswer => {
            controls = controls
                .child(ctl(k, "pause".into(), &icons::PAUSE, "Pause", move |_: &mut Ui| hub().engine.pause(id)))
                .child(ctl(k, "cancel".into(), &icons::STOP, "Cancel", move |_: &mut Ui| hub().engine.cancel(id)));
        }
        JobState::Paused => {
            controls = controls
                .child(ctl(k, "resume".into(), &icons::PLAY, "Resume", move |_: &mut Ui| hub().engine.resume(id)))
                .child(ctl(k, "cancel".into(), &icons::STOP, "Cancel", move |_: &mut Ui| hub().engine.cancel(id)));
        }
        JobState::Failed(_) | JobState::Cancelled => {
            controls = controls
                .child(ctl(k, "retry".into(), &icons::RETRY, "Retry", move |s: &mut Ui| {
                    if let Some(r) = s.transfers.row_mut(id) {
                        r.failures.clear();
                    }
                    hub().engine.retry(id);
                }))
                .child(ctl(k, "dismiss".into(), &icons::STOP, "Remove from list", move |s: &mut Ui| s.dismiss_job(id)));
        }
        JobState::Done => {
            if let Some(file) = r.reveal.clone() {
                let label = format!("Show in {}", crate::theme::ui_os().file_manager());
                controls = controls.child(ctl(k, "reveal".into(), &icons::FOLDER, &label, move |s: &mut Ui| s.reveal_local(file.clone())));
            }
            controls = controls.child(ctl(k, "dismiss".into(), &icons::STOP, "Remove from list", move |s: &mut Ui| s.dismiss_job(id)));
        }
    }
    let fraction = match r.state {
        JobState::Done => Some(1.0),
        _ => r.fraction(),
    };
    let bar = Bar::new(fraction).build(k).key("bar").label(format!("{} progress", r.label)).w_full();
    let detail = detail_line(r);
    let mut info = col().gap(4.0).grow(1.0).min_w(0.0)
        .child(row().gap(8.0).center_y().min_w(0.0)
            .child(text(r.label.clone()).font(k.ty.text_small_medium).color(k.c.text_base).truncate().shrink().min_w(0.0))
            .child(rune::el().grow(1.0))
            .child(text(r.state_label()).key("state").font(k.ty.text_xsmall).color(tone).truncate().max_w(280.0)))
        .child(bar);
    if !detail.is_empty() || r.progress.current.is_some() {
        let current = r.progress.current.as_ref().filter(|_| matches!(r.state, JobState::Running)).map(|c| format!("  {}", c.name())).unwrap_or_default();
        info = info.child(text(format!("{detail}{current}")).font(k.ty.text_xsmall).color(k.c.text_muted).truncate());
    }
    let mut c = col().key(format!("job-{id}")).role("listitem").label(format!("{}, {}", r.label, r.state_label())).w_full().gap(6.0).px(12.0).py(8.0)
        .border_sides([0.0, 0.0, 1.0, 0.0], paint.hair)
        .child(row().w_full().gap(10.0).center_y()
            .child(icon(glyph, 16.0).color(tone).a11y_hidden())
            .child(info)
            .child(controls));
    if let Some(q) = question(r, k, paint) {
        c = c.child(q);
    }
    if !r.failures.is_empty() || failed {
        let n = r.failures.len();
        let toggle = row().key("failures-toggle").role("button").label(if r.expanded { "Hide failed items" } else { "Show failed items" }).expanded(r.expanded)
            .gap(4.0).center_y().h(20.0).px(4.0).radius(4.0).hover(k.c.hover)
            .on_press(move |s: &mut Ui| {
                if let Some(r) = s.transfers.row_mut(id) {
                    r.expanded = !r.expanded;
                }
            })
            .child(icon(if r.expanded { k.glyph.chevron_down } else { &icons::FORWARD }, 12.0).color(k.c.icon_muted).a11y_hidden())
            .child(text(if n == 1 { "1 item failed".to_owned() } else { format!("{n} items failed") }).font(k.ty.text_xsmall_medium).color(paint.danger).a11y_hidden());
        let mut f = col().key("failures").gap(2.0).pl(26.0).child(toggle);
        if r.expanded {
            if let JobState::Failed(e) = &r.state
                && let Some(d) = &e.detail
            {
                f = f.child(text(d.clone()).font(k.ty.text_xsmall).color(k.c.text_muted).wrap().selectable(true));
            }
            for (i, (path, err)) in r.failures.iter().take(50).enumerate() {
                f = f.child(row().key(format!("fail-{i}")).gap(8.0).items_start().w_full()
                    .child(text(path.to_string()).font(k.ty.mono).color(k.c.text_base).truncate().w(220.0))
                    .child(text(err.message.clone()).font(k.ty.text_xsmall).color(k.c.text_muted).wrap().grow(1.0).min_w(0.0).selectable(true)));
            }
            if n > 50 {
                f = f.child(text(format!("and {} more", n - 50)).font(k.ty.text_xsmall).color(k.c.text_faint));
            }
        }
        c = c.child(f);
    }
    c
}

fn edits(s: &Ui, k: &Kit, paint: &Paint) -> Option<El> {
    let list: Vec<_> = s.edits.list.iter().filter(|e| e.watch).collect();
    if list.is_empty() {
        return None;
    }
    let mut c = col().key("edits").w_full().px(12.0).py(6.0).gap(4.0).border_sides([0.0, 0.0, 1.0, 0.0], paint.hair)
        .child(text("Editing").font(crate::theme::section(k)).color(k.c.text_muted));
    for e in list {
        let id = e.id;
        let (status, tone) = match &e.state {
            EditState::Downloading => ("Downloading…".to_owned(), k.c.text_muted),
            EditState::Watching => ("Watching for saves".to_owned(), paint.success),
            EditState::Uploading => ("Uploading your changes…".to_owned(), paint.accent),
            EditState::Opened => ("Opened".to_owned(), k.c.text_muted),
            EditState::Failed(m) => (m.clone(), paint.danger),
        };
        c = c.child(row().key(format!("edit-{id}")).role("listitem").label(format!("Editing {}, {status}", e.path.name())).w_full().h(26.0).gap(8.0).center_y()
            .child(icon(&icons::FILE_TEXT, 14.0).color(k.c.icon_muted).a11y_hidden())
            .child(text(format!("{} on {}", e.path.name(), e.label)).font(k.ty.text_small).color(k.c.text_base).truncate().grow(1.0).min_w(0.0).a11y_hidden())
            .child(text(status).font(k.ty.text_xsmall).color(tone).a11y_hidden())
            .child(Button::new("Stop Editing").variant(ButtonVariant::Ghost).control_size(ControlSize::Small).on_press(move |s: &mut Ui| s.stop_edit(id)).build(k).key("stop-edit")));
    }
    Some(c)
}

pub fn drawer(s: &Ui, k: &Kit, paint: &Paint) -> El {
    let done = s.transfers.rows.iter().any(|r| matches!(r.state, JobState::Done | JobState::Cancelled));
    let head = row().w_full().h(34.0).px(12.0).gap(8.0).center_y().border_sides([0.0, 0.0, 1.0, 0.0], paint.hair)
        .child(text("Transfers").font(k.ty.text_small_medium).color(k.c.text_base).role("heading").level(2))
        .child(text(s.transfers.summary().unwrap_or_else(|| if s.transfers.rows.is_empty() { "Nothing transferring".into() } else { "All done".into() }))
            .key("transfers-summary").font(k.ty.text_xsmall).color(k.c.text_muted).live_region(rune::LiveRegion::Polite))
        .child(rune::el().grow(1.0))
        .child(Button::new("Clear Completed").variant(ButtonVariant::Ghost).control_size(ControlSize::Small).disabled(!done)
            .on_press(|s: &mut Ui| s.clear_completed()).build(k).key("clear-completed"))
        .child(IconButton::new(k.glyph.close).variant(IconButtonVariant::Ghost).control_size(ControlSize::Small).label("Hide transfers")
            .on_press(|s: &mut Ui| s.prefs.transfers_open = false).build(k).key("hide-transfers"));
    let mut list = col().key("jobs").role("list").label("Transfers").w_full();
    if let Some(e) = edits(s, k, paint) {
        list = list.child(e);
    }
    if s.transfers.rows.is_empty() {
        list = list.child(text(format!("Drag files between panes, or in from {}, to transfer them.", crate::theme::ui_os().file_manager())).key("transfers-empty").font(k.ty.text_small).color(k.c.text_faint).padding(16.0));
    }
    // Questions first, then running, then the rest, newest first within each.
    let mut rows: Vec<&Row> = s.transfers.rows.iter().collect();
    rows.sort_by_key(|r| (r.question.is_none(), r.state.is_finished(), std::cmp::Reverse(r.id)));
    for r in rows {
        list = list.child(job_row(r, k, paint));
    }
    col().key("transfers").role("region").label("Transfers").w_full().h(if s.transfers.rows.is_empty() && s.edits.list.is_empty() { 96.0 } else { 300.0 }).fill(paint.list).border_sides([1.0, 0.0, 0.0, 0.0], paint.hair)
        .child(head)
        .child(col().w_full().grow(1.0).min_h(0.0).scroll(ScrollSpec { stick_bottom: false }).child(list))
}
