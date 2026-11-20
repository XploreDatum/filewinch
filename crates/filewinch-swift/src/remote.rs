//! The [`Remote`] implementation.

use std::io::{self, Read};
use std::sync::Arc;

use filewinch_core::{Caps, Entry, Error, ErrorKind, RPath, Remote, Result, WriteSink};
use filewinch_http::to_io;

use crate::Tuning;
use crate::client::{Client, Loc, enc, url_path};
use crate::config::AuthConfig;
use crate::list::{is_slo, not_found};
use crate::upload::{SingleSink, SloSink};

/// A signed-in Swift account.
pub struct SwiftRemote {
    client: Arc<Client>,
    home: RPath,
}

impl SwiftRemote {
    pub(crate) fn connect(cfg: AuthConfig, initial: Option<&str>, tuning: Tuning) -> Result<SwiftRemote> {
        let client = Client::connect(cfg, tuning)?;
        let what = "open the Swift account";
        let resp = client.send("HEAD", "", &[], None, what)?;
        if !resp.status().is_success() {
            return Err(client.fail(resp, what));
        }
        let home = initial.map(RPath::new).unwrap_or_else(RPath::root);
        Ok(SwiftRemote { client: Arc::new(client), home })
    }

    fn object(path: &RPath, verb: &str) -> Result<(String, String)> {
        match Loc::of(path) {
            Loc::Object { container, key } => Ok((container, key)),
            _ => Err(Error::new(
                ErrorKind::InvalidPath,
                format!("Can't {verb} “{path}”: files live inside a container."),
            )),
        }
    }
}

/// A download body (read errors already mapped to core errors by `body_reader`) that
/// also fails if the body ends before its Content-Length.
struct Download {
    body: Box<dyn Read + Send>,
    left: Option<u64>,
}

impl Read for Download {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.body.read(buf)?;
        if let Some(left) = self.left.as_mut() {
            if n == 0 && *left > 0 && !buf.is_empty() {
                return Err(to_io(Error::new(ErrorKind::Network, "The download ended before the whole file arrived.")));
            }
            *left = left.saturating_sub(n as u64);
        }
        Ok(n)
    }
}

impl Remote for SwiftRemote {
    fn caps(&self) -> Caps {
        Caps { resume_read: true, server_copy: true, ranged_reads: true, ..Caps::default() }
    }

    fn home(&self) -> Result<RPath> {
        self.client.check_open()?;
        Ok(self.home.clone())
    }

    fn list(&self, dir: &RPath) -> Result<Vec<Entry>> {
        self.client.list_dir(dir)
    }

    fn stat(&self, path: &RPath) -> Result<Entry> {
        self.client.stat_path(path)
    }

