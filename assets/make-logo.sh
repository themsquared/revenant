#!/bin/sh
# Turn the source logo render into the assets revenant ships.
#
#   1. save the logo art to assets/logo-source.png
#   2. sh assets/make-logo.sh
#
# Produces (all in assets/):
#   logo.png          full-res, dark background preserved
#   logo-transparent.png   dark background flooded to transparent
#   favicon.ico       multi-size icon (16/32/48/64/128/256)
#   logo.svg          color vector trace (via vtracer)
# Requires: imagemagick (magick), vtracer.
set -eu
cd "$(dirname "$0")"
SRC="${1:-logo-source.png}"
[ -f "$SRC" ] || { echo "save the logo to assets/$SRC first" >&2; exit 1; }

have() { command -v "$1" >/dev/null 2>&1; }
magick_bin=""
if have magick; then magick_bin="magick"; elif have convert; then magick_bin="convert"; fi
[ -n "$magick_bin" ] || { echo "need imagemagick (brew install imagemagick)" >&2; exit 1; }

echo "→ logo.png"
cp "$SRC" logo.png

# Flood the near-black background to transparent from the corners, feathered.
echo "→ logo-transparent.png"
"$magick_bin" "$SRC" -fuzz 12% -fill none \
  -draw "alpha 0,0 floodfill" \
  -draw "alpha $(($(sips -g pixelWidth "$SRC" | awk '/pixelWidth/{print $2}')-1)),0 floodfill" \
  logo-transparent.png 2>/dev/null || \
  "$magick_bin" "$SRC" -fuzz 12% -transparent black logo-transparent.png

# Multi-size .ico from the transparent version (square-cropped, centered).
echo "→ favicon.ico"
"$magick_bin" logo-transparent.png -background none -gravity center \
  -resize 256x256 -extent 256x256 /tmp/logo-square.png
"$magick_bin" /tmp/logo-square.png -define icon:auto-resize=16,32,48,64,128,256 favicon.ico

# Color vector trace.
if have vtracer; then
  echo "→ logo.svg"
  vtracer --input logo-transparent.png --output logo.svg --mode spline --color_precision 6 >/dev/null 2>&1 \
    && echo "  traced" || echo "  vtracer failed (skipping svg)"
else
  echo "  (install vtracer for logo.svg:  cargo install vtracer)"
fi

echo "done. assets:"; ls -1 logo*.png favicon.ico logo.svg 2>/dev/null
