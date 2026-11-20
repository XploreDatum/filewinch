//! OAuth loopback sign-in against a mock authorization server.

mod support;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use filewinch_cloud::oauth::{SignInOptions, sign_in_with};
use filewinch_cloud::{CancelToken, ClientConfig, Provider};
use filewinch_core::ErrorKind;
use serde_json::json;
use sha2::Digest;
use support::{Mock, Reply, parse_pairs};

/// What the fake browser should do with the consent URL.
#[derive(Clone)]
enum Browser {
    /// Approve: redirect back with `code` and the real state.
    Approve,
    /// Redirect back with this state instead of the real one.
    WrongState(&'static str),
    /// Redirect back with `error=…`.
    Deny(&'static str),
    /// Hit `/favicon.ico` first, then approve.
    StrayThenApprove,
    /// Never come back.
    Nothing,
}

#[derive(Default)]
struct Seen {
    authorize: Option<HashMap<String, String>>,
    page: Option<(u16, String)>,
}

fn http_get(url: &str) -> (u16, String) {
    let rest = url.strip_prefix("http://").unwrap();
    let (hostport, path) = rest.split_once('/').unwrap();
    let port = hostport.rsplit_once(':').unwrap().1;
    let mut s = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    write!(
        s,
        "GET /{path} HTTP/1.1\r\nHost: {hostport}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, out)
}

fn browser(mode: Browser, seen: Arc<Mutex<Seen>>) -> filewinch_cloud::oauth::OpenBrowser {
    Box::new(move |url: &str| {
        let q = parse_pairs(url.split_once('?').unwrap().1);
        seen.lock().unwrap().authorize = Some(q.clone());
        let (mode, seen) = (mode.clone(), seen.clone());
        std::thread::spawn(move || {
            let redirect = &q["redirect_uri"];
            let back = |query: String| http_get(&format!("{redirect}?{query}"));
            let page = match mode {
                Browser::Approve => back(format!("code=the-code&state={}", q["state"])),
                Browser::WrongState(s) => back(format!("code=the-code&state={s}")),
                Browser::Deny(e) => back(format!(
                    "error={e}&error_description=The+user+said+no&state={}",
                    q["state"]
                )),
                Browser::StrayThenApprove => {
                    let base = redirect.trim_end_matches("/callback");
                    assert_eq!(http_get(&format!("{base}/favicon.ico")).0, 404);
                    back(format!("code=the-code&state={}", q["state"]))
                }
                Browser::Nothing => return,
            };
            seen.lock().unwrap().page = Some(page);
        });
        Ok(())
    })
}

/// Mock token + account endpoints. Checks the PKCE verifier against the challenge.
fn auth_server(seen: Arc<Mutex<Seen>>, refresh_token: Option<&'static str>) -> Mock {
    Mock::start(move |r| match (r.method.as_str(), r.path.as_str()) {
        ("POST", "/oauth/token") => {
            let f = r.form();
            let auth = seen.lock().unwrap().authorize.clone().unwrap();
            let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(sha2::Sha256::digest(f["code_verifier"].as_bytes()));
            if f["grant_type"] != "authorization_code"
                || f["code"] != "the-code"
                || challenge != auth["code_challenge"]
                || f["redirect_uri"] != auth["redirect_uri"]
            {
                return Reply::json(
                    400,
                    json!({"error": "invalid_grant", "error_description": "code or verifier mismatch"}),
                );
            }
            let mut v =
                json!({"access_token": "access-1", "expires_in": 3600, "token_type": "Bearer"});
            if let Some(t) = refresh_token {
                v["refresh_token"] = t.into();
            }
            Reply::json(200, v)
        }
        ("GET", "/api/about") if r.bearer() == Some("access-1") => Reply::json(
            200,
            json!({"user": {"displayName": "Ana", "emailAddress": "ana@example.com"}}),
        ),
        ("POST", "/api/users/get_current_account") if r.bearer() == Some("access-1") => {
            Reply::json(
                200,
                json!({"email": "dee@example.com", "name": {"display_name": "Dee"}}),
            )
        }
        ("GET", "/api/users/me") if r.bearer() == Some("access-1") => {
            Reply::json(200, json!({"login": "bo@example.com", "name": "Bo"}))
        }
        _ => Reply::json(500, json!({"error": "unexpected"})),
    })
}

fn client(p: Provider) -> ClientConfig {
    let secret = p.needs_secret().then(|| "s3cret".to_string());
    ClientConfig::new("client-123", secret)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn run(
    p: Provider,
    mode: Browser,
    timeout: Duration,
    refresh: Option<&'static str>,
    cancel: CancelToken,
) -> (
    filewinch_core::Result<filewinch_cloud::SignedIn>,
    Arc<Mutex<Seen>>,
    Mock,
) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let mock = auth_server(seen.clone(), refresh);
    // A browser that does nothing never gets a page back; the others always do.
    let page_expected = !matches!(mode, Browser::Nothing);
    let opts = SignInOptions {
        timeout,
        endpoints: Some(mock.endpoints()),
        open_browser: browser(mode, seen.clone()),
        fixed_ports: vec![free_port(), free_port()],
    };
    let r = sign_in_with(p, &client(p), cancel, opts);
    // Wait for the fake browser to record the page it got back.
    let until = Instant::now() + if page_expected { Duration::from_secs(30) } else { Duration::ZERO };
    while seen.lock().unwrap().page.is_none()
        && Instant::now() < until
        && !matches!(r, Err(ref e) if e.kind == ErrorKind::Cancelled)
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    (r, seen, mock)
}

const LONG: Duration = Duration::from_secs(20);

#[test]
fn google_end_to_end_with_pkce() {
    let (r, seen, mock) = run(
        Provider::GoogleDrive,
        Browser::Approve,
        LONG,
        Some("refresh-xyz"),
        CancelToken::new(),
    );
    let signed = r.unwrap();
    assert_eq!(signed.refresh_token, "refresh-xyz");
    assert_eq!(signed.account_label, "ana@example.com");
    assert!(
        !format!("{signed:?}").contains("refresh-xyz"),
        "Debug must redact the token"
    );

    let seen = seen.lock().unwrap();
    let q = seen.authorize.as_ref().unwrap();
    assert_eq!(q["client_id"], "client-123");
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["access_type"], "offline");
    assert_eq!(q["scope"], "https://www.googleapis.com/auth/drive");
    assert!(
        q["redirect_uri"].starts_with("http://127.0.0.1:")
            && q["redirect_uri"].ends_with("/callback")
    );
    let (status, page) = seen.page.clone().unwrap();
    assert_eq!(status, 200);
    assert!(page.contains("You can close this tab and return to Filewinch."));

    let token = mock
        .requests()
        .into_iter()
        .find(|r| r.path == "/oauth/token")
        .unwrap()
        .form();
    assert_eq!(token["client_secret"], "s3cret");
    assert_eq!(token["client_id"], "client-123");
}

#[test]
fn dropbox_uses_first_free_fixed_port_and_fetches_label() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let mock = auth_server(seen.clone(), Some("rt"));
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let busy_port = busy.local_addr().unwrap().port();
    let free = free_port();
    let opts = SignInOptions {
        timeout: LONG,
        endpoints: Some(mock.endpoints()),
        open_browser: browser(Browser::Approve, seen.clone()),
        fixed_ports: vec![busy_port, free],
    };
    let signed = sign_in_with(
        Provider::Dropbox,
        &client(Provider::Dropbox),
        CancelToken::new(),
        opts,
    )
    .unwrap();
    assert_eq!(signed.account_label, "dee@example.com");
    let q = seen.lock().unwrap().authorize.clone().unwrap();
    assert_eq!(
        q["redirect_uri"],
        format!("http://127.0.0.1:{free}/callback")
    );
    assert_eq!(q["token_access_type"], "offline");
    let token = mock
        .requests()
        .into_iter()
        .find(|r| r.path == "/oauth/token")
        .unwrap()
        .form();
    assert!(
        !token.contains_key("client_secret"),
        "Dropbox uses PKCE without a secret"
    );
}

#[test]
fn fixed_ports_all_busy_is_a_clear_error() {
    let a = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = a.local_addr().unwrap().port();
    let opts = SignInOptions {
        fixed_ports: vec![port],
        open_browser: Box::new(|_| panic!("must not open")),
        ..SignInOptions::default()
    };
    let e = sign_in_with(
        Provider::Box,
        &client(Provider::Box),
        CancelToken::new(),
        opts,
    )
    .unwrap_err();
    assert!(e.message.contains("are all in use"), "{e}");
}

#[test]
fn onedrive_redirects_to_localhost_and_label_falls_back() {
    let (r, seen, _mock) = run(
        Provider::OneDrive,
        Browser::StrayThenApprove,
        LONG,
        Some("rt"),
        CancelToken::new(),
    );
    let signed = r.unwrap();
    // The mock has no /me route, so the label falls back to the provider name.
    assert_eq!(signed.account_label, "OneDrive");
    let q = seen.lock().unwrap().authorize.clone().unwrap();
    assert!(q["redirect_uri"].starts_with("http://localhost:"));
    assert!(q["scope"].contains("offline_access"));
}

#[test]
fn box_label_and_secret() {
    let (r, _seen, _mock) = run(
        Provider::Box,
        Browser::Approve,
        LONG,
        Some("rt"),
        CancelToken::new(),
    );
    assert_eq!(r.unwrap().account_label, "bo@example.com");
}

#[test]
fn wrong_state_is_rejected_without_exchanging_the_code() {
    let (r, seen, mock) = run(
        Provider::GoogleDrive,
        Browser::WrongState("forged"),
        LONG,
        Some("rt"),
        CancelToken::new(),
    );
    let e = r.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.message.contains("didn't match"), "{e}");
    let (status, page) = seen.lock().unwrap().page.clone().unwrap();
    assert_eq!(status, 400);
    assert!(page.contains("match"));
    assert_eq!(mock.count("POST", "/oauth/token"), 0);
}

