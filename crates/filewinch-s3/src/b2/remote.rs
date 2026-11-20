//! `Remote` for B2: listing, stat, download, folders, delete (every version) and copy.

use std::collections::BTreeMap;
use std::io::Read;

use filewinch_core::{Caps, Entry, EntryKind, Error, ErrorKind, RPath, Remote, Result, WriteSink};
use serde_json::{Value, json};

use super::api::{Failure, host_of, json_or_failure};
use super::upload::{B2Sink, upload_bytes};
use super::{B2Remote, MARKER};
use crate::checksum::b2_content_sha1;
use crate::s3::{part_size_for, split};
use crate::sigv4::encode_path;
use crate::time::from_millis;

fn entry(path: &RPath, kind: EntryKind) -> Entry {
    Entry {
        name: if path.is_root() { "/".into() } else { path.name().to_string() },
        path: path.clone(),
        kind,
        size: None,
        modified: None,
        mode: None,
        owner: None,
        group: None,
        link_target: None,
        extra: Vec::new(), checksums: Vec::new(), shared: None,
    }
}

fn file_entry(path: &RPath, f: &Value) -> Entry {
    let mut e = entry(path, EntryKind::File);
    e.size = f["contentLength"].as_u64();
    let millis = f["fileInfo"]["src_last_modified_millis"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| f["uploadTimestamp"].as_u64());
    e.modified = millis.map(from_millis);
    for (k, label) in [("contentType", "Content type"), ("contentSha1", "SHA-1")] {
        if let Some(v) = f[k].as_str().filter(|v| *v != "none") {
            e.extra.push((label.into(), v.to_string()));
        }
    }
    e.checksums.extend(b2_content_sha1(f["contentSha1"].as_str()));
    e
}

fn representable(name: &str) -> bool {
    !(name.is_empty() || name == "." || name == ".." || name.contains('/'))
}

fn need_name<'a>(path: &'a RPath, what: &str) -> Result<(&'a str, &'a str)> {
    match split(path) {
        (Some(b), k) if !k.is_empty() => Ok((b, k)),
        _ => Err(Error::new(ErrorKind::InvalidPath, format!("Couldn't {what}: files must be inside a bucket."))),
    }
}

impl B2Remote {
    fn bucket_entries(&self) -> Result<Vec<Entry>> {
        let acct = self.session.account();
        if let Some((_, name)) = &acct.bucket {
            return Ok(vec![entry(&RPath::root().join(name), EntryKind::Dir)]);
        }
        let v = self.session.call("b2_list_buckets", &json!({"accountId": acct.account_id}), "list the buckets", true)?;
        let mut out = Vec::new();
        for b in v["buckets"].as_array().into_iter().flatten() {
            let (Some(name), Some(id)) = (b["bucketName"].as_str(), b["bucketId"].as_str()) else { continue };
            self.session.remember_bucket(name, id);
            if representable(name) {
                out.push(entry(&RPath::root().join(name), EntryKind::Dir));
            }
        }
        Ok(out)
    }

    /// One `b2_list_file_names` page: (files, nextFileName).
    fn names_page(
        &self,
        bucket_id: &str,
        prefix: &str,
        delimiter: bool,
        start: Option<&str>,
        max: u32,
        what: &str,
    ) -> Result<(Vec<Value>, Option<String>)> {
        let mut body = json!({"bucketId": bucket_id, "prefix": prefix, "maxFileCount": max});
        if delimiter {
            body["delimiter"] = json!("/");
        }
        if let Some(s) = start {
            body["startFileName"] = json!(s);
        }
        let v = self.session.call("b2_list_file_names", &body, what, true)?;
        let files = v["files"].as_array().cloned().unwrap_or_default();
        let next = v["nextFileName"].as_str().map(str::to_string);
        Ok((files, next))
    }

    /// The current version of exactly `name`, if it exists.
    fn find_file(&self, bucket_id: &str, name: &str, what: &str) -> Result<Option<Value>> {
        let (files, _) = self.names_page(bucket_id, name, false, Some(name), 1, what)?;
        Ok(files.into_iter().find(|f| f["fileName"] == name && f["action"] == "upload"))
    }

    fn folder_exists(&self, bucket_id: &str, name: &str, what: &str) -> Result<bool> {
        let (files, _) = self.names_page(bucket_id, &format!("{name}/"), false, None, 1, what)?;
        Ok(!files.is_empty())
    }

