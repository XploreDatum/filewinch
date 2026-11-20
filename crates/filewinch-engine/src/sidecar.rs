//! Where a segmented download keeps its pieces: the hidden temp file it fills
//! (`.name.filewinch-part`) and a small sidecar (`.name.filewinch-ranges`) listing the byte
//! ranges already saved, so a pause, a failure or an app restart continues from them.
//!
//! The sidecar is plain text, rewritten whole (the local backend replaces a file atomically):
//!
//! ```text
//! filewinch-ranges 1
//! size 73400320
//! mtime 1732000000
//! source /data/big.iso
//! done 0 8388608
//! done 25165824 33554432
//! ```
//!
//! It belongs to one source file: a different size, modification time or path means the
//! source changed, and the download starts over.

use filewinch_core::{Entry, ErrorKind, RPath, Remote, Result};
use std::io::{Read, Write};
use std::time::UNIX_EPOCH;

const MAGIC: &str = "filewinch-ranges 1";
/// A sidecar bigger than this isn't ours.
const MAX_LEN: u64 = 1 << 20;

/// The temp file, its sidecar, and the name an overwritten original waits under.
pub(crate) struct Names {
    pub temp: RPath,
    pub ranges: RPath,
    pub old: RPath,
}

impl Names {
    pub fn for_target(target: &RPath) -> Names {
        let parent = target.parent().unwrap_or_else(RPath::root);
        let name = target.name();
        Names {
            temp: parent.join(&format!(".{name}.filewinch-part")),
            ranges: parent.join(&format!(".{name}.filewinch-ranges")),
            old: parent.join(&format!(".{name}.filewinch-old")),
        }
    }

    /// The names that go with a temp file from [`Names::for_target`].
    pub fn of_temp(temp: &RPath) -> Names {
        let parent = temp.parent().unwrap_or_else(RPath::root);
        let name = temp.name();
        let target = name.strip_prefix('.').and_then(|n| n.strip_suffix(".filewinch-part")).unwrap_or(name);
        Names::for_target(&parent.join(target))
    }
}

/// Sorted, non-overlapping, non-empty half-open byte ranges.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Ranges(pub Vec<(u64, u64)>);

impl Ranges {
    pub fn add(&mut self, a: u64, b: u64) {
        if a >= b {
            return;
        }
        self.0.push((a, b));
        self.0.sort_unstable();
        let mut out: Vec<(u64, u64)> = Vec::with_capacity(self.0.len());
        for &(a, b) in &self.0 {
            match out.last_mut() {
                Some(last) if a <= last.1 => last.1 = last.1.max(b),
                _ => out.push((a, b)),
            }
        }
        self.0 = out;
    }

    pub fn total(&self) -> u64 {
        self.0.iter().map(|(a, b)| b - a).sum()
    }

    /// What's missing from `0..size`.
    pub fn gaps(&self, size: u64) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        let mut at = 0;
        for &(a, b) in &self.0 {
            if a > at {
                out.push((at, a.min(size)));
            }
            at = at.max(b);
        }
        if at < size {
            out.push((at, size));
        }
        out.retain(|(a, b)| a < b);
        out
    }
}

/// Identifies the source a sidecar belongs to.
fn header(src: &Entry, size: u64) -> String {
    let mtime = src
        .modified
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map_or_else(|| "-".to_owned(), |d| d.as_secs().to_string());
    format!("{MAGIC}\nsize {size}\nmtime {mtime}\nsource {}\n", src.path)
}

pub(crate) fn format(src: &Entry, size: u64, done: &Ranges) -> String {
    let mut s = header(src, size);
    for (a, b) in &done.0 {
        s.push_str(&format!("done {a} {b}\n"));
    }
    s
}

/// The finished ranges in `text`, or None when it isn't a sidecar for this source.
pub(crate) fn parse(text: &str, src: &Entry, size: u64) -> Option<Ranges> {
    let head = header(src, size);
    let body = text.strip_prefix(&head)?;
    let mut done = Ranges::default();
    for line in body.lines() {
        let mut w = line.split(' ');
        let (Some("done"), Some(a), Some(b), None) = (w.next(), w.next(), w.next(), w.next()) else { return None };
        let (a, b): (u64, u64) = (a.parse().ok()?, b.parse().ok()?);
        if a >= b || b > size {
            return None;
        }
        done.add(a, b);
    }
    Some(done)
}

/// Reads the sidecar next to a temp file; None when there is none or it doesn't match.
pub(crate) fn load(dst: &dyn Remote, names: &Names, src: &Entry, size: u64) -> Result<Option<Ranges>> {
    let mut r = match dst.read(&names.ranges, 0) {
        Ok(r) => r,
        Err(e) if e.kind == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut text = String::new();
    if r.by_ref().take(MAX_LEN).read_to_string(&mut text).is_err() {
        return Ok(None);
    }
    Ok(parse(&text, src, size))
}

pub(crate) fn save(dst: &dyn Remote, names: &Names, src: &Entry, size: u64, done: &Ranges) -> Result<()> {
    let text = format(src, size, done);
    let mut w = dst.write(&names.ranges, 0, Some(text.len() as u64))?;
    w.write_all(text.as_bytes())?;
    w.finish()
}

/// Removes the temp file and its sidecar (best effort: either may be gone already).
pub(crate) fn remove(dst: &dyn Remote, names: &Names) {
    let _ = dst.remove_file(&names.temp);
    let _ = dst.remove_file(&names.ranges);
}

#[cfg(test)]
mod tests {
    use super::*;
    use filewinch_core::EntryKind;
    use std::time::{Duration, SystemTime};

    fn src() -> Entry {
        Entry {
            name: "big.iso".into(),
            path: RPath::new("/data/big.iso"),
            kind: EntryKind::File,
            size: Some(1000),
            modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_732_000_000)),
            mode: None,
            owner: None,
            group: None,
            link_target: None,
            extra: vec![],
            checksums: vec![],
            shared: None,
        }
    }

    #[test]
    fn ranges_merge_and_gaps() {
        let mut r = Ranges::default();
        r.add(10, 20);
        r.add(30, 40);
        r.add(15, 30);
        r.add(50, 50);
        assert_eq!(r.0, vec![(10, 40)]);
        assert_eq!(r.gaps(100), vec![(0, 10), (40, 100)]);
        assert_eq!(r.total(), 30);
        assert_eq!(Ranges::default().gaps(5), vec![(0, 5)]);
        r.add(0, 100);
        assert!(r.gaps(100).is_empty());
    }

    #[test]
    fn sidecar_round_trips_and_rejects_another_source() {
        let mut r = Ranges::default();
        r.add(0, 100);
        r.add(500, 700);
        let text = format(&src(), 1000, &r);
        assert_eq!(parse(&text, &src(), 1000), Some(r));
        assert_eq!(parse(&text, &src(), 1001), None, "size changed");
        let mut newer = src();
        newer.modified = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_732_000_001));
        assert_eq!(parse(&text, &newer, 1000), None, "modified since");
        assert_eq!(parse("garbage", &src(), 1000), None);
        assert_eq!(parse(&format!("{}done 900 1200\n", header(&src(), 1000)), &src(), 1000), None, "past the end");
    }

    #[test]
    fn names_are_hidden_siblings() {
        let n = Names::for_target(&RPath::new("/dl/big.iso"));
        assert_eq!(n.temp.as_str(), "/dl/.big.iso.filewinch-part");
        assert_eq!(n.ranges.as_str(), "/dl/.big.iso.filewinch-ranges");
        assert_eq!(Names::of_temp(&n.temp).ranges, n.ranges);
    }
}
