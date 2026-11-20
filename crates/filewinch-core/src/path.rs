use std::fmt;

/// An absolute, `/`-separated path on a remote. Always starts with `/`, never ends with one
/// (except the root), never holds empty, `.` or `..` components. Object stores map the first
/// component to the bucket/container when the site has none fixed; cloud drives map paths to
/// item IDs internally.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RPath(String);

impl RPath {
    pub fn root() -> Self {
        RPath("/".into())
    }

    /// Normalises any input: collapses repeated slashes, resolves `.` and `..` (never above
    /// root), and adds the leading slash.
    pub fn new(s: &str) -> Self {
        let mut parts: Vec<&str> = Vec::new();
        for c in s.split('/') {
            match c {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                c => parts.push(c),
            }
        }
        RPath(format!("/{}", parts.join("/")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0 == "/"
    }

    /// Appends one name (a name containing `/` is treated as a relative path).
    pub fn join(&self, name: &str) -> Self {
        RPath::new(&format!("{}/{}", self.0, name))
    }

    pub fn parent(&self) -> Option<Self> {
        if self.is_root() {
            return None;
        }
        let i = self.0.rfind('/').unwrap_or(0);
        Some(if i == 0 { RPath::root() } else { RPath(self.0[..i].into()) })
    }

    /// The last component; empty for the root.
    pub fn name(&self) -> &str {
        &self.0[self.0.rfind('/').map_or(0, |i| i + 1)..]
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|c| !c.is_empty())
    }

    /// True if `self` is `other` or inside it.
    pub fn starts_with(&self, other: &RPath) -> bool {
        other.is_root() || self.0 == other.0 || self.0.starts_with(&format!("{}/", other.0))
    }
}

impl fmt::Display for RPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::RPath;

    #[test]
    fn normalises() {
        assert_eq!(RPath::new("").as_str(), "/");
        assert_eq!(RPath::new("a//b/./c/").as_str(), "/a/b/c");
        assert_eq!(RPath::new("/a/../../b").as_str(), "/b");
    }

    #[test]
    fn parent_name_join() {
        let p = RPath::new("/var/www/index.html");
        assert_eq!(p.name(), "index.html");
        assert_eq!(p.parent().unwrap().as_str(), "/var/www");
        assert_eq!(RPath::new("/x").parent().unwrap(), RPath::root());
        assert_eq!(RPath::root().parent(), None);
        assert_eq!(RPath::root().join("a b").as_str(), "/a b");
        assert!(p.starts_with(&RPath::new("/var")));
        assert!(!RPath::new("/variable").starts_with(&RPath::new("/var")));
    }
}
