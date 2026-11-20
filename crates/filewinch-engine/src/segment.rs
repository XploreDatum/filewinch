//! Segmented downloads: one large file over several connections at once.
//!
//! Applies only when the source can serve concurrent ranged reads ([`Caps::ranged_reads`]),
//! the destination can take positional writes ([`Remote::open_positional`], in practice the
//! local disk), the file is at least [`MIN_FILE`] and there are at least [`MIN_PART`] bytes per
//! connection. Everything else streams over one connection as before.
//!
//! The pieces go into a hidden temp file next to the destination, with a sidecar of finished
//! ranges ([`crate::sidecar`]), so pause, failure and app restarts continue where they were.
//! A connection that finishes early takes half of the biggest piece still running. Each
//! piece retries transient errors on its own. If the server refuses the extra connections
//! (too many connections, rate limits, an ignored range, auth or permission errors), the
//! engine keeps what arrived, finishes the rest over one connection, notes why in the job's
//! notes, and uses one connection for that server for the rest of the session. Once every
//! byte is in, the size (and a server checksum, when the listing had one) is checked and the
//! temp file is renamed into place, applying the job's conflict policy at that moment.

use crate::hash;
use crate::runner::{Item, JobCtx};
use crate::sidecar::{self, Names, Ranges};
use crate::transfer::{Plan, resolve};
use filewinch_core::{Entry, Error, ErrorKind, PositionalSink, RPath, Remote, Result};
use std::io::{self, Read};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[cfg(doc)]
use filewinch_core::Caps;

/// Smallest file worth splitting.
pub(crate) const MIN_FILE: u64 = 16 * 1024 * 1024;
/// Smallest piece per connection (and the most a chunk may be, so a split never lands inside
/// bytes a connection is writing).
pub(crate) const MIN_PART: u64 = 4 * 1024 * 1024;
/// How often the sidecar is rewritten while bytes flow.
const SAVE_EVERY: Duration = Duration::from_secs(1);

pub(crate) enum Landed {
    At(RPath),
    /// The conflict policy said skip at the final rename; the download was discarded.
    Skipped,
    /// The destination can't take positional writes; stream it instead.
    NotSegmented,
}

/// Connections to use for `src` landing at `target`, or None to stream it as before. A file
/// with a partial segmented download keeps using its temp file (with one connection when
/// segmenting is now off or this server was dropped to one).
pub(crate) fn connections(ctx: &JobCtx, src: &Entry, target: &RPath, offset: u64) -> Option<usize> {
    let dst = ctx.job.dst.as_deref()?;
    let size = src.size?;
    if offset > 0 || ctx.same_remote() || size < MIN_FILE || !ctx.src().caps().ranged_reads || !dst.caps().server_rename {
        return None;
    }
    let n = ctx.cfg.segments_per_file.min(usize::try_from(size / MIN_PART).unwrap_or(usize::MAX));
    if n >= 2 && !ctx.inner.is_single(&ctx.job.src) {
        return Some(n);
    }
    dst.stat(&Names::for_target(target).ranges).is_ok().then_some(1)
}

#[derive(Clone, Copy, Debug)]
struct Part {
    start: u64,
    pos: u64,
    end: u64,
    claimed: bool,
}

enum Stop {
    /// Extra connections aren't welcome: finish over one.
    Fallback(Error),
    Failed(Error),
}

struct State {
    /// Finished before the current parts were planned.
    base: Ranges,
    parts: Vec<Part>,
    stop: Option<Stop>,
    /// Some ranged read was accepted, so a later refusal is about the extra connections.
    connected: bool,
    /// Bytes added to the job's meter for this file.
    counted: u64,
    last_save: Instant,
}

impl State {
    fn done(&self) -> Ranges {
        let mut d = self.base.clone();
        for p in &self.parts {
            d.add(p.start, p.pos);
        }
        d
    }
}

struct Seg<'a> {
    ctx: &'a JobCtx,
    dst: &'a dyn Remote,
    src: &'a Entry,
    names: Names,
    size: u64,
    sink: Box<dyn PositionalSink>,
    st: Mutex<State>,
    saving: Mutex<()>,
}

