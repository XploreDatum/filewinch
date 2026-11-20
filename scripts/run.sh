#!/usr/bin/env bash
# Builds Filewinch and launches the real app window.
#   scripts/run.sh            release build
#   scripts/run.sh --debug    debug build (faster to compile)
# Extra arguments after `--` go to the app.
set -euo pipefail
cd "$(dirname "$0")/.."
profile=release
if [[ "${1:-}" == "--debug" ]]; then profile=debug; shift; fi
[[ "${1:-}" == "--" ]] && shift
features="${FILEWINCH_FEATURES:-}"
args=(-p filewinch-app)
[[ "$profile" == release ]] && args+=(--release)
[[ -n "$features" ]] && args+=(--features "$features")
cargo build "${args[@]}"
target=$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
exec "$target/$profile/filewinch" "$@"
