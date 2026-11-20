//! Listing containers and pseudo-folders, and `stat`.

use std::collections::HashSet;

use filewinch_core::{Checksum, Entry, EntryKind, Error, ErrorKind, HashAlgo, RPath, Result};
use filewinch_http::ureq::Body;
use filewinch_http::ureq::http::Response;
use serde::Deserialize;

use crate::client::{Client, Loc, enc, url_path};
use crate::time::{parse_http, parse_iso};

#[derive(Deserialize)]
struct ContainerRow {
    name: String,
    count: Option<u64>,
    bytes: Option<u64>,
    last_modified: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ObjectRow {
    Subdir { subdir: String },
    Object {
        name: String,
        bytes: Option<u64>,
        hash: Option<String>,
        last_modified: Option<String>,
        content_type: Option<String>,
        /// Present (the manifest's own ETag) when the row is a Static Large Object.
        slo_etag: Option<String>,
    },
}

impl ObjectRow {
    fn marker(&self) -> &str {
        match self {
            ObjectRow::Subdir { subdir } => subdir,
            ObjectRow::Object { name, .. } => name,
        }
    }
}

pub(crate) fn is_dir_type(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|ct| {
        ct.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("application/directory")
    })
}

fn dir_entry(path: RPath) -> Entry {
    Entry {
        name: path.name().to_string(),
        path,
        kind: EntryKind::Dir,
        size: None,
        modified: None,
        mode: None,
        owner: None,
        group: None,
        link_target: None,
        extra: Vec::new(), checksums: Vec::new(), shared: None,
    }
}

impl Client {
    /// GETs one listing page. `None` when the container (or account) doesn't exist.
    fn page<T: serde::de::DeserializeOwned>(&self, path: &str, query: &str, what: &str) -> Result<Option<Vec<T>>> {
        let resp = self.send("GET", &format!("{path}?format=json{query}"), &[("accept", "application/json")], None, what)?;
        match resp.status().as_u16() {
            200 => self.json(resp, what).map(Some),
            204 => Ok(Some(Vec::new())),
            404 => Ok(None),
            _ => Err(self.fail(resp, what)),
        }
    }

