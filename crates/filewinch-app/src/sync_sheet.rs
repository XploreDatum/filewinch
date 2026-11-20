//! Synchronize the two panes' folders: choose a direction and options, compare, review the
//! plan item by item, then apply it as transfer jobs.

use std::collections::HashSet;
use std::time::Duration;

use rune::machine::Attempt;
use rune::{El, ScrollSpec, TextEdit, Tx, col, icon, row, text};
use rune_kit::{Button, ButtonVariant, Checkbox, ControlSize, Dialog, Kit, SegmentedControl, Spinner, TextInput};

use crate::fmt;
use crate::icons;
use crate::pane::Source;
use crate::theme::Paint;
use crate::transfers::engine::{
    CancelToken, CompareBy, CompareProgress, SyncActionKind, SyncDirection, SyncOptions, SyncPlan, apply, compare_with,
};
use crate::transfers::model::Purpose;
use crate::ui::{Sheet, Tone, Ui};

const SIZE_AND_DATE: CompareBy = CompareBy::SizeAndMtime { tolerance: Duration::from_secs(2) };

/// The compare choices, in the order the control shows them, with a one-line hint each.
const COMPARE: [(&str, CompareBy, &str); 3] = [
    ("Size and date", SIZE_AND_DATE, "Files differ when their size or date differs."),
    ("Size only", CompareBy::SizeOnly, "Ignores dates; only a size change counts."),
    ("Checksum", CompareBy::Checksum, "Slower: reads files the server can't hash."),
];

/// A compare in flight. Dropping it (Cancel, closing the sheet, changing an option, or the
/// result arriving) cancels the worker, so a checksum compare never runs on unseen.
pub struct Comparing {
    pub attempt: Attempt,
    pub cancel: CancelToken,
    pub progress: CompareProgress,
}

impl Drop for Comparing {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub enum Phase {
    Options,
    Comparing(Comparing),
    /// The person stopped the compare.
    Stopped,
    Plan { plan: SyncPlan, skipped: HashSet<usize> },
    Failed(String),
}

pub struct SyncSheet {
    pub left: (Source, filewinch_core::RPath),
    pub right: (Source, filewinch_core::RPath),
    pub direction: SyncDirection,
    pub compare: CompareBy,
    pub delete_orphans: bool,
    pub exclude: TextEdit,
    pub phase: Phase,
}

impl SyncSheet {
    pub fn options(&self) -> SyncOptions {
        SyncOptions {
            direction: self.direction,
            compare: self.compare,
            delete_orphans: self.delete_orphans && self.direction != SyncDirection::Mirror,
            exclude: self.exclude.text.split([',', '\n']).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect(),
            follow_symlinks: false,
        }
    }
}

impl Ui {
    pub fn open_sync(&mut self) {
        // Sync needs a folder on each side.
        let (Some(l), Some(r)) = (self.panes[0].src.clone(), self.panes[1].src.clone()) else {
            self.notify(Tone::Info, "Open a folder in both panes to synchronize them.", None);
            return;
        };
        self.sheet = Some(Sheet::Sync(Box::new(SyncSheet {
            left: (l, self.panes[0].dir.clone()),
            right: (r, self.panes[1].dir.clone()),
            direction: SyncDirection::AToB,
            compare: SIZE_AND_DATE,
            delete_orphans: false,
            exclude: TextEdit::new(".DS_Store, .git"),
            phase: Phase::Options,
        })));
    }

    fn sync_mut(&mut self) -> Option<&mut SyncSheet> {
        match &mut self.sheet {
            Some(Sheet::Sync(s)) => Some(s),
            _ => None,
        }
    }

