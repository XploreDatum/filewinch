//! Directory listing parsers: MLSD facts (RFC 3659), Unix `ls -l` style LIST, and the
//! Windows/IIS (DOS) LIST format. Times without a zone are taken as UTC.

use filewinch_core::{Entry, EntryKind, RPath};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub(crate) fn to_time(y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> Option<SystemTime> {
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    let secs = days_from_civil(y, mo, d) * 86_400 + (h * 3600 + mi * 60 + s) as i64;
    if secs >= 0 {
        Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs((-secs) as u64))
    }
}

/// `YYYYMMDDHHMMSS[.sss]` (MDTM / MLSD `modify`).
pub fn parse_timeval(s: &str) -> Option<SystemTime> {
    let (main, frac) = s.split_once('.').unwrap_or((s, ""));
    if main.len() != 14 || !main.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n = |r: std::ops::Range<usize>| main[r].parse::<u32>().ok();
    let t = to_time(main[..4].parse().ok()?, n(4..6)?, n(6..8)?, n(8..10)?, n(10..12)?, n(12..14)?)?;
    let ms: u32 = format!("{frac:0<3}")[..3].parse().unwrap_or(0);
    Some(t + Duration::from_millis(ms as u64))
}

/// The inverse of [`parse_timeval`], whole seconds (for MFMT).
pub fn format_timeval(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let r = secs.rem_euclid(86_400);
    format!("{y:04}{m:02}{d:02}{:02}{:02}{:02}", r / 3600, r / 60 % 60, r % 60)
}

fn entry(dir: &RPath, name: &str, kind: EntryKind) -> Entry {
    Entry {
        name: name.to_string(),
        path: dir.join(name),
        kind,
        size: None,
        modified: None,
        mode: None,
        owner: None,
        group: None,
        link_target: None,
        extra: vec![], checksums: Vec::new(), shared: None,
    }
}

/// One MLSD/MLST line: `fact=value;fact=value; name`. Returns None for `.`/`..` (cdir/pdir).
pub fn parse_mlsd_line(dir: &RPath, line: &str) -> Option<Entry> {
    let line = line.trim_end_matches(['\r', '\n']);
    let (facts, name) = line.split_once(' ')?;
    let name = name.rsplit('/').next().unwrap_or(name);
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    let mut e = entry(dir, name, EntryKind::File);
    for fact in facts.split(';').filter(|f| !f.is_empty()) {
        let (k, v) = fact.split_once('=').unwrap_or((fact, ""));
        match k.to_ascii_lowercase().as_str() {
            "type" => {
                let v = v.to_ascii_lowercase();
                e.kind = match v.as_str() {
                    "file" => EntryKind::File,
                    "dir" => EntryKind::Dir,
                    "cdir" | "pdir" => return None,
                    _ if v.starts_with("os.unix=slink") || v.starts_with("os.unix=symlink") => EntryKind::Symlink,
                    _ => EntryKind::Other,
                };
                if let Some(t) = v.split_once(':').map(|x| x.1) {
                    e.link_target = Some(t.to_string());
                }
            }
            "size" | "sizd" => e.size = v.parse().ok(),
            "modify" => e.modified = parse_timeval(v),
            "unix.mode" => e.mode = u32::from_str_radix(v.trim_start_matches("0o"), 8).ok(),
            "unix.owner" | "unix.ownername" => e.owner = Some(v.to_string()),
            "unix.group" | "unix.groupname" => e.group = Some(v.to_string()),
            "perm" => e.extra.push(("perm".into(), v.to_string())),
            _ => {}
        }
    }
    if e.kind == EntryKind::Dir {
        e.size = None;
    }
    Some(e)
}

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

fn month(s: &str) -> Option<u32> {
    let l = s.to_ascii_lowercase();
    MONTHS.iter().position(|m| l.starts_with(m) && s.len() <= 4).map(|i| i as u32 + 1)
}