    /// Deletes every version of `name`; returns how many there were.
    fn delete_all_versions(&self, bucket_id: &str, name: &str, what: &str) -> Result<usize> {
        let mut deleted = 0;
        let mut start: (String, Option<String>) = (name.to_string(), None);
        loop {
            let mut body = json!({"bucketId": bucket_id, "prefix": name, "startFileName": start.0, "maxFileCount": 1000});
            if let Some(id) = &start.1 {
                body["startFileId"] = json!(id);
            }
            let v = self.session.call("b2_list_file_versions", &body, what, true)?;
            for f in v["files"].as_array().into_iter().flatten().filter(|f| f["fileName"] == name) {
                let Some(id) = f["fileId"].as_str() else { continue };
                let del = json!({"fileName": name, "fileId": id});
                match self.session.call("b2_delete_file_version", &del, what, true) {
                    Ok(_) => deleted += 1,
                    // Gone already (another client, or our own retried call).
                    Err(e) if e.kind == ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
            match (v["nextFileName"].as_str(), v["nextFileId"].as_str()) {
                (Some(n), id) if n == name => start = (n.to_string(), id.map(str::to_string)),
                _ => return Ok(deleted),
            }
        }
    }

    fn copy_large(&self, src: &Value, to_bucket: &str, to: &str, size: u64, what: &str) -> Result<()> {
        let source_id = src["fileId"].as_str().unwrap_or_default();
        let content_type = src["contentType"].as_str().unwrap_or("b2/x-auto");
        let start = json!({"bucketId": to_bucket, "fileName": to, "contentType": content_type});
        let v = self.session.call("b2_start_large_file", &start, what, false)?;
        let file_id = v["fileId"].as_str().unwrap_or_default().to_string();
        let part = part_size_for(size, self.session.limits.copy_part, self.session.limits.max_parts);
        let result = (|| {
            let mut sha1s = Vec::new();
            let mut offset = 0u64;
            while offset < size {
                let end = (offset + part).min(size) - 1;
                let body = json!({
                    "sourceFileId": source_id, "largeFileId": file_id,
                    "partNumber": sha1s.len() + 1, "range": format!("bytes={offset}-{end}"),
                });
                let v = self.session.call("b2_copy_part", &body, what, true)?;
                sha1s.push(v["contentSha1"].as_str().unwrap_or_default().to_string());
                offset = end + 1;
            }
            let finish = json!({"fileId": file_id, "partSha1Array": sha1s});
            self.session.call("b2_finish_large_file", &finish, what, true).map(drop)
        })();
        if result.is_err() {
            let _ = self.session.call("b2_cancel_large_file", &json!({"fileId": file_id}), "cancel the copy", true);
        }
        result
    }
}

impl Remote for B2Remote {
    fn caps(&self) -> Caps {
        Caps { resume_read: true, server_copy: true, ranged_reads: true, ..Caps::default() }
    }

    fn home(&self) -> Result<RPath> {
        self.session.check_open()?;
        Ok(self.home.clone())
    }

    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        self.session.check_open()?;
        let (bucket, key) = split(dir);
        let Some(bucket) = bucket else { return self.bucket_entries() };
        let what = format!("list {dir}");
        let bucket_id = self.session.bucket_id(bucket)?;
        let prefix = if key.is_empty() { String::new() } else { format!("{key}/") };
        let mut out: BTreeMap<(String, bool), Entry> = BTreeMap::new();
        let (mut any, mut start) = (false, None::<String>);
        loop {
            let (files, next) = self.names_page(&bucket_id, &prefix, true, start.as_deref(), 1000, &what)?;
            for f in &files {
                any = true;
                let Some(full) = f["fileName"].as_str() else { continue };
                let Some(name) = full.strip_prefix(prefix.as_str()) else { continue };
                match f["action"].as_str() {
                    Some("folder") => {
                        let name = name.strip_suffix('/').unwrap_or(name);
                        if representable(name) {
                            out.insert((name.into(), true), entry(&dir.join(name), EntryKind::Dir));
                        }
                    }
                    Some("upload") if name != MARKER && !name.is_empty() && representable(name) => {
                        out.insert((name.into(), false), file_entry(&dir.join(name), f));
                    }
                    _ => {}
                }
            }
            match next {
                Some(n) if Some(&n) != start.as_ref() => start = Some(n),
                _ => break,
            }
        }
        if !key.is_empty() && !any {
            return Err(Error::new(ErrorKind::NotFound, format!("Couldn't {what}: the folder doesn't exist.")));
        }
        Ok(out.into_values().collect())
    }

    fn stat(&self, path: &RPath) -> Result<Entry> {
        self.session.check_open()?;
        let what = format!("get info for {path}");
        match split(path) {
            (None, _) => Ok(entry(path, EntryKind::Dir)),
            (Some(bucket), "") => self.session.bucket_id(bucket).map(|_| entry(path, EntryKind::Dir)),
            (Some(bucket), name) => {
                let id = self.session.bucket_id(bucket)?;
                if let Some(f) = self.find_file(&id, name, &what)? {
                    return Ok(file_entry(path, &f));
                }
                if self.folder_exists(&id, name, &what)? {
                    return Ok(entry(path, EntryKind::Dir));
                }
                Err(Error::new(ErrorKind::NotFound, format!("“{}” doesn't exist.", path.name())))
            }
        }
    }

    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        self.session.check_open()?;
        let what = format!("download {path}");
        let (bucket, name) = need_name(path, &what)?;
        let mut eof_case = false;
        let resp = self.session.with_auth(&what, |acct| {
            let url = format!("{}/file/{}/{}", acct.download_url, encode_path(bucket), encode_path(name));
            let host = host_of(&acct.download_url);
            let mut req = self.session.http.agent().get(&url).header("authorization", acct.token.as_str());
            if offset > 0 {
                req = req.header("range", format!("bytes={offset}-"));
            }
            let resp = req.call().map_err(|e| Failure::Error(self.session.http.transport_error(e, &host)))?;
            if resp.status().is_success() {
                return Ok(Some(resp));
            }
            match json_or_failure(resp, &what, &host) {
                Err(Failure::Error(e)) if offset > 0 && e.kind == ErrorKind::InvalidPath => {
                    eof_case = true;
                    Err(Failure::Error(e))
                }
                Err(f) => Err(f),
                Ok(_) => Ok(None),
            }
        });
        let resp = match resp {
            Ok(Some(r)) => r,
            Ok(None) => return Err(Error::new(ErrorKind::Other, format!("Couldn't {what}."))),
            Err(e) if eof_case => {
                // Resuming exactly at the end: nothing left to read.
                let size = self.stat(path)?.size;
                return if size == Some(offset) { Ok(Box::new(std::io::empty())) } else { Err(e) };
            }
            Err(e) => return Err(e),
        };
        let host = host_of(&self.session.account().download_url);
        crate::body::ranged_reader(resp, offset, what, host)
    }