/// Downloads `item` into `target` over up to `n` connections. `overwrite`: the user already
/// agreed to replace what is at `target`.
pub(crate) fn download(ctx: &JobCtx, item: &Item, target: &RPath, n: usize, overwrite: bool) -> Result<Landed> {
    let dst = ctx.job.dst.as_deref().expect("transfer has a destination");
    let src = &item.src;
    let size = src.size.expect("only sized files are segmented");
    let names = Names::for_target(target);
    let done = match sidecar::load(dst, &names, src, size) {
        Ok(Some(d)) if dst.stat(&names.temp).is_ok_and(|e| e.size == Some(size)) => d,
        _ => Ranges::default(),
    };
    let sink = match ctx.retrying(|| dst.open_positional(&names.temp, size)) {
        Ok(s) => s,
        Err(e) if e.kind == ErrorKind::Unsupported => return Ok(Landed::NotSegmented),
        Err(e) => return Err(e),
    };
    ctx.track_partial(&names.temp, true);
    ctx.add_bytes(done.total());
    let seg = Seg {
        ctx,
        dst,
        src,
        size,
        sink,
        st: Mutex::new(State {
            counted: done.total(),
            base: done,
            parts: Vec::new(),
            stop: None,
            connected: false,
            last_save: Instant::now(),
        }),
        saving: Mutex::new(()),
        names,
    };
    if let Err(e) = seg.fetch(n) {
        return Err(seg.give_up(e));
    }
    let Seg { sink, names, st, .. } = seg;
    let counted = st.into_inner().unwrap_or_else(|e| e.into_inner()).counted;
    let undo = |e: Error| {
        ctx.sub_bytes(counted);
        e
    };
    sink.finish().map_err(undo)?;
    verify(ctx, dst, &names, src, size).map_err(undo)?;
    let landed = land(ctx, item, &names, target, overwrite).map_err(undo)?;
    let _ = dst.remove_file(&names.ranges);
    ctx.track_partial(&names.temp, false);
    if landed.is_none() {
        // Skipped at the rename: the worker counts a skipped file's bytes itself.
        ctx.sub_bytes(counted);
    }
    Ok(landed.map_or(Landed::Skipped, Landed::At))
}

