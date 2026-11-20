use super::*;

const MD5: &str = "9e107d9d372bb6826bd81d3542a419d6";
const SHA1: &str = "2fd4e1c67a2d28fced849ee1bb76e7391b93eb12";

#[test]
fn s3_single_part_etag_is_md5() {
    let want = Checksum { algo: HashAlgo::Md5, hex: MD5.into() };
    assert_eq!(s3_etag_md5(&format!("\"{MD5}\"")), Some(want.clone()));
    assert_eq!(s3_etag_md5(MD5), Some(want.clone()), "unquoted (some compatibles)");
    assert_eq!(s3_etag_md5(&format!(" \"{}\" ", MD5.to_uppercase())), Some(want));
}

#[test]
fn s3_other_etags_are_not_md5() {
    for bad in [
        format!("\"{MD5}-3\""),            // multipart
        format!("{MD5}-12"),               // multipart, unquoted
        format!("W/\"{MD5}\""),            // weak
        "\"9e107d9d372bb6826bd81d3542a419\"".into(), // 30 hex
        format!("\"{MD5}00\""),            // 34 hex
        "\"zz107d9d372bb6826bd81d3542a419d6\"".into(), // not hex
        "\"\"".into(),
        String::new(),
        format!("\"{MD5}"),                // unbalanced quote
    ] {
        assert_eq!(s3_etag_md5(&bad), None, "{bad}");
    }
}

#[test]
fn s3_encryption_headers() {
    assert!(!s3_etag_is_opaque(None, None));
    assert!(!s3_etag_is_opaque(Some("AES256"), None), "SSE-S3 keeps the MD5 ETag");
    assert!(s3_etag_is_opaque(Some("aws:kms"), None));
    assert!(s3_etag_is_opaque(Some(" AWS:KMS:DSSE "), None));
    assert!(s3_etag_is_opaque(None, Some("AES256")), "SSE-C");
    assert!(!s3_etag_is_opaque(None, Some(" ")));
}

#[test]
fn b2_sha1_only_when_verified() {
    let want = Checksum { algo: HashAlgo::Sha1, hex: SHA1.into() };
    assert_eq!(b2_content_sha1(Some(SHA1)), Some(want.clone()));
    assert_eq!(b2_content_sha1(Some(&SHA1.to_uppercase())), Some(want));
    assert_eq!(b2_content_sha1(Some("none")), None);
    assert_eq!(b2_content_sha1(Some(&format!("unverified:{SHA1}"))), None);
    assert_eq!(b2_content_sha1(Some(&SHA1[..39])), None);
    assert_eq!(b2_content_sha1(Some(MD5)), None, "32 hex isn't a SHA-1");
    assert_eq!(b2_content_sha1(Some("")), None);
    assert_eq!(b2_content_sha1(None), None);
}