    pub fn sync_compare(&mut self) {
        let attempt = self.seq.next();
        let inbox = self.inbox.clone();
        let Some(s) = self.sync_mut() else { return };
        let opts = s.options();
        let (a, a_dir) = (s.left.0.remote.clone(), s.left.1.clone());
        let (b, b_dir) = (s.right.0.remote.clone(), s.right.1.clone());
        let cancel = CancelToken::new();
        let token = cancel.clone();
        s.phase = Phase::Comparing(Comparing { attempt, cancel, progress: CompareProgress::default() });
        let work = move || {
            // The engine reports about ten times a second; each report lands on the UI thread.
            let mut report = |p: &CompareProgress| {
                let p = p.clone();
                inbox.post(move |ui| {
                    if let Some(Phase::Comparing(c)) = ui.sync_mut().map(|s| &mut s.phase)
                        && c.attempt == attempt
                    {
                        c.progress = p;
                    }
                });
            };
            compare_with(&*a, &a_dir, &*b, &b_dir, &opts, &token, &mut report)
        };
        self.run(work, move |ui, res| {
            if let Some(s) = ui.sync_mut()
                && matches!(&s.phase, Phase::Comparing(c) if c.attempt == attempt)
            {
                s.phase = match res {
                    Ok(plan) => Phase::Plan { plan, skipped: HashSet::new() },
                    Err(e) if e.kind == filewinch_core::ErrorKind::Cancelled => Phase::Stopped,
                    Err(e) => Phase::Failed(e.message),
                };
            }
        });
    }

    /// Stops a running compare; the worker notices between items or hash chunks.
    pub fn sync_cancel_compare(&mut self) {
        if let Some(s) = self.sync_mut()
            && matches!(s.phase, Phase::Comparing(_))
        {
            s.phase = Phase::Stopped; // dropping the Comparing cancels its token
        }
    }

