//! Unit tests for crate-private helpers.

use crate::status::scrub;

#[test]
fn scrub_redacts_secret_query_params_only() {
    let s = scrub("GET /c/b?sv=2024-01-01&sig=abc%2Fdef&se=2030 HTTP/1.1");
    assert_eq!(s, "GET /c/b?sv=2024-01-01&sig=REDACTED&se=2030 HTTP/1.1");
    assert_eq!(
        scrub("X-Amz-Security-Token=tok&a=1"),
        "X-Amz-Security-Token=REDACTED&a=1"
    );
    assert_eq!(scrub("config=1 design=2"), "config=1 design=2");
    assert_eq!(scrub("password="), "password=");
}

#[test]
fn scrub_redacts_secret_xml_elements() {
    assert_eq!(
        scrub("<Key>a.txt</Key><SignatureProvided>xyz</SignatureProvided>"),
        "<Key>a.txt</Key><SignatureProvided>REDACTED</SignatureProvided>"
    );
    assert_eq!(
        scrub("<Token>t</Token><Code>X</Code>"),
        "<Token>REDACTED</Token><Code>X</Code>"
    );
    // Unclosed or malformed markup is left alone.
    assert_eq!(scrub("<Token>t"), "<Token>t");
    assert_eq!(scrub("a < b"), "a < b");
}
