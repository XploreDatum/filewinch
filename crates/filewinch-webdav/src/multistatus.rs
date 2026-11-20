//! Parsing PROPFIND `multistatus` responses (RFC 4918 §14.16) as real servers send them:
//! any namespace prefix (`D:`, `d:`, `a:`, `lp1:`, default namespace), hrefs as absolute URLs
//! or paths, one `propstat` per status (404 ones ignored), and sloppy dates.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use quick_xml::NsReader;
use quick_xml::events::Event;
use quick_xml::name::{Namespace, ResolveResult};

/// One `<response>` with the properties from its successful propstats.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct PropEntry {
    pub href: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub etag: Option<String>,
    pub content_type: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tag {
    Multistatus,
    Response,
    Href,
    RespStatus,
    Propstat,
    Prop,
    PsStatus,
    ResourceType,
    IsCollection,
    Length,
    Modified,
    Etag,
    ContentType,
    Other,
}

impl Tag {
    fn captures_text(self) -> bool {
        matches!(
            self,
            Tag::Href
                | Tag::RespStatus
                | Tag::PsStatus
                | Tag::IsCollection
                | Tag::Length
                | Tag::Modified
                | Tag::Etag
                | Tag::ContentType
        )
    }
}

/// The tag for an element named `name` (local part) inside `parent`. DAV properties must be
/// in the `DAV:` namespace, but elements with no namespace at all are tolerated.
fn classify(parent: Option<Tag>, dav: bool, name: &[u8]) -> Tag {
    use Tag::*;
    // Microsoft's `iscollection` lives in its own namespace.
    if parent == Some(Prop) && name.eq_ignore_ascii_case(b"iscollection") {
        return IsCollection;
    }
    if !dav {
        return Other;
    }
    match (parent, name) {
        (None, b"multistatus") => Multistatus,
        (Some(Multistatus), b"response") => Response,
        (Some(Response), b"href") => Href,
        (Some(Response), b"status") => RespStatus,
        (Some(Response), b"propstat") => Propstat,
        (Some(Propstat), b"prop") => Prop,
        (Some(Propstat), b"status") => PsStatus,
        (Some(Prop), b"resourcetype") => ResourceType,
        (Some(Prop), b"getcontentlength") => Length,
        (Some(Prop), b"getlastmodified") => Modified,
        (Some(Prop), b"getetag") => Etag,
        (Some(Prop), b"getcontenttype") => ContentType,
        _ => Other,
    }
}

#[derive(Default)]
struct Propstat {
    status: Option<u16>,
    props: PropEntry,
}

