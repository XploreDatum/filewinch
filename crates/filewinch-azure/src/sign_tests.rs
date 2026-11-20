use super::*;

/// Azurite's published development key (not a secret).
const DEV_KEY: &str =
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";

fn key() -> Vec<u8> {
    STANDARD.decode(DEV_KEY).unwrap()
}

fn s(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// Microsoft's documented example ("Authorize with Shared Key", Get Container Metadata,
/// version 2009-09-19). Signature cross-checked with Python's hmac/hashlib/base64.
#[test]
fn microsoft_example_string_to_sign_and_signature() {
    let headers =
        s(&[("x-ms-version", "2009-09-19"), ("x-ms-date", "Sun, 11 Oct 2009 21:49:13 GMT")]);
    let query = s(&[("restype", "container"), ("comp", "metadata"), ("timeout", "20")]);
    let sts = string_to_sign("GET", "myaccount", "/mycontainer", &query, &headers);
    let expected = "GET\n\n\n\n\n\n\n\n\n\n\n\n\
        x-ms-date:Sun, 11 Oct 2009 21:49:13 GMT\nx-ms-version:2009-09-19\n\
        /myaccount/mycontainer\ncomp:metadata\nrestype:container\ntimeout:20";
    assert_eq!(sts, expected);
    assert_eq!(signature(&key(), &sts), "m649E40iEJ3QQyCg9/WI2Fa9zS+RB/2rEBcLJb0CKs0=");
}

/// A Put Blob on a path-style (Azurite) endpoint: standard headers in position, encoded
/// path, account repeated. Signature cross-checked with Python.
#[test]
fn put_blob_path_style() {
    let headers = s(&[
        ("Content-Type", "text/plain; charset=UTF-8"),
        ("Content-Length", "11"),
        ("x-ms-blob-type", "BlockBlob"),
        ("X-MS-Version", "2021-08-06"),
        ("x-ms-date", "Fri, 20 Nov 2026 10:00:00 GMT"),
        ("user-agent", "ignored"),
    ]);
    let query = s(&[("timeout", "30")]);
    let sts = string_to_sign(
        "put",
        "devstoreaccount1",
        "/devstoreaccount1/photos/a%20b.txt",
        &query,
        &headers,
    );
    let expected = "PUT\n\n\n11\n\ntext/plain; charset=UTF-8\n\n\n\n\n\n\n\
        x-ms-blob-type:BlockBlob\nx-ms-date:Fri, 20 Nov 2026 10:00:00 GMT\nx-ms-version:2021-08-06\n\
        /devstoreaccount1/devstoreaccount1/photos/a%20b.txt\ntimeout:30";
    assert_eq!(sts, expected);
    assert_eq!(signature(&key(), &sts), "hvjClMYbVpovaPtAbfK8XSIJWIdr850W2qpDgE8bx3U=");
}

#[test]
fn zero_content_length_is_empty() {
    let headers = s(&[("content-length", "0")]);
    let sts = string_to_sign("PUT", "a", "/c", &[], &headers);
    assert!(sts.starts_with("PUT\n\n\n\n"), "{sts:?}");
}

#[test]
fn query_params_lowercased_sorted_and_grouped() {
    let query = s(&[
        ("restype", "container"),
        ("COMP", "list"),
        ("prefix", "a b/"),
        ("include", "snapshots"),
        ("include", "metadata"),
        ("marker", ""),
    ]);
    assert_eq!(
        canonical_resource("acct", "/box", &query),
        "/acct/box\ncomp:list\ninclude:metadata,snapshots\nmarker:\nprefix:a b/\nrestype:container"
    );
    assert_eq!(canonical_resource("acct", "", &[]), "/acct/");
}

#[test]
fn ms_headers_lowercased_sorted_trimmed() {
    let headers = s(&[
        ("X-Ms-Meta-B", "  two   words  "),
        ("x-ms-date", "D"),
        ("Content-Type", "x"),
        ("x-ms-meta-a", "1"),
    ]);
    assert_eq!(canonical_headers(&headers), "x-ms-date:D\nx-ms-meta-a:1\nx-ms-meta-b:two words\n");
}
