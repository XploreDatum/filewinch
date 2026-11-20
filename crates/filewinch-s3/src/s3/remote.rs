//! `Remote` for S3: listing, stat, read, write, folders, delete and server-side copy.

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;
use std::time::SystemTime;

use filewinch_core::{Caps, Entry, EntryKind, Error, ErrorKind, RPath, Remote, Result, WriteSink};
use filewinch_http::ureq::{Body, http::Response};

use super::client::{Call, Client};
use super::endpoint::DEFAULT_REGION;
use super::listing::{self, Object};
use super::multipart::{Multipart, part_size_for};
use super::upload::S3Sink;
use super::{S3Remote, split};
use crate::checksum;
use crate::sigv4::encode_path;
use crate::xml;

fn entry(path: &RPath, kind: EntryKind, size: Option<u64>, modified: Option<SystemTime>) -> Entry {
    let name = if path.is_root() { "/".to_string() } else { path.name().to_string() };
    Entry {
        name,
        path: path.clone(),
        kind,
        size,
        modified,
        mode: None,
        owner: None,
        group: None,
        link_target: None,
        extra: Vec::new(), checksums: Vec::new(), shared: None,
    }
}

fn object_entry(path: &RPath, o: &Object) -> Entry {
    let mut e = entry(path, EntryKind::File, Some(o.size), o.modified);
    if let Some(t) = &o.etag {
        e.extra.push(("ETag".into(), t.clone()));
        // Listings don't say how an object is encrypted, so an SSE-KMS/SSE-C object's opaque
        // 32-hex ETag passes as an MD5 here. That can only make sync see a difference and
        // copy again (a random value won't equal a local MD5); it can't make it skip a copy.
        e.checksums.extend(checksum::s3_etag_md5(t));
    }
    if let Some(c) = &o.storage_class {
        e.extra.push(("Storage class".into(), c.clone()));
    }
    e
}

/// A name S3 allows but a path can't hold.
fn representable(name: &str) -> bool {
    !(name.is_empty() || name == "." || name == ".." || name.contains('/'))
}

fn need_key<'a>(path: &'a RPath, what: &str) -> Result<(&'a str, &'a str)> {
    match split(path) {
        (Some(b), k) if !k.is_empty() => Ok((b, k)),
        _ => Err(Error::new(ErrorKind::InvalidPath, format!("Couldn't {what}: files must be inside a bucket."))),
    }
}

fn header<'a>(resp: &'a Response<Body>, name: &str) -> Option<&'a str> {
    resp.headers().get(name).and_then(|v| v.to_str().ok())
}

impl S3Remote {
    fn c(&self) -> &Arc<Client> {
        &self.client
    }

    fn list_buckets_as_entries(&self) -> Result<Vec<Entry>> {
        let buckets = match listing::list_buckets(self.c()) {
            Ok(b) => b,
            // A key limited to one bucket can't list buckets: show the site's bucket.
            Err(e) if e.kind == ErrorKind::PermissionDenied && self.initial_bucket().is_some() => {
                vec![listing::Bucket { name: self.initial_bucket().unwrap_or_default().into(), created: None }]
            }
            Err(e) => return Err(e),
        };
        Ok(buckets
            .into_iter()
            .filter(|b| representable(&b.name))
            .map(|b| entry(&RPath::root().join(&b.name), EntryKind::Dir, None, b.created))
            .collect())
    }

