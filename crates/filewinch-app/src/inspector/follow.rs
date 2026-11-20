//! Following the active pane: what the panel shows, when it changes, and opening and closing
//! the panel.

use std::time::Duration;

use rune::Tx;

use super::perms::PermsEdit;
use super::{Key, Load, Preview, Size, Subject};
use crate::ui::Ui;

impl Ui {
    /// The selection the panel should show: the active pane's, or `None` while it shows nothing.
    pub fn inspector_target(&self) -> Option<(Key, u64)> {
        let p = self.active;
        let pane = &self.panes[p];
        let src = pane.src.as_ref()?;
        let key = Key { pane: p, session: src.session, dir: pane.dir.clone(), names: pane.sel.names.clone() };
        Some((key, pane.version))
    }

    /// Whether the panel shows what the pane has selected (the view nudges a poll when not).
    pub fn inspector_current(&self) -> bool {
        let i = &self.inspector;
        match self.inspector_target() {
            Some((k, v)) => i.key.as_ref() == Some(&k) && i.version == v,
            None => i.subject.is_none(),
        }
    }

    /// The selection as the panel copies it.
    fn subject(&self, key: &Key) -> Option<Subject> {
        let pane = &self.panes[key.pane];
        let src = pane.src.as_ref()?;
        Some(Subject {
            pane: key.pane,
            session: src.session,
            local: src.is_local(),
            site: self.session(src.session).map(|s| s.site.clone()),
            label: src.label.clone(),
            remote: src.remote.clone(),
            caps: src.caps(),
            dir: pane.dir.clone(),
            items: pane.selected(),
            count: pane.rows.len(),
        })
    }

    /// Before every frame while the panel is open: follow the selection and land finished work.
    /// True when anything changed.
    pub fn inspector_poll(&mut self, tx: &mut Tx) -> bool {
        if !self.prefs.inspector_open {
            return false;
        }
        let mut changed = self.inspector_follow();
        changed |= self.inspector_land();
        if self.inspector.busy() {
            // Progress lives in atomics the worker bumps: redraw it a few times a second.
            tx.wake_after(Duration::from_millis(250));
        }
        changed
    }

    /// Takes a new selection, or a new listing of the same one. With staged permission
    /// changes, a new selection first asks whether to apply them.
    pub fn inspector_follow(&mut self) -> bool {
        let Some((key, version)) = self.inspector_target() else {
            if self.inspector.subject.is_none() {
                return false;
            }
            self.inspector.cancel_all();
            self.inspector.subject = None;
            self.inspector.key = None;
            return true;
        };
        if self.inspector.key.as_ref() == Some(&key) {
            if version == self.inspector.version {
                return false;
            }
            self.inspector_relisted(&key, version);
            return true;
        }
        if self.inspector.prompt {
            return false; // frozen until the question is answered
        }
        if self.inspector.dirty() && self.inspector.perms.applying.is_none() {
            self.inspector.prompt = true;
            return true;
        }
        self.inspector_take(key, version);
        true
    }

    /// A new selection: everything about the old one stops, and its loads start.
    fn inspector_take(&mut self, key: Key, version: u64) {
        let i = &mut self.inspector;
        i.cancel_all();
        let subject = self.subject(&key);
        let i = &mut self.inspector;
        i.key = Some(key);
        i.version = version;
        i.rename = None;
        i.details = Load::Idle;
        i.preview = Preview::None;
        i.size = Size::NotAsked;
        let modes = subject.as_ref().and_then(|s| s.modes(None)).unwrap_or_default();
        i.perms = PermsEdit::new(&modes);
        i.subject = subject;
        self.inspector_start();
    }

    /// The same selection, listed again (after a refresh, a rename elsewhere, an Apply): the
    /// copies are updated, staged changes and a counted size kept; a file whose size or date
    /// changed gets its preview again.
    fn inspector_relisted(&mut self, key: &Key, version: u64) {
        let Some(fresh) = self.subject(key) else { return };
        let i = &mut self.inspector;
        i.version = version;
        let Some(old) = &i.subject else { return };
        if old.items == fresh.items && old.count == fresh.count {
            return;
        }
        let refetch = match (old.items.as_slice(), fresh.items.as_slice()) {
            ([a], [b]) => a.size != b.size || a.modified != b.modified,
            _ => false,
        };
        let applying = i.perms.applying.is_some();
        i.subject = Some(fresh);
        if let Some(modes) = i.modes().filter(|_| !applying && i.perms.staged.is_empty()) {
            i.perms.sync_octal(&modes);
        }
        if refetch {
            self.inspector_preview(false);
        }
    }

    /// Opens or closes the panel (toolbar, View menu, ⌥⌘I / Ctrl+Alt+I), and remembers it.
    pub fn toggle_inspector(&mut self) {
        self.prefs.inspector_open = !self.prefs.inspector_open;
        if !self.prefs.inspector_open {
            // Closed: nothing keeps loading for it; opening again starts afresh.
            self.inspector.cancel_all();
            self.inspector.key = None;
            self.inspector.subject = None;
            self.inspector.prompt = false;
        } else {
            self.inspector_follow();
        }
        self.save_prefs();
    }

    /// Folds a section open or shut, remembered in the preferences.
    pub fn toggle_section(&mut self, id: &str) {
        let c = &mut self.prefs.inspector_collapsed;
        if let Some(i) = c.iter().position(|x| x == id) {
            c.remove(i);
        } else {
            c.push(id.to_owned());
        }
        self.save_prefs();
    }

    /// The width while the handle is dragged; saved when it's let go.
    pub fn set_inspector_width(&mut self, w: f32, save: bool) {
        self.prefs.inspector_width = w.round().clamp(super::MIN_W, super::MAX_W) as u32;
        if save {
            self.save_prefs();
        }
    }

    /// "Apply changes to “x”?" answered: apply them to the old selection (or drop them), then
    /// follow the new one.
    pub fn answer_prompt(&mut self, apply: bool) {
        self.inspector.prompt = false;
        if apply {
            self.apply_perms(false);
        } else if let Some(m) = self.inspector.modes() {
            self.inspector.perms.revert(&m);
        }
        // The Apply runs on for the old selection (a failure is reported in a notice, as the
        // panel has moved on); the panel shows the new one.
        if let Some((key, version)) = self.inspector_target() {
            self.inspector_take(key, version);
        }
    }

    /// The question closed without an answer: go back to the selection that has the changes.
    pub fn keep_editing(&mut self) {
        self.inspector.prompt = false;
        let Some(key) = self.inspector.key.clone() else { return };
        let pane = &mut self.panes[key.pane];
        if pane.session() == Some(key.session) && pane.dir == key.dir {
            pane.sel.names = key.names.clone();
            self.active = key.pane;
        }
    }

    /// Esc in the panel: back to the pane it follows.
    pub fn inspector_escape(&mut self, tx: &mut Tx) {
        self.inspector.rename = None;
        tx.focus(format!("pane-{}", self.active), true);
    }
}