    pub fn sync_apply(&mut self) {
        let Some(Sheet::Sync(s)) = self.sheet.take() else { return };
        let Phase::Plan { mut plan, skipped } = s.phase else {
            return;
        };
        plan.actions = plan.actions.into_iter().enumerate().filter(|(i, _)| !skipped.contains(i)).map(|(_, a)| a).collect();
        let jobs = apply(&plan, s.left.0.remote.clone(), s.right.0.remote.clone());
        let touched = vec![(s.left.0.session, s.left.1.clone()), (s.right.0.session, s.right.1.clone())];
        for job in jobs {
            self.enqueue(job, Purpose::Plain, touched.clone());
        }
    }
}

fn side_label(k: &Kit, (src, dir): &(Source, filewinch_core::RPath), which: &str) -> El {
    col().gap(2.0).min_w(0.0).grow(1.0).label(format!("{which}: {} on {}", dir, src.label))
        .child(text(which.to_owned()).font(k.ty.text_xsmall_medium).color(k.c.text_muted).a11y_hidden())
        .child(row().gap(6.0).center_y().min_w(0.0)
            .child(icon(icons::protocol(src.protocol), 14.0).color(k.c.icon_muted).a11y_hidden())
            .child(text(format!("{} — {}", src.label, dir)).font(k.ty.text_small).color(k.c.text_base).truncate().a11y_hidden()))
}

fn action_text(kind: SyncActionKind) -> (&'static str, &'static str) {
    match kind {
        SyncActionKind::CopyAToB => ("→", "Copy to right"),
        SyncActionKind::CopyBToA => ("←", "Copy to left"),
        SyncActionKind::DeleteInA => ("×", "Delete on left"),
        SyncActionKind::DeleteInB => ("×", "Delete on right"),
    }
}

fn plan_list(plan: &SyncPlan, skipped: &HashSet<usize>, k: &Kit, paint: &Paint) -> El {
    let mut list = col().key("plan-rows").w_full();
    for (i, a) in plan.actions.iter().enumerate() {
        let (arrow, what) = action_text(a.kind);
        let on = !skipped.contains(&i);
        let danger = matches!(a.kind, SyncActionKind::DeleteInA | SyncActionKind::DeleteInB);
        let cb = Checkbox::new(on).label(format!("{what}: {}", a.rel)).hide_label()
            .on_toggle(move |s: &mut Ui| {
                if let Some(Sheet::Sync(sh)) = &mut s.sheet
                    && let Phase::Plan { skipped, .. } = &mut sh.phase
                    && !skipped.remove(&i)
                {
                    skipped.insert(i);
                }
            })
            .build(k).key("include");
        list = list.child(row().key(format!("act-{i}")).w_full().h(26.0).gap(8.0).px(8.0).center_y()
            .fill(if i % 2 == 1 { paint.stripe } else { rune::Color::TRANSPARENT })
            .child(cb)
            .child(text(arrow).font(k.ty.mono).color(if danger { paint.danger } else { paint.accent }).w(14.0).a11y_hidden())
            .child(text(format!("{}{}", a.rel, if a.is_dir { "/" } else { "" })).font(k.ty.text_small).color(if on { k.c.text_base } else { k.c.text_faint }).truncate().grow(1.0).min_w(0.0))
            .child(text(what).font(k.ty.text_xsmall).color(k.c.text_muted).w(96.0))
            .child(text(a.size.map(fmt::size).unwrap_or_default()).font(k.ty.text_xsmall).color(k.c.text_muted).w(64.0)));
    }
    for (j, c) in plan.conflicts.iter().enumerate() {
        list = list.child(row().key(format!("conflict-{j}")).w_full().h(26.0).gap(8.0).px(8.0).center_y()
            .child(icon(&icons::WARNING, 14.0).color(paint.warning).a11y_hidden())
            .child(text(format!("{} — left alone: {}", c.rel, c.reason)).font(k.ty.text_small).color(k.c.text_muted).truncate().grow(1.0)));
    }
    col().key("plan").w_full().h(230.0).border(1.0, paint.hair).radius(6.0).clip_soft(0.0).scroll(ScrollSpec { stick_bottom: false }).child(list)
}

/// "Comparing… 1,204 items, 312 MB read" (the bytes only once checksums are being read).
pub fn progress_text(p: &CompareProgress) -> String {
    let items = if p.entries == 1 { "1 item".to_owned() } else { format!("{} items", fmt::count(p.entries)) };
    if p.bytes_hashed > 0 {
        format!("Comparing… {items}, {} read for checksums", fmt::size(p.bytes_hashed))
    } else if p.entries > 0 {
        format!("Comparing… {items}")
    } else {
        "Comparing…".to_owned()
    }
}

pub fn view(s: &SyncSheet, k: &Kit, paint: &Paint, layer: &mut Vec<El>) {
    let dirs = ["Left → Right", "Right → Left", "Both ways"];
    let current = match s.direction {
        SyncDirection::AToB => 0,
        SyncDirection::BToA => 1,
        SyncDirection::Mirror => 2,
    };
    let busy = matches!(s.phase, Phase::Comparing(_));
    let direction = SegmentedControl::new(&dirs, Some(current)).label("Direction").fit()
        .on_pick(|s: &mut Ui, i, _tx: &mut Tx| {
            if let Some(Sheet::Sync(sh)) = &mut s.sheet {
                sh.direction = [SyncDirection::AToB, SyncDirection::BToA, SyncDirection::Mirror][i.min(2)];
                sh.phase = Phase::Options;
            }
        })
        .build(k).key("direction");
    let set = |f: fn(&mut SyncSheet)| move |s: &mut Ui| {
        if let Some(Sheet::Sync(sh)) = &mut s.sheet {
            f(sh);
            sh.phase = Phase::Options;
        }
    };
    let orphans = Checkbox::new(s.delete_orphans && s.direction != SyncDirection::Mirror)
        .label("Delete items that exist only on the destination")
        .disabled(s.direction == SyncDirection::Mirror)
        .on_toggle(set(|sh| sh.delete_orphans = !sh.delete_orphans)).build(k).key("orphans");
    let picked = COMPARE.iter().position(|c| c.1 == s.compare).unwrap_or(0);
    let labels: Vec<&str> = COMPARE.iter().map(|c| c.0).collect();
    let compare_by = SegmentedControl::new(&labels, Some(picked)).label("Compare by").fit()
        .on_pick(|s: &mut Ui, i, _tx: &mut Tx| {
            if let Some(Sheet::Sync(sh)) = &mut s.sheet {
                sh.compare = COMPARE[i.min(COMPARE.len() - 1)].1;
                sh.phase = Phase::Options;
            }
        })
        .build(k).key("compare-by");
    let exclude = TextInput::new(&s.exclude).width(None).label("Skip items matching").placeholder("*.tmp, node_modules, .git")
        .on_edit(|s: &mut Ui| match &mut s.sheet {
            Some(Sheet::Sync(sh)) => &mut sh.exclude,
            _ => &mut s.sidebar_search,
        })
        .on_change(|s: &mut Ui, _tx: &mut Tx| {
            if let Some(Sheet::Sync(sh)) = &mut s.sheet {
                sh.phase = Phase::Options;
            }
        })
        .build(k).key("exclude");
    let mut body = col().key("sync-body").gap(12.0).px(16.0).pb(16.0).w_full()
        .child(row().gap(12.0).w_full().child(side_label(k, &s.left, "Left")).child(side_label(k, &s.right, "Right")))
        .child(direction)
        .child(orphans)
        .child(col().gap(4.0).w_full()
            .child(compare_by)
            .child(text(COMPARE[picked].2).key("compare-hint").font(k.ty.text_xsmall).color(k.c.text_faint)))
        .child(col().gap(4.0).w_full()
            .child(text("Skip items matching (comma-separated patterns)").font(k.ty.text_xsmall_medium).color(k.c.text_muted).a11y_hidden())
            .child(exclude));
    let (summary, can_apply) = match &s.phase {
        Phase::Options => ("Compare the folders to see what would change.".to_owned(), false),
        Phase::Comparing(c) => (progress_text(&c.progress), false),
        Phase::Stopped => ("Compare stopped. Nothing was changed.".to_owned(), false),
        Phase::Failed(m) => (m.clone(), false),
        Phase::Plan { plan, skipped } => {
            let n = plan.actions.len() - skipped.len();
            if plan.actions.is_empty() {
                ("The folders already match.".to_owned(), false)
            } else {
                (format!("{n} of {} changes selected, {} to copy.", plan.actions.len(), fmt::size(plan.bytes_to_copy())), n > 0)
            }
        }
    };
    if let Phase::Plan { plan, skipped } = &s.phase
        && !(plan.actions.is_empty() && plan.conflicts.is_empty())
    {
        body = body.child(plan_list(plan, skipped, k, paint));
    }
    let tone = if matches!(s.phase, Phase::Failed(_)) { paint.danger } else { k.c.text_muted };
    let mut status = row().gap(8.0).center_y().grow(1.0).min_w(0.0);
    if busy {
        status = status.child(Spinner::new().size(14.0).build(k));
    }
    status = status.child(text(summary).key("sync-summary").font(k.ty.text_small).color(tone).wrap().live_region(rune::LiveRegion::Polite));
    let compare_btn = if busy {
        Button::new("Stop").variant(ButtonVariant::Neutral).control_size(ControlSize::Regular)
            .on_press(|s: &mut Ui| s.sync_cancel_compare()).build(k).key("cancel-compare")
    } else {
        Button::new(if matches!(s.phase, Phase::Plan { .. }) { "Compare Again" } else { "Compare" })
            .variant(ButtonVariant::Neutral).control_size(ControlSize::Regular)
            .on_press(|s: &mut Ui| s.sync_compare()).build(k).key("compare")
    };
    let apply_btn = Button::new("Synchronize").variant(ButtonVariant::Contrast).control_size(ControlSize::Regular).disabled(!can_apply)
        .on_press(|s: &mut Ui| s.sync_apply()).build(&crate::theme::primary(k)).key("apply-sync");
    body = body.child(row().w_full().gap(8.0).center_y().child(status).child(compare_btn).child(apply_btn));
    Dialog::new("sync", "Synchronize Folders").open(true).width(620.0).fit().child(body)
        .on_close(|s: &mut Ui| s.sheet = None)
        .build(k, layer);
}