    /// Every page of a listing, following `marker`. `None` on 404.
    fn all_pages<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &str,
        marker_of: fn(&T) -> &str,
        what: &str,
    ) -> Result<Option<Vec<T>>> {
        let limit = self.tuning.page_size.max(2);
        let mut out = Vec::new();
        let mut marker = String::new();
        loop {
            let q = format!("&limit={limit}&marker={}{query}", enc(&marker));
            let Some(page) = self.page::<T>(path, &q, what)? else { return Ok(None) };
            let full = page.len() >= limit as usize;
            let next = page.last().map(|r| marker_of(r).to_string());
            out.extend(page);
            match next {
                Some(m) if full && m != marker => marker = m,
                _ => return Ok(Some(out)),
            }
        }
    }

    pub fn list_dir(&self, dir: &RPath) -> Result<Vec<Entry>> {
        let what = format!("list {dir}");
        match Loc::of(dir) {
            Loc::Root => {
                let rows = self
                    .all_pages::<ContainerRow>("", "", |r| &r.name, &what)?
                    .ok_or_else(|| Error::new(ErrorKind::NotFound, "This Swift account wasn't found."))?;
                Ok(rows.into_iter().map(container_entry).collect())
            }
            Loc::Container(c) => self.list_prefix(dir, &c, "", &what),
            Loc::Object { container, key } => self.list_prefix(dir, &container, &format!("{key}/"), &what),
        }
    }

    fn list_prefix(&self, dir: &RPath, container: &str, prefix: &str, what: &str) -> Result<Vec<Entry>> {
        let query = format!("&delimiter=%2F&prefix={}", enc(prefix));
        let Some(rows) = self.all_pages::<ObjectRow>(&url_path(container, None), &query, ObjectRow::marker, what)? else {
            return Err(Error::new(ErrorKind::NotFound, format!("The container “{container}” doesn't exist.")));
        };
        if rows.is_empty() && !prefix.is_empty() {
            // No marker and no children: an old-style folder object, a file, or nothing.
            let key = prefix.trim_end_matches('/');
            return match self.head_object(container, key, what)? {
                Some(e) if e.is_dir() => Ok(Vec::new()),
                Some(_) => Err(Error::new(ErrorKind::InvalidPath, format!("“{}” is a file, not a folder.", dir.name()))),
                None => Err(Error::new(ErrorKind::NotFound, format!("The folder “{dir}” doesn't exist."))),
            };
        }
        let mut dirs_seen = HashSet::new();
        let mut out = Vec::new();
        for row in rows {
            match row {
                ObjectRow::Subdir { subdir } => {
                    let name = subdir.strip_prefix(prefix).unwrap_or(&subdir).trim_end_matches('/');
                    if !name.is_empty() && dirs_seen.insert(name.to_string()) {
                        out.push(dir_entry(dir.join(name)));
                    }
                }
                ObjectRow::Object { name, bytes, hash, last_modified, content_type, slo_etag } => {
                    let rel = name.strip_prefix(prefix).unwrap_or(&name);
                    if rel.is_empty() || rel == "/" {
                        continue; // the folder's own marker
                    }
                    let is_marker = rel.ends_with('/') || is_dir_type(content_type.as_deref());
                    let rel = rel.trim_end_matches('/');
                    if rel.is_empty() || rel.contains('/') {
                        continue; // "a//b"-style names RPath can't address
                    }
                    if is_marker {
                        if dirs_seen.insert(rel.to_string()) {
                            out.push(dir_entry(dir.join(rel)));
                        }
                        continue;
                    }
                    let mut e = dir_entry(dir.join(rel));
                    e.kind = EntryKind::File;
                    e.size = bytes;
                    e.modified = last_modified.as_deref().and_then(parse_iso);
                    // SLO rows carry `slo_etag` (or, on older Swift, `swift_bytes=` in the
                    // content type). A DLO manifest lists as a plain 0-byte object whose hash
                    // is the MD5 of nothing, indistinguishable from an empty file, so 0-byte
                    // rows get no checksum here (a HEAD, which shows X-Object-Manifest, does).
                    let large = slo_etag.is_some() || content_type.as_deref().is_some_and(has_swift_bytes);
                    if let Some(h) = &hash
                        && !large
                        && bytes.is_some_and(|b| b > 0)
                    {
                        e.checksums.extend(md5_etag(h));
                    }
                    if let Some(ct) = content_type {
                        e.extra.push(("Content type".into(), ct));
                    }
                    if let Some(h) = hash {
                        e.extra.push(("ETag".into(), h));
                    }
                    out.push(e);
                }
            }
        }
        Ok(out)
    }

    /// HEADs an object. `None` if it doesn't exist.
    pub fn head_object(&self, container: &str, key: &str, what: &str) -> Result<Option<Entry>> {
        let resp = self.send("HEAD", &url_path(container, Some(key)), &[], None, what)?;
        match resp.status().as_u16() {
            200 | 204 => Ok(Some(object_entry(&RPath::new(&format!("{container}/{key}")), &resp))),
            404 => Ok(None),
            _ => Err(self.fail(resp, what)),
        }
    }

    /// True if any object name starts with `prefix` (a folder exists).
    pub fn prefix_exists(&self, container: &str, prefix: &str, what: &str) -> Result<bool> {
        let q = format!("&limit=1&prefix={}", enc(prefix));
        Ok(self.page::<ObjectRow>(&url_path(container, None), &q, what)?.is_some_and(|p| !p.is_empty()))
    }

    /// Objects under `prefix` other than the folder's own marker, up to 2 (enough to tell
    /// "empty" from "not empty"). `None` if the container doesn't exist.
    pub fn children_besides_marker(&self, container: &str, prefix: &str, what: &str) -> Result<Option<usize>> {
        let q = format!("&limit=2&prefix={}", enc(prefix));
        Ok(self
            .page::<ObjectRow>(&url_path(container, None), &q, what)?
            .map(|p| p.iter().filter(|r| r.marker() != prefix).count()))
    }

    pub fn stat_path(&self, path: &RPath) -> Result<Entry> {
        let what = format!("get info for {path}");
        match Loc::of(path) {
            Loc::Root => Ok(Entry { name: "/".into(), ..dir_entry(RPath::root()) }),
            Loc::Container(c) => {
                let resp = self.send("HEAD", &url_path(&c, None), &[], None, &what)?;
                match resp.status().as_u16() {
                    200 | 204 => {
                        let mut e = dir_entry(path.clone());
                        let h = |n: &str| resp.headers().get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
                        if let Some(n) = h("x-container-object-count") {
                            e.extra.push(("Objects".into(), n));
                        }
                        if let Some(n) = h("x-container-bytes-used") {
                            e.extra.push(("Bytes used".into(), n));
                        }
                        Ok(e)
                    }
                    404 => Err(not_found(path)),
                    _ => Err(self.fail(resp, &what)),
                }
            }
            Loc::Object { container, key } => {
                if let Some(e) = self.head_object(&container, &key, &what)? {
                    return Ok(e);
                }
                if self.prefix_exists(&container, &format!("{key}/"), &what)? {
                    return Ok(dir_entry(path.clone()));
                }
                Err(not_found(path))
            }
        }
    }
}

