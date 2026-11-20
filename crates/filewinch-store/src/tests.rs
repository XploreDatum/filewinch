use std::fs;
use std::path::PathBuf;

use filewinch_core::{Auth, Protocol, SecretRef, Site, TlsMode};

use super::*;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = scratch_root().join(format!("store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn sftp_site(id: &str) -> Site {
    let mut s = Site::new(id, "web1", Protocol::Sftp);
    s.host = "web1.example.com".into();
    s.port = Some(2222);
    s.user = "deploy".into();
    s.auth = Auth::Password { secret: SecretRef(secret_account(id, "password")) };
    s.initial_path = Some("/var/www".into());
    s
}

#[test]
fn sites_round_trip_through_disk_without_secrets() {
    let dir = Scratch::new("roundtrip");
    let store = Store::new(&dir.0);
    let mut file = SitesFile::default();
    let folder = file.add_folder("Work");
    file.upsert(Favorite::new(sftp_site("a"), Some(folder.clone())));
    let mut s3 = Site::new("b", "Assets", Protocol::S3);
    s3.region = Some("eu-west-1".into());
    s3.path_style = true;
    s3.tls = TlsMode::Implicit;
    s3.passive = false;
    s3.auth = Auth::KeyFile { path: "/k".into(), passphrase: None };
    s3.trusted_fingerprints = vec!["SHA256:nThbg6kXUpJWGl7E1IGOCspRomTxdCARLviKw6E5SY8".into()];
    file.upsert(Favorite::new(s3, None));
    let mut swift = Site::new("c", "Cloud", Protocol::Swift);
    swift.project = Some("demo".into());
    swift.domain = Some("corp".into());
    file.upsert(Favorite::new(swift, None));
    store.save_sites(&file).unwrap();

    let json = fs::read_to_string(dir.0.join("sites.json")).unwrap();
    assert!(json.contains("\"protocol\": \"sftp\"") && json.contains("a/password"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(dir.0.join("sites.json")).unwrap().permissions().mode() & 0o777, 0o600);
    }
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1, "no temp file left behind");

    let loaded = store.load_sites();
    assert_eq!(loaded.warning, None);
    assert_eq!(loaded.value, file);
}

#[test]
fn old_favorite_trust_moves_onto_the_site_in_canonical_form() {
    let hex = "sha256:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:aa:bb:cc:dd:ee:ff";
    let canon = filewinch_core::normalize_fingerprint(hex).unwrap();
    let json = format!(
        r#"{{"favorites":[{{"site":{{"id":"a","name":"web","protocol":"webdavs","auth":{{"type":"none"}},
        "trusted_fingerprints":["{canon}"]}},"trusted":["{hex}","SHA256:not-a-fingerprint"]}}]}}"#
    );
    let file: SitesFile = serde_json::from_str(&json).unwrap();
    assert_eq!(file.favorites[0].site.trusted_fingerprints, vec![canon, "SHA256:not-a-fingerprint".to_string()]);
    let again = serde_json::to_string(&file).unwrap();
    assert!(!again.contains("\"trusted\""), "the old key isn't written back: {again}");
    assert_eq!(serde_json::from_str::<SitesFile>(&again).unwrap(), file);
}

#[test]
fn old_swift_user_notation_moves_into_project_and_domain() {
    let json = r#"{"favorites":[
        {"site":{"id":"a","name":"s","protocol":"swift","user":"corp/demo:alice","auth":{"type":"none"}}},
        {"site":{"id":"b","name":"s","protocol":"swift","user":"test:tester","auth":{"type":"none"}}},
        {"site":{"id":"c","name":"s","protocol":"sftp","user":"a:b","auth":{"type":"none"}}}]}"#;
    let file: SitesFile = serde_json::from_str(json).unwrap();
    let s = |i: usize| &file.favorites[i].site;
    assert_eq!((s(0).user.as_str(), s(0).project.as_deref(), s(0).domain.as_deref()), ("alice", Some("demo"), Some("corp")));
    assert_eq!((s(1).user.as_str(), s(1).project.as_deref(), s(1).domain.as_deref()), ("tester", Some("test"), None));
    assert_eq!((s(2).user.as_str(), s(2).project.as_deref()), ("a:b", None), "only Swift is rewritten");
}

#[test]
fn a_missing_file_is_empty_without_warning() {
    let dir = Scratch::new("missing");
    let loaded = Store::new(&dir.0).load_sites();
    assert_eq!(loaded.value, SitesFile::default());
    assert!(loaded.warning.is_none());
}

