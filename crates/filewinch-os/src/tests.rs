use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::dirs::{Dirs, ROOT_ENV, home_from};
use crate::launch::{Launch, find_with_exts, parse_command, plan_open, plan_reveal};
use crate::names::{check, windows_safe};
use crate::Os;

fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
    move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| OsString::from(v))
}

#[test]
fn mac_dirs_keep_todays_paths() {
    let d = Dirs::from_env(Os::Mac, env(&[("HOME", "/Users/me")])).unwrap();
    assert_eq!(d.config, Path::new("/Users/me/Library/Application Support/Filewinch"));
    assert_eq!(d.cache, Path::new("/Users/me/Library/Caches/app.xpdt.filewinch"));
    assert_eq!(d.ssh(), Path::new("/Users/me/.ssh"));
    // XDG variables mean nothing on a Mac.
    let d2 = Dirs::from_env(Os::Mac, env(&[("HOME", "/Users/me"), ("XDG_CONFIG_HOME", "/x")])).unwrap();
    assert_eq!(d, d2);
}

#[test]
fn windows_dirs() {
    let vars = [("USERPROFILE", r"C:\Users\me"), ("APPDATA", r"C:\Users\me\AppData\Roaming"), ("LOCALAPPDATA", r"C:\Users\me\AppData\Local")];
    let d = Dirs::from_env(Os::Windows, env(&vars)).unwrap();
    assert_eq!(d.config, PathBuf::from(r"C:\Users\me\AppData\Roaming").join("Filewinch"));
    assert_eq!(d.cache, PathBuf::from(r"C:\Users\me\AppData\Local").join("Filewinch").join("Cache"));
    assert_eq!(d.ssh(), PathBuf::from(r"C:\Users\me").join(".ssh"));
    // Without APPDATA / LOCALAPPDATA, the profile's AppData folders.
    let d = Dirs::from_env(Os::Windows, env(&[("USERPROFILE", r"C:\Users\me")])).unwrap();
    assert_eq!(d.config, PathBuf::from(r"C:\Users\me").join("AppData").join("Roaming").join("Filewinch"));
    assert_eq!(d.cache, PathBuf::from(r"C:\Users\me").join("AppData").join("Local").join("Filewinch").join("Cache"));
}

#[test]
fn windows_home_fallbacks() {
    assert_eq!(home_from(Os::Windows, env(&[("HOMEDRIVE", "D:"), ("HOMEPATH", r"\Users\me")])), Some(PathBuf::from(r"D:\Users\me")));
    assert_eq!(home_from(Os::Windows, env(&[("HOME", "/c/Users/me")])), Some(PathBuf::from("/c/Users/me")));
    assert_eq!(home_from(Os::Windows, env(&[("USERPROFILE", ""), ("HOMEDRIVE", "D:")])), None);
    // HOME isn't read on Windows when USERPROFILE is set.
    assert_eq!(home_from(Os::Windows, env(&[("USERPROFILE", r"C:\U"), ("HOME", "/h")])), Some(PathBuf::from(r"C:\U")));
    assert_eq!(home_from(Os::Linux, env(&[("USERPROFILE", r"C:\U")])), None);
}

#[test]
fn linux_dirs_follow_xdg() {
    let d = Dirs::from_env(Os::Linux, env(&[("HOME", "/home/me")])).unwrap();
    assert_eq!(d.config, Path::new("/home/me/.config/filewinch"));
    assert_eq!(d.cache, Path::new("/home/me/.cache/filewinch"));
    let d = Dirs::from_env(Os::Linux, env(&[("HOME", "/home/me"), ("XDG_CONFIG_HOME", "/cfg"), ("XDG_CACHE_HOME", "/var/cache/me")])).unwrap();
    assert_eq!(d.config, Path::new("/cfg/filewinch"));
    assert_eq!(d.cache, Path::new("/var/cache/me/filewinch"));
    // A relative XDG variable is ignored, as the spec says.
    let d = Dirs::from_env(Os::Linux, env(&[("HOME", "/home/me"), ("XDG_CONFIG_HOME", "rel")])).unwrap();
    assert_eq!(d.config, Path::new("/home/me/.config/filewinch"));
}

