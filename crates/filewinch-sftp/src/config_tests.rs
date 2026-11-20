use super::*;

fn setup(text: &str) -> (PathBuf, PathBuf) {
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let home = std::env::temp_dir().join(format!("fw-sshcfg-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".ssh/conf.d")).unwrap();
    let cfg = home.join(".ssh/config");
    std::fs::write(&cfg, text).unwrap();
    (home, cfg)
}

#[test]
fn first_value_wins_and_patterns() {
    let (home, cfg) = setup(
        "# comment\nHost web1\n  HostName web1.example.com\n  User deploy\n  Port 2222\n  IdentityFile ~/.ssh/id_web\n\
         Host *.example.com !secret.example.com\n  User ops\n\
         Host *\n  User fallback\n  Port=22\n  IdentityFile %d/.ssh/id_%h\n  IdentitiesOnly yes\n",
    );
    let c = resolve_in(&cfg, &home, "web1");
    assert_eq!(c.hostname.as_deref(), Some("web1.example.com"));
    assert_eq!(c.user.as_deref(), Some("deploy"));
    assert_eq!(c.port, Some(2222));
    assert_eq!(c.identity_files, vec![home.join(".ssh/id_web"), home.join(".ssh/id_web1.example.com")]);
    assert!(c.identities_only);

    assert_eq!(resolve_in(&cfg, &home, "db.example.com").user.as_deref(), Some("ops"));
    assert_eq!(resolve_in(&cfg, &home, "secret.example.com").user.as_deref(), Some("fallback"));
    assert_eq!(resolve_in(&cfg, &home, "other").port, Some(22));
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn include_and_match_and_missing_file() {
    let (home, cfg) = setup("Include conf.d/*.conf\nMatch host x\n  User nope\nHost y\n  User why\n");
    std::fs::write(home.join(".ssh/conf.d/a.conf"), "Host x\n  HostName x.internal\n").unwrap();
    let c = resolve_in(&cfg, &home, "x");
    assert_eq!(c.hostname.as_deref(), Some("x.internal"));
    assert_eq!(c.user, None);
    assert_eq!(resolve_in(&cfg, &home, "y").user.as_deref(), Some("why"));
    assert_eq!(resolve_in(&home.join("missing"), &home, "x"), HostConfig::default());
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn wildcards() {
    assert!(wildcard("*", "anything"));
    assert!(wildcard("web?", "WEB1"));
    assert!(!wildcard("web?", "web12"));
    assert!(wildcard("*.example.*", "a.example.com"));
}