impl Seg<'_> {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn fetch(&self, n: usize) -> Result<()> {
        if n > 1 {
            {
                let mut st = self.lock();
                st.parts = split(&st.base.gaps(self.size), n);
            }
            let live = crate::runner::current_item();
            std::thread::scope(|s| {
                for _ in 1..n {
                    let live = live.clone();
                    s.spawn(move || {
                        // This file's counters and skip flag, as on the worker's thread.
                        crate::runner::set_current_item(live);
                        self.worker(true);
                    });
                }
                self.worker(true);
            });
            let mut st = self.lock();
            match st.stop.take() {
                None => {}
                Some(Stop::Failed(e)) => return Err(e),
                Some(Stop::Fallback(e)) => {
                    let connected = st.connected;
                    drop(st);
                    self.fall_back(&e, connected);
                }
            }
        }
        // One connection: whatever is still missing (all of it when n is 1).
        {
            let mut st = self.lock();
            st.base = st.done();
            st.parts = st.base.gaps(self.size).into_iter().map(|(a, b)| Part { start: a, pos: a, end: b, claimed: false }).collect();
        }
        self.worker(false);
        let stop = self.lock().stop.take();
        match stop {
            None => {}
            Some(Stop::Failed(e)) if e.kind == ErrorKind::Unsupported => {
                // The server ignores ranges even on one connection: read on from its stream.
                let mut st = self.lock();
                st.base = st.done();
                st.parts.clear();
                drop(st);
                self.stream_rest()?;
            }
            Some(Stop::Failed(e) | Stop::Fallback(e)) => return Err(e),
        }
        let missing = self.lock().done().gaps(self.size);
        if !missing.is_empty() {
            let e = Error::new(ErrorKind::Network, format!("Part of “{}” didn't arrive.", self.src.name));
            return Err(e.with_detail(format!("missing ranges {missing:?}")));
        }
        Ok(())
    }

    /// Records why, and drops this server to one connection when the refusal is about extra
    /// connections or ranges (an auth or network error counts only once some connection
    /// worked: otherwise it's about the file or the server, and the single connection will
    /// say so).
    fn fall_back(&self, e: &Error, connected: bool) {
        let single = connected || matches!(e.kind, ErrorKind::RateLimited | ErrorKind::Unsupported);
        if single {
            self.ctx.inner.set_single(&self.ctx.job.src);
        }
        let why = match e.kind {
            ErrorKind::Unsupported => "the server ignored a byte range",
            ErrorKind::RateLimited => "the server limits connections or requests",
            ErrorKind::Auth | ErrorKind::PermissionDenied => "the server refused the extra connections",
            _ => "the extra connections kept failing",
        };
        let rest = if single { " and this server will get one connection for the rest of the session" } else { "" };
        self.ctx.note(format!("“{}”: {why} ({}), so the rest came over one connection{rest}.", self.src.name, e));
    }

    /// Takes the next piece to download: an unclaimed one, else the back half of the biggest
    /// piece another connection is still on.
    fn claim(&self) -> Option<usize> {
        let mut st = self.lock();
        if st.stop.is_some() {
            return None;
        }
        if let Some(i) = st.parts.iter().position(|p| !p.claimed && p.pos < p.end) {
            st.parts[i].claimed = true;
            return Some(i);
        }
        let (i, left) = st.parts.iter().enumerate().map(|(i, p)| (i, p.end.saturating_sub(p.pos))).max_by_key(|x| x.1)?;
        if left < 2 * MIN_PART {
            return None;
        }
        let mid = st.parts[i].pos + left / 2;
        let end = std::mem::replace(&mut st.parts[i].end, mid);
        st.parts.push(Part { start: mid, pos: mid, end, claimed: true });
        Some(st.parts.len() - 1)
    }

    fn worker(&self, multi: bool) {
        while let Some(i) = self.claim() {
            if let Err(e) = self.run_part(i, multi) {
                self.halt(e, multi);
                return;
            }
            self.save(true);
        }
    }

    /// Stops every connection: to fall back to one, or to fail.
    fn halt(&self, e: Error, multi: bool) {
        let fallback = multi
            && matches!(
                e.kind,
                ErrorKind::Unsupported
                    | ErrorKind::RateLimited
                    | ErrorKind::Auth
                    | ErrorKind::PermissionDenied
                    | ErrorKind::Network
                    | ErrorKind::Closed
                    | ErrorKind::Other
            );
        let mut st = self.lock();
        match (&st.stop, fallback) {
            (Some(Stop::Failed(_)), _) | (Some(Stop::Fallback(_)), true) => {}
            _ => st.stop = Some(if fallback { Stop::Fallback(e) } else { Stop::Failed(e) }),
        }
    }

    fn stopped(&self) -> bool {
        self.lock().stop.is_some()
    }

    /// Where piece `i` stands, or None once it's complete.
    fn span(&self, i: usize) -> Option<(u64, u64)> {
        let p = self.lock().parts[i];
        (p.pos < p.end).then_some((p.pos, p.end))
    }

    /// Downloads piece `i`, retrying transient errors with backoff. With several connections,
    /// a refusal ends it at once so every piece can fall back to one connection.
    fn run_part(&self, i: usize, multi: bool) -> Result<()> {
        let mut attempt = 0;
        loop {
            self.ctx.checkpoint()?;
            if self.stopped() {
                return Ok(());
            }
            let Some((pos, end)) = self.span(i) else { return Ok(()) };
            let err = match self.pump(i, pos, end) {
                Ok(()) => continue,
                Err(e) => e,
            };
            let refused = matches!(
                err.kind,
                ErrorKind::Unsupported | ErrorKind::RateLimited | ErrorKind::Auth | ErrorKind::PermissionDenied
            );
            if err.kind == ErrorKind::Cancelled || (multi && refused) {
                return Err(err);
            }
            if err.kind.is_transient() && attempt < self.ctx.cfg.retries {
                attempt += 1;
                self.ctx.backoff(attempt, &err)?;
                continue;
            }
            return Err(err);
        }
    }

    /// One ranged read of piece `i` from `pos`, until the piece is done, shrinks under it, or
    /// something stops.
    fn pump(&self, i: usize, pos: u64, end: u64) -> Result<()> {
        let ctx = self.ctx;
        let mut reader = ctx.src().read_range(&self.src.path, pos, end - pos)?;
        self.lock().connected = true;
        let mut buf = vec![0u8; ctx.cfg.chunk.clamp(4096, MIN_PART as usize)];
        loop {
            ctx.checkpoint()?;
            if self.stopped() {
                return Ok(());
            }
            let Some((pos, end)) = self.span(i) else { return Ok(()) };
            let want = (end - pos).min(buf.len() as u64) as usize;
            let n = read_some(&mut *reader, &mut buf[..want])?;
            if n == 0 {
                return Err(Error::new(ErrorKind::Network, format!("The server stopped sending “{}” early.", self.src.name)));
            }
            ctx.inner.bucket.take(n, &ctx.ctl)?;
            // Another connection may have taken the back of this piece meanwhile.
            let n = {
                let p = self.lock().parts[i];
                n.min(p.end.saturating_sub(pos) as usize)
            };
            if n == 0 {
                return Ok(());
            }
            self.sink.write_at(pos, &buf[..n])?;
            {
                let mut st = self.lock();
                st.parts[i].pos = pos + n as u64;
                st.counted += n as u64;
            }
            ctx.add_bytes(n as u64);
            ctx.progress_tick();
            self.save(false);
        }
    }

    /// For a server that ignores ranges: reads the file as one stream (from the first missing
    /// byte when it can resume, else from the start) and fills in only what's missing.
    fn stream_rest(&self) -> Result<()> {
        let ctx = self.ctx;
        let mut resumable = ctx.src().caps().resume_read;
        let mut attempt = 0;
        loop {
            let Some(&(first, _)) = self.lock().done().gaps(self.size).first() else { return Ok(()) };
            let from = if resumable { first } else { 0 };
            let err = match self.stream_from(from) {
                Ok(()) => continue,
                Err(e) => e,
            };
            if err.kind == ErrorKind::Unsupported && resumable {
                resumable = false;
                continue;
            }
            if err.kind.is_transient() && attempt < ctx.cfg.retries {
                attempt += 1;
                ctx.backoff(attempt, &err)?;
                continue;
            }
            return Err(err);
        }
    }

    fn stream_from(&self, from: u64) -> Result<()> {
        let ctx = self.ctx;
        let mut reader = ctx.src().read(&self.src.path, from)?;
        let mut buf = vec![0u8; ctx.cfg.chunk.max(4096)];
        let mut at = from;
        while at < self.size {
            ctx.checkpoint()?;
            let want = (self.size - at).min(buf.len() as u64) as usize;
            let n = read_some(&mut *reader, &mut buf[..want])?;
            if n == 0 {
                return Err(Error::new(ErrorKind::Network, format!("The server stopped sending “{}” early.", self.src.name)));
            }
            ctx.inner.bucket.take(n, &ctx.ctl)?;
            let (a, b) = (at, at + n as u64);
            let missing: Vec<(u64, u64)> = {
                let st = self.lock();
                st.base.gaps(self.size).into_iter().map(|(x, y)| (x.max(a), y.min(b))).filter(|(x, y)| x < y).collect()
            };
            for (x, y) in missing {
                self.sink.write_at(x, &buf[(x - a) as usize..(y - a) as usize])?;
                let mut st = self.lock();
                st.base.add(x, y);
                st.counted += y - x;
                drop(st);
                ctx.add_bytes(y - x);
            }
            at = b;
            ctx.progress_tick();
            self.save(false);
        }
        Ok(())
    }

    /// Records the finished ranges (at most once a second unless `now`), after making the
    /// bytes they cover durable.
    fn save(&self, now: bool) {
        if !now && self.lock().last_save.elapsed() < SAVE_EVERY {
            return;
        }
        let Ok(_one) = self.saving.try_lock() else { return };
        let done = {
            let mut st = self.lock();
            st.last_save = Instant::now();
            st.done()
        };
        if self.sink.sync().is_ok() {
            let _ = sidecar::save(self.dst, &self.names, self.src, self.size, &done);
        }
    }

    /// Ends a download that didn't complete. A cancel removes the temp file and sidecar
    /// (unless the engine is shutting down: then, like a pause or a failure, they stay so the
    /// download continues later).
    fn give_up(self, e: Error) -> Error {
        let counted = self.lock().counted;
        self.ctx.sub_bytes(counted);
        if e.kind == ErrorKind::Cancelled && !self.ctx.ctl.keeps_partials() {
            let Seg { sink, names, dst, ctx, .. } = self;
            drop(sink);
            sidecar::remove(dst, &names);
            ctx.track_partial(&names.temp, false);
        } else {
            self.save(true);
        }
        e
    }
}

