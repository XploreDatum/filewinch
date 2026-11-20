//! A job's items in its Transfers row: the files moving now, inline under the header (each
//! with a pie that turns into a skip × on hover or focus), and behind "Show all N items" a
//! grouped list (Downloading now, Queued, Done, Failed, Skipped) built a screen at a time.

use std::time::Instant;

use rune::{El, Icon, Key, Tx, col, icon, row, text};
use rune_kit::{IconButton, IconButtonVariant, Kit, Loader};

use super::engine::{ActivePhase, Event, ItemState, ItemView, JobId};
use super::model::{Direction, GROUPS, Line, PAGE, Row};
use crate::fmt;
use crate::hub::hub;
use crate::icons;
use crate::theme::Paint;
use crate::ui::Ui;

/// The most files shown inline (the live limit's maximum).
const INLINE_MAX: usize = super::engine::MAX_FILES_PER_JOB_LIVE;
/// The expanded list's height cap; it scrolls inside.
const LIST_MAX_H: f32 = 300.0;
const LINE_H: f32 = 24.0;

impl Ui {
    /// Pulls job items after engine events: everything when they changed, the running files'
    /// progress with each progress event.
    pub fn follow_items(&mut self, ev: &Event) {
        match ev {
            Event::ItemsChanged(id) => self.refresh_items(*id, true),
            Event::Progress(id, _) if self.transfers.row(*id).is_some_and(|r| r.many_files()) => self.refresh_items(*id, false),
            _ => {}
        }
    }

    /// Fetches job `id`'s counts and running files; `all` also drops the fetched pages so the
    /// rows on screen are pulled again.
    pub fn refresh_items(&mut self, id: JobId, all: bool) {
        let engine = &hub().engine;
        let counts = engine.item_counts(id);
        let active: Vec<ItemView> = engine.items(id, ItemState::Active, 0..INLINE_MAX * 4).1.into_iter().filter(|v| !v.is_dir).take(INLINE_MAX).collect();
        let Some(r) = self.transfers.row_mut(id) else { return };
        let l = &mut r.items;
        l.counts = counts;
        l.active = active;
        if all {
            l.pages.clear();
            l.version += 1;
        }
    }

    /// Fetches the pages behind lines `range` of job `id`'s list. True when something new
    /// arrived (the view runs again).
    pub fn fetch_lines(&mut self, id: JobId, range: std::ops::Range<usize>) -> bool {
        let Some(r) = self.transfers.row(id) else { return false };
        let mut want: Vec<(usize, usize)> = range
            .filter_map(|i| match r.items.line(i) {
                Some(Line::Item(g, i)) => Some((g, i / PAGE)),
                _ => None,
            })
            .filter(|k| !r.items.pages.contains_key(k))
            .collect();
        want.dedup();
        if want.is_empty() {
            return false;
        }
        let got: Vec<((usize, usize), Vec<ItemView>)> =
            want.into_iter().map(|(g, p)| ((g, p), hub().engine.items(id, GROUPS[g], p * PAGE..(p + 1) * PAGE).1)).collect();
        let Some(r) = self.transfers.row_mut(id) else { return false };
        r.items.pages.extend(got);
        r.items.version += 1;
        true
    }

    /// Skips one item of job `id` (a folder skips what's inside it).
    pub fn skip_item(&mut self, id: JobId, path: &filewinch_core::RPath) {
        hub().engine.skip_item(id, path);
        self.refresh_items(id, true);
    }

    fn toggle_items(&mut self, id: JobId) {
        if let Some(r) = self.transfers.row_mut(id) {
            r.items.open = !r.items.open;
            r.items.version += 1;
        }
    }

    fn toggle_group(&mut self, id: JobId, g: usize) {
        if let Some(r) = self.transfers.row_mut(id) {
            r.items.groups_open[g] = !r.items.groups_open[g];
            r.items.version += 1;
        }
    }
}

/// "Friends.S01E05…East.mkv": keeps the start and the end (extension, episode) of a long name.
pub fn middle(name: &str, max: usize) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= max || max < 8 {
        return name.to_owned();
    }
    let tail = (max - 1) * 2 / 5;
    let head = max - 1 - tail;
    format!("{}…{}", chars[..head].iter().collect::<String>(), chars[chars.len() - tail..].iter().collect::<String>())
}

fn name_of(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

fn folder_of(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(d, _)| d)
}

/// "Retrying in 4 seconds" / "4 s". 0 once the moment has passed but the engine hasn't
/// started the next try yet (said as "Retrying now", never "in 0 seconds").
fn retry_left(at: Instant) -> u64 {
    at.saturating_duration_since(Instant::now()).as_secs_f64().ceil() as u64
}

