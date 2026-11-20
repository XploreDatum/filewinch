#!/usr/bin/env bash
# Writes THIRD-PARTY-NOTICES.md: every dependency Filewinch ships (all platforms, all
# features, no dev or build dependencies) with its licence and the full licence texts.
# Fails loudly if any dependency's licence isn't one Filewinch (AGPL-3.0-or-later) can
# combine with: the accepted list is scripts/licenses/about.toml.
#
# Needs cargo-about (`cargo +stable install cargo-about --locked --features cli`). Offline:
# licence texts come from the crates in ~/.cargo/registry, so run `cargo fetch` first if
# Cargo.lock changed.
set -euo pipefail
cd "$(dirname "$0")/.."
command -v cargo-about >/dev/null || { echo "cargo-about isn't installed: cargo +stable install cargo-about --locked --features cli" >&2; exit 1; }
json=$(mktemp)
trap 'rm -f "$json"' EXIT
if ! cargo-about generate --offline --workspace --all-features --fail -c scripts/licenses/about.toml --format json -o "$json"; then
  echo >&2
  echo "LICENCE CHECK FAILED: a dependency's licence is missing, unreadable or not compatible with AGPL-3.0-or-later." >&2
  echo "See the errors above. Don't add it to scripts/licenses/about.toml unless it really is compatible." >&2
  exit 1
fi
python3 -I scripts/licenses/render.py THIRD-PARTY-NOTICES.md < "$json"
echo "wrote THIRD-PARTY-NOTICES.md"
