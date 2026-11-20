//! Layout maths for a pane's chrome, kept free of UI so it can be tested on plain numbers:
//! which breadcrumbs fit the path bar, and the file table's column widths, resizing and
//! reordering.

use filewinch_store::{ColumnId, ColumnPref};

/// The first breadcrumb to show so the trail fits `avail` points. `widths` are the crumbs'
/// full widths (root first), `sep` the room a separator takes between two shown items, and
/// `ellipsis` the width of the "…" that stands for the crumbs left out. Crumbs give way from
/// the left; the last one always shows (it truncates if even it alone is too wide).
pub fn fit_crumbs(widths: &[f32], sep: f32, ellipsis: f32, avail: f32) -> usize {
    let n = widths.len();
    if n == 0 {
        return 0;
    }
    let need = |k: usize| {
        let shown: f32 = widths[k..].iter().sum::<f32>() + sep * (n - k - 1) as f32;
        if k > 0 { shown + ellipsis + sep } else { shown }
    };
    (0..n).find(|&k| need(k) <= avail).unwrap_or(n - 1)
}

/// The Name column takes whatever the others leave; the rest have widths.
pub fn is_flexible(id: ColumnId) -> bool {
    id == ColumnId::Name
}

pub fn title(id: ColumnId) -> &'static str {
    match id {
        ColumnId::Name => "Name",
        ColumnId::Size => "Size",
        ColumnId::Modified => "Modified",
        ColumnId::Kind => "Kind",
        ColumnId::Permissions => "Permissions",
        ColumnId::Unknown => "",
    }
}

pub fn standard_width(id: ColumnId) -> f32 {
    match id {
        ColumnId::Name => 200.0,
        ColumnId::Size => 80.0,
        ColumnId::Modified => 138.0,
        ColumnId::Kind => 120.0,
        ColumnId::Permissions => 96.0,
        ColumnId::Unknown => 80.0,
    }
}

pub fn min_width(id: ColumnId) -> f32 {
    match id {
        ColumnId::Name => 120.0,
        ColumnId::Size => 52.0,
        ColumnId::Modified => 80.0,
        ColumnId::Kind => 60.0,
        ColumnId::Permissions => 84.0,
        ColumnId::Unknown => 40.0,
    }
}

pub const MAX_WIDTH: f32 = 480.0;

/// The columns a table shows, in order: Permissions only when it's turned on.
pub fn visible(cols: &[ColumnPref], show_permissions: bool) -> Vec<ColumnPref> {
    ColumnPref::normalized(cols).into_iter().filter(|c| show_permissions || c.id != ColumnId::Permissions).collect()
}

/// A fixed column's width in points.
pub fn width_of(c: &ColumnPref) -> f32 {
    c.width.map_or(standard_width(c.id), |w| w as f32).clamp(min_width(c.id), MAX_WIDTH)
}

/// Every shown column's width, with Name given the room left in `avail` (or its standard width
/// before the table has been laid out).
pub fn resolve(cols: &[ColumnPref], avail: Option<f32>) -> Vec<f32> {
    let fixed: f32 = cols.iter().filter(|c| !is_flexible(c.id)).map(width_of).sum();
    cols.iter()
        .map(|c| if is_flexible(c.id) { avail.map_or(standard_width(c.id), |a| (a - fixed).max(min_width(c.id))) } else { width_of(c) })
        .collect()
}

/// The column a drag on the divider after column `divider` resizes, and which way: the column
/// on its left when that one has a width, else the one on its right (the divider then moves
/// that column's left edge, so the width changes against the pointer).
pub fn resize_target(cols: &[ColumnPref], divider: usize) -> Option<(usize, f32)> {
    let left = cols.get(divider)?;
    let right = cols.get(divider + 1)?;
    if !is_flexible(left.id) {
        Some((divider, 1.0))
    } else if !is_flexible(right.id) {
        Some((divider + 1, -1.0))
    } else {
        None
    }
}

/// The width column `target` takes when its divider has moved `dx` points from where it was at
/// `start` points, kept above its minimum and below what would squeeze Name under its own.
pub fn resized(cols: &[ColumnPref], target: usize, sign: f32, start: f32, dx: f32, avail: Option<f32>) -> f32 {
    let id = cols[target].id;
    let others: f32 = cols.iter().enumerate().filter(|(i, c)| *i != target && !is_flexible(c.id)).map(|(_, c)| width_of(c)).sum();
    let name_min: f32 = cols.iter().filter(|c| is_flexible(c.id)).map(|c| min_width(c.id)).sum();
    let room = avail.map_or(MAX_WIDTH, |a| (a - others - name_min).min(MAX_WIDTH));
    (start + sign * dx).min(room).max(min_width(id)).round()
}

/// A column's width that fits its widest content (`content`, text widths in points) plus the
/// cell's padding.
pub fn fitted(id: ColumnId, content: f32, padding: f32) -> f32 {
    (content + padding).ceil().clamp(min_width(id), MAX_WIDTH)
}

/// Where a header dragged `dx` points from column `from` lands: the index it takes among the
/// shown columns once its centre has passed the centres of the columns it moved over.
pub fn drop_index(widths: &[f32], from: usize, dx: f32) -> usize {
    let start: f32 = widths[..from].iter().sum();
    let center = start + widths[from] / 2.0 + dx;
    let mut x = 0.0;
    let mut to = 0;
    for (i, w) in widths.iter().enumerate() {
        if i != from && x + w / 2.0 < center {
            to += 1;
        }
        x += w;
    }
    to
}

/// Moves shown column `from` to shown position `to` in the full saved list `all` (which may hold
/// hidden columns: they keep their places relative to their neighbours).
pub fn reorder(all: &[ColumnPref], shown: &[ColumnPref], from: usize, to: usize) -> Vec<ColumnPref> {
    let mut all = ColumnPref::normalized(all);
    if from >= shown.len() || from == to {
        return all;
    }
    let moving = shown[from];
    let rest: Vec<ColumnId> = shown.iter().filter(|c| c.id != moving.id).map(|c| c.id).collect();
    all.retain(|c| c.id != moving.id);
    let at = match rest.get(to) {
        Some(before) => all.iter().position(|c| c.id == *before).unwrap_or(all.len()),
        None => rest.last().and_then(|last| all.iter().position(|c| c.id == *last)).map_or(all.len(), |i| i + 1),
    };
    all.insert(at, moving);
    all
}

/// `all` with column `id` set to `width` points.
pub fn with_width(all: &[ColumnPref], id: ColumnId, width: f32) -> Vec<ColumnPref> {
    ColumnPref::normalized(all).into_iter().map(|c| if c.id == id { ColumnPref { id, width: Some(width.round().max(1.0) as u32) } } else { c }).collect()
}
