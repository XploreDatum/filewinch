#!/usr/bin/env bash
# Builds a release Filewinch.app (bundle id app.xpdt.filewinch) into dist/.
#   scripts/bundle.sh                 -> dist/Filewinch.app, signed ad hoc
#   MAC_APP_SIGN="Developer ID Application: …" scripts/bundle.sh   signs with that identity
#   FILEWINCH_FEATURES="sftp ftp" scripts/bundle.sh                 backends to include
# For in-place relaunching while developing, Rune's vendor/rune/scripts/mac-app installs the
# same binary into ~/Applications instead.
set -euo pipefail
cd "$(dirname "$0")/.."
root=$(pwd)
version=$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="filewinch-app"))')
target=$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
args=(-p filewinch-app --release)
[[ -n "${FILEWINCH_FEATURES:-}" ]] && args+=(--features "$FILEWINCH_FEATURES")
cargo build "${args[@]}"

app="$root/dist/Filewinch.app"
stage="$root/dist/.Filewinch.app.building"
rm -rf "$stage"
mkdir -p "$stage/Contents/MacOS" "$stage/Contents/Resources"
cp "$target/release/filewinch" "$stage/Contents/MacOS/filewinch"
cp "$root/assets/icon/Filewinch.icns" "$stage/Contents/Resources/Filewinch.icns"
cat > "$stage/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>app.xpdt.filewinch</string>
  <key>CFBundleName</key><string>Filewinch</string>
  <key>CFBundleDisplayName</key><string>Filewinch</string>
  <key>CFBundleExecutable</key><string>filewinch</string>
  <key>CFBundleIconFile</key><string>Filewinch</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${version}</string>
  <key>CFBundleVersion</key><string>${version}</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
  <key>NSHumanReadableCopyright</key><string>Filewinch</string>
</dict>
</plist>
PLIST
plutil -lint "$stage/Contents/Info.plist" >/dev/null
codesign --force --sign "${MAC_APP_SIGN:--}" --timestamp=none "$stage" >/dev/null
# Swap in whole, so a failed build never leaves half an app behind.
rm -rf "$app"
mv "$stage" "$app"
echo "$app"
