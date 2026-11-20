#!/usr/bin/env bash
# Builds a release Filewinch for Linux and packages it into dist/linux/:
#   filewinch-<version>-linux-<arch>.tar.gz   FHS-like tree (bin/, share/) plus install.sh
#   Filewinch-<version>-<arch>.AppImage         when appimagetool is on PATH
#
# Usage (on Linux, from anywhere in the repo):
#   scripts/package-linux.sh                                 host target
#   scripts/package-linux.sh --target aarch64-unknown-linux-gnu
#   FILEWINCH_FEATURES="sftp ftp" scripts/package-linux.sh   backends to include (default: all)
#   Release builds leave out the app's dev-tools feature (headless snapshots, scenes, demo data).
# Then: tar xzf dist/linux/filewinch-*.tar.gz && ./filewinch-*/install.sh   (to ~/.local)
#       PREFIX=/usr/local sudo -E ./filewinch-*/install.sh                  (system-wide)
#       ./filewinch-*/install.sh --uninstall
#
# Prerequisites (Rune, vendor/rune/crates/rune/README.md "Linux"):
#   build:   Debian/Ubuntu `apt-get install libxkbcommon-dev libwayland-dev`
#            Fedora `dnf install libxkbcommon-devel wayland-devel`, Arch `pacman -S libxkbcommon wayland`
#   runtime: libxkbcommon-x11-0 and a Vulkan driver (e.g. mesa-vulkan-drivers); neither is
#            bundled into the AppImage, the host provides them.
#   AppImage: appimagetool (https://github.com/AppImage/appimagetool/releases), on PATH.
#   Optional: desktop-file-validate (desktop-file-utils), appstreamcli (appstream) to lint metadata.
#
# A local (gitignored) .cargo/config.toml may set target-dir to another machine's drive; when its
# parent folder doesn't exist here and CARGO_TARGET_DIR is unset, this script builds into <repo>/target.
set -euo pipefail
cd "$(dirname "$0")/.."
root=$(pwd)
id=app.xpdt.filewinch

die() { echo "package-linux.sh: $*" >&2; exit 1; }

target=""
while [ $# -gt 0 ]; do
  case "$1" in
    --target) [ $# -ge 2 ] || die "--target needs a value"; target=$2; shift 2 ;;
    --target=*) target=${1#--target=}; shift ;;
    -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
    *) die "unknown argument: $1 (see --help)" ;;
  esac
done

[ "$(uname -s)" = Linux ] || die "run this on Linux (this is $(uname -s)); scripts/cross-check.sh linux type-checks from a Mac"
command -v cargo >/dev/null || die "cargo not found (install Rust from https://rustup.rs)"

metadata=$(cargo metadata --format-version 1 --no-deps)
target_dir=$(printf '%s' "$metadata" | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
[ -n "$target_dir" ] || die "couldn't read target_directory from cargo metadata"
if [ -z "${CARGO_TARGET_DIR:-}" ] && [ ! -d "$(dirname "$target_dir")" ]; then
  echo "package-linux.sh: configured target dir $target_dir isn't usable here; building into $root/target"
  export CARGO_TARGET_DIR="$root/target"
  target_dir=$CARGO_TARGET_DIR
fi
pkgid=$(cargo pkgid -p filewinch-app)
version=${pkgid##*[@#]}
case "$version" in [0-9]*) ;; *) die "couldn't read the version from '$pkgid'" ;; esac

args=(build --release --locked -p filewinch-app --no-default-features --features "${FILEWINCH_FEATURES:-backends}")
if [ -n "$target" ]; then
  args+=(--target "$target")
  bin="$target_dir/$target/release/filewinch"
  arch=${target%%-*}
else
  bin="$target_dir/release/filewinch"
  arch=$(uname -m)
fi
case "$arch" in
  x86_64|amd64) arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
esac
# The release profile builds line tables for crash symbolication (macOS keeps a dSYM, Windows a
# PDB). Linux keeps no symbols, and any debuginfo makes rustc add .debug_gdb_scripts, an allocated
# section that neither the linker's --strip-all nor strip(1) removes, so build without it here.
export CARGO_PROFILE_RELEASE_DEBUG=false CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO=off
echo "== CARGO_PROFILE_RELEASE_DEBUG=false CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO=off cargo ${args[*]}"
cargo "${args[@]}"
[ -x "$bin" ] || die "build finished but $bin is missing"
# The release profile strips the binary (Cargo.toml) and the build above has no debuginfo; nothing
# debug-related goes in the tarball or the AppImage, which copy only the files named below.
if command -v readelf >/dev/null && readelf -SW "$bin" | grep -qE '\.(symtab|debug_)'; then
  readelf -SW "$bin" | grep -E '\.(symtab|debug_)' >&2
  die "$bin isn't stripped (sections above); check [profile.release] strip = \"symbols\" in Cargo.toml"
fi

dist="$root/dist/linux"
name="filewinch-$version-linux-$arch"
stage="$dist/.$name.building"
rm -rf "$stage"
mkdir -p "$stage/$name"
tree="$stage/$name"

# --- FHS-like tree -------------------------------------------------------------------------
install -Dm755 "$bin" "$tree/bin/filewinch"
install -Dm644 "$root/packaging/linux/$id.desktop" "$tree/share/applications/$id.desktop"
install -Dm644 "$root/packaging/linux/$id.metainfo.xml" "$tree/share/metainfo/$id.metainfo.xml"
for size in 16 24 32 48 64 128 256 512; do
  png="$root/assets/icon/hicolor/${size}x${size}/apps/$id.png"
  [ -f "$png" ] || die "missing $png (run assets/icon/render-desktop.sh on a Mac)"
  install -Dm644 "$png" "$tree/share/icons/hicolor/${size}x${size}/apps/$id.png"
