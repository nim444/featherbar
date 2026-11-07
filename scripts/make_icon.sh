#!/bin/sh
# Regenerate assets/AppIcon.icns from assets/icon.svg.
# Needs rsvg-convert (brew install librsvg); iconutil ships with macOS.
set -eu

cd "$(dirname "$0")/.."

SET=$(mktemp -d)/AppIcon.iconset
mkdir -p "$SET"
for size in 16 32 128 256 512; do
    rsvg-convert -w $size -h $size assets/icon.svg -o "$SET/icon_${size}x${size}.png"
    rsvg-convert -w $((size * 2)) -h $((size * 2)) assets/icon.svg -o "$SET/icon_${size}x${size}@2x.png"
done
iconutil -c icns "$SET" -o assets/AppIcon.icns
rm -rf "$(dirname "$SET")"

echo "Wrote assets/AppIcon.icns"
