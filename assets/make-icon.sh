#!/bin/sh
# Regenerate assets/AppIcon.icns from assets/icon.svg (macOS built-ins only).
set -eu
cd "$(dirname "$0")"
tmp=$(mktemp -d)
qlmanage -t -s 1024 -o "$tmp" icon.svg >/dev/null
mkdir "$tmp/AppIcon.iconset"
for s in 16 32 128 256 512; do
  sips -z $s $s "$tmp/icon.svg.png" --out "$tmp/AppIcon.iconset/icon_${s}x${s}.png" >/dev/null
  sips -z $((s*2)) $((s*2)) "$tmp/icon.svg.png" --out "$tmp/AppIcon.iconset/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$tmp/AppIcon.iconset" -o AppIcon.icns
rm -rf "$tmp"
echo "wrote assets/AppIcon.icns"