    fn read(&self, path: &RPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        let (container, key) = Self::object(path, "download")?;
        let what = format!("download {}", path.name());
        let range = format!("bytes={offset}-");
        let headers: &[(&str, &str)] = if offset > 0 { &[("range", &range)] } else { &[] };
        let resp = self.client.send("GET", &url_path(&container, Some(&key)), headers, None, &what)?;
        let status = resp.status().as_u16();
        match status {
            200 if offset == 0 => {}
            206 if offset > 0 => {}
            200 => {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    format!("Couldn't resume {}: the server sent the whole file instead of the rest.", path.name()),
                ));
            }
            416 => {
                // Asking from exactly the end is an empty read, not an error.
                let size = self.client.head_object(&container, &key, &what)?.and_then(|e| e.size);
                if size == Some(offset) {
                    return Ok(Box::new(io::empty()));
                }
                return Err(self.client.fail(resp, &what));
            }
            404 => return Err(not_found(path)),
            _ => return Err(self.client.fail(resp, &what)),
        }
        let left = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        Ok(Box::new(Download { body: filewinch_http::body_reader(resp), left }))
    }

    fn read_range(&self, path: &RPath, offset: u64, len: u64) -> Result<Box<dyn Read + Send>> {
        let (container, key) = Self::object(path, "download")?;
        let what = format!("download {}", path.name());
        let range = filewinch_core::range_header(offset, len);
        let resp = self.client.send("GET", &url_path(&container, Some(&key)), &[("range", &range)], None, &what)?;
        let status = resp.status().as_u16();
        match status {
            200..=299 => {}
            404 => return Err(not_found(path)),
            _ => return Err(self.client.fail(resp, &what)),
        }
        let header = |name: &str| resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let n = filewinch_core::check_range_reply(status, header("content-range").as_deref(), offset, len)?;
        let left = header("content-length").and_then(|v| v.trim().parse::<u64>().ok());
        let body = Download { body: filewinch_http::body_reader(resp), left };
        Ok(filewinch_core::exact_reader(Box::new(body), n, &format!("“{}”", path.name())))
    }

    fn write(&self, path: &RPath, offset: u64, size: Option<u64>) -> Result<Box<dyn WriteSink>> {
        self.client.check_open()?;
        if offset > 0 {
            return Err(Error::unsupported("resuming uploads"));
        }
        let (container, key) = Self::object(path, "upload")?;
        let what = format!("upload {}", path.name());
        match size {
            Some(n) if n <= self.client.tuning.large_object_threshold => {
                Ok(Box::new(SingleSink::new(&self.client, &container, &key, n, what)))
            }
            _ => Ok(Box::new(SloSink::new(&self.client, &container, &key, size, what)?)),
        }
    }

    fn mkdir(&self, path: &RPath) -> Result<()> {
        let what = format!("create the folder {}", path.name());
        let exists = || Error::new(ErrorKind::AlreadyExists, format!("“{}” already exists.", path.name()));
        match Loc::of(path) {
            Loc::Root => Err(Error::new(ErrorKind::InvalidPath, "The top level can't be created.")),
            Loc::Container(c) => {
                let resp = self.client.send("PUT", &url_path(&c, None), &[], Some(b""), &what)?;
                match resp.status().as_u16() {
                    201 => Ok(()),
                    202 => Err(exists()),
                    _ => Err(self.client.fail(resp, &what)),
                }
            }
            Loc::Object { container, key } => {
                if self.client.head_object(&container, &key, &what)?.is_some()
                    || self.client.prefix_exists(&container, &format!("{key}/"), &what)?
                {
                    return Err(exists());
                }
                let marker = url_path(&container, Some(&format!("{key}/")));
                let resp = self.client.send(
                    "PUT",
                    &marker,
                    &[("content-type", "application/directory")],
                    Some(b""),
                    &what,
                )?;
                match resp.status().as_u16() {
                    200 | 201 => Ok(()),
                    404 => Err(Error::new(ErrorKind::NotFound, format!("The container “{container}” doesn't exist."))),
                    _ => Err(self.client.fail(resp, &what)),
                }
            }
        }
    }

    fn rename(&self, _from: &RPath, _to: &RPath) -> Result<()> {
        Err(Error::unsupported("renaming (Swift copies, then deletes)"))
    }

    fn remove_file(&self, path: &RPath) -> Result<()> {
        let (container, key) = Self::object(path, "delete")?;
        let what = format!("delete {}", path.name());
        let Some(entry) = self.client.head_object(&container, &key, &what)? else { return Err(not_found(path)) };
        let target = url_path(&container, Some(&key));
        if !is_slo(&entry) {
            let resp = self.client.send("DELETE", &target, &[], None, &what)?;
            return match resp.status().as_u16() {
                200 | 202 | 204 => Ok(()),
                404 => Err(not_found(path)),
                _ => Err(self.client.fail(resp, &what)),
            };
        }
        // A large object: delete the manifest and its segments in one request.
        let resp = self.client.send(
            "DELETE",
            &format!("{target}?multipart-manifest=delete"),
            &[("accept", "application/json")],
            None,
            &what,
        )?;
        if !resp.status().is_success() {
            return Err(self.client.fail(resp, &what));
        }
        #[derive(serde::Deserialize)]
        struct Bulk {
            #[serde(rename = "Response Status")]
            status: Option<String>,
            #[serde(rename = "Errors", default)]
            errors: Vec<serde_json::Value>,
        }
        let bulk: Bulk = self.client.json(resp, &what)?;
        let code: u16 = bulk.status.as_deref().and_then(|s| s.get(..3)?.parse().ok()).unwrap_or(200);
        if (200..300).contains(&code) || code == 404 {
            return Ok(());
        }
        let detail = serde_json::Value::Array(bulk.errors).to_string();
        Err(crate::client::swift_error(code, &Default::default(), &detail, &what))
    }

    fn remove_dir(&self, path: &RPath) -> Result<()> {
        let what = format!("delete the folder {}", path.name());
        let not_empty = || Error::new(ErrorKind::InvalidPath, format!("The folder “{}” isn't empty.", path.name()));
        match Loc::of(path) {
            Loc::Root => Err(Error::new(ErrorKind::InvalidPath, "The top level can't be deleted.")),
            Loc::Container(c) => {
                let resp = self.client.send("DELETE", &url_path(&c, None), &[], None, &what)?;
                match resp.status().as_u16() {
                    200 | 202 | 204 => Ok(()),
                    409 => Err(not_empty()),
                    404 => Err(not_found(path)),
                    _ => Err(self.client.fail(resp, &what)),
                }
            }
            Loc::Object { container, key } => {
                let prefix = format!("{key}/");
                match self.client.children_besides_marker(&container, &prefix, &what)? {
                    None => return Err(not_found(path)),
                    Some(n) if n > 0 => return Err(not_empty()),
                    Some(_) => {}
                }
                let resp = self.client.send("DELETE", &url_path(&container, Some(&prefix)), &[], None, &what)?;
                match resp.status().as_u16() {
                    200 | 202 | 204 => Ok(()),
                    404 => {
                        // No `name/` marker: maybe an old-style `application/directory` object.
                        match self.client.head_object(&container, &key, &what)? {
                            Some(e) if e.is_dir() => {
                                let r = self.client.send("DELETE", &url_path(&container, Some(&key)), &[], None, &what)?;
                                if r.status().is_success() || r.status().as_u16() == 404 {
                                    Ok(())
                                } else {
                                    Err(self.client.fail(r, &what))
                                }
                            }
                            Some(_) => Err(Error::new(
                                ErrorKind::InvalidPath,
                                format!("“{}” is a file, not a folder.", path.name()),
                            )),
                            None => Err(not_found(path)),
                        }
                    }
                    _ => Err(self.client.fail(resp, &what)),
                }
            }
        }
    }

    fn copy(&self, from: &RPath, to: &RPath) -> Result<()> {
        let (sc, sk) = Self::object(from, "copy")?;
        let (dc, dk) = Self::object(to, "copy to")?;
        let what = format!("copy {}", from.name());
        let dest = format!("/{}/{}", enc(&dc), dk.split('/').map(enc).collect::<Vec<_>>().join("/"));
        let resp = self.client.send("COPY", &url_path(&sc, Some(&sk)), &[("destination", &dest)], None, &what)?;
        match resp.status().as_u16() {
            200..=202 => Ok(()),
            404 => Err(Error::new(
                ErrorKind::NotFound,
                format!("Couldn't {what}: the file or the container “{dc}” wasn't found."),
            )),
            _ => Err(self.client.fail(resp, &what)),
        }
    }

    fn close(&self) {
        self.client.close();
    }
}
