#!/usr/bin/env bash
# Review screenshots of every page at 1440 and 390 px, light and dark, into site/_review/.
# Serve first:  (cd site/_review/root && python3 -m http.server 8765)  with root/filewinch -> site
set -euo pipefail
site="$(cd "$(dirname "$0")/.." && pwd)"; out="$site/_review"; base="${BASE:-http://127.0.0.1:8765/filewinch/}"
chrome="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
pages="${PAGES:-index download buy thanks privacy license 404}"
for p in $pages; do
  url="$base$p.html"; [ "$p" = index ] && url="$base"; [ "$p" = 404 ] && url="${base}404.html"
  for w in 1440 390; do
    h=$([ $w = 1440 ] && echo 9000 || echo 14000)
    for theme in light dark; do
      flag=(); [ $theme = dark ] && flag=(--force-dark-mode)
      "$chrome" --headless=new --disable-gpu --hide-scrollbars --force-device-scale-factor=1 \
        ${flag[@]+"${flag[@]}"} --virtual-time-budget=5000 --window-size=$((w < 600 ? 600 : w)),$h \
        --screenshot="$out/$p-$w-$theme.png" "${base}_tools/frame.html?w=$w&h=$h&u=$(python3 -c 'import sys,urllib.parse;print(urllib.parse.quote(sys.argv[1],safe=""))' "$url")" 2>/dev/null
      magick "$out/$p-$w-$theme.png" -crop "${w}x$h+0+0" +repage "$out/$p-$w-$theme.png"
      # crop the empty tail below the footer
      f="$out/$p-$w-$theme.png"; read -r th ty < <(magick "$f" -fuzz 0.5% -trim -format '%h %Y\n' info:)
      magick "$f" -crop "${w}x$((th + ty + 2))+0+0" +repage "$f"
    done
  done
done
ls "$out"
