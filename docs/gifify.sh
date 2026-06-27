#!/usr/bin/env bash
# Convert a screen recording into an optimized GIF for the README.
#
# On GNOME (Wayland) the easiest capture is the built-in recorder:
#   press Ctrl+Alt+Shift+R to start/stop — it saves a .webm to ~/Videos.
# Then:
#   docs/gifify.sh ~/Videos/your-recording.webm docs/demo.gif
#
# Tip: keep clips short (5–8 s). Tweak FPS/WIDTH below for size vs. smoothness.
set -euo pipefail

in="${1:?usage: gifify.sh <input-video> [output.gif]}"
out="${2:-demo.gif}"
fps="${FPS:-20}"
width="${WIDTH:-900}"

palette="$(mktemp --suffix=.png)"
trap 'rm -f "$palette"' EXIT

ffmpeg -y -i "$in" -vf "fps=${fps},scale=${width}:-1:flags=lanczos,palettegen=stats_mode=diff" "$palette"
ffmpeg -y -i "$in" -i "$palette" \
  -lavfi "fps=${fps},scale=${width}:-1:flags=lanczos[x];[x][1:v]paletteuse=dither=bayer:bayer_scale=3" \
  "$out"

echo "wrote $out ($(du -h "$out" | cut -f1))"
