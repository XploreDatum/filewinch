//! ListBuckets and ListObjectsV2: requests, paging and parsing.

use std::time::SystemTime;

use filewinch_core::{Error, ErrorKind, Result};
use percent_encoding::percent_decode_str;

use super::client::{Call, Client};
use crate::time::parse_iso8601;
use crate::xml::{self, Element};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Bucket {
    pub name: String,
    pub created: Option<SystemTime>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Object {
    pub key: String,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub etag: Option<String>,
    pub storage_class: Option<String>,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Page {
    pub objects: Vec<Object>,
    /// CommonPrefixes, each ending in the delimiter.
    pub prefixes: Vec<String>,
    /// Continuation token when the listing is truncated.
    pub next: Option<String>,
}

fn bad_xml(client: &Client, what: &str, detail: impl std::fmt::Display) -> Error {
    Error::new(ErrorKind::Other, format!("Couldn't {what}: {} sent a listing Filewinch couldn't read.", client.host()))
        .with_detail(detail)
}

/// Every bucket the key can see (following ListBuckets continuation tokens).
pub(crate) fn list_buckets(client: &Client) -> Result<Vec<Bucket>> {
    let what = "list the buckets";
    let mut out = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut call = Call::new("GET", None, "");
        if let Some(t) = &token {
            call = call.query("continuation-token", t.clone());
        }
        let body = client.text(client.send(&call, what)?, what)?;
        let root = xml::parse(&body).map_err(|e| bad_xml(client, what, e))?;
        let (buckets, next) = parse_buckets(&root);
        out.extend(buckets);
        match next {
            Some(t) if Some(&t) != token.as_ref() => token = Some(t),
            _ => return Ok(out),
        }
    }
}

pub(crate) fn parse_buckets(root: &Element) -> (Vec<Bucket>, Option<String>) {
    let buckets = root
        .child("Buckets")
        .map(|b| {
            b.all("Bucket")
                .filter_map(|b| {
                    let name = b.text_of("Name")?.trim().to_string();
                    let created = b.text_of("CreationDate").and_then(parse_iso8601);
                    (!name.is_empty()).then_some(Bucket { name, created })
                })
                .collect()
        })
        .unwrap_or_default();
    let next = root.text_of("ContinuationToken").map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
    (buckets, next)
}

/// One ListObjectsV2 page.
pub(crate) fn list_page(
    client: &Client,
    bucket: &str,
    prefix: &str,
    delimiter: bool,
    token: Option<&str>,
    max_keys: Option<u32>,
    what: &str,
) -> Result<Page> {
    let mut call = Call::new("GET", Some(bucket), "")
        .query("list-type", "2")
        .query("prefix", prefix)
        .query("encoding-type", "url");
    if delimiter {
        call = call.query("delimiter", "/");
    }
    if let Some(t) = token {
        call = call.query("continuation-token", t);
    }
    if let Some(m) = max_keys {
        call = call.query("max-keys", m.to_string());
    }
    let body = client.text(client.send(&call, what)?, what)?;
    let root = xml::parse(&body).map_err(|e| bad_xml(client, what, e))?;
    if root.name == "Error" {
        client.check_body_error(&body, what)?;
    }
    Ok(parse_page(&root))
}

/// Every object and common prefix under `prefix` (all pages).
pub(crate) fn list_all(client: &Client, bucket: &str, prefix: &str, what: &str) -> Result<Page> {
    let mut all = Page::default();
    let mut token: Option<String> = None;
    loop {
        let page = list_page(client, bucket, prefix, true, token.as_deref(), None, what)?;
        all.objects.extend(page.objects);
        all.prefixes.extend(page.prefixes);
        match page.next {
            // A server that repeats its token would loop forever; stop instead.
            Some(t) if Some(&t) != token.as_ref() => token = Some(t),
            _ => return Ok(all),
        }
    }
}

pub(crate) fn parse_page(root: &Element) -> Page {
    let url_encoded = root.text_of("EncodingType").is_some_and(|e| e.trim().eq_ignore_ascii_case("url"));
    let decode = |s: &str| -> String {
        if url_encoded {
            // S3's "url" encoding writes spaces as '+' (form encoding).
            let s = s.replace('+', " ");
            percent_decode_str(&s).decode_utf8_lossy().into_owned()
        } else {
            s.to_string()
        }
    };
    let objects = root
        .all("Contents")
        .filter_map(|c| {
            let key = decode(c.text_of("Key")?);
            Some(Object {
                key,
                size: c.text_of("Size").and_then(|s| s.trim().parse().ok()).unwrap_or(0),
                modified: c.text_of("LastModified").and_then(parse_iso8601),
                etag: c.text_of("ETag").map(|e| e.trim().to_string()).filter(|e| !e.is_empty()),
                storage_class: c.text_of("StorageClass").map(|e| e.trim().to_string()).filter(|e| !e.is_empty()),
            })
        })
        .collect();
    let prefixes = root.all("CommonPrefixes").filter_map(|p| p.text_of("Prefix")).map(decode).collect();
    let truncated = root.text_of("IsTruncated").is_some_and(|t| t.trim() == "true");
    let next = root
        .text_of("NextContinuationToken")
        .map(str::trim)
        .filter(|t| truncated && !t.is_empty())
        .map(str::to_string);
    Page { objects, prefixes, next }
}