#[test]
fn a_corrupt_file_is_kept_aside_and_reported() {
    let dir = Scratch::new("corrupt");
    fs::create_dir_all(&dir.0).unwrap();
    fs::write(dir.0.join("sites.json"), b"{ not json").unwrap();
    let loaded = Store::new(&dir.0).load_sites();
    assert_eq!(loaded.value, SitesFile::default());
    let warning = loaded.warning.expect("a warning");
    assert!(warning.contains("damaged") && warning.contains("sites.json.corrupt-"), "{warning}");
    let names: Vec<String> = fs::read_dir(&dir.0).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert!(names.iter().any(|n| n.starts_with("sites.json.corrupt-")), "{names:?}");
    assert!(!dir.0.join("sites.json").exists());
}

#[test]
fn an_unknown_protocol_counts_as_damage() {
    let dir = Scratch::new("unknown");
    fs::create_dir_all(&dir.0).unwrap();
    fs::write(
        dir.0.join("sites.json"),
        br#"{"favorites":[{"site":{"id":"x","name":"x","protocol":"gopher","auth":{"type":"none"}}}]}"#,
    )
    .unwrap();
    assert!(Store::new(&dir.0).load_sites().warning.is_some());
}

#[test]
fn saving_into_an_unwritable_place_fails_with_a_message() {
    // A folder "inside" a file can't be made on any OS.
    let dir = Scratch::new("unwritable");
    fs::create_dir_all(&dir.0).unwrap();
    fs::write(dir.0.join("file"), b"").unwrap();
    let store = Store::new(dir.0.join("file").join("filewinch"));
    let e = store.save_prefs(&Prefs::default()).unwrap_err();
    assert!(e.message.contains("couldn't save prefs.json"), "{}", e.message);
}

#[test]
fn prefs_default_and_clamp() {
    let dir = Scratch::new("prefs");
    let store = Store::new(&dir.0);
    assert_eq!(store.load_prefs().value, Prefs::default());
    let p = Prefs { max_jobs: 0, retries: 99, bandwidth_limit: Some(0), ..Prefs::default() }.clamped();
    assert_eq!((p.max_jobs, p.retries, p.bandwidth_limit), (1, 20, None));
    let p2 = Prefs { show_hidden: true, appearance: Appearance::Dark, conflict: ConflictDefault::Skip, ..Prefs::default() };
    store.save_prefs(&p2).unwrap();
    assert_eq!(store.load_prefs().value, p2);
    // Older files without newer keys still load.
    fs::write(dir.0.join("prefs.json"), br#"{"max_jobs": 5}"#).unwrap();
    assert_eq!(store.load_prefs().value.max_jobs, 5);
}

#[test]
fn column_layout_round_trips_and_old_files_get_the_standard_columns() {
    let dir = Scratch::new("prefs-columns");
    let store = Store::new(&dir.0);
    let cols = vec![
        ColumnPref { id: ColumnId::Modified, width: Some(170) },
        ColumnPref { id: ColumnId::Name, width: None },
        ColumnPref { id: ColumnId::Permissions, width: Some(100) },
        ColumnPref { id: ColumnId::Size, width: None },
        ColumnPref { id: ColumnId::Kind, width: Some(90) },
    ];
    let p = Prefs { columns: cols.clone(), show_permissions: true, ..Prefs::default() };
    store.save_prefs(&p).unwrap();
    let back = store.load_prefs().value.clamped();
    assert_eq!(back.columns, cols);
    assert!(back.show_permissions);
    // A file from before columns were saved.
    fs::write(dir.0.join("prefs.json"), br#"{"show_hidden": true}"#).unwrap();
    assert_eq!(store.load_prefs().value.clamped().columns, ColumnPref::defaults());
}

#[test]
fn a_hand_edited_column_list_is_made_whole() {
    let dir = Scratch::new("prefs-columns-edited");
    let store = Store::new(&dir.0);
    fs::create_dir_all(&dir.0).unwrap();
    // An unknown column (from a newer version), a repeat, a missing width, out-of-range widths,
    // and Size, Modified and Kind left out.
    fs::write(
        dir.0.join("prefs.json"),
        br#"{"columns": [{"id": "owner", "width": 80}, {"id": "permissions", "width": 5}, {"id": "name"}, {"id": "permissions", "width": 300}, {"id": "kind", "width": 99999}]}"#,
    )
    .unwrap();
    let loaded = store.load_prefs();
    assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
    let cols = loaded.value.clamped().columns;
    let ids: Vec<ColumnId> = cols.iter().map(|c| c.id).collect();
    assert_eq!(ids, [ColumnId::Permissions, ColumnId::Name, ColumnId::Kind, ColumnId::Size, ColumnId::Modified]);
    assert_eq!(cols[0].width, Some(ColumnPref::WIDTH_RANGE.0));
    assert_eq!(cols[2].width, Some(ColumnPref::WIDTH_RANGE.1));
    assert_eq!(cols[3].width, None);
}

#[test]
fn duplicate_gets_a_new_id_name_and_secret() {
    let mut f = SitesFile::default();
    f.upsert(Favorite::new(sftp_site("a"), None));
    let (copy, moves) = f.duplicate("a").unwrap();
    assert_ne!(copy.site.id, "a");
    assert_eq!(copy.site.name, "web1 copy");
    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0].0, SecretRef("a/password".into()));
    assert_eq!(moves[0].1, SecretRef(format!("{}/password", copy.site.id)));
    let (copy2, _) = f.duplicate("a").unwrap();
    assert_eq!(copy2.site.name, "web1 copy 2");
    assert_eq!(f.favorites[1].site.id, copy2.site.id, "the copy sits right after the original");
    assert!(f.duplicate("nope").is_none());
}

