//! The mock's object store: account, containers, objects, listings, Range, COPY and SLO.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tiny_http::{Request, Response};

use super::{Obj, Resp, State, decode, hdr, header, status};

const MODIFIED_ISO: &str = "2024-01-02T03:04:05.000000";
const MODIFIED_HTTP: &str = "Tue, 02 Jan 2024 03:04:05 GMT";

pub fn plain(data: Vec<u8>, content_type: &str) -> Obj {
    let etag = format!("{:x}", md5::compute(&data));
    Obj { data, content_type: content_type.into(), etag, manifest: None, dlo: None }
}

/// A DLO's segments: every object under `container/prefix`, in name order.
fn dlo_segments<'a>(st: &'a State, manifest: &str) -> Option<Vec<&'a Obj>> {
    let (c, prefix) = manifest.trim_start_matches('/').split_once('/')?;
    Some(st.containers.get(c)?.iter().filter(|(k, _)| k.starts_with(prefix)).map(|(_, o)| o).collect())
}

fn json_resp(v: Value) -> Resp {
    Response::from_data(v.to_string().into_bytes()).with_header(hdr("Content-Type", "application/json; charset=utf-8"))
}

/// The object's bytes, assembling an SLO from its segments.
fn content(st: &State, obj: &Obj) -> Option<Vec<u8>> {
    if let Some(m) = &obj.dlo {
        return Some(dlo_segments(st, m)?.into_iter().flat_map(|o| o.data.clone()).collect());
    }
    let Some(segs) = &obj.manifest else { return Some(obj.data.clone()) };
    let mut out = Vec::new();
    for p in segs {
        let (c, k) = p.split_once('/')?;
        out.extend_from_slice(&st.containers.get(c)?.get(k)?.data);
    }
    Some(out)
}

pub fn storage(
    st: &Arc<Mutex<State>>,
    req: &mut Request,
    method: &str,
    full_path: &str,
    rest: &str,
    q: &BTreeMap<String, String>,
    url: &str,
) -> Resp {
    let _ = full_path;
    {
        let mut s = st.lock().unwrap();
        s.log.push(format!("{method} {url}"));
        let token = header(req, "x-auth-token").unwrap_or_default();
        if s.reject_tokens || !s.tokens.contains(&token) {
            return Response::from_data(b"<html><h1>Unauthorized</h1></html>".to_vec()).with_status_code(401);
        }
        if let Some((code, headers, body)) = s.fail.pop_front() {
            let mut r = Response::from_data(body.into_bytes()).with_status_code(code);
            for (k, v) in headers {
                r = r.with_header(hdr(k, &v));
            }
            return r;
        }
    }
    let rest = rest.trim_start_matches('/');
    if rest.is_empty() {
        return account(st, method, q);
    }
    let (c, o) = match rest.split_once('/') {
        Some((c, o)) if !o.is_empty() => (decode(c), Some(decode(o))),
        Some((c, _)) => (decode(c), None),
        None => (decode(rest), None),
    };
    match o {
        None => container(st, method, &c, q),
        Some(o) => object(st, req, method, &c, &o, q),
    }
}

fn limit_marker(q: &BTreeMap<String, String>) -> (usize, String) {
    let limit = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(10_000);
    (limit, q.get("marker").cloned().unwrap_or_default())
}

fn account(st: &Arc<Mutex<State>>, method: &str, q: &BTreeMap<String, String>) -> Resp {
    let s = st.lock().unwrap();
    match method {
        "HEAD" => status(204).with_header(hdr("X-Account-Container-Count", &s.containers.len().to_string())),
        "GET" => {
            let (limit, marker) = limit_marker(q);
            let rows: Vec<Value> = s
                .containers
                .iter()
                .filter(|(n, _)| n.as_str() > marker.as_str())
                .take(limit)
                .map(|(n, c)| {
                    json!({"name": n, "count": c.len(), "bytes": c.values().map(|o| o.data.len()).sum::<usize>(), "last_modified": MODIFIED_ISO})
                })
                .collect();
            json_resp(Value::Array(rows))
        }
        _ => status(405),
    }
}