    fn read_range(&self, path: &RPath, offset: u64, len: u64) -> Result<Box<dyn Read + Send>> {
        self.session.check_open()?;
        let what = format!("download {path}");
        let (bucket, name) = need_name(path, &what)?;
        let range = filewinch_core::range_header(offset, len);
        let resp = self.session.with_auth(&what, |acct| {
            let url = format!("{}/file/{}/{}", acct.download_url, encode_path(bucket), encode_path(name));
            let host = host_of(&acct.download_url);
            let req = self.session.http.agent().get(&url).header("authorization", acct.token.as_str()).header("range", range.as_str());
            let resp = req.call().map_err(|e| Failure::Error(self.session.http.transport_error(e, &host)))?;
            if resp.status().is_success() {
                return Ok(Some(resp));
            }
            json_or_failure(resp, &what, &host).map(|_| None)
        });
        let Some(resp) = resp? else { return Err(Error::new(ErrorKind::Other, format!("Couldn't {what}."))) };
        let host = host_of(&self.session.account().download_url);
        crate::body::exact_range(resp, offset, len, what, host)
    }

    fn write(&self, path: &RPath, offset: u64, size: Option<u64>) -> Result<Box<dyn WriteSink>> {
        self.session.check_open()?;
        let what = format!("upload {path}");
        if offset > 0 {
            return Err(Error::unsupported("resuming uploads"));
        }
        let (bucket, name) = need_name(path, &what)?;
        let id = self.session.bucket_id(bucket)?;
        Ok(Box::new(B2Sink::start(self.session.clone(), id, name.to_string(), size, what)?))
    }

