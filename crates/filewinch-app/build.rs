//! Embeds Filewinch's Windows resources in `filewinch.exe`: the icon (`assets/icon/filewinch.ico`,
//! resource 1, which `rune::set_app_icon_resource(1)` loads for the windows), the version info,
//! and the application manifest (`windows/filewinch.manifest`). Does nothing for other targets.
//!
//! Build scripts run on the host, so the target is read from `CARGO_CFG_TARGET_OS`. The resource
//! compiler comes from `embed-resource`, a build-dependency only on Windows hosts (rc.exe from the
//! Windows SDK for MSVC, windres for GNU): a cross `cargo check` from macOS or Linux skips the
//! resources with a warning, and only a build on Windows embeds them.

use std::env;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let icon = dir.join("../../assets/icon/filewinch.ico");
    let manifest = dir.join("windows/filewinch.manifest");
    let template = dir.join("windows/filewinch.rc.in");
    for input in [&icon, &manifest, &template] {
        println!("cargo:rerun-if-changed={}", input.display());
    }
    let rc = write_rc(&template, &icon, &manifest);
    compile(&rc);
}

/// Fills the template's placeholders into `OUT_DIR/filewinch.rc`.
fn write_rc(template: &Path, icon: &Path, manifest: &Path) -> PathBuf {
    let read = |p: &Path| {
        std::fs::read_to_string(p).unwrap_or_else(|e| panic!("can't read {}: {e}", p.display()))
    };
    let version = env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let part = |name: &str| {
        env::var(name)
            .ok()
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(0)
    };
    let commas = format!(
        "{},{},{},0",
        part("CARGO_PKG_VERSION_MAJOR"),
        part("CARGO_PKG_VERSION_MINOR"),
        part("CARGO_PKG_VERSION_PATCH")
    );
    // VS_FF_DEBUG (1) for debug builds, VS_FF_PRERELEASE (2) for versions like 1.0.0-beta.1.
    let debug = env::var("PROFILE").as_deref() == Ok("debug");
    let pre = !env::var("CARGO_PKG_VERSION_PRE")
        .unwrap_or_default()
        .is_empty();
    let flags = format!("0x{:x}L", u8::from(debug) | (u8::from(pre) << 1));
    let values = [
        ("@ICON@", rc_path(icon)),
        ("@MANIFEST@", rc_path(manifest)),
        ("@VERSION_COMMAS@", commas),
        ("@FILEFLAGS@", flags),
        ("@VERSION@", version),
    ];
    let mut text = read(template);
    let unfilled = values
        .iter()
        .fold(text.clone(), |t, (name, _)| t.replace(name, ""));
    assert!(
        !unfilled.contains('@'),
        "{} has a placeholder build.rs doesn't fill",
        template.display()
    );
    for (name, value) in &values {
        text = text.replace(name, value);
    }
    let rc = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("filewinch.rc");
    std::fs::write(&rc, text).unwrap_or_else(|e| panic!("can't write {}: {e}", rc.display()));
    rc
}

/// An absolute path as an .rc string literal: backslashes doubled, which rc.exe, llvm-rc and
/// windres all read back as one.
fn rc_path(path: &Path) -> String {
    let path = path
        .canonicalize()
        .unwrap_or_else(|e| panic!("can't find {}: {e}", path.display()));
    let text = path
        .to_str()
        .unwrap_or_else(|| panic!("{} isn't UTF-8", path.display()));
    // canonicalize gives a verbatim `\\?\C:\…` path on Windows, which rc.exe doesn't take.
    text.strip_prefix(r"\\?\")
        .unwrap_or(text)
        .replace('\\', r"\\")
}

#[cfg(windows)]
fn compile(rc: &Path) {
    match embed_resource::compile_for(rc, ["filewinch"], embed_resource::NONE) {
        embed_resource::CompilationResult::Ok | embed_resource::CompilationResult::NotWindows => {}
        // No resource compiler found: the exe still builds and runs, without its icon, version
        // info or manifest (so blurry on high-DPI displays). Say so on every build.
        embed_resource::CompilationResult::NotAttempted(why) => {
            println!(
                "cargo:warning=Windows resources not embedded (no resource compiler: {why}); install the Windows SDK"
            );
        }
        embed_resource::CompilationResult::Failed(why) => {
            panic!("compiling {}: {why}", rc.display())
        }
    }
}

#[cfg(not(windows))]
fn compile(rc: &Path) {
    let _ = rc;
    println!(
        "cargo:warning=Windows resources (icon, version info, manifest) are embedded only when building on Windows; this cross build has none"
    );
}