/// The item's state for assistive technology and tooltips.
pub fn item_status(v: &ItemView) -> String {
    let pct = |v: &ItemView| v.size.filter(|s| *s > 0).map(|s| (v.bytes_done as f64 / s as f64 * 100.0).round() as u64);
    match (v.state, v.phase) {
        (ItemState::Active, Some(ActivePhase::Connecting)) => "Connecting…".into(),
        (ItemState::Active, Some(ActivePhase::Retrying { at })) => match retry_left(at) {
            0 => "Retrying now".into(),
            1 => "Retrying in 1 second".into(),
            n => format!("Retrying in {n} seconds"),
        },
        (ItemState::Active, _) => pct(v).map_or_else(|| "Transferring".into(), |p| format!("{p}% downloaded")),
        (ItemState::Queued, _) => "Queued".into(),
        (ItemState::Done, _) => "Done".into(),
        (ItemState::Failed, _) => v.error.as_ref().map_or_else(|| "Failed".into(), |e| format!("Failed: {}", e.message)),
        (ItemState::Skipped, _) => "Skipped".into(),
    }
}

/// A pie that fills clockwise from 12 o'clock (`v` in 0..=1), quantised to 2 %.
fn pie(v: f32) -> &'static Icon {
    use std::sync::Mutex;
    static CACHE: Mutex<Vec<Option<&'static Icon>>> = Mutex::new(Vec::new());
    let step = (v.clamp(0.0, 1.0) * 50.0).round() as usize;
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() < 51 {
        cache.resize(51, None);
    }
    if let Some(i) = cache[step] {
        return i;
    }
    let ring = r#"<circle cx="8" cy="8" r="6.5" stroke="currentColor" stroke-width="1.2" fill="none"/>"#;
    let wedge = match step {
        0 => String::new(),
        50 => r#"<circle cx="8" cy="8" r="5" fill="currentColor"/>"#.to_owned(),
        _ => {
            let a = step as f32 / 50.0 * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
            let (x, y) = (8.0 + 5.0 * a.cos(), 8.0 + 5.0 * a.sin());
            let large = u8::from(step > 25);
            format!(r#"<path d="M8 8V3A5 5 0 {large} 1 {x:.3} {y:.3}Z" fill="currentColor"/>"#)
        }
    };
    let body: &'static str = Box::leak(format!("{ring}{wedge}").into_boxed_str());
    let i: &'static Icon = Box::leak(Box::new(Icon { name: "fw-pie", view: 16.0, body }));
    cache[step] = Some(i);
    i
}

/// The skip control: a pie (or a spinner while nothing moves yet) that shows an × on hover or
/// keyboard focus.
fn skip_button(id: JobId, v: &ItemView, k: &Kit, paint: &Paint) -> El {
    let name = name_of(&v.rel).to_owned();
    let path = v.path.clone();
    let indicator = match (v.state, v.phase) {
        (ItemState::Active, Some(ActivePhase::Transferring)) => {
            let f = v.size.filter(|s| *s > 0).map_or(0.0, |s| v.bytes_done as f32 / s as f32);
            icon(pie(f), 16.0).color(paint.accent)
        }
        // Under reduced motion the loader holds still: a static "waiting" ring.
        (ItemState::Active, _) => Loader::new().size(14.0).color(paint.accent).build(k),
        _ => rune::el().size(16.0, 16.0),
    };
    let x = IconButton::new(&icons::STOP).variant(IconButtonVariant::Ghost).control_size(rune_kit::ControlSize::Small)
        .label(format!("Skip {name}, {}", item_status(v).to_lowercase()))
        .help(format!("Skip “{name}”"))
        .on_press(move |s: &mut Ui| s.skip_item(id, &path))
        .build(k).key("skip").group_opacity(0.0, 1.0);
    rune::el().key("skip-box").size(24.0, 24.0).center()
        .child(indicator.a11y_hidden().group_opacity(1.0, 0.0).absolute(4.0, 4.0))
        .child(x.absolute(0.0, 0.0))
}

fn file_icon(v: &ItemView) -> &'static Icon {
    if v.is_dir { &icons::FOLDER } else { icons::kind_of(name_of(&v.rel), filewinch_core::EntryKind::File).icon() }
}

