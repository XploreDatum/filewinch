//! A pane's listing as data: sort, filter and selection, with no UI in it.

use std::cmp::Ordering;
use std::collections::HashSet;

use filewinch_core::{Entry, EntryKind};

use crate::icons::{FileKind, kind_of};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Modified,
    Kind,
    Permissions,
}

impl SortKey {
    pub fn id(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Size => "size",
            SortKey::Modified => "modified",
            SortKey::Kind => "kind",
            SortKey::Permissions => "permissions",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub ascending: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Sort { key: SortKey::Name, ascending: true }
    }
}

/// Names compared as people read them: case-insensitive, digits by value (`file2` before
/// `file10`).
pub fn natural(a: &str, b: &str) -> Ordering {
    rune_kit::natural_cmp(a, b)
}

fn is_dirish(e: &Entry) -> bool {
    e.kind == EntryKind::Dir
}

/// A file Filewinch itself writes while a transfer runs, never shown even with hidden files on:
/// a segmented download's `.name.filewinch-part` (and its `-ranges`, `-old` siblings) and the
/// local backend's `.name.filewinch-<pid>-<n>` stand-in for a file being replaced.
pub fn is_transfer_temp(name: &str) -> bool {
    name.starts_with('.') && name.contains(".filewinch-")
}

/// The visible rows' indexes into `entries`, filtered and sorted. Folders come first whichever
/// way a column sorts.
pub fn arrange(entries: &[Entry], sort: Sort, filter: &str, show_hidden: bool) -> Vec<u32> {
    let needle = filter.trim().to_lowercase();
    let mut rows: Vec<u32> = (0..entries.len() as u32)
        .filter(|&i| {
            let e = &entries[i as usize];
            !is_transfer_temp(&e.name)
                && (show_hidden || !filewinch_local::is_hidden(e))
                && (needle.is_empty() || e.name.to_lowercase().contains(&needle))
        })
        .collect();
    let kinds: Vec<String> = if sort.key == SortKey::Kind {
        entries.iter().map(|e| kind_of(&e.name, e.kind).describe(&e.name)).collect()
    } else {
        Vec::new()
    };
    rows.sort_by(|&a, &b| {
        let (x, y) = (&entries[a as usize], &entries[b as usize]);
        let dirs = is_dirish(y).cmp(&is_dirish(x));
        if dirs != Ordering::Equal {
            return dirs;
        }
        let by = match sort.key {
            SortKey::Name => Ordering::Equal,
            SortKey::Size => x.size.unwrap_or(0).cmp(&y.size.unwrap_or(0)),
            SortKey::Modified => x.modified.cmp(&y.modified),
            SortKey::Kind => kinds[a as usize].cmp(&kinds[b as usize]),
            SortKey::Permissions => x.mode.cmp(&y.mode),
        };
        let o = by.then_with(|| natural(&x.name, &y.name)).then_with(|| x.name.cmp(&y.name));
        if sort.ascending { o } else { o.reverse() }
    });
    rows
}

pub fn file_kind(e: &Entry) -> FileKind {
    kind_of(&e.name, e.kind)
}

/// Which rows are selected (by name, so it survives a re-sort or refresh), the keyboard cursor
/// and the anchor a Shift-click extends from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Selection {
    pub names: HashSet<String>,
    pub cursor: Option<String>,
    pub anchor: Option<String>,
}

/// How a click or key changes the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    /// Select only this row.
    Only,
    /// Cmd-click: add or remove this row.
    Toggle,
    /// Shift: everything from the anchor to here.
    Extend,
}

