#!/usr/bin/env bash
# Renders dmg-background.svg into dmg-background.tiff (600x400 at 1x plus a 2x Retina
# representation), which scripts/release-mac.sh puts in the disk image. The .tiff is kept in the
# repo, so a release never needs this. Needs rsvg-convert (brew install librsvg) and tiffutil.
set -euo pipefail
cd "$(dirname "$0")"
command -v rsvg-convert >/dev/null || { echo "render-dmg-background.sh: rsvg-convert not found (brew install librsvg)" >&2; exit 1; }
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
rsvg-convert -w 600 -h 400 dmg-background.svg -o "$work/bg.png"
rsvg-convert -w 1200 -h 800 dmg-background.svg -o "$work/bg@2x.png"
# 72 and 144 dpi so tiffutil pairs them as one image with a Retina representation.
sips -s dpiWidth 72 -s dpiHeight 72 "$work/bg.png" >/dev/null
sips -s dpiWidth 144 -s dpiHeight 144 "$work/bg@2x.png" >/dev/null
tiffutil -cathidpicheck "$work/bg.png" "$work/bg@2x.png" -out dmg-background.tiff.building
mv -f dmg-background.tiff.building dmg-background.tiff
echo "wrote $(pwd)/dmg-background.tiff"
