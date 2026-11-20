//! SigV4 against AWS's published vectors: the aws-sig-v4-test-suite (service "service") and
//! the worked examples in the S3 "Signature Calculations for the Authorization Header" docs.

use super::*;

fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// The aws-sig-v4-test-suite credentials, region and time.
fn suite_sig(method: &str, query: &[(&str, &str)], headers: &[(&str, &str)], body: &str) -> String {
    let creds = Credentials {
        access_key: "AKIDEXAMPLE".into(),
        secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
    };
    let query = h(query);
    let mut all = h(&[("Host", "example.amazonaws.com"), ("X-Amz-Date", "20150830T123600Z")]);
    all.extend(h(headers));
    let hash = sha256_hex(body.as_bytes());
    let req = Request { method, canonical_uri: "/", query: &query, headers: &all, payload_hash: &hash };
    signature(&creds, "us-east-1", "service", "20150830T123600Z", &req).1
}

#[test]
fn suite_get_vanilla() {
    assert_eq!(
        suite_sig("GET", &[], &[], ""),
        "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
    );
}

#[test]
fn suite_get_vanilla_query_order_key_case() {
    assert_eq!(
        suite_sig("GET", &[("Param2", "value2"), ("Param1", "value1")], &[], ""),
        "b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500"
    );
}

#[test]
fn suite_post_vanilla() {
    assert_eq!(
        suite_sig("POST", &[], &[], ""),
        "5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b"
    );
}

#[test]
fn suite_post_x_www_form_urlencoded() {
    assert_eq!(
        suite_sig("POST", &[], &[("Content-Type", "application/x-www-form-urlencoded")], "Param1=value1"),
        "ff11897932ad3f4e8b18135d722051e5ac45fc38421b1da7b9d196a0fe09473a"
    );
}

#[test]
fn suite_get_vanilla_utf8_query() {
    assert_eq!(
        suite_sig("GET", &[("\u{1234}", "bar")], &[], ""),
        "2cdec8eed098649ff3a119c94853b13c643bcf08f8b0a1d91e12c9027818dd04"
    );
}

#[test]
fn suite_get_header_value_trim() {
    assert_eq!(
        suite_sig("GET", &[], &[("My-Header1", " value1"), ("My-Header2", " \"a   b   c\"")], ""),
        "acc3ed3afb60bb290fc8d2dd0098b9911fcaa05412b367055dee359757a9c736"
    );
}

#[test]
fn suite_get_header_key_duplicate() {
    assert_eq!(
        suite_sig(
            "GET",
            &[],
            &[("My-Header1", "value2"), ("My-Header1", "value2"), ("My-Header1", "value1")],
            ""
        ),
        "c9d5ea9f3f72853aea855b47ea873832890dbdd183b4468f858259531a5138ea"
    );
}

/// The S3 docs' example credentials, bucket and time.
fn s3_doc(method: &str, path: &str, query: &[(&str, &str)], headers: &[(&str, &str)], hash: &str) -> (String, String) {
    let creds = Credentials {
        access_key: "AKIAIOSFODNN7EXAMPLE".into(),
        secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
    };
    let query = h(query);
    let mut all = h(&[
        ("Host", "examplebucket.s3.amazonaws.com"),
        ("x-amz-date", "20130524T000000Z"),
        ("x-amz-content-sha256", hash),
    ]);
    all.extend(h(headers));
    let uri = encode_path(path);
    let req = Request { method, canonical_uri: &uri, query: &query, headers: &all, payload_hash: hash };
    let auth = authorization(&creds, "us-east-1", "s3", "20130524T000000Z", &req);
    (signature(&creds, "us-east-1", "s3", "20130524T000000Z", &req).1, auth)
}

const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[test]
fn s3_get_object_with_range() {
    let (sig, auth) = s3_doc("GET", "/test.txt", &[], &[("Range", "bytes=0-9")], EMPTY);
    assert_eq!(sig, "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41");
    assert_eq!(
        auth,
        "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
         SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
         Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
    );
}

#[test]
fn s3_put_object_encodes_key() {
    let body = "Welcome to Amazon S3.";
    let hash = sha256_hex(body.as_bytes());
    assert_eq!(hash, "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072");
    let (sig, _) = s3_doc(
        "PUT",
        "/test$file.text",
        &[],
        &[("Date", "Fri, 24 May 2013 00:00:00 GMT"), ("x-amz-storage-class", "REDUCED_REDUNDANCY")],
        &hash,
    );
    assert_eq!(sig, "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd");
}

#[test]
fn s3_get_bucket_lifecycle() {
    let (sig, _) = s3_doc("GET", "/", &[("lifecycle", "")], &[], EMPTY);
    assert_eq!(sig, "fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543");
}

#[test]
fn s3_list_objects() {
    let (sig, _) = s3_doc("GET", "/", &[("max-keys", "2"), ("prefix", "J")], &[], EMPTY);
    assert_eq!(sig, "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7");
}

#[test]
fn path_and_query_encoding() {
    assert_eq!(encode_path("/a b/ü+x/~._-"), "/a%20b/%C3%BC%2Bx/~._-");
    assert_eq!(encode_path("/folder/"), "/folder/");
    assert_eq!(
        canonical_query(&h(&[("prefix", "a b/"), ("delimiter", "/"), ("list-type", "2")])),
        "delimiter=%2F&list-type=2&prefix=a%20b%2F"
    );
}

#[test]
fn debug_hides_secret() {
    let c = Credentials { access_key: "AK".into(), secret_key: "very-secret".into() };
    assert!(!format!("{c:?}").contains("very-secret"));
}
