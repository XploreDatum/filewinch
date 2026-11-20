//! The [`Remote`] impl: path mapping, listing, stat and downloads. Writes are in
//! `upload.rs`; mkdir, delete and copy in `ops.rs`.

use std::io::{self, Read};
use std::sync::Arc;

use filewinch_core::{Caps, Entry, EntryKind, Error, ErrorKind, RPath, Remote, Result, WriteSink};
use filewinch_http::{from_io, to_io};

use crate::client::{Call, Client, header, network};
use crate::xml::{self, BlobItem, BlobPage};

/// A path split into container and blob name.
pub(crate) enum Loc {
    Root,
    Container(String),
    /// Container and blob name (no leading or trailing `/`).
    Blob(String, String),
}

pub(crate) struct AzureRemote {
    pub(crate) c: Arc<Client>,
}

pub(crate) fn quoted(path: &RPath) -> String {
    format!("“{}”", if path.is_root() { "/" } else { path.name() })
}

impl AzureRemote {
    pub(crate) fn loc(&self, path: &RPath) -> Result<Loc> {
        self.c.check_open()?;
        let s = path.as_str().trim_start_matches('/');
        if s.is_empty() {
            return Ok(Loc::Root);
        }
        let (container, key) = match s.split_once('/') {
            Some((c, k)) => (c.to_string(), Some(k.to_string())),
            None => (s.to_string(), None),
        };
        if let Some(only) = &self.c.cfg.scoped
            && *only != container {
                return Err(Error::new(
                    ErrorKind::PermissionDenied,
                    format!("This connection can only reach the container “{only}”."),
                ));
            }
        Ok(match key {
            Some(k) => Loc::Blob(container, k),
            None => Loc::Container(container),
        })
    }