    /// HEAD of an object: Some(entry) if it exists, None on 404.
    fn head_object(&self, path: &RPath, bucket: &str, key: &str, what: &str) -> Result<Option<Entry>> {
        match self.c().send(&Call::new("HEAD", Some(bucket), key), what) {
            Ok(resp) => {
                let size = header(&resp, "content-length").and_then(|v| v.trim().parse().ok());
                let modified = header(&resp, "last-modified").and_then(|v| httpdate::parse_http_date(v).ok());
                let mut e = entry(path, EntryKind::File, size, modified);
                for (h, label) in [("etag", "ETag"), ("content-type", "Content type"), ("x-amz-storage-class", "Storage class")] {
                    if let Some(v) = header(&resp, h) {
                        e.extra.push((label.into(), v.to_string()));
                    }
                }
                let opaque = checksum::s3_etag_is_opaque(
                    header(&resp, "x-amz-server-side-encryption"),
                    header(&resp, "x-amz-server-side-encryption-customer-algorithm"),
                );
                if !opaque {
                    e.checksums.extend(header(&resp, "etag").and_then(checksum::s3_etag_md5));
                }
                Ok(Some(e))
            }
            Err(e) if e.kind == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Whether anything (marker or any key) exists under `key/`.
    fn folder_exists(&self, bucket: &str, key: &str, what: &str) -> Result<bool> {
        let page = listing::list_page(self.c(), bucket, &format!("{key}/"), false, None, Some(1), what)?;
        Ok(!page.objects.is_empty() || !page.prefixes.is_empty())
    }

    fn create_bucket(&self, bucket: &str) -> Result<()> {
        let what = format!("create the bucket “{bucket}”");
        let region = self.c().default_region().to_string();
        let doc;
        let mut call = Call::new("PUT", Some(bucket), "");
        // us-east-1 is the default and rejects an explicit constraint; R2's "auto" has none.
        if region != DEFAULT_REGION && region != "auto" {
            doc = format!(
                "<CreateBucketConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                 <LocationConstraint>{}</LocationConstraint></CreateBucketConfiguration>",
                xml::escape(&region)
            );
            call = call.header("content-type", "application/xml").body(doc.as_bytes());
        }
        self.c().set_region(bucket, &region);
        self.c().send(&call, &what).map(drop)
    }

    fn copy_object(&self, from: (&str, &str), to: (&str, &str), size: u64, what: &str) -> Result<()> {
        let source = format!("/{}", encode_path(&format!("{}/{}", from.0, from.1)));
        if size <= self.c().limits.copy_max {
            let call = Call::new("PUT", Some(to.0), to.1).header("x-amz-copy-source", source);
            let resp = self.c().send(&call, what)?;
            let body = self.c().text(resp, what)?;
            return self.c().check_body_error(&body, what);
        }
        // Too big for one CopyObject: multipart copy in ranges.
        let mut mp = Multipart::create(self.c().clone(), to.0, to.1, what)?;
        let part = part_size_for(size, self.c().limits.copy_part, self.c().limits.max_parts);
        let result = (|| {
            let mut start = 0u64;
            while start < size {
                let end = (start + part).min(size) - 1;
                let n = mp.next_part();
                let call = mp
                    .part_call(n)
                    .header("x-amz-copy-source", source.clone())
                    .header("x-amz-copy-source-range", format!("bytes={start}-{end}"));
                let resp = self.c().send(&call, what)?;
                let body = self.c().text(resp, what)?;
                self.c().check_body_error(&body, what)?;
                let etag = xml::parse(&body)
                    .ok()
                    .and_then(|r| r.text_of("ETag").map(|t| t.trim().to_string()))
                    .filter(|t| !t.is_empty())
                    .ok_or_else(|| {
                        Error::new(ErrorKind::Other, format!("Couldn't {what}: the server didn't confirm a copied part."))
                    })?;
                mp.parts.push((n, etag));
                start = end + 1;
            }
            mp.complete(what)
        })();
        if result.is_err() {
            mp.abort_in_background();
        }
        result
    }
}

impl Remote for S3Remote {
    fn caps(&self) -> Caps {
        Caps { resume_read: true, server_copy: true, ..Caps::default() }
    }

    fn home(&self) -> Result<RPath> {
        self.c().check_open()?;
        Ok(self.home.clone())
    }

    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        self.c().check_open()?;
        let (bucket, key) = split(dir);
        let Some(bucket) = bucket else { return self.list_buckets_as_entries() };
        let prefix = if key.is_empty() { String::new() } else { format!("{key}/") };
        let what = format!("list {dir}");
        let page = listing::list_all(self.c(), bucket, &prefix, &what)?;
        let mut seen_self = false;
        let mut out: BTreeMap<(String, bool), Entry> = BTreeMap::new();
        for o in &page.objects {
            let Some(name) = o.key.strip_prefix(&prefix) else { continue };
            if name.is_empty() {
                seen_self = true; // the folder's own marker
                continue;
            }
            if representable(name) {
                out.insert((name.to_string(), false), object_entry(&dir.join(name), o));
            }
        }
        for p in &page.prefixes {
            let Some(name) = p.strip_prefix(&prefix) else { continue };
            let name = name.strip_suffix('/').unwrap_or(name);
            if representable(name) {
                out.entry((name.to_string(), true)).or_insert_with(|| entry(&dir.join(name), EntryKind::Dir, None, None));
            }
        }
        if !key.is_empty() && !seen_self && page.objects.is_empty() && page.prefixes.is_empty() {
            return Err(Error::new(ErrorKind::NotFound, format!("Couldn't list {dir}: the folder doesn't exist.")));
        }
        Ok(out.into_values().collect())
    }

    fn stat(&self, path: &RPath) -> Result<Entry> {
        self.c().check_open()?;
        let what = format!("get info for {path}");
        match split(path) {
            (None, _) => Ok(entry(path, EntryKind::Dir, None, None)),
            (Some(bucket), "") => {
                self.c().send(&Call::new("HEAD", Some(bucket), ""), &what).map_err(|e| {
                    if e.kind == ErrorKind::NotFound {
                        Error::new(ErrorKind::NotFound, format!("The bucket “{bucket}” doesn't exist."))
                    } else {
                        e
                    }
                })?;
                Ok(entry(path, EntryKind::Dir, None, None))
            }
            (Some(bucket), key) => {
                if let Some(e) = self.head_object(path, bucket, key, &what)? {
                    return Ok(e);
                }
                if self.folder_exists(bucket, key, &what)? {
                    return Ok(entry(path, EntryKind::Dir, None, None));
                }
                Err(Error::new(ErrorKind::NotFound, format!("“{}” doesn't exist.", path.name())))
            }
        }
    }

    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        self.c().check_open()?;
        let what = format!("download {path}");
        let (bucket, key) = need_key(path, &what)?;
        let mut call = Call::new("GET", Some(bucket), key);
        if offset > 0 {
            call = call.header("range", format!("bytes={offset}-"));
        }
        let resp = match self.c().send(&call, &what) {
            Ok(r) => r,
            // Resuming exactly at the end: nothing left to read.
            Err(e) if offset > 0 && e.kind == ErrorKind::InvalidPath => {
                let size = self.head_object(path, bucket, key, &what)?.and_then(|e| e.size);
                if size == Some(offset) {
                    return Ok(Box::new(std::io::empty()));
                }
                return Err(e);
            }
            Err(e) => return Err(e),
        };
        crate::body::ranged_reader(resp, offset, what, self.c().host())
    }

