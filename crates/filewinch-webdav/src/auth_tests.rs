use super::*;

#[test]
fn rfc2617_md5_example() {
    let c = parse_challenges([
        r#"Digest realm="testrealm@host.com", qop="auth,auth-int", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", opaque="5ccc069c403ebaf9f0171e9517f40e41""#,
    ]);
    let d = Digest::from_challenge(&c[0]).unwrap();
    assert_eq!((d.algorithm, d.sess, d.qop_auth), (Algorithm::Md5, false, true));
    assert_eq!(
        d.response("Mufasa", "Circle Of Life", "GET", "/dir/index.html", "0a4f113b", "00000001"),
        "6629fae49393a05397450978507c4ef1"
    );
}

#[test]
fn rfc7616_md5_and_sha256_examples() {
    let header = |alg: &str| {
        format!(
            r#"Digest realm="http-auth@example.org", qop="auth, auth-int", algorithm={alg}, nonce="7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v", opaque="FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS""#
        )
    };
    let cnonce = "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ";
    let md5 = Digest::from_challenge(&parse_challenges([header("MD5").as_str()])[0]).unwrap();
    assert_eq!(
        md5.response("Mufasa", "Circle of Life", "GET", "/dir/index.html", cnonce, "00000001"),
        "8ca523f5e9506fed4657c9700eebdbec"
    );
    let mut sha = Digest::from_challenge(&parse_challenges([header("SHA-256").as_str()])[0]).unwrap();
    assert_eq!(sha.algorithm, Algorithm::Sha256);
    assert_eq!(
        sha.response("Mufasa", "Circle of Life", "GET", "/dir/index.html", cnonce, "00000001"),
        "753927fa0e85d155564e2e272a28d1802ca10daf4496794697cf8db5856cb6c1"
    );
    let h = sha.authorize("Mufasa", "Circle of Life", "GET", "/dir/index.html", cnonce);
    assert!(h.starts_with("Digest username=\"Mufasa\", realm=\"http-auth@example.org\""), "{h}");
    assert!(h.contains("algorithm=SHA-256"), "{h}");
    assert!(h.contains("response=\"753927fa0e85d155564e2e272a28d1802ca10daf4496794697cf8db5856cb6c1\""));
    assert!(h.contains("qop=auth, nc=00000001, cnonce=\""));
    assert!(h.contains("opaque=\"FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS\""));
    assert!(sha.authorize("Mufasa", "Circle of Life", "GET", "/", cnonce).contains("nc=00000002"));
}

#[test]
fn rfc7616_userhash() {
    // RFC 7616 §3.9.2 user-hash of "J\u{e4}s\u{f8}n Doe" in realm "api@example.org".
    let c = parse_challenges([
        r#"Digest realm="api@example.org", qop=auth, algorithm=SHA-512-256, nonce="5TsQWLVdgBdmrQ0XsxbDODV+57QdFR34I9HAbC/RVvkK", userhash=true"#,
    ]);
    assert!(Digest::from_challenge(&c[0]).is_none(), "SHA-512-256 isn't supported");
    let c = parse_challenges([r#"Digest realm="r", nonce="n", algorithm=SHA-256, qop=auth, userhash=true"#]);
    let mut d = Digest::from_challenge(&c[0]).unwrap();
    let h = d.authorize("u", "p", "GET", "/", "c");
    let expected_user = Algorithm::Sha256.hash("u:r");
    assert!(h.contains(&format!("username=\"{expected_user}\"")) && h.ends_with("userhash=true"), "{h}");
}

#[test]
fn sess_and_legacy_digests() {
    let c = parse_challenges([r#"Digest realm="r", nonce="n", algorithm=MD5-sess, qop="auth""#]);
    let d = Digest::from_challenge(&c[0]).unwrap();
    assert!(d.sess);
    let ha1 = Algorithm::Md5.hash(&format!("{}:n:c", Algorithm::Md5.hash("u:r:p")));
    let ha2 = Algorithm::Md5.hash("GET:/x");
    assert_eq!(d.response("u", "p", "GET", "/x", "c", "00000001"), Algorithm::Md5.hash(&format!("{ha1}:n:00000001:c:auth:{ha2}")));

    // RFC 2069: no qop.
    let mut d = Digest::from_challenge(&parse_challenges([r#"Digest realm="r", nonce="n""#])[0]).unwrap();
    assert!(!d.qop_auth);
    let h = d.authorize("u", "p", "GET", "/x", "c");
    assert!(!h.contains("qop=") && !h.contains("nc="), "{h}");

    // auth-int only: unsupported.
    let c = parse_challenges([r#"Digest realm="r", nonce="n", qop="auth-int""#]);
    assert!(Digest::from_challenge(&c[0]).is_none());
}

#[test]
fn parses_multiple_challenges_and_prefers_sha256_digest() {
    let c = parse_challenges([
        r#"Basic realm="files", Digest realm="files", nonce="a,b", algorithm=MD5, qop="auth""#,
        r#"Digest realm="files", nonce="x\"y", algorithm=SHA-256, qop="auth", stale=TRUE"#,
        "Negotiate",
    ]);
    let schemes: Vec<&str> = c.iter().map(|c| c.scheme.as_str()).collect();
    assert_eq!(schemes, ["basic", "digest", "digest", "negotiate"]);
    assert_eq!(c[1].param("nonce"), Some("a,b"));
    assert_eq!(c[2].param("nonce"), Some("x\"y"));
    let (scheme, stale) = choose(&c).unwrap();
    match scheme {
        Scheme::Digest(d) => assert_eq!((d.algorithm, d.nonce.as_str()), (Algorithm::Sha256, "x\"y")),
        other => panic!("{other:?}"),
    }
    assert!(stale);
    assert!(matches!(choose(&parse_challenges([r#"Basic realm="x""#])), Ok((Scheme::Basic, false))));
    assert_eq!(choose(&parse_challenges(["Negotiate", "NTLM"])).unwrap_err(), "negotiate, ntlm");
    assert!(choose(&[]).is_err());
}

#[test]
fn basic_header_and_cnonce() {
    assert_eq!(basic("Aladdin", "open sesame"), "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
    let (a, b) = (cnonce(), cnonce());
    assert_eq!(a.len(), 32);
    assert_ne!(a, b);
}

#[test]
fn quoting_in_headers() {
    let mut d = Digest::from_challenge(&parse_challenges([r#"Digest realm="a\"b", nonce="n", qop=auth"#])[0]).unwrap();
    let h = d.authorize("we\"ird", "p", "GET", "/", "c");
    assert!(h.contains(r#"username="we\"ird""#) && h.contains(r#"realm="a\"b""#), "{h}");
}
