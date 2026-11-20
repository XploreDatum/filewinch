#!/bin/sh
# C compiler for cross *checks* from a Mac (scripts/cross-check.sh): `zig cc` ships libc headers
# for Linux and Windows. ZIG_TARGET picks the target; cc-rs's own --target flag is dropped.
for a in "$@"; do
  shift
  case "$a" in --target=*) continue ;; esac
  set -- "$@" "$a"
done
exec zig cc -target "$ZIG_TARGET" "$@"
