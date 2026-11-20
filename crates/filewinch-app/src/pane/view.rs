//! A pane's file table: sortable header, virtual rows (100k-entry folders build only what's on
//! screen), selection, in-place rename, drag and drop, and the loading, empty and error states.

use std::time::SystemTime;

use filewinch_core::EntryKind;
use rune::{Align, Color, Cx, El, GrowthSpec, ScrollSpec, Tx, col, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit, Spinner, TextInput};

use filewinch_store::ColumnId;

use super::header::{CELL_PAD, HEADER_PAD, header, size_text, sized};
use super::layout;
use super::model::file_kind;
use super::{Listing, Pane};
use crate::fmt;
use crate::theme::Paint;
use crate::ui::Ui;

/// `ink`: the text and icon colour on a focused selection, when the look sets one (Finder's white).
fn name_cell(s: &Ui, p: usize, e: &filewinch_core::Entry, k: &Kit, paint: &Paint, selected: bool, ink: Option<Color>) -> El {
    let pane = &s.panes[p];
    let kind = file_kind(e);
    let tint = ink.unwrap_or_else(|| kind.tint(paint.folder, k.c.icon_muted, paint.dark));
    let glyph = icon(kind.icon(), 16.0).color(tint).a11y_hidden();
    let renaming = pane.rename.as_ref().filter(|r| r.original == e.name);
    let body = match renaming {
        Some(r) => {
            let input = TextInput::new(&r.edit)
                .plain()
                .label(format!("New name for {}", e.name))
                // The row's own type, so the name doesn't jump when the field opens.
                .text_style(k.ty.text_small)
                .height(22.0)
                .width(None)
                .invalid(r.error.is_some())
                .on_edit(move |s: &mut Ui| s.rename_edit(p))
                .on_submit(move |s: &mut Ui, tx: &mut Tx| s.commit_rename(p, tx))
                .on_key(move |s: &mut Ui, ev: &rune::KeyEvent, tx: &mut Tx| {
                    if ev.key == rune::Key::Escape {
                        s.cancel_rename(p, tx);
                        return true;
                    }
                    false
                })
                .build(k)
                .key(format!("rename-{p}"))
                .fill(k.c.bg_base)
                .radius(3.0)
                .border(1.0, k.c.border_focus);
            let mut c = col().grow(1.0).min_w(0.0).child(input);
            if let Some(err) = &r.error {
                c = c.child(text(err.clone()).key("rename-error").font(k.ty.text_xsmall).color(paint.danger).live_region(rune::LiveRegion::Polite));
            }
            c
        }
        None => {
            let color = ink.unwrap_or(if selected { k.c.text_strong } else { k.c.text_base });
            let mut t = row().gap(6.0).min_w(0.0).center_y().child(
                text(e.name.clone()).font(k.ty.text_small).color(color).truncate().shrink().min_w(0.0).help_when_truncated(e.name.clone()),
            );
            if let Some(target) = &e.link_target {
                t = t.child(text(format!("→ {target}")).font(k.ty.text_xsmall).color(ink.map_or(k.c.text_faint, |c| c.alpha(0.8))).truncate().shrink().min_w(0.0).a11y_hidden());
            }
            if e.shared.is_some() {
                t = t.child(shared_badge(k));
            }
            t
        }
    };
    row().gap(7.0).center_y().min_w(0.0).grow(1.0).child(glyph).child(body)
}

/// A small "Shared" pill after the name: deleting this item behaves differently (it's left,
/// not emptied), so the row says so up front.
fn shared_badge(k: &Kit) -> El {
    row().key("shared-badge").gap(3.0).h(16.0).px(5.0).center_y().radius(8.0).fill(k.c.hover).border(1.0, k.c.border_base)
        .help("Shared with others. Deleting it removes it only for you when the service allows; its contents are never deleted for everyone without asking.")
        .child(icon(&crate::icons::SHARED, 11.0).color(k.c.icon_muted).a11y_hidden())
        .child(text("Shared").font(k.ty.text_xsmall).color(k.c.text_muted).a11y_hidden())
}

fn cell(id: ColumnId, w: f32, content: El) -> El {
    sized(row().role("gridcell").h_full().px(CELL_PAD).center_y().clip_soft(0.0), id, w).child(content)
}

