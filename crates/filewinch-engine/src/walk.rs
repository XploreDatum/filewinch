//! The scanning phase: walks source folders iteratively (an explicit stack, no recursion),
//! creates destination folders ahead of their files, counts totals and feeds files to workers.

use crate::runner::{Item, JobCtx, Tally};
use crate::transfer;
use crate::{ConflictPolicy, Op};
use filewinch_core::{Entry, EntryKind, Error, ErrorKind, RPath, Remote, Result};
use std::sync::mpsc::Sender;

fn send(ctx: &JobCtx, tx: &Sender<Item>, src: Entry, dst: RPath, keep_src: bool) {
    let size = src.size.unwrap_or(0);
    ctx.meter.add_total(1, size);
    let already = ctx.done.lock().unwrap_or_else(|e| e.into_inner()).contains(&src.path);
    if already {
        // Finished in an earlier run of this job (Engine::retry).
        ctx.meter.add_bytes(size);
        ctx.meter.file_done();
        return;
    }
    // Workers outlive the scan, so the receiver is alive while we send.
    let _ = tx.send(Item { src, dst, keep_src });
}

/// Follows a symlink (up to 8 hops) to what it points at, keeping the link's own name and
/// path. Uses `link_target` when the backend fills it, else `stat` (some backends follow).
/// None when the target is missing or the chain is too long.
pub(crate) fn follow_link(r: &dyn Remote, link: &Entry) -> Option<Entry> {
    let mut cur = link.clone();
    for _ in 0..8 {
        if cur.kind != EntryKind::Symlink {
            return Some(Entry { name: link.name.clone(), path: link.path.clone(), ..cur });
        }
        let next = match &cur.link_target {
            Some(t) if t.starts_with('/') => RPath::new(t),
            Some(t) => cur.path.parent().unwrap_or_else(RPath::root).join(t),
            None => cur.path.clone(),
        };
        let e = r.stat(&next).ok()?;
        if cur.link_target.is_none() && e.kind == EntryKind::Symlink {
            return None; // stat doesn't follow and no target is known
        }
        cur = e;
    }
    None
}

/// What a symlink inside a folder becomes: a file to copy, or nothing for a linked folder
/// (following those risks loops; a selected top-level link to a folder is followed).
fn resolve_link(ctx: &JobCtx, e: Entry) -> Option<Entry> {
    match follow_link(ctx.src(), &e) {
        Some(t) if t.kind == EntryKind::File => Some(t),
        _ => None,
    }
}

