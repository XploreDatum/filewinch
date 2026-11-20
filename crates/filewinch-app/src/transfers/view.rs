//! The Transfers drawer: a row per job with progress, speed and time left, its controls,
//! failed items, and the "already exists" question.

use rune::{El, PopupItem, PopupMenu, ScrollSpec, Tx, col, icon, row, text};
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

/// The caution beside choices above 3 files at once.
pub const FILES_CAUTION: &str = "More at once can be unstable; some servers limit connections.";

/// "3 at a time".
pub fn files_label(n: usize) -> String {
    format!("{n} at a time")
}

impl Ui {
    /// Sets how many files job `id` moves at once, live.
    pub fn set_job_files(&mut self, id: JobId, n: usize) {
        let n = n.clamp(1, super::engine::MAX_FILES_PER_JOB_LIVE);
        hub().engine.set_job_files(id, n);
        if let Some(r) = self.transfers.row_mut(id) {
            r.files_at_once = n;
        }
    }
}

/// The per-job "3 at a time ▾" control: a native menu of 1–16 where there is one, else − and +.
fn files_control(r: &Row, k: &Kit) -> El {
    let (id, n) = (r.id, r.files_at_once);
    let label = format!("Files at a time, {n}");
    let help = if n > 3 { FILES_CAUTION.to_owned() } else { "How many of this job's files move at once.".to_owned() };
    if !rune::popup_menu_supported() {
        let step = |key: &str, glyph: &'static rune::Icon, what: &str, to: usize, off: bool| {
            IconButton::new(glyph).variant(IconButtonVariant::Ghost).control_size(ControlSize::Small).label(what).disabled(off)
                .on_press(move |s: &mut Ui| s.set_job_files(id, to)).build(k).key(key.to_owned())
        };
        let max = super::engine::MAX_FILES_PER_JOB_LIVE;
        return row().key("files-at-once").role("spinbutton").label("Files at a time").a11y_value(n.to_string())
            .range_value(n as f64, 1.0, max as f64).help(help).gap(2.0).center_y()
            .on_step(move |s: &mut Ui, steps, _tx: &mut Tx| s.set_job_files(id, (n as i64 + steps as i64).clamp(1, max as i64) as usize))
            .child(step("fewer-files", &icons::MINUS, "Fewer files at a time", n.saturating_sub(1), n <= 1))
            .child(text(files_label(n)).font(k.ty.text_xsmall).color(k.c.text_muted).a11y_hidden())
            .child(step("more-files", &icons::PLUS, "More files at a time", n + 1, n >= max));
    }
    Button::new(format!("{} ▾", files_label(n))).variant(ButtonVariant::Ghost).control_size(ControlSize::Small).expanded(false)
        .on_press_tx(move |_: &mut Ui, tx: &mut Tx| {
            let mut menu = PopupMenu::new();
            for i in 1..=super::engine::MAX_FILES_PER_JOB_LIVE {
                menu = menu.item(PopupItem::new(i.to_string(), files_label(i)).checked(i == n));
            }
            menu = menu.separator().item(PopupItem::new("caution", FILES_CAUTION).enabled(false)).selected(n.to_string());
            tx.popup_menu(menu, format!("job-{id}/files-at-once"), move |s: &mut Ui, picked, _tx: &mut Tx| {
                if let Some(v) = picked.and_then(|p| p.parse::<usize>().ok()) {
                    s.set_job_files(id, v);
                }
            });
        })
        .build(k).key("files-at-once").label(label).help(help)
}

/// An animated bar for work whose total isn't known yet. The segment always stays on the
/// track (the kit's sweep leaves it empty part of each cycle); reduced motion holds it still.
fn counting_bar(k: &Kit, label: String) -> El {
    let fill = rune::el().key("fill").h_full().w_frac(0.25).radius(999.0).fill(k.c.border_active)
        .looping(rune::LoopSpec::slide(1.6, [0.0, 0.0], [3.0, 0.0]));
    row().key("bar").role("progressbar").label(label).w_full().h(8.0).radius(999.0).fill(k.c.surface_base).border(1.0, k.c.border_weak)
        .clip_soft(0.0).child(fill)
}

/// The header of a job with several files: "Downloading 6 of 24 to “Downloads”" ("24+" while
/// still counting). Single files keep their label ("Downloading “big.iso” to …").
pub fn header_line(r: &Row) -> String {
    if !r.many_files() || r.dest.is_empty() {
        return r.label.clone();
    }
    let p = &r.progress;
    let verb = r.label.split_whitespace().next().unwrap_or("Transferring");
    let total = p.files_total.unwrap_or(p.files_found);
    let at = (p.files_done + r.items.active.len().max(1) as u64).min(total.max(1));
    let plus = if p.files_total.is_none() { "+" } else { "" };
    let at = if r.state.is_finished() { total } else { at };
    format!("{verb} {} of {}{plus} to “{}”", fmt::count(at), fmt::count(total), r.dest)
}