    fn mkdir(&self, path: &RPath) -> Result<()> {
        self.session.check_open()?;
        let what = format!("create the folder {path}");
        match split(path) {
            (None, _) => Err(Error::new(ErrorKind::AlreadyExists, "The top level already exists.")),
            (Some(bucket), "") => {
                let acct = self.session.account();
                let body = json!({"accountId": acct.account_id, "bucketName": bucket, "bucketType": "allPrivate"});
                let v = self.session.call("b2_create_bucket", &body, &format!("create the bucket “{bucket}”"), false)?;
                if let Some(id) = v["bucketId"].as_str() {
                    self.session.remember_bucket(bucket, id);
                }
                Ok(())
            }
            (Some(bucket), name) => {
                let id = self.session.bucket_id(bucket)?;
                upload_bytes(&self.session, &id, &format!("{name}/{MARKER}"), &[], &what).map(drop)
            }
        }
    }

    fn rename(&self, _from: &RPath, _to: &RPath) -> Result<()> {
        Err(Error::unsupported("renaming in place (Filewinch copies, then deletes)"))
    }

    fn remove_file(&self, path: &RPath) -> Result<()> {
        self.session.check_open()?;
        let what = format!("delete {path}");
        let (bucket, name) = need_name(path, &what)?;
        let id = self.session.bucket_id(bucket)?;
        if self.delete_all_versions(&id, name, &what)? == 0 {
            return Err(Error::new(ErrorKind::NotFound, format!("Couldn't {what}: it doesn't exist.")));
        }
        Ok(())
    }

    fn remove_dir(&self, path: &RPath) -> Result<()> {
        self.session.check_open()?;
        let what = format!("delete the folder {path}");
        match split(path) {
            (None, _) => Err(Error::new(ErrorKind::InvalidPath, "The top level can't be deleted.")),
            (Some(bucket), "") => {
                let acct = self.session.account();
                let id = self.session.bucket_id(bucket)?;
                self.session.call("b2_delete_bucket", &json!({"accountId": acct.account_id, "bucketId": id}), &what, true)?;
                self.session.forget_bucket(bucket);
                Ok(())
            }
            (Some(bucket), name) => {
                let id = self.session.bucket_id(bucket)?;
                let prefix = format!("{name}/");
                let marker = format!("{prefix}{MARKER}");
                let (files, _) = self.names_page(&id, &prefix, false, None, 3, &what)?;
                let names: Vec<&str> = files.iter().filter_map(|f| f["fileName"].as_str()).collect();
                if names.iter().any(|n| *n != marker && *n != prefix) {
                    return Err(Error::new(ErrorKind::InvalidPath, format!("Couldn't {what}: it isn't empty.")));
                }
                for n in [marker.as_str(), prefix.as_str()] {
                    if names.contains(&n) {
                        self.delete_all_versions(&id, n, &what)?;
                    }
                }
                Ok(())
            }
        }
    }

    fn copy(&self, from: &RPath, to: &RPath) -> Result<()> {
        self.session.check_open()?;
        let what = format!("copy {from} to {to}");
        let (fb, fname) = need_name(from, &what)?;
        let (tb, tname) = need_name(to, &what)?;
        let from_id = self.session.bucket_id(fb)?;
        let to_id = self.session.bucket_id(tb)?;
        let Some(src) = self.find_file(&from_id, fname, &what)? else {
            if self.folder_exists(&from_id, fname, &what)? {
                // A folder: the engine copies its contents; here only the marker.
                return upload_bytes(&self.session, &to_id, &format!("{tname}/{MARKER}"), &[], &what).map(drop);
            }
            return Err(Error::new(ErrorKind::NotFound, format!("Couldn't {what}: “{}” doesn't exist.", from.name())));
        };
        let size = src["contentLength"].as_u64().unwrap_or(0);
        if size > self.session.limits.copy_max {
            return self.copy_large(&src, &to_id, tname, size, &what);
        }
        let mut body = json!({"sourceFileId": src["fileId"], "fileName": tname});
        if to_id != from_id {
            body["destinationBucketId"] = json!(to_id);
        }
        self.session.call("b2_copy_file", &body, &what, true).map(drop)
    }

    fn close(&self) {
        self.session.close();
    }
}
