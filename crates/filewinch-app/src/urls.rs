//! URLs for Copy URL: `sftp://user@host:port/path`, `file:///path` on the Mac.

use filewinch_core::{Protocol, RPath, Site};

fn escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn scheme(p: Protocol) -> &'static str {
    match p {
        Protocol::Local => "file",
        Protocol::Sftp => "sftp",
        Protocol::Ftp => "ftp",
        Protocol::Ftps => "ftps",
        Protocol::WebDav => "http",
        Protocol::WebDavs => "https",
        Protocol::S3 => "s3",
        Protocol::B2 => "b2",
        Protocol::AzureBlob => "azure",
        Protocol::Swift => "swift",
        Protocol::GoogleDrive => "gdrive",
        Protocol::Dropbox => "dropbox",
        Protocol::OneDrive => "onedrive",
        Protocol::Box => "box",
    }
}

/// The URL of `path` on `site` (`None`: This Mac). Never includes a password.
pub fn url_for(site: Option<&Site>, path: &RPath) -> String {
    let Some(site) = site.filter(|s| s.protocol != Protocol::Local) else {
        return format!("file://{}", escape(path.as_str()));
    };
    let host = site.host.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/');
    let user = if site.user.is_empty() || matches!(site.protocol, Protocol::S3 | Protocol::B2 | Protocol::AzureBlob) {
        String::new()
    } else {
        format!("{}@", escape(&site.user).replace('/', "%2F"))
    };
    let port = match (site.port, site.protocol.default_port()) {
        (Some(p), Some(d)) if p != d => format!(":{p}"),
        (Some(p), None) => format!(":{p}"),
        _ => String::new(),
    };
    format!("{}://{user}{host}{port}{}", scheme(site.protocol), escape(path.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(url_for(None, &RPath::new("/Users/me/a b.txt")), "file:///Users/me/a%20b.txt");
        let mut s = Site::new("x", "web", Protocol::Sftp);
        s.host = "web1.example.com".into();
        s.user = "deploy".into();
        s.port = Some(2222);
        assert_eq!(url_for(Some(&s), &RPath::new("/var/www")), "sftp://deploy@web1.example.com:2222/var/www");
        s.port = Some(22);
        assert_eq!(url_for(Some(&s), &RPath::root()), "sftp://deploy@web1.example.com/");
        let mut b = Site::new("y", "b", Protocol::S3);
        b.user = "AKIA".into();
        assert_eq!(url_for(Some(&b), &RPath::new("/bucket/k")), "s3:///bucket/k");
    }
}
