//! Opening things outside Filewinch: a URL in the browser (OAuth sign-in), a downloaded file
//! revealed in the file manager, and a file in its default app or the external editor.
//!
//! | | macOS | Windows | Linux |
//! | --- | --- | --- | --- |
//! | URL, default app | `open <target>` | `ShellExecuteW("open")` | `xdg-open <target>` |
//! | reveal | `open -R <file>` | `explorer.exe /select,"<file>"` | the file manager's `ShowItems` over D-Bus, else `xdg-open <folder>` |
//! | editor setting | an app name: `open -a <app> <file>` | a command or exe path: `"C:\…\code.exe" --new-window` | a command: `code --new-window` |
//!
//! The editor command is split into words as the OS's shell would ([`parse_command`]); the
//! file is added as the last argument. [`plan_open`] and [`plan_reveal`] are pure, so each
//! OS's commands are tested on any OS; [`open_url`], [`open_file`] and [`reveal`] run them.

use std::ffi::OsString;
use std::path::Path;

use crate::Os;

/// One way to open something.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Launch {
    /// Run `program` with `args` and wait for it (launchers that hand off and exit).
    Run { program: String, args: Vec<OsString> },
    /// Start `program` with `args` and don't wait (an editor runs until the person quits it).
    Start { program: String, args: Vec<OsString> },
    /// Windows: `program` with an already-quoted command line (`explorer.exe` parses its own).
    RunRaw { program: String, raw: String },
    /// Windows: `ShellExecuteW` with the "open" verb (default browser or app).
    ShellOpen(OsString),
}

/// How to open `target` (a URL or a file) on `os`: in the default app, or in `editor` (the
/// external-editor setting) when it's set. Errors say what's wrong with the editor setting.
pub fn plan_open(os: Os, target: &Path, editor: Option<&str>) -> Result<Launch, String> {
    let editor = editor.map(str::trim).filter(|e| !e.is_empty());
    let target = target.as_os_str().to_owned();
    Ok(match (os, editor) {
        (Os::Mac, None) => Launch::Run { program: "/usr/bin/open".into(), args: vec![target] },
        (Os::Mac, Some(app)) => Launch::Run { program: "/usr/bin/open".into(), args: vec!["-a".into(), app.into(), target] },
        (Os::Windows, None) => Launch::ShellOpen(target),
        (Os::Linux, None) => Launch::Run { program: "xdg-open".into(), args: vec![target] },
        (_, Some(cmd)) => {
            let mut words = parse_command(os, cmd)?.into_iter();
            let program = words.next().ok_or("The external editor setting is empty.")?;
            let mut args: Vec<OsString> = words.map(OsString::from).collect();
            args.push(target);
            Launch::Start { program, args }
        }
    })
}

/// How to show `file` selected in the file manager on `os`, best first.
pub fn plan_reveal(os: Os, file: &Path) -> Vec<Launch> {
    match os {
        Os::Mac => vec![Launch::Run { program: "/usr/bin/open".into(), args: vec!["-R".into(), file.as_os_str().to_owned()] }],
        // Windows paths can't hold `"`, so quoting the path is enough; explorer.exe reads
        // `/select,"<path>"` itself and doesn't follow the C runtime's quoting rules.
        Os::Windows => vec![Launch::RunRaw { program: "explorer.exe".into(), raw: format!("/select,\"{}\"", file.display()) }],
        Os::Linux => {
            let show = Launch::Run {
                program: "dbus-send".into(),
                args: [
                    "--session",
                    "--print-reply",
                    "--dest=org.freedesktop.FileManager1",
                    "--type=method_call",
                    "/org/freedesktop/FileManager1",
                    "org.freedesktop.FileManager1.ShowItems",
                ]
                .into_iter()
                .map(OsString::from)
                .chain([OsString::from(format!("array:string:{}", file_uri(file))), "string:".into()])
                .collect(),
            };
            let folder = file.parent().unwrap_or(file).as_os_str().to_owned();
            vec![show, Launch::Run { program: "xdg-open".into(), args: vec![folder] }]
        }
    }
}

