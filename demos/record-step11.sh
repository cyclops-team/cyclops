#!/usr/bin/env bash
set -euo pipefail
export DISPLAY="${DISPLAY:-:1}"
REPO="/workspace"

xfce4-terminal --title="Cyclops Workspace Step 11" --geometry=120x35 \
  --command="$REPO/demos/launch-step11.sh" &
TERM_PID=$!

WIN=""
for _ in $(seq 1 25); do
  WIN=$(xdotool search --name "Cyclops Workspace Step 11" 2>/dev/null | head -1 || true)
  [ -n "$WIN" ] && break
  sleep 0.5
done
[ -n "$WIN" ] || { echo "window not found" >&2; exit 1; }

sleep 3
xdotool windowactivate --sync "$WIN"
sleep 1

eval "$(xdotool getwindowgeometry --shell "$WIN")"

# Drag vertical divider between left and right columns.
DX=$((WIDTH * 50 / 100))
DY=$((HEIGHT * 45 / 100))
DX2=$((DX + WIDTH * 8 / 100))
xdotool mousemove --window "$WIN" --sync "$DX" "$DY"
xdotool mousedown 1
xdotool mousemove --window "$WIN" --sync "$DX2" "$DY"
xdotool mouseup 1
sleep 1.5

# Drag horizontal divider in left column.
HX=$((WIDTH * 25 / 100))
HY=$((HEIGHT * 55 / 100))
HY2=$((HY + HEIGHT * 6 / 100))
xdotool mousemove --window "$WIN" --sync "$HX" "$HY"
xdotool mousedown 1
xdotool mousemove --window "$WIN" --sync "$HX" "$HY2"
xdotool mouseup 1
sleep 1.5

# Drag tab to reorder (tab 2 toward tab 1).
T1=$((WIDTH * 35 / 100))
T2=$((WIDTH * 48 / 100))
TY=$((HEIGHT * 6 / 100))
xdotool mousemove --window "$WIN" --sync "$T2" "$TY"
xdotool mousedown 1
xdotool mousemove --window "$WIN" --sync "$T1" "$TY"
xdotool mouseup 1
sleep 2

xdotool key ctrl+b; sleep 0.3; xdotool key d
sleep 2
kill "$TERM_PID" 2>/dev/null || true