pub(crate) fn not_found(path: &RPath) -> Error {
    Error::new(ErrorKind::NotFound, format!("“{}” wasn't found on the server.", path.name()))
}

fn container_entry(r: ContainerRow) -> Entry {
    let mut e = dir_entry(RPath::root().join(&r.name));
    e.modified = r.last_modified.as_deref().and_then(parse_iso);
    if let Some(n) = r.count {
        e.extra.push(("Objects".into(), n.to_string()));
    }
    if let Some(n) = r.bytes {
        e.extra.push(("Bytes used".into(), n.to_string()));
    }
    e
}

fn object_entry(path: &RPath, resp: &Response<Body>) -> Entry {
    let h = |n: &str| resp.headers().get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
    let ct = h("content-type");
    let mut e = dir_entry(path.clone());
    if is_dir_type(ct.as_deref()) {
        return e;
    }
    e.kind = EntryKind::File;
    e.size = h("content-length").and_then(|v| v.trim().parse().ok());
    e.modified = h("last-modified").as_deref().and_then(parse_http);
    if let Some(ct) = ct {
        e.extra.push(("Content type".into(), ct));
    }
    if let Some(t) = h("etag") {
        e.extra.push(("ETag".into(), t.trim_matches('"').to_string()));
    }
    let slo = h("x-static-large-object").is_some_and(|v| v.trim().eq_ignore_ascii_case("true"));
    let dlo = h("x-object-manifest").is_some();
    if slo {
        e.extra.push(("Large object".into(), "Static (segmented)".into()));
    }
    if !slo && !dlo {
        e.checksums.extend(h("etag").as_deref().and_then(md5_etag));
    }
    e
}

/// A plain object's ETag (listing `hash` or HEAD `ETag`) as its MD5: exactly 32 hex digits,
/// quotes stripped. Callers exclude large objects: an SLO's ETag is the MD5 of its segments'
/// MD5s and a DLO's the MD5 of its segments' ETags, neither the MD5 of the content.
pub(crate) fn md5_etag(etag: &str) -> Option<Checksum> {
    let t = etag.trim();
    let t = t.strip_prefix('"').and_then(|t| t.strip_suffix('"')).unwrap_or(t);
    (t.len() == 32).then(|| Checksum::new(HashAlgo::Md5, t)).flatten()
}

/// Older Swift marks an SLO row in listings with `;swift_bytes=<n>` on its content type.
pub(crate) fn has_swift_bytes(content_type: &str) -> bool {
    content_type.split(';').skip(1).any(|p| p.trim().to_ascii_lowercase().starts_with("swift_bytes="))
}

/// True if a HEAD response's entry is a Static Large Object manifest.
pub(crate) fn is_slo(e: &Entry) -> bool {
    e.extra.iter().any(|(k, _)| k == "Large object")
}
