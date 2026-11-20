//! The file table's header: sort by a click, resize a column by dragging (or double-clicking)
//! the divider at its right edge, move a column by dragging its header, and choose columns from
//! its context menu. Both panes share the column layout, which is saved in the preferences.

use filewinch_core::EntryKind;
use filewinch_store::{ColumnId, ColumnPref};
use rune::{Cursor, Drag, El, Phase, PointerPress, PopupItem, PopupMenu, Tx, el, icon, row, text};
use rune_kit::Kit;

use super::layout::{self, is_flexible};
use super::model::{SortKey, file_kind};
use crate::fmt;
use crate::theme::Paint;
use crate::ui::Ui;

/// Header padding either side, and each cell's own.
pub const HEADER_PAD: f32 = 4.0;
pub const CELL_PAD: f32 = 8.0;
/// Travel before a press on a header becomes a move.
const MOVE_SLOP: f32 = 4.0;
/// Rows auto-fit looks at, so a huge folder doesn't stall a double-click.
const FIT_ROWS: usize = 5000;

/// A header being dragged sideways to a new place.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HeaderDrag {
    Moving { id: ColumnId, dx: f32 },
    /// Just dropped over its own header: the click that ends the drag doesn't sort.
    Dropped,
}

pub fn sort_key(id: ColumnId) -> SortKey {
    match id {
        ColumnId::Size => SortKey::Size,
        ColumnId::Modified => SortKey::Modified,
        ColumnId::Kind => SortKey::Kind,
        ColumnId::Permissions => SortKey::Permissions,
        ColumnId::Name | ColumnId::Unknown => SortKey::Name,
    }
}

impl Ui {
    /// The columns a table shows, in order.
    pub fn shown_columns(&self) -> Vec<ColumnPref> {
        layout::visible(&self.prefs.columns, self.prefs.show_permissions)
    }

    fn header_press(&mut self, p: usize, id: ColumnId) {
        if self.panes[p].header_drag.take() == Some(HeaderDrag::Dropped) {
            return;
        }
        self.sort_by(p, sort_key(id));
    }

    fn header_drag(&mut self, p: usize, id: ColumnId, d: &Drag, widths: &[f32]) {
        match d.phase {
            Phase::Down => self.panes[p].header_drag = None,
            Phase::Move => {
                if d.dx.abs() > MOVE_SLOP || matches!(self.panes[p].header_drag, Some(HeaderDrag::Moving { .. })) {
                    self.panes[p].header_drag = Some(HeaderDrag::Moving { id, dx: d.dx });
                }
            }
            Phase::Up => {
                let moved = matches!(self.panes[p].header_drag, Some(HeaderDrag::Moving { .. }));
                self.panes[p].header_drag = None;
                if !moved || d.cancelled() {
                    return;
                }
                let shown = self.shown_columns();
                if let Some(from) = shown.iter().position(|c| c.id == id).filter(|_| widths.len() == shown.len()) {
                    let to = layout::drop_index(widths, from, d.dx);
                    if to != from {
                        self.prefs.columns = layout::reorder(&self.prefs.columns, &shown, from, to);
                        self.save_prefs();
                    }
                }
                // The release over the header also counts as a click on it; that one mustn't sort.
                let inside = d.x >= 0.0 && d.y >= 0.0 && d.x <= d.w && d.y <= d.h;
                if inside {
                    self.panes[p].header_drag = Some(HeaderDrag::Dropped);
                }
            }
        }
    }

    fn divider_drag(&mut self, p: usize, divider: usize, d: &mut Drag, avail: Option<f32>) {
        let shown = self.shown_columns();
        let Some((target, sign)) = layout::resize_target(&shown, divider) else { return };
        let id = shown[target].id;
        match d.phase {
            Phase::Down if d.clicks >= 2 => {
                d.data = -1.0;
                self.fit_column(p, id);
            }
            Phase::Down => d.data = layout::width_of(&shown[target]),
            Phase::Move if d.data > 0.0 => {
                let w = layout::resized(&shown, target, sign, d.data, d.dx, avail);
                self.prefs.columns = layout::with_width(&self.prefs.columns, id, w);
            }
            Phase::Up if d.data > 0.0 => {
                if d.cancelled() {
                    self.prefs.columns = layout::with_width(&self.prefs.columns, id, d.data);
                } else {
                    let w = layout::resized(&shown, target, sign, d.data, d.dx, avail);
                    self.prefs.columns = layout::with_width(&self.prefs.columns, id, w);
                }
                self.save_prefs();
            }
            _ => {}
        }
    }

    /// Sizes column `id` to its widest value in pane `p` (and its title).
    pub fn fit_column(&mut self, p: usize, id: ColumnId) {
        if is_flexible(id) {
            return;
        }
        let k = crate::theme::kit(self.dark());
        let measure = |t: &str, style| rune::text::measure(t, &[], style, None).0;
        // The title, plus room for the sort arrow.
        let mut widest = measure(layout::title(id), k.ty.text_small_medium) + 17.0;
        let now = fmt::now();
        let pane = &self.panes[p];
        for e in (0..pane.rows.len().min(FIT_ROWS)).filter_map(|i| pane.row_entry(i)) {
            let (value, style) = match id {
                ColumnId::Size => (size_text(e), k.ty.text_small),
                ColumnId::Modified => (e.modified.map(|m| fmt::date(m, now)).unwrap_or_else(|| "—".into()), k.ty.text_small),
                ColumnId::Kind => (file_kind(e).describe(&e.name), k.ty.text_small),
                ColumnId::Permissions => (e.mode.map(fmt::mode_string).unwrap_or_default(), k.ty.mono),
                ColumnId::Name | ColumnId::Unknown => continue,
            };
            widest = widest.max(measure(&value, style));
        }
        let w = layout::fitted(id, widest, 2.0 * CELL_PAD);
        self.prefs.columns = layout::with_width(&self.prefs.columns, id, w);
        self.save_prefs();
    }

