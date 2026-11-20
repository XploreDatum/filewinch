#!/usr/bin/env bash
# Renders the Filewinch app icon from its SVG sources into Filewinch.iconset/ and Filewinch.icns,
# and draws preview.png (every size on light and dark backgrounds).
#   icon.svg        master, used for 64 px and up (32@2x .. 512@2x)
#   icon-small.svg  simplified variant for 16 and 32 px (16@1x, 16@2x, 32@1x)
# Needs: rsvg-convert (brew install librsvg), iconutil (Xcode CLT), ImageMagick `magick` for the preview.
# The rendered PNGs and .icns are kept in the repo, so scripts/bundle.sh never needs this script.
set -euo pipefail
cd "$(dirname "$0")"
for t in rsvg-convert iconutil magick; do
  command -v "$t" >/dev/null || { echo "render.sh: $t not found" >&2; exit 1; }
done

set_dir=Filewinch.iconset
rm -rf "$set_dir.building"
mkdir -p "$set_dir.building"
render() {   # render <svg> <pixels> <name>
  rsvg-convert -w "$2" -h "$2" "$1" -o "$set_dir.building/$3"
}
render icon-small.svg   16 icon_16x16.png
render icon-small.svg   32 icon_16x16@2x.png
render icon-small.svg   32 icon_32x32.png
render icon.svg         64 icon_32x32@2x.png
render icon.svg        128 icon_128x128.png
render icon.svg        256 icon_128x128@2x.png
render icon.svg        256 icon_256x256.png
render icon.svg        512 icon_256x256@2x.png
render icon.svg        512 icon_512x512.png
render icon.svg       1024 icon_512x512@2x.png
rm -rf "$set_dir"
mv "$set_dir.building" "$set_dir"
iconutil -c icns "$set_dir" -o Filewinch.icns

# Contact sheet: each size at 1:1 in its own cell, on a light and a dark strip,
# then 16 and 32 px magnified 8x (nearest neighbour) to show the pixels.
sheet=preview.png
cell() {   # cell <png> <background> [scale%]
  if [[ -n "${3:-}" ]]; then
    echo "( $1 -filter point -resize $3 -background $2 -gravity center -extent 300x560 )"
  else
    echo "( $1 -background $2 -gravity center -extent $(( $(magick identify -format %w "$1") + 40 ))x560 )"
  fi
}
strip() {  # strip <background>
  local bg=$1 out=()
  for f in icon_16x16 icon_32x32 icon_32x32@2x icon_128x128 icon_256x256 icon_512x512; do
    out+=( $(cell "$set_dir/$f.png" "$bg") )
  done
  out+=( $(cell "$set_dir/icon_16x16.png" "$bg" 800%) $(cell "$set_dir/icon_32x32.png" "$bg" 800%) )
  echo "( ${out[*]} +append )"
}
# shellcheck disable=SC2046  # word splitting of the generated magick arguments is intended
magick $(strip '#f6f6f4') $(strip '#16181b') -append "$sheet"
echo "wrote $(pwd)/$set_dir, $(pwd)/Filewinch.icns, $(pwd)/$sheet"