#[test]
fn folders_and_search() {
    let mut f = SitesFile::default();
    let work = f.add_folder("Work");
    f.upsert(Favorite::new(sftp_site("a"), None));
    let mut b = Site::new("b", "Photos", Protocol::Dropbox);
    b.user = "me@example.com".into();
    f.upsert(Favorite::new(b, None));
    f.move_to("a", Some(&work));
    assert_eq!(f.get("a").unwrap().folder.as_deref(), Some(work.as_str()));
    f.move_to("b", Some("no-such-folder"));
    assert_eq!(f.get("b").unwrap().folder, None);
    assert!(f.rename_folder(&work, "Clients"));
    f.remove_folder(&work);
    assert_eq!(f.get("a").unwrap().folder, None, "orphans move to the top level");

    let ids = |q: &str| f.search(q).map(|x| x.site.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids("WEB1.example"), ["a"]);
    assert_eq!(ids("dropbox"), ["b"]);
    assert_eq!(ids("example"), ["a", "b"]);
    assert_eq!(ids(""), ["a", "b"]);
    assert!(f.remove("a").is_some());
    assert!(f.remove("a").is_none());
}

#[test]
fn recents_dedupe_and_cap() {
    let mut r = Recents::default();
    for i in 0..20 {
        let mut s = sftp_site(&format!("s{i}"));
        s.host = format!("h{i}");
        r.touch(&s, i);
    }
    assert_eq!(r.items.len(), 12);
    assert_eq!(r.items[0].site.id, "s19");
    let mut again = sftp_site("s15");
    again.host = "h15".into();
    r.touch(&again, 100);
    assert_eq!(r.items[0].at, 100);
    assert_eq!(r.items.iter().filter(|x| x.site.id == "s15").count(), 1);
}

#[test]
fn memory_secrets() {
    let m = MemorySecrets::default();
    assert_eq!(m.get("x").unwrap(), None);
    m.set("x", "pw").unwrap();
    assert_eq!(m.get("x").unwrap().as_deref(), Some("pw"));
    m.delete("x").unwrap();
    m.delete("x").unwrap();
    assert_eq!(m.get("x").unwrap(), None);
}

/// Touches the real secret store (login Keychain, Credential Manager, Secret Service), which
/// can prompt: run with `--ignored` by hand.
#[test]
#[ignore]
fn system_secrets_round_trip() {
    let k = SystemSecrets;
    let _same_type: &dyn SecretStore = &Keychain;
    let account = format!("test-{}/password", new_id());
    k.set(&account, "throwaway-local-secret").unwrap();
    assert_eq!(k.get(&account).unwrap().as_deref(), Some("throwaway-local-secret"));
    k.set(&account, "changed").unwrap();
    assert_eq!(k.get(&account).unwrap().as_deref(), Some("changed"));
    k.delete(&account).unwrap();
    assert_eq!(k.get(&account).unwrap(), None);
}

#[test]
fn ssh_config_hosts() {
    let src = r#"
# comment
Host web1 web1-alias
    HostName 10.0.0.5
    User deploy
    Port 2200
    IdentityFile "/keys/id_ed25519"
Host *.internal *
    User ignored
Host bare
Match host foo
    User nobody
Host web1
    User second-value-ignored
"#;
    let hosts = parse_ssh_config(src);
    assert_eq!(hosts.len(), 3);
    assert_eq!(
        hosts[0],
        SshHost {
            alias: "web1".into(),
            host: "10.0.0.5".into(),
            user: Some("deploy".into()),
            port: Some(2200),
            identity_file: Some("/keys/id_ed25519".into()),
        }
    );
    assert_eq!(hosts[1].alias, "web1-alias");
    assert_eq!(hosts[1].host, "10.0.0.5");
    assert_eq!(hosts[2], SshHost { alias: "bare".into(), host: "bare".into(), ..SshHost::default() });
    assert!(parse_ssh_config("garbage without value\nPort").is_empty());
}

