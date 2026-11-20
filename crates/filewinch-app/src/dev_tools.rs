//! The `dev-tools` feature: Rune's headless runs (`--snap`, `--tree`, `--script`, ...), the demo
//! scenes and their data (`scenes.rs`, `fake.rs`) and `--look`/`--appearance`. Release builds
//! leave it out, and a release binary asked for one of those flags says so and exits rather than
//! quietly opening a normal window.

/// Flags that only mean something with the dev tools: Rune's headless and scene arguments, and
/// Filewinch's own `--look` and `--appearance`.
#[cfg(any(test, not(feature = "dev-tools")))]
const FLAGS: &[&str] = &[
    "--snap", "--scene", "--tree", "--script", "--trace", "--trace-all", "--strip", "--video", "--html",
    "--desktop", "--frames", "--duration", "--out", "--list-scenes", "--list-windows", "--first-pixels",
    "--window", "--keys", "--pointer", "--playground", "--hot", "--snap-on-patch", "--look", "--appearance",
];

/// The first dev-tools flag among `args`, as `--snap` or `--snap=out.png` names it.
#[cfg(any(test, not(feature = "dev-tools")))]
pub fn requested(args: impl IntoIterator<Item = String>) -> Option<String> {
    args.into_iter().find_map(|a| {
        let name = a.split_once('=').map_or(a.as_str(), |(n, _)| n);
        FLAGS.contains(&name).then(|| name.to_owned())
    })
}

/// In a build without the dev tools: when the command line asks for one, say it isn't
/// available and exit with status 2.
#[cfg(not(feature = "dev-tools"))]
pub fn refuse_flags() {
    if let Some(flag) = requested(std::env::args().skip(1)) {
        eprintln!(
            "filewinch: {flag} is not available in release builds; it needs the dev-tools feature \
             (cargo run -p filewinch-app has it)"
        );
        std::process::exit(2);
    }
}