#[test]
fn no_home_no_dirs_and_root_override() {
    assert_eq!(Dirs::from_env(Os::Linux, env(&[])), None);
    assert_eq!(Dirs::from_env(Os::Mac, env(&[("HOME", "")])), None);
    let d = Dirs::from_env(Os::Windows, env(&[("USERPROFILE", r"C:\U"), (ROOT_ENV, "/scratch")])).unwrap();
    assert_eq!(d.config, Path::new("/scratch/config"));
    assert_eq!(d.cache, Path::new("/scratch/cache"));
}

#[test]
fn names_on_unix() {
    for os in [Os::Mac, Os::Linux] {
        assert_eq!(check(os, "notes: draft?.txt"), None);
        assert_eq!(check(os, "CON"), None);
        assert_eq!(check(os, "trailing. "), None);
        assert!(check(os, "a/b").is_some());
        assert!(check(os, "").is_some());
        assert!(check(os, "  ").is_some());
        assert!(check(os, "..").is_some());
        assert!(check(os, "a\0b").is_some());
    }
}

#[test]
fn names_windows_refuses() {
    for bad in ["a<b", "a>b", "a:b", "a\"b", "a/b", "a\\b", "a|b", "a?b", "a*b", "tab\there", "dot.", "space ", "CON", "con", "nul.txt", "Com1", "LPT9.log", "COM¹", "aux .txt"] {
        assert!(check(Os::Windows, bad).is_some(), "{bad:?} should be refused");
    }
    for ok in ["CONSOLE", "COM0", "COM10", "LPT", "nul2.txt", ".gitignore", "report final.pdf", "a.b.c", "日本語.txt"] {
        assert_eq!(check(Os::Windows, ok), None, "{ok:?} should be fine");
    }
    assert!(check(Os::Windows, &"x".repeat(256)).is_some());
    assert_eq!(check(Os::Windows, &"x".repeat(255)), None);
    assert!(check(Os::Windows, "a:b").unwrap().contains("< > : \" / \\ | ? *"));
}

