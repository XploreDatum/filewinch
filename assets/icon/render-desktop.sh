#!/usr/bin/env bash
# Renders the Windows and Linux icons from the same SVG sources as render.sh (which makes the
# macOS .icns and is left alone):
#   filewinch.ico                       16, 24, 32, 48, 64, 128, 256 px (256 stored as PNG,
#                                       the rest as 32-bit BMP)
#   hicolor/<n>x<n>/apps/app.xpdt.filewinch.png   16, 24, 32, 48, 64, 128, 256, 512 px
#   hicolor/scalable/apps/app.xpdt.filewinch.svg
# icon-small.svg is used up to 32 px, icon.svg from 48 px. icon.svg is drawn on the macOS grid
# (an 824-unit tile in 1024); Windows and Linux icons fill more of their square, so the master is
# rendered through a tighter viewBox (40 40 944 944) that gives its tile the same 87.5 % share as
# icon-small.svg's while keeping the drop shadow inside the canvas.
# Needs: rsvg-convert (brew install librsvg), ImageMagick 7 `magick` (brew install imagemagick)
# and python3 (standard library only).
# The outputs are kept in the repo, so the packaging scripts never need this script.
set -euo pipefail
cd "$(dirname "$0")"
for t in rsvg-convert magick; do
  command -v "$t" >/dev/null || { echo "render-desktop.sh: $t not found" >&2; exit 1; }
done

id=app.xpdt.filewinch
out=hicolor.building
rm -rf "$out"
mkdir -p "$out/scalable/apps"
# The desktop framing of the master, as an SVG of its own (also the Linux scalable icon).
sed -e '1s|width="1024" height="1024" viewBox="0 0 1024 1024"|width="944" height="944" viewBox="40 40 944 944"|' \
  icon.svg >"$out/scalable/apps/$id.svg"
grep -q 'viewBox="40 40 944 944"' "$out/scalable/apps/$id.svg" ||
  { echo "render-desktop.sh: icon.svg's root <svg> changed; update the viewBox rewrite" >&2; exit 1; }

for n in 16 24 32 48 64 128 256 512; do
  if [ "$n" -le 32 ]; then src=icon-small.svg; else src="$out/scalable/apps/$id.svg"; fi
  mkdir -p "$out/${n}x${n}/apps"
  rsvg-convert -w "$n" -h "$n" "$src" -o "$out/${n}x${n}/apps/$id.png"
done

# ImageMagick writes every ICO entry as a 32-bit BMP, so it makes 16..128 and python3 appends
# the 256 px PNG as the last entry (PNG entries are valid in ICO since Windows Vista).
ico=filewinch.ico.building
sizes=()
for n in 16 24 32 48 64 128; do sizes+=("$out/${n}x${n}/apps/$id.png"); done
magick "${sizes[@]}" "ico:$ico"
python3 - "$ico" "$out/256x256/apps/$id.png" <<'PY'
import struct, sys
ico, png = sys.argv[1], open(sys.argv[2], 'rb').read()
data = open(ico, 'rb').read()
_, kind, count = struct.unpack('<HHH', data[:6])
entries = [list(struct.unpack('<BBBBHHII', data[6 + 16 * i:22 + 16 * i])) for i in range(count)]
images = [data[e[7]:e[7] + e[6]] for e in entries]
entries.append([0, 0, 0, 0, 1, 32, len(png), 0])  # 0 x 0 means 256 x 256
images.append(png)
offset = 6 + 16 * len(entries)
for e, img in zip(entries, images):
    e[6], e[7] = len(img), offset
    offset += len(img)
with open(ico, 'wb') as f:
    f.write(struct.pack('<HHH', 0, kind, len(entries)))
    for e in entries:
        f.write(struct.pack('<BBBBHHII', *e))
    for img in images:
        f.write(img)
PY

rm -rf hicolor
mv "$out" hicolor
mv "$ico" filewinch.ico
echo "wrote $(pwd)/filewinch.ico and $(pwd)/hicolor/"