/// `file:///…` with the path percent-encoded (Unix paths).
fn file_uri(p: &Path) -> String {
    let mut out = String::from("file://");
    for b in p.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Splits a command line into words as `os`'s shell would.
///
/// Linux (POSIX): whitespace separates words; `'…'` is literal; `"…"` keeps spaces and takes
/// `\"`, `\\`, `\$` and `` \` `` escapes; a bare `\` escapes the next character.
/// Windows (the C runtime's rules): whitespace separates words; `"…"` keeps spaces;
/// backslashes are literal except before a `"` (`\"` is a quote, `\\"` a backslash then a
/// closing quote), so `C:\Program Files\…` paths work as typed.
/// macOS: the setting is an app name, so the whole text is one word.
pub fn parse_command(os: Os, cmd: &str) -> Result<Vec<String>, String> {
    let cmd = cmd.trim();
    if os == Os::Mac {
        return Ok(vec![cmd.to_owned()]);
    }
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = cmd.chars().peekable();
    match os {
        Os::Windows => {
            let mut quoted = false;
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        let mut n = 1;
                        while chars.peek() == Some(&'\\') {
                            chars.next();
                            n += 1;
                        }
                        if chars.peek() == Some(&'"') {
                            cur.extend(std::iter::repeat_n('\\', n / 2));
                            if n % 2 == 1 {
                                chars.next();
                                cur.push('"');
                            }
                        } else {
                            cur.extend(std::iter::repeat_n('\\', n));
                        }
                        in_word = true;
                    }
                    '"' => {
                        // `""` inside quotes is a literal quote.
                        if quoted && chars.peek() == Some(&'"') {
                            chars.next();
                            cur.push('"');
                        } else {
                            quoted = !quoted;
                        }
                        in_word = true;
                    }
                    c if c.is_whitespace() && !quoted => {
                        if in_word {
                            words.push(std::mem::take(&mut cur));
                            in_word = false;
                        }
                    }
                    c => {
                        cur.push(c);
                        in_word = true;
                    }
                }
            }
            if quoted {
                return Err("The external editor command has an unclosed quote (\").".into());
            }
        }
        Os::Linux | Os::Mac => {
            while let Some(c) = chars.next() {
                match c {
                    '\'' => {
                        in_word = true;
                        loop {
                            match chars.next() {
                                Some('\'') => break,
                                Some(c) => cur.push(c),
                                None => return Err("The external editor command has an unclosed quote (').".into()),
                            }
                        }
                    }
                    '"' => {
                        in_word = true;
                        loop {
                            match chars.next() {
                                Some('"') => break,
                                Some('\\') if matches!(chars.peek(), Some('"' | '\\' | '$' | '`')) => cur.push(chars.next().unwrap_or('\\')),
                                Some(c) => cur.push(c),
                                None => return Err("The external editor command has an unclosed quote (\").".into()),
                            }
                        }
                    }
                    '\\' => {
                        in_word = true;
                        match chars.next() {
                            Some(c) => cur.push(c),
                            None => cur.push('\\'),
                        }
                    }
                    c if c.is_whitespace() => {
                        if in_word {
                            words.push(std::mem::take(&mut cur));
                            in_word = false;
                        }
                    }
                    c => {
                        cur.push(c);
                        in_word = true;
                    }
                }
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    if words.is_empty() {
        return Err("The external editor setting is empty.".into());
    }
    Ok(words)
}

/// Opens `url` in the default browser.
pub fn open_url(url: &str) -> Result<(), String> {
    run(&plan_open(Os::CURRENT, Path::new(url), None)?, "your web browser")
}

/// Opens `file` in its default app, or in `editor` (the external-editor setting) when set.
pub fn open_file(file: &Path, editor: Option<&str>) -> Result<(), String> {
    let what = match editor.map(str::trim).filter(|e| !e.is_empty()) {
        Some(e) => format!("“{e}”"),
        None => "the file's default app".to_owned(),
    };
    run(&plan_open(Os::CURRENT, file, editor)?, &what)
}

/// Shows `file` selected in the file manager (or its folder, where selecting isn't possible).
pub fn reveal(file: &Path) -> Result<(), String> {
    let mut last = String::new();
    for launch in plan_reveal(Os::CURRENT, file) {
        match run(&launch, Os::CURRENT.file_manager()) {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Runs one [`Launch`]; the error names `what` couldn't be opened and why.
pub fn run(launch: &Launch, what: &str) -> Result<(), String> {
    use std::process::{Command, Stdio};
    let failed = |why: String| format!("Couldn't open {what}: {why}");
    match launch {
        Launch::Run { program, args } => {
            let out = Command::new(program).args(args).stdin(Stdio::null()).output().map_err(|e| failed(spawn_reason(program, &e)))?;
            if out.status.success() {
                return Ok(());
            }
            let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            Err(failed(if err.is_empty() { format!("{program} exited with {}.", out.status) } else { err }))
        }
        Launch::Start { program, args } => {
            let exe = resolve_program(program);
            let mut child = Command::new(&exe)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| failed(spawn_reason(program, &e)))?;
            // A command that fails at once (a bad flag) says so; one still running is the
            // editor, reaped on a thread so it leaves no zombie.
            for _ in 0..10 {
                match child.try_wait() {
                    Ok(Some(status)) if !status.success() => return Err(failed(format!("{program} exited with {status}."))),
                    Ok(Some(_)) => return Ok(()),
                    Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
                    Err(_) => break,
                }
            }
            std::thread::spawn(move || child.wait());
            Ok(())
        }
        Launch::RunRaw { program, raw } => run_raw(program, raw).map_err(failed),
        Launch::ShellOpen(target) => shell_open(target).map_err(failed),
    }
}

fn spawn_reason(program: &str, e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        format!("“{program}” wasn't found. Check the name, or give the full path to the program.")
    } else {
        e.to_string()
    }
}

/// On Windows, finds a bare command (`code`) on `PATH` with the extensions in `PATHEXT`, so
/// `code.cmd` and `subl.exe` start as they would from a prompt. Elsewhere `PATH` lookup is the
/// OS's own.
fn resolve_program(program: &str) -> std::path::PathBuf {
    if Os::CURRENT != Os::Windows || Path::new(program).extension().is_some() || program.contains(['\\', '/']) {
        return program.into();
    }
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    let dirs = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default();
    find_with_exts(program, &dirs, &exts, |p| p.is_file()).unwrap_or_else(|| program.into())
}

/// The first `<dir>/<program><ext>` that `exists`, over `dirs` then `exts` (`;`-separated).
pub fn find_with_exts(program: &str, dirs: &[std::path::PathBuf], exts: &str, exists: impl Fn(&Path) -> bool) -> Option<std::path::PathBuf> {
    dirs.iter().find_map(|d| exts.split(';').filter(|e| !e.is_empty()).map(|e| d.join(format!("{program}{}", e.to_ascii_lowercase()))).find(|p| exists(p)))
}

#[cfg(windows)]
fn run_raw(program: &str, raw: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    // explorer.exe exits with 1 even when it worked, so only a failure to start counts.
    std::process::Command::new(program).raw_arg(raw).spawn().map(drop).map_err(|e| spawn_reason(program, &e))
}

#[cfg(not(windows))]
fn run_raw(program: &str, _raw: &str) -> Result<(), String> {
    Err(format!("{program} only runs on Windows."))
}

#[cfg(windows)]
fn shell_open(target: &std::ffi::OsStr) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let wide = |s: &std::ffi::OsStr| s.encode_wide().chain(Some(0)).collect::<Vec<u16>>();
    let (verb, file) = (wide("open".as_ref()), wide(target));
    // SAFETY: both strings are NUL-terminated and outlive the call; null parent, parameters
    // and directory are allowed.
    let code = unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL) } as isize;
    // ShellExecuteW returns a value above 32 on success, else an error code.
    match code {
        c if c > 32 => Ok(()),
        2 | 3 => Err("it doesn't exist.".into()),
        5 => Err("access was denied.".into()),
        31 => Err("no app is set to open this kind of file. Choose one in Windows Settings ▸ Apps ▸ Default apps.".into()),
        c => Err(format!("Windows error {c}.")),
    }
}

#[cfg(not(windows))]
fn shell_open(_target: &std::ffi::OsStr) -> Result<(), String> {
    Err("ShellExecute only runs on Windows.".into())
}
