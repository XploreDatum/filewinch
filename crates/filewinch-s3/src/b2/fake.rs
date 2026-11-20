//! Test-only in-memory Backblaze B2 (native API v2) served by [`crate::mock::Mock`]: enough of
//! the protocol to exercise authorization, re-authorization, listing with folders and paging,
//! uploads with SHA-1 checks (including `hex_digits_at_end`), large files, ranged downloads,
//! copies, version deletes and injected failures.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use base64::Engine;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};

use crate::mock::{Mock, Reply, Seen};

#[derive(Clone)]
pub(crate) struct Version {
    pub id: String,
    pub name: String,
    pub bucket: String,
    pub data: Vec<u8>,
    /// `contentSha1` as B2 reports it: hex, `none`, or `unverified:<hex>`.
    pub sha1: String,
    /// `fileInfo`.
    pub info: Value,
}

/// (bucket id, file name, parts by number).
pub(crate) type LargeFile = (String, String, BTreeMap<u64, Vec<u8>>);

#[derive(Default)]
pub(crate) struct State {
    pub base: String,
    pub key: (String, String),
    pub token: String,
    pub authorizations: u32,
    /// The next N API calls answer 401 expired_auth_token.
    pub expire_next: u32,
    /// The next N API calls answer 503 with Retry-After: 5.
    pub busy_next: u32,
    pub recommended_part: u64,
    pub min_part: u64,
    /// Restrict the key to this bucket (id, name).
    pub restricted: Option<(String, String)>,
    pub buckets: BTreeMap<String, String>, // name -> id
    pub versions: Vec<Version>,
    /// Unfinished large files by id.
    pub large: BTreeMap<String, LargeFile>,
    /// `fileInfo` given to b2_start_large_file, by large file id.
    pub large_info: BTreeMap<String, Value>,
    pub cancelled: Vec<String>,
    pub next_id: u64,
    pub page_size: usize,
}

impl State {
    fn id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}{}", self.next_id)
    }

    pub fn put(&mut self, bucket: &str, name: &str, data: &[u8]) {
        let id = self.id("f");
        let sha1 = hex::encode(Sha1::digest(data));
        self.versions.push(Version { id, name: name.into(), bucket: bucket.into(), data: data.to_vec(), sha1, info: json!({}) });
    }

    pub fn latest(&self, bucket: &str, name: &str) -> Option<&Version> {
        self.versions.iter().rev().find(|v| v.bucket == bucket && v.name == name)
    }

    fn file_json(v: &Version, action: &str) -> Value {
        json!({"fileId": v.id, "fileName": v.name, "action": action, "contentLength": v.data.len(),
               "contentSha1": v.sha1, "contentType": "application/octet-stream", "uploadTimestamp": 1_700_000_000_000u64,
               "fileInfo": v.info})
    }
}

fn err(status: u16, code: &str, message: &str) -> Reply {
    Reply::json(status, json!({"status": status, "code": code, "message": message}))
}

pub(crate) fn start(state: State) -> (Mock, Arc<Mutex<State>>) {
    let st = Arc::new(Mutex::new(state));
    let s2 = st.clone();
    let mock = Mock::start(move |req| handle(&mut s2.lock().unwrap(), req));
    {
        let mut s = st.lock().unwrap();
        s.base = mock.url();
        if s.recommended_part == 0 {
            s.recommended_part = 100_000_000;
        }
        if s.min_part == 0 {
            s.min_part = 5_000_000;
        }
        if s.page_size == 0 {
            s.page_size = 1000;
        }
    }
    (mock, st)
}

