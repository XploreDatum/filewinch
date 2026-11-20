//! Dragging rows between panes where Rune has no system drag (Windows and Linux: `drag_out`
//! is macOS only). The row's own `on_drag` follows the pointer; the pane under it (and the
//! folder row under it) is found from where the tables and their rows were laid out. Files
//! still come in from File Explorer or Files through `on_drop_files`, and remote items go out
//! with Download to… instead of a drag to the desktop.

use filewinch_core::EntryKind;
use rune::{Drag, Phase, Tx};

use crate::transfers::engine::Op;
use crate::ui::{DragSrc, Ui};

/// The pointer travel that starts a drag (Rune's own threshold for system drags here).
const THRESHOLD: f32 = 6.0;

fn inside(r: [f32; 4], x: f32, y: f32) -> bool {
    x >= r[0] && y >= r[1] && x < r[0] + r[2] && y < r[1] + r[3]
}

impl Ui {
    /// A row of pane `p` named `name` being dragged.
    pub fn row_drag(&mut self, p: usize, name: &str, d: &Drag, tx: &mut Tx) {
        match d.phase {
            Phase::Down => {}
            Phase::Move => {
                if self.drag.is_none() {
                    if d.dx.hypot(d.dy) < THRESHOLD || self.panes[p].rename.is_some() {
                        return;
                    }
                    let Some(source) = self.panes[p].src.clone() else { return };
                    let paths = self.drag_rows(p, name);
                    if paths.is_empty() {
                        return;
                    }
                    self.drag = Some(DragSrc { pane: p, source, paths, handed: Vec::new() });
                }
                let target = self.drop_at(d.window_point());
                for (i, pane) in self.panes.iter_mut().enumerate() {
                    let t = target.as_ref().filter(|(tp, _)| *tp == i).map(|(_, f)| f.clone().unwrap_or_default());
                    if pane.drop_target != t {
                        pane.drop_target = t;
                    }
                }
            }
            Phase::Up => {
                let Some(drag) = self.drag.take() else { return };
                for pane in &mut self.panes {
                    pane.drop_target = None;
                }
                if d.cancelled() {
                    return;
                }
                let Some((to_p, folder)) = self.drop_at(d.window_point()) else { return };
                let Some(to) = self.panes[to_p].src.clone() else { return };
                let to_dir = folder.as_ref().map_or_else(|| self.panes[to_p].dir.clone(), |f| self.panes[to_p].dir.join(f));
                // As with the system drag: a drop on a folder of the same site moves, Alt moves.
                let same = drag.source.session == to.session;
                let op = if tx.mods().alt || (same && folder.is_some()) { Op::Move } else { Op::Copy };
                self.transfer(&drag.source, drag.paths, &to, to_dir, op);
                self.active = to_p;
            }
        }
    }

    /// The pane (and folder row in it) at window point `[x, y]`, if one can take a drop.
    pub fn drop_at(&self, [x, y]: [f32; 2]) -> Option<(usize, Option<String>)> {
        let p = self.panes.iter().position(|pane| pane.src.is_some() && inside(pane.rect, x, y))?;
        let pane = &self.panes[p];
        let dragged = self.drag.as_ref().map(|d| d.paths.as_slice()).unwrap_or_default();
        let folder = pane
            .rows_rect
            .filter(|r| inside(*r, x, y) && !pane.rows.is_empty())
            .and_then(|r| {
                let row_h = r[3] / pane.rows.len() as f32;
                pane.row_entry(((y - r[1]) / row_h).floor() as usize)
            })
            .filter(|e| e.kind == EntryKind::Dir && !dragged.contains(&e.path))
            .map(|e| e.name.clone());
        Some((p, folder))
    }
}
