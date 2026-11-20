use super::*;

#[test]
fn parses_list_result_with_entities_and_namespaces() {
    let doc = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>b</Name><IsTruncated>false</IsTruncated>
  <Contents><Key>a &amp; b&#x41;.txt</Key><Size>3</Size></Contents>
  <Contents><Key><![CDATA[c<d]]></Key><Size>0</Size><Owner/></Contents>
</ListBucketResult>"#;
    let root = parse(doc).unwrap();
    assert_eq!(root.name, "ListBucketResult");
    let keys: Vec<&str> = root.all("Contents").filter_map(|c| c.text_of("Key")).collect();
    assert_eq!(keys, ["a & bA.txt", "c<d"]);
    assert_eq!(root.text_of("IsTruncated"), Some("false"));
}

#[test]
fn rejects_garbage() {
    assert!(parse("<a><b></a>").is_err());
    assert!(parse("<a>").is_err());
    assert!(parse("").is_err());
}

#[test]
fn escapes() {
    assert_eq!(escape("a<b>&\"c'"), "a&lt;b&gt;&amp;&quot;c&apos;");
}
