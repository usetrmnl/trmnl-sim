#!/bin/sh
# Render the app icon (crates/sim-ui/assets/icon.svg) into the macOS bundle's icon set,
# packaging/macos/AppIcon.icns. Needs rsvg-convert (brew install librsvg) and iconutil.
#
#   scripts/make-icns.sh
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
set_dir=$(mktemp -d)/AppIcon.iconset
trap 'rm -rf "$(dirname "$set_dir")"' EXIT
mkdir -p "$set_dir"
for size in 16 32 128 256 512; do
  rsvg-convert -w $size -h $size "$here/crates/sim-ui/assets/icon.svg" -o "$set_dir/icon_${size}x${size}.png"
  rsvg-convert -w $((size * 2)) -h $((size * 2)) "$here/crates/sim-ui/assets/icon.svg" \
    -o "$set_dir/icon_${size}x${size}@2x.png"
done
iconutil -c icns -o "$here/packaging/macos/AppIcon.icns" "$set_dir"