fn row_el(s: &Ui, p: usize, i: usize, k: &Kit, paint: &Paint, now: SystemTime, widths: &[f32]) -> El {
    let pane = &s.panes[p];
    let Some(e) = pane.row_entry(i) else { return el().key(format!("gone-{i}")).h(paint.row_h) };
    let name = e.name.clone();
    let selected = pane.sel.is_selected(&name);
    let is_cursor = pane.sel.cursor.as_deref() == Some(name.as_str());
    let drop_here = pane.drop_target.as_deref() == Some(name.as_str());
    let fill = if drop_here {
        k.alpha(paint.accent, 0.28)
    } else if selected {
        if s.active == p { paint.selected } else { paint.selected_inactive }
    } else if i % 2 == 1 {
        paint.stripe
    } else {
        rune::Color::TRANSPARENT
    };
    let ink = if selected && s.active == p && !drop_here { paint.selected_ink } else { None };
    let muted_ink = ink.unwrap_or(k.c.text_muted);
    let muted = |t: String| text(t).font(k.ty.text_small).color(muted_ink).truncate();
    let mut r = row()
        .key(name.clone())
        .role("row")
        .label(if e.shared.is_some() { format!("{name}, shared") } else { name.clone() })
        .checked(selected)
        .w_full()
        .h(paint.row_h)
        .px(4.0)
        .center_y()
        .fill(fill)
        .radius(k.radius.sm)
        .hover(k.c.hover);
    if selected && let Some(pill) = crate::theme::pill(paint, paint.row_h) {
        r = r.child(pill);
    }
    if is_cursor && pane.sel.names.len() > 1 {
        r = r.outline(1.0, -1.0, k.alpha(paint.accent, 0.6));
    }
    for (c, w) in s.shown_columns().iter().zip(widths) {
        let content = match c.id {
            ColumnId::Name | ColumnId::Unknown => name_cell(s, p, e, k, paint, selected, ink),
            ColumnId::Size => muted(size_text(e)).align(Align::End),
            ColumnId::Modified => muted(e.modified.map(|m| fmt::date(m, now)).unwrap_or_else(|| "—".into())),
            ColumnId::Kind => muted(file_kind(e).describe(&e.name)),
            // Blank where the server doesn't say.
            ColumnId::Permissions => text(e.mode.map(fmt::mode_string).unwrap_or_default()).font(k.ty.mono).color(muted_ink).truncate(),
        };
        r = r.child(cell(c.id, *w, content));
    }
    let (n1, n2, n3, n4, n5) = (name.clone(), name.clone(), name.clone(), name.clone(), name.clone());
    r = r
        .on_press_tx(move |s: &mut Ui, tx: &mut Tx| s.row_click(p, &n1, tx))
        .on_double_press_tx(move |s: &mut Ui, _tx: &mut Tx| s.open_entry(p, &n2))
        .on_secondary_press(move |s: &mut Ui, press, tx: &mut Tx| s.row_menu(p, &n3, press, tx));
    r = if crate::drag::DRAG_OUT {
        r.drag_out(move |s: &Ui| s.drag_item(p, &n4))
            .on_drag_start(move |s: &mut Ui, _tx: &mut Tx| s.drag_started(p, &n5))
            .on_drag_end(|s: &mut Ui, outcome, _tx: &mut Tx| s.drag_ended(outcome))
    } else {
        r.on_drag(move |s: &mut Ui, d: &mut rune::Drag, tx: &mut Tx| s.row_drag(p, &n4, d, tx))
    };
    if e.kind == EntryKind::Dir {
        let (a, b) = (name.clone(), name);
        r = r
            .on_drop_files(move |s: &mut Ui, files, tx: &mut Tx| s.dropped(p, Some(&a), files, tx))
            .on_drop_hover(move |s: &mut Ui, over, _tx: &mut Tx| {
                let t = &mut s.panes[p].drop_target;
                if over {
                    *t = Some(b.clone());
                } else if t.as_deref() == Some(b.as_str()) {
                    *t = None;
                }
            });
    }
    r
}

fn centered(k: &Kit, title: &str, detail: Option<&str>, extra: Option<El>) -> El {
    let mut c = col().key("state").w_full().grow(1.0).center().gap(6.0).padding(24.0).child(text(title.to_owned()).font(k.ty.ui_medium).color(k.c.text_muted));
    if let Some(d) = detail {
        c = c.child(text(d.to_owned()).font(k.ty.text_small).color(k.c.text_faint).wrap().max_w(360.0).align(Align::Center));
    }
    if let Some(x) = extra {
        c = c.child(x.mt(6.0));
    }
    c
}