fn read_some(r: &mut dyn Read, buf: &mut [u8]) -> Result<usize> {
    loop {
        match r.read(buf) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::from(e)),
        }
    }
}

/// Splits the missing ranges into about `n` pieces of similar size (none under
/// [`MIN_PART`] unless a range itself is smaller).
fn split(gaps: &[(u64, u64)], n: usize) -> Vec<Part> {
    let total: u64 = gaps.iter().map(|(a, b)| b - a).sum();
    let target = total.div_ceil(n.max(1) as u64).max(MIN_PART);
    let mut parts = Vec::new();
    for &(a, b) in gaps {
        let k = (b - a).div_ceil(target).max(1);
        let len = (b - a) / k;
        for j in 0..k {
            let start = a + j * len;
            let end = if j + 1 == k { b } else { start + len };
            parts.push(Part { start, pos: start, end, claimed: false });
        }
    }
    parts
}

/// Checks the finished temp file: its size, and its content against a server checksum when
/// the listing had one this engine can compute. A mismatch deletes the download.
fn verify(ctx: &JobCtx, dst: &dyn Remote, names: &Names, src: &Entry, size: u64) -> Result<()> {
    let got = ctx.retrying(|| dst.stat(&names.temp))?.size;
    if got != Some(size) {
        sidecar::remove(dst, names);
        ctx.track_partial(&names.temp, false);
        let msg = format!("“{}” didn't download completely, so it was discarded. Try again.", src.name);
        return Err(Error::new(ErrorKind::Other, msg).with_detail(format!("expected {size} bytes, got {got:?}")));
    }
    let Some(want) = src.checksums.iter().min_by_key(|c| hash::cost(c.algo)) else { return Ok(()) };
    let have = hash::hash_file(dst, &names.temp, want.algo, &mut |_| ctx.checkpoint())?;
    if have.hex != want.hex {
        sidecar::remove(dst, names);
        ctx.track_partial(&names.temp, false);
        let msg = format!("“{}” arrived damaged (its checksum doesn't match the server's), so it was discarded. Try again.", src.name);
        return Err(Error::new(ErrorKind::Other, msg).with_detail(format!("{:?} expected {}, got {}", want.algo, want.hex, have.hex)));
    }
    Ok(())
}

