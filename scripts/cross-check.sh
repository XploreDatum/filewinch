#!/bin/sh
# Type-checks the workspace for Windows and Linux from a Mac (no linking):
#   scripts/cross-check.sh [windows|linux|linux-arm|all] [extra cargo args]
# C dependencies (ring) are compiled with `zig cc`, which ships Linux and Windows libc headers
# (brew install zig). The objects are never linked; a real build happens on the target OS.
set -eu
cd "$(dirname "$0")/.."
here="$PWD/scripts/cross"
which="${1:-all}"; [ $# -gt 0 ] && shift
run() { # <rust target> <zig target> <env suffix> <archiver>
  t="$1"; z="$2"; e="$3"; a="$4"; shift 4
  echo "== cargo check --target $t"
  env ZIG_TARGET="$z" "CC_$e=$here/zigcc.sh" "AR_$e=$a" \
    cargo check --offline --workspace --all-targets --target "$t" "$@"
}
case "$which" in
  windows) set -- x86_64-pc-windows-msvc x86_64-windows-gnu x86_64_pc_windows_msvc "zig lib" "$@"; run "$@" ;;
  linux) set -- x86_64-unknown-linux-gnu x86_64-linux-gnu x86_64_unknown_linux_gnu "zig ar" "$@"; run "$@" ;;
  linux-arm) set -- aarch64-unknown-linux-gnu aarch64-linux-gnu aarch64_unknown_linux_gnu "zig ar" "$@"; run "$@" ;;
  all) "$0" windows "$@"; "$0" linux "$@" ;;
  *) echo "usage: $0 [windows|linux|linux-arm|all] [cargo args]" >&2; exit 2 ;;
esac
