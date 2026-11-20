#!/usr/bin/env bash
# Builds, signs, notarizes and staples the macOS release of Filewinch:
#   dist/mac/Filewinch-<version>-macos-universal.dmg   (Filewinch.app, arm64 + x86_64, macOS 13+)
#   dist/mac/symbols/<version>/<arch>/filewinch.dSYM    crash-report symbols; never shipped
#
# Usage (on a Mac with the Developer ID certificate in the login keychain):
#   scripts/release-mac.sh 0.9.0                 build, sign, notarize, staple, verify
#   scripts/release-mac.sh 0.9.0 --upload        ...then attach the .dmg to the v0.9.0 GitHub
#                                                draft release and update its SHA256SUMS
#   scripts/release-mac.sh 0.9.0 --no-notarize   dry run: stop after signing and verifying
#                                                (writes ...-unnotarized.dmg; can't be uploaded)
#   --allow-dirty   build even if the git tree has uncommitted changes
#
# VERSION must equal the workspace version in Cargo.toml (it's compiled into the binary).
# Needs: Xcode command line tools, rustup with x86_64-apple-darwin and aarch64-apple-darwin,
# the notary profile (the script prints the command to make it if it's missing), and for
# --upload `gh` signed in with write access to the repo.
#
# Environment overrides:
#   MAC_SIGN_IDENTITY       default "Developer ID Application: XPLOREDATUM TECHNOLOGIES LLP (SUHRWJYB95)"
#   NOTARY_PROFILE          default filewinch-notary (xcrun notarytool keychain profile)
#   FILEWINCH_REPO          default XploreDatum/filewinch
#   FILEWINCH_MINISIGN_KEY  minisign secret key file; needed by --upload only when the release
#                           already has SHA256SUMS.minisig (CI signed it), to re-sign the new sums
#   CARGO_NET_OFFLINE=true  build without network access (dependencies already fetched)
#   FILEWINCH_DIST_DIR      default dist/mac: where the .dmg, Filewinch.app and symbols/ go
#                           (e.g. a scratch folder for a dry run while dist/mac's app is in use)
set -euo pipefail
cd "$(dirname "$0")/.."
root=$(pwd)

identity=${MAC_SIGN_IDENTITY:-"Developer ID Application: XPLOREDATUM TECHNOLOGIES LLP (SUHRWJYB95)"}
profile=${NOTARY_PROFILE:-filewinch-notary}
repo=${FILEWINCH_REPO:-XploreDatum/filewinch}
min_macos=13.0   # Rune's metal-backend needs MSL 3.0 (macOS 13); keep in step with Info.plist.in
targets=(aarch64-apple-darwin x86_64-apple-darwin)

die() { echo "release-mac.sh: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }

version="" upload=0 notarize=1 allow_dirty=0
while [ $# -gt 0 ]; do
  case "$1" in
    --upload) upload=1 ;;
    --no-notarize) notarize=0 ;;
    --allow-dirty) allow_dirty=1 ;;
    -h|--help) sed -n '2,27p' "$0"; exit 0 ;;
    -*) die "unknown option $1 (see --help)" ;;
    *) [ -z "$version" ] || die "one VERSION only (got '$version' and '$1')"; version=${1#v} ;;
  esac
  shift
done
[ -n "$version" ] || die "usage: scripts/release-mac.sh VERSION [--upload] [--no-notarize] [--allow-dirty]"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || die "VERSION '$version' isn't semver (e.g. 0.9.0)"
[ $upload = 1 ] && [ $notarize = 0 ] && die "--upload needs a notarized build; drop --no-notarize"

# --- preflight: everything that can fail fast, before a long build ----------------------------
[ "$(uname -s)" = Darwin ] || die "run this on macOS"
for tool in cargo rustup xcrun codesign lipo hdiutil ditto plutil shasum otool dwarfdump nm; do
  command -v "$tool" >/dev/null || die "$tool not found"
done

workspace_version=$(cargo metadata --format-version 1 --no-deps |
  python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="filewinch-app"))')
