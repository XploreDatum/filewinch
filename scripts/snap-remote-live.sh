#!/usr/bin/env bash
# Renders docs/screens/remote-live.png: the app, headless, with its right pane connected to a
# real SFTP server (`rclone serve sftp` on 127.0.0.1) holding a small, realistic site tree.
#
#   scripts/snap-remote-live.sh [out.png]
#
# The server, its host key and a random throwaway password live in a scratch folder that is
# removed on exit, along with the server. The app gets the server through FILEWINCH_LIVE_*
# (see crates/filewinch-app/src/live.rs), trusts the host key by its fingerprint, keeps the
# password in memory and never touches the Keychain. The run fails if the listing isn't there.
set -euo pipefail
cd "$(dirname "$0")/.."
out="${1:-docs/screens/remote-live.png}"

rclone=/usr/local/bin/rclone
[[ -x "$rclone" ]] || rclone=$(command -v rclone) || { echo "rclone isn't installed" >&2; exit 1; }

scratch=$(mktemp -d "${TMPDIR:-/tmp}/filewinch-remote-live.XXXXXX")
server_pid=""
cleanup() {
  [[ -n "$server_pid" ]] && kill "$server_pid" 2>/dev/null && wait "$server_pid" 2>/dev/null
  rm -rf "$scratch"
}
trap cleanup EXIT

# A small site the way a web host shows it: varied sizes, dates, a hidden file.
root="$scratch/root"
put() { # put <path> <bytes> <touch -t stamp>
  mkdir -p "$(dirname "$root/$1")"
  head -c "$2" /dev/urandom > "$root/$1"
  touch -t "$3" "$root/$1"
}
put public_html/index.html 18902 202611181430
put public_html/about.html 9214 202611021105
put public_html/contact.php 4380 202610290920
put public_html/.htaccess 612 202609150800
put public_html/css/site.css 33410 202611181428
put public_html/js/app.js 141093 202611181429
put public_html/img/hero.jpg 812004 202611100915
put public_html/img/team.png 402113 202610051630
put logs/access.log 5242880 202611201015
put logs/error.log 48211 202611191742
put backups/site-2026-10-31.tar.gz 9812004 202610312359
put backups/db-2026-11-19.sql.gz 1210534 202611190300
put README.md 2210 202608211200
put deploy.sh 1388 202611011045
put release-2026-11.tar.gz 3401872 202611181500
mkdir -p "$root/tmp"
touch -t 202611200930 "$root/tmp" "$root/public_html/css" "$root/public_html/js" "$root/public_html/img"
touch -t 202611181430 "$root/public_html"
touch -t 202611201015 "$root/logs"
touch -t 202611190300 "$root/backups"

ssh-keygen -q -t ed25519 -N "" -C filewinch-snap -f "$scratch/host_ed25519"
fingerprint=$(ssh-keygen -l -E sha256 -f "$scratch/host_ed25519.pub" | awk '{print $2}')
password=$(openssl rand -hex 16)
port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')

RCLONE_CONFIG="$scratch/rclone.conf" "$rclone" serve sftp "$root" --addr "127.0.0.1:$port" \
  --key "$scratch/host_ed25519" --user deploy --pass "$password" --log-file "$scratch/server.log" &
server_pid=$!
for _ in $(seq 100); do
  nc -z 127.0.0.1 "$port" 2>/dev/null && break
  sleep 0.1
done
nc -z 127.0.0.1 "$port" || { echo "rclone serve sftp didn't start:" >&2; cat "$scratch/server.log" >&2; exit 1; }

cargo build --offline -p filewinch-app
target=$(cargo metadata --offline --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
bin="$target/debug/filewinch"

export FILEWINCH_LIVE_PROTOCOL=sftp FILEWINCH_LIVE_HOST=127.0.0.1 FILEWINCH_LIVE_PORT="$port" \
  FILEWINCH_LIVE_USER=deploy FILEWINCH_LIVE_PASSWORD="$password" FILEWINCH_LIVE_FINGERPRINT="$fingerprint" \
  FILEWINCH_LIVE_PATH=/
"$bin" --scene remote-live --tree "$scratch/tree.json"
for name in public_html release-2026-11.tar.gz backups "SFTP on 127.0.0.1"; do
  grep -q "$name" "$scratch/tree.json" || { echo "the remote listing is missing “$name”; see the app's output above" >&2; exit 1; }
done
"$bin" --scene remote-live --snap "$out"
echo "wrote $out"
