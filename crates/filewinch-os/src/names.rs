//! File names the local disk accepts.
//!
//! macOS and Linux refuse only `/`, NUL, empty names, `.` and `..`. Windows also refuses
//! `< > : " / \ | ? *` and control characters, names ending in a dot or a space, the reserved
//! device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`, also with an
//! extension: `nul.txt`), and names longer than 255 UTF-16 units.
//!
//! [`check`] says why a name typed into Rename or New Folder can't be used. [`windows_safe`]
//! is the substitution a download uses for a remote name Windows refuses, so the file still
//! arrives under a name that reads the same (the approach rclone and OneDrive use):
//!
//! | Refused | Saved as |
//! | --- | --- |
//! | `< > : " / \ \| ? *` | the full-width forms `＜ ＞ ： ＂ ／ ＼ ｜ ？ ＊` |
//! | control characters U+0000–U+001F | the control pictures `␀`–`␟` |
//! | a trailing `.` / space | `．` / `␠` |
//! | a reserved device name | `_` after the base name: `CON` → `CON_`, `nul.txt` → `nul_.txt` |
//! | over 255 UTF-16 units | cut at 255, keeping the extension |
//!
//! The substitution is idempotent and never produces a name Windows refuses.

use std::borrow::Cow;

use crate::Os;

const WINDOWS_BAD: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
const RESERVED: &[&str] = &["CON", "PRN", "AUX", "NUL"];
const MAX_UTF16: usize = 255;

/// Why `name` can't name a file or folder on `os`'s local disk, or `None` if it can.
pub fn check(os: Os, name: &str) -> Option<String> {
    if name.trim().is_empty() {
        return Some("A name can't be empty.".into());
    }
    if name == "." || name == ".." {
        return Some(format!("“{name}” is reserved for folder navigation."));
    }
    if name.contains('\0') {
        return Some("A name can't contain a NUL character.".into());
    }
    if os != Os::Windows {
        return name.contains('/').then(|| "A name can't contain “/”.".into());
    }
    if let Some(c) = name.chars().find(|c| WINDOWS_BAD.contains(c)) {
        return Some(format!("Windows doesn't allow “{c}” in names. These characters can't be used: < > : \" / \\ | ? *"));
    }
    if name.chars().any(|c| (c as u32) < 0x20) {
        return Some("Windows doesn't allow control characters in names.".into());
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Some("Windows doesn't allow a name to end with a dot or a space.".into());
    }
    if is_reserved(name) {
        return Some(format!("“{name}” is a reserved device name on Windows. Choose another name."));
    }
    if name.encode_utf16().count() > MAX_UTF16 {
        return Some("The name is too long for Windows (at most 255 characters).".into());
    }
    None
}

/// The device names Windows reserves, alone or before an extension, in any case.
fn is_reserved(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let upper = base.to_ascii_uppercase();
    if RESERVED.contains(&upper.as_str()) {
        return true;
    }
    // COM1–COM9 and LPT1–LPT9, plus the superscript digits Windows also reserves.
    let mut chars = upper.chars();
    let head: String = chars.by_ref().take(3).collect();
    let tail: Vec<char> = chars.collect();
    (head == "COM" || head == "LPT") && tail.len() == 1 && matches!(tail[0], '1'..='9' | '¹' | '²' | '³')
}

/// `name` as Windows will accept it (see the module docs); borrowed when already fine.
pub fn windows_safe(name: &str) -> Cow<'_, str> {
    if check(Os::Windows, name).is_none() {
        return Cow::Borrowed(name);
    }
    match name {
        "" => return Cow::Borrowed("_"),
        "." => return Cow::Borrowed("．"),
        ".." => return Cow::Borrowed("．．"),
        _ => {}
    }
    let mut out: String = name
        .chars()
        .map(|c| match c {
            '<' => '＜',
            '>' => '＞',
            ':' => '：',
            '"' => '＂',
            '/' => '／',
            '\\' => '＼',
            '|' => '｜',
            '?' => '？',
            '*' => '＊',
            c if (c as u32) < 0x20 => char::from_u32(0x2400 + c as u32).unwrap_or('_'),
            c => c,
        })
        .collect();
    let trailing = out.len() - out.trim_end_matches(['.', ' ']).len();
    if trailing > 0 {
        let tail: String = out[out.len() - trailing..].chars().map(|c| if c == '.' { '．' } else { '␠' }).collect();
        out.truncate(out.len() - trailing);
        out.push_str(&tail);
    }
    if is_reserved(&out) {
        let cut = out.find('.').unwrap_or(out.len());
        out.insert(cut, '_');
    }
    if out.encode_utf16().count() > MAX_UTF16 {
        out = shorten(&out);
    }
    Cow::Owned(out)
}

/// Cuts a long name to 255 UTF-16 units, keeping a short extension.
fn shorten(name: &str) -> String {
    let ext = name.rfind('.').map(|i| &name[i..]).filter(|e| e.encode_utf16().count() <= 16).unwrap_or("");
    let budget = MAX_UTF16 - ext.encode_utf16().count();
    let mut stem = String::new();
    let mut used = 0;
    for c in name[..name.len() - ext.len()].chars() {
        if used + c.len_utf16() > budget {
            break;
        }
        used += c.len_utf16();
        stem.push(c);
    }
    // The cut may leave a trailing dot or space.
    while stem.ends_with(['.', ' ']) {
        stem.pop();
    }
    stem + ext
}
