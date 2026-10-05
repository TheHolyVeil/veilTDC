#!/usr/bin/env bash
# Phase 1 smoke test: run every sub-cell renderer on one source image at a
# realistic terminal size (210x50 — NOT cropped to 1:1) and render the result
# both as raw geometry and through a real font.
#
#   ./test-braille.sh [source-image]     # no arg -> synthetic UI-text image
#   COLS=160 ROWS=45 OUT=/tmp/x ./test-braille.sh
set -euo pipefail
cd "$(dirname "$0")"

OUT=${OUT:-/tmp/braille-test}
COLS=${COLS:-210}
ROWS=${ROWS:-50}
mkdir -p "$OUT"

SRC=${1:-}
if [ -z "$SRC" ]; then
    python3 render_real_font.py sample "$OUT/source.png"
    SRC="$OUT/source.png"
fi

for mode in halfblock quadrant braille; do
    cargo run --release -q -p veil-render --example braille_preview -- \
        "$SRC" --"$mode" --cols "$COLS" --rows "$ROWS" --dump \
        --png "$OUT/$mode-geometry.png" > "$OUT/$mode.txt"
    python3 render_real_font.py grid "$OUT/$mode.txt" "$OUT/$mode-font.png"
done

echo "done -> $OUT"
ls -1 "$OUT"