/// "84 downloaded, 2 skipped, 1 failed" for a finished job with several files.
pub fn summary_line(r: &Row) -> Option<String> {
    let c = &r.items.counts;
    if !r.state.is_finished() || c.done + c.skipped + c.failed == 0 || !r.many_files() {
        return None;
    }
    let verb = match r.direction {
        Direction::Upload => "uploaded",
        Direction::Download => "downloaded",
        _ => "copied",
    };
    let mut parts = vec![format!("{} {verb}", fmt::count(c.done as u64))];
    if c.skipped > 0 {
        parts.push(format!("{} skipped", fmt::count(c.skipped as u64)));
    }
    if c.failed > 0 {
        parts.push(format!("{} failed", fmt::count(c.failed as u64)));
    }
    Some(parts.join(", "))
}

/// The second line: "88.3 MB of 21.55 GB · 25 min left · 13.3 MB/s", or while the totals are
/// still being counted "88.3 MB · counting… · 6.3 MB/s".
pub fn detail_line(r: &Row) -> String {
    let p = &r.progress;
    let mut parts = Vec::new();
    // Deleting moves no bytes: count the items.
    if r.direction == Direction::Delete {
        if let Some(n) = p.files_total {
            parts.push(if n == 1 { format!("{} of 1 item", p.files_done) } else { format!("{} of {n} items", p.files_done) });
        }
        return parts.join(" · ");
    }
    if let Some(sum) = summary_line(r) {
        parts.push(sum);
    }
    match p.bytes_total {
        Some(t) => parts.push(format!("{} of {}", fmt::size(p.bytes_done), fmt::size(t))),
        None if p.bytes_done > 0 => parts.push(fmt::size(p.bytes_done)),
        None => {}
    }
    if r.counting() {
        parts.push("counting…".into());
    }
    if matches!(r.state, JobState::Running | JobState::Scanning) {
        if let Some(eta) = p.eta.filter(|_| p.bytes_total.is_some()) {
            parts.push(format!("{} left", fmt::duration(eta)));
        }
        if p.bytes_per_sec >= 1.0 {
            parts.push(fmt::speed(p.bytes_per_sec));
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

fn job_row(r: &Row, k: &Kit, paint: &Paint, dark: bool) -> El {
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
    if r.many_files() && r.files_at_once > 0 && !r.state.is_finished() {
        controls = controls.child(files_control(r, k));
    }
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
    let bar = match fraction {
        None if r.counting() => counting_bar(k, format!("{} progress", r.label)),
        // Waiting or paused before any total: a still, empty bar.
        f => Bar::new(Some(f.unwrap_or(0.0))).build(k).key("bar").label(format!("{} progress", r.label)).w_full(),
    };
    let detail = detail_line(r);
    let mut info = col().gap(4.0).grow(1.0).min_w(0.0)
        .child(row().gap(8.0).center_y().min_w(0.0)
            .child(text(header_line(r)).key("header").font(k.ty.text_small_medium).color(k.c.text_base).truncate().shrink().min_w(0.0))
            .child(rune::el().grow(1.0))
            .child(text(r.state_label()).key("state").font(k.ty.text_xsmall).color(tone).truncate().max_w(280.0)))
        .child(bar);
    // (A single file is named in the header; several show theirs in the rows below.)
    if !detail.is_empty() {
        info = info.child(text(detail).key("detail").font(k.ty.text_xsmall).color(k.c.text_muted).truncate());
    }
    let mut c = col().key(format!("job-{id}")).role("listitem").label(format!("{}, {}", header_line(r), r.state_label())).w_full().gap(6.0).px(12.0).py(8.0)
        .border_sides([0.0, 0.0, 1.0, 0.0], paint.hair)
        .child(row().w_full().gap(10.0).center_y()
            .child(icon(glyph, 16.0).color(tone).a11y_hidden())
            .child(info)
            .child(controls));
    if let Some(items) = super::items_view::items_section(r, k, paint, dark) {
        c = c.child(items);
    }
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

/// Taller while a job's item list is open, so the list has room.
fn drawer_height(s: &Ui) -> f32 {
    if s.transfers.rows.is_empty() && s.edits.list.is_empty() {
        96.0
    } else if s.transfers.rows.iter().any(|r| r.items.open && r.many_files()) {
        520.0
    } else {
        300.0
    }
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
        list = list.child(job_row(r, k, paint, s.dark()));
    }
    col().key("transfers").role("region").label("Transfers").w_full().h(drawer_height(s)).fill(paint.list).border_sides([1.0, 0.0, 0.0, 0.0], paint.hair)
        .child(head)
        .child(col().w_full().grow(1.0).min_h(0.0).scroll(ScrollSpec { stick_bottom: false }).child(list))
}