    fn dir_entry(path: &RPath) -> Entry {
        Entry {
            name: if path.is_root() { "/".into() } else { path.name().into() },
            path: path.clone(),
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

    fn file_entry(path: RPath, b: &BlobItem) -> Entry {
        let mut extra = Vec::new();
        if let Some(t) = &b.content_type {
            extra.push(("Content type".into(), t.clone()));
        }
        if let Some(t) = &b.tier {
            extra.push(("Access tier".into(), t.clone()));
        }
        if let Some(t) = &b.etag {
            extra.push(("ETag".into(), t.clone()));
        }
        Entry {
            name: path.name().into(),
            path,
            kind: EntryKind::File,
            size: b.size,
            modified: b.modified,
            mode: None,
            owner: None,
            group: None,
            link_target: None,
            extra,
            checksums: xml::content_md5(b.content_md5.as_deref()).into_iter().collect(),
            shared: None,
        }
    }

    /// One page of List Blobs.
    pub(crate) fn blob_page(
        &self,
        container: &str,
        prefix: &str,
        delimiter: bool,
        max: u32,
        marker: Option<&str>,
        what: &str,
    ) -> Result<BlobPage> {
        let mut call = Call::new("GET", Some(container), None)
            .q("restype", "container")
            .q("comp", "list")
            .q("maxresults", max.to_string());
        if !prefix.is_empty() {
            call = call.q("prefix", prefix);
        }
        if delimiter {
            call = call.q("delimiter", "/");
        }
        if let Some(m) = marker {
            call = call.q("marker", m);
        }
        let resp = self.c.send(&call, &[], what)?;
        xml::blobs(&self.c.text(resp)?)
    }

    fn list_containers(&self, what: &str) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        let mut marker: Option<String> = None;
        loop {
            let mut call = Call::new("GET", None, None).q("comp", "list").q("maxresults", self.c.page_size.to_string());
            if let Some(m) = &marker {
                call = call.q("marker", m.clone());
            }
            let resp = self.c.send(&call, &[], what)?;
            let (items, next) = xml::containers(&self.c.text(resp)?)?;
            for it in items {
                let mut e = Self::dir_entry(&RPath::root().join(&it.name));
                e.modified = it.modified;
                if let Some(t) = it.etag {
                    e.extra.push(("ETag".into(), t));
                }
                out.push(e);
            }
            match next {
                Some(m) if Some(&m) != marker.as_ref() => marker = Some(m),
                _ => return Ok(out),
            }
        }
    }

    fn list_prefix(&self, dir: &RPath, container: &str, key: &str) -> Result<Vec<Entry>> {
        let what = format!("list {}", quoted(dir));
        let prefix = if key.is_empty() { String::new() } else { format!("{key}/") };
        let mut out = Vec::new();
        let mut saw_marker = false;
        let mut marker: Option<String> = None;
        loop {
            let page = self.blob_page(container, &prefix, true, self.c.page_size, marker.as_deref(), &what)?;
            for p in &page.prefixes {
                let Some(rel) = p.strip_prefix(&prefix) else { continue };
                let rel = rel.strip_suffix('/').unwrap_or(rel);
                if usable(rel) {
                    out.push(Self::dir_entry(&dir.join(rel)));
                }
            }
            for b in &page.blobs {
                let Some(rel) = b.name.strip_prefix(&prefix) else { continue };
                if rel.is_empty() {
                    saw_marker = true;
                } else if usable(rel) {
                    out.push(Self::file_entry(dir.join(rel), b));
                }
            }
            match page.next_marker {
                Some(m) if Some(&m) != marker.as_ref() => marker = Some(m),
                _ => break,
            }
        }
        if out.is_empty() && !saw_marker && !key.is_empty() {
            // Nothing under the prefix: a file, or nothing at all.
            return Err(match self.head_blob(container, key, &what)? {
                Some(_) => Error::new(ErrorKind::InvalidPath, format!("Couldn't {what}: it's a file, not a folder.")),
                None => Error::new(ErrorKind::NotFound, format!("Couldn't {what}: the folder doesn't exist.")),
            });
        }
        Ok(out)
    }

    /// Get Blob Properties. `Ok(None)` when the blob doesn't exist (but the container does).
    pub(crate) fn head_blob(&self, container: &str, key: &str, what: &str) -> Result<Option<BlobItem>> {
        let resp = self.c.send_raw(&Call::new("HEAD", Some(container), Some(key)), &[])?;
        let code = header(&resp, "x-ms-error-code");
        if resp.status().as_u16() == 404 && code.as_deref() != Some("ContainerNotFound") {
            return Ok(None);
        }
        let resp = self.c.ok(resp, what)?;
        Ok(Some(BlobItem {
            name: key.to_string(),
            size: header(&resp, "content-length").and_then(|v| v.parse().ok()),
            modified: xml::http_date(header(&resp, "last-modified")),
            content_type: header(&resp, "content-type"),
            etag: header(&resp, "etag"),
            tier: header(&resp, "x-ms-access-tier"),
            content_md5: header(&resp, "content-md5"),
        }))
    }

    /// True if anything (other than `except`) has a name starting with `prefix`.
    pub(crate) fn has_blobs(&self, container: &str, prefix: &str, except: Option<&str>, what: &str) -> Result<bool> {
        let page = self.blob_page(container, prefix, false, 2, None, what)?;
        Ok(page.blobs.iter().any(|b| Some(b.name.as_str()) != except))
    }

    fn stat_container(&self, path: &RPath, container: &str) -> Result<Entry> {
        let what = format!("open {}", quoted(path));
        let mut e = Self::dir_entry(path);
        if matches!(self.c.cfg.credential, crate::config::Credential::SharedKey(_)) {
            let call = Call::new("HEAD", Some(container), None).q("restype", "container");
            let resp = self.c.send(&call, &[], &what)?;
            e.modified = xml::http_date(header(&resp, "last-modified"));
        } else {
            // A container SAS or public access may not allow Get Container Properties.
            self.blob_page(container, "", false, 1, None, &what)?;
        }
        Ok(e)
    }
}

/// A listed name that maps to one path component.
fn usable(rel: &str) -> bool {
    !rel.is_empty() && rel != "." && rel != ".." && !rel.contains('/')
}

impl Remote for AzureRemote {
    fn caps(&self) -> Caps {
        Caps { resume_read: true, real_dirs: false, server_copy: true, ranged_reads: true, ..Caps::default() }
    }

    fn home(&self) -> Result<RPath> {
        self.c.check_open()?;
        Ok(self.c.cfg.home.clone())
    }

    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        match self.loc(dir)? {
            Loc::Root => match &self.c.cfg.scoped {
                Some(only) => Ok(vec![Self::dir_entry(&RPath::root().join(only))]),
                None => self.list_containers(&format!("list the containers of {}", self.c.cfg.account)),
            },
            Loc::Container(c) => self.list_prefix(dir, &c, ""),
            Loc::Blob(c, k) => self.list_prefix(dir, &c, &k),
        }
    }