/// Parses a multistatus body. `Err` carries a short reason for the error detail.
pub(crate) fn parse(xml: &[u8]) -> Result<Vec<PropEntry>, String> {
    let text = String::from_utf8_lossy(xml);
    let mut reader = NsReader::from_str(&text);
    let mut stack: Vec<Tag> = Vec::new();
    let mut text_buf = String::new();
    let mut out = Vec::new();
    let mut seen_root = false;
    let mut resp = PropEntry::default();
    let mut resp_status: Option<u16> = None;
    let mut ps = Propstat::default();

    loop {
        let (ns, event) = reader.read_resolved_event().map_err(|e| format!("bad XML: {e}"))?;
        let dav = matches!(ns, ResolveResult::Bound(Namespace(b"DAV:")) | ResolveResult::Unbound);
        let (start, empty) = match &event {
            Event::Start(_) => (true, false),
            Event::Empty(_) => (true, true),
            _ => (false, false),
        };
        if start {
            let (Event::Start(e) | Event::Empty(e)) = &event else { unreachable!() };
            let parent = stack.last().copied();
            let tag = if parent == Some(Tag::Other) {
                Tag::Other
            } else {
                classify(parent, dav, e.local_name().as_ref())
            };
            match tag {
                Tag::Multistatus => seen_root = true,
                Tag::Response => {
                    resp = PropEntry::default();
                    resp_status = None;
                }
                Tag::Propstat => ps = Propstat::default(),
                _ => {}
            }
            if parent == Some(Tag::ResourceType) && dav && e.local_name().as_ref() == b"collection" {
                ps.props.is_dir = true;
            }
            text_buf.clear();
            stack.push(tag);
            if !empty {
                continue;
            }
        }
        match event {
            Event::Text(t) => {
                if stack.last().is_some_and(|t| t.captures_text()) {
                    text_buf.push_str(&t.xml10_content().map_err(|e| format!("bad text: {e}"))?);
                }
                continue;
            }
            Event::CData(t) => {
                if stack.last().is_some_and(|t| t.captures_text()) {
                    text_buf.push_str(&String::from_utf8_lossy(&t));
                }
                continue;
            }
            Event::GeneralRef(r) => {
                if stack.last().is_some_and(|t| t.captures_text()) {
                    if let Ok(Some(c)) = r.resolve_char_ref() {
                        text_buf.push(c);
                    } else {
                        let name = r.decode().map_err(|e| format!("bad entity: {e}"))?;
                        let resolved = quick_xml::escape::resolve_predefined_entity(&name)
                            .ok_or_else(|| format!("unknown entity &{name};"))?;
                        text_buf.push_str(resolved);
                    }
                }
                continue;
            }
            Event::Eof => break,
            Event::End(_) => {}
            _ if !empty => continue,
            _ => {}
        }
        // End of an element (or an empty element).
        let Some(tag) = stack.pop() else { continue };
        let value = text_buf.trim();
        match tag {
            Tag::Href => resp.href = value.to_string(),
            Tag::RespStatus => resp_status = status_code(value),
            Tag::PsStatus => ps.status = status_code(value),
            Tag::IsCollection => ps.props.is_dir |= value == "1" || value.eq_ignore_ascii_case("true"),
            Tag::Length => ps.props.size = value.parse().ok(),
            Tag::Modified => ps.props.modified = parse_date(value),
            Tag::Etag if !value.is_empty() => ps.props.etag = Some(value.to_string()),
            Tag::ContentType if !value.is_empty() => ps.props.content_type = Some(value.to_string()),
            Tag::Propstat => {
                let ps = std::mem::take(&mut ps);
                if ps.status.is_none_or(|s| (200..300).contains(&s)) {
                    let p = ps.props;
                    resp.is_dir |= p.is_dir;
                    resp.size = p.size.or(resp.size);
                    resp.modified = p.modified.or(resp.modified);
                    resp.etag = p.etag.or(resp.etag.take());
                    resp.content_type = p.content_type.or(resp.content_type.take());
                }
            }
            Tag::Response if !resp.href.is_empty() && resp_status.is_none_or(|s| (200..300).contains(&s)) => {
                out.push(std::mem::take(&mut resp));
            }
            _ => {}
        }
        text_buf.clear();
    }
    if !seen_root {
        return Err("the reply isn't a WebDAV multistatus".into());
    }
    Ok(out)
}

/// `HTTP/1.1 404 Not Found` → 404.
fn status_code(line: &str) -> Option<u16> {
    line.split_whitespace().nth(1)?.parse().ok()
}

/// RFC 1123 (and the other HTTP date forms) first, then ISO 8601 / RFC 3339, which some
/// servers send by mistake. Anything else is `None`.
pub(crate) fn parse_date(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    httpdate::parse_http_date(s).ok().or_else(|| parse_rfc3339(s))
}

/// `YYYY-MM-DDTHH:MM:SS[.frac](Z|±HH:MM)`.
fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, min, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let mut rest = &s[19..];
    if let Some(r) = rest.strip_prefix('.') {
        rest = r.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ if rest.len() == 6 && (rest.starts_with('+') || rest.starts_with('-')) => {
            let h: i64 = rest[1..3].parse().ok()?;
            let m: i64 = rest[4..6].parse().ok()?;
            let o = h * 3600 + m * 60;
            if rest.starts_with('-') { -o } else { o }
        }
        _ => return None,
    };
    // Days from civil (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hour * 3600 + min * 60 + sec - offset;
    u64::try_from(secs).ok().map(|s| UNIX_EPOCH + Duration::from_secs(s))
}

#[cfg(test)]
#[path = "multistatus_tests.rs"]
mod tests;
