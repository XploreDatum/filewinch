//! Parsing the Blob service's XML: List Containers, List Blobs, error bodies.

use std::time::SystemTime;

use base64::Engine;
use filewinch_core::{Checksum, Error, ErrorKind, HashAlgo, Result};
use roxmltree::{Document, Node};

pub(crate) struct ContainerItem {
    pub name: String,
    pub modified: Option<SystemTime>,
    pub etag: Option<String>,
}

pub(crate) struct BlobItem {
    pub name: String,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub tier: Option<String>,
    /// `Content-MD5` as sent (base64); see [`content_md5`].
    pub content_md5: Option<String>,
}

#[derive(Default)]
pub(crate) struct BlobPage {
    pub blobs: Vec<BlobItem>,
    pub prefixes: Vec<String>,
    pub next_marker: Option<String>,
}

fn doc(xml: &str) -> Result<Document<'_>> {
    Document::parse(xml.trim_start_matches('\u{feff}')).map_err(|e| {
        Error::new(ErrorKind::Other, "Azure sent a listing Filewinch couldn't read.").with_detail(e)
    })
}

fn child<'a>(n: Node<'a, 'a>, name: &str) -> Option<Node<'a, 'a>> {
    n.children().find(|c| c.has_tag_name(name))
}

fn text(n: Node, name: &str) -> Option<String> {
    child(n, name).map(|c| c.text().unwrap_or_default().to_string())
}

fn next_marker(root: Node) -> Option<String> {
    text(root, "NextMarker").filter(|m| !m.is_empty())
}

pub(crate) fn http_date(s: Option<String>) -> Option<SystemTime> {
    s.and_then(|v| httpdate::parse_http_date(v.trim()).ok())
}

pub(crate) fn containers(xml: &str) -> Result<(Vec<ContainerItem>, Option<String>)> {
    let d = doc(xml)?;
    let root = d.root_element();
    let mut out = Vec::new();
    if let Some(list) = child(root, "Containers") {
        for c in list.children().filter(|c| c.has_tag_name("Container")) {
            let Some(name) = text(c, "Name") else { continue };
            let props = child(c, "Properties");
            out.push(ContainerItem {
                name,
                modified: http_date(props.and_then(|p| text(p, "Last-Modified"))),
                etag: props.and_then(|p| text(p, "Etag")),
            });
        }
    }
    Ok((out, next_marker(root)))
}

pub(crate) fn blobs(xml: &str) -> Result<BlobPage> {
    let d = doc(xml)?;
    let root = d.root_element();
    let mut page = BlobPage { next_marker: next_marker(root), ..BlobPage::default() };
    let Some(list) = child(root, "Blobs") else { return Ok(page) };
    for n in list.children() {
        if n.has_tag_name("BlobPrefix") {
            if let Some(name) = text(n, "Name") {
                page.prefixes.push(name);
            }
        } else if n.has_tag_name("Blob") {
            let Some(name) = text(n, "Name") else { continue };
            let props = child(n, "Properties");
            let prop = |k: &str| props.and_then(|p| text(p, k)).filter(|v| !v.is_empty());
            page.blobs.push(BlobItem {
                name,
                size: prop("Content-Length").and_then(|v| v.trim().parse().ok()),
                modified: http_date(prop("Last-Modified")),
                content_type: prop("Content-Type"),
                etag: prop("Etag"),
                tier: prop("AccessTier"),
                content_md5: prop("Content-MD5"),
            });
        }
    }
    Ok(page)
}

/// A blob's `Content-MD5` (base64) as an MD5 checksum: only when it decodes to exactly 16
/// bytes. Azure fills it for blobs written in one Put Blob (computed or verified by the
/// service); blobs committed from blocks have none unless the writer set it.
pub(crate) fn content_md5(b64: Option<&str>) -> Option<Checksum> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64?.trim()).ok()?;
    (bytes.len() == 16).then(|| Checksum::from_bytes(HashAlgo::Md5, &bytes))
}

/// `(Code, Message)` of an `<Error>` body, when it is one.
pub(crate) fn error_body(xml: &str) -> (Option<String>, Option<String>) {
    let Ok(d) = Document::parse(xml.trim_start_matches('\u{feff}').trim()) else { return (None, None) };
    let root = d.root_element();
    if !root.has_tag_name("Error") {
        return (None, None);
    }
    (text(root, "Code"), text(root, "Message"))
}