fn container(st: &Arc<Mutex<State>>, method: &str, c: &str, q: &BTreeMap<String, String>) -> Resp {
    let mut s = st.lock().unwrap();
    match method {
        "PUT" => {
            if s.containers.contains_key(c) {
                status(202)
            } else {
                s.containers.insert(c.into(), BTreeMap::new());
                status(201)
            }
        }
        "DELETE" => match s.containers.get(c) {
            None => status(404),
            Some(objs) if !objs.is_empty() => Response::from_data(b"There was a conflict when trying to complete your request.".to_vec()).with_status_code(409),
            Some(_) => {
                s.containers.remove(c);
                status(204)
            }
        },
        "HEAD" => match s.containers.get(c) {
            None => status(404),
            Some(objs) => status(204).with_header(hdr("X-Container-Object-Count", &objs.len().to_string())),
        },
        "GET" => {
            let Some(objs) = s.containers.get(c) else { return status(404) };
            let (limit, marker) = limit_marker(q);
            let prefix = q.get("prefix").cloned().unwrap_or_default();
            let delim = q.get("delimiter").cloned().filter(|d| !d.is_empty());
            let mut out = Vec::new();
            let mut last_sub = String::new();
            for (name, obj) in objs.iter().filter(|(n, _)| n.as_str() > marker.as_str()) {
                if out.len() >= limit {
                    break;
                }
                if !name.starts_with(&prefix) {
                    continue;
                }
                if let Some(d) = &delim
                    && let Some(i) = name[prefix.len()..].find(d.as_str())
                {
                    let sub = &name[..prefix.len() + i + d.len()];
                    if sub != last_sub {
                        last_sub = sub.to_string();
                        out.push(json!({"subdir": sub}));
                    }
                    continue;
                }
                // Like Swift: a DLO manifest lists as its own (empty) object; an SLO with its
                // total size, the segments' ETag hash and `slo_etag`.
                let bytes = if obj.dlo.is_some() { obj.data.len() } else { content(&s, obj).map_or(0, |d| d.len()) };
                let mut row = json!({"name": name, "bytes": bytes, "hash": obj.etag, "last_modified": MODIFIED_ISO, "content_type": obj.content_type});
                if obj.manifest.is_some() {
                    row["slo_etag"] = json!(format!("\"{:x}\"", md5::compute(b"manifest")));
                }
                out.push(row);
            }
            json_resp(Value::Array(out))
        }
        _ => status(405),
    }
}

fn read_body(req: &mut Request) -> Option<Vec<u8>> {
    let mut data = Vec::new();
    req.as_reader().read_to_end(&mut data).ok()?;
    if let Some(len) = header(req, "content-length").and_then(|l| l.parse::<usize>().ok())
        && len != data.len()
    {
        return None;
    }
    Some(data)
}

fn object(st: &Arc<Mutex<State>>, req: &mut Request, method: &str, c: &str, o: &str, q: &BTreeMap<String, String>) -> Resp {
    if !st.lock().unwrap().containers.contains_key(c) {
        return status(404);
    }
    match method {
        "PUT" => put(st, req, c, o, q),
        "GET" | "HEAD" => get(st, req, method, c, o),
        "DELETE" => {
            let mut s = st.lock().unwrap();
            let Some(obj) = s.containers.get_mut(c).and_then(|m| m.remove(o)) else { return status(404) };
            if q.get("multipart-manifest").map(String::as_str) == Some("delete") {
                let segs = obj.manifest.unwrap_or_default();
                for p in &segs {
                    if let Some((sc, sk)) = p.split_once('/') {
                        s.containers.get_mut(sc).and_then(|m| m.remove(sk));
                    }
                }
                return json_resp(json!({"Number Deleted": segs.len() + 1, "Number Not Found": 0, "Response Body": "", "Response Status": "200 OK", "Errors": []}));
            }
            status(204)
        }
        "COPY" => {
            let Some(dest) = header(req, "destination") else { return status(412) };
            let dest = decode(&dest);
            let Some((dc, dk)) = dest.trim_start_matches('/').split_once('/') else { return status(412) };
            let mut s = st.lock().unwrap();
            let Some(src) = s.containers.get(c).and_then(|m| m.get(o)) else { return status(404) };
            let Some(data) = content(&s, src) else { return status(409) };
            let obj = plain(data, &src.content_type.clone());
            let Some(dm) = s.containers.get_mut(dc) else { return status(404) };
            dm.insert(dk.to_string(), obj);
            status(201)
        }
        _ => status(405),
    }
}

