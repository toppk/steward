#!/usr/bin/env bash
# Build the documentation site with pandoc: site/build.sh [OUT] (default _site).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo_root=$(dirname "$here")
out=${1:-"$repo_root/_site"}
theme="$here/theme"
edit="https://github.com/toppk/steward/edit/master"

# Reading order: page|title|group|source (source defaults to pages/<page>.md).
pages=(
  "index|Overview|Start"
  "getting-started|Getting started|Start"
  "concepts|Concepts|Start"
  "configuration|Configuration|Use"
  "cli|Command line|Use"
  "gui|Desktop app|Use"
  "operations|Operations|Use"
  "applications|Building on steward|Build"
  "agents|For AI agents|Build"
  "api|API reference|Reference"
  "design|Design notes|Reference|docs/design.md"
)

field() { cut -d'|' -f"$2" <<<"$1"; }
source_of() {
  local s
  s=$(field "$1" 4)
  if [[ -n "$s" ]]; then echo "$repo_root/$s"; else echo "$here/pages/$(field "$1" 1).md"; fi
}

nav_for() {
  local current=$1 group="" html="" p name title g
  for p in "${pages[@]}"; do
    name=$(field "$p" 1) title=$(field "$p" 2) g=$(field "$p" 3)
    if [[ "$g" != "$group" ]]; then
      [[ -n "$group" ]] && html+="</ul></div>"
      html+="<div class=\"nav-group\"><span class=\"nav-label\">$g</span><ul>"
      group=$g
    fi
    if [[ "$name" == "$current" ]]; then
      html+="<li><a href=\"$name.html\" aria-current=\"page\">$title</a></li>"
    else
      html+="<li><a href=\"$name.html\">$title</a></li>"
    fi
  done
  echo "$html</ul></div>"
}

rm -rf "$out"
mkdir -p "$out/theme" "$out/assets"
cp "$theme/horizon.css" "$theme/horizon.js" "$out/theme/"
cp -r "$here/assets/." "$out/assets/"
cp "$here/llms.txt" "$out/llms.txt"
cp "$here/install.sh" "$out/install.sh"
: >"$out/llms-full.txt"
touch "$out/.nojekyll"

count=${#pages[@]}
for i in "${!pages[@]}"; do
  p=${pages[$i]}
  name=$(field "$p" 1)
  src=$(source_of "$p")
  pager=""
  if ((i > 0)); then
    prev=${pages[$((i - 1))]}
    pager+="<a class=\"prev\" href=\"$(field "$prev" 1).html\"><span>Previous</span>$(field "$prev" 2)</a>"
  fi
  if ((i < count - 1)); then
    next=${pages[$((i + 1))]}
    pager+="<a class=\"next\" href=\"$(field "$next" 1).html\"><span>Next</span>$(field "$next" 2)</a>"
  fi
  args=()
  [[ "$name" != "index" ]] && args+=(--toc --toc-depth=3)
  [[ "$name" == "design" ]] && args+=(--shift-heading-level-by=-1 --metadata eyebrow=Reference
    --metadata lede="How steward works inside: storage, scanning, classification, hashing, and the reasons behind them.")
  pandoc "$src" \
    --from markdown+fenced_divs+bracketed_spans \
    --to html5 \
    --template "$theme/template.html" \
    --lua-filter "$theme/horizon.lua" \
    --metadata-file "$here/site.yaml" \
    --metadata source="$name.md" \
    --metadata edit-url="$edit/${src#"$repo_root/"}" \
    --variable nav="$(nav_for "$name")" \
    --variable pager="$pager" \
    "${args[@]}" \
    --output "$out/$name.html"
  cp "$src" "$out/$name.md"
  { printf '\n\n<!-- %s.md -->\n\n' "$name"; cat "$src"; } >>"$out/llms-full.txt"
done

echo "built $count pages into $out"
