//! The [`Remote`] implementation: listing, stat, MKCOL, MOVE/COPY, DELETE, and the
//! transfer entry points.

use std::io::Read;
use std::sync::Arc;

use filewinch_core::{Caps, Entry, EntryKind, Error, ErrorKind, RPath, Remote, Result, WriteSink};

use crate::client::Client;
use crate::multistatus::PropEntry;
use crate::target::href_segments;
use crate::transfer;

pub(crate) struct WebDavRemote {
    pub client: Arc<Client>,
    pub home: RPath,
}

fn entry(name: String, path: RPath, p: &PropEntry) -> Entry {
    let mut extra = Vec::new();
    if let Some(t) = &p.content_type {
        extra.push(("Content type".to_string(), t.clone()));
    }
    if let Some(e) = &p.etag {
        extra.push(("ETag".to_string(), e.clone()));
    }
    Entry {
        name,
        path,
        kind: if p.is_dir { EntryKind::Dir } else { EntryKind::File },
        size: if p.is_dir { None } else { p.size },
        modified: p.modified,
        mode: None,
        owner: None,
        group: None,
        link_target: None,
        extra, checksums: Vec::new(), shared: None,
    }
}

fn root_refused(what: &str) -> Error {
    Error::new(ErrorKind::InvalidPath, format!("Couldn't {what}: that's the top folder of this site."))
}

impl WebDavRemote {
    /// Lists `dir` (Depth 1). Returns the folder's own entry (if the server sent it) and its
    /// children.
    fn list_raw(&self, dir: &RPath, what: &str) -> Result<(Option<PropEntry>, Vec<Entry>)> {
        let c = &self.client;
        let resps = c.propfind(dir, 1, true, what)?;
        let want = c.target.segments(dir);
        let segs: Vec<Vec<String>> = resps.iter().map(|r| href_segments(&r.href)).collect();
        // The folder itself: the href that decodes to the request path. If a proxy rewrote
        // the prefix, fall back to the one entry that is one level shallower than the rest.
        let mut self_idx = segs.iter().position(|s| *s == want);
        if self_idx.is_none() && !segs.is_empty() {
            let min = segs.iter().map(Vec::len).min().unwrap_or(0);
            let shallow: Vec<usize> = (0..segs.len()).filter(|&i| segs[i].len() == min).collect();
            let rest_deeper = segs.iter().all(|s| s.len() == min || s.len() == min + 1);
            let same_tail = |i: usize| segs[i].last() == want.last();
            if shallow.len() == 1 && rest_deeper && (segs.len() > 1 || same_tail(shallow[0])) {
                self_idx = Some(shallow[0]);
            }
        }
        let mut entries = Vec::with_capacity(resps.len());
        for (i, (r, s)) in resps.iter().zip(&segs).enumerate() {
            if Some(i) == self_idx {
                continue;
            }
            let Some(name) = s.last() else { continue };
            if name.is_empty() || name == "." || name == ".." || name.contains('/') {
                continue;
            }
            entries.push(entry(name.clone(), dir.join(name), r));
        }
        let me = self_idx.map(|i| resps[i].clone());
        if me.as_ref().is_some_and(|m| !m.is_dir) {
            return Err(Error::new(
                ErrorKind::InvalidPath,
                format!("Couldn't {what}: it's a file, not a folder."),
            ));
        }
        Ok((me, entries))
    }

    /// MOVE or COPY with `Overwrite: F`. Folders get trailing slashes on both ends (nginx
    /// requires them to match).
    fn transfer(&self, method: &str, from: &RPath, to: &RPath, verb: &str) -> Result<()> {
        let what = format!("{verb} {}", from.name());
        if from.is_root() || to.is_root() {
            return Err(root_refused(&what));
        }
        let is_dir = self.stat(from)?.is_dir();
        let t = &self.client.target;
        let dest = t.url(&t.request_path(to, is_dir));
        let src = t.request_path(from, is_dir);
        let headers = [("destination", dest.as_str()), ("overwrite", "F")];
        let resp = self.client.send(method, &src, &headers, "")?;
        match resp.status().as_u16() {
            207 => Err(Error::new(ErrorKind::Other, format!("Couldn't {what}: some items failed on the server."))
                .with_detail("HTTP 207")),
            200..=299 => Ok(()),
            412 => Err(Error::new(
                ErrorKind::AlreadyExists,
                format!("Couldn't {what}: an item named “{}” already exists there.", to.name()),
            )
            .with_detail("HTTP 412")),
            409 => Err(Error::new(
                ErrorKind::NotFound,
                format!("Couldn't {what}: the destination folder doesn't exist."),
            )
            .with_detail("HTTP 409")),
            _ => Err(self.client.fail(resp, &what)),
        }
    }