pub(crate) fn scan_transfer(
    ctx: &JobCtx,
    tally: &Tally,
    tx: &Sender<Item>,
    src_dirs: &mut Vec<RPath>,
) -> Result<()> {
    let dst = ctx.job.dst.as_deref().expect("checked by run_transfer");
    let dst_dir = ctx.job.dst_dir.as_ref().expect("checked by run_transfer");
    let same = ctx.same_remote();
    // A Move that copies then deletes would empty a shared item for everyone: copy it, keep
    // the source. (A server-side rename moves the share itself and is fine.)
    let copy_deletes = ctx.job.op == Op::Move && !(same && ctx.src().caps().server_rename);
    // (source folder, destination folder, keep the source)
    let mut stack: Vec<(RPath, RPath, bool)> = Vec::new();

    for p in &ctx.job.src_paths {
        ctx.ctl.checkpoint()?;
        let mut e = match ctx.retrying(|| ctx.src().stat(p)) {
            Ok(e) => e,
            Err(e) => {
                ctx.fail(tally, p, e);
                continue;
            }
        };
        let mut target = dst_dir.join(p.name());
        if same && target.starts_with(p) && target != *p {
            let msg = format!("“{}” can't be put inside itself.", p.name());
            ctx.fail(tally, p, Error::new(ErrorKind::InvalidPath, msg));
            continue;
        }
        if same && target == *p {
            if ctx.job.op == Op::Move {
                continue; // moving onto itself: nothing to do
            }
            // Copying into its own folder makes a duplicate, like the Finder does.
            target = match transfer::free_name(ctx, dst, &target) {
                Ok(t) => t,
                Err(err) => {
                    ctx.fail(tally, p, err);
                    continue;
                }
            };
        }
        if e.kind == EntryKind::Symlink {
            match follow_link(ctx.src(), &e) {
                Some(t) => e = t,
                None if e.link_target.is_none() => {
                    // e.g. a OneDrive shortcut to someone else's folder: browsable, not followable.
                    let msg = format!(
                        "“{}” is a link the server can't resolve to a path. Open it and copy what's inside instead.",
                        p.name()
                    );
                    ctx.fail(tally, p, Error::new(ErrorKind::Unsupported, msg));
                    continue;
                }
                None => {
                    let msg = format!("“{}” is a link to something that doesn't exist.", p.name());
                    ctx.fail(tally, p, Error::new(ErrorKind::NotFound, msg));
                    continue;
                }
            }
        }
        let keep = copy_deletes && e.shared.is_some();
        if keep {
            ctx.fail(tally, p, kept_shared(&e));
        }
        match e.kind {
            EntryKind::Dir => {
                if ctx.job.op == Op::Move && same && ctx.src().caps().server_rename {
                    match transfer::rename_dir(ctx, p, &target) {
                        Ok(true) => {
                            ctx.meter.add_total(1, 0);
                            ctx.meter.file_done();
                            continue;
                        }
                        Ok(false) => {} // destination exists: merge file by file below
                        Err(err) => {
                            ctx.fail(tally, p, err);
                            continue;
                        }
                    }
                }
                match prepare_dir(ctx, &target, true) {
                    Ok(Some(t)) => {
                        if !keep {
                            src_dirs.push(p.clone());
                        }
                        stack.push((p.clone(), t, keep));
                    }
                    Ok(None) => tally.keep(p),
                    Err(err) => ctx.fail(tally, p, err),
                }
            }
            EntryKind::Other => {}
            EntryKind::File | EntryKind::Symlink => send(ctx, tx, e, target, keep),

        }
    }

    while let Some((sd, td, kept)) = stack.pop() {
        ctx.ctl.checkpoint()?;
        let children = match ctx.retrying(|| ctx.src().list(&sd)) {
            Ok(c) => c,
            Err(err) => {
                ctx.fail(tally, &sd, err);
                continue;
            }
        };
        for c in children {
            let target = td.join(&c.name);
            let keep = kept || (copy_deletes && c.shared.is_some());
            if keep && !kept {
                ctx.fail(tally, &c.path, kept_shared(&c));
            }
            match c.kind {
                EntryKind::File => send(ctx, tx, c, target, keep),
                EntryKind::Symlink => {
                    if let Some(f) = resolve_link(ctx, c) {
                        send(ctx, tx, f, target, keep);
                    }
                }
                EntryKind::Dir => match prepare_dir(ctx, &target, false) {
                    Ok(Some(t)) => {
                        if !keep {
                            src_dirs.push(c.path.clone());
                        }
                        stack.push((c.path, t, keep));
                    }
                    Ok(None) => tally.keep(&c.path),
                    Err(err) => ctx.fail(tally, &c.path, err),
                },
                EntryKind::Other => {}
            }
        }
    }
    Ok(())
}