[ "$workspace_version" = "$version" ] ||
  die "VERSION $version doesn't match Cargo.toml's workspace version $workspace_version; bump [workspace.package] version first"
target_dir=$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')

if [ $allow_dirty = 0 ] && git rev-parse --git-dir >/dev/null 2>&1 && [ -n "$(git status --porcelain)" ]; then
  die "the git tree has uncommitted changes; commit them (a release should match a tag) or pass --allow-dirty"
fi

installed=$(rustup target list --installed)
for t in "${targets[@]}"; do
  grep -qx "$t" <<<"$installed" || die "Rust target $t isn't installed: rustup target add $t"
done

security find-identity -v -p codesigning | grep -qF "\"$identity\"" ||
  die "signing identity not in the keychain: $identity (security find-identity -v -p codesigning lists them)"

if [ $notarize = 1 ]; then
  if ! out=$(xcrun notarytool history --keychain-profile "$profile" 2>&1); then
    if grep -q "No Keychain password item" <<<"$out"; then
      key=$(ls "$HOME"/.appstoreconnect/private_keys/AuthKey_*.p8 2>/dev/null | head -1 || true)
      key_id=${key##*/AuthKey_}; key_id=${key_id%.p8}
      cat >&2 <<MSG
release-mac.sh: the notary keychain profile '$profile' doesn't exist yet. Create it once with:

  xcrun notarytool store-credentials $profile \\
    --key ${key:-~/.appstoreconnect/private_keys/AuthKey_<KEY ID>.p8} \\
    --key-id ${key_id:-<KEY ID>} \\
    --issuer <issuer UUID>

The issuer UUID is at App Store Connect > Users and Access > Integrations > App Store Connect API
(above the keys table). Then run this script again. (--no-notarize does a dry run without it.)
MSG
      exit 1
    fi
    die "couldn't check the notary profile '$profile' (network?): $out"
  fi
fi

if [ $upload = 1 ]; then
  command -v gh >/dev/null || die "--upload needs gh (brew install gh)"
  gh auth status >/dev/null 2>&1 || die "gh isn't signed in: gh auth login"
  gh release view "v$version" --repo "$repo" >/dev/null 2>&1 ||
    die "no GitHub release v$version in $repo yet; push the tag and let the release workflow make the draft first"
fi

out_dir=${FILEWINCH_DIST_DIR:-"$root/dist/mac"}
mkdir -p "$out_dir"
out_dir=$(cd "$out_dir" && pwd)
work="$out_dir/.build"
rm -rf "$work"
mkdir -p "$work"
mounted=""
cleanup() { if [ -n "$mounted" ]; then hdiutil detach "$mounted" -force >/dev/null 2>&1 || true; fi; }
trap cleanup EXIT

# --- build -------------------------------------------------------------------------------------
# The release profile (root Cargo.toml) builds with fat LTO, line tables and split-debuginfo =
# "packed": rustc links, runs dsymutil into filewinch.dSYM while the line tables still exist, and
# only then strips the binary. The dSYMs go to dist/mac/symbols/<version>/<arch>/ for
# symbolicating crash reports (atos, or Xcode, finds them by UUID); the app and DMG never hold them.
symbols="$out_dir/symbols/$version"
human() { awk -v b="$1" 'BEGIN { printf "%.1f MB (%d bytes)", b / 1000000, b }'; }
slices=()
for t in "${targets[@]}"; do
  step "cargo build --release --target $t --no-default-features --features backends (MACOSX_DEPLOYMENT_TARGET=$min_macos)"
  # Every backend, without the dev tools (headless snapshots, scenes, demo data): see the
  # app's [features] in crates/filewinch-app/Cargo.toml.
  MACOSX_DEPLOYMENT_TARGET=$min_macos cargo build --release --locked -p filewinch-app --target "$t" \
    --no-default-features --features backends
  bin="$target_dir/$t/release/filewinch"
  dsym="$target_dir/$t/release/filewinch.dSYM"
  [ -x "$bin" ] || die "build finished but $bin is missing"
  [ -d "$dsym" ] || die "build finished but $dsym is missing (is [profile.release] split-debuginfo = \"packed\"?)"
  # Stripped: no debug map (N_OSO/N_FUN stabs) and no local symbols left in the shipped binary.
  if nm -ap "$bin" 2>/dev/null | awk '$2 == "-" || $2 ~ /^[tdbs]$/ {found=1; exit} END {exit !found}'; then
    die "$bin still carries symbols; [profile.release] should have strip = \"symbols\""
  fi
  bin_uuid=$(dwarfdump --uuid "$bin" | awk '{print $2}')
  dsym_uuid=$(dwarfdump --uuid "$dsym" | awk '{print $2}')
  [ -n "$bin_uuid" ] && [ "$bin_uuid" = "$dsym_uuid" ] ||
    die "$dsym (UUID ${dsym_uuid:-none}) doesn't belong to $bin (UUID ${bin_uuid:-none}); rebuild"
  arch=$(lipo -archs "$bin")
  rm -rf "$symbols/$arch"
  mkdir -p "$symbols/$arch"
  ditto "$dsym" "$symbols/$arch/filewinch.dSYM"
  echo "$arch binary: $(human "$(stat -f %z "$bin")"), UUID $bin_uuid; symbols: $symbols/$arch/filewinch.dSYM"
  slices+=("$bin")
done
universal="$work/filewinch"
lipo -create -output "$universal" "${slices[@]}"
for arch in arm64 x86_64; do
  lipo "$universal" -verify_arch "$arch" || die "universal binary lacks $arch (has: $(lipo -archs "$universal"))"
done
for arch in arm64 x86_64; do
  minos=$(otool -arch "$arch" -l "$universal" | awk '/LC_BUILD_VERSION/{f=1} f&&$1=="minos"{print $2; exit}')
  [ "$minos" = "$min_macos" ] || die "$arch slice targets macOS ${minos:-?}, expected $min_macos"
done
echo "universal binary: $(lipo -archs "$universal"), minos $min_macos, $(human "$(stat -f %z "$universal")")"

# --- bundle ------------------------------------------------------------------------------------
step "bundle Filewinch.app $version"
app="$work/Filewinch.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$universal" "$app/Contents/MacOS/filewinch"
cp "$root/assets/icon/Filewinch.icns" "$app/Contents/Resources/Filewinch.icns"
sed -e "s/@VERSION@/$version/g" -e "s/@YEAR@/$(date +%Y)/g" "$root/packaging/macos/Info.plist.in" >"$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist" >/dev/null
grep -q '@[A-Z]*@' "$app/Contents/Info.plist" && die "Info.plist has an unfilled placeholder"
printf 'APPL????' >"$app/Contents/PkgInfo"
# The bundle holds exactly these files (the signature adds Contents/_CodeSignature/CodeResources):
# no AppleDouble ._ files, .DS_Store, debug symbols or stray resources.
expected_app_files=$(printf '%s\n' Contents/Info.plist Contents/MacOS/filewinch Contents/PkgInfo Contents/Resources/Filewinch.icns)
actual_app_files=$(cd "$app" && find . \( -type f -o -type l \) | sed 's|^\./||' | LC_ALL=C sort)
[ "$actual_app_files" = "$expected_app_files" ] ||
  die "Filewinch.app holds unexpected files: $(diff <(echo "$expected_app_files") <(echo "$actual_app_files") | grep '^[<>]' | tr '\n' ' ')"
xattr -cr "$app"

# --- sign, inside out (no --deep) --------------------------------------------------------------
step "sign with $identity"
sign() { codesign --force --timestamp --options runtime --sign "$identity" "$@"; }
# Any nested Mach-O (frameworks, helpers, dylibs) first, deepest first; today there are none.
while IFS= read -r nested; do
  [ -n "$nested" ] || continue
  echo "signing nested $nested"
  sign "$nested"
done < <(find "$app/Contents" -type f ! -path "$app/Contents/MacOS/filewinch" -print0 |
  xargs -0 file -h | awk -F': ' '/Mach-O/{print $1}' | awk -F/ '{print NF "\t" $0}' | sort -rn | cut -f2-)
sign --entitlements "$root/packaging/macos/Filewinch.entitlements" "$app"

step "verify the app's signature"
codesign --verify --deep --strict --verbose=2 "$app"
details=$(codesign -dvvv "$app" 2>&1)
grep -q 'flags=0x10000(runtime)' <<<"$details" || die "hardened runtime flag missing: $details"
grep -q '^Timestamp=' <<<"$details" || die "no secure timestamp: $details"
grep -qF "Authority=$identity" <<<"$details" || die "not signed by $identity: $details"
grep -E '^(Identifier|Format|CodeDirectory|Authority|Timestamp|TeamIdentifier|Runtime Version)' <<<"$details"

notarize_and_staple() {   # notarize_and_staple <file to submit> <file to staple>
  local submit=$1 staple=$2 result id status
  step "notarize $(basename "$submit") (this takes a few minutes)"
  result=$(xcrun notarytool submit "$submit" --keychain-profile "$profile" --wait --output-format json) ||
    die "notarytool submit failed: $result"
  id=$(python3 -c 'import json,sys; print(json.load(sys.stdin).get("id",""))' <<<"$result")
  status=$(python3 -c 'import json,sys; print(json.load(sys.stdin).get("status",""))' <<<"$result")
  if [ "$status" != Accepted ]; then
    echo "notarization status: ${status:-unknown} (submission ${id:-?}); Apple's log:" >&2
    [ -n "$id" ] && xcrun notarytool log "$id" --keychain-profile "$profile" >&2 || true
    die "notarization of $(basename "$submit") was not accepted"
  fi
  echo "accepted: submission $id"
  xcrun stapler staple "$staple"
  xcrun stapler validate "$staple"
}

if [ $notarize = 1 ]; then
  # The app is notarized and stapled on its own first, so the copy users drag out of the disk
  # image carries its ticket and opens offline on first launch.
  ditto -c -k --keepParent "$app" "$work/Filewinch.zip"
  notarize_and_staple "$work/Filewinch.zip" "$app"
  rm -f "$work/Filewinch.zip"
fi

# --- disk image --------------------------------------------------------------------------------
step "make the disk image"
[ -e /Volumes/Filewinch ] && die "a volume named Filewinch is already mounted; eject it first"
src="$work/dmg"
mkdir -p "$src/.background"
ditto "$app" "$src/Filewinch.app"
ln -s /Applications "$src/Applications"
cp "$root/packaging/macos/dmg-background.tiff" "$src/.background/background.tiff"
# Room for the app plus slack for Finder's .DS_Store; the final image is compressed anyway.
size_mb=$(( $(du -sm "$src" | cut -f1) + 32 ))
hdiutil create -quiet -volname Filewinch -srcfolder "$src" -fs HFS+ -format UDRW -size "${size_mb}m" -ov "$work/rw.dmg"
mounted=$(hdiutil attach -readwrite -noverify -noautoopen "$work/rw.dmg" | awk -F'\t' '/Apple_HFS/{print $1}' | tr -d ' ')
[ -n "$mounted" ] && [ -d /Volumes/Filewinch ] || die "couldn't mount the disk image"

# Finder lays the window out (background, icon positions) and saves it in the image's .DS_Store.
# The first run asks to let your terminal control Finder: allow it (System Settings > Privacy &
# Security > Automation).
osascript >"$work/layout.log" 2>&1 <<'APPLESCRIPT' &
with timeout of 90 seconds
  tell application "Finder"
    tell disk "Filewinch"
      open
      set current view of container window to icon view
      set toolbar visible of container window to false
      set statusbar visible of container window to false
      set the bounds of container window to {200, 120, 800, 548}
      set opts to the icon view options of container window
      set arrangement of opts to not arranged
      set icon size of opts to 128
      set text size of opts to 13
      set background picture of opts to file ".background:background.tiff"
      set position of item "Filewinch.app" of container window to {150, 190}
      set position of item "Applications" of container window to {450, 190}
      update without registering applications
      delay 1
      close
    end tell
  end tell
end timeout
APPLESCRIPT
layout=$!
for _ in $(seq 1 120); do kill -0 $layout 2>/dev/null || break; sleep 1; done
if kill -0 $layout 2>/dev/null; then
  kill $layout 2>/dev/null || true
  die "Finder didn't lay out the disk image within 2 minutes (waiting for the Automation permission prompt?)"
fi
wait $layout || die "Finder couldn't lay out the disk image: $(cat "$work/layout.log"). Allow your terminal to control Finder in System Settings > Privacy & Security > Automation, then run again."
[ -f /Volumes/Filewinch/.DS_Store ] || die "Finder saved no window layout in the disk image"
# Only Filewinch.app, the Applications link, .background/background.tiff and Finder's .DS_Store
# go in: drop what macOS writes on a mounted volume (event logs, trash, Spotlight index, temp
# items) and any AppleDouble ._ files.
rm -rf /Volumes/Filewinch/.fseventsd /Volumes/Filewinch/.Trashes /Volumes/Filewinch/.Spotlight-V100 \
  /Volumes/Filewinch/.TemporaryItems /Volumes/Filewinch/__MACOSX
find /Volumes/Filewinch -name '._*' -delete
sync
for attempt in 1 2 3 4 5; do
  hdiutil detach "$mounted" -quiet && { mounted=""; break; }
  sleep $((attempt * 2))
done
[ -z "$mounted" ] || { hdiutil detach "$mounted" -force -quiet; mounted=""; }

suffix=""; [ $notarize = 1 ] || suffix="-unnotarized"
dmg="$out_dir/Filewinch-$version-macos-universal$suffix.dmg"
rm -f "$dmg"
# LZMA (ULMO): the smallest of hdiutil's formats; macOS 10.15 and later open it (we need 13).
hdiutil convert -quiet "$work/rw.dmg" -format ULMO -o "$dmg"
rm -f "$work/rw.dmg"

# What a user sees, hidden files included: exactly the app, the link, the background and the
# layout. The mount is read-only and hidden from Finder. (hdiutil refuses a -mountpoint outside
# /Volumes on some setups, so it mounts at /Volumes/Filewinch, which was checked free above.)
# Right after hdiutil convert the new image can be busy for a moment (a scan); try a few times.
check_mount=""
for attempt in 1 2 3 4 5; do
  if attach_out=$(hdiutil attach -readonly -nobrowse -noautoopen "$dmg" 2>&1); then
    check_mount=$(awk -F'\t' '/Apple_HFS/{print $NF}' <<<"$attach_out")
    [ -n "$check_mount" ] && [ -d "$check_mount" ] && break
  fi
  check_mount=""
  sleep $((attempt * 2))
done
[ -n "$check_mount" ] || die "couldn't mount $dmg to check its contents: $attach_out"
mounted="$check_mount"
echo "disk image contents:"
ls -la "$check_mount"
top=$(cd "$check_mount" && ls -A | LC_ALL=C sort | tr '\n' ' ')
[ "$top" = ".DS_Store .background Applications Filewinch.app " ] || die "the disk image holds unexpected files: $top"
[ "$(cd "$check_mount/.background" && ls -A)" = background.tiff ] || die "the disk image's .background holds more than background.tiff"
[ "$(readlink "$check_mount/Applications")" = /Applications ] || die "the disk image's Applications isn't a link to /Applications"
stray=$(find "$check_mount" \( -name '._*' -o -name '.DS_Store' -o -name '*.dSYM' \) ! -path "$check_mount/.DS_Store" -print)
[ -z "$stray" ] || die "the disk image holds stray files: $stray"
codesign --verify --deep --strict "$check_mount/Filewinch.app" || die "the app inside the disk image fails codesign --verify"
hdiutil detach "$check_mount" -quiet || hdiutil detach "$check_mount" -force -quiet
mounted=""
codesign --force --timestamp --sign "$identity" "$dmg"
codesign --verify --strict --verbose=2 "$dmg"

if [ $notarize = 1 ]; then
  notarize_and_staple "$dmg" "$dmg"
fi

# --- final checks ------------------------------------------------------------------------------
step "final verification"
codesign --verify --deep --strict --verbose=2 "$app"
if [ $notarize = 1 ]; then
  spctl -a -vvv -t exec "$app"
  spctl -a -vvv -t install "$dmg"
else
  echo "(dry run) Gatekeeper's view before notarization; 'Unnotarized Developer ID' is expected:"
  spctl -a -vvv -t exec "$app" 2>&1 || true
fi
sum=$(cd "$out_dir" && shasum -a 256 "$(basename "$dmg")")
echo "$sum"
echo "sizes: universal binary $(human "$(stat -f %z "$app/Contents/MacOS/filewinch")"), Filewinch.app $(du -sk "$app" | awk '{print $1}') KiB on disk, disk image $(human "$(stat -f %z "$dmg")")"
echo "symbols (keep, don't ship): $symbols"
echo "wrote $dmg"

# --- upload ------------------------------------------------------------------------------------
if [ $upload = 1 ]; then
  step "upload to the v$version release in $repo"
  up="$work/upload"
  mkdir -p "$up"
  assets=$(gh release view "v$version" --repo "$repo" --json assets -q '.assets[].name')
  if grep -qx SHA256SUMS <<<"$assets"; then
    gh release download "v$version" --repo "$repo" -p SHA256SUMS -D "$up"
  else
    : >"$up/SHA256SUMS"
  fi
  name=$(basename "$dmg")
  # Replace this file's line (a re-run), keep everyone else's.
  { awk -v n="$name" '$2 != n && $2 != "*" n' "$up/SHA256SUMS"; echo "$sum"; } | sort -k2 >"$up/SHA256SUMS.new"
  mv "$up/SHA256SUMS.new" "$up/SHA256SUMS"
  files=("$dmg" "$up/SHA256SUMS")
  if grep -qx SHA256SUMS.minisig <<<"$assets"; then
    # CI signed the old sums; the new ones need a new signature, or the old one would fail.
    key=${FILEWINCH_MINISIGN_KEY:-}
    [ -n "$key" ] && [ -f "$key" ] ||
      die "the release has SHA256SUMS.minisig, so the updated SHA256SUMS must be re-signed: set FILEWINCH_MINISIGN_KEY to the minisign secret key file (nothing was uploaded)"
    command -v minisign >/dev/null || die "minisign not found (brew install minisign); nothing was uploaded"
    minisign -S -s "$key" -m "$up/SHA256SUMS" -x "$up/SHA256SUMS.minisig" -t "Filewinch $version SHA256SUMS"
    files+=("$up/SHA256SUMS.minisig")
  fi
  gh release upload "v$version" --repo "$repo" --clobber "${files[@]}"
  echo "uploaded $(basename "$dmg") and SHA256SUMS to v$version (still a draft until you publish it)"
fi
# Keep the signed app next to the disk image for inspection; drop the scratch files.
rm -rf "$out_dir/Filewinch.app"
mv "$app" "$out_dir/Filewinch.app"
rm -rf "$work"
echo "app: $out_dir/Filewinch.app"