/// Renames the finished temp file into place, applying the conflict policy to whatever is at
/// the target now. Returns where it landed, or None when the policy skipped it (the temp
/// file is then deleted).
fn land(ctx: &JobCtx, item: &Item, names: &Names, target: &RPath, overwrite: bool) -> Result<Option<RPath>> {
    let dst = ctx.job.dst.as_deref().expect("transfer has a destination");
    let (mut target, mut overwrite) = (target.clone(), overwrite);
    for _ in 0..8 {
        let existing = match ctx.retrying(|| dst.stat(&target)) {
            Ok(e) => Some(e),
            Err(e) if e.kind == ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        match existing {
            None => match ctx.retrying(|| dst.rename(&names.temp, &target)) {
                Ok(()) => return Ok(Some(target)),
                Err(e) if e.kind == ErrorKind::AlreadyExists => continue, // appeared just now
                Err(e) => return Err(e),
            },
            Some(e) if e.is_dir() => {
                let msg = format!("A folder named “{}” is in the way.", e.name);
                return Err(Error::new(ErrorKind::AlreadyExists, msg));
            }
            Some(_) if overwrite => {
                replace(ctx, dst, names, &target)?;
                return Ok(Some(target));
            }
            Some(e) => match resolve(ctx, item, &e)? {
                Plan::Skip => {
                    sidecar::remove(dst, names);
                    return Ok(None);
                }
                Plan::Write { target: t, .. } => {
                    overwrite = t == target;
                    target = t;
                }
            },
        }
    }
    Err(Error::new(ErrorKind::AlreadyExists, format!("Couldn't put “{}” in place: its name keeps being taken.", item.src.name)))
}

/// Swaps the temp file in for an existing file; the original is put back if that fails.
fn replace(ctx: &JobCtx, dst: &dyn Remote, names: &Names, target: &RPath) -> Result<()> {
    let _ = dst.remove_file(&names.old);
    ctx.retrying(|| dst.rename(target, &names.old))?;
    if let Err(e) = ctx.retrying(|| dst.rename(&names.temp, target)) {
        let _ = dst.rename(&names.old, target);
        return Err(e);
    }
    let _ = dst.remove_file(&names.old);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(p: &[Part]) -> Vec<(u64, u64)> {
        p.iter().map(|p| (p.start, p.end)).collect()
    }

    #[test]
    fn split_is_even_with_an_uneven_tail() {
        const M: u64 = MIN_PART;
        assert_eq!(spans(&split(&[(0, 12 * M)], 3)), vec![(0, 4 * M), (4 * M, 8 * M), (8 * M, 12 * M)]);
        let p = split(&[(0, 10 * M + 7)], 3);
        assert_eq!(p.len(), 3);
        assert_eq!(p.last().unwrap().end, 10 * M + 7);
        assert!(p.windows(2).all(|w| w[0].end == w[1].start));
        // Resume: two gaps share the connections by size; a small one stays whole.
        assert_eq!(
            spans(&split(&[(0, M), (5 * M, 13 * M)], 4)),
            vec![(0, M), (5 * M, 7 * M), (7 * M, 9 * M), (9 * M, 11 * M), (11 * M, 13 * M)]
        );
    }
}