fn put(st: &Arc<Mutex<State>>, req: &mut Request, c: &str, o: &str, q: &BTreeMap<String, String>) -> Resp {
    let Some(data) = read_body(req) else { return status(599) };
    let ct = header(req, "content-type").unwrap_or_else(|| "application/octet-stream".into());
    let mut s = st.lock().unwrap();
    let obj = if let Some(src) = header(req, "x-copy-from") {
        let src = decode(&src);
        let Some((sc, sk)) = src.trim_start_matches('/').split_once('/') else { return status(412) };
        let Some(so) = s.containers.get(sc).and_then(|m| m.get(sk)) else { return status(404) };
        let Some(data) = content(&s, so) else { return status(409) };
        plain(data, &so.content_type.clone())
    } else if q.get("multipart-manifest").map(String::as_str) == Some("put") {
        let Ok(Value::Array(items)) = serde_json::from_slice::<Value>(&data) else { return status(400) };
        let mut segs = Vec::new();
        let mut etags = String::new();
        for it in items {
            let path = it["path"].as_str().unwrap_or("").trim_start_matches('/').to_string();
            let Some((sc, sk)) = path.split_once('/') else { return status(400) };
            let Some(seg) = s.containers.get(sc).and_then(|m| m.get(sk)) else {
                return Response::from_data(format!("Errors:\n{path}, 404 Not Found").into_bytes()).with_status_code(400);
            };
            if it["etag"].as_str() != Some(seg.etag.as_str()) || it["size_bytes"].as_u64() != Some(seg.data.len() as u64) {
                return Response::from_data(format!("Errors:\n{path}, Etag Mismatch").into_bytes()).with_status_code(400);
            }
            etags.push_str(&seg.etag);
            segs.push(path);
        }
        Obj {
            data: Vec::new(),
            content_type: "application/octet-stream".into(),
            etag: format!("{:x}", md5::compute(etags)),
            manifest: Some(segs),
            dlo: None,
        }
    } else {
        plain(data, &ct)
    };
    let etag = if s.bad_etag { "0123456789abcdef0123456789abcdef".to_string() } else { obj.etag.clone() };
    let Some(m) = s.containers.get_mut(c) else { return status(404) };
    m.insert(o.to_string(), obj);
    status(201).with_header(hdr("Etag", &etag))
}

fn get(st: &Arc<Mutex<State>>, req: &Request, method: &str, c: &str, o: &str) -> Resp {
    let s = st.lock().unwrap();
    let Some(obj) = s.containers.get(c).and_then(|m| m.get(o)) else { return status(404) };
    let Some(data) = content(&s, obj) else { return status(409) };
    let mut code = 200;
    let mut body = data.clone();
    let mut range_hdr = None;
    if let Some(r) = header(req, "range").and_then(|r| r.strip_prefix("bytes=").map(str::to_string)) {
        let (a, b) = r.split_once('-').unwrap_or((&r, ""));
        let start: usize = a.parse().unwrap_or(0);
        let end: usize = b.parse().map(|e: usize| e + 1).unwrap_or(data.len()).min(data.len());
        if start >= data.len() {
            return status(416).with_header(hdr("Content-Range", &format!("bytes */{}", data.len())));
        }
        body = data[start..end].to_vec();
        code = 206;
        range_hdr = Some(format!("bytes {start}-{}/{}", end - 1, data.len()));
    }
    let _ = method;
    // Always Content-Length (tiny_http would switch to chunked above 32 KiB), like Swift.
    let mut r = Response::from_data(body)
        .with_chunked_threshold(usize::MAX)
        .with_status_code(code)
        .with_header(hdr("Etag", &head_etag(&s, obj)))
        .with_header(hdr("Content-Type", &obj.content_type))
        .with_header(hdr("Last-Modified", MODIFIED_HTTP));
    if let Some(cr) = range_hdr {
        r = r.with_header(hdr("Content-Range", &cr));
    }
    if obj.manifest.is_some() {
        r = r.with_header(hdr("X-Static-Large-Object", "True"));
    }
    if let Some(m) = &obj.dlo {
        r = r.with_header(hdr("X-Object-Manifest", m));
    }
    r
}

/// The ETag a GET/HEAD shows: an object's MD5; for a large object, quoted, the MD5 of its
/// segments' ETags (as Swift does).
fn head_etag(st: &State, obj: &Obj) -> String {
    match &obj.dlo {
        Some(m) => {
            let etags: String = dlo_segments(st, m).unwrap_or_default().iter().map(|o| o.etag.as_str()).collect();
            format!("\"{:x}\"", md5::compute(etags))
        }
        None if obj.manifest.is_some() => format!("\"{}\"", obj.etag),
        None => obj.etag.clone(),
    }
}