#[test]
fn wildcards() {
    assert!(ssh_config::wild("*.conf", "a.conf"));
    assert!(ssh_config::wild("h?st", "host"));
    assert!(!ssh_config::wild("*.conf", "a.txt"));
}

/// Ephemeral scratch for one test run. The build drive is exFAT (no Unix permissions, and
/// AppleDouble `._` files), so these filesystem tests use the system's temp volume (APFS).
fn scratch_root() -> PathBuf {
    std::env::temp_dir().join("filewinch-tests")
}

#[test]
fn default_dir_is_the_os_config_folder() {
    let dirs = filewinch_os::dirs::Dirs::current().expect("a home folder");
    assert_eq!(Store::default_dir(), Some(dirs.config.clone()));
    #[cfg(target_os = "macos")]
    if std::env::var_os(filewinch_os::dirs::ROOT_ENV).is_none() {
        // Where Filewinch has always kept its files on macOS: nothing moves on upgrade.
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(dirs.config, home.join("Library/Application Support/Filewinch"));
    }
    assert_eq!(KEYCHAIN_SERVICE, filewinch_os::APP_ID);
}

#[test]
fn ssh_includes_resolve_under_the_ssh_folder_and_windows_paths_survive() {
    let dir = Scratch::new("ssh");
    let (home, ssh) = (dir.0.join("home"), dir.0.join("home").join(".ssh"));
    fs::create_dir_all(ssh.join("conf.d")).unwrap();
    fs::write(
        ssh.join("config"),
        "Include conf.d/*.conf\nHost win\n  HostName win.example.com\n  IdentityFile C:\\Users\\me\\.ssh\\id_ed25519\n\
         Host quoted\n  IdentityFile \"C:\\Program Files\\keys\\id rsa\"\nHost tilde\n  IdentityFile ~/.ssh/id_tilde\n",
    )
    .unwrap();
    fs::write(ssh.join("conf.d").join("a.conf"), "Host included\n  User inc\n").unwrap();
    fs::write(ssh.join("conf.d").join("b.txt"), "Host not-included\n").unwrap();
    let hosts = ssh_config::read_ssh_config_in(&home, &ssh);
    let get = |a: &str| hosts.iter().find(|h| h.alias == a).cloned();
    assert_eq!(get("included").and_then(|h| h.user).as_deref(), Some("inc"));
    assert!(get("not-included").is_none());
    assert_eq!(get("win").unwrap().identity_file.as_deref(), Some(r"C:\Users\me\.ssh\id_ed25519"));
    assert_eq!(get("win").unwrap().host, "win.example.com");
    assert_eq!(get("quoted").unwrap().identity_file.as_deref(), Some(r"C:\Program Files\keys\id rsa"));
    assert_eq!(get("tilde").unwrap().identity_file, Some(home.join(".ssh/id_tilde").to_string_lossy().into_owned()));
    // A missing config is no hosts, not an error.
    assert!(ssh_config::read_ssh_config_in(&home, &dir.0.join("nowhere")).is_empty());
}

mod secret_errors_map {
    use filewinch_core::ErrorKind;
    use filewinch_os::Os;

    use crate::secret_errors::{Op, text_means_no_service, to_error};

    fn platform(text: &str) -> keyring::Error {
        keyring::Error::PlatformFailure(Box::new(std::io::Error::other(text.to_owned())))
    }

    #[test]
    fn linux_without_a_secret_service_says_what_to_install() {
        for text in [
            "no secret service provider or dbus session found",
            "zbus error: org.freedesktop.DBus.Error.ServiceUnknown: The name org.freedesktop.secrets was not provided by any .service files",
            "org.freedesktop.DBus.Error.Spawn.ChildExited: Process org.freedesktop.secrets exited with status 1",
        ] {
            for op in [Op::Read, Op::Save, Op::Delete] {
                let e = to_error(&platform(text), op, Os::Linux);
                assert_eq!(e.kind, ErrorKind::Unsupported, "{text}");
                assert_eq!(e.message, "No password store is running. Install and unlock GNOME Keyring or KWallet.");
                assert_eq!(e.message, crate::NO_SECRET_SERVICE);
                assert!(e.detail.unwrap().contains(text));
            }
        }
        // No session bus socket at all.
        let gone = keyring::Error::PlatformFailure(Box::new(std::io::Error::from(std::io::ErrorKind::NotFound)));
        assert_eq!(to_error(&gone, Op::Read, Os::Linux).message, crate::NO_SECRET_SERVICE);
        assert!(!text_means_no_service("org.freedesktop.Secret.Error.IsLocked"));
    }

