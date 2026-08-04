#!/usr/bin/env bash
set -euo pipefail
export DISPLAY="${DISPLAY:-:1}"
REPO="/workspace"

xfce4-terminal --title="Cyclops Workspace Step 6" --geometry=120x35 \
  --command="$REPO/demos/launch-step6.sh" &
TERM_PID=$!

WIN=""
for _ in $(seq 1 25); do
  WIN=$(xdotool search --name "Cyclops Workspace Step 6" 2>/dev/null | head -1 || true)
  [ -n "$WIN" ] && break
  sleep 0.5
done
[ -n "$WIN" ] || { echo "window not found" >&2; exit 1; }

sleep 3
xdotool windowactivate --sync "$WIN"
sleep 1
# split right
xdotool key ctrl+b; sleep 0.3; xdotool key shift+5; sleep 1.5
# split down (")
xdotool key ctrl+b; sleep 0.3; xdotool key quotedbl; sleep 1.5
# zoom active pane
xdotool key ctrl+b; sleep 0.3; xdotool key z; sleep 1.5
xdotool key ctrl+b; sleep 0.3; xdotool key z; sleep 1
# new tab
xdotool key ctrl+b; sleep 0.3; xdotool key c; sleep 1.5
# rename tab
xdotool key ctrl+b; sleep 0.3; xdotool key comma; sleep 0.5
xdotool type --delay 40 'build'; xdotool key Return; sleep 1.5
xdotool key ctrl+b; sleep 0.3; xdotool key d
sleep 2
kill "$TERM_PID" 2>/dev/null || true