done
install -Dm644 "$root/assets/icon/hicolor/scalable/apps/$id.svg" "$tree/share/icons/hicolor/scalable/apps/$id.svg"
for doc in LICENSE LICENSE.md LICENSE.txt README.md; do
  if [ -f "$root/$doc" ]; then install -Dm644 "$root/$doc" "$tree/share/doc/filewinch/$doc"; fi
done

cat >"$tree/install.sh" <<'INSTALL'
#!/bin/sh
# Installs Filewinch into PREFIX (default ~/.local): bin/filewinch, its .desktop entry, icons and
# AppStream metadata, then refreshes the desktop and icon caches when their tools exist.
#   ./install.sh                  install for this user (~/.local; ~/.local/bin should be on PATH)
#   PREFIX=/usr/local ./install.sh
#   ./install.sh --uninstall      remove what install.sh put there
set -eu
here=$(cd "$(dirname "$0")" && pwd)
prefix=${PREFIX:-"$HOME/.local"}
case "$prefix" in
  /*) ;;
  *) echo "install.sh: PREFIX must be an absolute path (got '$prefix')" >&2; exit 2 ;;
esac
case "$prefix" in
  *[\\\"\`\$\|\&%]*) echo "install.sh: PREFIX can't contain \\ \" \` \$ | & or % (a .desktop Exec line can't carry them)" >&2; exit 2 ;;
esac
id=app.xpdt.filewinch
files="bin/filewinch share/applications/$id.desktop share/metainfo/$id.metainfo.xml
share/icons/hicolor/scalable/apps/$id.svg"
for size in 16 24 32 48 64 128 256 512; do
  files="$files share/icons/hicolor/${size}x${size}/apps/$id.png"
done

refresh() {
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q "$prefix/share/applications" || true
  fi
  if command -v gtk-update-icon-cache >/dev/null 2>&1 && [ -f "$prefix/share/icons/hicolor/index.theme" ]; then
    gtk-update-icon-cache -q -t -f "$prefix/share/icons/hicolor" || true
  fi
  # GNOME and KDE also watch the directories, so a missing tool only delays the icon.
  touch "$prefix/share/icons/hicolor" 2>/dev/null || true
}

case "${1:-}" in
  --uninstall)
    for f in $files; do rm -f "$prefix/$f"; done
    refresh
    echo "Removed Filewinch from $prefix"
    exit 0 ;;
  "") ;;
  *) echo "usage: $0 [--uninstall]" >&2; exit 2 ;;
esac

for f in $files; do
  mkdir -p "$(dirname "$prefix/$f")"
  cp "$here/$f" "$prefix/$f.new"
  mv -f "$prefix/$f.new" "$prefix/$f"
done
chmod 755 "$prefix/bin/filewinch"
# Point the launcher at the installed binary, so it works even when PREFIX/bin isn't on PATH.
desktop="$prefix/share/applications/$id.desktop"
sed -e "s|^Exec=filewinch |Exec=\"$prefix/bin/filewinch\" |" -e "s|^TryExec=filewinch\$|TryExec=$prefix/bin/filewinch|" \
  "$desktop" >"$desktop.new"
mv -f "$desktop.new" "$desktop"
refresh
echo "Installed Filewinch into $prefix"
case ":$PATH:" in
  *":$prefix/bin:"*) ;;
  *) echo "Note: $prefix/bin isn't on PATH; the app menu entry works, or add it to PATH to run 'filewinch'." ;;
esac
INSTALL
chmod 755 "$tree/install.sh"

if command -v desktop-file-validate >/dev/null; then
  desktop-file-validate "$tree/share/applications/$id.desktop"
else
  echo "package-linux.sh: desktop-file-validate not found; skipping .desktop validation"
fi
if command -v appstreamcli >/dev/null; then
  appstreamcli validate --no-net "$tree/share/metainfo/$id.metainfo.xml" ||
    die "appstreamcli rejected the metainfo"
fi

mkdir -p "$dist"
tarball="$dist/$name.tar.gz"
tar -C "$stage" -czf "$tarball.building" "$name"
mv -f "$tarball.building" "$tarball"
echo "wrote $tarball"

# --- AppImage ------------------------------------------------------------------------------
appimage="$dist/Filewinch-$version-$arch.AppImage"
if ! command -v appimagetool >/dev/null; then
  echo "package-linux.sh: appimagetool not found; skipping the AppImage."
  echo "  Get it from https://github.com/AppImage/appimagetool/releases (appimagetool-$arch.AppImage),"
  echo "  chmod +x it, put it on PATH as 'appimagetool', and run this script again."
else
  appdir="$stage/Filewinch.AppDir"
  mkdir -p "$appdir/usr"
  cp -R "$tree/bin" "$tree/share" "$appdir/usr/"
  cp "$tree/share/applications/$id.desktop" "$appdir/$id.desktop"
  cp "$tree/share/icons/hicolor/256x256/apps/$id.png" "$appdir/$id.png"
  ln -s "$id.png" "$appdir/.DirIcon"
  cat >"$appdir/AppRun" <<'APPRUN'
#!/bin/sh
# AppImage entry point: runs the bundled binary with the AppImage's arguments.
here=$(dirname "$(readlink -f "$0")")
exec "$here/usr/bin/filewinch" "$@"
APPRUN
  chmod 755 "$appdir/AppRun"
  ARCH=$arch appimagetool "$appdir" "$appimage.building"
  mv -f "$appimage.building" "$appimage"
  echo "wrote $appimage"
fi

rm -rf "$stage"