#[test]
fn user_denied_access() {
    let (r, seen, mock) = run(
        Provider::Box,
        Browser::Deny("access_denied"),
        LONG,
        Some("rt"),
        CancelToken::new(),
    );
    let e = r.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert_eq!(e.message, "You didn't give Filewinch access to Box.");
    assert!(
        seen.lock()
            .unwrap()
            .page
            .as_ref()
            .unwrap()
            .1
            .contains("You didn't give Filewinch access to Box.")
    );
    assert_eq!(mock.count("POST", "/oauth/token"), 0);
}

#[test]
fn other_provider_error_is_reported() {
    let (r, _seen, _mock) = run(
        Provider::Dropbox,
        Browser::Deny("server_error"),
        LONG,
        Some("rt"),
        CancelToken::new(),
    );
    let e = r.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.message.contains("The user said no"), "{e}");
}

#[test]
fn times_out() {
    let started = Instant::now();
    let (r, _seen, _mock) = run(
        Provider::GoogleDrive,
        Browser::Nothing,
        Duration::from_millis(300),
        Some("rt"),
        CancelToken::new(),
    );
    let e = r.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.message.contains("timed out"), "{e}");
    // Nothing else ends it (the browser does nothing); the bound only catches a timeout read in
    // the wrong unit, with room for a loaded runner.
    assert!(started.elapsed() < Duration::from_secs(15), "{:?}", started.elapsed());
}