    fn delete(&self, path: &RPath, collection: bool, what: &str) -> Result<()> {
        let rp = self.client.target.request_path(path, collection);
        let resp = self.client.send("DELETE", &rp, &[], "")?;
        match resp.status().as_u16() {
            207 => Err(Error::new(ErrorKind::Other, format!("Couldn't {what}: some items couldn't be deleted."))
                .with_detail("HTTP 207")),
            200..=299 => Ok(()),
            _ => Err(self.client.fail(resp, what)),
        }
    }
}

impl Remote for WebDavRemote {
    fn caps(&self) -> Caps {
        Caps {
            resume_read: true,
            resume_write: false,
            permissions: false,
            set_mtime: false,
            symlinks: false,
            real_dirs: true,
            server_rename: true,
            server_copy: true,
        }
    }

    fn home(&self) -> Result<RPath> {
        self.client.check_open()?;
        Ok(self.home.clone())
    }

    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        Ok(self.list_raw(dir, &format!("list {dir}"))?.1)
    }

    fn stat(&self, path: &RPath) -> Result<Entry> {
        let what = format!("get info for {path}");
        let resps = self.client.propfind(path, 0, path.is_root(), &what)?;
        let want = self.client.target.segments(path);
        let p = resps
            .iter()
            .find(|r| href_segments(&r.href) == want)
            .or(resps.first())
            .ok_or_else(|| {
                Error::new(ErrorKind::NotFound, format!("Couldn't {what}: the server says it wasn't found."))
            })?;
        let name = if path.is_root() { "/".to_string() } else { path.name().to_string() };
        Ok(entry(name, path.clone(), p))
    }

    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        transfer::open_read(&self.client, path, offset)
    }

    fn write(&self, path: &RPath, offset: u64, size: Option<u64>) -> Result<Box<dyn WriteSink>> {
        if offset > 0 {
            return Err(Error::unsupported("resuming uploads"));
        }
        if path.is_root() {
            return Err(root_refused(&format!("upload to {path}")));
        }
        transfer::open_write(self.client.clone(), path, size)
    }

    fn mkdir(&self, path: &RPath) -> Result<()> {
        let what = format!("create folder {}", path.name());
        if path.is_root() {
            return Err(Error::new(ErrorKind::AlreadyExists, format!("Couldn't {what}: it already exists.")));
        }
        let rp = self.client.target.request_path(path, true);
        let resp = self.client.send("MKCOL", &rp, &[], "")?;
        match resp.status().as_u16() {
            200..=299 => Ok(()),
            405 => Err(Error::new(
                ErrorKind::AlreadyExists,
                format!("Couldn't {what}: something with that name already exists."),
            )
            .with_detail("HTTP 405")),
            409 => Err(Error::new(
                ErrorKind::NotFound,
                format!("Couldn't {what}: the parent folder doesn't exist."),
            )
            .with_detail("HTTP 409")),
            _ => Err(self.client.fail(resp, &what)),
        }
    }

    fn rename(&self, from: &RPath, to: &RPath) -> Result<()> {
        self.transfer("MOVE", from, to, "rename")
    }

    fn copy(&self, from: &RPath, to: &RPath) -> Result<()> {
        self.transfer("COPY", from, to, "copy")
    }

    fn remove_file(&self, path: &RPath) -> Result<()> {
        let what = format!("delete {}", path.name());
        if path.is_root() {
            return Err(root_refused(&what));
        }
        // WebDAV DELETE of a folder is recursive: never let a "file" delete take a folder.
        if self.stat(path)?.is_dir() {
            return Err(Error::new(ErrorKind::InvalidPath, format!("Couldn't {what}: it's a folder, not a file.")));
        }
        self.delete(path, false, &what)
    }

    fn remove_dir(&self, path: &RPath) -> Result<()> {
        let what = format!("delete folder {}", path.name());
        if path.is_root() {
            return Err(root_refused(&what));
        }
        // WebDAV DELETE is recursive; only delete a folder we've just seen empty.
        let (me, children) = self.list_raw(path, &what)?;
        if me.is_none() && !self.stat(path)?.is_dir() {
            return Err(Error::new(ErrorKind::InvalidPath, format!("Couldn't {what}: it's a file, not a folder.")));
        }
        if !children.is_empty() {
            return Err(Error::new(ErrorKind::InvalidPath, format!("Couldn't {what}: it isn't empty."))
                .with_detail(format!("{} items inside", children.len())));
        }
        self.delete(path, true, &what)
    }

    fn close(&self) {
        self.client.close();
    }
}