    #[test]
    fn messages_name_each_os_store() {
        let e = to_error(&platform("boom"), Op::Save, Os::Windows);
        assert_eq!((e.kind, e.message.as_str()), (ErrorKind::Other, "Windows Credential Manager couldn't save the password."));
        assert_eq!(to_error(&platform("boom"), Op::Read, Os::Mac).message, "Your Keychain couldn't read a saved password.");
        assert_eq!(to_error(&platform("boom"), Op::Delete, Os::Linux).message, "Your keyring couldn't delete the saved password.");
        // D-Bus names mean nothing off Linux.
        let ss = platform("org.freedesktop.DBus.Error.ServiceUnknown");
        assert_eq!(to_error(&ss, Op::Save, Os::Mac).kind, ErrorKind::Other);
    }

    #[test]
    fn locked_store_is_permission_denied() {
        let e = to_error(&keyring::Error::NoStorageAccess(Box::new(std::io::Error::other("locked"))), Op::Read, Os::Mac);
        assert_eq!(e.kind, ErrorKind::PermissionDenied);
        assert_eq!(e.message, "Your Keychain is locked or didn't allow Filewinch to read a saved password.");
    }

    #[test]
    fn bad_text_too_long_and_duplicates_are_explained_without_the_secret() {
        let e = to_error(&keyring::Error::BadEncoding(b"s3cr\xffet".to_vec()), Op::Read, Os::Linux);
        assert_eq!(e.message, "A saved password in your keyring isn't valid text.");
        assert!(!format!("{e:?}").contains("s3cr"), "the secret never appears");

        let e = to_error(&keyring::Error::TooLong("password".into(), 2560), Op::Save, Os::Windows);
        assert_eq!(
            e.message,
            "This password or token is too long to save in Windows Credential Manager (the limit is 2560 bytes)."
        );
        let e = to_error(&keyring::Error::TooLong("target".into(), 32767), Op::Save, Os::Windows);
        assert_eq!(e.message, "Windows Credential Manager couldn't save the password: its target is longer than the limit of 32767.");

        let e = to_error(&keyring::Error::Ambiguous(Vec::new()), Op::Read, Os::Linux);
        assert!(e.message.starts_with("There's more than one saved password for this site in your keyring."), "{}", e.message);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_cancel_and_auth_codes() {
        let code = |c: i32| keyring::Error::PlatformFailure(Box::new(security_framework::base::Error::from_code(c)));
        let e = to_error(&code(-128), Op::Read, Os::Mac);
        assert_eq!((e.kind, e.message.as_str()), (ErrorKind::Cancelled, "Unlocking your Keychain was cancelled, so Filewinch couldn't read a saved password."));
        assert_eq!(to_error(&code(-25293), Op::Save, Os::Mac).kind, ErrorKind::PermissionDenied);
        assert_eq!(to_error(&code(-25308), Op::Save, Os::Mac).kind, ErrorKind::PermissionDenied);
        assert_eq!(to_error(&code(-50), Op::Save, Os::Mac).kind, ErrorKind::Other);
    }

    #[cfg(windows)]
    #[test]
    fn windows_codes() {
        let code = |c: u32| keyring::Error::PlatformFailure(Box::new(keyring::windows::Error(c)));
        assert_eq!(to_error(&code(5), Op::Save, Os::Windows).kind, ErrorKind::PermissionDenied);
        assert_eq!(to_error(&code(1223), Op::Save, Os::Windows).kind, ErrorKind::Cancelled);
        assert_eq!(to_error(&code(1312), Op::Save, Os::Windows).kind, ErrorKind::Unsupported);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_secret_service_errors() {
        use secret_service::Error as Ss;
        let ss = |e: Ss| keyring::Error::PlatformFailure(Box::new(e));
        let e = to_error(&ss(Ss::Unavailable), Op::Save, Os::Linux);
        assert_eq!((e.kind, e.message.as_str()), (ErrorKind::Unsupported, crate::NO_SECRET_SERVICE));
        assert_eq!(to_error(&ss(Ss::Prompt), Op::Save, Os::Linux).kind, ErrorKind::Cancelled);
        assert_eq!(to_error(&keyring::Error::NoStorageAccess(Box::new(Ss::Locked)), Op::Read, Os::Linux).kind, ErrorKind::PermissionDenied);
    }
}