fn handle(s: &mut State, req: &Seen) -> Reply {
    let path = req.path().to_string();
    if path == "/b2api/v2/b2_authorize_account" {
        let want = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", s.key.0, s.key.1));
        if req.header("authorization") != Some(&format!("Basic {want}")) {
            return err(401, "unauthorized", "invalid key");
        }
        s.authorizations += 1;
        s.token = format!("tok-{}", s.authorizations);
        let mut allowed = json!({"capabilities": ["listFiles"]});
        if let Some((id, name)) = &s.restricted {
            allowed["bucketId"] = json!(id);
            allowed["bucketName"] = json!(name);
        }
        return Reply::json(
            200,
            json!({"accountId": "acct", "authorizationToken": s.token, "apiUrl": s.base, "downloadUrl": s.base,
                   "recommendedPartSize": s.recommended_part, "absoluteMinimumPartSize": s.min_part, "allowed": allowed}),
        );
    }
    if s.busy_next > 0 {
        s.busy_next -= 1;
        return err(503, "service_unavailable", "busy").header("Retry-After", "5");
    }
    if let Some(bucket_id) = path.strip_prefix("/upload/") {
        return upload(s, bucket_id.to_string(), req);
    }
    if let Some(file_id) = path.strip_prefix("/part/") {
        return part(s, file_id.to_string(), req);
    }
    if s.expire_next > 0 {
        s.expire_next -= 1;
        return err(401, "expired_auth_token", "expired");
    }
    if req.header("authorization") != Some(s.token.as_str()) {
        return err(401, "bad_auth_token", "bad token");
    }
    if let Some(rest) = path.strip_prefix("/file/") {
        return download(s, rest, req);
    }
    let body = req.json();
    let api = path.trim_start_matches("/b2api/v2/");
    match api {
        "b2_list_buckets" => {
            let list: Vec<Value> = s
                .buckets
                .iter()
                .filter(|(n, _)| body["bucketName"].as_str().is_none_or(|w| w == n.as_str()))
                .map(|(n, id)| json!({"bucketName": n, "bucketId": id, "bucketType": "allPrivate"}))
                .collect();
            Reply::json(200, json!({"buckets": list}))
        }
        "b2_create_bucket" => {
            let name = body["bucketName"].as_str().unwrap_or_default().to_string();
            if s.buckets.contains_key(&name) {
                return err(400, "duplicate_bucket_name", "Bucket name is already in use.");
            }
            let id = s.id("bk");
            s.buckets.insert(name.clone(), id.clone());
            Reply::json(200, json!({"bucketId": id, "bucketName": name}))
        }
        "b2_delete_bucket" => {
            let id = body["bucketId"].as_str().unwrap_or_default();
            if s.versions.iter().any(|v| v.bucket == id) {
                return err(400, "cannot_delete_non_empty_bucket", "not empty");
            }
            s.buckets.retain(|_, v| v != id);
            Reply::json(200, json!({}))
        }
        "b2_list_file_names" => list_names(s, &body),
        "b2_list_file_versions" => {
            let bucket = body["bucketId"].as_str().unwrap_or_default();
            let prefix = body["prefix"].as_str().unwrap_or_default();
            let start = body["startFileName"].as_str().unwrap_or_default();
            let files: Vec<Value> = s
                .versions
                .iter()
                .filter(|v| v.bucket == bucket && v.name.starts_with(prefix) && v.name.as_str() >= start)
                .map(|v| State::file_json(v, "upload"))
                .collect();
            Reply::json(200, json!({"files": files, "nextFileName": null, "nextFileId": null}))
        }
        "b2_delete_file_version" => {
            let id = body["fileId"].as_str().unwrap_or_default();
            let before = s.versions.len();
            s.versions.retain(|v| v.id != id);
            if s.versions.len() == before {
                return err(400, "file_not_present", "no such version");
            }
            Reply::json(200, json!({"fileId": id}))
        }
        "b2_get_upload_url" => {
            let b = body["bucketId"].as_str().unwrap_or_default();
            Reply::json(200, json!({"bucketId": b, "uploadUrl": format!("{}/upload/{b}", s.base), "authorizationToken": "up-tok"}))
        }
        "b2_start_large_file" => {
            let id = s.id("L");
            let b = body["bucketId"].as_str().unwrap_or_default().to_string();
            let n = body["fileName"].as_str().unwrap_or_default().to_string();
            s.large.insert(id.clone(), (b, n, BTreeMap::new()));
            if let Some(info) = body.get("fileInfo").filter(|i| i.is_object()) {
                s.large_info.insert(id.clone(), info.clone());
            }
            Reply::json(200, json!({"fileId": id}))
        }
        "b2_get_upload_part_url" => {
            let f = body["fileId"].as_str().unwrap_or_default();
            Reply::json(200, json!({"fileId": f, "uploadUrl": format!("{}/part/{f}", s.base), "authorizationToken": "up-tok"}))
        }
        "b2_finish_large_file" => {
            let id = body["fileId"].as_str().unwrap_or_default();
            let Some((bucket, name, parts)) = s.large.remove(id) else { return err(400, "bad_request", "no such file") };
            let want: Vec<String> = parts.values().map(|p| hex::encode(Sha1::digest(p))).collect();
            let got: Vec<String> =
                body["partSha1Array"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect();
            if want != got || parts.len() < 2 {
                return err(400, "bad_request", "part sha1s don't match or too few parts");
            }
            let data: Vec<u8> = parts.into_values().flatten().collect();
            s.put(&bucket, &name, &data);
            // Like B2: a large file has no verified SHA-1; a client-supplied one is "unverified:".
            let info = s.large_info.remove(id).unwrap_or_else(|| json!({}));
            let v = s.versions.last_mut().expect("just put");
            v.sha1 = match info["large_file_sha1"].as_str() {
                Some(h) => format!("unverified:{h}"),
                None => "none".into(),
            };
            v.info = info;
            Reply::json(200, json!({"fileId": id}))
        }
        "b2_cancel_large_file" => {
            let id = body["fileId"].as_str().unwrap_or_default().to_string();
            s.large.remove(&id);
            s.cancelled.push(id.clone());
            Reply::json(200, json!({"fileId": id}))
        }
        "b2_copy_file" => {
            let src = body["sourceFileId"].as_str().unwrap_or_default();
            let Some(v) = s.versions.iter().find(|v| v.id == src).cloned() else { return err(400, "bad_request", "no source") };
            let bucket = body["destinationBucketId"].as_str().map(String::from).unwrap_or(v.bucket.clone());
            s.put(&bucket, body["fileName"].as_str().unwrap_or_default(), &v.data);
            Reply::json(200, json!({}))
        }
        "b2_copy_part" => {
            let src = body["sourceFileId"].as_str().unwrap_or_default();
            let Some(v) = s.versions.iter().find(|v| v.id == src).cloned() else { return err(400, "bad_request", "no source") };
            let range = body["range"].as_str().unwrap_or_default().trim_start_matches("bytes=");
            let (a, b) = range.split_once('-').unwrap_or(("0", "0"));
            let (a, b): (usize, usize) = (a.parse().unwrap_or(0), b.parse().unwrap_or(0));
            let chunk = v.data[a..=b].to_vec();
            let sha = hex::encode(Sha1::digest(&chunk));
            let large = body["largeFileId"].as_str().unwrap_or_default();
            let n = body["partNumber"].as_u64().unwrap_or(0);
            if let Some(l) = s.large.get_mut(large) {
                l.2.insert(n, chunk);
            }
            Reply::json(200, json!({"contentSha1": sha}))
        }
        _ => err(400, "bad_request", "unknown call"),
    }
}

fn list_names(s: &State, body: &Value) -> Reply {
    let bucket = body["bucketId"].as_str().unwrap_or_default();
    let prefix = body["prefix"].as_str().unwrap_or_default();
    let start = body["startFileName"].as_str().unwrap_or_default();
    let delim = body["delimiter"].as_str().is_some();
    let max = (body["maxFileCount"].as_u64().unwrap_or(100) as usize).min(s.page_size);
    let mut latest: BTreeMap<String, &Version> = BTreeMap::new();
    for v in s.versions.iter().filter(|v| v.bucket == bucket && v.name.starts_with(prefix)) {
        latest.insert(v.name.clone(), v);
    }
    let mut out: Vec<Value> = Vec::new();
    let mut last_folder = String::new();
    let mut next = None;
    for (name, v) in latest.range(start.to_string()..) {
        let rest = &name[prefix.len()..];
        let item = match rest.find('/') {
            Some(i) if delim => {
                let folder = format!("{prefix}{}", &rest[..=i]);
                if folder == last_folder || folder.as_str() < start {
                    continue;
                }
                last_folder = folder.clone();
                json!({"fileName": folder, "action": "folder", "fileId": null, "contentLength": 0})
            }
            _ => State::file_json(v, "upload"),
        };
        if out.len() == max {
            next = Some(item["fileName"].as_str().unwrap_or_default().to_string());
            break;
        }
        out.push(item);
    }
    Reply::json(200, json!({"files": out, "nextFileName": next}))
}

fn check_sha(req: &Seen) -> Result<Vec<u8>, Reply> {
    let header = req.header("x-bz-content-sha1").unwrap_or_default();
    let (data, sha) = if header == "hex_digits_at_end" {
        let n = req.body.len().saturating_sub(40);
        (req.body[..n].to_vec(), String::from_utf8_lossy(&req.body[n..]).into_owned())
    } else {
        (req.body.clone(), header.to_string())
    };
    if hex::encode(Sha1::digest(&data)) != sha {
        return Err(err(400, "bad_request", "Sha1 did not match data received"));
    }
    Ok(data)
}

fn upload(s: &mut State, bucket_id: String, req: &Seen) -> Reply {
    if req.header("authorization") != Some("up-tok") {
        return err(401, "bad_auth_token", "bad upload token");
    }
    let data = match check_sha(req) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let name = percent_encoding::percent_decode_str(req.header("x-bz-file-name").unwrap_or_default())
        .decode_utf8_lossy()
        .into_owned();
    s.put(&bucket_id, &name, &data);
    let v = s.versions.last().cloned().expect("just put");
    Reply::json(200, State::file_json(&v, "upload"))
}

fn part(s: &mut State, file_id: String, req: &Seen) -> Reply {
    let data = match check_sha(req) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let n: u64 = req.header("x-bz-part-number").and_then(|v| v.parse().ok()).unwrap_or(0);
    let Some(l) = s.large.get_mut(&file_id) else { return err(400, "bad_request", "no such large file") };
    l.2.insert(n, data);
    Reply::json(200, json!({"partNumber": n}))
}

fn download(s: &State, rest: &str, req: &Seen) -> Reply {
    let (bucket_name, name) = rest.split_once('/').unwrap_or((rest, ""));
    let name = percent_encoding::percent_decode_str(name).decode_utf8_lossy().into_owned();
    let Some(bucket) = s.buckets.get(bucket_name) else { return err(404, "not_found", "bucket") };
    let Some(v) = s.latest(bucket, &name) else { return err(404, "not_found", "file") };
    match req.header("range").and_then(|r| r.strip_prefix("bytes=")).and_then(|r| r.strip_suffix('-')) {
        Some(from) => {
            let from: usize = from.parse().unwrap_or(0);
            if from >= v.data.len() {
                return err(416, "range_not_satisfiable", "range");
            }
            Reply::new(206, v.data[from..].to_vec())
                .header("Content-Range", &format!("bytes {from}-{}/{}", v.data.len() - 1, v.data.len()))
        }
        None => Reply::new(200, v.data.clone()),
    }
}