impl Selection {
    pub fn is_selected(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    pub fn clear(&mut self) {
        self.names.clear();
        self.anchor = None;
    }

    pub fn only(&mut self, name: &str) {
        self.names.clear();
        self.names.insert(name.to_owned());
        self.cursor = Some(name.to_owned());
        self.anchor = Some(name.to_owned());
    }

    /// Applies a pick of row `i` in `order` (visible names, top to bottom).
    pub fn pick(&mut self, order: &[&str], i: usize, how: Pick) {
        let Some(&name) = order.get(i) else { return };
        match how {
            Pick::Only => self.only(name),
            Pick::Toggle => {
                if !self.names.remove(name) {
                    self.names.insert(name.to_owned());
                }
                self.cursor = Some(name.to_owned());
                self.anchor = Some(name.to_owned());
            }
            Pick::Extend => {
                let anchor = self.anchor.as_deref().and_then(|a| order.iter().position(|n| *n == a)).unwrap_or(i);
                let (lo, hi) = (anchor.min(i), anchor.max(i));
                self.names = order[lo..=hi].iter().map(|n| (*n).to_owned()).collect();
                self.cursor = Some(name.to_owned());
                if self.anchor.is_none() {
                    self.anchor = Some(name.to_owned());
                }
            }
        }
    }

    /// The cursor's row in `order`, if it's still there.
    pub fn cursor_row(&self, order: &[&str]) -> Option<usize> {
        let c = self.cursor.as_deref()?;
        order.iter().position(|n| *n == c)
    }

    /// Moves the cursor by `delta` rows (clamped), selecting as `extend` says.
    pub fn step(&mut self, order: &[&str], delta: isize, extend: bool) -> Option<usize> {
        if order.is_empty() {
            return None;
        }
        let target = match self.cursor_row(order) {
            Some(r) => (r as isize + delta).clamp(0, order.len() as isize - 1) as usize,
            None if delta < 0 => order.len() - 1,
            None => 0,
        };
        self.pick(order, target, if extend { Pick::Extend } else { Pick::Only });
        Some(target)
    }

    pub fn select_all(&mut self, order: &[&str]) {
        self.names = order.iter().map(|n| (*n).to_owned()).collect();
        if self.cursor.is_none() {
            self.cursor = order.first().map(|n| (*n).to_owned());
        }
    }

    /// Drops names that are no longer listed (after a refresh).
    pub fn retain_listed(&mut self, listed: &HashSet<&str>) {
        self.names.retain(|n| listed.contains(n.as_str()));
        if self.cursor.as_deref().is_some_and(|c| !listed.contains(c)) {
            self.cursor = None;
        }
        if self.anchor.as_deref().is_some_and(|c| !listed.contains(c)) {
            self.anchor = None;
        }
    }
}

/// Type-to-select: the typed prefix, and when it was last extended (seconds on the UI clock).
#[derive(Clone, Debug, Default)]
pub struct TypeAhead {
    pub typed: String,
    pub at: f64,
}

impl TypeAhead {
    /// Typing within a second extends the prefix; a pause starts a new one. Returns the first
    /// row whose name starts with the prefix (case-insensitive), else the first that sorts
    /// after it.
    pub fn type_char(&mut self, c: &str, now: f64, order: &[&str]) -> Option<usize> {
        if now - self.at > 1.0 {
            self.typed.clear();
        }
        self.at = now;
        self.typed.push_str(&c.to_lowercase());
        let p = self.typed.as_str();
        order
            .iter()
            .position(|n| n.to_lowercase().starts_with(p))
            .or_else(|| order.iter().position(|n| natural(&n.to_lowercase(), p) == Ordering::Greater))
    }
}

/// The name for a new item: `base`, or `base 2`, `base 3`… (before the extension).
pub fn unique_name(base: &str, taken: &HashSet<&str>) -> String {
    if !taken.contains(base) {
        return base.to_owned();
    }
    let (stem, ext) = match base.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (base, String::new()),
    };
    (2..).map(|i| format!("{stem} {i}{ext}")).find(|n| !taken.contains(n.as_str())).unwrap_or_else(|| base.to_owned())
}

/// Why a name can't be used, or `None` when it can.
/// Why `name` can't be used. On this computer (`local`) the OS's own rules apply: Windows
/// also refuses `< > : " \\ | ? *`, trailing dots and spaces, and names like CON and NUL.
/// Servers decide for themselves beyond `/`, NUL, `.` and `..`.
pub fn invalid_name(name: &str, local: bool) -> Option<String> {
    let os = if local { filewinch_os::Os::CURRENT } else { filewinch_os::Os::Linux };
    filewinch_os::names::check(os, name)
}
