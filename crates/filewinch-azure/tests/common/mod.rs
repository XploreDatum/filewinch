//! Test helpers: an Azurite blob server (run through Node, killed on drop) and a scripted
//! `tiny_http` mock of the Blob service.
#![allow(dead_code)]

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use filewinch_azure::AzureConnector;
use filewinch_core::{Auth, Connector, Protocol, Remote, Result, SecretRef, Site};

/// Azurite's published development account and key (not secrets).
pub const DEV_ACCOUNT: &str = "devstoreaccount1";
pub const DEV_KEY: &str =
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";

pub fn tmp_base() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}

/// A site for `http://127.0.0.1:<port>/<account>` with `secret` as the Keychain password.
pub fn site(port: u16, account: &str) -> Site {
    let mut s = Site::new("test", "test", Protocol::AzureBlob);
    s.host = format!("http://127.0.0.1:{port}/{account}");
    s.auth = Auth::Password { secret: SecretRef("azure-test".into()) };
    s
}

pub fn connect(site: &Site, secret: &str) -> Result<Box<dyn Remote>> {
    let secret = secret.to_string();
    AzureConnector.connect(site, std::sync::Arc::new(filewinch_core::FnSecrets(move |_: &filewinch_core::SecretRef| Ok(Some(secret.clone())))))
}

pub fn connect_paged(site: &Site, secret: &str, page: u32) -> Result<Box<dyn Remote>> {
    let secret = secret.to_string();
    filewinch_azure::testing::connect_with_page_size(site, &move |_| Ok(Some(secret.clone())), page)
}

/// A running Azurite blob service. Killed (whole process group) on drop.
pub struct Azurite {
    pub port: u16,
    child: Child,
    scratch: PathBuf,
}

impl Azurite {
    /// Starts Azurite, or returns None (with a note on stderr) when Node isn't available
    /// or it won't start; the mock tests cover the protocol then.
    pub fn start() -> Option<Azurite> {
        let scratch = filewinch_testkit::scratch_dir(&tmp_base(), "azurite");
        for _ in 0..3 {
            let port = filewinch_testkit::free_port();
            let Some(mut cmd) = azurite_command() else {
                eprintln!("SKIP: node/npx not found, so Azurite can't run; relying on the mock tests.");
                return None;
            };
            cmd.args(["--silent", "--location"])
                .arg(scratch.join("data"))
                .args(["--blobHost", "127.0.0.1", "--blobPort", &port.to_string()])
                .args(["--skipApiVersionCheck", "--loose"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            #[cfg(unix)]
            std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("SKIP: couldn't start Azurite ({e}); relying on the mock tests.");
                    return None;
                }
            };
            // The first npx run downloads the package: allow a long wait.
            let deadline = Instant::now() + Duration::from_secs(240);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = child.try_wait() {
                    break;
                }
                if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    return Some(Azurite { port, child, scratch });
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            kill_group(&mut child);
        }
        eprintln!("SKIP: Azurite didn't start; relying on the mock tests.");
        None
    }

    pub fn site(&self) -> Site {
        site(self.port, DEV_ACCOUNT)
    }

    pub fn remote(&self) -> Box<dyn Remote> {
        connect(&self.site(), DEV_KEY).expect("connect to Azurite")
    }
}

impl Drop for Azurite {
    fn drop(&mut self) {
        kill_group(&mut self.child);
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.scratch);
        }
    }
}

fn kill_group(child: &mut Child) {
    #[cfg(unix)]
    // `--` makes `-<pgid>` a process group, never an option: procps-ng `kill` on Linux misread
    // `kill -KILL -<pgid>` and signalled every process of the user, taking the CI runner down.
    // The guard keeps a bad id from ever becoming `-1` or `-0`.
    if child.id() > 1 {
        let _ = Command::new("kill").args(["-s", "KILL", "--", &format!("-{}", child.id())]).status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// `node <cached azurite main.js>` when npx has the package cached, else
/// `npx --yes --package azurite azurite-blob` (downloads into the npm cache, no global
/// install).
fn azurite_command() -> Option<Command> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let npx_cache = home.join(".npm/_npx");
    if let Ok(dirs) = std::fs::read_dir(&npx_cache) {
        for d in dirs.flatten() {
            let main = d.path().join("node_modules/azurite/dist/src/blob/main.js");
            if main.exists()
                && let Some(node) = which("node") {
                    let mut c = Command::new(node);
                    c.arg(main);
                    return Some(c);
                }
        }
    }
    let npx = which("npx")?;
    let mut c = Command::new(npx);
    c.args(["--yes", "--package", "azurite", "azurite-blob"]);
    Some(c)
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| Path::new(p).is_file())
}

/// One request the mock saw.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub url: String,
    /// Lowercased names.
    pub headers: Vec<(String, String)>,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
    pub fn has_query(&self, part: &str) -> bool {
        self.url.split_once('?').is_some_and(|(_, q)| q.split('&').any(|p| p == part))
    }
}

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Reply {
    pub fn new(status: u16, body: &str) -> Reply {
        Reply { status, headers: Vec::new(), body: body.into() }
    }
    pub fn h(mut self, k: &str, v: &str) -> Reply {
        self.headers.push((k.into(), v.into()));
        self
    }
}

/// A scripted Blob service: `handler` answers every request; all requests are recorded.
pub struct Mock {
    pub port: u16,
    pub seen: Arc<Mutex<Vec<Seen>>>,
}

impl Mock {
    pub fn start(handler: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> Mock {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("mock server");
        let port = server.server_addr().to_ip().expect("ip addr").port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for mut req in server.incoming_requests() {
                let s = Seen {
                    method: req.method().as_str().to_string(),
                    url: req.url().to_string(),
                    headers: req
                        .headers()
                        .iter()
                        .map(|h| (h.field.as_str().as_str().to_ascii_lowercase(), h.value.as_str().to_string()))
                        .collect(),
                };
                let mut sink = Vec::new();
                let _ = std::io::Read::read_to_end(req.as_reader(), &mut sink);
                log.lock().unwrap().push(s.clone());
                let r = handler(&s);
                let mut resp = tiny_http::Response::from_string(r.body).with_status_code(r.status);
                for (k, v) in r.headers {
                    resp.add_header(tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap());
                }
                let _ = req.respond(resp);
            }
        });
        Mock { port, seen }
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

pub const EMPTY_CONTAINERS: &str =
    "<?xml version=\"1.0\" encoding=\"utf-8\"?><EnumerationResults><Containers/><NextMarker/></EnumerationResults>";