    fn stat(&self, path: &RPath) -> Result<Entry> {
        match self.loc(path)? {
            Loc::Root => Ok(Self::dir_entry(path)),
            Loc::Container(c) => self.stat_container(path, &c),
            Loc::Blob(c, k) => {
                let what = format!("get info for {}", quoted(path));
                if let Some(b) = self.head_blob(&c, &k, &what)? {
                    return Ok(Self::file_entry(path.clone(), &b));
                }
                let prefix = format!("{k}/");
                let page = self.blob_page(&c, &prefix, false, 1, None, &what)?;
                match page.blobs.first() {
                    Some(first) => {
                        let mut e = Self::dir_entry(path);
                        if first.name == prefix {
                            e.modified = first.modified;
                        }
                        Ok(e)
                    }
                    None => Err(Error::new(ErrorKind::NotFound, format!("Couldn't find {}.", quoted(path)))),
                }
            }
        }
    }

    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        let what = format!("download {}", quoted(path));
        let Loc::Blob(c, k) = self.loc(path)? else {
            return Err(Error::new(ErrorKind::InvalidPath, format!("Couldn't {what}: it's a folder.")));
        };
        let mut call = Call::new("GET", Some(&c), Some(&k));
        if offset > 0 {
            call = call.h("x-ms-range", format!("bytes={offset}-"));
        }
        let resp = self.c.send_raw(&call, &[])?;
        let status = resp.status().as_u16();
        if status == 416 && offset > 0 {
            // Resuming exactly at the end: nothing left to read.
            let total = header(&resp, "content-range")
                .and_then(|v| v.rsplit('/').next().and_then(|n| n.trim().parse::<u64>().ok()));
            if total == Some(offset) {
                return Ok(Box::new(io::empty()));
            }
        }
        let resp = self.c.ok(resp, &what)?;
        let status = resp.status().as_u16();
        if offset > 0 && status != 206 {
            return Err(Error::new(
                ErrorKind::Other,
                format!("Couldn't {what}: Azure didn't honour the resume position."),
            )
            .with_detail(format!("HTTP {status} for a ranged read from byte {offset}")));
        }
        let left = header(&resp, "content-length").and_then(|v| v.parse().ok());
        Ok(Box::new(Download {
            inner: filewinch_http::body_reader(resp),
            left,
            host: self.c.cfg.host.clone(),
        }))
    }

    fn read_range(&self, path: &RPath, offset: u64, len: u64) -> Result<Box<dyn Read + Send>> {
        let what = format!("download {}", quoted(path));
        let Loc::Blob(c, k) = self.loc(path)? else {
            return Err(Error::new(ErrorKind::InvalidPath, format!("Couldn't {what}: it's a folder.")));
        };
        let call = Call::new("GET", Some(&c), Some(&k)).h("x-ms-range", filewinch_core::range_header(offset, len));
        let resp = self.c.ok(self.c.send_raw(&call, &[])?, &what)?;
        let status = resp.status().as_u16();
        let n = filewinch_core::check_range_reply(status, header(&resp, "content-range").as_deref(), offset, len)?;
        let left = header(&resp, "content-length").and_then(|v| v.parse().ok());
        let body = Download { inner: filewinch_http::body_reader(resp), left, host: self.c.cfg.host.clone() };
        Ok(filewinch_core::exact_reader(Box::new(body), n, &quoted(path)))
    }

    fn write(&self, path: &RPath, offset: u64, size: Option<u64>) -> Result<Box<dyn WriteSink>> {
        crate::upload::open(self, path, offset, size)
    }

    fn mkdir(&self, path: &RPath) -> Result<()> {
        crate::ops::mkdir(self, path)
    }

    fn rename(&self, _from: &RPath, _to: &RPath) -> Result<()> {
        self.c.check_open()?;
        Err(Error::unsupported("renaming (Azure Blob Storage copies, then deletes)"))
    }

    fn remove_file(&self, path: &RPath) -> Result<()> {
        crate::ops::remove_file(self, path)
    }

    fn remove_dir(&self, path: &RPath) -> Result<()> {
        crate::ops::remove_dir(self, path)
    }

    fn copy(&self, from: &RPath, to: &RPath) -> Result<()> {
        crate::ops::copy(self, from, to)
    }

    fn close(&self) {
        self.c.close();
    }
}

/// A download body: maps read errors to core errors and treats an early end as a lost
/// connection.
struct Download {
    inner: Box<dyn Read + Send>,
    left: Option<u64>,
    host: String,
}

impl Read for Download {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        match self.inner.read(buf) {
            Ok(0) if self.left.is_some_and(|l| l > 0) => Err(to_io(Error::new(
                ErrorKind::Network,
                format!("The connection to {} was lost during the download.", self.host),
            ))),
            Ok(n) => {
                if let Some(l) = &mut self.left {
                    *l = l.saturating_sub(n as u64);
                }
                Ok(n)
            }
            Err(e) => Err(to_io(network(from_io(e), &self.host))),
        }
    }
}
