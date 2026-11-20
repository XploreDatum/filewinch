//! Pointer and keyboard on a pane: clicks with Shift and Cmd, arrows, Return, Cmd-Up, Delete,
//! F2 rename, Cmd-A and type-to-select.

use rune::{Key, KeyEvent, TextEdit, Tx};

use super::model::{Pick, Sort, SortKey};
use crate::ui::Ui;

impl Ui {
    pub fn row_click(&mut self, p: usize, name: &str, tx: &mut Tx) {
        self.active = p;
        let mods = tx.mods();
        let pane = &mut self.panes[p];
        if pane.rename.as_ref().is_some_and(|r| r.original != name) {
            pane.rename = None;
        }
        let order: Vec<String> = pane.order().into_iter().map(str::to_owned).collect();
        let order: Vec<&str> = order.iter().map(String::as_str).collect();
        let Some(i) = order.iter().position(|n| *n == name) else { return };
        let how = if mods.shift {
            Pick::Extend
        } else if mods.cmd {
            Pick::Toggle
        } else {
            Pick::Only
        };
        pane.sel.pick(&order, i, how);
        tx.focus(format!("pane-{p}"), true);
    }

    pub fn sort_by(&mut self, p: usize, key: SortKey) {
        let hidden = self.prefs.show_hidden;
        let pane = &mut self.panes[p];
        pane.sort = if pane.sort.key == key { Sort { key, ascending: !pane.sort.ascending } } else { Sort { key, ascending: true } };
        pane.rearrange(hidden);
    }

    /// The rename field's text (a spare when no rename is open, so a late edit lands nowhere).
    pub fn rename_edit(&mut self, p: usize) -> &mut TextEdit {
        match &mut self.panes[p].rename {
            Some(r) => &mut r.edit,
            None => &mut self.panes[p].filter,
        }
    }

    pub fn refresh_pane(&mut self, p: usize) {
        let hidden = self.prefs.show_hidden;
        self.panes[p].refresh(hidden);
    }

    pub fn go_back(&mut self, p: usize) {
        let hidden = self.prefs.show_hidden;
        self.panes[p].back(hidden);
    }

    pub fn go_forward(&mut self, p: usize) {
        let hidden = self.prefs.show_hidden;
        self.panes[p].forward(hidden);
    }

    pub fn go_up(&mut self, p: usize) {
        let hidden = self.prefs.show_hidden;
        self.panes[p].up(hidden);
    }

    fn step(&mut self, p: usize, delta: isize, extend: bool, tx: &mut Tx) {
        let pane = &mut self.panes[p];
        let order: Vec<String> = pane.order().into_iter().map(str::to_owned).collect();
        let order: Vec<&str> = order.iter().map(String::as_str).collect();
        if let Some(i) = pane.sel.step(&order, delta, extend) {
            tx.scroll_into_view_in(format!("pane-{p}"), order[i].to_owned());
        }
    }

    fn jump(&mut self, p: usize, to_end: bool, extend: bool, tx: &mut Tx) {
        let pane = &mut self.panes[p];
        let order: Vec<String> = pane.order().into_iter().map(str::to_owned).collect();
        let order: Vec<&str> = order.iter().map(String::as_str).collect();
        if order.is_empty() {
            return;
        }
        let i = if to_end { order.len() - 1 } else { 0 };
        pane.sel.pick(&order, i, if extend { Pick::Extend } else { Pick::Only });
        tx.scroll_into_view_in(format!("pane-{p}"), order[i].to_owned());
    }

    /// Keys while pane `p`'s list has focus. True when taken.
    pub fn pane_key(&mut self, p: usize, ev: &KeyEvent, tx: &mut Tx) -> bool {
        self.active = p;
        if self.panes[p].rename.is_some() {
            return false; // the rename field has its own keys
        }
        if self.panes[p].is_empty() {
            return self.empty_pane_key(p, ev); // nothing listed to act on
        }
        let m = ev.mods;
        let page = 20;
        match &ev.key {
            Key::Up if m.cmd => self.go_up(p),
            Key::Down if m.cmd => self.open_selection(p),
            Key::Up => self.step(p, -1, m.shift, tx),
            Key::Down => self.step(p, 1, m.shift, tx),
            Key::PageUp => self.step(p, -page, m.shift, tx),
            Key::PageDown => self.step(p, page, m.shift, tx),
            Key::Home => self.jump(p, false, m.shift, tx),
            Key::End => self.jump(p, true, m.shift, tx),
            Key::Enter if m.alt => self.begin_rename(p, tx),
            Key::Enter => self.open_selection(p),
            Key::Function(2) => self.begin_rename(p, tx),
            Key::Backspace if m.cmd => self.ask_delete(p),
            Key::Delete => self.ask_delete(p),
            Key::Escape => {
                let pane = &mut self.panes[p];
                if pane.sel.names.is_empty() {
                    return false;
                }
                pane.sel.clear();
            }
            Key::Command('a') => {
                let pane = &mut self.panes[p];
                let order: Vec<String> = pane.order().into_iter().map(str::to_owned).collect();
                let order: Vec<&str> = order.iter().map(String::as_str).collect();
                pane.sel.select_all(&order);
            }
            Key::Command('[') => self.go_back(p),
            Key::Command(']') => self.go_forward(p),
            Key::Command('o') => self.open_selection(p),
            Key::Tab if !m.cmd => {
                let other = self.other(p);
                self.active = other;
                tx.focus(format!("pane-{other}"), true);
            }
            Key::Text(t) if !m.cmd && !m.ctrl && !t.trim().is_empty() => {
                let now = tx.clock();
                let pane = &mut self.panes[p];
                let order: Vec<String> = pane.order().into_iter().map(str::to_owned).collect();
                let order: Vec<&str> = order.iter().map(String::as_str).collect();
                if let Some(i) = pane.type_ahead.type_char(t, now, &order) {
                    pane.sel.pick(&order, i, Pick::Only);
                    tx.scroll_into_view_in(format!("pane-{p}"), order[i].to_owned());
                }
            }
            _ => return false,
        }
        true
    }
}
