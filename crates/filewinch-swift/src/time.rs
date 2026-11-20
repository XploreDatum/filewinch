//! Timestamps: Swift listings (`2024-03-05T10:20:30.123456`, UTC without a zone), Keystone
//! expiries (`…Z`, `…+00:00`, Rackspace's `…-06:00`) and HTTP dates (`Last-Modified`).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Parses ISO 8601 `YYYY-MM-DDTHH:MM:SS[.fraction][Z|±HH:MM]`; no zone means UTC.
pub(crate) fn parse_iso(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let t = s.get(r)?;
        if t.bytes().all(|c| c.is_ascii_digit()) { t.parse().ok() } else { None }
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut rest = &s[19..];
    let mut nanos = 0u32;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let mut n: u64 = frac[..digits.min(9)].parse().ok()?;
        for _ in digits.min(9)..9 {
            n *= 10;
        }
        nanos = n as u32;
        rest = &frac[digits..];
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        o if o.len() == 6 && (o.starts_with('+') || o.starts_with('-')) && &o[3..4] == ":" => {
            let v = num_str(&o[1..3])? * 3600 + num_str(&o[4..6])? * 60;
            if o.starts_with('-') { -v } else { v }
        }
        _ => return None,
    };
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se - offset;
    let secs = u64::try_from(secs).ok()?;
    Some(UNIX_EPOCH + Duration::new(secs, nanos))
}

fn num_str(s: &str) -> Option<i64> {
    if s.bytes().all(|c| c.is_ascii_digit()) { s.parse().ok() } else { None }
}

/// `Last-Modified` and similar HTTP dates.
pub(crate) fn parse_http(s: &str) -> Option<SystemTime> {
    httpdate::parse_http_date(s.trim()).ok()
}

// Howard Hinnant's days-from-civil.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
