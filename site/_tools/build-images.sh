#!/usr/bin/env bash
# Regenerates the site's optimised screenshots and icons from the app repo.
# Needs ImageMagick (magick) and cwebp. Run from anywhere: site/_tools/build-images.sh
set -euo pipefail
site="$(cd "$(dirname "$0")/.." && pwd)"
repo="$(cd "$site/.." && pwd)"
shots="$repo/docs/screens"; out="$site/assets/shots"; icons="$site/assets/icons"
mkdir -p "$out" "$icons"
shot() {  # shot <os> <name>  -> <os>-<name>.webp at 1600 px wide
  magick "$shots/$1/$2.png" -resize 1600x -strip "$out/.tmp.png"
  cwebp -quiet -q 80 -m 6 "$out/.tmp.png" -o "$out/$1-$2.webp"
  rm -f "$out/.tmp.png"
}
crop() {  # crop <os> <name> -> <os>-<name>.webp: the sheet in the middle of the window, at 2x
  magick "$shots/$1/$2.png" -crop 1400x1260+620+190 +repage -strip "$out/.tmp.png"
  cwebp -quiet -q 80 -m 6 "$out/.tmp.png" -o "$out/$1-$2.webp"
  rm -f "$out/.tmp.png"
}
for n in transfers transfers-dark remote remote-dark; do shot macos "$n"; done
for n in sync sync-dark delete-shared-everyone delete-shared-everyone-dark \
         trust trust-dark connect-sftp connect-sftp-dark; do crop macos "$n"; done
for os in windows linux; do shot "$os" remote; shot "$os" remote-dark; done
# Icons: brand SVG and PNG sizes, plus favicon.ico
cp "$repo/assets/icon/icon-small.svg" "$icons/favicon.svg"
set_dir="$repo/assets/icon/Filewinch.iconset"
cp "$set_dir/icon_128x128.png" "$icons/icon-128.png"
cp "$set_dir/icon_256x256.png" "$icons/icon-256.png"
magick "$set_dir/icon_128x128@2x.png" -resize 180x180 -strip "$icons/apple-touch-icon.png"
magick "$set_dir/icon_16x16.png" "$set_dir/icon_32x32.png" \( "$set_dir/icon_32x32@2x.png" -resize 48x48 \) "$site/favicon.ico"
# Social card (1200x630): rendered from _tools/og-card.html with headless Chrome.
chrome="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
if [ -x "$chrome" ]; then
  "$chrome" --headless=new --disable-gpu --hide-scrollbars --force-device-scale-factor=1 \
    --window-size=1200,630 --screenshot="$site/assets/og.png" "file://$site/_tools/og-card.html" 2>/dev/null
fi