    fn write(&self, path: &RPath, offset: u64, size: Option<u64>) -> Result<Box<dyn WriteSink>> {
        self.c().check_open()?;
        let what = format!("upload {path}");
        if offset > 0 {
            return Err(Error::unsupported("resuming uploads"));
        }
        let (bucket, key) = need_key(path, &what)?;
        Ok(Box::new(S3Sink::start(self.c().clone(), bucket, key, size, what)?))
    }

    fn mkdir(&self, path: &RPath) -> Result<()> {
        self.c().check_open()?;
        match split(path) {
            (None, _) => Err(Error::new(ErrorKind::AlreadyExists, "The top level already exists.")),
            (Some(bucket), "") => self.create_bucket(bucket),
            (Some(bucket), key) => {
                let marker = format!("{key}/");
                self.c().send(&Call::new("PUT", Some(bucket), &marker), &format!("create the folder {path}")).map(drop)
            }
        }
    }

    fn rename(&self, _from: &RPath, _to: &RPath) -> Result<()> {
        Err(Error::unsupported("renaming in place (Filewinch copies, then deletes)"))
    }

    fn remove_file(&self, path: &RPath) -> Result<()> {
        self.c().check_open()?;
        let what = format!("delete {path}");
        let (bucket, key) = need_key(path, &what)?;
        // S3 answers 204 for missing keys too; look first so a missing file reports NotFound.
        if self.head_object(path, bucket, key, &what)?.is_none() {
            return Err(Error::new(ErrorKind::NotFound, format!("Couldn't {what}: it doesn't exist.")));
        }
        self.c().send(&Call::new("DELETE", Some(bucket), key), &what).map(drop)
    }

    fn remove_dir(&self, path: &RPath) -> Result<()> {
        self.c().check_open()?;
        let what = format!("delete the folder {path}");
        match split(path) {
            (None, _) => Err(Error::new(ErrorKind::InvalidPath, "The top level can't be deleted.")),
            (Some(bucket), "") => self.c().send(&Call::new("DELETE", Some(bucket), ""), &what).map(drop),
            (Some(bucket), key) => {
                let marker = format!("{key}/");
                let page = listing::list_page(self.c(), bucket, &marker, false, None, Some(2), &what)?;
                if page.objects.iter().any(|o| o.key != marker) || !page.prefixes.is_empty() {
                    return Err(Error::new(ErrorKind::InvalidPath, format!("Couldn't {what}: it isn't empty.")));
                }
                // A folder without a marker (implied by keys since deleted) is already gone.
                if page.objects.is_empty() {
                    return Ok(());
                }
                self.c().send(&Call::new("DELETE", Some(bucket), &marker), &what).map(drop)
            }
        }
    }

    fn copy(&self, from: &RPath, to: &RPath) -> Result<()> {
        self.c().check_open()?;
        let what = format!("copy {from} to {to}");
        let (fb, fk) = need_key(from, &what)?;
        let (tb, tk) = need_key(to, &what)?;
        match self.head_object(from, fb, fk, &what)? {
            Some(src) => self.copy_object((fb, fk), (tb, tk), src.size.unwrap_or(0), &what),
            None if self.folder_exists(fb, fk, &what)? => {
                // A folder: the engine copies its contents; here only the marker.
                self.c().send(&Call::new("PUT", Some(tb), &format!("{tk}/")), &what).map(drop)
            }
            None => Err(Error::new(ErrorKind::NotFound, format!("Couldn't {what}: “{}” doesn't exist.", from.name()))),
        }
    }

    fn close(&self) {
        self.c().close();
    }
}
