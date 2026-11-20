//! HTTP authentication for WebDAV: `WWW-Authenticate` parsing, Basic, and Digest
//! (RFC 7616: MD5, SHA-256 and their `-sess` variants, `qop=auth`, `userhash`, nonce
//! counting). Passwords and `Authorization` values never leave this module except as the
//! header itself.

use base64::Engine as _;
use sha2::{Digest as _, Sha256};

/// One challenge from a `WWW-Authenticate` header: scheme (lowercase) and its parameters
/// (names lowercase, values unquoted).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Challenge {
    pub scheme: String,
    pub params: Vec<(String, String)>,
}

impl Challenge {
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// Parses every challenge in the given header values. A single value may hold several
/// challenges (`Digest realm="a", nonce="b", Basic realm="a"`).
pub(crate) fn parse_challenges<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<Challenge> {
    let mut out = Vec::new();
    for value in values {
        let mut p = Cursor { s: value.as_bytes(), i: 0 };
        loop {
            p.skip(|c| c == b',' || c.is_ascii_whitespace());
            let scheme = p.token();
            if scheme.is_empty() {
                break;
            }
            let mut params = Vec::new();
            loop {
                p.skip(|c| c == b',' || c.is_ascii_whitespace());
                if p.peek().is_none() {
                    break;
                }
                let save = p.i;
                let name = p.token();
                p.skip(|c| c.is_ascii_whitespace());
                if name.is_empty() {
                    p.i = save + 1; // a stray byte: skip it
                    continue;
                }
                if p.peek() != Some(b'=') {
                    // Not `name=value`: the next challenge's scheme (a token68 credential,
                    // which no scheme we use has, also lands here as a harmless "scheme").
                    p.i = save;
                    break;
                }
                p.i += 1;
                p.skip(|c| c.is_ascii_whitespace());
                let value = p.value();
                params.push((name.to_ascii_lowercase(), value));
            }
            out.push(Challenge { scheme: scheme.to_ascii_lowercase(), params });
        }
    }
    out
}

struct Cursor<'a> {
    s: &'a [u8],
    i: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }
    fn skip(&mut self, f: impl Fn(u8) -> bool) {
        while self.peek().is_some_and(&f) {
            self.i += 1;
        }
    }
    fn token(&mut self) -> String {
        let start = self.i;
        self.skip(|c| c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c));
        String::from_utf8_lossy(&self.s[start..self.i]).into_owned()
    }
    fn value(&mut self) -> String {
        if self.peek() == Some(b'"') {
            self.i += 1;
            let mut v = Vec::new();
            while let Some(c) = self.peek() {
                self.i += 1;
                match c {
                    b'"' => break,
                    b'\\' => {
                        if let Some(n) = self.peek() {
                            v.push(n);
                            self.i += 1;
                        }
                    }
                    c => v.push(c),
                }
            }
            String::from_utf8_lossy(&v).into_owned()
        } else {
            let start = self.i;
            self.skip(|c| c != b',' && !c.is_ascii_whitespace());
            String::from_utf8_lossy(&self.s[start..self.i]).into_owned()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Algorithm {
    Md5,
    Sha256,
}

impl Algorithm {
    fn hash(self, data: &str) -> String {
        match self {
            Algorithm::Md5 => format!("{:x}", md5::compute(data.as_bytes())),
            Algorithm::Sha256 => {
                Sha256::digest(data.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
            }
        }
    }
}

/// A Digest session: the server's challenge plus our nonce count.
#[derive(Clone, Debug)]
pub(crate) struct Digest {
    pub realm: String,
    pub nonce: String,
    pub opaque: Option<String>,
    pub algorithm: Algorithm,
    pub sess: bool,
    /// `true` when the server offered `qop=auth`; `false` for legacy RFC 2069 digests.
    pub qop_auth: bool,
    pub userhash: bool,
    pub nc: u32,
}

impl Digest {
    /// `None` for algorithms we don't implement or a `qop` list with only `auth-int`.
    pub fn from_challenge(c: &Challenge) -> Option<Digest> {
        if c.scheme != "digest" {
            return None;
        }
        let alg = c.param("algorithm").unwrap_or("MD5").to_ascii_uppercase();
        let (algorithm, sess) = match alg.as_str() {
            "MD5" => (Algorithm::Md5, false),
            "MD5-SESS" => (Algorithm::Md5, true),
            "SHA-256" => (Algorithm::Sha256, false),
            "SHA-256-SESS" => (Algorithm::Sha256, true),
            _ => return None,
        };
        let qop_auth = match c.param("qop") {
            None => false,
            Some(q) => {
                if !q.split(',').any(|v| v.trim().eq_ignore_ascii_case("auth")) {
                    return None;
                }
                true
            }
        };
        Some(Digest {
            realm: c.param("realm").unwrap_or_default().to_string(),
            nonce: c.param("nonce")?.to_string(),
            opaque: c.param("opaque").map(str::to_string),
            algorithm,
            sess,
            qop_auth,
            userhash: c.param("userhash").is_some_and(|v| v.eq_ignore_ascii_case("true")),
            nc: 0,
        })
    }

    /// The `Authorization` value for one request; bumps the nonce count.
    pub fn authorize(&mut self, user: &str, pass: &str, method: &str, uri: &str, cnonce: &str) -> String {
        self.nc = self.nc.wrapping_add(1);
        let nc = format!("{:08x}", self.nc);
        let response = self.response(user, pass, method, uri, cnonce, &nc);
        let h = self.algorithm;
        let username = if self.userhash { h.hash(&format!("{user}:{}", self.realm)) } else { quote(user) };
        let alg_name = match (h, self.sess) {
            (Algorithm::Md5, false) => "MD5",
            (Algorithm::Md5, true) => "MD5-sess",
            (Algorithm::Sha256, false) => "SHA-256",
            (Algorithm::Sha256, true) => "SHA-256-sess",
        };
        let mut out = format!(
            "Digest username=\"{username}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", algorithm={alg_name}, response=\"{response}\"",
            quote(&self.realm),
            quote(&self.nonce),
            quote(uri),
        );
        if self.qop_auth {
            out.push_str(&format!(", qop=auth, nc={nc}, cnonce=\"{}\"", quote(cnonce)));
        }
        if let Some(o) = &self.opaque {
            out.push_str(&format!(", opaque=\"{}\"", quote(o)));
        }
        if self.userhash {
            out.push_str(", userhash=true");
        }
        out
    }

    /// The `response` value (RFC 7616 §3.4.1).
    pub fn response(&self, user: &str, pass: &str, method: &str, uri: &str, cnonce: &str, nc: &str) -> String {
        let h = self.algorithm;
        let mut ha1 = h.hash(&format!("{user}:{}:{pass}", self.realm));
        if self.sess {
            ha1 = h.hash(&format!("{ha1}:{}:{cnonce}", self.nonce));
        }
        let ha2 = h.hash(&format!("{method}:{uri}"));
        if self.qop_auth {
            h.hash(&format!("{ha1}:{}:{nc}:{cnonce}:auth:{ha2}", self.nonce))
        } else {
            h.hash(&format!("{ha1}:{}:{ha2}", self.nonce))
        }
    }
}

fn quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

pub(crate) fn basic(user: &str, pass: &str) -> String {
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}")))
}

/// A fresh client nonce: 16 random bytes as hex.
pub(crate) fn cnonce() -> String {
    let mut b = [0u8; 16];
    if getrandom::fill(&mut b).is_err() {
        // Uniqueness is all Digest needs from a cnonce; fall back to the clock.
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        b.copy_from_slice(&n.as_nanos().to_le_bytes());
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// What we send on each request once the server has told us how to sign in.
#[derive(Clone, Debug)]
pub(crate) enum Scheme {
    /// Not challenged yet: send nothing.
    None,
    Basic,
    Digest(Digest),
}

/// The scheme to use for a 401's challenges: Digest (SHA-256 before MD5) when we can do it,
/// else Basic. `Err` names what the server asked for when we support none of it.
pub(crate) fn choose(challenges: &[Challenge]) -> Result<(Scheme, bool), String> {
    let digests: Vec<(Digest, bool)> = challenges
        .iter()
        .filter_map(|c| {
            let stale = c.param("stale").is_some_and(|v| v.eq_ignore_ascii_case("true"));
            Digest::from_challenge(c).map(|d| (d, stale))
        })
        .collect();
    if let Some((d, stale)) = digests.iter().find(|(d, _)| d.algorithm == Algorithm::Sha256).or(digests.first()) {
        return Ok((Scheme::Digest(d.clone()), *stale));
    }
    if challenges.iter().any(|c| c.scheme == "basic") {
        return Ok((Scheme::Basic, false));
    }
    let names: Vec<&str> = challenges.iter().map(|c| c.scheme.as_str()).collect();
    Err(if names.is_empty() { "no WWW-Authenticate challenge".into() } else { names.join(", ") })
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