fn body(s: &Ui, p: usize, k: &Kit, avail: Option<f32>) -> El {
    let pane = &s.panes[p];
    match &pane.listing {
        Listing::Loading if pane.entries.is_empty() => {
            col().key("state").w_full().grow(1.0).center().gap(10.0)
                .child(Spinner::new().size(18.0).build(k))
                .child(text("Loading…").font(k.ty.text_small).color(k.c.text_muted).live_region(rune::LiveRegion::Polite))
        }
        Listing::Failed(e) => {
            let retry = Button::new("Try Again").variant(ButtonVariant::Neutral).control_size(ControlSize::Small)
                .on_press(move |s: &mut Ui| s.refresh_pane(p)).build(k).key("retry");
            let back = Button::new("Go Back").variant(ButtonVariant::Ghost).control_size(ControlSize::Small)
                .disabled(pane.back.is_empty()).on_press(move |s: &mut Ui| s.go_back(p)).build(k).key("go-back");
            let detail = e.detail.clone();
            centered(k, &e.message, detail.as_deref(), Some(row().gap(8.0).child(back).child(retry)))
                .role("alert")
        }
        _ if pane.rows.is_empty() && !pane.filter.text.trim().is_empty() => {
            centered(k, "No matches", Some(&format!("Nothing here has “{}” in its name.", pane.filter.text.trim())), None)
        }
        _ if pane.rows.is_empty() => centered(k, "This folder is empty", Some("Drop files here to add them."), None),
        _ => {
            let now = fmt::now();
            let widths = layout::resolve(&s.shown_columns(), avail);
            let rows = col()
                .key("rows")
                .w_full()
                .growth(GrowthSpec::default())
                .virtual_rows(
                    pane.rows.len(),
                    crate::theme::look().row_height(),
                    move |s: &Ui, i| s.panes[p].row_entry(i).map_or(std::borrow::Cow::Owned(format!("gone-{i}")), |e| std::borrow::Cow::Borrowed(e.name.as_str())),
                    move |s: &Ui, i| {
                        let k = crate::theme::kit(s.dark());
                        row_el(s, p, i, &k, &crate::theme::paint(&k), now, &widths)
                    },
                )
                // Rows are cached by version: include the appearance and accent (the kit's
                // version) so a light/dark or accent switch rebuilds them instead of keeping the
                // old colours.
                .virtual_rows_version((pane.version, s.dark(), crate::theme::version(s.dark())));
            // Where the rows are, for drags between panes without a system drag. Live only:
            // the view doesn't depend on it, so a scroll runs no view.
            let rows = if crate::drag::DRAG_OUT {
                rows
            } else {
                rows.on_layout_tx(move |s: &mut Ui, r, tx: &mut Tx| {
                    s.panes[p].rows_rect = Some(r);
                    tx.live_only();
                })
            };
            col().key("viewport").w_full().grow(1.0).min_h(0.0).px(4.0).py(2.0).scroll(ScrollSpec { stick_bottom: false }).child(rows)
        }
    }
}

/// The table: header, rows and drop target, as one keyboard-focusable grid.
pub fn table(s: &Ui, p: usize, k: &Kit, paint: &Paint, cx: &mut Cx<Ui>) -> El {
    let pane = &s.panes[p];
    // The header's inner width, which Name's share and the rows' cells follow.
    let avail = cx.layout_size(&format!("pane-{p}/header")).map(|[w, _]| w - 2.0 * HEADER_PAD);
    let over_pane = pane.drop_target.as_deref() == Some("");
    let label = format!("Files in {} on {}", if pane.dir.is_root() { "/" } else { pane.dir.name() }, pane.label());
    let mut t = col()
        .key(format!("pane-{p}"))
        .role("grid")
        .label(label)
        .focusable(true)
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .clip_soft(0.0)
        .on_key(move |s: &mut Ui, ev: &rune::KeyEvent, tx: &mut Tx| s.pane_key(p, ev, tx))
        .on_secondary_press(move |s: &mut Ui, press, tx: &mut Tx| s.pane_menu(p, press, tx))
        .on_press(move |s: &mut Ui| s.active = p)
        .on_drop_files(move |s: &mut Ui, files, tx: &mut Tx| s.dropped(p, None, files, tx))
        .on_drop_hover(move |s: &mut Ui, over, _tx: &mut Tx| {
            let t = &mut s.panes[p].drop_target;
            if over {
                *t = Some(String::new());
            } else if t.as_deref() == Some("") {
                *t = None;
            }
        })
        .child(header(s, p, k, paint, avail))
        .child(body(s, p, k, avail));
    if !crate::drag::DRAG_OUT {
        t = t.on_layout_tx(move |s: &mut Ui, r, tx: &mut Tx| {
            s.panes[p].rect = r;
            tx.live_only();
        });
    }
    if let Some(cursor) = &pane.sel.cursor {
        t = t.active_descendant(cursor.clone());
    }
    if over_pane {
        t = t.outline(2.0, -2.0, paint.accent);
    }
    t
}

impl Pane {
    pub fn is_ready(&self) -> bool {
        matches!(self.listing, Listing::Ready)
    }
}
