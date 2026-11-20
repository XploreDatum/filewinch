use super::*;

fn at(secs: u64) -> Option<SystemTime> {
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

// 2024-03-05T14:07:09Z
const T: u64 = 1_709_647_629;

#[test]
fn apache_mod_dav_with_lp_prefixes() {
    let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:ns0="DAV:">
<D:response xmlns:lp1="DAV:" xmlns:lp2="http://apache.org/dav/props/">
<D:href>/dav/docs/</D:href>
<D:propstat>
<D:prop>
<lp1:resourcetype><D:collection/></lp1:resourcetype>
<lp1:getlastmodified>Tue, 05 Mar 2024 14:07:09 GMT</lp1:getlastmodified>
<lp1:getetag>"1000-61a3"</lp1:getetag>
<D:getcontenttype>httpd/unix-directory</D:getcontenttype>
</D:prop>
<D:status>HTTP/1.1 200 OK</D:status>
</D:propstat>
<D:propstat>
<D:prop><lp1:getcontentlength/></D:prop>
<D:status>HTTP/1.1 404 Not Found</D:status>
</D:propstat>
</D:response>
<D:response xmlns:lp1="DAV:" xmlns:lp2="http://apache.org/dav/props/">
<D:href>/dav/docs/report%202024.pdf</D:href>
<D:propstat>
<D:prop>
<lp1:resourcetype/>
<lp1:getcontentlength>48213</lp1:getcontentlength>
<lp1:getlastmodified>Tue, 05 Mar 2024 14:07:09 GMT</lp1:getlastmodified>
<lp1:getetag>"bc55-61a3"</lp1:getetag>
<D:getcontenttype>application/pdf</D:getcontenttype>
<lp2:executable>F</lp2:executable>
</D:prop>
<D:status>HTTP/1.1 200 OK</D:status>
</D:propstat>
</D:response>
</D:multistatus>"#;
    let r = parse(xml.as_bytes()).unwrap();
    assert_eq!(r.len(), 2);
    assert_eq!(r[0].href, "/dav/docs/");
    assert!(r[0].is_dir);
    assert_eq!(r[0].size, None);
    assert_eq!(r[0].modified, at(T));
    assert_eq!(
        r[1],
        PropEntry {
            href: "/dav/docs/report%202024.pdf".into(),
            is_dir: false,
            size: Some(48213),
            modified: at(T),
            etag: Some("\"bc55-61a3\"".into()),
            content_type: Some("application/pdf".into()),
        }
    );
}

#[test]
fn nginx_dav_ext_default_namespace_prefix() {
    let xml = r#"<?xml version="1.0" encoding="utf-8" ?>
<D:multistatus xmlns:D="DAV:">
<D:response>
<D:href>/files/</D:href>
<D:propstat><D:prop>
<D:displayname>files</D:displayname>
<D:getlastmodified>Tue, 05 Mar 2024 14:07:09 GMT</D:getlastmodified>
<D:resourcetype><D:collection/></D:resourcetype>
</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
</D:response>
<D:response>
<D:href>/files/caf%C3%A9%20menu.txt</D:href>
<D:propstat><D:prop>
<D:getcontentlength>12</D:getcontentlength>
<D:getlastmodified>Tue, 05 Mar 2024 14:07:09 GMT</D:getlastmodified>
<D:resourcetype></D:resourcetype>
</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
</D:response>
</D:multistatus>"#;
    let r = parse(xml.as_bytes()).unwrap();
    assert_eq!(r.len(), 2);
    assert!(r[0].is_dir && !r[1].is_dir);
    assert_eq!(r[1].size, Some(12));
    assert_eq!(crate::target::href_segments(&r[1].href), ["files", "café menu.txt"]);
}

#[test]
fn nextcloud_with_oc_namespace_and_lowercase_prefix() {
    let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns">
 <d:response>
  <d:href>/remote.php/dav/files/alice/Photos/</d:href>
  <d:propstat>
   <d:prop>
    <d:getlastmodified>Tue, 05 Mar 2024 14:07:09 GMT</d:getlastmodified>
    <d:resourcetype><d:collection/></d:resourcetype>
    <d:getetag>&quot;65e7273d1c2a5&quot;</d:getetag>
    <oc:size>2048</oc:size>
    <oc:permissions>RGDNVCK</oc:permissions>
   </d:prop>
   <d:status>HTTP/1.1 200 OK</d:status>
  </d:propstat>
  <d:propstat>
   <d:prop><d:getcontentlength/><d:getcontenttype/></d:prop>
   <d:status>HTTP/1.1 404 Not Found</d:status>
  </d:propstat>
 </d:response>
 <d:response>
  <d:href>/remote.php/dav/files/alice/Photos/Tom%20%26%20Jerry%20%231.jpg</d:href>
  <d:propstat>
   <d:prop>
    <d:getlastmodified>Tue, 05 Mar 2024 14:07:09 GMT</d:getlastmodified>
    <d:getcontentlength>2048</d:getcontentlength>
    <d:resourcetype/>
    <d:getetag>"abc"</d:getetag>
    <d:getcontenttype>image/jpeg</d:getcontenttype>
   </d:prop>
   <d:status>HTTP/1.1 200 OK</d:status>
  </d:propstat>
 </d:response>
</d:multistatus>"#;
    let r = parse(xml.as_bytes()).unwrap();
    assert_eq!(r.len(), 2);
    assert!(r[0].is_dir);
    assert_eq!(r[0].size, None, "oc:size must not be taken as getcontentlength");
    assert_eq!(r[0].etag.as_deref(), Some("\"65e7273d1c2a5\""));
    assert_eq!(r[1].content_type.as_deref(), Some("image/jpeg"));
    assert_eq!(
        crate::target::href_segments(&r[1].href).last().unwrap(),
        "Tom & Jerry #1.jpg"
    );
}

#[test]
fn iis_absolute_urls_and_typed_dates() {
    let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<a:multistatus xmlns:b="urn:uuid:c2f41010-65b3-11d1-a29f-00aa00c14882/" xmlns:a="DAV:">
<a:response><a:href>http://iis.example.com/share/</a:href><a:propstat><a:status>HTTP/1.1 200 OK</a:status><a:prop>
<a:getcontentlength b:dt="int">0</a:getcontentlength>
<a:getlastmodified b:dt="dateTime.rfc1123">Tue, 05 Mar 2024 14:07:09 GMT</a:getlastmodified>
<a:resourcetype><a:collection/></a:resourcetype>
<a:iscollection b:dt="boolean">1</a:iscollection>
</a:prop></a:propstat></a:response>
<a:response><a:href>http://iis.example.com/share/Budget%20Q1.xlsx</a:href><a:propstat><a:status>HTTP/1.1 200 OK</a:status><a:prop>
<a:getcontentlength b:dt="int">9911</a:getcontentlength>
<a:getlastmodified b:dt="dateTime.rfc1123">Tue, 05 Mar 2024 14:07:09 GMT</a:getlastmodified>
<a:resourcetype/>
<a:getcontenttype>application/vnd.openxmlformats-officedocument.spreadsheetml.sheet</a:getcontenttype>
</a:prop></a:propstat></a:response>
</a:multistatus>"#;
    let r = parse(xml.as_bytes()).unwrap();
    assert_eq!(r.len(), 2);
    assert!(r[0].is_dir);
    assert_eq!(r[1].size, Some(9911));
    assert_eq!(r[1].modified, at(T));
    assert_eq!(crate::target::href_segments(&r[1].href), ["share", "Budget Q1.xlsx"]);
}

#[test]
fn default_namespace_and_odd_values() {
    let xml = r#"<multistatus xmlns="DAV:">
<response><href>/x/</href>
<propstat><prop><resourcetype><collection/></resourcetype><getcontentlength></getcontentlength>
<getlastmodified>not a date</getlastmodified></prop><status>HTTP/1.1 200 OK</status></propstat></response>
<response><href>/x/%E2%9C%93%2520%25.txt</href>
<propstat><prop><getcontentlength> 5 </getcontentlength><getlastmodified>2024-03-05T14:07:09Z</getlastmodified></prop></propstat></response>
<response><href>/x/gone</href><status>HTTP/1.1 404 Not Found</status></response>
</multistatus>"#;
    let r = parse(xml.as_bytes()).unwrap();
    assert_eq!(r.len(), 2, "the 404 response is dropped");
    assert!(r[0].is_dir);
    assert_eq!((r[0].size, r[0].modified), (None, None));
    assert_eq!(r[1].size, Some(5));
    assert_eq!(r[1].modified, at(T));
    assert_eq!(crate::target::href_segments(&r[1].href), ["x", "✓%20%.txt"]);
}

#[test]
fn foreign_elements_with_dav_names_are_ignored() {
    // An `href` or `getcontentlength` from another namespace must not be picked up.
    let xml = r#"<D:multistatus xmlns:D="DAV:" xmlns:X="urn:x"><D:response><D:href>/a</D:href>
<D:propstat><D:prop><X:getcontentlength>99</X:getcontentlength><D:getcontentlength>1</D:getcontentlength>
<X:wrap><D:getcontentlength>77</D:getcontentlength></X:wrap></D:prop>
<D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
    let r = parse(xml.as_bytes()).unwrap();
    assert_eq!(r[0].size, Some(1));
}

#[test]
fn rejects_non_multistatus_and_broken_xml() {
    assert!(parse(b"<html><body>Index of /</body></html>").is_err());
    // Truncated: either an error or no complete response, never a half-read entry.
    assert!(parse(b"<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/a</D:href>").is_ok_and(|r| r.is_empty())
        || parse(b"<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/a</D:href>").is_err());
    assert!(parse(b"<D:multistatus xmlns:D=\"DAV:\"></D:wrong>").is_err());
    assert!(parse(b"").is_err());
}

#[test]
fn dates() {
    assert_eq!(parse_date("Tue, 05 Mar 2024 14:07:09 GMT"), at(T));
    assert_eq!(parse_date("2024-03-05T19:37:09.123+05:30"), at(T));
    assert_eq!(parse_date("2024-03-05T14:07:09"), None);
    assert_eq!(parse_date("yesterday"), None);
    assert_eq!(parse_date(""), None);
}
