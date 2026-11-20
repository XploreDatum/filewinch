//! RFC 3339 timestamps (Drive, Graph, Dropbox, Box all use them) without a date crate.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Parses `2024-03-05T10:20:30Z`, `…30.123Z` and `…30-08:00`. `None` if malformed.
pub(crate) fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
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
        "Z" | "z" => 0,
        o if o.len() == 6 && (o.starts_with('+') || o.starts_with('-')) && &o[3..4] == ":" => {
            let oh: i64 = o[1..3].parse().ok()?;
            let om: i64 = o[4..6].parse().ok()?;
            let v = oh * 3600 + om * 60;
            if o.starts_with('-') { -v } else { v }
        }
        _ => return None,
    };
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se - offset;
    if secs >= 0 {
        Some(UNIX_EPOCH + Duration::new(secs as u64, nanos))
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs((-secs) as u64))?
            .checked_add(Duration::from_nanos(nanos as u64))
    }
}

/// `2024-03-05T10:20:30Z` (UTC, whole seconds; times before 1970 clamp to the epoch).
pub(crate) fn format_rfc3339(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

// Howard Hinnant's days-from-civil algorithms.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if m <= 2 {
            yoe + era * 400 + 1
        } else {
            yoe + era * 400
        },
        m,
        d,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_shapes() {
        let z = parse_rfc3339("2024-03-05T10:20:30Z").unwrap();
        assert_eq!(
            z.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_709_634_030
        );
        let frac = parse_rfc3339("2024-03-05T10:20:30.250Z").unwrap();
        assert_eq!(frac.duration_since(z).unwrap(), Duration::from_millis(250));
        let off = parse_rfc3339("2024-03-05T02:20:30-08:00").unwrap();
        assert_eq!(off, z);
        assert!(parse_rfc3339("not a date").is_none());
        assert!(parse_rfc3339("2024-13-05T10:20:30Z").is_none());
    }

    #[test]
    fn round_trips() {
        let t = UNIX_EPOCH + Duration::from_secs(1_709_634_030);
        assert_eq!(format_rfc3339(t), "2024-03-05T10:20:30Z");
        assert_eq!(parse_rfc3339(&format_rfc3339(t)), Some(t));
        assert_eq!(
            format_rfc3339(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00Z"
        );
    }
}