    fn columns_menu(&mut self, p: usize, press: PointerPress, tx: &mut Tx) {
        let shown = self.shown_columns();
        let mut menu = PopupMenu::new().item(PopupItem::new("perms", "Permissions").checked(self.prefs.show_permissions)).separator();
        for c in shown.iter().filter(|c| !is_flexible(c.id)) {
            menu = menu.item(PopupItem::new(format!("fit:{}", sort_key(c.id).id()), format!("Size “{}” to Fit", layout::title(c.id))));
        }
        menu = menu.separator().item(PopupItem::new("reset", "Reset Columns"));
        tx.popup_menu(menu, press, move |s: &mut Ui, picked, _tx: &mut Tx| {
            let Some(id) = picked else { return };
            if id == "perms" {
                s.prefs.show_permissions = !s.prefs.show_permissions;
                s.save_prefs();
            } else if id == "reset" {
                s.prefs.columns = ColumnPref::defaults();
                s.save_prefs();
            } else if let Some(key) = id.strip_prefix("fit:")
                && let Some(c) = ColumnId::ALL.into_iter().find(|c| sort_key(*c).id() == key)
            {
                s.fit_column(p, c);
            }
        });
    }
}

/// What the Size column shows for `e`.
pub fn size_text(e: &filewinch_core::Entry) -> String {
    match e.kind {
        EntryKind::Dir => "—".to_owned(),
        _ => e.size.map(fmt::size).unwrap_or_else(|| "—".into()),
    }
}

/// A cell's box at column `c`'s width (Name takes the free space).
pub fn sized(e: El, id: ColumnId, w: f32) -> El {
    let e = if is_flexible(id) { e.w(0.0).grow(1.0).shrink().min_w(layout::min_width(id)) } else { e.w(w) };
    if id == ColumnId::Size { e.justify_end() } else { e }
}

/// The header row. `avail` is the header's inner width once laid out.
pub fn header(s: &Ui, p: usize, k: &Kit, paint: &Paint, avail: Option<f32>) -> El {
    let pane = &s.panes[p];
    let shown = s.shown_columns();
    let widths = layout::resolve(&shown, avail);
    let moving = match pane.header_drag {
        Some(HeaderDrag::Moving { id, dx }) => Some((id, dx)),
        _ => None,
    };
    let mut h = row().key("header").role("row").w_full().h(26.0).border_sides([0.0, 0.0, 1.0, 0.0], paint.hair).px(HEADER_PAD)
        .on_secondary_press(move |s: &mut Ui, press, tx: &mut Tx| s.columns_menu(p, press, tx));
    let last = shown.len().saturating_sub(1);
    for (i, c) in shown.iter().enumerate() {
        let id = c.id;
        let title = layout::title(id);
        let key = sort_key(id);
        let sorted = (pane.sort.key == key).then_some(pane.sort.ascending);
        let mut inner = row().gap(3.0).center_y().min_w(0.0).child(
            text(title).font(k.ty.text_small_medium).color(if sorted.is_some() { k.c.text_base } else { k.c.text_muted }).truncate(),
        );
        if let Some(asc) = sorted {
            inner = inner.child(icon(if asc { k.glyph.chevron_up } else { k.glyph.chevron_down }, 14.0).color(k.c.text_base).a11y_hidden());
        }
        let ws = widths.clone();
        let mut cell = sized(row().key(key.id()).role("columnheader").label(title).sorted(sorted).h_full().px(CELL_PAD).center_y(), id, widths[i])
            .cursor(Cursor::Pointer)
            .hover(k.c.hover)
            .on_press(move |s: &mut Ui| s.header_press(p, id))
            .on_drag(move |s: &mut Ui, d: &mut Drag, _tx: &mut Tx| s.header_drag(p, id, d, &ws))
            .child(inner.a11y_hidden());
        if moving.is_some_and(|(m, _)| m == id) {
            cell = cell.fill(k.alpha(paint.accent, 0.14));
        }
        if i < last && layout::resize_target(&shown, i).is_some() {
            cell = cell.child(divider(p, i, title, paint, avail));
        }
        h = h.child(cell);
    }
    // Where a moving header will land.
    if let Some((id, dx)) = moving
        && let Some(from) = shown.iter().position(|c| c.id == id)
    {
        let to = layout::drop_index(&widths, from, dx);
        let others: Vec<f32> = widths.iter().enumerate().filter(|(j, _)| *j != from).map(|(_, w)| *w).collect();
        let x = HEADER_PAD + others[..to.min(others.len())].iter().sum::<f32>();
        h = h.child(el().key("drop-marker").absolute(x - 1.0, 3.0).w(2.0).h(20.0).radius(1.0).fill(paint.accent).a11y_hidden());
    }
    h
}

/// The grip at a header cell's right edge.
fn divider(p: usize, i: usize, title: &str, paint: &Paint, avail: Option<f32>) -> El {
    el().key(format!("divider-{i}")).pin(None, Some(0.0), Some(0.0), Some(0.0)).w(7.0).center()
        .cursor(Cursor::ResizeColumn)
        .help(format!("Drag to resize “{title}”; double-click to fit"))
        .on_drag(move |s: &mut Ui, d: &mut Drag, _tx: &mut Tx| s.divider_drag(p, i, d, avail))
        .child(el().w(1.0).h(14.0).fill(paint.hair).a11y_hidden())
        .a11y_hidden()
}
