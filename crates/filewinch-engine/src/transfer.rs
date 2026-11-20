//! One file: conflict handling, streaming with retries and resume, metadata, and the source
//! delete for a Move.

use crate::runner::{Item, JobCtx, Outcome};
use crate::segment::{self, Landed};
use crate::{ConflictAsk, ConflictChoice, ConflictPolicy, Event, Op};
use filewinch_core::{Entry, EntryKind, Error, ErrorKind, RPath, Remote, Result};
use std::io::{self, Read, Write};
use std::time::Duration;

/// An I/O error from a backend's reader or writer. Core's `From` unwraps a wrapped [`Error`],
/// so the backend's kind, detail and `retry_after` survive.
fn io_error(e: io::Error) -> Error {
    Error::from(e)
}

pub(crate) enum Plan {
    Write { target: RPath, offset: u64 },
    Skip,
}

pub(crate) fn file(ctx: &JobCtx, item: &Item) -> Result<Outcome> {
    let dst = ctx.job.dst.as_deref().expect("transfer has a destination");
    let src_path = &item.src.path;
    ctx.meter.set_current(src_path);
    ctx.progress_tick();

    let existing = match ctx.retrying(|| dst.stat(&item.dst)) {
        Ok(e) => Some(e),
        Err(e) if e.kind == ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let replacing = existing.is_some();
    let (mut target, offset) = match existing {
        None => (item.dst.clone(), 0),
        Some(d) => match resolve(ctx, item, &d)? {
            Plan::Skip => return Ok(Outcome::Skipped),
            Plan::Write { target, offset } => {
                if target == item.dst && d.is_dir() {
                    let msg = format!("A folder named “{}” is in the way.", d.name);
                    return Err(Error::new(ErrorKind::AlreadyExists, msg));
                }
                (target, offset)
            }
        },
    };
    let size = item.src.size.unwrap_or(0);
    let same = ctx.same_remote();
    let scaps = ctx.src().caps();

    if ctx.job.op == Op::Move && same && scaps.server_rename {
        if target == item.dst && existing_file(dst, &target) {
            ctx.retrying(|| dst.remove_file(&target))?;
        }
        ctx.retrying(|| dst.rename(src_path, &target))?;
        ctx.add_bytes(size);
        return Ok(Outcome::Done);
    }

    if same && scaps.server_copy {
        ctx.retrying(|| dst.copy(src_path, &target))?;
        ctx.add_bytes(size);
    } else {
        let landed = match segment::connections(ctx, &item.src, &target, offset) {
            Some(n) => segment::download(ctx, item, &target, n, replacing && target == item.dst)?,
            None => Landed::NotSegmented,
        };
        match landed {
            Landed::At(t) => target = t,
            Landed::Skipped => return Ok(Outcome::Skipped),
            Landed::NotSegmented => {
                if let Err(e) = stream_with_retries(ctx, &item.src, &target, offset) {
                    if ctx.item_skipped() && !replacing {
                        // Skipped by the person: a new file's partial bytes go too. (An
                        // overwrite was writing a hidden sibling, already gone.)
                        let _ = dst.remove_file(&target);
                    }
                    return Err(e);
                }
            }
        }
    }
    set_metadata(ctx, &item.src, &target);

    if ctx.job.op == Op::Move && !item.keep_src {
        ctx.retrying(|| ctx.src().remove_file(src_path)).map_err(|e| {
            Error::new(e.kind, format!("“{}” was copied, but the original couldn't be removed: {}", src_path.name(), e.message))
        })?;
    }
    Ok(Outcome::Done)
}

fn existing_file(r: &dyn Remote, p: &RPath) -> bool {
    r.stat(p).is_ok_and(|e| !e.is_dir())
}

fn can_resume(ctx: &JobCtx, src: &Entry, dst: &Entry) -> bool {
    let d = ctx.job.dst.as_deref().expect("transfer has a destination");
    ctx.src().caps().resume_read
        && d.caps().resume_write
        && dst.kind == EntryKind::File
        && matches!((dst.size, src.size), (Some(ds), Some(ss)) if ds > 0 && ds < ss)
}

pub(crate) fn resolve(ctx: &JobCtx, item: &Item, d: &Entry) -> Result<Plan> {
    let resumable = can_resume(ctx, &item.src, d);
    let choice = match ctx.job.conflict {
        ConflictPolicy::Overwrite => ConflictChoice::Overwrite,
        ConflictPolicy::Skip => ConflictChoice::Skip,
        ConflictPolicy::Rename => ConflictChoice::Rename,
        ConflictPolicy::ResumeOrOverwrite => ConflictChoice::Resume,
        ConflictPolicy::OverwriteIfNewer => {
            // One second of slack: many servers store whole seconds.
            let newer = match (item.src.modified, d.modified) {
                (Some(s), Some(t)) => s > t + Duration::from_secs(1),
                _ => true,
            };
            if newer { ConflictChoice::Overwrite } else { ConflictChoice::Skip }
        }
        ConflictPolicy::Ask => ctx.ctl.ask(|| {
            ctx.inner.refresh(ctx.id);
            let ask = ConflictAsk { src: item.src.clone(), dst: d.clone(), can_resume: resumable };
            ctx.emit(Event::Conflict { job: ctx.id, ask });
        })?,
    };
    ctx.inner.refresh(ctx.id);
    let dst = ctx.job.dst.as_deref().expect("transfer has a destination");
    Ok(match choice {
        ConflictChoice::Skip => Plan::Skip,
        ConflictChoice::Overwrite => Plan::Write { target: item.dst.clone(), offset: 0 },
        ConflictChoice::Resume if resumable => {
            Plan::Write { target: item.dst.clone(), offset: d.size.unwrap_or(0) }
        }
        ConflictChoice::Resume => Plan::Write { target: item.dst.clone(), offset: 0 },
        ConflictChoice::Rename => Plan::Write { target: free_name(ctx, dst, &item.dst)?, offset: 0 },
    })
}

/// "name.txt" → "name 2.txt"; keeps double extensions like ".tar.gz" together.
pub(crate) fn numbered(name: &str, n: u32) -> String {
    let lower = name.to_ascii_lowercase();
    let double = [".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst"].iter().find(|x| lower.ends_with(*x) && lower.len() > x.len());
    let split = match double {
        Some(x) => Some(name.len() - x.len()),
        None => name.rfind('.').filter(|i| *i > 0),
    };
    match split {
        Some(i) => format!("{} {n}{}", &name[..i], &name[i..]),
        None => format!("{name} {n}"),
    }
}

/// The first "name N.ext" next to `target` that doesn't exist yet.
pub(crate) fn free_name(ctx: &JobCtx, r: &dyn Remote, target: &RPath) -> Result<RPath> {
    let parent = target.parent().unwrap_or_else(RPath::root);
    for n in 2..10_000 {
        let candidate = parent.join(&numbered(target.name(), n));
        match ctx.retrying(|| r.stat(&candidate)) {
            Err(e) if e.kind == ErrorKind::NotFound => return Ok(candidate),
            Err(e) => return Err(e),
            Ok(_) => {}
        }
    }
    Err(Error::new(ErrorKind::AlreadyExists, format!("Couldn't find a free name for “{}”.", target.name())))
}

/// Server-side rename of a whole folder for a same-remote Move. Returns false (without doing
/// anything) when the destination exists, so the caller merges file by file.
pub(crate) fn rename_dir(ctx: &JobCtx, from: &RPath, to: &RPath) -> Result<bool> {
    match ctx.retrying(|| ctx.src().stat(to)) {
        Ok(_) => return Ok(false),
        Err(e) if e.kind == ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    ctx.retrying(|| ctx.src().rename(from, to))?;
    Ok(true)
}

/// Copies bytes, retrying transient errors. Each retry resumes from what the destination
/// already holds when both sides support it, otherwise restarts the file.
fn stream_with_retries(ctx: &JobCtx, src: &Entry, target: &RPath, start: u64) -> Result<()> {
    let dst = ctx.job.dst.as_deref().expect("transfer has a destination");
    let mut resumable = ctx.src().caps().resume_read && dst.caps().resume_write;
    let mut offset = start;
    ctx.add_bytes(offset);
    let mut attempt = 0;
    loop {
        let mut counted = 0u64;
        let err = match stream(ctx, src, target, offset, &mut counted) {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        if err.kind == ErrorKind::Unsupported && offset > 0 && resumable {
            // The server can't continue this particular file after all (some servers refuse
            // writes at an offset): start it over, once, without resuming.
            resumable = false;
            ctx.sub_bytes(offset + counted);
            offset = 0;
            continue;
        }
        if !err.kind.is_transient() || attempt >= ctx.cfg.retries {
            ctx.sub_bytes(offset + counted);
            return Err(err);
        }
        attempt += 1;
        if let Err(e) = ctx.backoff(attempt, &err) {
            ctx.sub_bytes(offset + counted);
            return Err(e);
        }
        let reached = offset + counted;
        let next = if resumable {
            // Trust what the server actually has, but never more than we sent.
            match dst.stat(target) {
                Ok(e) => e.size.filter(|s| *s <= reached).unwrap_or(0),
                Err(_) => 0,
            }
        } else {
            0
        };
        ctx.sub_bytes(reached - next);
        offset = next;
    }
}

fn stream(ctx: &JobCtx, src: &Entry, target: &RPath, offset: u64, counted: &mut u64) -> Result<()> {
    let dst = ctx.job.dst.as_deref().expect("transfer has a destination");
    ctx.checkpoint()?;
    let mut reader = ctx.src().read(&src.path, offset)?;
    let mut sink = dst.write(target, offset, src.size)?;
    let mut buf = vec![0u8; ctx.cfg.chunk.max(4096)];
    loop {
        // A pause or cancel takes effect here; cancel drops `sink` unfinished, which aborts it.
        ctx.checkpoint()?;
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(io_error(e)),
        };
        ctx.inner.bucket.take(n, &ctx.ctl)?;
        sink.write_all(&buf[..n]).map_err(io_error)?;
        *counted += n as u64;
        ctx.add_bytes(n as u64);
        ctx.progress_tick();
    }
    ctx.checkpoint()?;
    sink.finish()
}

/// Copies mtime and permission bits where the destination supports them. Best effort: a file
/// that arrived intact isn't failed over metadata.
fn set_metadata(ctx: &JobCtx, src: &Entry, target: &RPath) {
    let dst = ctx.job.dst.as_deref().expect("transfer has a destination");
    let caps = dst.caps();
    if caps.set_mtime
        && let Some(m) = src.modified {
            let _ = ctx.retrying(|| dst.set_mtime(target, m));
        }
    if caps.permissions && ctx.src().caps().permissions
        && let Some(mode) = src.mode {
            let _ = ctx.retrying(|| dst.set_mode(target, mode & 0o7777));
        }
}