#[test]
fn cancel_stops_waiting() {
    let cancel = CancelToken::new();
    let c = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        c.cancel();
    });
    let started = Instant::now();
    let (r, _seen, _mock) = run(
        Provider::OneDrive,
        Browser::Nothing,
        LONG,
        Some("rt"),
        cancel,
    );
    // Cancelled (not the 20 s timeout's Auth error), and well before that timeout.
    assert_eq!(r.unwrap_err().kind, ErrorKind::Cancelled);
    assert!(started.elapsed() < LONG / 2, "{:?}", started.elapsed());
}

#[test]
fn missing_refresh_token_is_an_auth_error() {
    let (r, _seen, _mock) = run(
        Provider::GoogleDrive,
        Browser::Approve,
        LONG,
        None,
        CancelToken::new(),
    );
    let e = r.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.message.contains("offline access"), "{e}");
}

#[test]
fn client_config_missing_file_and_env() {
    let none = |_: &str| None;
    let e = ClientConfig::load_from(Provider::GoogleDrive, None, &none).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert_eq!(
        e.message,
        "Google Drive isn't set up yet: add a client ID (see docs/oauth-setup.md)"
    );
    // Box needs a secret too.
    let id_only = |k: &str| (k == "FILEWINCH_BOX_CLIENT_ID").then(|| "box-id".to_string());
    let e = ClientConfig::load_from(Provider::Box, None, &id_only).unwrap_err();
    assert_eq!(
        e.message,
        "Box isn't set up yet: add a client secret (see docs/oauth-setup.md)"
    );
    // Dropbox doesn't.
    let dbx = |k: &str| (k == "FILEWINCH_DROPBOX_CLIENT_ID").then(|| "dbx-id".to_string());
    assert_eq!(
        ClientConfig::load_from(Provider::Dropbox, None, &dbx)
            .unwrap()
            .client_id,
        "dbx-id"
    );
}

#[test]
fn client_config_file_wins_and_env_fills_secret() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("oauth-clients-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("oauth-clients.json");
    std::fs::write(
        &path,
        r#"{"google": {"client_id": "file-id"}, "onedrive": {"client_id": "ms-id"}}"#,
    )
    .unwrap();
    let env = |k: &str| match k {
        "FILEWINCH_GOOGLE_CLIENT_ID" => Some("env-id".to_string()),
        "FILEWINCH_GOOGLE_CLIENT_SECRET" => Some("env-secret".to_string()),
        _ => None,
    };
    let g = ClientConfig::load_from(Provider::GoogleDrive, Some(&path), &env).unwrap();
    assert_eq!(
        (g.client_id.as_str(), g.client_secret.as_deref()),
        ("file-id", Some("env-secret"))
    );
    assert!(!format!("{g:?}").contains("env-secret"));
    assert_eq!(
        ClientConfig::load_from(Provider::OneDrive, Some(&path), &env)
            .unwrap()
            .client_id,
        "ms-id"
    );

    std::fs::write(&path, "{ not json").unwrap();
    let e = ClientConfig::load_from(Provider::GoogleDrive, Some(&path), &env).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.message.contains("isn't valid"), "{e}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn sign_in_refuses_unconfigured_client() {
    let opts = SignInOptions {
        open_browser: Box::new(|_| panic!("must not open")),
        ..SignInOptions::default()
    };
    let e = sign_in_with(
        Provider::GoogleDrive,
        &ClientConfig::new("", None),
        CancelToken::new(),
        opts,
    )
    .unwrap_err();
    assert!(e.message.starts_with("Google Drive isn't set up yet"));
}
