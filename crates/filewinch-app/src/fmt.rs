//! Human formatting: sizes, dates in local time, speeds, durations, permissions.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Seconds since 1970 that "now" is pinned to for headless scenes; 0 follows the clock.
static FROZEN: AtomicU64 = AtomicU64::new(0);

/// The moment dates are shown relative to ("Today, 14:05"): the clock, unless a headless scene
/// pinned it so its screenshots come out the same every run.
pub fn now() -> SystemTime {
    match FROZEN.load(Ordering::Relaxed) {
        0 => SystemTime::now(),
        s => UNIX_EPOCH + Duration::from_secs(s),
    }
}

/// Pins [`now`] to `t` (headless scenes only).
pub fn freeze_now(t: SystemTime) {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()).max(1);
    FROZEN.store(secs, Ordering::Relaxed);
}

/// "0 bytes", "1 byte", "12.3 KB" (decimal units, as Finder shows them).
pub fn size(bytes: u64) -> String {
    match bytes {
        0 => "Zero bytes".into(),
        1 => "1 byte".into(),
        b if b < 1000 => format!("{b} bytes"),
        b => {
            let units = ["KB", "MB", "GB", "TB", "PB"];
            let mut v = b as f64 / 1000.0;
            let mut u = 0;
            while v >= 999.95 && u + 1 < units.len() {
                v /= 1000.0;
                u += 1;
            }
            if v < 10.0 { format!("{v:.1} {}", units[u]) } else { format!("{v:.0} {}", units[u]) }
        }
    }
}

/// "1,234,567 bytes" for Get Info.
pub fn exact_bytes(bytes: u64) -> String {
    format!("{} bytes", count(bytes))
}

/// "1,234,567"
pub fn count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn speed(bytes_per_sec: f64) -> String {
    if bytes_per_sec < 1.0 {
        return "—".into();
    }
    format!("{}/s", size(bytes_per_sec as u64))
}

/// "4 s", "3 min", "1 h 12 min".
pub fn duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{} min", s.div_ceil(60)),
        _ => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    }
}

/// Broken-down local time for `t` (via `localtime_r`).
pub struct Local {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub min: u32,
}

pub fn local(t: SystemTime) -> Local {
    let secs = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as libc::time_t,
        Err(e) => -(e.duration().as_secs() as libc::time_t),
    };
    // SAFETY: `tm` is a plain C struct; localtime_r / localtime_s fill it and keep no reference.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        #[cfg(unix)]
        libc::localtime_r(&secs, &mut tm);
        #[cfg(windows)]
        libc::localtime_s(&mut tm, &secs);
        tm
    };
    Local { year: tm.tm_year + 1900, month: tm.tm_mon as u32 + 1, day: tm.tm_mday as u32, hour: tm.tm_hour as u32, min: tm.tm_min as u32 }
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// "Today, 14:05", "Yesterday, 09:12", "3 Mar 2026, 18:40".
pub fn date(t: SystemTime, now: SystemTime) -> String {
    let (a, n) = (local(t), local(now));
    let same_day = |x: &Local, y: &Local| x.year == y.year && x.month == y.month && x.day == y.day;
    let yesterday = local(now - Duration::from_secs(86_400));
    let clock = format!("{:02}:{:02}", a.hour, a.min);
    if same_day(&a, &n) {
        format!("Today, {clock}")
    } else if same_day(&a, &yesterday) {
        format!("Yesterday, {clock}")
    } else {
        format!("{} {} {}, {clock}", a.day, MONTHS[(a.month as usize).saturating_sub(1).min(11)], a.year)
    }
}

/// "rwxr-xr-x" for the low nine mode bits, with setuid/setgid/sticky shown as ls does.
pub fn mode_string(mode: u32) -> String {
    let mut s: Vec<char> = "rwxrwxrwx".chars().collect();
    for (i, c) in s.iter_mut().enumerate() {
        if mode & (1 << (8 - i)) == 0 {
            *c = '-';
        }
    }
    let special = [(0o4000, 2, 's', 'S'), (0o2000, 5, 's', 'S'), (0o1000, 8, 't', 'T')];
    for (bit, at, with_x, without_x) in special {
        if mode & bit != 0 {
            s[at] = if s[at] == 'x' { with_x } else { without_x };
        }
    }
    s.into_iter().collect()
}

/// Parses "644", "0755" or "4755" into mode bits; `None` for anything else.
pub fn parse_octal(s: &str) -> Option<u32> {
    let s = s.trim();
    if s.is_empty() || s.len() > 5 || !s.chars().all(|c| ('0'..='7').contains(&c)) {
        return None;
    }
    u32::from_str_radix(s, 8).ok().filter(|m| *m <= 0o7777)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(size(0), "Zero bytes");
        assert_eq!(size(1), "1 byte");
        assert_eq!(size(999), "999 bytes");
        assert_eq!(size(1_000), "1.0 KB");
        assert_eq!(size(12_345), "12 KB");
        assert_eq!(size(999_960), "1.0 MB");
        assert_eq!(size(5_300_000_000), "5.3 GB");
        assert_eq!(exact_bytes(1_234_567), "1,234,567 bytes");
        assert_eq!(exact_bytes(12), "12 bytes");
    }

    #[test]
    fn durations_and_speeds() {
        assert_eq!(duration(Duration::from_secs(4)), "4 s");
        assert_eq!(duration(Duration::from_secs(61)), "2 min");
        assert_eq!(duration(Duration::from_secs(4320)), "1 h 12 min");
        assert_eq!(speed(0.0), "—");
        assert_eq!(speed(2_500_000.0), "2.5 MB/s");
    }

    #[test]
    fn modes() {
        assert_eq!(mode_string(0o755), "rwxr-xr-x");
        assert_eq!(mode_string(0o644), "rw-r--r--");
        assert_eq!(mode_string(0o4755), "rwsr-xr-x");
        assert_eq!(mode_string(0o1777), "rwxrwxrwt");
        assert_eq!(parse_octal("644"), Some(0o644));
        assert_eq!(parse_octal("0755"), Some(0o755));
        assert_eq!(parse_octal("8"), None);
        assert_eq!(parse_octal(""), None);
        assert_eq!(parse_octal("77777"), None);
    }

    #[test]
    fn dates_relative_to_now() {
        let now = SystemTime::now();
        assert!(date(now, now).starts_with("Today, "));
        assert!(date(now - Duration::from_secs(86_400), now).starts_with("Yesterday, "));
        let old = date(UNIX_EPOCH + Duration::from_secs(1_700_000_000), now);
        assert!(old.contains("Nov 2023"), "{old}");
    }
}
