#!/usr/bin/env bash
set -euo pipefail
export DISPLAY="${DISPLAY:-:1}"
REPO="/workspace"

xfce4-terminal --title="Cyclops Workspace Step 9" --geometry=120x35 \
  --command="$REPO/demos/launch-step9.sh" &
TERM_PID=$!

WIN=""
for _ in $(seq 1 25); do
  WIN=$(xdotool search --name "Cyclops Workspace Step 9" 2>/dev/null | head -1 || true)
  [ -n "$WIN" ] && break
  sleep 0.5
done
[ -n "$WIN" ] || { echo "window not found" >&2; exit 1; }

sleep 3
xdotool windowactivate --sync "$WIN"
sleep 1

eval "$(xdotool getwindowgeometry --shell "$WIN")"
# Sidebar ~20 cols of ~118 → click right pane center.
RX=$((WIDTH * 68 / 100))
RY=$((HEIGHT * 45 / 100))
xdotool mousemove --window "$WIN" --sync "$RX" "$RY"
xdotool click 1
sleep 1

# Wheel scroll history in left pane.
LX=$((WIDTH * 28 / 100))
xdotool mousemove --window "$WIN" --sync "$LX" "$RY"
xdotool click --repeat 6 --delay 100 5
sleep 1.5

# Click second tab.
TX=$((WIDTH * 55 / 100))
TY=$((HEIGHT * 6 / 100))
xdotool mousemove --window "$WIN" --sync "$TX" "$TY"
xdotool click 1
sleep 1.5

# Right-click pane for context menu.
xdotool mousemove --window "$WIN" --sync "$RX" "$RY"
xdotool click 3
sleep 1.5
xdotool key Escape
sleep 0.5

# Application menu (bottom-left of main area).
MX=$((WIDTH * 22 / 100))
MY=$((HEIGHT * 92 / 100))
xdotool mousemove --window "$WIN" --sync "$MX" "$MY"
xdotool click 1
sleep 1.5
xdotool key Escape
sleep 0.5

xdotool key ctrl+b; sleep 0.3; xdotool key d
sleep 2
kill "$TERM_PID" 2>/dev/null || true
