//! Where requests go: the endpoint from the site, the region to sign with, and whether a
//! bucket is addressed virtual-host style (`bucket.host/key`) or path style (`host/bucket/key`).

use filewinch_core::{Error, ErrorKind, Result, Site};

use crate::sigv4::encode_path;

pub(crate) const DEFAULT_REGION: &str = "us-east-1";

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Endpoint {
    pub https: bool,
    /// Host name without port. For AWS this is rebuilt per region.
    pub host: String,
    /// Explicit port, only when it isn't the scheme's default.
    pub port: Option<u16>,
    /// Amazon S3 itself: the host follows the region (`s3.<region>.amazonaws.com`).
    pub aws: Option<AwsHost>,
    pub force_path_style: bool,
}

/// How an AWS host is spelled, so a region switch can rebuild it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AwsHost {
    /// `amazonaws.com` or `amazonaws.com.cn`.
    pub suffix: String,
}

impl Endpoint {
    /// Parses the site's host (empty means AWS) and port. Returns the endpoint and the region
    /// to start with (the site's, else one inferred from the host, else `us-east-1`).
    pub fn from_site(site: &Site) -> Result<(Endpoint, String)> {
        let raw = site.host.trim();
        let (https, rest) = if let Some(r) = strip_prefix_ci(raw, "https://") {
            (true, r)
        } else if let Some(r) = strip_prefix_ci(raw, "http://") {
            (false, r)
        } else {
            (true, raw)
        };
        // Any path after the authority is ignored: S3 endpoints live at the root.
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        let (host, url_port) = split_port(authority)?;
        let host = host.to_ascii_lowercase();
        let port = site.port.or(url_port).filter(|p| *p != if https { 443 } else { 80 });
        let site_region = site.region.as_deref().map(str::trim).filter(|r| !r.is_empty());

        let aws = if host.is_empty() {
            Some(AwsHost { suffix: "amazonaws.com".into() })
        } else if host.ends_with(".amazonaws.com") || host.ends_with(".amazonaws.com.cn") {
            let suffix = if host.ends_with(".cn") { "amazonaws.com.cn" } else { "amazonaws.com" };
            Some(AwsHost { suffix: suffix.into() })
        } else {
            None
        };
        let region = site_region
            .map(str::to_string)
            .or_else(|| infer_region(&host))
            .unwrap_or_else(|| DEFAULT_REGION.to_string());
        let mut ep = Endpoint { https, host, port, aws, force_path_style: site.path_style };
        if ep.aws.is_some() {
            ep.host = ep.aws_host(&region);
        }
        if ep.host.is_empty() {
            return Err(Error::new(ErrorKind::Other, "The S3 endpoint address is empty."));
        }
        Ok((ep, region))
    }

    fn aws_host(&self, region: &str) -> String {
        match &self.aws {
            Some(aws) => format!("s3.{region}.{}", aws.suffix),
            None => self.host.clone(),
        }
    }

    /// The host (no port) for requests signed with `region`.
    pub fn host_for(&self, region: &str) -> String {
        if self.aws.is_some() { self.aws_host(region) } else { self.host.clone() }
    }

    /// A name for messages ("s3.eu-west-1.amazonaws.com", "127.0.0.1:9000").
    pub fn display(&self) -> String {
        match self.port {
            Some(p) => format!("{}:{p}", self.host),
            None => self.host.clone(),
        }
    }

    /// Whether `bucket` must be addressed path style on this endpoint.
    pub fn path_style(&self, bucket: &str) -> bool {
        self.force_path_style
            || is_ip_or_localhost(&self.host)
            || !dns_compatible(bucket)
            || (self.https && bucket.contains('.'))
    }

    /// (authority for the Host header, encoded path, full URL without query).
    pub fn locate(&self, region: &str, bucket: Option<&str>, key: &str) -> (String, String, String) {
        let host = self.host_for(region);
        let port = self.port.map(|p| format!(":{p}")).unwrap_or_default();
        let (authority, path) = match bucket {
            None => (format!("{host}{port}"), "/".to_string()),
            Some(b) if self.path_style(b) => {
                let key = if key.is_empty() { String::new() } else { format!("/{key}") };
                (format!("{host}{port}"), encode_path(&format!("/{b}{key}")))
            }
            Some(b) => (format!("{b}.{host}{port}"), encode_path(&format!("/{key}"))),
        };
        let scheme = if self.https { "https" } else { "http" };
        let url = format!("{scheme}://{authority}{path}");
        (authority, path, url)
    }
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &s[prefix.len()..])
}

fn split_port(authority: &str) -> Result<(&str, Option<u16>)> {
    let bad = || Error::new(ErrorKind::Other, "The S3 endpoint's port isn't a number.");
    if let Some(rest) = authority.strip_prefix('[') {
        // [IPv6]:port
        let end = rest.find(']').ok_or_else(|| Error::new(ErrorKind::Other, "The S3 endpoint address isn't valid."))?;
        let port = match &rest[end + 1..] {
            "" => None,
            p => Some(p.strip_prefix(':').ok_or_else(bad)?.parse().map_err(|_| bad())?),
        };
        return Ok((&authority[..end + 2], port));
    }
    match authority.rsplit_once(':') {
        Some((h, p)) => Ok((h, Some(p.parse().map_err(|_| bad())?))),
        None => Ok((authority, None)),
    }
}

/// Region implied by well-known endpoint spellings.
pub(crate) fn infer_region(host: &str) -> Option<String> {
    if host.ends_with(".r2.cloudflarestorage.com") || host == "r2.cloudflarestorage.com" {
        return Some("auto".into());
    }
    let labels: Vec<&str> = host.split('.').collect();
    let n = labels.len();
    let domain = |d: &[&str]| n > d.len() && labels[n - d.len()..] == *d;
    if domain(&["amazonaws", "com"]) || domain(&["amazonaws", "com", "cn"]) {
        // s3.<region>.amazonaws.com, s3-<region>.amazonaws.com, s3.dualstack.<region>...
        let first = labels[0];
        if let Some(r) = first.strip_prefix("s3-").filter(|r| *r != "external-1") {
            return Some(r.to_string());
        }
        let candidates = &labels[1..n - if domain(&["amazonaws", "com", "cn"]) { 3 } else { 2 }];
        return candidates.iter().rev().find(|l| l.contains('-')).map(|r| r.to_string());
    }
    if (domain(&["backblazeb2", "com"]) || domain(&["wasabisys", "com"])) && n == 4 && labels[0] == "s3" {
        return Some(labels[1].to_string());
    }
    if domain(&["digitaloceanspaces", "com"]) && n >= 3 {
        return Some(labels[n - 3].to_string());
    }
    None
}

fn is_ip_or_localhost(host: &str) -> bool {
    host == "localhost"
        || host.ends_with(".localhost")
        || host.starts_with('[')
        || host.parse::<std::net::Ipv4Addr>().is_ok()
}

/// Bucket names usable as a DNS label set (virtual-hosted style).
pub(crate) fn dns_compatible(bucket: &str) -> bool {
    let len_ok = (3..=63).contains(&bucket.len());
    let chars_ok = bucket.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'.');
    let ends_ok = bucket.bytes().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && bucket.bytes().last().is_some_and(|c| c.is_ascii_alphanumeric());
    len_ok
        && chars_ok
        && ends_ok
        && !bucket.contains("..")
        && !bucket.contains(".-")
        && !bucket.contains("-.")
        && bucket.parse::<std::net::Ipv4Addr>().is_err()
}