/// One inline row for a file moving now.
fn active_row(id: JobId, v: &ItemView, k: &Kit, paint: &Paint) -> El {
    let name = name_of(&v.rel);
    let mut line = row().gap(6.0).center_y().grow(1.0).min_w(0.0)
        .child(icon(file_icon(v), 14.0).color(k.c.icon_muted).a11y_hidden())
        .child(text(middle(name, 44)).font(k.ty.text_xsmall).color(k.c.text_base).truncate().shrink().min_w(0.0).a11y_hidden());
    let extra = match v.phase {
        Some(ActivePhase::Retrying { at }) => match retry_left(at) {
            0 => "Retrying now…".to_owned(),
            n => format!("Retrying in {n} s…"),
        },
        Some(ActivePhase::Connecting) => "Connecting…".to_owned(),
        _ => match v.size {
            Some(s) if v.bytes_per_sec >= 1.0 => format!("{} of {} · {}", fmt::size(v.bytes_done), fmt::size(s), fmt::speed(v.bytes_per_sec)),
            Some(s) => format!("{} of {}", fmt::size(v.bytes_done), fmt::size(s)),
            None => fmt::size(v.bytes_done),
        },
    };
    line = line.child(text(extra).font(k.ty.text_xsmall).color(k.c.text_faint).a11y_hidden());
    row().key(format!("now-{}", v.rel)).role("listitem").label(format!("{name}, {}", item_status(v))).help(item_status(v))
        .w_full().h(24.0).pl(26.0).gap(6.0).center_y().hover_group()
        .child(line)
        .child(skip_button(id, v, k, paint))
}

/// The inline rows for the files moving now (multi-file jobs only), and the "Show all"
/// disclosure with its list.
pub fn items_section(r: &Row, k: &Kit, paint: &Paint, dark: bool) -> Option<El> {
    if !r.many_files() {
        return None;
    }
    let id = r.id;
    let l = &r.items;
    let mut c = col().key("items").w_full().gap(0.0);
    if !r.state.is_finished() {
        let mut now = col().key("now").role("list").label("Moving now").w_full();
        for v in l.active.iter().take(r.files_at_once.max(1)) {
            now = now.child(active_row(id, v, k, paint));
        }
        c = c.child(now);
    }
    let total = l.total();
    if total == 0 {
        return Some(c);
    }
    let label = if l.open { "Hide items".to_owned() } else if total == 1 { "Show the item".to_owned() } else { format!("Show all {} items", fmt::count(total as u64)) };
    let toggle = row().key("items-toggle").role("button").label(label.clone()).expanded(l.open)
        .gap(4.0).center_y().h(22.0).ml(22.0).px(4.0).radius(4.0).hover(k.c.hover)
        .on_press(move |s: &mut Ui| s.toggle_items(id))
        .child(icon(if l.open { k.glyph.chevron_down } else { &icons::FORWARD }, 12.0).color(k.c.icon_muted).a11y_hidden())
        .child(text(label).font(k.ty.text_xsmall_medium).color(k.c.text_muted).a11y_hidden());
    c = c.child(row().w_full().child(toggle));
    if l.open {
        c = c.child(item_list(r, dark));
    }
    Some(c)
}

fn group_title(g: usize, d: Direction) -> &'static str {
    match (GROUPS[g], d) {
        (ItemState::Active, Direction::Upload) => "Uploading now",
        (ItemState::Active, Direction::Download) => "Downloading now",
        (ItemState::Active, _) => "Copying now",
        (ItemState::Queued, _) => "Queued",
        (ItemState::Done, _) => "Done",
        (ItemState::Failed, _) => "Failed",
        (ItemState::Skipped, _) => "Skipped",
    }
}

/// The grouped list: virtual rows over headers and items, pulled a page at a time.
fn item_list(r: &Row, dark: bool) -> El {
    let id = r.id;
    let lines = r.items.lines();
    let height = (lines as f32 * LINE_H).min(LIST_MAX_H);
    let dir = r.direction;
    let rows = col().key("rows").w_full().growth(rune::GrowthSpec::default())
        .virtual_rows(
            lines,
            LINE_H,
            move |s: &Ui, i| {
                let l = s.transfers.row(id).map(|r| &r.items);
                match l.and_then(|l| l.line(i)) {
                    Some(Line::Header(g)) => format!("h{g}").into(),
                    Some(Line::Item(g, i)) => match l.and_then(|l| l.item(g, i)) {
                        Some(v) => format!("{g}-{}", v.path).into(),
                        None => format!("{g}-#{i}").into(),
                    },
                    None => format!("x{i}").into(),
                }
            },
            move |s: &Ui, i| {
                let Some(r) = s.transfers.row(id) else { return rune::el() };
                let k = crate::theme::kit(s.dark());
                let paint = crate::theme::paint(&k);
                match r.items.line(i) {
                    Some(Line::Header(g)) => header_line(id, g, r, dir, &k),
                    Some(Line::Item(g, j)) => match r.items.item(g, j) {
                        Some(v) => item_line(id, v, &k, &paint),
                        None => row().h(LINE_H).px(8.0).center_y().child(text("…").font(k.ty.text_xsmall).color(k.c.text_faint)),
                    },
                    None => rune::el(),
                }
            },
        )
        // Rows are cached by version: the appearance and accent rebuild them too.
        .virtual_rows_version((r.items.version, dark, crate::theme::version(dark)))
        .on_visible_rows(move |s: &mut Ui, range| s.fetch_lines(id, range));
    col().key("item-list").role("list").label("Items").focus_group().w_full().h(height).pl(22.0)
        .scroll(rune::ScrollSpec { stick_bottom: false })
        .child(rows)
}