/// Splits on runs of spaces, keeping each token's byte offset so names with spaces survive.
fn tokens(line: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in line.char_indices() {
        match (c == ' ' || c == '\t', start) {
            (true, Some(s)) => {
                out.push((s, &line[s..i]));
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((s, &line[s..]));
    }
    out
}

fn parse_mode(perm: &str) -> Option<u32> {
    let b = perm.as_bytes();
    if b.len() < 10 {
        return None;
    }
    let mut mode = 0u32;
    for (i, bit) in [0o400, 0o200, 0o100, 0o40, 0o20, 0o10, 0o4, 0o2, 0o1].iter().enumerate() {
        let c = b[i + 1];
        if c != b'-' && c != b'S' && c != b'T' {
            mode |= bit;
        }
        match (i, c) {
            (2, b's' | b'S') => mode |= 0o4000,
            (5, b's' | b'S') => mode |= 0o2000,
            (8, b't' | b'T') => mode |= 0o1000,
            _ => {}
        }
    }
    Some(mode)
}

/// One LIST line in Unix or DOS format. `now` resolves year-less Unix dates (a date more than a
/// day in the future belongs to last year). Returns None for totals, `.`/`..` and junk.
pub fn parse_list_line(dir: &RPath, line: &str, now: SystemTime) -> Option<Entry> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.is_empty() || line.starts_with("total ") {
        return None;
    }
    let first = line.as_bytes()[0];
    if first.is_ascii_digit() {
        return parse_dos(dir, line);
    }
    if !matches!(first, b'-' | b'd' | b'l' | b'b' | b'c' | b'p' | b's') {
        return None;
    }
    parse_unix(dir, line, now)
}

fn parse_unix(dir: &RPath, line: &str, now: SystemTime) -> Option<Entry> {
    let t = tokens(line);
    // Find "Mon DD (HH:MM|YYYY)" followed by a name.
    let i = (3..t.len().saturating_sub(3)).find(|&i| {
        month(t[i].1).is_some() && t[i + 1].1.parse::<u32>().is_ok_and(|d| (1..=31).contains(&d)) && (t[i + 2].1.contains(':') || t[i + 2].1.len() == 4)
    })?;
    let size: Option<u64> = t[i - 1].1.parse().ok();
    let name_start = t[i + 3].0;
    let mut name = &line[name_start..];
    let perm = t[0].1;
    let kind = match perm.as_bytes()[0] {
        b'-' => EntryKind::File,
        b'd' => EntryKind::Dir,
        b'l' => EntryKind::Symlink,
        _ => EntryKind::Other,
    };
    let mut target = None;
    if kind == EntryKind::Symlink
        && let Some((n, tgt)) = name.split_once(" -> ") {
            name = n;
            target = Some(tgt.to_string());
        }
    if name == "." || name == ".." || name.is_empty() {
        return None;
    }
    let mut e = entry(dir, name, kind);
    e.link_target = target;
    e.mode = parse_mode(perm);
    e.size = if kind == EntryKind::Dir { None } else { size };
    // Owner/group: tokens between the link count and the size (one or two of them).
    let between: Vec<&str> = t[2..i - 1].iter().map(|x| x.1).collect();
    e.owner = between.first().map(|s| s.to_string());
    e.group = between.get(1).map(|s| s.to_string());

    let mo = month(t[i].1)?;
    let day: u32 = t[i + 1].1.parse().ok()?;
    let ty = t[i + 2].1;
    e.modified = if let Some((h, m)) = ty.split_once(':') {
        let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
        let now_secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
        let (year, _, _) = civil_from_days(now_secs.div_euclid(86_400));
        let cand = to_time(year, mo, day, h, m, 0)?;
        if cand > now + Duration::from_secs(86_400) { to_time(year - 1, mo, day, h, m, 0) } else { Some(cand) }
    } else {
        to_time(ty.parse().ok()?, mo, day, 0, 0, 0)
    };
    Some(e)
}

/// `01-15-20  03:04PM       <DIR>          name` or `... 1234 name`; 2- or 4-digit years,
/// 12- or 24-hour times.
fn parse_dos(dir: &RPath, line: &str) -> Option<Entry> {
    let t = tokens(line);
    if t.len() < 4 {
        return None;
    }
    let date: Vec<&str> = t[0].1.split(['-', '/']).collect();
    if date.len() != 3 {
        return None;
    }
    let (mo, d): (u32, u32) = (date[0].parse().ok()?, date[1].parse().ok()?);
    let mut y: i64 = date[2].parse().ok()?;
    if date[2].len() == 2 {
        y += if y < 70 { 2000 } else { 1900 };
    }
    let time = t[1].1.to_ascii_uppercase();
    let (hm, pm) = match time.strip_suffix("PM") {
        Some(x) => (x, Some(true)),
        None => match time.strip_suffix("AM") {
            Some(x) => (x, Some(false)),
            None => (time.as_str(), None),
        },
    };
    let (h, m) = hm.split_once(':')?;
    let (mut h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    match pm {
        Some(true) if h < 12 => h += 12,
        Some(false) if h == 12 => h = 0,
        _ => {}
    }
    let name = &line[t[3].0..];
    if name == "." || name == ".." {
        return None;
    }
    let is_dir = t[2].1.eq_ignore_ascii_case("<DIR>");
    let mut e = entry(dir, name, if is_dir { EntryKind::Dir } else { EntryKind::File });
    if !is_dir {
        e.size = Some(t[2].1.replace(',', "").parse().ok()?);
    }
    e.modified = to_time(y, mo, d, h, m, 0);
    Some(e)
}

#[cfg(test)]
#[path = "list_tests.rs"]
mod tests;