#[test]
fn windows_safe_substitutes() {
    assert!(matches!(windows_safe("fine.txt"), std::borrow::Cow::Borrowed("fine.txt")));
    assert_eq!(windows_safe("12:30 <draft>?.txt"), "12：30 ＜draft＞？.txt");
    assert_eq!(windows_safe(r#"a"b|c*d\e"#), "a＂b｜c＊d＼e");
    assert_eq!(windows_safe("bell\u{7}"), "bell␇");
    assert_eq!(windows_safe("end."), "end．");
    assert_eq!(windows_safe("end. ."), "end．␠．");
    assert_eq!(windows_safe("CON"), "CON_");
    assert_eq!(windows_safe("nul.tar.gz"), "nul_.tar.gz");
    assert_eq!(windows_safe("com1"), "com1_");
    assert_eq!(windows_safe(".."), "．．");
    let long = format!("{}.txt", "y".repeat(300));
    let short = windows_safe(&long);
    assert_eq!(short.encode_utf16().count(), 255);
    assert!(short.ends_with(".txt"));
    // Idempotent, and always acceptable.
    for name in ["12:30 <draft>?.txt", "CON", "end. .", "a\u{1}b", &long, "..", "con .txt", "x:"] {
        let once = windows_safe(name).into_owned();
        assert_eq!(check(Os::Windows, &once), None, "{name:?} -> {once:?}");
        assert_eq!(windows_safe(&once), once);
    }
}

#[test]
fn editor_commands_split_per_os() {
    assert_eq!(parse_command(Os::Mac, " Visual Studio Code "), Ok(vec!["Visual Studio Code".into()]));
    assert_eq!(parse_command(Os::Linux, "code --new-window"), Ok(vec!["code".into(), "--new-window".into()]));
    assert_eq!(parse_command(Os::Linux, r#"'/opt/My Editor/bin/ed' -a "x y" z\ w"#), Ok(vec!["/opt/My Editor/bin/ed".into(), "-a".into(), "x y".into(), "z w".into()]));
    assert_eq!(parse_command(Os::Linux, r#"ed "say \"hi\"""#), Ok(vec!["ed".into(), "say \"hi\"".into()]));
    assert!(parse_command(Os::Linux, "ed 'open").is_err());
    assert!(parse_command(Os::Linux, "   ").is_err());
    assert_eq!(
        parse_command(Os::Windows, r#""C:\Program Files\Notepad++\notepad++.exe" -multiInst"#),
        Ok(vec![r"C:\Program Files\Notepad++\notepad++.exe".into(), "-multiInst".into()])
    );
    assert_eq!(parse_command(Os::Windows, r"C:\Tools\vim.exe"), Ok(vec![r"C:\Tools\vim.exe".into()]));
    assert_eq!(parse_command(Os::Windows, r#"ed "a\"b" "c\\" d"#), Ok(vec!["ed".into(), "a\"b".into(), "c\\".into(), "d".into()]));
    assert_eq!(parse_command(Os::Windows, r#"ed "say ""hi""""#), Ok(vec!["ed".into(), "say \"hi\"".into()]));
    assert!(parse_command(Os::Windows, r#""C:\Program Files\x.exe"#).is_err());
}

#[test]
fn open_plans() {
    let f = Path::new("/c/a b.txt");
    let run = |p: &str, a: &[&str]| Launch::Run { program: p.into(), args: a.iter().map(OsString::from).collect() };
    assert_eq!(plan_open(Os::Mac, f, None), Ok(run("/usr/bin/open", &["/c/a b.txt"])));
    assert_eq!(plan_open(Os::Mac, f, Some("BBEdit")), Ok(run("/usr/bin/open", &["-a", "BBEdit", "/c/a b.txt"])));
    assert_eq!(plan_open(Os::Mac, f, Some("  ")), Ok(run("/usr/bin/open", &["/c/a b.txt"])));
    assert_eq!(plan_open(Os::Linux, f, None), Ok(run("xdg-open", &["/c/a b.txt"])));
    assert_eq!(plan_open(Os::Windows, Path::new("https://example.com/?a=1&b=2"), None), Ok(Launch::ShellOpen("https://example.com/?a=1&b=2".into())));
    assert_eq!(
        plan_open(Os::Linux, f, Some("code --wait")),
        Ok(Launch::Start { program: "code".into(), args: vec!["--wait".into(), "/c/a b.txt".into()] })
    );
    assert_eq!(
        plan_open(Os::Windows, Path::new(r"C:\c\a b.txt"), Some(r#""C:\Program Files\Sublime Text\subl.exe""#)),
        Ok(Launch::Start { program: r"C:\Program Files\Sublime Text\subl.exe".into(), args: vec![r"C:\c\a b.txt".into()] })
    );
    assert!(plan_open(Os::Linux, f, Some("ed 'x")).is_err());
}

#[test]
fn reveal_plans() {
    assert_eq!(plan_reveal(Os::Mac, Path::new("/d/a.txt")), vec![Launch::Run { program: "/usr/bin/open".into(), args: vec!["-R".into(), "/d/a.txt".into()] }]);
    assert_eq!(plan_reveal(Os::Windows, Path::new(r"C:\d\a b.txt")), vec![Launch::RunRaw { program: "explorer.exe".into(), raw: r#"/select,"C:\d\a b.txt""#.into() }]);
    let linux = plan_reveal(Os::Linux, Path::new("/home/me/a b#.txt"));
    let Launch::Run { program, args } = &linux[0] else { panic!("{linux:?}") };
    assert_eq!(program, "dbus-send");
    assert!(args.contains(&OsString::from("array:string:file:///home/me/a%20b%23.txt")), "{args:?}");
    assert_eq!(linux[1], Launch::Run { program: "xdg-open".into(), args: vec!["/home/me".into()] });
}

#[test]
fn windows_command_lookup_uses_pathext() {
    let dirs = [PathBuf::from("/bin1"), PathBuf::from("/bin2")];
    let found = find_with_exts("code", &dirs, ".COM;.EXE;.BAT;.CMD", |p| p == Path::new("/bin2/code.cmd"));
    assert_eq!(found, Some(PathBuf::from("/bin2/code.cmd")));
    assert_eq!(find_with_exts("nope", &dirs, ".EXE", |_| false), None);
}

#[test]
fn os_words() {
    assert_eq!(Os::Mac.secret_store(), "your Keychain");
    assert_eq!(Os::Windows.secret_store(), "Windows Credential Manager");
    assert_eq!(Os::Linux.file_manager(), "Files");
    assert_eq!(Os::Windows.this_computer(), "This Computer");
}