fn header_line(id: JobId, g: usize, r: &Row, d: Direction, k: &Kit) -> El {
    let open = r.items.groups_open[g];
    let n = r.items.count(g);
    let title = format!("{} ({})", group_title(g, d), fmt::count(n as u64));
    row().key(format!("group-{g}")).role("button").label(title.clone()).expanded(open).focusable(true)
        .w_full().h(LINE_H).gap(4.0).px(4.0).center_y().radius(4.0).hover(k.c.hover)
        .on_press(move |s: &mut Ui| s.toggle_group(id, g))
        .child(icon(if open { k.glyph.chevron_down } else { &icons::FORWARD }, 12.0).color(k.c.icon_muted).a11y_hidden())
        .child(text(title).font(k.ty.text_xsmall_medium).color(k.c.text_muted).a11y_hidden())
}

fn item_line(id: JobId, v: &ItemView, k: &Kit, paint: &Paint) -> El {
    let name = name_of(&v.rel).to_owned();
    let folder = folder_of(&v.rel).to_owned();
    let status = item_status(v);
    let can_skip = matches!(v.state, ItemState::Active | ItemState::Queued);
    let tone = match v.state {
        ItemState::Failed => paint.danger,
        ItemState::Skipped | ItemState::Done => k.c.text_faint,
        _ => k.c.text_base,
    };
    let mut words = row().gap(6.0).center_y().grow(1.0).min_w(0.0)
        .child(icon(file_icon(v), 14.0).color(k.c.icon_muted).a11y_hidden())
        .child(text(middle(&name, 40)).font(k.ty.text_xsmall).color(tone).truncate().shrink().min_w(0.0).a11y_hidden());
    if !folder.is_empty() {
        words = words.child(text(folder).font(k.ty.text_xsmall).color(k.c.text_faint).truncate().shrink().min_w(0.0).a11y_hidden());
    }
    let right = match v.state {
        ItemState::Active | ItemState::Queued => v.size.map(fmt::size).unwrap_or_default(),
        ItemState::Skipped => "Skipped".into(),
        ItemState::Failed => v.error.as_ref().map_or_else(|| "Failed".into(), |e| e.message.clone()),
        ItemState::Done => v.size.map(fmt::size).unwrap_or_default(),
    };
    let path = v.path.clone();
    let mut line = row().key("line").role("listitem").label(format!("{name}, {status}")).help(status).focusable(true)
        .w_full().h(LINE_H).px(4.0).gap(6.0).center_y().radius(4.0).hover_group()
        .child(words)
        .child(text(right).font(k.ty.text_xsmall).color(k.c.text_faint).truncate().max_w(220.0).a11y_hidden());
    if can_skip {
        let p = path.clone();
        line = line
            .on_key(move |s: &mut Ui, ev: &rune::KeyEvent, _tx: &mut Tx| {
                if matches!(ev.key, Key::Delete | Key::Backspace) {
                    s.skip_item(id, &p);
                    return true;
                }
                false
            })
            .child(skip_button(id, v, k, paint));
    } else {
        line = line.child(rune::el().size(24.0, 24.0));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::middle;

    #[test]
    fn long_names_keep_their_start_and_end() {
        assert_eq!(middle("short.mkv", 20), "short.mkv");
        let m = middle("Friends.S01E05.The.One.With.The.East.German.Laundry.Detergent.East.mkv", 30);
        assert_eq!(m.chars().count(), 30);
        assert!(m.starts_with("Friends.S01E05") && m.ends_with("East.mkv"), "{m}");
    }
}
