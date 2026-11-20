use super::*;

#[test]
fn pkce_matches_rfc7636_example() {
    // RFC 7636 appendix B.
    assert_eq!(
        pkce::challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
    let v = pkce::verifier();
    assert_eq!(v.len(), 43);
    assert!(
        v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    );
    assert_ne!(pkce::random_token(), pkce::random_token());
}

#[test]
fn form_encoding_escapes_reserved() {
    assert_eq!(
        form_encode(&[("scope", "a b/c".into()), ("x", "é&=".into())]),
        "scope=a%20b%2Fc&x=%C3%A9%26%3D"
    );
}

#[test]
fn provider_round_trips_protocol() {
    for p in Provider::ALL {
        assert_eq!(Provider::from_protocol(p.protocol()), Some(p));
    }
    assert_eq!(
        Provider::from_protocol(filewinch_core::Protocol::Sftp),
        None
    );
}
