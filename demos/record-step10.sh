#!/usr/bin/env bash
set -euo pipefail
export DISPLAY="${DISPLAY:-:1}"
REPO="/workspace"

xfce4-terminal --title="Cyclops Workspace Step 10" --geometry=120x35 \
  --command="$REPO/demos/launch-step10.sh" &
TERM_PID=$!

WIN=""
for _ in $(seq 1 25); do
  WIN=$(xdotool search --name "Cyclops Workspace Step 10" 2>/dev/null | head -1 || true)
  [ -n "$WIN" ] && break
  sleep 0.5
done
[ -n "$WIN" ] || { echo "window not found" >&2; exit 1; }

sleep 3
xdotool windowactivate --sync "$WIN"
sleep 1

eval "$(xdotool getwindowgeometry --shell "$WIN")"
# Drag across COPY_DEMO text in the pane body.
X1=$((WIDTH * 25 / 100))
Y1=$((HEIGHT * 22 / 100))
X2=$((WIDTH * 75 / 100))
Y2=$((HEIGHT * 28 / 100))
xdotool mousemove --window "$WIN" --sync "$X1" "$Y1"
xdotool mousedown 1
xdotool mousemove --window "$WIN" --sync "$X2" "$Y2"
xdotool mouseup 1
sleep 1.5

# Double-click word selection.
xdotool mousemove --window "$WIN" --sync "$X1" "$Y1"
xdotool click --repeat 2 --delay 120 1
sleep 1.5

# Triple-click line selection.
Y3=$((HEIGHT * 32 / 100))
xdotool mousemove --window "$WIN" --sync "$X1" "$Y3"
xdotool click --repeat 3 --delay 120 1
sleep 2

xdotool key ctrl+b; sleep 0.3; xdotool key d
sleep 2
kill "$TERM_PID" 2>/dev/null || true