/// Makes sure a destination folder exists. Existing folders are merged into, except that the
/// Rename policy gives a selected (top-level) folder a fresh name. A file in the way fails,
/// unless the policy renames or skips. Returns the folder to fill, or None to skip it.
fn prepare_dir(ctx: &JobCtx, target: &RPath, top: bool) -> Result<Option<RPath>> {
    let dst = ctx.job.dst.as_deref().expect("transfer has a destination");
    let policy = ctx.job.conflict;
    let existing = match ctx.retrying(|| dst.stat(target)) {
        Ok(e) => Some(e),
        Err(e) if e.kind == ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let target = match existing {
        Some(e) if e.is_dir() && !(top && policy == ConflictPolicy::Rename) => return Ok(Some(target.clone())),
        Some(e) if !e.is_dir() && policy == ConflictPolicy::Skip => return Ok(None),
        Some(e) if !e.is_dir() && policy != ConflictPolicy::Rename => {
            let msg = format!("A file named “{}” is in the way of the folder.", target.name());
            return Err(Error::new(ErrorKind::AlreadyExists, msg));
        }
        Some(_) => transfer::free_name(ctx, dst, target)?,
        None => target.clone(),
    };
    match ctx.retrying(|| dst.mkdir(&target)) {
        Ok(()) => Ok(Some(target)),
        Err(e) if e.kind == ErrorKind::AlreadyExists => Ok(Some(target)),
        Err(e) => Err(e),
    }
}

/// Why a Move copied a shared item but left the original.
fn kept_shared(e: &Entry) -> Error {
    let msg = format!(
        "“{}” is shared with others, so Filewinch copied it and left the original in place. Moving it would delete it for everyone.",
        e.name
    );
    Error::new(ErrorKind::PermissionDenied, msg)
}

/// Why a selected shared item wasn't deleted (it can't be left, and the user didn't confirm
/// deleting its contents for everyone).
fn shared_refusal(e: &Entry) -> Error {
    let what = if e.is_dir() { "files inside a shared folder" } else { "a shared file" };
    let msg = format!(
        "“{}” is shared with others. Filewinch won't delete {what}. Remove it from the provider's website if you own it.",
        e.name
    );
    Error::new(ErrorKind::PermissionDenied, msg)
}

/// Why a shared item found inside a folder being deleted was left alone.
fn nested_shared(e: &Entry, parent: &RPath) -> Error {
    let msg = format!(
        "“{}” in “{}” is shared with others, so Filewinch left it (and the folders around it) in place. Select it on its own to remove it.",
        e.name,
        parent.name()
    );
    Error::new(ErrorKind::PermissionDenied, msg)
}

/// Takes a selected shared item out of this account only ([`Remote::leave_shared`]).
fn leave(ctx: &JobCtx, tally: &Tally, e: &Entry) {
    ctx.meter.add_total(1, 0);
    ctx.meter.set_current(&e.path);
    match ctx.retrying(|| ctx.src().leave_shared(&e.path)) {
        Err(err) if err.kind != ErrorKind::NotFound => ctx.fail(tally, &e.path, err),
        _ => {
            ctx.done.lock().unwrap_or_else(|e| e.into_inner()).insert(e.path.clone());
            ctx.meter.file_done();
        }
    }
    ctx.progress_tick();
}

/// Walks what a Delete removes. Shared items ([`Entry::shared`]) are never emptied: a selected
/// one is left ([`Remote::leave_shared`]) or refused, unless the job carries the user's
/// explicit `delete_shared_contents`; one found inside a selected ordinary folder is skipped
/// with an explanation (which also keeps its parent folders).
pub(crate) fn scan_delete(ctx: &JobCtx, tally: &Tally, tx: &Sender<Item>, dirs: &mut Vec<RPath>) -> Result<()> {
    // (folder, inside a shared item the user confirmed deleting for everyone)
    let mut stack: Vec<(RPath, bool)> = Vec::new();
    for p in &ctx.job.src_paths {
        ctx.ctl.checkpoint()?;
        if p.is_root() {
            ctx.fail(tally, p, Error::new(ErrorKind::InvalidPath, "The top folder can't be deleted."));
            continue;
        }
        let e = match ctx.retrying(|| ctx.src().stat(p)) {
            Ok(e) => e,
            Err(e) if e.kind == ErrorKind::NotFound => continue,
            Err(e) => {
                ctx.fail(tally, p, e);
                continue;
            }
        };
        let consented = e.shared.is_some() && ctx.job.delete_shared_contents;
        match e.shared {
            Some(s) if !consented && s.can_leave => leave(ctx, tally, &e),
            Some(_) if !consented => ctx.fail(tally, p, shared_refusal(&e)),
            _ if e.is_dir() => {
                dirs.push(p.clone());
                stack.push((p.clone(), consented));
            }
            _ => send(ctx, tx, e, RPath::root(), false),
        }
    }
    while let Some((d, consented)) = stack.pop() {
        ctx.ctl.checkpoint()?;
        let children = match ctx.retrying(|| ctx.src().list(&d)) {
            Ok(c) => c,
            Err(e) => {
                ctx.fail(tally, &d, e);
                continue;
            }
        };
        for c in children {
            if c.shared.is_some() && !consented {
                let err = nested_shared(&c, &d);
                ctx.fail(tally, &c.path, err);
            } else if c.is_dir() {
                dirs.push(c.path.clone());
                stack.push((c.path, consented));
            } else {
                send(ctx, tx, c, RPath::root(), false);
            }
        }
    }
    Ok(())
}
