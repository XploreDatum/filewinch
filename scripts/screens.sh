#!/usr/bin/env bash
# Re-renders every screenshot in docs/screens/ with the headless tooling (debug build, so the
# tooling is in; no window, no Keychain). Scenes are fixed data at a fixed "now", so the output
# is the same every run (dates are shown in the machine's time zone; the committed shots are IST).
#   scripts/screens.sh            all of them
#   scripts/screens.sh local sync only these scenes (names from `filewinch --list-scenes`)
#   scripts/screens.sh --look windows [scenes]   one platform's look (macos, windows, linux, kde),
#       every scene light and dark, into docs/screens/<look>/ (`<scene>.png`, `<scene>-dark.png`)
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --offline -p filewinch-app
target=$(cargo metadata --offline --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
bin="$target/debug/filewinch"
if [[ ${1:-} == --look ]]; then
  look=${2:?--look needs macos, windows, linux or kde}
  shift 2
  out=docs/screens/$look
  mkdir -p "$out"
  # Each scene once; `--appearance` draws it light, then dark, whatever the scene sets.
  scenes=(empty empty-right local remote connect-sftp connect-s3 connect-swift transfers info sync sync-comparing trust failed password delete-shared delete-shared-everyone settings banner license-sheet license-settings about)
  [[ $# -gt 0 ]] && scenes=("$@")
  for s in "${scenes[@]}"; do
    for mode in light dark; do
      name=$s; [[ $mode == dark ]] && name=$s-dark
      if [[ $s == settings ]]; then
        "$bin" --look "$look" --appearance "$mode" --window settings --snap "$out/$name.png" --theme "$mode" >/dev/null
      elif [[ $s == license-settings ]]; then
        "$bin" --look "$look" --appearance "$mode" --window settings --scene license-unregistered --snap "$out/$name.png" --theme "$mode" >/dev/null
      elif [[ $s == about ]]; then
        "$bin" --look "$look" --appearance "$mode" --window about --snap "$out/$name.png" --theme "$mode" >/dev/null
      else
        "$bin" --look "$look" --appearance "$mode" --snap "$out/$name.png" --scene "$s" --theme "$mode" >/dev/null
      fi
      echo "$out/$name.png"
    done
  done
  exit 0
fi
out=docs/screens
scenes=(empty empty-right empty-right-dark local local-dark remote connect-sftp connect-s3 connect-swift transfers transfers-dark info sync sync-comparing trust failed password delete-shared delete-shared-everyone banner license-sheet license-sheet-limit)
[[ $# -gt 0 ]] && scenes=("$@")
# Settings and About are windows of their own, not scenes: these names render them.
[[ $# -eq 0 ]] && scenes+=(settings settings-dark license-settings license-settings-registered license-settings-deactivate about about-unregistered about-notices)
for s in "${scenes[@]}"; do
  case "$s" in
    settings) "$bin" --window settings --snap "$out/$s.png" --theme light >/dev/null ;;
    settings-dark) "$bin" --window settings --snap "$out/$s.png" --theme dark >/dev/null ;;
    license-settings) "$bin" --window settings --scene license-unregistered --snap "$out/$s.png" --theme light >/dev/null ;;
    license-settings-registered) "$bin" --window settings --scene light --snap "$out/$s.png" --theme light >/dev/null ;;
    license-settings-deactivate) "$bin" --window settings --scene license-deactivate --snap "$out/$s.png" --theme light >/dev/null ;;
    about) "$bin" --window about --scene light --snap "$out/$s.png" --theme light >/dev/null ;;
    about-unregistered) "$bin" --window about --scene unregistered --snap "$out/$s.png" --theme light >/dev/null ;;
    about-notices) "$bin" --window about --scene notices --snap "$out/$s.png" --theme light >/dev/null ;;
    *) "$bin" --snap "$out/$s.png" --scene "$s" >/dev/null ;;
  esac
  echo "$out/$s.png"
done
"$bin" --window settings --snap "$out/settings-dark.png" --theme dark 2>/dev/null && echo "$out/settings-dark.png"
"$bin" --tree "$out/tree-local.json" --scene local 2>/dev/null && echo "$out/tree-local.json"
# Scripted runs: a scene, a script, then a picture and/or the accessibility tree at a moment.
sc=$out/scripted
shot() { # scene script at name [png] [tree]
  local scene=$1 script=$2 at=$3 name=$4 png=$5 tree=$6
  [[ $png == 1 ]] && "$bin" --scene "$scene" --script "$sc/$script.script.json" --snap "$sc/$name.png" --at "$at" 2>/dev/null && echo "$sc/$name.png"
  [[ $tree == 1 ]] && "$bin" --scene "$scene" --script "$sc/$script.script.json" --tree "$sc/$name.json" --at "$at" 2>/dev/null && echo "$sc/$name.json"
  return 0
}
shot local navigate 1.2 navigate-inside 0 1
shot local navigate 2.4 navigate-end 1 1
shot remote rename 1.4 rename-typing 1 0
shot remote rename 2.4 rename-end 0 1
shot remote delete 1.1 delete-confirm 1 1
shot remote delete 2.6 delete-end 0 1
shot local connect-validate 1.6 connect-validate 1 1
